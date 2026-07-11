//! Validation benchmarks.
//!
//! Two groups:
//!   * `validate_in_process` — the Rust validator called in-process (pure validation time,
//!     schema parsed once up front).
//!   * `validate_cli_end_to_end` — the Rust binary vs the reference TypeScript validator
//!     (`deno run …/bids-validator.ts`), each run as a subprocess so the numbers include
//!     process startup and schema loading (what a user actually experiences at the CLI).
//!
//! The TS comparison is skipped automatically when `deno` (or the vendored TS source) is
//! unavailable. Run with `cargo bench`; datasets are chosen to avoid network access
//! (no HED schema fetching).

use criterion::{Criterion, criterion_group, criterion_main};
use std::path::{Path, PathBuf};
use std::process::Command;

use bids_validator_rs::schema::BidsSchema;
use bids_validator_rs::validator::validate;

/// Path to the compiled `bids-validator` binary (provided by cargo to benches).
const RUST_BIN: &str = env!("CARGO_BIN_EXE_bids-validator");
/// Vendored reference TS validator entry point (relative to the crate root).
const TS_ENTRY: &str = "lib/bids-validator/src/bids-validator.ts";
/// Datasets to benchmark: pure-fMRI (no HED), so no schema/HED network access is needed.
const DATASETS: &[&str] = &["ds007", "7t_trt"];

fn dataset(name: &str) -> PathBuf {
    PathBuf::from("tests/data/bids-examples").join(name)
}

fn deno_available() -> bool {
    Command::new("deno")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn bench_in_process(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let schema = BidsSchema::bundled().unwrap();

    let mut group = c.benchmark_group("validate_in_process");
    for &name in DATASETS {
        let path = dataset(name);
        if !path.is_dir() {
            continue;
        }
        group.bench_function(name, |b| {
            b.to_async(&rt).iter(|| async {
                let _ = validate(&path, &schema, None).await;
            });
        });
    }
    group.finish();
}

fn bench_cli(c: &mut Criterion) {
    let have_deno = deno_available() && Path::new(TS_ENTRY).exists();
    if !have_deno {
        eprintln!("deno not found or {TS_ENTRY} missing; skipping TS comparison benchmarks.");
    }

    let mut group = c.benchmark_group("validate_cli_end_to_end");
    // Subprocess benchmarks (especially deno startup) are slow; keep the sample count modest.
    group.sample_size(10);
    for &name in DATASETS {
        let path = dataset(name);
        if !path.is_dir() {
            continue;
        }
        group.bench_function(format!("rust/{name}"), |b| {
            b.iter(|| {
                Command::new(RUST_BIN)
                    .arg("--json")
                    .arg(&path)
                    .output()
                    .expect("run rust CLI");
            });
        });
        if have_deno {
            group.bench_function(format!("ts/{name}"), |b| {
                b.iter(|| {
                    Command::new("deno")
                        .args(["run", "-A", TS_ENTRY])
                        .arg(&path)
                        .arg("--json")
                        .output()
                        .expect("run TS CLI");
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_in_process, bench_cli);
criterion_main!(benches);
