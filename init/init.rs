//! Minimal init process for vmsh.

use std::ffi::CStr;
use std::ffi::c_ulong;
use std::fs;
use std::fs::DirBuilder;
use std::io;
use std::os::unix::fs::DirBuilderExt as _;
use std::ptr;

use libc::EBUSY;
use libc::mount;


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

fn main() {}
