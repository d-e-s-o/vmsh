//! Integration tests for `vmsh`.

use std::env;
use std::fs;
use std::fs::remove_file;
use std::io::Write as _;
use std::process;
use std::process::Child;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;


/// Run a shell snippet inside the VM, returning the captured `Output`.
fn run(shell_input: &str) -> Output {
  run_with_args(shell_input, &[])
}

/// Run a shell snippet inside the VM with extra CLI arguments.
fn run_with_args(shell_input: &str, extra_args: &[&str]) -> Output {
  let kernel = env::var("VMSH_KERNEL").expect("VMSH_KERNEL must be set");
  Command::new(env!("CARGO_BIN_EXE_vmsh"))
    .args(extra_args)
    .arg(&kernel)
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


/// Run a command inside the VM (via `-- cmd args...`), returning the captured `Output`.
fn run_command(cmd: &[&str]) -> Output {
  let kernel = env::var("VMSH_KERNEL").expect("VMSH_KERNEL must be set");
  Command::new(env!("CARGO_BIN_EXE_vmsh"))
    .arg(&kernel)
    .arg("--")
    .args(cmd)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .and_then(Child::wait_with_output)
    .expect("failed to run vmsh")
}


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

#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn guest_sees_host_filesystem() {
  // Create a temporary file on the host with unique content.
  let marker = format!("vmsh-test-{}", process::id());
  let path = env::temp_dir().join(&marker);
  let () = fs::write(&path, &marker).expect("failed to create temp file on host");

  // Read that file from inside the guest via the virtiofs-shared root.
  let path_str = path.to_str().unwrap();
  let output = run_command(&["/bin/cat", path_str]);

  let () = drop(remove_file(&path));

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

#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn guest_writes_to_host_filesystem() {
  let dir = env::temp_dir().join(format!("vmsh-write-test-{}", process::id()));
  let () = fs::create_dir_all(&dir).expect("failed to create temp dir on host");

  let file_path = dir.join("output.txt");
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

  let () = fs::remove_dir_all(&dir).unwrap();
}

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

#[test]
#[ignore = "requires /dev/kvm present and VMSH_KERNEL set"]
fn host_home_inherited() {
  let host_home = env::var("HOME").expect("HOME should be set on the host");
  let output = run_command(&["/bin/sh", "-c", "echo $HOME"]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected exit code; stderr:\n{stderr}",
  );
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert_eq!(
    stdout.trim(),
    host_home,
    "guest HOME should match host HOME, got: {stdout:?}",
  );
}
