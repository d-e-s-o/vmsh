//! Transparently run a shell (or other binary) in a VM.

mod args;

use std::env;
use std::env::temp_dir;
use std::ffi::c_char;
use std::ffi::CString;
use std::ffi::OsString;
use std::fs::remove_file;
use std::fs::write;
use std::mem::MaybeUninit;
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

use crate::args::Args;


/// Embedded init binary.
const INIT_BINARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vmsh-init"));

/// Kernel format constant for `bzImage` (matches `KRUN_KERNEL_FORMAT_BZIMAGE`).
const KRUN_KERNEL_FORMAT_BZIMAGE: u32 = 6;


/// RAII guard to clean up the init binary on exit.
struct CleanupGuard(Option<PathBuf>);

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


fn set_kernel(ctx: u32, kernel: PathBuf, init_guest_path: &Path, verbosity: u8) -> Result<()> {
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
      KRUN_KERNEL_FORMAT_BZIMAGE,
      initramfs,
      c_cmdline.as_ptr(),
    )
  };
  ensure!(rc >= 0, "failed to set kernel (code {rc})");
  Ok(())
}


fn set_exec(ctx: u32, command: Vec<String>) -> Result<()> {
  let hostname = c"HOSTNAME=krun-boot";

  // Count how many of stdin/stdout/stderr are non-terminal. `libkrun`
  // creates virtio console ports for redirected FDs; let the guest init
  // know how many to expect so it can wait for them.
  // SAFETY: `isatty` is always safe to call.
  let redirect_count = unsafe { (libc::isatty(libc::STDIN_FILENO) == 0) as u8 }
    + unsafe { (libc::isatty(libc::STDOUT_FILENO) == 0) as u8 }
    + unsafe { (libc::isatty(libc::STDERR_FILENO) == 0) as u8 };

  let mut env_ptrs = vec![hostname.as_ptr()];

  let redirect_env;
  if redirect_count > 0 {
    redirect_env = CString::new(format!("VMSH_REDIRECT={redirect_count}"))?;
    let () = env_ptrs.push(redirect_env.as_ptr());
  }

  let home_env;
  if let Some(path) = env::var_os("HOME") {
    let mut path_var = OsString::from("HOME=");
    let () = path_var.push(&path);
    home_env = CString::new(path_var.into_vec())?;
    let () = env_ptrs.push(home_env.as_ptr());
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
    verbosity,
  } = args;

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

  let () = set_exec(ctx, command)?;

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
