use std::ffi::CStr;
use std::fs::File;
use std::io;
use std::io::Read as _;
use std::mem::MaybeUninit;
use std::path::Path;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;


/// Kernel image format, with discriminant values matching the `libkrun`
/// C constants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum KernelFormat {
  /// Uncompressed ELF (`KRUN_KERNEL_FORMAT_ELF`).
  Elf = 1,
  /// bzip2-compressed ELF (`KRUN_KERNEL_FORMAT_IMAGE_BZ2`).
  Bz2 = 3,
  /// gzip-compressed ELF (`KRUN_KERNEL_FORMAT_IMAGE_GZ`).
  Gz = 4,
  /// zstd-compressed ELF (`KRUN_KERNEL_FORMAT_IMAGE_ZSTD`).
  Zstd = 5,
}


/// Detect the kernel format by reading magic bytes from the file header.
pub fn detect_kernel_format(path: &Path) -> Result<KernelFormat> {
  let mut magic = [0u8; 4];
  let () = File::open(path)
    .and_then(|mut f| f.read_exact(&mut magic))
    .with_context(|| format!("failed to read kernel header from `{}`", path.display()))?;

  let format = match magic {
    [0x7f, b'E', b'L', b'F'] => KernelFormat::Elf,
    [0x1f, 0x8b, ..] => KernelFormat::Gz,
    [b'B', b'Z', b'h', _] => KernelFormat::Bz2,
    [0x28, 0xb5, 0x2f, 0xfd] => KernelFormat::Zstd,
    _ => bail!(
      "unrecognized kernel format (magic: {magic:02x?}) for `{}`",
      path.display()
    ),
  };
  Ok(format)
}


/// Retrieve the system's host name.
pub fn hostname() -> Result<String> {
  // POSIX defines `HOST_NAME_MAX` as 255, but we +1 them all.
  const HOST_NAME_MAX: usize = 256;

  let mut buf = MaybeUninit::<[u8; HOST_NAME_MAX]>::uninit();
  // SAFETY: The `buf` pointer is valid as it is derived from a
  //         reference.
  let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), HOST_NAME_MAX) };
  if rc != 0 {
    return Err(io::Error::last_os_error()).context("failed to retrieve host name")
  }
  // Make sure to NUL terminate unconditionally. Some systems may not
  // do that when host name exceeds buffer size.
  let _nul = <MaybeUninit<[_; _]> as AsMut<[MaybeUninit<_>; _]>>::as_mut(&mut buf)
    .last_mut()
    .unwrap()
    .write(b'\0');

  // SAFETY: The `buf` pointer is valid as it is derived from a
  //         reference. Contents are guaranteed to be initialized
  //         because that's what `gethostname` does.
  let hostname = unsafe { CStr::from_ptr(buf.as_ptr().cast()) }
    .to_string_lossy()
    .into_owned();
  Ok(hostname)
}


#[cfg(test)]
mod tests {
  use super::*;


  /// Check that we can retrieve the system's host name.
  #[test]
  fn hostname_retrieval() {
    let host_name = hostname().unwrap();
    assert_ne!(host_name, "");
  }
}
