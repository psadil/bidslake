//! BIDS Validator CLI — validate a BIDS dataset from the command line.

use bids_validator_rs::config::ValidatorConfig;
use bids_validator_rs::filetree::read_file_tree;
use bids_validator_rs::schema::BidsSchema;
use clap::Parser;
use std::path::PathBuf;
use std::process;
// Process each file concurrently

/// A pure-Rust BIDS (Brain Imaging Data Structure) validator.
#[derive(Parser, Debug)]
#[command(name = "bids-validator", version, about)]
struct Cli {
    /// Path to the BIDS dataset to validate.
    dataset: PathBuf,

    /// Output results as JSON.
    #[arg(long)]
    json: bool,

    /// Show verbose output.
    #[arg(short, long)]
    verbose: bool,

    /// Ignore warnings, only report errors.
    #[arg(long)]
    ignore_warnings: bool,

    /// Schema to validate against: a version (`vX.Y.Z`, `stable`, `latest`) fetched from the
    /// BIDS specification site, an `http(s)://` URL, or a path to a local schema JSON file.
    /// If not provided, uses the bundled schema. Overridden by the `BIDS_SCHEMA` env var.
    #[arg(long)]
    schema: Option<String>,

    /// Skip NIfTI header checks (faster, avoids reading large files).
    #[arg(long)]
    skip_nifti: bool,

    /// Path to a custom config JSON file (e.g. .bids-validator-config.json).
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Validate that the dataset path exists
    if !cli.dataset.exists() {
        eprintln!(
            "Error: dataset path '{}' does not exist",
            cli.dataset.display()
        );
        process::exit(1);
    }
    if !cli.dataset.is_dir() {
        eprintln!("Error: '{}' is not a directory", cli.dataset.display());
        process::exit(1);
    }

    // Load schema, mirroring the TS validator's resolution (version/URL/path, BIDS_SCHEMA env).
    if cli.verbose {
        match &cli.schema {
            Some(s) => eprintln!("Resolving schema: {}", s),
            None => eprintln!("Using bundled BIDS schema"),
        }
    }
    let schema = match BidsSchema::resolve(cli.schema.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error loading schema: {}", e);
            process::exit(1);
        }
    };

    if cli.verbose {
        eprintln!(
            "BIDS schema version: {} (spec {})",
            schema.schema_version, schema.bids_version
        );
    }

    // Read file tree
    if cli.verbose {
        eprintln!("Reading file tree from: {}", cli.dataset.display());
    }
    let pseudo_exts = schema.pseudo_file_extensions();
    let tree = match read_file_tree(&cli.dataset, &pseudo_exts, true) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error reading dataset: {}", e);
            process::exit(1);
        }
    };

    if cli.verbose {
        eprintln!("Found {} files", tree.walk_files().count());
    }

    // Load configuration if present
    let config_path = cli
        .config
        .unwrap_or_else(|| cli.dataset.join(".bids-validator-config.json"));
    let config = if config_path.exists() {
        if cli.verbose {
            eprintln!("Loading config from: {}", config_path.display());
        }
        match ValidatorConfig::from_file(&config_path) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("Error loading config: {}", e);
                process::exit(1);
            }
        }
    } else {
        None
    };

    let issues = match bids_validator_rs::validator::validate(
        &cli.dataset,
        &schema,
        config.as_ref(),
    )
    .await
    {
        Ok(issues) => issues,
        Err(e) => {
            eprintln!("{}", e);
            process::exit(1);
        }
    };

    // Output results
    if cli.json {
        let output = issues.to_json();
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
    } else {
        // Human-readable output
        let errors = issues.errors();
        let warnings = issues.warnings();

        if !errors.is_empty() {
            println!("\n\x1b[1;31m=== Errors ({}) ===\x1b[0m\n", errors.len());
            for issue in &errors {
                println!("  \x1b[31m✗\x1b[0m [{}] {}", issue.code, issue.message);
                println!("    \x1b[90m{}\x1b[0m", issue.location);
                if let Some(ref sub) = issue.sub_message {
                    println!("    \x1b[90m{}\x1b[0m", sub);
                }
            }
        }

        if !cli.ignore_warnings && !warnings.is_empty() {
            println!("\n\x1b[1;33m=== Warnings ({}) ===\x1b[0m\n", warnings.len());
            for issue in &warnings {
                println!("  \x1b[33m⚠\x1b[0m [{}] {}", issue.code, issue.message);
                println!("    \x1b[90m{}\x1b[0m", issue.location);
                if let Some(ref sub) = issue.sub_message {
                    println!("    \x1b[90m{}\x1b[0m", sub);
                }
            }
        }

        println!();
        if issues.has_errors() {
            println!("\x1b[1;31m{}\x1b[0m", issues.format_summary());
        } else if !warnings.is_empty() {
            println!("\x1b[1;33m{}\x1b[0m", issues.format_summary());
        } else {
            println!("\x1b[1;32mValidation complete: no issues found! ✓\x1b[0m");
        }
    }

    // Exit with non-zero status if there are errors
    if issues.has_errors() {
        process::exit(1);
    }
}
