//! Transparently run a shell (or other binary) in a VM.

mod args;
mod embed;

use std::cell::LazyCell;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::env;
use std::env::temp_dir;
use std::ffi::CStr;
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

use crate::args::Args;
use crate::args::Command;
use crate::args::RunArgs;


/// The action to take for a given filesystem path in the guest.
#[derive(Clone, Debug, PartialEq)]
enum ShareAction {
  /// Share the path read-write via a virtiofs device.
  ReadWrite,
  /// Share the path read-only via a virtiofs device. This is only
  /// needed for explicit `--share-ro` requests; the default read-only
  /// root already covers most paths.
  ReadOnly,
  /// Hide the path by overlaying an empty tmpfs in the host mount
  /// namespace, so the guest cannot bypass it.
  Hide,
}


/// Compute the final list of filesystem shares and hide actions.
///
/// Starts with defaults (cwd rw, /tmp rw), then applies user flags
/// in fixed group order: `--share-rw`, then `--share-ro`, then
/// `--hide`. When the same path appears in multiple groups, the
/// later group wins (`--hide` > `--share-ro` > `--share-rw`).
///
/// Returns the list of `(path, action)` pairs and whether the root
/// should be read-only (false only if `--share-rw /` was given).
fn compute_shares(
  cwd: &Path,
  share_rw: &[PathBuf],
  share_ro: &[PathBuf],
  hide: &[PathBuf],
) -> (Vec<(PathBuf, ShareAction)>, bool) {
  // We need to process arguments in the order they were given on the
  // command line. Since clap collects each flag into its own Vec, we
  // reconstruct the ordering by interleaving them according to their
  // original positions. However, clap doesn't expose argument positions
  // easily, so we use a simpler model: defaults first, then --share-rw,
  // then --share-ro, then --hide. This means --hide always beats
  // --share-rw for the same path (which is the last-wins semantics
  // applied to the flag groups in this fixed order).
  //
  // For truly order-dependent semantics we would need a single
  // `--share` flag with inline mode syntax, but the current approach is
  // simpler and covers the expected use cases.
  let mut map = Vec::<(PathBuf, ShareAction)>::new();

  // Apply defaults.
  let () = map.push((cwd.to_path_buf(), ShareAction::ReadWrite));
  let () = map.push((PathBuf::from("/tmp"), ShareAction::ReadWrite));

  // Apply user overrides in flag-group order.
  for path in share_rw {
    let () = map.push((path.clone(), ShareAction::ReadWrite));
  }
  for path in share_ro {
    let () = map.push((path.clone(), ShareAction::ReadOnly));
  }
  for path in hide {
    let () = map.push((path.clone(), ShareAction::Hide));
  }

  // Deduplicate: last-wins. Walk backwards, keep only the first
  // occurrence of each canonical path.
  let mut seen = Vec::new();
  let mut deduped = Vec::new();
  for (path, action) in map.into_iter().rev() {
    if seen.iter().any(|p: &PathBuf| p == &path) {
      continue;
    }
    let () = seen.push(path.clone());
    let () = deduped.push((path, action));
  }
  let () = deduped.reverse();

  // The root path is always handled via the root virtiofs device
  // (krun_add_virtiofs3 with KRUN_FS_ROOT_TAG), so remove it from the
  // per-share list and derive the read-only flag from the action.
  let root = Path::new("/");
  let root_action = deduped
    .iter()
    .find(|(p, _)| p == root)
    .map(|(_, a)| a.clone());
  let () = deduped.retain(|(p, _)| p != root);
  let root_read_only = !matches!(root_action, Some(ShareAction::ReadWrite));

  (deduped, root_read_only)
}


/// Format share metadata as an environment variable value.
///
/// Format: `tag:path:mode[;tag:path:mode]...`
/// where mode is `rw` or `ro`.
fn format_shares_env(shares: &[(PathBuf, ShareAction)]) -> Vec<u8> {
  let mut out = Vec::new();
  let mut idx = 0usize;
  for (path, action) in shares {
    let mode = match action {
      ShareAction::ReadWrite => &b"rw"[..],
      ShareAction::ReadOnly => &b"ro"[..],
      ShareAction::Hide => continue,
    };
    if !out.is_empty() {
      out.push(b';');
    }
    out.extend_from_slice(format!("vmsh-{idx}").as_bytes());
    out.push(b':');
    out.extend_from_slice(path.as_os_str().as_bytes());
    out.push(b':');
    out.extend_from_slice(mode);
    idx += 1;
  }
  out
}


/// Enter a user + mount namespace and hide the given paths.
///
/// Creates a new user namespace (to avoid requiring root) and a new
/// mount namespace, then mounts empty tmpfs over each path. The
/// virtiofs daemon in libkrun inherits this namespace, so the guest
/// cannot see the hidden paths.
fn enter_hiding_namespace(paths: &[PathBuf]) -> Result<()> {
  // SAFETY: `getuid` and `getgid` are always safe.
  let uid = unsafe { libc::getuid() };
  let gid = unsafe { libc::getgid() };

  // SAFETY: `unshare` is safe to call.
  let rc = unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) };
  if rc < 0 {
    let err = std::io::Error::last_os_error();
    anyhow::bail!(
      "failed to create user/mount namespace for --hide: {err}\n\
       hint: check `sysctl kernel.unprivileged_userns_clone` or run with CAP_SYS_ADMIN"
    );
  }

  // Set up identity UID/GID mappings so file access semantics are preserved.
  write("/proc/self/setgroups", "deny").context("failed to write /proc/self/setgroups")?;
  write("/proc/self/uid_map", format!("{uid} {uid} 1"))
    .context("failed to write /proc/self/uid_map")?;
  write("/proc/self/gid_map", format!("{gid} {gid} 1"))
    .context("failed to write /proc/self/gid_map")?;

  // Prevent mount events from propagating back to the parent namespace.
  // SAFETY: "/" is a valid path, no data argument needed for remount.
  let rc = unsafe { libc::mount(ptr::null(), c"/".as_ptr(), ptr::null(), libc::MS_REC | libc::MS_PRIVATE, ptr::null()) };
  ensure!(rc == 0, "failed to make mounts private: {}", std::io::Error::last_os_error());

  for path in paths {
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: All arguments are valid NUL-terminated strings.
    let rc = unsafe {
      libc::mount(c"tmpfs".as_ptr(), c_path.as_ptr(), c"tmpfs".as_ptr(), 0, c"size=0".as_ptr().cast())
    };
    ensure!(
      rc == 0,
      "failed to mount tmpfs over `{}`: {}",
      path.display(),
      std::io::Error::last_os_error()
    );
  }

  Ok(())
}


/// The virtiofs tag used by libkrun for the root filesystem device.
///
/// Corresponds to `KRUN_FS_ROOT_TAG` in `libkrun.h`.
const KRUN_FS_ROOT_TAG: &CStr = c"/dev/root";

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
  shares_env: Option<&[u8]>,
) -> Result<()> {
  // Provide defaults for some relevant variables, but these will be
  // overwritten by any user provided values (present on the env port).
  let hostname = c"HOSTNAME=krun-boot";
  let home = c"HOME=/root";

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

  let shares_env_cstr;
  if let Some(val) = shares_env {
    let mut buf = b"VMSH_SHARES=".to_vec();
    let () = buf.extend_from_slice(val);
    shares_env_cstr = CString::new(buf)?;
    let () = env_ptrs.push(shares_env_cstr.as_ptr());
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
    share_rw,
    share_ro,
    hide,
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

  // Compute filesystem shares. By default, the root is read-only with
  // the cwd and /tmp shared read-write. User flags override this.
  let cwd = env::current_dir()
    .and_then(|p| p.canonicalize())
    .context("failed to determine current directory")?;
  let (shares, root_read_only) = compute_shares(&cwd, &share_rw, &share_ro, &hide);

  // Hide paths by overlaying empty tmpfs in a new mount namespace.
  // Must happen before virtiofs setup so the daemon inherits the view.
  let hide_paths: Vec<PathBuf> = shares
    .iter()
    .filter(|(_, a)| matches!(a, ShareAction::Hide))
    .map(|(p, _)| p.clone())
    .collect();
  if !hide_paths.is_empty() {
    for path in &hide_paths {
      ensure!(
        !cwd.starts_with(path),
        "`--hide {}` would hide the current working directory `{}`",
        path.display(),
        cwd.display()
      );
      for &upath in unlink_paths {
        ensure!(
          !upath.starts_with(path),
          "`--hide {}` would hide `{}`",
          path.display(),
          upath.display()
        );
      }
    }
    enter_hiding_namespace(&hide_paths)?;
  }

  // Set up the root filesystem via the well-known root virtiofs tag.
  // By default the root is read-only; `--share-rw /` makes it rw.
  let c_rootfs = c"/";
  // Use the same 512 MiB DAX window that `krun_set_root` uses.
  const ROOT_SHM_SIZE: u64 = 1 << 29;
  // SAFETY: `ctx` is a valid krun context, `KRUN_FS_ROOT_TAG` and
  //         `c_rootfs` are valid NUL-terminated strings.
  let rc = unsafe {
    krun::krun_add_virtiofs3(
      ctx,
      KRUN_FS_ROOT_TAG.as_ptr(),
      c_rootfs.as_ptr(),
      ROOT_SHM_SIZE,
      root_read_only,
    )
  };
  ensure!(rc >= 0, "failed to set root filesystem");

  // Add virtiofs devices for each share (rw or explicit ro).
  let mut share_idx = 0u32;
  for (path, action) in &shares {
    let read_only = match action {
      ShareAction::ReadWrite => false,
      ShareAction::ReadOnly => true,
      ShareAction::Hide => continue,
    };
    let tag = format!("vmsh-{share_idx}");
    let c_tag = CString::new(tag)?;
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `ctx` is a valid krun context, `c_tag` and `c_path` are
    //         valid NUL-terminated strings.
    let rc =
      unsafe { krun::krun_add_virtiofs3(ctx, c_tag.as_ptr(), c_path.as_ptr(), 0, read_only) };
    ensure!(
      rc >= 0,
      "failed to add virtiofs share for `{}`",
      path.display()
    );
    share_idx += 1;
  }

  // Set the working directory to the host cwd.
  let c_workdir = CString::new(cwd.as_os_str().as_bytes())?;
  // SAFETY: `ctx` is a valid krun context and `c_workdir` is a valid
  //         NUL-terminated string.
  let rc = unsafe { krun::krun_set_workdir(ctx, c_workdir.as_ptr()) };
  ensure!(rc >= 0, "failed to set working directory");

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

  // Build share metadata for the guest init.
  let shares_env_val = format_shares_env(&shares);
  let shares_env = if shares_env_val.is_empty() {
    None
  } else {
    Some(shares_env_val.as_slice())
  };

  let () = set_exec(
    ctx,
    command,
    has_env_port,
    unlink_paths,
    shares_env,
  )?;

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


  /// Default shares include cwd (rw) and /tmp (rw), with root as ro.
  #[test]
  fn default_shares() {
    let cwd = PathBuf::from("/home/user/project");
    let (shares, root_ro) = compute_shares(&cwd, &[], &[], &[]);
    assert!(root_ro);
    assert_eq!(shares.len(), 2);
    assert_eq!(
      shares[0],
      (PathBuf::from("/home/user/project"), ShareAction::ReadWrite)
    );
    assert_eq!(shares[1], (PathBuf::from("/tmp"), ShareAction::ReadWrite));
  }

  /// `--share-rw /data` adds an rw entry.
  #[test]
  fn share_rw_adds_path() {
    let cwd = PathBuf::from("/home/user");
    let (shares, root_ro) = compute_shares(&cwd, &[PathBuf::from("/data")], &[], &[]);
    assert!(root_ro);
    assert!(shares.contains(&(PathBuf::from("/data"), ShareAction::ReadWrite)));
  }

  /// `--share-ro /opt` adds an ro entry.
  #[test]
  fn share_ro_adds_path() {
    let cwd = PathBuf::from("/home/user");
    let (shares, root_ro) = compute_shares(&cwd, &[], &[PathBuf::from("/opt")], &[]);
    assert!(root_ro);
    assert!(shares.contains(&(PathBuf::from("/opt"), ShareAction::ReadOnly)));
  }

  /// `--hide /tmp` removes the default /tmp share and replaces it
  /// with a hide action.
  #[test]
  fn hide_removes_default() {
    let cwd = PathBuf::from("/home/user");
    let (shares, _) = compute_shares(&cwd, &[], &[], &[PathBuf::from("/tmp")]);
    assert!(!shares.contains(&(PathBuf::from("/tmp"), ShareAction::ReadWrite)));
    assert!(shares.contains(&(PathBuf::from("/tmp"), ShareAction::Hide)));
  }

  /// Last-wins: `--share-ro /data --share-rw /data` results in rw.
  /// (In our fixed ordering, --share-rw comes before --share-ro,
  /// so --share-ro would win. But if both are rw, the last rw wins.)
  #[test]
  fn last_wins_same_flag_type() {
    let cwd = PathBuf::from("/home/user");
    // --share-rw /data appears first, but --share-ro /data comes
    // later in the fixed ordering.
    let (shares, _) = compute_shares(
      &cwd,
      &[PathBuf::from("/data")],
      &[PathBuf::from("/data")],
      &[],
    );
    // --share-ro group comes after --share-rw, so ro wins.
    assert!(shares.contains(&(PathBuf::from("/data"), ShareAction::ReadOnly)));
    assert!(!shares.contains(&(PathBuf::from("/data"), ShareAction::ReadWrite)));
  }

  /// `--share-rw /etc --hide /etc` results in /etc hidden
  /// (hide group comes last).
  #[test]
  fn last_wins_hide_over_share() {
    let cwd = PathBuf::from("/home/user");
    let (shares, _) = compute_shares(
      &cwd,
      &[PathBuf::from("/etc")],
      &[],
      &[PathBuf::from("/etc")],
    );
    assert!(shares.contains(&(PathBuf::from("/etc"), ShareAction::Hide)));
    assert!(!shares.contains(&(PathBuf::from("/etc"), ShareAction::ReadWrite)));
  }

  /// `--share-rw /` disables isolation (root is rw).
  #[test]
  fn share_rw_root_disables_isolation() {
    let cwd = PathBuf::from("/home/user");
    let (shares, root_ro) = compute_shares(&cwd, &[PathBuf::from("/")], &[], &[]);
    assert!(!root_ro);
    // The "/" entry itself is removed from shares.
    assert!(!shares.iter().any(|(p, _)| p == Path::new("/")));
  }

  /// `--share-ro /` is a no-op: root is already read-only and no
  /// redundant share entry is produced.
  #[test]
  fn share_ro_root_is_noop() {
    let cwd = PathBuf::from("/home/user");
    let (shares, root_ro) = compute_shares(&cwd, &[], &[PathBuf::from("/")], &[]);
    assert!(root_ro);
    assert!(!shares.iter().any(|(p, _)| p == Path::new("/")));
  }

  /// Verify the `VMSH_SHARES` env var format.
  #[test]
  fn share_metadata_format() {
    let shares = vec![
      (PathBuf::from("/home/user/project"), ShareAction::ReadWrite),
      (PathBuf::from("/tmp"), ShareAction::ReadWrite),
      (PathBuf::from("/opt"), ShareAction::ReadOnly),
      (PathBuf::from("/secret"), ShareAction::Hide),
    ];
    let env = format_shares_env(&shares);
    assert_eq!(
      env,
      b"vmsh-0:/home/user/project:rw;vmsh-1:/tmp:rw;vmsh-2:/opt:ro"
    );
  }

  /// Non-UTF-8 path bytes survive the share encoding round-trip.
  #[test]
  fn non_utf8_paths() {
    // 0x80 is not valid UTF-8 on its own.
    let rw_path = PathBuf::from(OsString::from_vec(b"/rw-\x80".to_vec()));
    let ro_path = PathBuf::from(OsString::from_vec(b"/ro-\x80".to_vec()));
    let hide_path = PathBuf::from(OsString::from_vec(b"/hide-\x80".to_vec()));

    let shares = vec![
      (rw_path, ShareAction::ReadWrite),
      (ro_path, ShareAction::ReadOnly),
      (hide_path, ShareAction::Hide),
    ];

    let env = format_shares_env(&shares);
    assert_eq!(env, b"vmsh-0:/rw-\x80:rw;vmsh-1:/ro-\x80:ro");
  }
}
