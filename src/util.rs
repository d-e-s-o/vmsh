use std::fs::File;
use std::io::Read as _;
use std::path::Path;

use anyhow::Context as _;
use anyhow::Result;


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
    _ => anyhow::bail!(
      "unrecognized kernel format (magic: {magic:02x?}) for `{}`",
      path.display()
    ),
  };
  Ok(format)
}
