//! How many filesystem operations an ingest keeps in flight.
//!
//! Two dials rather than one, because the operations divide by which server answers them. A
//! `readdir` and a `stat` are **metadata** operations — on Lustre they go to the MDS, on NFS to
//! whatever serves attributes — while reading a JSON sidecar, a `.bval` or a TSV header is a
//! **data** operation served by the OSSes. The two saturate independently, and a metadata
//! server that is already struggling gets slower, not faster, as more requests arrive. Tuning
//! them together would mean throttling bulk reads to protect the MDS, or hammering the MDS to
//! keep reads wide.
//!
//! Both default to the behaviour that was hardcoded before they existed, so an ingest that sets
//! neither runs exactly as it did.
//!
//! Set them per run with `--metadata-concurrency` / `--read-concurrency`, or per site with
//! `BIDSLAKE_METADATA_CONCURRENCY` / `BIDSLAKE_READ_CONCURRENCY`; the flags win where both are
//! given. The metadata dial also reaches the directory walk in `bids-core`, which
//! `bids-validator` shares — so the environment variable tunes validation runs too, while the
//! flags are bidslake's own.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Zero means "not set": fall back to the environment, then to the default.
static METADATA: AtomicUsize = AtomicUsize::new(0);
static DATA: AtomicUsize = AtomicUsize::new(0);

/// The default width for both dials, and the value both were fixed at before they were
/// configurable.
///
/// Not a core count. What it bounds is round trips in flight, and the number that matters is
/// the filesystem's, not this machine's: sixteen concurrent stats is nothing for a local SSD
/// and may be more than a loaded metadata server wants.
const DEFAULT: usize = 16;

/// Read a dial: an explicit override, else `var`, else [`DEFAULT`].
fn resolve(cell: &AtomicUsize, var: &str) -> usize {
    choose(
        cell.load(Ordering::Relaxed),
        std::env::var(var).ok().as_deref(),
    )
}

/// The precedence itself, with the environment passed in rather than read.
///
/// Split out so it can be tested: `set_var` is process-global and, in a test binary that runs
/// its tests on several threads, racing it against any other thread's `getenv` is undefined
/// behaviour — which is why Rust 2024 made it `unsafe`. A pure function needs none of that.
///
/// A value that does not parse, or is zero, is ignored rather than fatal: a tuning knob
/// mistyped in a job script should not fail an ingest that would otherwise succeed.
fn choose(override_width: usize, env: Option<&str>) -> usize {
    if override_width > 0 {
        return override_width;
    }
    env.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT)
}

/// Directory reads and stats in flight at once.
pub fn metadata() -> usize {
    resolve(&METADATA, "BIDSLAKE_METADATA_CONCURRENCY")
}

/// File-body and header reads in flight at once.
pub fn data() -> usize {
    resolve(&DATA, "BIDSLAKE_READ_CONCURRENCY")
}

/// Override either dial for this process. `None` leaves it to the environment.
///
/// Called once from `main` before any walk, so the walk in `bids-core` — which has no access to
/// bidslake's CLI — is configured through its own setter rather than by reading the flag.
pub fn configure(metadata_width: Option<usize>, data_width: Option<usize>) {
    if let Some(n) = metadata_width.filter(|&n| n > 0) {
        METADATA.store(n, Ordering::Relaxed);
    }
    if let Some(n) = data_width.filter(|&n| n > 0) {
        DATA.store(n, Ordering::Relaxed);
    }
    bids_core::filetree::set_walk_threads(metadata());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unset dial is the historical default, so an ingest that configures nothing runs
    /// exactly as it did before the dials existed.
    #[test]
    fn an_unset_dial_is_the_previous_hardcoded_width() {
        let width = choose(0, None);

        assert_eq!(width, DEFAULT);
    }

    /// The environment sets it for a site.
    #[test]
    fn the_environment_sets_the_width() {
        let width = choose(0, Some("4"));

        assert_eq!(width, 4);
    }

    /// An explicit override beats the environment — what makes the flag win over the
    /// site-wide setting.
    #[test]
    fn an_override_beats_the_environment() {
        let width = choose(4, Some("64"));

        assert_eq!(width, 4);
    }

    /// A malformed value falls back rather than failing the run.
    #[test]
    fn a_malformed_environment_value_falls_back_to_the_default() {
        let width = choose(0, Some("wide please"));

        assert_eq!(width, DEFAULT);
    }

    /// Zero means "unset", not "no concurrency" — a width of zero would deadlock the
    /// bounded streams it feeds.
    #[test]
    fn zero_is_treated_as_unset() {
        let width = choose(0, Some("0"));

        assert_eq!(width, DEFAULT);
    }
}
