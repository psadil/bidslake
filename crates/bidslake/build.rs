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

use std::path::{Path, PathBuf};
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

fn watch(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// Everything whose change should re-stamp the build.
///
/// `.git/HEAD` alone is not enough, and getting this wrong is how a stale stamp once
/// mislabelled a profiling run: on a branch, `HEAD` holds `ref: refs/heads/<branch>`
/// and that text is identical before and after a commit — the new sha lands in the
/// ref file it points at, or in `packed-refs` once the ref has been packed. So all
/// three are watched.
///
/// The crate's own sources are watched too, since editing them is what makes the tree
/// dirty in practice. That part is best-effort by nature: a build script cannot watch
/// the whole repository, so `-dirty` can lag a change made in a sibling crate. The
/// commit hash, which is the part a measurement is traced by, cannot.
fn watch_git_state(manifest: &Path) {
    watch(&manifest.join("src"));
    watch(&manifest.join("Cargo.toml"));

    let Some(git_dir) = manifest
        .ancestors()
        .map(|a| a.join(".git"))
        .find(|p| p.is_dir())
    else {
        return;
    };
    let head = git_dir.join("HEAD");
    watch(&head);
    watch(&git_dir.join("packed-refs"));

    // Follow the symref to the file that actually carries the sha.
    if let Ok(contents) = std::fs::read_to_string(&head)
        && let Some(reference) = contents.strip_prefix("ref: ")
    {
        watch(&git_dir.join(reference.trim()));
    }
}

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    watch_git_state(&manifest);

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
