//! Minimal init process for vmsh.

use std::env;
use std::ffi::CStr;
use std::ffi::c_char;
use std::ffi::c_int;
use std::ffi::c_short;
use std::ffi::c_ulong;
use std::fs;
use std::fs::DirBuilder;
use std::fs::File;
use std::fs::read_dir;
use std::io;
use std::io::BufRead as _;
use std::io::BufReader;
use std::mem;
use std::mem::MaybeUninit;
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::fs::symlink;
use std::os::unix::io::AsRawFd as _;
use std::os::unix::io::FromRawFd as _;
use std::os::unix::io::IntoRawFd as _;
use std::os::unix::io::OwnedFd;
use std::path::Path;
use std::path::PathBuf;
use std::ptr;
use std::thread::sleep;
use std::time::Duration;

use libc::_exit;
use libc::AF_INET;
use libc::EBUSY;
use libc::ENOENT;
use libc::IFF_UP;
use libc::MS_NODEV;
use libc::MS_NOEXEC;
use libc::MS_NOSUID;
use libc::MS_RELATIME;
use libc::SIOCSIFFLAGS;
use libc::SOCK_DGRAM;
use libc::STDERR_FILENO;
use libc::STDIN_FILENO;
use libc::STDOUT_FILENO;
use libc::close;
use libc::dup2;
use libc::execvp;
use libc::ifreq;
use libc::ioctl;
use libc::mount;
use libc::socket;
use libc::statfs;


/// Ioctl number used by libkrun's virtiofs to communicate exit codes.
const KRUN_EXIT_CODE_IOCTL: c_ulong = 0x7602;

/// virtiofs magic number from `statfs`.
const VIRTIOFS_MAGIC: c_ulong = 0x6573_5546;


#[allow(dead_code)]
fn mkdir_p(path: &str) {
  let _ = DirBuilder::new().mode(0o755).create(path);
}

#[allow(dead_code)]
fn mount_or_err(
  source: &CStr,
  target: &CStr,
  fstype: &CStr,
  flags: c_ulong,
) -> Result<(), io::Error> {
  // SAFETY: All arguments are valid NUL-terminated strings.
  let ret = unsafe {
    mount(
      source.as_ptr(),
      target.as_ptr(),
      fstype.as_ptr(),
      flags,
      ptr::null(),
    )
  };
  if ret < 0 {
    let err = io::Error::last_os_error();
    // `EBUSY` on `/dev` is OK (already mounted).
    if target == c"/dev" && err.raw_os_error() == Some(EBUSY) {
      return Ok(());
    }
    eprintln!(
      "vmsh-init: mount({}): {err}",
      target.to_str().unwrap_or("?")
    );
    Err(err)
  } else {
    Ok(())
  }
}

/// Mount with a warning on failure (non-fatal).
#[allow(dead_code)]
fn mount_or_warn(
  source: Option<&CStr>,
  target: &CStr,
  fstype: Option<&CStr>,
  flags: c_ulong,
  label: &str,
) {
  let src = source.map_or(ptr::null(), CStr::as_ptr);
  let fst = fstype.map_or(ptr::null(), CStr::as_ptr);
  // SAFETY: All non-null arguments are valid NUL-terminated strings.
  let ret = unsafe { mount(src, target.as_ptr(), fst, flags, ptr::null()) };
  if ret < 0 {
    let err = io::Error::last_os_error();
    eprintln!("vmsh-init: warning: mount {label}: {err}");
  }
}

/// Check whether the kernel supports a filesystem type.
#[allow(dead_code)]
fn kernel_supports_fs(fstype: &str) -> bool {
  let content = match fs::read_to_string("/proc/filesystems") {
    Ok(c) => c,
    Err(_) => return false,
  };

  for line in content.lines() {
    // Each line is either "nodev\t<fstype>" or "\t<fstype>".
    if let Some(name) = line.split_once('\t').map(|(_, n)| n)
      && name == fstype
    {
      return true;
    }
  }
  false
}

/// Mount various filesystems.
#[allow(dead_code)]
fn mount_filesystems() -> Result<(), io::Error> {
  // Create level-1 directories.
  let () = mkdir_p("/dev");
  let () = mkdir_p("/proc");
  let () = mkdir_p("/sys");

  let () = mount_or_err(c"devtmpfs", c"/dev", c"devtmpfs", MS_RELATIME)?;
  let flags = MS_NODEV | MS_NOEXEC | MS_NOSUID | MS_RELATIME;
  let () = mount_or_err(c"proc", c"/proc", c"proc", flags)?;
  let () = mount_or_err(c"sysfs", c"/sys", c"sysfs", flags)?;

  if kernel_supports_fs("debugfs") {
    let () = mkdir_p("/sys/kernel/debug");
    let () = mount_or_warn(
      Some(c"debugfs"),
      c"/sys/kernel/debug",
      Some(c"debugfs"),
      flags,
      "debugfs",
    );
  }

  if kernel_supports_fs("tracefs") {
    let () = mkdir_p("/sys/kernel/tracing");
    let () = mount_or_warn(
      Some(c"tracefs"),
      c"/sys/kernel/tracing",
      Some(c"tracefs"),
      flags,
      "tracefs",
    );
  }

  if kernel_supports_fs("bpf") {
    let () = mkdir_p("/sys/fs/bpf");
    let () = mount_or_warn(Some(c"bpffs"), c"/sys/fs/bpf", Some(c"bpf"), flags, "bpffs");
  }

  let () = mkdir_p("/sys/fs/cgroup");
  let () = mount_or_warn(
    Some(c"cgroup2"),
    c"/sys/fs/cgroup",
    Some(c"cgroup2"),
    flags,
    "cgroup2",
  );

  // Create level-2 directories (after devtmpfs is mounted).
  let () = mkdir_p("/dev/pts");
  let () = mkdir_p("/dev/shm");

  let flags = MS_NOEXEC | MS_NOSUID | MS_RELATIME;
  let () = mount_or_err(c"devpts", c"/dev/pts", c"devpts", flags)?;
  let () = mount_or_err(c"tmpfs", c"/dev/shm", c"tmpfs", flags)?;

  let _result = symlink("/proc/self/fd", "/dev/fd");
  let _result = symlink("/proc/self/fd/0", "/dev/stdin");
  let _result = symlink("/proc/self/fd/1", "/dev/stdout");
  let _result = symlink("/proc/self/fd/2", "/dev/stderr");

  Ok(())
}

/// Try to bring up the loopback device.
#[allow(dead_code)]
fn bring_up_loopback() {
  // SAFETY: Creating a UDP socket is always safe.
  let sockfd = unsafe { socket(AF_INET, SOCK_DGRAM, 0) };
  if sockfd < 0 {
    return;
  }
  // SAFETY: `socket` succeeded, so `sockfd` is a valid file descriptor
  //         that we own.
  let sock = unsafe { OwnedFd::from_raw_fd(sockfd) };

  // SAFETY: zero-initializing `ifreq` is safe.
  let mut ifr = unsafe { mem::zeroed::<ifreq>() };
  let () = ifr.ifr_name[..2].copy_from_slice(&[b'l' as c_char, b'o' as c_char]);
  // SAFETY: `ifr` is a valid, zero-initialized `ifreq` struct.
  unsafe { ifr.ifr_ifru.ifru_flags |= IFF_UP as c_short };

  // SAFETY: `sock` is a valid socket, `ifr` is a valid `ifreq` struct.
  let _rc = unsafe { ioctl(sock.as_raw_fd(), SIOCSIFFLAGS, &ifr) };
}

/// Find a named virtio console port.
///
/// The function scans `/sys/class/virtio-ports/` for a port whose name
/// matches `target_name`. It returns the device path (e.g.
/// `/dev/vport0p1`) on success, polling up to `max_attempts` times, 1
/// ms apart.
#[allow(dead_code)]
fn find_virtio_port(target_name: &str, max_attempts: i32) -> Option<PathBuf> {
  let base = Path::new("/sys/class/virtio-ports");
  for attempt in 0..max_attempts {
    if attempt > 0 {
      let () = sleep(Duration::from_millis(1));
    }

    let entries = match read_dir(base) {
      Ok(e) => e,
      Err(_) => continue,
    };

    for entry in entries.flatten() {
      let name_path = entry.path().join("name");
      let port_name = match fs::read_to_string(&name_path) {
        Ok(n) => n,
        Err(_) => continue,
      };

      if port_name.trim_end_matches(['\n', '\r']) == target_name {
        return Some(Path::new("/dev").join(entry.file_name()));
      }
    }
  }
  None
}


#[allow(dead_code)]
fn set_exit_code(code: c_int) {
  let mut buf = MaybeUninit::<statfs>::uninit();
  // SAFETY: "/" is a valid NUL-terminated path and `buf` is writable.
  let rc = unsafe { statfs(c"/".as_ptr(), buf.as_mut_ptr()) };
  if rc != 0 {
    eprintln!("vmsh-init: warning: could not statfs /");
    return;
  }
  // SAFETY: `statfs` succeeded, so `buf` is initialized.
  let buf = unsafe { buf.assume_init() };
  if buf.f_type as c_ulong != VIRTIOFS_MAGIC {
    return;
  }

  let file = match File::open("/") {
    Ok(f) => f,
    Err(_) => {
      eprintln!("vmsh-init: warning: could not open / for exit code ioctl");
      return;
    },
  };

  // SAFETY: `file` is a valid, open file descriptor.
  let _rc = unsafe { ioctl(file.as_raw_fd(), KRUN_EXIT_CODE_IOCTL, code) };
}


/// Load environment variables from a virtio console port.
///
/// The kernel command line has a limited number of env var slots, so we
/// pass bulk env vars (other than `VMSH_*`) through a dedicated virtio
/// console port backed by a memfd on the host. The file format is one
/// `KEY=VALUE` per line.
#[allow(dead_code)]
fn load_env_vars() {
  if env::var_os("VMSH_ENV_PORT").is_none() {
    return;
  }

  let dev_path = match find_virtio_port("krun-env", 500) {
    Some(p) => p,
    None => return,
  };

  let file = match File::open(&dev_path) {
    Ok(f) => f,
    Err(_) => return,
  };

  let reader = BufReader::new(file);
  for line in reader.lines() {
    let line = match line {
      Ok(l) => l,
      Err(_) => break,
    };
    if line.is_empty() {
      continue;
    }

    if let Some((key, value)) = line.split_once('=') {
      let () = unsafe { env::set_var(key, value) };
    }
  }
}


/// Redirect stdin/stdout/stderr to virtio console ports.
///
/// `VMSH_STDIN`, `VMSH_STDOUT`, and `VMSH_STDERR` signal which file
/// descriptors have been redirected and need a corresponding virtio
/// console port. Each port is discovered and `dup2`'d onto the
/// corresponding file descriptor.
#[allow(dead_code)]
fn setup_redirects() {
  #[derive(Clone, Copy)]
  struct Redirect {
    port_name: &'static str,
    target_fd: c_int,
    read: bool,
    done: bool,
  }

  let mut redirects = [Redirect {
    port_name: "",
    target_fd: 0,
    read: false,
    done: false,
  }; 3];
  let mut count = 0;

  let mut push = |port_name, target_fd, read| {
    redirects[count] = Redirect {
      port_name,
      target_fd,
      read,
      done: false,
    };
    count += 1;
  };

  if env::var_os("VMSH_STDIN").is_some() {
    let () = push("krun-stdin", STDIN_FILENO, true);
  }
  if env::var_os("VMSH_STDOUT").is_some() {
    let () = push("krun-stdout", STDOUT_FILENO, false);
  }
  if env::var_os("VMSH_STDERR").is_some() {
    let () = push("krun-stderr", STDERR_FILENO, false);
  }

  if count == 0 {
    return;
  }

  // Poll all pending ports concurrently, 1 ms apart, up to 500 ms.
  let redirects = &mut redirects[..count];
  let mut remaining = count;

  for attempt in 0..500 {
    if remaining == 0 {
      break;
    }
    if attempt > 0 {
      let () = sleep(Duration::from_millis(1));
    }

    for redirect in redirects.iter_mut() {
      if redirect.done {
        continue;
      }

      let dev_path = match find_virtio_port(redirect.port_name, 1) {
        Some(p) => p,
        None => continue,
      };

      let result = if redirect.read {
        File::open(&dev_path)
      } else {
        fs::OpenOptions::new().write(true).open(&dev_path)
      };

      if let Ok(file) = result {
        let fd = file.into_raw_fd();
        // SAFETY: both `fd` and `target_fd` are valid file descriptors.
        let _rc = unsafe { dup2(fd, redirect.target_fd) };
        if fd != redirect.target_fd {
          // SAFETY: `fd` is a valid file descriptor.
          let _rc = unsafe { close(fd) };
        }
      }

      redirect.done = true;
      remaining -= 1;
    }
  }
}


#[allow(dead_code)]
fn do_exec(exec_path: &CStr, exec_argv: &[*const c_char]) -> ! {
  // SAFETY: `exec_path` and `exec_argv` are valid NUL-terminated
  // strings. `exec_argv` is NULL-terminated.
  let _rc = unsafe { execvp(exec_path.as_ptr(), exec_argv.as_ptr()) };

  // If exec returns, it failed.
  let err = io::Error::last_os_error();
  let code = if err.raw_os_error() == Some(ENOENT) {
    127
  } else {
    126
  };
  eprintln!(
    "vmsh-init: couldn't execute '{}': {err}",
    exec_path.to_str().unwrap_or("?")
  );
  let () = set_exit_code(code);
  // SAFETY: Exiting the process is always safe at this point.
  unsafe { _exit(code) }
}


fn main() {}
