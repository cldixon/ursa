//! Sets `URSA_VERSION`, the version `ursa --version` and the shell print.
//!
//! A release (a plain version such as `0.7.0`) prints just that. A
//! development version (`0.7.0-dev`) also names the commit it was built from,
//! when the build runs in a git checkout, so bug reports from builds of `main`
//! can be told apart: `0.7.0-dev (a1b2c3d4e)`, with `-dirty` for uncommitted
//! changes. Builds from crates.io have no git checkout and print the version.
//!
//! A nightly build (.github/workflows/nightly-cli.yml) sets
//! `URSA_BUILD_VERSION`, e.g. `0.8.0-nightly.20261107`, to print in place of
//! the crate's `0.8.0-dev`. It must be a prerelease of the same `X.Y.Z`, so it
//! also names its commit: `0.8.0-nightly.20261107 (a1b2c3d4e)`. The version
//! says the channel: a plain `X.Y.Z` is a stable release, `-nightly.` a
//! nightly, and `-dev` a build from source.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}

fn main() {
    let crate_version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    println!("cargo:rerun-if-env-changed=URSA_BUILD_VERSION");
    let version = match std::env::var("URSA_BUILD_VERSION") {
        Ok(v) if !v.is_empty() => {
            let base = |s: &str| s.split('-').next().unwrap_or_default().to_string();
            assert!(
                v.contains('-') && base(&v) == base(&crate_version),
                "URSA_BUILD_VERSION={v} must be a prerelease of the crate's version \
                 {crate_version}, such as {}-nightly.20261107",
                base(&crate_version)
            );
            v
        }
        _ => crate_version,
    };
    let mut full = version.clone();
    if version.contains('-') {
        if let Some(hash) = git(&["rev-parse", "--short=9", "HEAD"]) {
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
                .is_some_and(|s| !s.is_empty());
            full = format!("{version} ({hash}{})", if dirty { "-dirty" } else { "" });
        }
        // Rebuild when HEAD moves. Paths are relative to this crate.
        if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
            println!("cargo:rerun-if-changed={head}");
        }
        if let Some(refs) = git(&["rev-parse", "--git-path", "refs/heads"]) {
            println!("cargo:rerun-if-changed={refs}");
        }
        if let Some(index) = git(&["rev-parse", "--git-path", "index"]) {
            println!("cargo:rerun-if-changed={index}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-env=URSA_VERSION={full}");
}
