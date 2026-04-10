//! Integration tests for `vmsh`.

use std::env;
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::process::Child;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;

use tempfile::NamedTempFile;
use tempfile::TempDir;

use vmsh::KernelFormat;


/// Run a shell snippet inside the VM using an explicit kernel path.
fn run_with_kernel(kernel_path: &Path, shell_input: &str, extra_args: &[&str]) -> Output {
  Command::new(env!("CARGO_BIN_EXE_vmsh"))
    .args(extra_args)
    .arg(kernel_path)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .and_then(|mut child| {
      child
        .stdin
        .take()
        .unwrap()
        .write_all(shell_input.as_bytes())?;
      child.wait_with_output()
    })
    .expect("failed to run vmsh")
}

/// Run a shell snippet inside the VM, returning the captured `Output`.
fn run(shell_input: &str) -> Output {
  run_with_args(shell_input, &[])
}

/// Run a shell snippet inside the VM with extra CLI arguments.
fn run_with_args(shell_input: &str, extra_args: &[&str]) -> Output {
  let kernel = env::var("VMSH_KERNEL").expect("VMSH_KERNEL must be set");
  run_with_kernel(Path::new(&kernel), shell_input, extra_args)
}


/// Run a command inside the VM (via `-- cmd args...`), returning the
/// captured [`Output`].
fn run_command(cmd: &[&str]) -> Output {
  run_command_with_env(cmd, &[], &[])
}

/// Run a command inside the VM with extra CLI args and environment variables set on the host process.
fn run_command_with_env(cmd: &[&str], extra_args: &[&str], env_vars: &[(&str, &str)]) -> Output {
  let kernel = env::var("VMSH_KERNEL").expect("VMSH_KERNEL must be set");
  let mut command = Command::new(env!("CARGO_BIN_EXE_vmsh"));
  command
    .args(extra_args)
    .arg(&kernel)
    .arg("--")
    .args(cmd)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
  for &(key, value) in env_vars {
    command.env(key, value);
  }
  command
    .spawn()
    .and_then(Child::wait_with_output)
    .expect("failed to run vmsh")
}


/// Test that a successful shell command exits with 0.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn exit_success() {
  let output = run("true\n");
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code; stderr:\n{stderr}",
  );
}

/// Check that a non-zero shell exit code is propagated.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn exit_failure() {
  let output = run("exit 42\n");
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(42),
    "unexpected exit code; stderr:\n{stderr}",
  );
}

/// Verify that guest stdout is forwarded to the host.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn stdout_capture() {
  let output = run("echo hello\n");
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code; stderr:\n{stderr}",
  );
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert_eq!(
    stdout, "hello\n",
    "stdout should be 'hello\\n', got: {stdout:?}",
  );
}

/// Test that guest stderr is captured separately from stdout.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn stderr_capture() {
  let output = run("echo err >&2\n");
  let stdout = String::from_utf8_lossy(&output.stdout);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    stderr, "err\n",
    "stderr should be 'err\\n', got: {stderr:?}",
  );
  assert!(stdout.is_empty(), "stdout should be empty, got: {stdout:?}",);
}

/// Check that `--verbose` emits boot messages on stderr without
/// polluting stdout.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn verbose_boot_on_stderr() {
  let output = run_with_args("echo hello\n", &["--verbose"]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code; stderr:\n{stderr}",
  );
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert_eq!(
    stdout, "hello\n",
    "stdout should still be clean with --verbose, got: {stdout:?}",
  );
  assert!(
    !stderr.is_empty(),
    "stderr should contain boot messages with --verbose",
  );
}

/// Verify that `-vv` produces more `stderr` output than `-v`.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn very_verbose_more_output() {
  let v1 = run_with_args("true\n", &["-v"]);
  let v2 = run_with_args("true\n", &["-vv"]);
  let v1_stderr = String::from_utf8_lossy(&v1.stderr);
  let v2_stderr = String::from_utf8_lossy(&v2.stderr);
  assert_eq!(
    v1.status.code(),
    Some(0),
    "unexpected exit code for -v; stderr:\n{v1_stderr}",
  );
  assert_eq!(
    v2.status.code(),
    Some(0),
    "unexpected exit code for -vv; stderr:\n{v2_stderr}",
  );
  assert!(
    v2.stderr.len() > v1.stderr.len(),
    "-vv should produce more stderr than -v ({} vs {} bytes)",
    v2.stderr.len(),
    v1.stderr.len(),
  );
}

/// Test that `-- /bin/true` produces exit code 0.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn command_exit_success() {
  let output = run_command(&["/bin/true"]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code; stderr:\n{stderr}",
  );
}

/// Check that a non-zero `--` command exit code is propagated.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn command_exit_failure() {
  let output = run_command(&["/bin/sh", "-c", "exit 42"]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(42),
    "unexpected exit code; stderr:\n{stderr}",
  );
}

/// Verify that `stdout` from a `--` command is captured.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn command_stdout() {
  let output = run_command(&["/bin/echo", "hello"]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code; stderr:\n{stderr}",
  );
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert_eq!(
    stdout, "hello\n",
    "stdout should be 'hello\\n', got: {stdout:?}",
  );
}

/// Test that `stderr` from a `--` command is captured separately from
/// `stdout`.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn command_stderr() {
  let output = run_command(&["/bin/sh", "-c", "echo err >&2"]);
  let stdout = String::from_utf8_lossy(&output.stdout);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    stderr, "err\n",
    "stderr should be 'err\\n', got: {stderr:?}",
  );
  assert!(stdout.is_empty(), "stdout should be empty, got: {stdout:?}");
}

/// Check that multiple arguments after `--` are forwarded to the guest
/// command.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn command_with_arguments() {
  let output = run_command(&["/bin/echo", "foo", "bar", "baz"]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code; stderr:\n{stderr}",
  );
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert_eq!(
    stdout, "foo bar baz\n",
    "stdout should be 'foo bar baz\\n', got: {stdout:?}",
  );
}

/// Verify that the guest can read host files via the virtiofs-shared
/// root.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn guest_sees_host_filesystem() {
  // Create a temporary file on the host with unique content.
  let marker = format!("vmsh-test-{}", process::id());
  let mut file = NamedTempFile::new().expect("failed to create temp file on host");
  let () = file
    .write_all(marker.as_bytes())
    .expect("failed to write temp file on host");

  // Read that file from inside the guest via the virtiofs-shared root.
  let path_str = file.path().to_str().unwrap();
  let output = run_command(&["/bin/cat", path_str]);

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code; stderr:\n{stderr}",
  );
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert_eq!(
    stdout, marker,
    "guest should see host file contents, got: {stdout:?}",
  );
}

/// Test that files written by the guest appear on the host.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn guest_writes_to_host_filesystem() {
  let dir = TempDir::new().expect("failed to create temp dir on host");

  let file_path = dir.path().join("output.txt");
  let file_path_str = file_path.to_str().unwrap();
  let content = "written_by_guest";

  let output = run_command(&[
    "/bin/sh",
    "-c",
    &format!("echo -n {content} > {file_path_str}"),
  ]);

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code; stderr:\n{stderr}",
  );

  let host_content =
    fs::read_to_string(&file_path).expect("file written by guest should exist on host");
  assert_eq!(
    host_content, content,
    "host file should contain guest-written content, got: {host_content:?}",
  );
}

/// Check that the guest's working directory defaults to `/`.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn working_directory() {
  let output = run_command(&["/bin/pwd"]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code; stderr:\n{stderr}",
  );
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert_eq!(
    stdout.trim(),
    "/",
    "guest working directory should be /, got: {stdout:?}",
  );
}

/// Verify that `/proc` is mounted and functional in the guest.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn proc_mount() {
  let output = run_command(&["/bin/cat", "/proc/self/comm"]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "/proc/self/comm should be readable; stderr:\n{stderr}",
  );
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert_eq!(
    stdout.trim(),
    "cat",
    "/proc/self/comm should report 'cat', got: {stdout:?}",
  );
}

/// Test that `/dev/null` is available in the guest.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn dev_null_availability() {
  // `/dev/null` works
  let output = run_command(&["/bin/sh", "-c", "echo gone > /dev/null && echo ok"]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "/dev/null redirect should succeed; stderr:\n{stderr}",
  );
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert_eq!(
    stdout.trim(),
    "ok",
    "/dev/null redirect should work, got: {stdout:?}",
  );
}

/// Check that `/dev/std{in,out,err}` are symlinks to
/// `/proc/self/fd/{0,1,2}`.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn dev_stdin_stdout_stderr_symlinks() {
  let output = run_command(&[
    "/bin/sh",
    "-c",
    "test -L /dev/stdin && test -L /dev/stdout && test -L /dev/stderr \
     && readlink /dev/stdin && readlink /dev/stdout && readlink /dev/stderr",
  ]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "/dev/stdin, /dev/stdout, /dev/stderr should be symlinks; stderr:\n{stderr}",
  );
  let stdout = String::from_utf8_lossy(&output.stdout);
  let lines = stdout.trim().lines().collect::<Vec<&str>>();
  assert_eq!(
    lines,
    &["/proc/self/fd/0", "/proc/self/fd/1", "/proc/self/fd/2"],
    "symlinks should point to /proc/self/fd/{{0,1,2}}, got: {lines:?}",
  );
}

/// Verify that the loopback interface is up in the guest.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn loopback_device() {
  let output = run_command(&["/bin/cat", "/sys/class/net/lo/operstate"]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "loopback operstate should be readable; stderr:\n{stderr}",
  );
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.trim() == "up" || stdout.trim() == "unknown",
    "loopback should be 'up' or 'unknown', got: {stdout:?}",
  );
}

/// Test that `--all-envs` forwards host env vars to the guest.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn env_passthrough() {
  let output = run_command_with_env(
    &["/bin/sh", "-c", "echo $__VMSH_TEST_MARKER"],
    &["--all-envs"],
    &[("__VMSH_TEST_MARKER", "hello_from_host")],
  );
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code; stderr:\n{stderr}",
  );
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert_eq!(
    stdout, "hello_from_host\n",
    "host env var should be visible in guest, got: {stdout:?}",
  );
}

/// Check that `--env=KEY=VALUE` sets an explicit env var in the guest.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn env_explicit_value() {
  let output = run_command_with_env(&["/bin/sh", "-c", "echo $FOO"], &["--env=FOO=bar"], &[]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code; stderr:\n{stderr}",
  );
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert_eq!(
    stdout, "bar\n",
    "explicit --env=FOO=bar should be visible in guest, got: {stdout:?}",
  );
}

/// Verify that `--env=KEY` forwards the host's value for that key into
/// the guest.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn env_passthrough_specific() {
  let output = run_command_with_env(
    &["/bin/sh", "-c", "echo $__VMSH_TEST_MARKER"],
    &["--env=__VMSH_TEST_MARKER"],
    &[("__VMSH_TEST_MARKER", "specific_value")],
  );
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code; stderr:\n{stderr}",
  );
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert_eq!(
    stdout, "specific_value\n",
    "--env=KEY should forward the host value, got: {stdout:?}",
  );
}

/// Test that `--env=KEY=VALUE` overrides the same key from
/// `--all-envs`.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn env_override_with_all_envs() {
  let output = run_command_with_env(
    &["/bin/sh", "-c", "echo $__VMSH_TEST_MARKER"],
    &["--all-envs", "--env=__VMSH_TEST_MARKER=overridden"],
    &[("__VMSH_TEST_MARKER", "original")],
  );
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code; stderr:\n{stderr}",
  );
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert_eq!(
    stdout, "overridden\n",
    "--env should override --all-envs, got: {stdout:?}",
  );
}


/// Check whether a host tool is available in `$PATH`.
fn has_tool(name: &str) -> bool {
  Command::new(name)
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok()
}


/// Return the decompression command for a compressed kernel format.
fn decompress_cmd(format: KernelFormat) -> (&'static str, &'static [&'static str]) {
  match format {
    KernelFormat::Gz => ("gzip", &["-d", "-c"]),
    KernelFormat::Bz2 => ("bzip2", &["-d", "-c"]),
    KernelFormat::Zstd => ("zstd", &["-d", "-c", "-q"]),
    KernelFormat::Elf => unreachable!("ELF kernels do not need decompression"),
  }
}


/// Get a raw ELF kernel, decompressing `VMSH_KERNEL` if necessary.
///
/// Returns the path and an optional temp file handle that keeps the
/// decompressed file alive.
fn ensure_elf_kernel() -> (PathBuf, Option<NamedTempFile>) {
  let kernel_path = PathBuf::from(env::var("VMSH_KERNEL").expect("VMSH_KERNEL must be set"));
  let format = vmsh::detect_kernel_format(&kernel_path).expect("failed to detect kernel format");

  if format == KernelFormat::Elf {
    return (kernel_path, None);
  }

  let (tool, args) = decompress_cmd(format);
  let output = Command::new(tool)
    .args(args)
    .arg(&kernel_path)
    .stderr(Stdio::piped())
    .output()
    .unwrap_or_else(|e| panic!("failed to run {tool}: {e}"));
  assert!(
    output.status.success(),
    "{tool} decompression failed: {}",
    String::from_utf8_lossy(&output.stderr),
  );

  let mut tmp = NamedTempFile::new().expect("failed to create temp file");
  let () = tmp
    .write_all(&output.stdout)
    .expect("failed to write decompressed kernel");
  (tmp.path().to_path_buf(), Some(tmp))
}


/// Compress an ELF kernel using a host tool, returning a temp file with
/// the compressed data.
fn compress_kernel(tool: &str, args: &[&str]) -> NamedTempFile {
  let (elf_path, _elf_guard) = ensure_elf_kernel();
  let output = Command::new(tool)
    .args(args)
    .arg(&elf_path)
    .stderr(Stdio::piped())
    .output()
    .unwrap_or_else(|e| panic!("failed to run {tool}: {e}"));
  assert!(
    output.status.success(),
    "{tool} compression failed: {}",
    String::from_utf8_lossy(&output.stderr),
  );

  let mut tmp = NamedTempFile::new().expect("failed to create temp file");
  let () = tmp
    .write_all(&output.stdout)
    .expect("failed to write compressed kernel");
  tmp
}


/// Test booting from a gzip-compressed kernel.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn boot_gz_kernel() {
  if !has_tool("gzip") {
    eprintln!("warning: gzip not found, skipping test");
    return;
  }
  let compressed = compress_kernel("gzip", &["-c"]);
  let output = run_with_kernel(compressed.path(), "true\n", &[]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code for gzip kernel; stderr:\n{stderr}",
  );
}

/// Test booting from a bzip2-compressed kernel.
#[expect(unreachable_code)]
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn boot_bz2_kernel() {
  // TODO: `libkrun` has a bug where it cannot correctly detect bzip2
  //       headers. Re-enable this test once the upstream fix landed.
  return;

  if !has_tool("bzip2") {
    eprintln!("warning: bzip2 not found, skipping test");
    return;
  }
  let compressed = compress_kernel("bzip2", &["-c"]);
  let output = run_with_kernel(compressed.path(), "true\n", &[]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code for bzip2 kernel; stderr:\n{stderr}",
  );
}

/// Test booting from a zstd-compressed kernel.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn boot_zstd_kernel() {
  if !has_tool("zstd") {
    eprintln!("warning: zstd not found, skipping test");
    return;
  }
  let compressed = compress_kernel("zstd", &["-c", "-q"]);
  let output = run_with_kernel(compressed.path(), "true\n", &[]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code for zstd kernel; stderr:\n{stderr}",
  );
}
