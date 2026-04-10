//! Core `vmsh` functionality.

// At the moment everything here is considered an implementation detail
// and not stable.
#![doc(hidden)]

mod util;

pub use util::detect_kernel_format;
pub use util::KernelFormat;
