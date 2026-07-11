use clap::Parser;
use hed_validator_rs::parser::parse_hed_string;
use hed_validator_rs::schema::{SchemaCollection, load_schema_version};
use hed_validator_rs::validator::{
    DefinitionMap, DefinitionSite, PlaceholderMode, ValidationContext, Validator,
    gather_definitions,
};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The HED string to validate
    #[arg(short, long)]
    string: Option<String>,

    /// Path to a HED string file (one per line)
    #[arg(short, long)]
    file: Option<String>,

    /// The HED schema version(s) to use: a single version ("8.4.0"), or a comma-separated
    /// merge-group spec with optional namespace prefixes ("8.3.0,sc:score_1.0.0").
    #[arg(long, default_value = "8.4.0")]
    schema_version: String,

    /// Optional path to a hed-standard/hed-schemas checkout for offline schema loading.
    #[arg(long)]
    schema_dir: Option<std::path::PathBuf>,
}

fn main() {
    let args = Args::parse();

    println!("Loading schema version {}...", args.schema_version);
    let specs: Vec<String> = args
        .schema_version
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let schemas = match load_schema_version(&specs, args.schema_dir.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to load schema: {}", e);
            std::process::exit(1);
        }
    };
    let validator = Validator::new(&schemas);

    if let Some(s) = args.string {
        validate_string(&s, &schemas, &validator);
    }

    if let Some(path) = args.file {
        if let Ok(lines) = std::fs::read_to_string(&path) {
            for (i, line) in lines.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                println!("Line {}: {}", i + 1, trimmed);
                validate_string(trimmed, &schemas, &validator);
            }
        } else {
            eprintln!("Failed to read file: {}", path);
        }
    }
}

fn validate_string(s: &str, schemas: &SchemaCollection, validator: &Validator) {
    match parse_hed_string(s) {
        Ok(nodes) => {
            let mut defs = DefinitionMap::new();
            let mut errors = Vec::new();
            gather_definitions(
                schemas,
                &nodes.nodes,
                DefinitionSite::PlainString,
                &mut defs,
                &mut errors,
            );
            let ctx = ValidationContext::new(
                PlaceholderMode::Forbidden,
                DefinitionSite::PlainString,
                &defs,
            );
            errors.extend(validator.validate(&nodes, &ctx));
            if errors.is_empty() {
                println!("  Valid!");
            } else {
                println!("  Found {} errors:", errors.len());
                for e in errors {
                    println!("  - {}", e);
                }
            }
        }
        Err(e) => {
            println!("  Parse error: {}", e);
        }
    }
}
