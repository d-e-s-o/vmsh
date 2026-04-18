//! Integration tests for `vmsh`.

use std::borrow::Cow;
use std::env;
use std::fs;
use std::io::Read as _;
use std::io::Write as _;
use std::net::TcpListener;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::process::Child;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::thread::spawn;

use tempfile::NamedTempFile;
use tempfile::TempDir;

use vmsh::KernelFormat;


/// Builder for running commands inside a VM.
struct Vm {
  kernel: Option<PathBuf>,
  args: Vec<String>,
  env_vars: Vec<(String, String)>,
}

impl Vm {
  fn new() -> Self {
    Self {
      kernel: None,
      args: Vec::new(),
      env_vars: Vec::new(),
    }
  }

  fn kernel(mut self, path: &Path) -> Self {
    self.kernel = Some(path.to_path_buf());
    self
  }

  fn arg(mut self, arg: &str) -> Self {
    let () = self.args.push(arg.to_string());
    self
  }

  fn env(mut self, key: &str, value: &str) -> Self {
    let () = self.env_vars.push((key.to_string(), value.to_string()));
    self
  }

  fn kernel_path(&self) -> Cow<'_, Path> {
    self
      .kernel
      .as_deref()
      .map(Cow::Borrowed)
      .unwrap_or_else(|| {
        Cow::Owned(PathBuf::from(
          env::var("VMSH_KERNEL").expect("VMSH_KERNEL must be set"),
        ))
      })
  }

  fn apply_env(&self, cmd: &mut Command) {
    for (key, value) in &self.env_vars {
      cmd.env(key, value);
    }
  }

  /// Run a shell snippet inside the VM via stdin.
  fn run_shell(&self, input: &str) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vmsh"));
    let () = self.apply_env(&mut cmd);

    cmd
      .args(&self.args)
      .arg(self.kernel_path().as_os_str())
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .and_then(|mut child| {
        child.stdin.take().unwrap().write_all(input.as_bytes())?;
        child.wait_with_output()
      })
      .expect("failed to run vmsh")
  }

  /// Run a command inside the VM via `-- cmd args...`.
  fn run(&self, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vmsh"));
    let () = self.apply_env(&mut cmd);

    cmd
      .args(&self.args)
      .arg(self.kernel_path().as_os_str())
      .arg("--")
      .args(args)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .and_then(Child::wait_with_output)
      .expect("failed to run vmsh")
  }
}


/// Test that a successful shell command exits with 0.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn exit_success() {
  let output = Vm::new().run_shell("true\n");
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
  let output = Vm::new().run_shell("exit 42\n");
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
  let output = Vm::new().run_shell("echo hello\n");
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
  let output = Vm::new().run_shell("echo err >&2\n");
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
  let output = Vm::new().arg("--verbose").run_shell("echo hello\n");
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
  let v1 = Vm::new().arg("-v").run_shell("true\n");
  let v2 = Vm::new().arg("-vv").run_shell("true\n");
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
  let output = Vm::new().run(&["/bin/true"]);
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
  let output = Vm::new().run(&["/bin/sh", "-c", "exit 42"]);
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
  let output = Vm::new().run(&["/bin/echo", "hello"]);
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
  let output = Vm::new().run(&["/bin/sh", "-c", "echo err >&2"]);
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
  let output = Vm::new().run(&["/bin/echo", "foo", "bar", "baz"]);
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
  let output = Vm::new().run(&["/bin/cat", path_str]);

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

  let output = Vm::new().run(&[
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
  let output = Vm::new().run(&["/bin/pwd"]);
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
  let output = Vm::new().run(&["/bin/cat", "/proc/self/comm"]);
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
  let output = Vm::new().run(&["/bin/sh", "-c", "echo gone > /dev/null && echo ok"]);
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
  let output = Vm::new().run(&[
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
  let output = Vm::new().run(&["/bin/cat", "/sys/class/net/lo/operstate"]);
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
  let output = Vm::new()
    .arg("--all-envs")
    .env("__VMSH_TEST_MARKER", "hello_from_host")
    .run(&["/bin/sh", "-c", "echo $__VMSH_TEST_MARKER"]);
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
  let output = Vm::new()
    .arg("--env=FOO=bar")
    .run(&["/bin/sh", "-c", "echo $FOO"]);
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
  let output = Vm::new()
    .arg("--env=__VMSH_TEST_MARKER")
    .env("__VMSH_TEST_MARKER", "specific_value")
    .run(&["/bin/sh", "-c", "echo $__VMSH_TEST_MARKER"]);
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
  let output = Vm::new()
    .arg("--all-envs")
    .arg("--env=__VMSH_TEST_MARKER=overridden")
    .env("__VMSH_TEST_MARKER", "original")
    .run(&["/bin/sh", "-c", "echo $__VMSH_TEST_MARKER"]);
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
    .stdin(Stdio::null())
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
  let output = Vm::new().kernel(compressed.path()).run_shell("true\n");
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code for gzip kernel; stderr:\n{stderr}",
  );
}

/// Test booting from a bzip2-compressed kernel.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn boot_bz2_kernel() {
  if !has_tool("bzip2") {
    eprintln!("warning: bzip2 not found, skipping test");
    return;
  }
  let compressed = compress_kernel("bzip2", &["-c"]);
  let output = Vm::new().kernel(compressed.path()).run_shell("true\n");
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
  let output = Vm::new().kernel(compressed.path()).run_shell("true\n");
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code for zstd kernel; stderr:\n{stderr}",
  );
}

/// Verify that TCP connections through TSI require `--net`.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn network_tcp_connection() {
  let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind TCP listener");
  let port = listener.local_addr().unwrap().port();

  // Without --net, TCP connect should fail.
  let py = format!(
    "import socket; s=socket.socket(); s.settimeout(2); \
     s.connect(('127.0.0.1',{port})); s.send(b'vmsh-tsi-test'); s.close()"
  );
  let output = Vm::new().run(&["/usr/bin/python3", "-c", &py]);
  assert_ne!(
    output.status.code(),
    Some(0),
    "TCP connect should fail without --net",
  );

  // With --net, TCP connect should succeed.
  let handle = spawn(move || {
    let () = listener.set_nonblocking(false).unwrap();
    let (mut conn, _addr) = listener.accept().expect("accept failed");
    let mut buf = [0u8; 256];
    let n = conn.read(&mut buf).expect("read failed");
    String::from_utf8_lossy(&buf[..n]).to_string()
  });

  let output = Vm::new().arg("--net").run(&["/usr/bin/python3", "-c", &py]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "guest TCP connect should succeed with --net; stderr:\n{stderr}",
  );

  let received = handle.join().expect("server thread panicked");
  assert_eq!(
    received, "vmsh-tsi-test",
    "host should receive data sent from guest via TSI, got: {received:?}",
  );
}

/// Verify that UNIX domain socket connections through TSI require
/// `--uds`.
#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn uds_connection() {
  let dir = TempDir::new().expect("failed to create temp dir");
  let sock_path = dir.path().join("test.sock");
  let sock_path_str = sock_path.to_str().unwrap().to_string();

  let listener = UnixListener::bind(&sock_path).expect("failed to bind unix socket");

  let py = format!(
    "import socket; s=socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); \
     s.connect('{sock_path_str}'); s.send(b'vmsh-uds-test'); s.close()"
  );

  // Without --uds, UDS connect should fail.
  let output = Vm::new().run(&["/usr/bin/python3", "-c", &py]);
  assert_ne!(
    output.status.code(),
    Some(0),
    "UDS connect should fail without --uds",
  );

  // With --uds, UDS connect should succeed.
  let handle = spawn(move || {
    let (mut conn, _addr) = listener.accept().expect("accept failed");
    let mut buf = [0u8; 256];
    let n = conn.read(&mut buf).expect("read failed");
    String::from_utf8_lossy(&buf[..n]).to_string()
  });

  let output = Vm::new().arg("--uds").run(&["/usr/bin/python3", "-c", &py]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "guest UDS connect should succeed with --uds; stderr:\n{stderr}",
  );

  let received = handle.join().expect("server thread panicked");
  assert_eq!(
    received, "vmsh-uds-test",
    "host should receive data sent from guest via UDS TSI, got: {received:?}",
  );
}
