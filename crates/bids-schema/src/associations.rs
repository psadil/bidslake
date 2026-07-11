//! Schema-driven association resolution.
//!
//! [`resolve_associations`] evaluates the schema's `meta.associations` entries for one file
//! against an in-memory [`FileTree`] and returns the associated files it finds. It is **pure and
//! synchronous** — selector evaluation plus in-memory tree search; it reads no file content. The
//! caller (the validator's typed `BidsAssociations`, or bidslake's `file_associations` rows)
//! builds whatever it needs on top of the returned hits.
//!
//! The schema is a **parameter** (`meta_associations`), so bidslake and the validator can pass
//! different schema.json — though the workspace now unifies on one ([`crate::SCHEMA_JSON`]).

use crate::expression::{EvalContext, do_selectors_select};
use bids_core::filetree::{BidsFile, FileTree};
use bids_core::inheritance::{find_all_associated_files, find_associated_file};
use serde_json::Value;

/// One resolved association hit: which `meta.associations` entry matched, the file it points at,
/// and provenance flags. No file content is read.
#[derive(Debug, Clone)]
pub struct ResolvedAssociation {
    /// The schema key, e.g. `"events"`, `"bval"`, `"coordsystems"`.
    pub name: String,
    /// The associated file discovered in the tree.
    pub target_file: BidsFile,
    /// The entry's `inherit` flag (whether the search ascended parent directories).
    pub inherit: bool,
    /// Whether the entry declares `target.entities` (a free-entity, multi-file association such
    /// as `coordsystems`/`electrodes`); such entries can yield more than one hit sharing `name`.
    pub multi: bool,
}

/// Resolve every `meta.associations` entry for `file` against `tree`.
///
/// `meta_associations` is the schema's `meta.associations` object; `file_ctx` is the file-level
/// selector context from [`crate::context::build_file_context`].
pub fn resolve_associations(
    meta_associations: &Value,
    file: &BidsFile,
    tree: &FileTree,
    file_ctx: &Value,
) -> Vec<ResolvedAssociation> {
    let null = Value::Null;
    let eval_ctx = EvalContext::file_only(file_ctx, &null);
    let mut out = Vec::new();

    let Some(assoc_obj) = meta_associations.as_object() else {
        return out;
    };

    for (assoc_name, assoc_def) in assoc_obj {
        let Some(selectors) = assoc_def.get("selectors").and_then(|s| s.as_array()) else {
            continue;
        };
        let selector_strings: Vec<String> = selectors
            .iter()
            .filter_map(|s| s.as_str().map(|s| s.to_string()))
            .collect();
        if !do_selectors_select(&Some(selector_strings), &eval_ctx) {
            continue;
        }
        let Some(target) = assoc_def.get("target") else {
            continue;
        };

        let inherit = assoc_def
            .get("inherit")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let target_suffix = target.get("suffix").and_then(|v| v.as_str());

        // Target extension can be a string or an array of strings.
        let mut target_extensions: Vec<&str> = Vec::new();
        if let Some(ext) = target.get("extension") {
            if let Some(s) = ext.as_str() {
                target_extensions.push(s);
            } else if let Some(arr) = ext.as_array() {
                for v in arr {
                    if let Some(s) = v.as_str() {
                        target_extensions.push(s);
                    }
                }
            }
        }

        // A `target.entities` list marks a free-entity (multi-file) association.
        let free_entities: Vec<&str> = target
            .get("entities")
            .and_then(|e| e.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        if !free_entities.is_empty() {
            for f in find_all_associated_files(
                file,
                tree,
                target_suffix,
                &target_extensions,
                &free_entities,
                inherit,
            ) {
                out.push(ResolvedAssociation {
                    name: assoc_name.clone(),
                    target_file: f,
                    inherit,
                    multi: true,
                });
            }
        } else if let Some(f) =
            find_associated_file(file, tree, target_suffix, &target_extensions, inherit)
        {
            out.push(ResolvedAssociation {
                name: assoc_name.clone(),
                target_file: f,
                inherit,
                multi: false,
            });
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn file(path: &str) -> BidsFile {
        let name = path.rsplit('/').next().unwrap().to_string();
        BidsFile {
            name,
            path: path.to_string(),
            absolute_path: PathBuf::new(),
            size: 0,
        }
    }

    fn dir(name: &str, path: &str, files: Vec<BidsFile>, directories: Vec<FileTree>) -> FileTree {
        FileTree {
            name: name.to_string(),
            path: path.to_string(),
            files,
            directories,
        }
    }

    /// A task BOLD run resolves its sibling `events.tsv` and `bval`/`bvec` resolve for a DWI —
    /// via the real schema's `meta.associations`, driven by the shared file-context builder.
    #[test]
    fn resolves_structural_associations_from_the_schema() {
        let schema: Value = serde_json::from_str(crate::SCHEMA_JSON).unwrap();
        let meta = &schema["meta"]["associations"];

        let bold = file("/sub-01/func/sub-01_task-rest_bold.nii.gz");
        let events = file("/sub-01/func/sub-01_task-rest_events.tsv");
        let dwi = file("/sub-01/dwi/sub-01_dwi.nii.gz");
        let bval = file("/sub-01/dwi/sub-01_dwi.bval");
        let bvec = file("/sub-01/dwi/sub-01_dwi.bvec");

        let tree = dir(
            "",
            "/",
            vec![],
            vec![dir(
                "sub-01",
                "/sub-01",
                vec![],
                vec![
                    dir("func", "/sub-01/func", vec![bold.clone(), events], vec![]),
                    dir("dwi", "/sub-01/dwi", vec![dwi.clone(), bval, bvec], vec![]),
                ],
            )],
        );

        let bold_ctx = crate::context::build_file_context(&bold, &schema);
        let bold_hits = resolve_associations(meta, &bold, &tree, &bold_ctx);
        assert!(
            bold_hits
                .iter()
                .any(|h| h.name == "events" && h.target_file.name == "sub-01_task-rest_events.tsv"),
            "task BOLD should resolve its events.tsv; got {:?}",
            bold_hits.iter().map(|h| &h.name).collect::<Vec<_>>()
        );

        let dwi_ctx = crate::context::build_file_context(&dwi, &schema);
        let dwi_hits = resolve_associations(meta, &dwi, &tree, &dwi_ctx);
        assert!(
            dwi_hits.iter().any(|h| h.name == "bval"),
            "DWI should resolve its bval; got {:?}",
            dwi_hits.iter().map(|h| &h.name).collect::<Vec<_>>()
        );
        assert!(
            dwi_hits.iter().any(|h| h.name == "bvec"),
            "DWI should resolve its bvec"
        );
    }
}
