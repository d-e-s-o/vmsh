use std::path::PathBuf;

use clap::ArgAction;
use clap::Parser;


/// Transparently run a shell (or other binary) in a VM.
#[derive(Debug, Parser)]
#[clap(version = env!("VERSION"))]
pub struct Args {
  /// Path to the kernel bzImage.
  pub kernel: PathBuf,
  /// Number of vCPUs present in the VM.
  #[clap(long, default_value_t = 2)]
  pub cpus: u8,
  /// Amount of main memory present in the VM (in MiB).
  #[clap(long, default_value_t = 1024)]
  pub memory: u32,
  /// Increase verbosity (can be supplied multiple times).
  #[clap(short = 'v', long = "verbose", global = true, action = ArgAction::Count)]
  pub verbosity: u8,
}
