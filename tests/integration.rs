//! Integration tests for `vmsh`.

use std::env;
use std::io::Write as _;
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
