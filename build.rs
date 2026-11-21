//! Build script: embeds the git commit for the `telemetry` feature's
//! `session` record (`JUMPKEY_GIT_SHA`, `JUMPKEY_GIT_DIRTY`).
//!
//! Only runs git when the feature is on; otherwise (and outside a git
//! checkout, e.g. a crates.io build) the values are `unknown` / `0`.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_TELEMETRY");
    let telemetry = std::env::var_os("CARGO_FEATURE_TELEMETRY").is_some();
    let (sha, dirty) = if telemetry {
        // Re-run when HEAD moves (checkout, commit) or the index changes.
        if let Some(git_dir) = git(&["rev-parse", "--git-dir"]) {
            println!("cargo:rerun-if-changed={git_dir}/HEAD");
            println!("cargo:rerun-if-changed={git_dir}/index");
            if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
                println!("cargo:rerun-if-changed={git_dir}/{head_ref}");
            }
        }
        let sha = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
        let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        (sha, dirty)
    } else {
        ("unknown".to_string(), false)
    };
    println!("cargo:rustc-env=JUMPKEY_GIT_SHA={sha}");
    println!("cargo:rustc-env=JUMPKEY_GIT_DIRTY={}", u8::from(dirty));
}
