//! Stamps the build with the commit it came from.
//!
//! A binary is usually copied to wherever it runs — a cluster login node, a scratch
//! directory — and the copy carries nothing that says which source it was built from.
//! When a run is being profiled that ambiguity is expensive: an unchanged timing
//! report is equally consistent with "the fix did not work" and "this is last week's
//! binary", and telling those apart costs another full run.
//!
//! So `BIDSLAKE_BUILD` becomes the package version plus the short commit hash, with
//! `-dirty` when the tree had uncommitted changes. It surfaces in `--version`, in the
//! `BIDSLAKE_TIMING` report, and in the `bidslake_meta` row stamped into every
//! catalog.
//!
//! Degrades quietly: outside a git checkout (a published crate, a source tarball) the
//! version alone is stamped.

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();

    // Rebuild when HEAD moves. `.git` may be a file (a worktree or submodule), in
    // which case there is no HEAD to watch and the stamp is simply refreshed on the
    // next full build.
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        let head = Path::new(&manifest).join("../../.git/HEAD");
        if head.is_file() {
            println!("cargo:rerun-if-changed={}", head.display());
        }
    }

    let build = match git(&["rev-parse", "--short", "HEAD"]) {
        Some(hash) => {
            let dirty = git(&["status", "--porcelain"]).is_some();
            let suffix = if dirty { "-dirty" } else { "" };
            format!("{version} ({hash}{suffix})")
        }
        None => version,
    };
    println!("cargo:rustc-env=BIDSLAKE_BUILD={build}");
}
