//! `bidslake-synth` — write a synthetic BIDS or derivative tree.
//!
//! Two things it is for, and they want different flags. A benchmark wants scale and uniformity:
//! `--subjects`, `--runs`, `--confound-columns`, and nothing that makes one subject differ from
//! another. Authoring an adapter bundle wants fidelity to the documents: `--layout`,
//! `--term-map`, `--ingestion` by path, one subject, and `--explain` to read back what each
//! generated path classifies as before any real data exists.

use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use bids_schema::layout::{BUNDLED_LAYOUT_NAMES, bundled_layout, load_layout};
use bids_schema::term_map::{TermMap, bundled_term_map, load_term_map};
use bidslake::schema::ingestion::Disposition;
use bidslake::schema::tabular::FileContext;
use bidslake::schema::{AppliedOverlay, Ingestion, Schema};
use bidslake_synth::{Claim, Hazards, Plan, Producer, Scale};
use clap::Parser;
use serde_json::Value;

#[derive(Parser)]
#[command(name = "bidslake-synth")]
#[command(about = "Write a synthetic BIDS or derivative tree, driven by the bundled schemas")]
struct Cli {
    /// Directory to create the tree in.
    out: PathBuf,

    /// What to build: `raw`, `fmriprep`, or the name of a bundled layout (`feat`,
    /// `freesurfer`). Repeatable; each producer gets its own subdirectory under `out`.
    #[arg(long = "producer", default_values_t = [String::from("raw")])]
    producers: Vec<String>,

    /// A layout document to build from, by path — a producer bidslake does not bundle. Its
    /// `TermMap` must still name a bundled term map, which is what the round trip needs.
    #[arg(long = "layout")]
    layouts: Vec<PathBuf>,

    /// A schema overlay to merge, by path. Same meaning as `bidslake index --overlay`.
    #[arg(long = "overlay")]
    overlays: Vec<PathBuf>,

    /// A term map to load, by path. Used for the `--explain` report and for verification;
    /// bundled adapters supply their own.
    #[arg(long = "term-map")]
    term_maps: Vec<PathBuf>,

    /// An ingestion fragment to merge onto the base, by path.
    #[arg(long = "ingestion")]
    ingestion: Vec<PathBuf>,

    /// A bundled adapter whose overlay, term map and ingestion fragment to apply, by name.
    #[arg(long = "adapter")]
    adapters: Vec<String>,

    #[arg(long, default_value_t = 2)]
    subjects: usize,
    #[arg(long, default_value_t = 1)]
    sessions: usize,
    #[arg(long, default_value_t = 2)]
    runs: usize,
    /// Task labels to cycle through.
    #[arg(long = "tasks", value_delimiter = ',', default_values_t = [String::from("rest")])]
    tasks: Vec<String>,
    /// `space-` labels to emit per run, beyond the native-space file.
    #[arg(long = "spaces", value_delimiter = ',', default_values_t = [String::from("MNI152NLin2009cAsym")])]
    spaces: Vec<String>,
    /// Total confound columns, declared plus undeclared.
    #[arg(long, default_value_t = 16)]
    confound_columns: usize,
    /// Rows in each generated table. For confounds, the volume count.
    #[arg(long, default_value_t = 8)]
    confound_rows: usize,

    /// The measured shape of a real fMRIPrep 25.2.5 confounds file: 1841 columns by 450 rows.
    #[arg(long)]
    preset_a2cps_melodic: bool,

    /// Pathological shapes to include, comma-separated, or `all`. Off by default: every one of
    /// them breaks the uniformity a scale sweep depends on.
    #[arg(long, default_value = "")]
    hazards: String,

    /// Write the `.bidsignore` fMRIPrep really writes. Off by default, because two of its
    /// patterns hide the files this tree exists to exercise.
    #[arg(long)]
    bidsignore: bool,

    /// Print the plan and write nothing.
    #[arg(long)]
    dry_run: bool,

    /// For each planned file, report how it classifies and what ingest would do with it, then
    /// write nothing. Exits nonzero if any path is claimed by nothing.
    #[arg(long)]
    explain: bool,

    /// Print the `bidslake index` commands this tree needs, and exit.
    #[arg(long)]
    print_index: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut scale = Scale {
        subjects: cli.subjects,
        sessions: cli.sessions,
        runs: cli.runs,
        tasks: cli.tasks.clone(),
        spaces: cli.spaces.clone(),
        confound_columns: cli.confound_columns,
        confound_rows: cli.confound_rows,
    };
    if cli.preset_a2cps_melodic {
        scale = scale.a2cps_melodic();
    }
    let hazards = Hazards::parse(&cli.hazards)?;

    let mut producers: Vec<Producer> = Vec::new();
    for name in &cli.producers {
        producers.push(named_producer(name)?);
    }
    for path in &cli.layouts {
        let layout =
            load_layout(path).with_context(|| format!("loading layout {}", path.display()))?;
        producers.push(Producer::Layout(Box::new(layout)));
    }
    if producers.is_empty() {
        bail!("nothing to build: pass --producer or --layout");
    }

    // Every adapter *any* producer needs, applied to every printed index command. ADR 0006: the
    // adapter set describes the **catalog**, not the dataset being added, so a run narrower than
    // the catalog is refused — indexing an fMRIPrep tree first and a FreeSurfer tree second into
    // one database fails with "no column for projected" unless both runs name both adapters.
    let catalog_adapters: Vec<String> = {
        let mut all: Vec<String> = Vec::new();
        for producer in &producers {
            for adapter in producer.adapters() {
                if !all.iter().any(|a| a == adapter) {
                    all.push(adapter.to_string());
                }
            }
        }
        for adapter in &cli.adapters {
            if !all.contains(adapter) {
                all.push(adapter.clone());
            }
        }
        all
    };

    let mut exit_code = 0;
    for producer in producers {
        let name = producer.name().to_string();
        let schema = build_schema(&cli, &producer)?;
        let term_maps = build_term_maps(&cli, &producer)?;
        let root = cli.out.join(&name);

        if cli.print_index {
            print_index(&root, &producer, &catalog_adapters);
            continue;
        }

        let plan = Plan::producer(producer)
            .scale(scale.clone())
            .hazards(hazards)
            .bidsignore(cli.bidsignore);

        if cli.explain {
            exit_code |= explain(&plan, &schema, &term_maps)? as i32;
            continue;
        }
        if cli.dry_run {
            for file in plan.paths(&schema)? {
                println!("{}", file.rel_path);
            }
            continue;
        }

        let manifest = plan.write(&root, &schema)?;
        let counts = manifest.claim_counts();
        let hazard_note = if manifest.hazards.any() {
            format!(", hazards {}", manifest.hazards.enabled().join(","))
        } else {
            String::new()
        };
        println!(
            "{name}: {} files under {} ({}){hazard_note}",
            manifest.files.len(),
            root.display(),
            counts
                .iter()
                .map(|(k, v)| format!("{v} {k}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        if let Err(violations) = manifest.verify(&schema, &term_maps) {
            for violation in &violations {
                eprintln!(
                    "  unverified: {} — {}",
                    violation.rel_path, violation.detail
                );
            }
            exit_code = 1;
        }
        print_index(&root, plan.producer_of(), &catalog_adapters);
    }

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

fn named_producer(name: &str) -> Result<Producer> {
    match name {
        "raw" => Ok(Producer::Raw),
        "fmriprep" => Ok(Producer::Fmriprep),
        other => match bundled_layout(other) {
            Some(layout) => Ok(Producer::Layout(Box::new(layout))),
            None => bail!(
                "unknown producer {other:?}; known producers are `raw`, `fmriprep`, and the \
                 bundled layouts {BUNDLED_LAYOUT_NAMES:?}"
            ),
        },
    }
}

/// Every bundled adapter this run applies: the producer's own, plus whatever `--adapter` named.
///
/// The producer's own is not optional. An fMRIPrep tree built against a schema without the
/// `fmriprep` overlay has confounds that route nowhere, and a benchmark over it measures the
/// walk rather than the tabular path.
fn adapter_names(cli: &Cli, producer: &Producer) -> Vec<String> {
    let mut names: Vec<String> = producer
        .adapters()
        .into_iter()
        .map(str::to_string)
        .collect();
    for name in &cli.adapters {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    names
}

/// The effective schema: the producer's own adapter overlay, plus whatever the caller named.
fn build_schema(cli: &Cli, producer: &Producer) -> Result<Schema> {
    let adapters = adapter_names(cli, producer);
    let mut overlays: Vec<AppliedOverlay> = Vec::new();
    for name in &adapters {
        if let Some(content) = bids_schema::overlay::bundled_overlay(name) {
            overlays.push(AppliedOverlay {
                source: name.clone(),
                content,
            });
        }
    }
    for path in &cli.overlays {
        let content = bids_schema::overlay::load_overlay(path)
            .with_context(|| format!("loading overlay {}", path.display()))?;
        overlays.push(AppliedOverlay {
            source: path.display().to_string(),
            content,
        });
    }

    let mut sources: Vec<String> = vec![
        bids_schema::bundled_ingestion_source("base")
            .expect("bundled base ingestion")
            .to_string(),
    ];
    for name in &adapters {
        if let Some(src) = bids_schema::bundled_ingestion_source(name) {
            sources.push(src.to_string());
        }
    }
    for path in &cli.ingestion {
        sources.push(
            std::fs::read_to_string(path)
                .with_context(|| format!("reading ingestion fragment {}", path.display()))?,
        );
    }
    let refs: Vec<&str> = sources.iter().map(String::as_str).collect();
    let ingestion = Ingestion::from_sources(&refs)?;

    let term_maps = build_term_maps(cli, producer)?;
    Schema::load_full(None, &overlays, ingestion, &term_maps)
}

fn build_term_maps(cli: &Cli, producer: &Producer) -> Result<Vec<TermMap>> {
    let mut maps = Vec::new();
    for name in adapter_names(cli, producer) {
        if let Some(map) = bundled_term_map(&name) {
            maps.push(map);
        }
    }
    for path in &cli.term_maps {
        maps.push(
            load_term_map(path).with_context(|| format!("loading term map {}", path.display()))?,
        );
    }
    Ok(maps)
}

/// Per planned file: what claims it, what it projects, and what ingest would do. Returns whether
/// anything went unclaimed.
fn explain(plan: &Plan, schema: &Schema, term_maps: &[TermMap]) -> Result<bool> {
    let mut unclaimed = false;
    for file in plan.paths(schema)? {
        let facts = term_maps.iter().find_map(|tm| tm.classify(&file.rel_path));
        // A BIDS-named file has no projection, so its facts come from its own name — the same
        // two sources ingest itself uses, in the same order.
        let name = file.rel_path.rsplit('/').next().unwrap_or(&file.rel_path);
        let parts = bids_core::entities::read_entities(name);
        let (datatype, suffix, extension) = match &facts {
            Some(f) => (
                f.datatype.as_deref(),
                f.suffix.as_deref(),
                f.extension.as_deref(),
            ),
            None => (
                file.rel_path.split('/').nth_back(1),
                (!parts.suffix.is_empty()).then_some(parts.suffix.as_str()),
                (!parts.extension.is_empty()).then_some(parts.extension.as_str()),
            ),
        };

        let slashed = format!("/{}", file.rel_path);
        let null = Value::Null;
        let ctx = FileContext {
            path: &slashed,
            datatype,
            suffix,
            extension,
            sidecar: &null,
            dataset_type: Some("derivative"),
        };
        let rule = schema.ingestion().classify(&ctx);
        let disposition = match rule.map(|r| r.disposition) {
            Some(Disposition::Read) => format!(
                "read/{}",
                rule.and_then(|r| r.reader.as_deref()).unwrap_or("?")
            ),
            Some(Disposition::Catalog) => "catalog".to_string(),
            Some(Disposition::Ignore) => "ignore".to_string(),
            None => "unmatched".to_string(),
        };
        let table = schema
            .tabular()
            .route(&ctx)
            .map(|s| s.table.as_str())
            .unwrap_or("-");

        let claimed = match &file.claim {
            Claim::Projected { .. } => facts.is_some(),
            _ => true,
        };
        if !claimed {
            unclaimed = true;
        }

        println!(
            "{:<70} {:<10} {:<12} {:<14} {}",
            file.rel_path,
            if facts.is_some() { "projected" } else { "bids" },
            suffix.unwrap_or("-"),
            disposition,
            table
        );
    }
    Ok(unclaimed)
}

/// The `bidslake index` command this tree needs.
///
/// `catalog_adapters` is every adapter *any* generated tree needs, not just this one's, because
/// they all land in one catalog and the adapter set describes the catalog (ADR 0006). Printing
/// only the producer's own would give a command that works for the first tree and is refused for
/// the second.
fn print_index(root: &std::path::Path, producer: &Producer, catalog_adapters: &[String]) {
    let adapters: String = catalog_adapters
        .iter()
        .map(|a| format!(" --adapter {a}"))
        .collect();
    // fMRIPrep hides its transforms and confounds behind `.bidsignore`, so an index that does
    // not pass this flag walks past exactly the files the tree was generated for.
    let ignore = match producer {
        Producer::Fmriprep => " --no-bidsignore",
        _ => "",
    };
    println!(
        "bidslake index --input {} --output synthetic.duckdb --dataset-id {}{adapters}{ignore}",
        root.display(),
        producer.name()
    );
}
