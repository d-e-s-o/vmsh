//! Embed and extract a kernel image in the vmsh binary.
//!
//! The binary layout after embedding:
//! ```text
//! [original ELF] [kernel bytes] [u64 LE: kernel size] [magic: "VMSHKRNL"]
//! ```

use std::env;
use std::fs;
use std::fs::File;
use std::io::Read as _;
use std::io::Seek as _;
use std::io::SeekFrom;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::process::Command;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::ensure;

use crate::CleanupGuard;


/// Trailer magic written after the kernel payload.
const MAGIC: &[u8; 8] = b"VMSHKRNL";
/// Total trailer size: 8 bytes kernel size + 8 bytes magic.
const TRAILER_SIZE: u64 = 16;
/// Environment variable used to tell a re-exec'd copy to clean itself
/// up after it finishes writing.
const CLEANUP_ENV: &str = "VMSH_EMBED_CLEANUP";


/// Embed a kernel image into a copy of this binary.
///
/// If `output` is `None`, the current executable is overwritten in
/// place.
pub(crate) fn embed_kernel(kernel: &Path, output: Option<&Path>) -> Result<()> {
  let self_path = env::current_exe().context("failed to locate own executable")?;

  // When no explicit output is given, we need to overwrite ourselves.
  // Linux returns `ETXTBSY` for writes to a running executable, so we
  // copy ourselves to a temp file and exec that copy with `-o` set.
  if output.is_none() {
    let tmp = env::temp_dir().join(format!("vmsh-embed-{}", process::id()));
    let () = fs::copy(&self_path, &tmp)
      .map(|_| ())
      .with_context(|| format!("failed to copy self to `{}`", tmp.display()))?;
    let () = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
      .with_context(|| format!("failed to set permissions on `{}`", tmp.display()))?;

    let err = Command::new(&tmp)
      .arg("embed")
      .arg(kernel)
      .arg("-o")
      .arg(&self_path)
      .env(CLEANUP_ENV, &tmp)
      .exec();
    // `exec` only returns on error.
    return Err(err).with_context(|| format!("failed to re-exec via `{}`", tmp.display()));
  }

  let mut binary = fs::read(&self_path).context("failed to read own executable")?;
  // Strip an existing embedded kernel, if any.
  let () = strip_trailer(&mut binary);

  let kernel_data =
    fs::read(kernel).with_context(|| format!("failed to read kernel at `{}`", kernel.display()))?;
  ensure!(!kernel_data.is_empty(), "kernel file is empty");

  // Append: [kernel bytes] [kernel_size as u64 LE] [magic]
  let kernel_size = kernel_data.len() as u64;
  let () = binary.extend_from_slice(&kernel_data);
  let () = binary.extend_from_slice(&kernel_size.to_le_bytes());
  let () = binary.extend_from_slice(MAGIC);

  // SANITY: If `output` were `None` we'd have re-exec'd above.
  let out_path = output.unwrap();
  let () = fs::write(out_path, &binary)
    .with_context(|| format!("failed to write output to `{}`", out_path.display()))?;

  let () = fs::set_permissions(out_path, fs::Permissions::from_mode(0o755))
    .with_context(|| format!("failed to set permissions on `{}`", out_path.display()))?;

  let kernel_kib = kernel_size / 1024;
  eprintln!(
    "embedded {kernel_kib} KiB kernel from `{}` into `{}`",
    kernel.display(),
    out_path.display(),
  );

  // If we are the re-exec'd copy, clean up the temp binary now that
  // we have finished writing.
  if let Some(temp) = env::var_os(CLEANUP_ENV) {
    let () = fs::remove_file(&temp)
      .with_context(|| format!("failed to clean up temporary file `{}`", temp.display()))?;
  }

  Ok(())
}


/// Strip a `VMSHKRNL` trailer from a binary blob, if present.
fn strip_trailer(binary: &mut Vec<u8>) {
  if binary.len() < TRAILER_SIZE as usize {
    return;
  }
  let len = binary.len();
  let magic_start = len - MAGIC.len();
  if &binary[magic_start..] != MAGIC {
    return;
  }
  let size_start = magic_start - 8;
  let mut size_bytes = [0u8; 8];
  let () = size_bytes.copy_from_slice(&binary[size_start..magic_start]);
  let kernel_size = u64::from_le_bytes(size_bytes) as usize;

  // The kernel must fit within the file (minus the trailer).
  let payload_end = size_start;
  if kernel_size <= payload_end {
    let () = binary.truncate(payload_end - kernel_size);
  }
}


/// Extract the embedded kernel from `/proc/self/exe`, if present.
///
/// Returns a [`CleanupGuard`] that holds a temporary file containing
/// the kernel image. The file is removed when the guard is dropped.
pub(crate) fn extract_embedded_kernel() -> Result<CleanupGuard> {
  let mut file = File::open("/proc/self/exe").context("failed to open /proc/self/exe")?;
  let file_len = file
    .seek(SeekFrom::End(0))
    .context("failed to seek to end of /proc/self/exe")?;
  ensure!(
    file_len >= TRAILER_SIZE,
    "binary too small to contain an embedded kernel"
  );

  let () = file
    .seek(SeekFrom::End(-(TRAILER_SIZE as i64)))
    .map(|_| ())
    .context("failed to seek to trailer")?;
  let mut trailer = [0u8; TRAILER_SIZE as usize];
  let () = file
    .read_exact(&mut trailer)
    .context("failed to read trailer")?;

  let magic = &trailer[8..];
  ensure!(
    magic == MAGIC,
    "no embedded kernel found (missing VMSHKRNL trailer)"
  );

  let mut size_bytes = [0u8; 8];
  let () = size_bytes.copy_from_slice(&trailer[..8]);
  let kernel_size = u64::from_le_bytes(size_bytes);

  let kernel_start = file_len - TRAILER_SIZE - kernel_size;
  let () = file
    .seek(SeekFrom::Start(kernel_start))
    .map(|_| ())
    .context("failed to seek to kernel data")?;
  let mut kernel_data = vec![0u8; kernel_size as usize];
  let () = file
    .read_exact(&mut kernel_data)
    .context("failed to read embedded kernel")?;

  // Write to a temp file (`krun_set_kernel` needs a file path).
  let path = PathBuf::from(format!("/tmp/vmsh-kernel-{}", process::id(),));
  let () = fs::write(&path, &kernel_data)
    .with_context(|| format!("failed to write embedded kernel to `{}`", path.display()))?;

  Ok(CleanupGuard(Some(path)))
}


#[cfg(test)]
mod tests {
  use super::*;


  /// Build a blob with a valid `VMSHKRNL` trailer.
  fn blob_with_trailer(base: &[u8], kernel: &[u8]) -> Vec<u8> {
    let mut buf = Vec::from(base);
    let size = kernel.len() as u64;
    let () = buf.extend_from_slice(kernel);
    let () = buf.extend_from_slice(&size.to_le_bytes());
    let () = buf.extend_from_slice(MAGIC);
    buf
  }


  /// Check that a blob without a trailer is left unchanged.
  #[test]
  fn strip_no_trailer() {
    let original = vec![1, 2, 3, 4, 5];
    let mut data = original.clone();
    let () = strip_trailer(&mut data);
    assert_eq!(data, original);
  }

  /// Test that a blob with a valid trailer is truncated to the original
  /// content.
  #[test]
  fn strip_with_trailer() {
    let base = vec![0xAA; 32];
    let kernel = vec![0xBB; 64];
    let mut data = blob_with_trailer(&base, &kernel);
    assert_eq!(data.len(), 32 + 64 + 16);

    let () = strip_trailer(&mut data);
    assert_eq!(data, base);
  }

  /// Verify that a blob shorter than the trailer size is left
  /// unchanged.
  #[test]
  fn strip_too_small() {
    let mut data = vec![1, 2, 3];
    let original = data.clone();
    let () = strip_trailer(&mut data);
    assert_eq!(data, original);
  }

  /// Make sure that stripping twice produces the same result as
  /// stripping once.
  #[test]
  fn strip_idempotent() {
    let base = vec![0xCC; 16];
    let kernel = vec![0xDD; 8];
    let mut data = blob_with_trailer(&base, &kernel);

    let () = strip_trailer(&mut data);
    let after_first = data.clone();
    let () = strip_trailer(&mut data);
    assert_eq!(data, after_first);
  }

  /// Check that a trailer whose size exceeds the available space is not
  /// stripped.
  #[test]
  fn strip_corrupt_size() {
    // Build a trailer that claims a kernel larger than the remaining
    // blob. `strip_trailer` should leave it untouched instead of doing
    // the wrong thing.
    let mut data = vec![0xEE; 4];
    // "kernel" (2 bytes, but size says 9999)
    let () = data.extend_from_slice(&[0xFF; 2]);
    let () = data.extend_from_slice(&9999u64.to_le_bytes());
    let () = data.extend_from_slice(MAGIC);
    let original = data.clone();

    let () = strip_trailer(&mut data);
    assert_eq!(data, original);
  }
}
