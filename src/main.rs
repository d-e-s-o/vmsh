//! Transparently run a shell (or other binary) in a VM.

mod args;
mod embed;

use std::cell::LazyCell;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::env;
use std::env::home_dir;
use std::env::temp_dir;
use std::ffi::CString;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::ffi::c_char;
use std::fs::File;
use std::fs::remove_file;
use std::fs::write;
use std::io::Seek as _;
use std::io::Write as _;
use std::mem::MaybeUninit;
use std::ops::Deref;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::io::AsRawFd as _;
use std::os::unix::io::FromRawFd as _;
use std::os::unix::io::OwnedFd;
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::ptr;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::ensure;

use clap::Parser;

use vmsh::detect_kernel_format;
use vmsh::hostname;

use crate::args::Args;
use crate::args::Command;
use crate::args::RunArgs;


/// Embedded init binary.
const INIT_BINARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vmsh-init"));


/// RAII guard to clean up a temporary file on exit.
struct CleanupGuard(Option<PathBuf>);

impl Deref for CleanupGuard {
  type Target = Path;

  fn deref(&self) -> &Self::Target {
    // SANITY: We only ever unset `0` as part of the `Drop` impl.
    self.0.as_deref().unwrap()
  }
}

impl Drop for CleanupGuard {
  fn drop(&mut self) {
    if let Some(path) = self.0.take() {
      let _result = remove_file(path);
    }
  }
}


/// Write the embedded init binary to the host filesystem (visible in
/// guest via `virtiofs`).
fn deploy_init_binary(path: &Path) -> Result<CleanupGuard> {
  let () = write(path, INIT_BINARY)
    .with_context(|| format!("failed to write init binary to {}", path.display()))?;

  let guard = CleanupGuard(Some(path.to_path_buf()));

  let c_path = CString::new(path.as_os_str().as_bytes())?;
  // SAFETY: `c_path` is a valid NUL-terminated string.
  let rc = unsafe { libc::chmod(c_path.as_ptr(), 0o755) };
  ensure!(
    rc == 0,
    "failed to `chmod` init binary at `{}`",
    path.display()
  );
  Ok(guard)
}


/// Build the environment variable payload from CLI flags and host env.
///
/// Variables that are reserved or overridden by vmsh (`VMSH_*`) or
/// internal to libkrun (`KRUN_*`) are excluded when `all_envs` is set.
fn build_env_content(
  env_args: &[String],
  all_envs: bool,
  host_env: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<u8> {
  let host = LazyCell::new(|| host_env.into_iter().collect::<HashMap<_, _>>());
  let mut out = BTreeMap::new();

  if all_envs {
    for (key, value) in &*host {
      if let Some(k) = key.to_str() {
        if k.starts_with("VMSH_") || k.starts_with("KRUN_") {
          continue;
        }
      }
      let _prev = out.insert(key.as_os_str(), value.as_os_str());
    }
  }

  for arg in env_args {
    if let Some(pos) = arg.find('=') {
      let key = OsStr::new(&arg[..pos]);
      let value = OsStr::new(&arg[pos + 1..]);
      let _prev = out.insert(key, value);
    } else {
      let key = OsStr::new(arg);
      if let Some(value) = host.get(key) {
        let _prev = out.insert(key, value);
      }
    }
  }

  let mut buf = Vec::new();
  for (key, value) in &out {
    let () = buf.extend_from_slice(key.as_bytes());
    let () = buf.push(b'=');
    let () = buf.extend_from_slice(value.as_bytes());
    let () = buf.push(b'\n');
  }
  buf
}


/// Create a memfd containing the environment variable payload.
fn create_env_memfd(content: &[u8]) -> Result<OwnedFd> {
  // SAFETY: "vmsh-env" is a valid NUL-terminated name.
  let fd = unsafe { libc::memfd_create(c"vmsh-env".as_ptr(), 0) };
  ensure!(fd >= 0, "failed to create memfd for env vars");

  // SAFETY: `memfd_create` succeeded, so `fd` is a valid, open file
  //         descriptor that we own.
  let fd = unsafe { OwnedFd::from_raw_fd(fd) };
  let mut file = File::from(fd);
  let () = file
    .write_all(content)
    .context("failed to write env vars to memfd")?;
  let () = file.rewind().context("failed to seek memfd to start")?;

  Ok(file.into())
}


fn set_kernel(ctx: u32, kernel: PathBuf, init_guest_path: &Path, verbosity: u8) -> Result<()> {
  let kernel_format = detect_kernel_format(&kernel)?;
  let quiet = if verbosity < 2 { "quiet" } else { "" };
  let cmdline = format!(
    "earlycon=uart,io,0x3f8 reboot=k panic=5 console=hvc0 rootfstype=virtiofs rw init={} {quiet}",
    init_guest_path.display()
  );
  let c_kernel_path = CString::new(kernel.into_os_string().into_vec())?;
  // SANITY: `cmdline` is built from ASCII literals and a path display.
  let c_cmdline = CString::new(cmdline).unwrap();
  let initramfs = ptr::null();

  // SAFETY: `ctx` is a valid krun context and all pointers reference
  //         valid NUL-terminated strings (or null for initramfs).
  let rc = unsafe {
    krun::krun_set_kernel(
      ctx,
      c_kernel_path.as_ptr(),
      kernel_format as u32,
      initramfs,
      c_cmdline.as_ptr(),
    )
  };
  ensure!(rc >= 0, "failed to set kernel (code {rc})");
  Ok(())
}


fn set_exec(
  ctx: u32,
  command: Vec<String>,
  has_env_port: bool,
  unlink_paths: &[&Path],
) -> Result<()> {
  // Provide defaults for some relevant variables, but these will be
  // overwritten by any user provided values.
  let hostname = hostname().context("failed to retrieve host name")?;
  let hostname = CString::new(format!("HOSTNAME={hostname}")).unwrap();
  let home_owned;
  let home = if let Some(home_dir) = home_dir() {
    home_owned = CString::new(format!("HOME={}", home_dir.display())).unwrap();
    &home_owned
  } else {
    c"/root"
  };

  // Determine which of stdin/stdout/stderr are non-terminal.
  // `libkrun` creates virtio console ports for redirected FDs; tell the
  // guest init which ports to look for.
  // SAFETY: `isatty` is always safe to call.
  let stdin_redir = unsafe { libc::isatty(libc::STDIN_FILENO) == 0 };
  // SAFETY: `isatty` is always safe to call.
  let stdout_redir = unsafe { libc::isatty(libc::STDOUT_FILENO) == 0 };
  // SAFETY: `isatty` is always safe to call.
  let stderr_redir = unsafe { libc::isatty(libc::STDERR_FILENO) == 0 };

  let mut env_ptrs = vec![hostname.as_ptr(), home.as_ptr()];

  // Tell the guest init which temporary files to unlink. libkrun exits
  // the process hard on VM exit, bypassing host-side RAII cleanup.
  let unlink_env = if !unlink_paths.is_empty() {
    let value = unlink_paths
      .iter()
      .map(|p| p.display().to_string())
      .collect::<Vec<_>>()
      .join(":");
    Some(CString::new(format!("VMSH_UNLINK={value}"))?)
  } else {
    None
  };
  if let Some(ref env) = unlink_env {
    let () = env_ptrs.push(env.as_ptr());
  }

  // Tell the guest init to look for a "krun-env" virtio console port.
  let env_port_env = c"VMSH_ENV_PORT=1";
  if has_env_port {
    let () = env_ptrs.push(env_port_env.as_ptr());
  }

  if stdin_redir {
    let () = env_ptrs.push(c"VMSH_STDIN=1".as_ptr());
  }
  if stdout_redir {
    let () = env_ptrs.push(c"VMSH_STDOUT=1".as_ptr());
  }
  if stderr_redir {
    let () = env_ptrs.push(c"VMSH_STDERR=1".as_ptr());
  }
  let () = env_ptrs.push(ptr::null());

  if !command.is_empty() {
    let cmd = CString::new(command[0].as_str())?;
    let args = command[1..]
      .iter()
      .map(|a| CString::new(a.as_str()))
      .collect::<Result<Vec<_>, _>>()?;
    let mut argv = args
      .iter()
      .map(|a| a.as_ptr())
      .collect::<Vec<*const c_char>>();
    let () = argv.push(ptr::null());

    // SAFETY: `ctx` is a valid krun context and all pointers reference
    //         valid NUL-terminated strings or null sentinels.
    let rc = unsafe { krun::krun_set_exec(ctx, cmd.as_ptr(), argv.as_ptr(), env_ptrs.as_ptr()) };
    ensure!(rc >= 0, "failed to set exec command");
  } else {
    // SAFETY: `ctx` is a valid krun context and `env_ptrs` is a valid
    //         NUL-terminated pointer array.
    let rc = unsafe { krun::krun_set_env(ctx, env_ptrs.as_ptr()) };
    ensure!(rc >= 0, "failed to set environment");
  }
  Ok(())
}


fn exec_vm(args: RunArgs, init_guest_path: &Path, unlink_paths: &[&Path]) -> Result<()> {
  let RunArgs {
    kernel,
    cpus,
    memory,
    net,
    uds,
    command,
    env_vars,
    all_envs,
    verbosity,
  } = args;

  // SANITY: Caller guarantees that a kernel is always set.
  let kernel = kernel.unwrap();

  if verbosity > 0 {
    // SAFETY: `STDERR_FILENO` is a valid file descriptor.
    let rc = unsafe {
      krun::krun_init_log(
        libc::STDERR_FILENO,
        u32::from(verbosity),
        2, // KRUN_LOG_STYLE_NEVER
        0, // use env
      )
    };
    ensure!(rc >= 0, "failed to set log level");
  }

  let ctx = krun::krun_create_ctx() as u32;

  let rc = krun::krun_set_vm_config(ctx, cpus, memory);
  ensure!(rc >= 0, "failed to set VM config");

  // libkrun creates an implicit virtio console on `hvc0` connected to
  // stdin/stdout/stderr, so we don't need to add one explicitly.

  // Add a serial console so `earlycon=uart,io,0x3f8` works.
  // SAFETY: `ctx` is a valid krun context.
  let rc = unsafe {
    krun::krun_add_serial_console_default(
      ctx,
      -1,
      if verbosity > 0 {
        libc::STDERR_FILENO
      } else {
        -1
      },
    )
  };
  ensure!(rc >= 0, "failed to add serial console");

  // Enable TSI (Transparent Socket Impersonation) for networking. TSI
  // intercepts AF_INET/AF_INET6 socket calls in the guest kernel and
  // proxies them through the host VMM via virtio-vsock, providing
  // network connectivity without a virtual NIC. This requires a guest
  // kernel with TSI patches applied (see `var/linux-tsi-patches/`).
  const KRUN_TSI_HIJACK_INET: u32 = 1 << 0;
  const KRUN_TSI_HIJACK_UNIX: u32 = 1 << 1;

  let rc = krun::krun_disable_implicit_vsock(ctx);
  ensure!(rc >= 0, "failed to disable implicit vsock");

  let mut tsi_features = 0;
  if net {
    tsi_features |= KRUN_TSI_HIJACK_INET;
  }
  if net || uds {
    tsi_features |= KRUN_TSI_HIJACK_UNIX;
  }
  let rc = krun::krun_add_vsock(ctx, tsi_features);
  ensure!(rc >= 0, "failed to add vsock device");

  let () = set_kernel(ctx, kernel, init_guest_path, verbosity)?;

  let c_rootfs = c"/";
  // SAFETY: `ctx` is a valid krun context and `c_rootfs` is a valid
  //         NUL-terminated string.
  let rc = unsafe { krun::krun_set_root(ctx, c_rootfs.as_ptr()) };
  ensure!(rc >= 0, "failed to set root filesystem");

  // Pass environment variables to the guest via a virtio console port
  // backed by a memfd. The guest init discovers the port by name and
  // reads KEY=VALUE lines until EOF.
  let env_content = build_env_content(&env_vars, all_envs, env::vars_os());
  let has_env_port = !env_content.is_empty();
  let env_fd;
  if has_env_port {
    env_fd = create_env_memfd(&env_content)?;

    // SAFETY: `ctx` is a valid krun context.
    let console_id = unsafe { krun::krun_add_virtio_console_multiport(ctx) };
    ensure!(console_id >= 0, "failed to add virtio console for env port");

    // SAFETY: `ctx` is a valid krun context, `console_id` is a valid
    //         console index, "krun-env" is NUL-terminated, and `env_fd`
    //         is a valid, open file descriptor.
    let rc = unsafe {
      krun::krun_add_console_port_inout(
        ctx,
        console_id as u32,
        c"krun-env".as_ptr(),
        env_fd.as_raw_fd(),
        -1,
      )
    };
    ensure!(rc >= 0, "failed to add env console port");
  }
  let () = set_exec(ctx, command, has_env_port, unlink_paths)?;

  let rc = krun::krun_start_enter(ctx);
  ensure!(rc >= 0, "failed to start VM (code {rc})");
  Ok(())
}


/// Raise `RLIMIT_NOFILE` to the maximum allowed number of file
/// descriptors.
///
/// This is necessary, because libkrun's virtiofs passthrough filesystem
/// holds one open file descriptor per inode the guest touches, which
/// can quickly add up.
fn set_rlimits() -> Result<()> {
  let mut limit = MaybeUninit::<libc::rlimit>::uninit();
  // SAFETY: `getrlimit` initializes the `rlimit` struct on success.
  let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) };
  ensure!(rc >= 0, "failed to get `RLIMIT_NOFILE`");

  // SAFETY: `getrlimit` succeeded, so `limit` is fully initialized.
  let mut limit = unsafe { limit.assume_init() };
  limit.rlim_cur = limit.rlim_max;
  // SAFETY: `limit` is a valid, initialized `rlimit` struct.
  let _rc = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
  Ok(())
}


fn main() -> Result<()> {
  let args = Args::parse();

  let mut args = match args.command {
    Some(Command::Embed(embed_args)) => {
      return embed::embed_kernel(&embed_args.kernel, embed_args.output.as_deref());
    },
    Some(Command::Run(run_args)) => run_args,
    None => args.args,
  };

  // Resolve kernel: explicit positional arg > embedded > error.
  let kernel_guard;
  let extracted_kernel = match &mut args.kernel {
    Some(path) => {
      ensure!(
        path.exists(),
        "failed to find kernel at `{}`",
        path.display()
      );
      None
    },
    None => {
      kernel_guard = embed::extract_embedded_kernel()
        .context("no kernel specified and no embedded kernel found")?;
      args.kernel = Some(kernel_guard.to_path_buf());
      Some(&*kernel_guard)
    },
  };

  let () = set_rlimits()?;

  // Deploy init binary to `/tmp/` which is typically writable and
  // visible inside the guest via virtiofs.
  let init_filename = format!("vmsh-init-{}", process::id());
  let init_path = temp_dir().join(&init_filename);
  let _guard = deploy_init_binary(&init_path)?;

  // Collect temporary files for the guest init to clean up.
  let mut unlink_paths = vec![init_path.as_path()];
  if let Some(kernel_path) = extracted_kernel {
    let () = unlink_paths.push(kernel_path);
  }

  let () = exec_vm(args, &init_path, &unlink_paths)?;
  Ok(())
}


#[cfg(test)]
mod tests {
  use super::*;


  fn host_env() -> Vec<(OsString, OsString)> {
    vec![
      (OsString::from("PATH"), OsString::from("/usr/bin")),
      (OsString::from("USER"), OsString::from("alice")),
      (OsString::from("HOSTNAME"), OsString::from("myhost")),
      (OsString::from("HOME"), OsString::from("/home/alice")),
      (OsString::from("VMSH_REDIRECT"), OsString::from("3")),
      (
        OsString::from("VMSH_KERNEL"),
        OsString::from("/boot/vmlinuz"),
      ),
      (OsString::from("KRUN_LOG_LEVEL"), OsString::from("debug")),
      (OsString::from("EDITOR"), OsString::from("vim")),
    ]
  }


  /// Test that without environment variable related flags set,
  /// [`build_env_content`] produces an empty env var file.
  #[test]
  fn no_flags_empty_output() {
    let all_envs = false;
    let content = build_env_content(&[], all_envs, Vec::<(OsString, OsString)>::new());
    assert!(content.is_empty());
  }

  /// Check that `all_envs` exports non-blocked host vars.
  #[test]
  fn all_envs_exports_host() {
    let all_envs = true;
    let content = build_env_content(&[], all_envs, host_env());
    let text = String::from_utf8(content).unwrap();
    assert!(text.contains("PATH=/usr/bin\n"));
    assert!(text.contains("USER=alice\n"));
    assert!(text.contains("EDITOR=vim\n"));
  }

  /// Verify that `all_envs` excludes `VMSH_*` and `KRUN_*` prefixed
  /// variables.
  #[test]
  fn all_envs_skips_blocked() {
    let all_envs = true;
    let content = build_env_content(&[], all_envs, host_env());
    let text = String::from_utf8(content).unwrap();
    assert!(text.contains("HOSTNAME=myhost\n"));
    assert!(text.contains("HOME=/home/alice\n"));
    assert!(!text.contains("VMSH_REDIRECT="));
    assert!(!text.contains("VMSH_KERNEL="));
    assert!(!text.contains("KRUN_LOG_LEVEL="));
  }

  /// Test that `--env=KEY` resolves its value from the host
  /// environment.
  #[test]
  fn env_key_resolves_from_host() {
    let all_envs = false;
    let content = build_env_content(&["PATH".to_string()], all_envs, host_env());
    let text = String::from_utf8(content).unwrap();
    assert_eq!(text, "PATH=/usr/bin\n");
  }

  /// Check that `--env=KEY=VALUE` uses the provided value.
  #[test]
  fn env_key_value_explicit() {
    let all_envs = false;
    let content = build_env_content(
      &["FOO=bar".to_string()],
      all_envs,
      Vec::<(OsString, OsString)>::new(),
    );
    let text = String::from_utf8(content).unwrap();
    assert_eq!(text, "FOO=bar\n");
  }

  /// Verify that a bare key missing from the host env is silently
  /// skipped.
  #[test]
  fn env_key_missing_skipped() {
    let all_envs = false;
    let content = build_env_content(&["NONEXISTENT".to_string()], all_envs, host_env());
    assert!(content.is_empty());
  }

  /// Test that `--env=KEY=VALUE` overrides the same key from
  /// `all_envs`.
  #[test]
  fn env_overrides_all_envs() {
    let all_envs = true;
    let content = build_env_content(&["PATH=custom".to_string()], all_envs, host_env());
    let text = String::from_utf8(content).unwrap();
    assert!(text.contains("PATH=custom\n"));
    assert!(!text.contains("PATH=/usr/bin\n"));
  }

  /// Check that a key in both `all_envs` and `--env` produces only one
  /// entry.
  #[test]
  fn no_duplicates() {
    let all_envs = true;
    let content = build_env_content(&["PATH".to_string()], all_envs, host_env());
    let text = String::from_utf8(content).unwrap();
    let count = text.matches("PATH=").count();
    assert_eq!(count, 1);
  }
}
