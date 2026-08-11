//! Wall-time and operation accounting for an index run, active only when
//! `BIDSLAKE_TIMING` is set.
//!
//! A phase breakdown is the only practical way to tell a slow filesystem from slow
//! code, and the two want different fixes. What is measured is therefore chosen to
//! *separate* them: [`Phase::Walk`] and [`Phase::Prefetch`] are dominated by
//! round-trip latency, [`Phase::Inherit`] and [`Phase::Associations`] by CPU, and
//! [`Phase::Commit`] by how the database file is stored.
//!
//! The counters exist for the same reason. On a latency-bound filesystem wall time is
//! roughly `operations × round-trip`, so a count of directories, files, and body reads
//! turns a measured RTT into a prediction — and turns a regression that adds an
//! operation per file into something visible rather than merely slow.
//!
//! State is process-global. The alternative — threading a handle through `parse` and
//! its callees — is what the previous five-`Option<Duration>`-parameter reporter did,
//! and it could not reach the phases that live in `main` (schema load, DDL, and the
//! commit, which on a network-hosted catalog is not a small share). Nothing here is
//! read except by [`report`], so a global costs nothing in clarity.
//!
//! Everything is gated on one `bool` read once at startup: when timing is off,
//! [`scope`] returns a guard holding `None` and its `Drop` is a single branch.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// A phase of an index run. Declaration order is reporting order: phases in the order
/// they run, each followed by its own sub-slices (see [`Phase::is_nested`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Parsing the BIDS schema and merging overlays/adapters.
    SchemaLoad,
    /// `CREATE TABLE` plus the provenance stamps (which serialize the whole
    /// effective schema).
    Ddl,
    /// The directory walk and the categorize loop over its result.
    Walk,
    /// The concurrent prefetch of file bodies and TSV headers.
    Prefetch,
    /// The serial per-file passes (prefetch excluded).
    Process,
    /// The slice of `Process` spent in the tabular `read_csv` ingest.
    TabularReadCsv,
    /// Reading back the dataset's existing `scans` rows before the bulk append.
    ScansScan,
    /// Resolving the schema's structural associations over the file tree.
    Associations,
    /// Everything else between the passes and inheritance: links, diffusion,
    /// orphan promotion, the `scans` append.
    Finalize,
    /// Sidecar inheritance and the `sidecars` append.
    Inherit,
    /// Ingesting the deferred headerless recordings.
    Flush,
    /// `COMMIT` — where a file-backed database is actually written and fsynced.
    Commit,
}

const PHASES: &[Phase] = &[
    Phase::SchemaLoad,
    Phase::Ddl,
    Phase::Walk,
    Phase::Prefetch,
    Phase::Process,
    Phase::TabularReadCsv,
    Phase::Finalize,
    Phase::ScansScan,
    Phase::Associations,
    Phase::Inherit,
    Phase::Flush,
    Phase::Commit,
];

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Phase::SchemaLoad => "schema",
            Phase::Ddl => "ddl",
            Phase::Walk => "walk",
            Phase::Prefetch => "prefetch",
            Phase::Process => "process",
            Phase::TabularReadCsv => "  tabular read_csv",
            Phase::ScansScan => "  scans read-back",
            Phase::Associations => "  associations",
            Phase::Finalize => "finalize",
            Phase::Inherit => "inherit",
            Phase::Flush => "flush",
            Phase::Commit => "commit",
        }
    }

    /// Whether this phase is a slice of another (shown indented) and so must not be
    /// added into the accounted total, which would otherwise exceed wall time.
    fn is_nested(self) -> bool {
        matches!(
            self,
            Phase::TabularReadCsv | Phase::ScansScan | Phase::Associations
        )
    }
}

/// A countable quantity. Counts are charged next to the operations they describe, so
/// each is one relaxed atomic add against work that is orders of magnitude larger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counter {
    /// Directories the walk descended into (one `readdir` each).
    Dirs,
    /// Files the walk recorded.
    Files,
    /// File bodies read whole, in Rust.
    BodiesRead,
    /// TSV headers read as a bounded prefix.
    HeadsRead,
    /// Data files registered in `scans`.
    ImagingFiles,
    /// JSON sidecars collected.
    Sidecars,
    /// Tabular files deferred to the batched ingest.
    PendingTabular,
    /// `(table, header)` groups the batched ingest issued.
    TabularGroups,
    /// Files in the largest such group.
    TabularGroupMax,
    /// File-association rows resolved.
    Associations,
}

const COUNTERS: &[Counter] = &[
    Counter::Dirs,
    Counter::Files,
    Counter::BodiesRead,
    Counter::HeadsRead,
    Counter::ImagingFiles,
    Counter::Sidecars,
    Counter::PendingTabular,
    Counter::TabularGroups,
    Counter::TabularGroupMax,
    Counter::Associations,
];

impl Counter {
    fn label(self) -> &'static str {
        match self {
            Counter::Dirs => "dirs",
            Counter::Files => "files",
            Counter::BodiesRead => "bodies_read",
            Counter::HeadsRead => "headers_read",
            Counter::ImagingFiles => "data_files",
            Counter::Sidecars => "sidecars",
            Counter::PendingTabular => "tabular_files",
            Counter::TabularGroups => "tabular_groups",
            Counter::TabularGroupMax => "tabular_group_max",
            Counter::Associations => "associations",
        }
    }
}

struct Timing {
    enabled: bool,
    started: Instant,
    phase_nanos: Vec<AtomicU64>,
    counters: Vec<AtomicU64>,
}

fn timing() -> &'static Timing {
    static TIMING: OnceLock<Timing> = OnceLock::new();
    TIMING.get_or_init(|| Timing {
        enabled: std::env::var_os("BIDSLAKE_TIMING").is_some(),
        started: Instant::now(),
        phase_nanos: PHASES.iter().map(|_| AtomicU64::new(0)).collect(),
        counters: COUNTERS.iter().map(|_| AtomicU64::new(0)).collect(),
    })
}

fn phase_slot(phase: Phase) -> usize {
    PHASES
        .iter()
        .position(|p| *p == phase)
        .expect("known phase")
}

fn counter_slot(counter: Counter) -> usize {
    COUNTERS
        .iter()
        .position(|c| *c == counter)
        .expect("known counter")
}

/// Whether timing is on. Call sites that would have to *compute* something to report
/// it can check this first; a plain [`count`] does not need to.
pub fn enabled() -> bool {
    timing().enabled
}

/// Start timing `phase`; the elapsed time is added when the returned guard drops.
///
/// Returning a guard rather than a start/stop pair means a phase cannot be left
/// unclosed on an early return — which the previous hand-balanced version could be.
pub fn scope(phase: Phase) -> Guard {
    Guard {
        phase,
        start: timing().enabled.then(Instant::now),
    }
}

/// Adds its phase's elapsed time on drop. Inert when timing is off.
pub struct Guard {
    phase: Phase,
    start: Option<Instant>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            timing().phase_nanos[phase_slot(self.phase)]
                .fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }
}

/// Add `n` to `counter`.
pub fn count(counter: Counter, n: u64) {
    let t = timing();
    if t.enabled {
        t.counters[counter_slot(counter)].fetch_add(n, Ordering::Relaxed);
    }
}

/// Raise `counter` to `n` if `n` is larger (for high-water marks like the largest
/// tabular group).
pub fn count_max(counter: Counter, n: u64) {
    let t = timing();
    if t.enabled {
        t.counters[counter_slot(counter)].fetch_max(n, Ordering::Relaxed);
    }
}

/// Print the breakdown to stderr. No-op when timing is off.
///
/// Wall time is reported alongside the phase sum rather than instead of it: the gap
/// between them is unaccounted work, and printing only the sum (as the previous
/// reporter did) made that gap invisible.
pub fn report() {
    let t = timing();
    if !t.enabled {
        return;
    }
    let wall = t.started.elapsed();
    let ms = |slot: usize| t.phase_nanos[slot].load(Ordering::Relaxed) as f64 / 1e6;

    let accounted: f64 = PHASES
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.is_nested())
        .map(|(i, _)| ms(i))
        .sum();

    eprintln!("[timing] phase breakdown (ms)");
    for (i, phase) in PHASES.iter().enumerate() {
        let v = ms(i);
        if v > 0.0 {
            eprintln!("[timing]   {:<20} {:>10.0}", phase.label(), v);
        }
    }
    eprintln!(
        "[timing]   {:<20} {:>10.0}  (accounted {:.0}, unaccounted {:.0})",
        "WALL",
        wall.as_secs_f64() * 1e3,
        accounted,
        (wall.as_secs_f64() * 1e3 - accounted).max(0.0),
    );

    let counters: Vec<String> = COUNTERS
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let v = t.counters[i].load(Ordering::Relaxed);
            (v > 0).then(|| format!("{}={}", c.label(), v))
        })
        .collect();
    if !counters.is_empty() {
        eprintln!("[timing] counts  {}", counters.join("  "));
    }
    let files = t.counters[counter_slot(Counter::Files)].load(Ordering::Relaxed);
    if files > 0 && wall.as_secs_f64() > 0.0 {
        eprintln!(
            "[timing] rate    {:.0} files/s over {:.1}s",
            files as f64 / wall.as_secs_f64(),
            wall.as_secs_f64(),
        );
    }
}
