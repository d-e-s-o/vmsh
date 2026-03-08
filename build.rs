//! Build script for `vmsh`.

use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use grev::git_revision_auto;


fn build_init(manifest_dir: &Path) {
  let init_src = manifest_dir.join("init").join("main.c");

  let out_dir = env::var("OUT_DIR").expect("failed to read `OUT_DIR` variable");
  let out_dir = PathBuf::from(out_dir);
  let init_bin = out_dir.join("vmsh-init");

  let cc = env::var("CC").unwrap_or_else(|_| "cc".to_string());
  let output = Command::new(&cc)
    .args(["-static", "-Os", "-s", "-Wall", "-Wextra", "-o"])
    .arg(&init_bin)
    .arg(&init_src)
    .output()
    .unwrap_or_else(|e| panic!("failed to run C compiler `{cc}`: {e}"));

  if !output.status.success() {
    let stderr = String::from_utf8_lossy(&output.stderr);
    panic!("failed to compile init binary:\n{stderr}");
  }

  println!("cargo:rerun-if-env-changed=CC");
  println!("cargo:rerun-if-changed=init/main.c");
}

fn determine_version(manifest_dir: &Path) {
  let pkg_version = env::var("CARGO_PKG_VERSION").expect("`CARGO_PKG_VERSION` variable not set");

  let git_rev = git_revision_auto(manifest_dir).expect("failed to determine Git revision");
  if let Some(git_rev) = git_rev {
    println!("cargo:rustc-env=VERSION={pkg_version} ({git_rev})");
  } else {
    println!("cargo:rustc-env=VERSION={pkg_version}");
  }
}

fn main() {
  let manifest_dir =
    env::var_os("CARGO_MANIFEST_DIR").expect("failed to read `CARGO_MANIFEST_DIR` variable");
  let manifest_dir = PathBuf::from(manifest_dir);

  let () = build_init(&manifest_dir);
  let () = determine_version(&manifest_dir);
}
