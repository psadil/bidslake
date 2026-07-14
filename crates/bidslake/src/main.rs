use anyhow::Result;
use clap::{Parser, Subcommand};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

mod bids;
mod db;
mod fs;
mod readers;
mod s3;
mod schema;

use bids::BidsParser;
use db::BidsDb;
use fs::{BidsFileSystem, LocalFileSystem};
use schema::Schema;

#[derive(Parser)]
#[command(name = "bidslake")]
#[command(about = "Convert BIDS datasets to DuckDB lakehouse format")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Index a BIDS dataset into a DuckDB database (also used to bring
    /// additional datasets under management — see the README on managed mode).
    Index {
        /// Input BIDS dataset directory or S3 URI (e.g., s3://bucket/prefix)
        #[arg(short, long)]
        input: String,

        /// Output DuckDB file path
        #[arg(short, long, default_value = "bidslake.duckdb")]
        output: String,

        /// Dataset ID (optional, inferred from dataset_description.json if not provided)
        #[arg(short, long)]
        dataset_id: Option<String>,

        /// Use anonymous access for S3 (no AWS credentials required)
        #[arg(long)]
        no_sign_request: bool,

        /// Path to BIDS schema JSON file (optional, uses embedded schema if not provided)
        #[arg(long)]
        schema_path: Option<PathBuf>,

        /// Schema overlay to merge onto the base schema, so bidslake can index
        /// "bidsish" derivative outputs (e.g. fMRIPrep). Either a bundled pipeline
        /// name (fmriprep, mriqc, qsiprep) or a path to an overlay JSON file.
        /// Repeatable; applied left to right.
        #[arg(long = "overlay")]
        overlay: Vec<String>,

        /// Layout adapter for indexing a standardized *non-BIDS* dataset (e.g. FreeSurfer
        /// `recon-all` derivatives, whose files have no BIDS entities). Either a bundled
        /// name (`freesurfer`, `freesurfer-long`) or a path to an adapter JSON file.
        /// Repeatable. Distinct from `--overlay`, which extends the schema for
        /// BIDS-*named* derivatives.
        #[arg(long = "adapter")]
        adapter: Vec<String>,

        /// Walk and route the dataset without writing a database, then report how
        /// each tabular file would be handled (ingested vs skipped). Use it to check
        /// whether an overlay captures the files you expect.
        #[arg(long)]
        dry_run: bool,

        /// Ignore the dataset's `.bidsignore` and walk every file. Pipelines like
        /// fMRIPrep hide their non-standard outputs (e.g. `*_timeseries.tsv`,
        /// `*_xfm.*`) in `.bidsignore`; pass this alongside `--overlay` to index them.
        #[arg(long)]
        no_bidsignore: bool,
    },

    /// Print the DuckDB schema bidslake would build from the BIDS schema (plus any
    /// overlays), or — with `--diff` — only what the overlays add. Writes no database;
    /// for previewing how an overlay changes the catalog.
    Schema {
        /// Schema overlay to merge (bundled pipeline name or overlay JSON path).
        /// Repeatable; applied left to right.
        #[arg(long = "overlay")]
        overlay: Vec<String>,

        /// Layout adapter whose tables to include (bundled name or adapter JSON path).
        /// Repeatable.
        #[arg(long = "adapter")]
        adapter: Vec<String>,

        /// Show only the tables and columns the overlays add versus the base schema.
        #[arg(long)]
        diff: bool,
    },

    /// (Managed mode, not yet implemented) Verify integrity of managed files:
    /// check that every file the catalog records is present and uncorrupted.
    Verify {
        /// bidslake DuckDB database
        #[arg(short, long, default_value = "bidslake.duckdb")]
        database: String,
    },

    /// (Managed mode, not yet implemented) Change the on-disk storage format of
    /// managed files (e.g. recompress .nii.gz -> .nii.zst), updating catalog
    /// storage pointers.
    Transcode {
        /// bidslake DuckDB database
        #[arg(short, long, default_value = "bidslake.duckdb")]
        database: String,

        /// Target storage format (e.g. "zst")
        #[arg(long)]
        to: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Index {
            input,
            output,
            dataset_id,
            no_sign_request,
            schema_path,
            overlay,
            adapter,
            dry_run,
            no_bidsignore,
        } => {
            let schema_path_str = schema_path
                .as_deref()
                .map(|p| {
                    p.to_str()
                        .ok_or_else(|| anyhow::anyhow!("--schema path is not valid UTF-8: {p:?}"))
                })
                .transpose()?;
            // A dataset can self-describe with a `.bidslake/overlay.json` at its root
            // (local datasets); it is applied first (lowest precedence) so explicit
            // `--overlay` flags still take effect. Additive merge makes the order moot
            // for the result, but it keeps provenance in dataset-then-flag order.
            let overlay = discover_embedded_overlay(&input, overlay);
            let mut overlays = resolve_overlays(&overlay)?;
            // `--adapter <name>` contributes an overlay (tables), a term map (projection),
            // and an ingestion fragment (read/catalog/ignore policy).
            let bundle = resolve_adapters(&adapter)?;
            overlays.extend(bundle.overlays);
            let schema = Schema::load_full(schema_path_str, &overlays, bundle.ingestion)?;
            run_indexer(
                input,
                output,
                dataset_id,
                no_sign_request,
                schema,
                bundle.term_maps,
                bundle.term_map_provenance,
                bundle.ingestion_provenance,
                dry_run,
                !no_bidsignore,
            )
            .await
        }
        Commands::Schema {
            overlay,
            adapter,
            diff,
        } => {
            let mut overlays = resolve_overlays(&overlay)?;
            let bundle = resolve_adapters(&adapter)?;
            overlays.extend(bundle.overlays);
            let augmented = Schema::load_full(None, &overlays, bundle.ingestion)?;
            if diff {
                // Adapter overlays add tables/columns, so a diff against a base *without*
                // them shows the adapter's additions.
                print_schema_diff(&Schema::load(None)?, &augmented)
            } else {
                print_schema(&augmented)
            }
        }
        Commands::Verify { database } => {
            anyhow::bail!(
                "`verify` is not yet implemented (managed mode). \
                 See the README on managed mode. (database: {database})"
            )
        }
        Commands::Transcode { database, to } => {
            anyhow::bail!(
                "`transcode` is not yet implemented (managed mode). \
                 See the README on managed mode. (database: {database}, to: {to})"
            )
        }
    }
}

/// Prepend a dataset-embedded overlay (`<input>/.bidslake/overlay.json`) to the
/// `--overlay` list when the input is a local directory carrying one, so a derivative
/// dataset can self-describe with no flags. Remote (`s3://`) inputs are not scanned.
fn discover_embedded_overlay(input: &str, mut overlay: Vec<String>) -> Vec<String> {
    if input.starts_with("s3://") {
        return overlay;
    }
    let embedded = std::path::Path::new(input)
        .join(".bidslake")
        .join("overlay.json");
    if embedded.is_file() {
        println!("Using dataset-embedded overlay: {}", embedded.display());
        overlay.insert(0, embedded.to_string_lossy().into_owned());
    }
    overlay
}

/// Resolve each `--overlay` argument to an [`AppliedOverlay`] (source label + parsed
/// content). An argument that names a bundled pipeline (`fmriprep`, `mriqc`,
/// `qsiprep`) resolves to the embedded overlay; otherwise it is treated as a path to
/// an overlay JSON file.
fn resolve_overlays(specs: &[String]) -> Result<Vec<schema::AppliedOverlay>> {
    use anyhow::Context as _;
    specs
        .iter()
        .map(|spec| {
            let content = if let Some(bundled) = bids_schema::overlay::bundled_overlay(spec) {
                bundled
            } else {
                bids_schema::overlay::load_overlay(std::path::Path::new(spec)).with_context(
                    || {
                        format!(
                            "loading overlay {spec:?} (not a bundled pipeline name; bundled are {:?})",
                            bids_schema::overlay::BUNDLED_OVERLAY_NAMES
                        )
                    },
                )?
            };
            Ok(schema::AppliedOverlay {
                source: spec.clone(),
                content,
            })
        })
        .collect()
}

/// A resolved adapter bundle: `--adapter <name>` resolves to a trio of standard artifacts —
/// a BIDS overlay (tables), a BEP-043 term map (path→concept projection), and a bidslake
/// ingestion fragment (read/catalog/ignore policy) — plus their provenance for the
/// self-describing stamps.
struct AdapterBundle {
    overlays: Vec<schema::AppliedOverlay>,
    term_maps: Vec<bids_schema::term_map::TermMap>,
    ingestion: schema::Ingestion,
    term_map_provenance: Vec<(String, serde_json::Value)>,
    ingestion_provenance: Vec<(String, serde_json::Value)>,
}

/// Resolve each `--adapter` bundled name (e.g. `freesurfer`) into its overlay + term-map +
/// ingestion trio, validating each artifact against its metaschema.
fn resolve_adapters(names: &[String]) -> Result<AdapterBundle> {
    use anyhow::Context as _;
    let mut bundle = AdapterBundle {
        overlays: Vec::new(),
        term_maps: Vec::new(),
        ingestion: schema::Ingestion::default(),
        term_map_provenance: Vec::new(),
        ingestion_provenance: Vec::new(),
    };
    // Every ingest starts from the base BIDS ingestion policy (e.g. `events` ordering),
    // then layers on each adapter's fragment.
    let mut ingestion_sources: Vec<String> = vec![
        bids_schema::bundled_ingestion_source("base")
            .expect("base ingestion")
            .to_string(),
    ];
    for name in names {
        let overlay = bids_schema::overlay::bundled_overlay(name).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown adapter {name:?}; bundled adapters are {:?}",
                bids_schema::term_map::BUNDLED_TERM_MAP_NAMES
            )
        })?;
        bundle.overlays.push(schema::AppliedOverlay {
            source: name.clone(),
            content: overlay,
        });

        let tm_src = bids_schema::term_map::bundled_term_map_source(name)
            .ok_or_else(|| anyhow::anyhow!("adapter {name:?} has no bundled term map"))?;
        bundle.term_maps.push(
            bids_schema::term_map::bundled_term_map(name)
                .with_context(|| format!("compiling term map {name:?}"))?,
        );
        bundle
            .term_map_provenance
            .push((name.clone(), serde_json::from_str(tm_src)?));

        let ing_src = bids_schema::bundled_ingestion_source(name)
            .ok_or_else(|| anyhow::anyhow!("adapter {name:?} has no bundled ingestion schema"))?;
        ingestion_sources.push(ing_src.to_string());
        bundle
            .ingestion_provenance
            .push((name.clone(), serde_json::from_str(ing_src)?));
    }
    bundle.ingestion = schema::Ingestion::from_sources(
        &ingestion_sources
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    )?;
    Ok(bundle)
}

#[allow(clippy::too_many_arguments)]
async fn run_indexer(
    input: String,
    output: String,
    dataset_id: Option<String>,
    no_sign_request: bool,
    schema: Schema,
    term_maps: Vec<bids_schema::term_map::TermMap>,
    term_map_provenance: Vec<(String, serde_json::Value)>,
    ingestion_provenance: Vec<(String, serde_json::Value)>,
    dry_run: bool,
    apply_bidsignore: bool,
) -> Result<()> {
    println!("Input BIDS location: {}", input);
    // A dry run parses into a throwaway in-memory database and reports routing rather
    // than writing anything to disk.
    let db_path = if dry_run { ":memory:" } else { &output };
    if dry_run {
        println!("Dry run: routing only, no database written");
    } else {
        println!("Output DuckDB file: {}", output);
    }

    let db = BidsDb::new(db_path)?;
    db.create_tables(&schema)?;
    db.stamp_term_maps(&term_map_provenance)?;
    db.stamp_ingestion(&ingestion_provenance)?;

    // Region/anonymous settings for httpfs, when the input is S3.
    let mut s3_httpfs: Option<(String, bool)> = None;
    let fs: Box<dyn BidsFileSystem> = if input.starts_with("s3://") {
        {
            // Parse bucket and prefix from s3://bucket/prefix
            let parts: Vec<&str> = input.trim_start_matches("s3://").splitn(2, '/').collect();
            let bucket = parts[0];
            let prefix = if parts.len() > 1 { parts[1] } else { "" };

            println!("Using S3 backend: bucket={}, prefix={}", bucket, prefix);
            let signing = if no_sign_request {
                s3::SigningMode::Anonymous
            } else {
                s3::SigningMode::Signed
            };
            let client = s3::S3Client::new(bucket, prefix, signing).await?;
            s3_httpfs = Some((client.region().to_string(), client.anonymous()));
            Box::new(client)
        }
    } else {
        println!("Using local filesystem backend");
        Box::new(LocalFileSystem::new(PathBuf::from(&input)))
    };

    // Teach DuckDB to read `s3://` TSVs directly (both the write connection and the
    // parser's read-preflight connection run `read_csv` over them).
    if let Some((region, anonymous)) = &s3_httpfs {
        s3::configure_httpfs(&db.conn, region, *anonymous)?;
    }

    let s3_httpfs_cfg = s3_httpfs
        .as_ref()
        .map(|(region, anonymous)| bids::S3Httpfs {
            region: region.clone(),
            anonymous: *anonymous,
        });
    let mut parser: BidsParser =
        BidsParser::new(fs, dataset_id, schema, s3_httpfs_cfg, apply_bidsignore)
            .with_term_maps(term_maps);

    // Wrap the whole ingest in one transaction. DuckDB otherwise autocommits
    // every statement, fsyncing per row on a file-backed database — the single
    // biggest cost for real (file) ingests. Dropping `txn` without committing
    // (i.e. on an error `?` below) rolls the whole ingest back.
    let txn = db.conn.unchecked_transaction()?;
    parser.parse(&db).await?;
    txn.commit()?;

    if dry_run {
        print_routing_summary(&db)?;
    } else {
        println!("Conversion complete!");
    }

    Ok(())
}

/// Report, for a dry run, how each tabular file was routed — a count by disposition
/// plus the list of `skipped` files (the ones an overlay could bring in).
fn print_routing_summary(db: &BidsDb) -> Result<()> {
    println!("\n=== dry run: tabular routing ===");
    let mut stmt = db
        .conn
        .prepare("SELECT status, count(*) FROM tabular_files GROUP BY status ORDER BY status")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    for row in rows {
        let (status, n) = row?;
        println!("  {status}: {n}");
    }

    let mut stmt = db.conn.prepare(
        "SELECT file_path FROM tabular_files WHERE status = 'skipped' ORDER BY file_path",
    )?;
    let skipped: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    if skipped.is_empty() {
        println!("\nNo skipped tabular files.");
    } else {
        println!("\nSkipped tabular files (an overlay could capture these):");
        for f in &skipped {
            println!("  {f}");
        }
    }
    Ok(())
}

/// Every `main`-schema table's `(column, duckdb_type)` in ordinal order, read from a
/// throwaway in-memory database built via the real `create_tables` path (so it
/// includes the generated virtual columns).
fn introspect_schema(schema: &Schema) -> Result<BTreeMap<String, Vec<(String, String)>>> {
    let db = BidsDb::new(":memory:")?;
    db.create_tables(schema)?;
    let mut out: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut stmt = db.conn.prepare(
        "SELECT table_name, column_name, data_type FROM information_schema.columns \
         WHERE table_schema = 'main' ORDER BY table_name, ordinal_position",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (table, col, ty) = row?;
        out.entry(table).or_default().push((col, ty));
    }
    Ok(out)
}

/// Print the full effective schema: every table and its columns.
fn print_schema(schema: &Schema) -> Result<()> {
    for (table, columns) in &introspect_schema(schema)? {
        println!("{table}");
        for (col, ty) in columns {
            println!("  {col} {ty}");
        }
        println!();
    }
    Ok(())
}

/// Print only what the overlays add versus the base schema: new tables (with their
/// columns), and — grouped so a uniform effect like a new entity's generated columns
/// is reported once — the new columns on existing tables. The internal `bidslake_*`
/// provenance tables are omitted (they are not part of the augmented catalog surface).
fn print_schema_diff(base: &Schema, augmented: &Schema) -> Result<()> {
    let base_cols = introspect_schema(base)?;
    let aug_cols = introspect_schema(augmented)?;
    let is_internal = |table: &str| table.starts_with("bidslake_");

    let mut new_tables: Vec<(&String, &Vec<(String, String)>)> = Vec::new();
    // Group existing tables by the exact set of columns they gained, so "+from/to/mode
    // on every file-based table" prints once instead of dozens of times.
    let mut additions: BTreeMap<Vec<(String, String)>, Vec<String>> = BTreeMap::new();

    for (table, columns) in &aug_cols {
        if is_internal(table) {
            continue;
        }
        match base_cols.get(table) {
            None => new_tables.push((table, columns)),
            Some(base_table) => {
                let existing: HashSet<&str> = base_table.iter().map(|(c, _)| c.as_str()).collect();
                let added: Vec<(String, String)> = columns
                    .iter()
                    .filter(|(c, _)| !existing.contains(c.as_str()))
                    .cloned()
                    .collect();
                if !added.is_empty() {
                    additions.entry(added).or_default().push(table.clone());
                }
            }
        }
    }

    if new_tables.is_empty() && additions.is_empty() {
        println!("No schema changes (no overlays supplied, or they add nothing new).");
        return Ok(());
    }

    for (table, columns) in &new_tables {
        println!("+ new table {table}");
        for (col, ty) in *columns {
            println!("    {col} {ty}");
        }
        println!();
    }
    for (added, mut tables) in additions {
        tables.sort();
        let cols = added
            .iter()
            .map(|(c, ty)| format!("{c} {ty}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("+ columns [{cols}]");
        println!("    on {} tables: {}", tables.len(), tables.join(", "));
        println!();
    }
    Ok(())
}
