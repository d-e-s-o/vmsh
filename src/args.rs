use std::path::PathBuf;

use clap::ArgAction;
use clap::Parser;


/// Transparently run a shell (or other binary) in a VM.
#[derive(Debug, Parser)]
#[clap(version = env!("VERSION"))]
pub struct Args {
  /// Path to the kernel vmlinux.
  ///
  /// The image can optionally be gzip, bzip, or zstd compressed.
  pub kernel: PathBuf,
  /// Number of vCPUs present in the VM.
  #[clap(long, default_value_t = 2)]
  pub cpus: u8,
  /// Amount of main memory present in the VM (in MiB).
  #[clap(long, default_value_t = 1024)]
  pub memory: u32,
  /// Enable networking via TSI socket impersonation.
  ///
  /// Note that this requires additional kernel patches. Implies `--uds`.
  #[clap(long)]
  pub net: bool,
  /// Enable UNIX domain socket impersonation via TSI.
  ///
  /// Note that this requires additional kernel patches.
  #[clap(long)]
  pub uds: bool,
  /// Command and arguments to run inside the VM (after --).
  #[clap(last = true)]
  pub command: Vec<String>,
  /// Pass a host environment variable to the guest.
  ///
  /// Use `--env=KEY` to forward the current value or `--env=KEY=VALUE`
  /// to set an explicit value. Can be specified multiple times.
  #[clap(long = "env")]
  pub env_vars: Vec<String>,
  /// Forward all host environment variables to the guest.
  ///
  /// Variables that conflict with vmsh internals (`VMSH_*`) or libkrun
  /// (`KRUN_*`) are excluded. Individual `--env` flags take precedence.
  #[clap(long)]
  pub all_envs: bool,
  /// Increase verbosity (can be supplied multiple times).
  #[clap(short = 'v', long = "verbose", global = true, action = ArgAction::Count)]
  pub verbosity: u8,
}
