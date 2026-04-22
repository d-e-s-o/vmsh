use std::fs::canonicalize;
use std::path::PathBuf;

use anyhow::Context as _;
use anyhow::Result;

use clap::ArgAction;
use clap::Parser;
use clap::Subcommand;


fn parse_absolute_path(s: &str) -> Result<PathBuf> {
  let p = canonicalize(s).with_context(|| format!("failed to resolve path `{s}`"))?;
  let bytes = p.as_os_str().as_encoded_bytes();
  if bytes.contains(&b':') || bytes.contains(&b';') {
    anyhow::bail!(
      "path `{}` contains reserved delimiter characters (':' or ';')",
      p.display()
    );
  }
  Ok(p)
}


fn parse_directory_path(s: &str) -> Result<PathBuf> {
  let p = parse_absolute_path(s)?;
  if !p.is_dir() {
    anyhow::bail!("path `{}` is not a directory", p.display());
  }
  Ok(p)
}


/// Transparently run a shell (or other binary) in a VM.
#[derive(Debug, Parser)]
#[clap(version = env!("VERSION"), args_conflicts_with_subcommands = true)]
pub struct Args {
  #[clap(subcommand)]
  pub command: Option<Command>,
  #[clap(flatten)]
  pub args: RunArgs,
}


/// Available subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
  /// Run a command in a VM (default).
  Run(RunArgs),
  /// Embed a kernel image into a copy of this binary.
  Embed(EmbedArgs),
}


/// Arguments for the default `run` subcommand.
#[derive(Debug, Parser)]
pub struct RunArgs {
  /// Path to the kernel vmlinux.
  ///
  /// The image can optionally be gzip, bzip, or zstd compressed. When
  /// omitted, the embedded kernel is used (see `vmsh embed`).
  #[clap(short, long)]
  pub kernel: Option<PathBuf>,
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
  /// Share a host path as read-write inside the guest.
  ///
  /// By default the guest sees the host filesystem as read-only.
  /// This flag punches through a writable mount at the given path.
  /// Can be specified multiple times. When the same path appears in
  /// multiple flags, `--hide` takes precedence over `--share-ro`,
  /// which takes precedence over `--share-rw`.
  #[clap(long, value_parser = parse_absolute_path)]
  pub share_rw: Vec<PathBuf>,
  /// Share a host path as read-only inside the guest.
  ///
  /// Useful for sharing additional paths with explicit read-only
  /// enforcement beyond what the read-only root already provides.
  #[clap(long, value_parser = parse_absolute_path)]
  pub share_ro: Vec<PathBuf>,
  /// Hide a host directory from the guest.
  ///
  /// The directory is hidden by overlaying an empty tmpfs in the host
  /// mount namespace, so the guest cannot bypass it. The path must be
  /// an existing directory.
  #[clap(long, value_parser = parse_directory_path)]
  pub hide: Vec<PathBuf>,
}


/// Arguments for the `embed` subcommand.
#[derive(Debug, Parser)]
pub struct EmbedArgs {
  /// Path to the kernel image to embed.
  pub kernel: PathBuf,
  /// Output path for the new binary.
  ///
  /// Defaults to overwriting the current executable in place.
  #[clap(short, long)]
  pub output: Option<PathBuf>,
}
