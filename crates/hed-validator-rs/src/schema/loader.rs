//! Multi-schema version loader, ported from hed-python's `hed_schema_io.py`. A version
//! spec list like `["8.3.0", "sc:score_1.0.0"]` becomes a `SchemaCollection`: specs group
//! by namespace prefix; within a namespace the first spec loads as the base and later ones
//! merge into it (subject to `withStandard` compatibility); a partnered library loaded
//! standalone first loads and copies its partner standard schema.

use super::collection::SchemaCollection;
use super::model::Schema;
use super::wiki_parser::{SchemaLoadError, load_wiki_string};
use crate::errors::codes;
use directories::ProjectDirs;
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

static SEMVER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d+\.\d+\.\d+$").expect("static semver regex is valid"));

/// Successful per-namespace merged loads, keyed by the comma-joined spec string. The
/// conformance suite loads the same merge groups repeatedly; failures are not cached (they
/// re-derive quickly and keep error paths simple).
static MERGED_CACHE: LazyLock<Mutex<HashMap<String, Schema>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Loads the schemas named by `specs` (each `[namespace:]([library_]version)`) into a
/// collection. `schema_dir` points at a checkout of hed-standard/hed-schemas (e.g. the
/// tests/hed-schemas submodule); missing there, versions fall back to the local cache
/// directory and finally a network fetch.
pub fn load_schema_version(
    specs: &[String],
    schema_dir: Option<&Path>,
) -> Result<SchemaCollection, SchemaLoadError> {
    // Group specs by namespace, preserving order within each.
    let mut order: Vec<String> = Vec::new();
    let mut by_namespace: HashMap<String, Vec<String>> = HashMap::new();
    for spec in specs {
        let (namespace, version) = match spec.split_once(':') {
            Some((ns, rest)) => (format!("{}:", ns), rest.to_string()),
            None => (String::new(), spec.clone()),
        };
        if namespace.len() > 1 {
            let bare = &namespace[..namespace.len() - 1];
            if bare.is_empty() || !bare.chars().all(|c| c.is_alphabetic()) {
                return Err(SchemaLoadError::single(
                    codes::SCHEMA_LIBRARY_INVALID,
                    &format!(
                        "Schema namespace prefix '{}' can only contain alpha characters",
                        namespace
                    ),
                ));
            }
        }
        let entry = by_namespace.entry(namespace.clone()).or_insert_with(|| {
            order.push(namespace.clone());
            Vec::new()
        });
        if entry.contains(&version) {
            return Err(SchemaLoadError::single(
                codes::SCHEMA_LIBRARY_INVALID,
                &format!(
                    "Attempting to load same library '{}' twice: {:?}",
                    version, entry
                ),
            ));
        }
        entry.push(version);
    }

    let mut schemas = Vec::new();
    for namespace in order {
        let versions = &by_namespace[&namespace];
        let mut schema = load_merged(versions, schema_dir)?;
        schema.namespace = namespace;
        schemas.push(schema);
    }
    SchemaCollection::from_schemas(schemas)
}

/// Loads `versions[0]` then merges each later version into it (hed-python's
/// `_load_schema_version`), collecting post-merge duplicate names as SCHEMA_DUPLICATE_NAMES.
fn load_merged(versions: &[String], schema_dir: Option<&Path>) -> Result<Schema, SchemaLoadError> {
    let cache_key = versions.join(",");
    if let Some(cached) = MERGED_CACHE.lock().unwrap().get(&cache_key) {
        return Ok(cached.clone());
    }

    let mut schema = load_single(&versions[0], schema_dir)?;
    for version in &versions[1..] {
        let text = read_schema_source(version, schema_dir)?;
        schema = load_wiki_string(&text, Some(schema), &partner_loader(schema_dir))?;
        if schema.has_duplicates() {
            let issues = schema
                .duplicate_names
                .values()
                .flatten()
                .map(|dup| {
                    crate::errors::HedError::error(
                        codes::SCHEMA_DUPLICATE_NAMES,
                        &format!(
                            "Duplicate tag {} found when merging schemas: {:?}",
                            dup.name, versions
                        ),
                        Some(dup.name.clone()),
                    )
                })
                .collect();
            return Err(SchemaLoadError::from_issues(issues));
        }
    }

    MERGED_CACHE
        .lock()
        .unwrap()
        .insert(cache_key, schema.clone());
    Ok(schema)
}

/// The `withStandard` partner loader handed to the wiki parser.
fn partner_loader(
    schema_dir: Option<&Path>,
) -> impl Fn(&str) -> Result<Schema, SchemaLoadError> + '_ {
    move |version: &str| load_single(version, schema_dir)
}

/// Loads one `[library_]version` spec as a standalone schema (partnered libraries pull in
/// and copy their base standard schema via the wiki parser).
fn load_single(spec: &str, schema_dir: Option<&Path>) -> Result<Schema, SchemaLoadError> {
    let (library, version) = split_library_version(spec);
    if !SEMVER_RE.is_match(version) {
        return Err(SchemaLoadError::single(
            codes::SCHEMA_VERSION_INVALID,
            &format!("Invalid version format '{}'", version),
        ));
    }

    // Fast path: the embedded standard schema.
    if library.is_empty() && version == "8.4.0" {
        return Schema::load_standard("8.4.0")
            .map_err(|e| SchemaLoadError::single(codes::SCHEMA_LOAD_FAILED, &e.to_string()));
    }

    let text = read_schema_source(spec, schema_dir)?;
    load_wiki_string(&text, None, &partner_loader(schema_dir))
}

fn split_library_version(spec: &str) -> (&str, &str) {
    match spec.split_once('_') {
        Some((library, version)) => (library, version),
        None => ("", spec),
    }
}

fn wiki_relative_path(spec: &str) -> Result<PathBuf, SchemaLoadError> {
    let (library, version) = split_library_version(spec);
    if !SEMVER_RE.is_match(version) {
        return Err(SchemaLoadError::single(
            codes::SCHEMA_VERSION_INVALID,
            &format!("Invalid version format '{}'", version),
        ));
    }
    Ok(if library.is_empty() {
        PathBuf::from(format!("standard_schema/hedwiki/HED{}.mediawiki", version))
    } else {
        PathBuf::from(format!(
            "library_schemas/{}/hedwiki/HED_{}_{}.mediawiki",
            library, library, version
        ))
    })
}

/// Finds the mediawiki source for a spec: the hed-schemas checkout first, then the local
/// cache, then a network fetch (cached for next time).
fn read_schema_source(spec: &str, schema_dir: Option<&Path>) -> Result<String, SchemaLoadError> {
    let relative = wiki_relative_path(spec)?;

    if let Some(dir) = schema_dir {
        let path = dir.join(&relative);
        if path.exists() {
            return std::fs::read_to_string(&path).map_err(|e| {
                SchemaLoadError::single(
                    codes::SCHEMA_LOAD_FAILED,
                    &format!("Failed to read {:?}: {}", path, e),
                )
            });
        }
    }

    let cache_dir = ProjectDirs::from("", "HED", "ValidatorRs")
        .map(|proj_dirs| proj_dirs.cache_dir().to_path_buf())
        .unwrap_or_else(std::env::temp_dir);
    let file_name = relative
        .file_name()
        .expect("wiki path always has a file name")
        .to_owned();
    let cache_path = cache_dir.join(&file_name);
    if cache_path.exists()
        && let Ok(text) = std::fs::read_to_string(&cache_path)
    {
        return Ok(text);
    }

    let url = format!(
        "https://raw.githubusercontent.com/hed-standard/hed-schemas/main/{}",
        relative.to_string_lossy()
    );
    let resp = reqwest::blocking::get(&url).map_err(|e| {
        SchemaLoadError::single(
            codes::SCHEMA_LOAD_FAILED,
            &format!(
                "Schema '{}' not found locally and fetching failed: {}",
                spec, e
            ),
        )
    })?;
    if !resp.status().is_success() {
        return Err(SchemaLoadError::single(
            codes::SCHEMA_LOAD_FAILED,
            &format!(
                "Schema '{}' not found: HTTP {} for {}",
                spec,
                resp.status(),
                url
            ),
        ));
    }
    let text = resp.text().map_err(|e| {
        SchemaLoadError::single(
            codes::SCHEMA_LOAD_FAILED,
            &format!("Failed to read response for {}: {}", url, e),
        )
    })?;
    let _ = std::fs::create_dir_all(&cache_dir);
    let _ = std::fs::write(&cache_path, &text);
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> Option<&'static Path> {
        Some(Path::new("tests/hed-schemas"))
    }

    fn specs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn loads_two_namespace_group() {
        // ["8.3.0", "sc:score_1.0.0"] — two namespaces, no merging.
        let coll =
            load_schema_version(&specs(&["8.3.0", "sc:score_1.0.0"]), dir()).expect("should load");
        assert!(coll.schema_for("").is_some());
        assert!(coll.schema_for("sc:").is_some());
        // Sleep-modulator is a score tag; Red is a standard tag.
        assert!(matches!(
            coll.resolve_tag("sc:Sleep-modulator"),
            crate::schema::TagResolution::Full(_)
        ));
        assert!(matches!(
            coll.resolve_tag("Red"),
            crate::schema::TagResolution::Full(_)
        ));
        assert!(matches!(
            coll.resolve_tag("ts:Red"),
            crate::schema::TagResolution::UnknownNamespace
        ));
    }

    #[test]
    fn loads_prefixed_standard() {
        let coll = load_schema_version(&specs(&["ts:8.3.0"]), dir()).expect("should load");
        assert!(coll.schema_for("ts:").is_some());
        assert!(coll.schema_for("").is_none());
        assert!(matches!(
            coll.resolve_tag("ts:Creation-date/2009-04-09T12:04:14"),
            crate::schema::TagResolution::Value { .. }
        ));
        assert!(matches!(
            coll.resolve_tag("Creation-date/2009-04-09T12:04:14"),
            crate::schema::TagResolution::UnknownNamespace
        ));
    }

    #[test]
    fn standard_plus_library_in_one_namespace_fails() {
        // Mirrors hed-python: the base (8.1.0) has no withStandard, so merging fails.
        let err = load_schema_version(&specs(&["8.1.0", "testlib_2.0.0"]), dir()).unwrap_err();
        assert_eq!(err.code, codes::SCHEMA_LOAD_FAILED);

        let err = load_schema_version(
            &specs(&["8.2.0", "testlib_2.0.0", "testlib_3.0.0", "sc:8.1.0"]),
            dir(),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::SCHEMA_LOAD_FAILED);
    }

    #[test]
    fn incompatible_partners_fail() {
        // score_2.0.0 partners 8.3.0; lang_1.1.0 partners 8.4.0.
        let err = load_schema_version(&specs(&["score_2.0.0", "lang_1.1.0"]), dir()).unwrap_err();
        assert_eq!(err.code, codes::SCHEMA_LOAD_FAILED);
    }
}
