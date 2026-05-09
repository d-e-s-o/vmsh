//! Build script for `vmsh`.

use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use grev::git_revision_auto;


fn build_init(manifest_dir: &Path) {
  let init_dir = manifest_dir.join("init");

  let out_dir = env::var("OUT_DIR").expect("failed to read `OUT_DIR` variable");
  let out_dir = PathBuf::from(out_dir);
  let init_target_dir = out_dir.join("init-target");
  let init_bin = out_dir.join("vmsh-init");

  let debug = env::var("DEBUG").expect("failed to read `DEBUG` variable");
  let (profile, target_dir) = match debug.as_ref() {
    "true" => ("dev", "debug"),
    "false" => ("release", "release"),
    _ => {
      panic!("encountered unexpected value in `DEBUG` variable: {debug}")
    },
  };
  let build_dir_cfg = format!("build.build-dir='{}'", init_target_dir.display());

  let cargo = env::var("CARGO").expect("`CARGO` variable not present");
  // TODO: Once we publish the crate we need to figure out how to rely
  //       on a crates.io released `vmsh-init` instead.
  let output = Command::new(&cargo)
    .args([
      "build",
      "--bin=vmsh-init",
      "--profile",
      profile,
      "--target-dir",
    ])
    .arg(&init_target_dir)
    // NB: We explicitly set the `build.build-dir` configuration here,
    //     because without it a recursive `cargo build` may deadlock
    //     trying to acquire a lock on the build directory, depending on
    //     user configuration.
    .arg("--config")
    .arg(&build_dir_cfg)
    .current_dir(&init_dir)
    // Clear inherited flags to avoid host-specific settings leaking
    // into the init build.
    .env_remove("RUSTFLAGS")
    .env_remove("CARGO_ENCODED_RUSTFLAGS")
    .output()
    .unwrap_or_else(|e| panic!("failed to run `cargo build` for init crate: {e}"));

  if !output.status.success() {
    let stderr = String::from_utf8_lossy(&output.stderr);
    panic!("failed to build init binary:\n{stderr}");
  }

  let built = init_target_dir.join(target_dir).join("vmsh-init");
  let _cnt =
    fs::copy(&built, &init_bin).unwrap_or_else(|e| panic!("failed to copy init binary: {e}"));

  println!("cargo:rerun-if-changed=init/init.rs");
  println!("cargo:rerun-if-changed=init/Cargo.toml");
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
