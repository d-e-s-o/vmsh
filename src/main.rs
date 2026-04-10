//! Transparently run a shell (or other binary) in a VM.

mod args;

use std::cell::LazyCell;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::env;
use std::env::temp_dir;
use std::ffi::c_char;
use std::ffi::CString;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::fs::remove_file;
use std::fs::write;
use std::mem::MaybeUninit;
use std::ops::Deref;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::ffi::OsStringExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::ptr;

use anyhow::ensure;
use anyhow::Context as _;
use anyhow::Result;

use clap::Parser;

use vmsh::detect_kernel_format;

use crate::args::Args;


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


/// Build the content of the environment file from CLI flags and host
/// env.
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


/// Write host environment variables to a file for the guest init to load.
///
/// Returns `None` when there are no variables to forward, skipping the
/// file creation entirely.
fn write_env_file(
  path: &Path,
  env_args: &[String],
  all_envs: bool,
) -> Result<Option<CleanupGuard>> {
  let buf = build_env_content(env_args, all_envs, env::vars_os());
  if buf.is_empty() {
    return Ok(None);
  }
  let () = fs::write(path, &buf)
    .with_context(|| format!("failed to write env file to `{}`", path.display()))?;
  let guard = CleanupGuard(Some(path.to_path_buf()));
  Ok(Some(guard))
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


fn set_exec(ctx: u32, command: Vec<String>, env_file: Option<&Path>) -> Result<()> {
  // Provide defaults for some relevant variables, but these will be
  // overwritten by any user provided values (present in `env_file`).
  let hostname = c"HOSTNAME=krun-boot";
  let home = c"HOME=/root";

  // Count how many of stdin/stdout/stderr are non-terminal. `libkrun`
  // creates virtio console ports for redirected FDs; let the guest init
  // know how many to expect so it can wait for them.
  // SAFETY: `isatty` is always safe to call.
  let redirect_count = unsafe { (libc::isatty(libc::STDIN_FILENO) == 0) as u8 }
    + unsafe { (libc::isatty(libc::STDOUT_FILENO) == 0) as u8 }
    + unsafe { (libc::isatty(libc::STDERR_FILENO) == 0) as u8 };

  let mut env_ptrs = vec![hostname.as_ptr(), home.as_ptr()];

  let env_file_env;
  if let Some(path) = env_file {
    env_file_env = CString::new(format!("VMSH_ENV_FILE={}", path.display()))?;
    let () = env_ptrs.push(env_file_env.as_ptr());
  }

  let redirect_env;
  if redirect_count > 0 {
    redirect_env = CString::new(format!("VMSH_REDIRECT={redirect_count}"))?;
    let () = env_ptrs.push(redirect_env.as_ptr());
  }

  let () = env_ptrs.push(ptr::null());

  // SAFETY: `ctx` is a valid krun context and `env_ptrs` is a valid
  //         NUL-terminated pointer array.
  let rc = unsafe { krun::krun_set_env(ctx, env_ptrs.as_ptr()) };
  ensure!(rc >= 0, "failed to set environment");

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


fn exec_vm(args: Args, init_guest_path: &Path) -> Result<()> {
  let Args {
    kernel,
    cpus,
    memory,
    command,
    env_vars,
    all_envs,
    verbosity,
  } = args;

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

  // Disable TSI (Transparent Socket Impersonation) by explicitly
  // configuring vsock with no TSI flags. Without this, libkrun's
  // implicit vsock adds `tsi_hijack` to the kernel command line, the
  // handling of which requires custom kernel patches. Without said
  // patches, the kernel passes through the argument to init as an
  // unknown parameter.
  // TODO: Networking support will likely need to add vsock support
  //       back.
  let rc = krun::krun_disable_implicit_vsock(ctx);
  ensure!(rc >= 0, "failed to disable implicit vsock");
  let rc = krun::krun_add_vsock(ctx, 0);
  ensure!(rc >= 0, "failed to add vsock device");

  let () = set_kernel(ctx, kernel, init_guest_path, verbosity)?;

  let c_rootfs = c"/";
  // SAFETY: `ctx` is a valid krun context and `c_rootfs` is a valid
  //         NUL-terminated string.
  let rc = unsafe { krun::krun_set_root(ctx, c_rootfs.as_ptr()) };
  ensure!(rc >= 0, "failed to set root filesystem");

  let env_path = temp_dir().join(format!("vmsh-env-{}", process::id()));
  let _env_guard = write_env_file(&env_path, &env_vars, all_envs)?;
  let env_file = _env_guard.as_deref();
  let () = set_exec(ctx, command, env_file)?;

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

  ensure!(
    args.kernel.exists(),
    "failed to find kernel at `{}`",
    args.kernel.display()
  );

  let () = set_rlimits()?;

  // Deploy init binary to `/tmp/` which is typically writable and
  // visible inside the guest via virtiofs.
  let init_filename = format!("vmsh-init-{}", process::id());
  let init_path = temp_dir().join(&init_filename);
  let _guard = deploy_init_binary(&init_path)?;

  let () = exec_vm(args, &init_path)?;
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
