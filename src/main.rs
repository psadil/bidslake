use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod bids;
mod db;
mod fs;
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
    /// additional datasets under management — see `docs/managed-mode.md`).
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
        } => {
            let schema_path_str = schema_path.as_ref().map(|p| p.to_str().unwrap());
            let schema = Schema::load(schema_path_str);
            run_indexer(input, output, dataset_id, no_sign_request, schema).await
        }
        Commands::Verify { database } => {
            anyhow::bail!(
                "`verify` is not yet implemented (managed mode). \
                 See docs/managed-mode.md. (database: {database})"
            )
        }
        Commands::Transcode { database, to } => {
            anyhow::bail!(
                "`transcode` is not yet implemented (managed mode). \
                 See docs/managed-mode.md. (database: {database}, to: {to})"
            )
        }
    }
}

async fn run_indexer(
    input: String,
    output: String,
    dataset_id: Option<String>,
    no_sign_request: bool,
    schema: Schema,
) -> Result<()> {
    println!("Input BIDS location: {}", input);
    println!("Output DuckDB file: {}", output);

    let db = BidsDb::new(&output)?;
    db.create_tables(&schema)?;

    let fs: Box<dyn BidsFileSystem> = if input.starts_with("s3://") {
        {
            // Parse bucket and prefix from s3://bucket/prefix
            let parts: Vec<&str> = input.trim_start_matches("s3://").splitn(2, '/').collect();
            let bucket = parts[0];
            let prefix = if parts.len() > 1 { parts[1] } else { "" };

            println!("Using S3 backend: bucket={}, prefix={}", bucket, prefix);
            Box::new(s3::S3Client::new(bucket, prefix, no_sign_request).await?)
        }
    } else {
        println!("Using local filesystem backend");
        Box::new(LocalFileSystem::new(PathBuf::from(&input)))
    };

    let mut parser: BidsParser = BidsParser::new(fs, dataset_id, schema);

    // Wrap the whole ingest in one transaction. DuckDB otherwise autocommits
    // every statement, fsyncing per row on a file-backed database — the single
    // biggest cost for real (file) ingests. Dropping `txn` without committing
    // (i.e. on an error `?` below) rolls the whole ingest back.
    let txn = db.conn.unchecked_transaction()?;
    parser.parse(&db).await?;
    txn.commit()?;

    println!("Conversion complete!");

    Ok(())
}
