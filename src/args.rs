use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;

use clap::ArgAction;
use clap::Parser;
use clap::Subcommand;
use clap::builder::PathBufValueParser;
use clap::builder::TypedValueParser as _;


/// Canonicalize a path and ensure it doesn't contain reserved
/// characters.
fn canonicalize_path(p: PathBuf) -> Result<PathBuf> {
  let p = p
    .canonicalize()
    .with_context(|| format!("failed to resolve path `{}`", p.display()))?;
  let bytes = p.as_os_str().as_bytes();
  if bytes.contains(&b':') || bytes.contains(&b';') {
    bail!(
      "path `{}` contains reserved delimiter characters (':' or ';')",
      p.display()
    );
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
  /// Share a host path as read-write inside the guest.
  ///
  /// By default the current working directory as well as the tmp
  /// directory will be writable already. Sharing '/' this way makes
  /// the entire guest visible file system writable (subject to
  /// user/group permissions, of course). Can be specified multiple
  /// times.
  #[clap(long, value_parser = PathBufValueParser::new().try_map(canonicalize_path))]
  pub share_rw: Vec<PathBuf>,
  /// Skip the user-namespace setup that maps the host uid/gid to root
  /// in the guest.
  ///
  /// Escape hatch for systems where unprivileged user namespaces are
  /// blocked (e.g. Debian/Ubuntu `kernel.unprivileged_userns_clone=0`,
  /// Ubuntu 24.04 `kernel.apparmor_restrict_unprivileged_userns=1`).
  /// Without the mapping, host files appear in the guest with their
  /// original uid/gid, causing some tools to fail ownership checks.
  #[clap(long, hide = true)]
  pub no_uid_map: bool,
  /// Increase verbosity (can be supplied multiple times).
  #[clap(short = 'v', long = "verbose", global = true, action = ArgAction::Count)]
  pub verbosity: u8,
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
