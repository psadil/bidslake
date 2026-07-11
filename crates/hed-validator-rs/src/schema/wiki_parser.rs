//! Parser for HED schema source files in mediawiki format, ported from hed-python's
//! `schema_io/wiki2schema.py`. The grammar is line-oriented: a header line, then a fixed
//! sequence of `'''Section'''` / `!# ...` markers, with tag nesting expressed by leading
//! `*` counts and per-line `{attributes}` / `[description]` blocks.

use super::model::*;
use crate::errors::{HedError, codes};
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

static TAG_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\*+|'{3})(.*?)('{3})?\s*([\[\{]|$)+").expect("static tag-name regex is valid")
});
static HEADER_ATTR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"([^ ,]+?)="(.*?)""#).expect("static header regex is valid"));
static NOWIKI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"</?nowiki>").expect("static nowiki regex is valid"));
static ATTRIBUTE_TOKEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z]+(=.+)?$").expect("static attribute regex is valid"));
static SEMVER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\d+\.\d+\.\d+(-[0-9A-Za-z.\-]+)?(\+[0-9A-Za-z.\-]+)?$")
        .expect("static semver regex is valid")
});

/// A failed schema load: one primary code plus every issue accumulated before giving up
/// (the wiki parser collects all line-level problems rather than failing on the first).
#[derive(Debug)]
pub struct SchemaLoadError {
    pub code: String,
    pub message: String,
    pub issues: Vec<HedError>,
}

impl SchemaLoadError {
    pub fn single(code: &str, message: &str) -> Self {
        SchemaLoadError {
            code: code.to_string(),
            message: message.to_string(),
            issues: vec![HedError::error(code, message, None)],
        }
    }

    pub fn from_issues(issues: Vec<HedError>) -> Self {
        let first = issues
            .first()
            .expect("from_issues requires at least one issue");
        SchemaLoadError {
            code: first.issue_code.clone(),
            message: first.message.clone(),
            issues: issues.clone(),
        }
    }
}

impl std::fmt::Display for SchemaLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}] {} ({} issue(s))",
            self.code,
            self.message,
            self.issues.len()
        )
    }
}

// Section identifiers, ordered as they must appear in the file.
const SEC_HEADER: u8 = 2;
const SEC_PROLOGUE: u8 = 3;
const SEC_SCHEMA: u8 = 4;
const SEC_END_SCHEMA: u8 = 5;
const SEC_UNIT_CLASSES: u8 = 6;
const SEC_UNIT_MODIFIERS: u8 = 7;
const SEC_VALUE_CLASSES: u8 = 8;
const SEC_ATTRIBUTES: u8 = 9;
const SEC_PROPERTIES: u8 = 10;
const SEC_EPILOGUE: u8 = 11;
const SEC_SOURCES: u8 = 12;
const SEC_PREFIXES: u8 = 13;
const SEC_EXTERNAL: u8 = 14;
const SEC_END_HED: u8 = 15;

const SECTION_MARKERS: [(u8, &str); 14] = [
    (SEC_HEADER, "HED"),
    (SEC_PROLOGUE, "'''Prologue'''"),
    (SEC_SCHEMA, "!# start schema"),
    (SEC_END_SCHEMA, "!# end schema"),
    (SEC_UNIT_CLASSES, "'''Unit classes'''"),
    (SEC_UNIT_MODIFIERS, "'''Unit modifiers'''"),
    (SEC_VALUE_CLASSES, "'''Value classes'''"),
    (SEC_ATTRIBUTES, "'''Schema attributes'''"),
    (SEC_PROPERTIES, "'''Properties'''"),
    (SEC_EPILOGUE, "'''Epilogue'''"),
    (SEC_SOURCES, "'''Sources'''"),
    (SEC_PREFIXES, "'''Prefixes'''"),
    (SEC_EXTERNAL, "'''External annotations'''"),
    (SEC_END_HED, "!# end hed"),
];

const REQUIRED_SECTIONS: [u8; 10] = [
    SEC_PROLOGUE,
    SEC_SCHEMA,
    SEC_END_SCHEMA,
    SEC_UNIT_CLASSES,
    SEC_UNIT_MODIFIERS,
    SEC_VALUE_CLASSES,
    SEC_ATTRIBUTES,
    SEC_PROPERTIES,
    SEC_EPILOGUE,
    SEC_END_HED,
];

const VALID_HEADER_ATTRIBUTES: [&str; 8] = [
    "version",
    "library",
    "withStandard",
    "unmerged",
    "xmlns",
    "xmlns:xsi",
    "xsi:noNamespaceSchemaLocation",
    "xsi:schemaLocation",
];

fn marker_for(section: u8) -> &'static str {
    SECTION_MARKERS
        .iter()
        .find(|(n, _)| *n == section)
        .map(|(_, m)| m)
        .unwrap()
}

/// Loads a mediawiki schema source. `append_into` merges the parsed content into an
/// existing schema (hed-python's "appending" mode used for same-namespace merge groups);
/// otherwise a fresh schema is built, auto-loading and copying the partner standard schema
/// first when the header declares `withStandard` + `unmerged`. `base_loader` supplies that
/// partner standard schema by version string.
pub fn load_wiki_string(
    text: &str,
    append_into: Option<Schema>,
    base_loader: &dyn Fn(&str) -> Result<Schema, SchemaLoadError>,
) -> Result<Schema, SchemaLoadError> {
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();

    let header = parse_header(lines.first().copied().unwrap_or(""))?;
    validate_header(&header)?;

    let with_standard = header.get("withStandard").cloned().unwrap_or_default();
    let library = header.get("library").cloned().unwrap_or_default();
    let version = header.get("version").cloned().unwrap_or_default();
    let unmerged = header.get("unmerged").cloned().unwrap_or_default();
    let file_merged = unmerged.is_empty();

    let appending = append_into.is_some();
    let mut loading_merged = true;

    let mut schema = match append_into {
        Some(mut base) => {
            if base.with_standard.is_empty() {
                return Err(SchemaLoadError::single(
                    codes::SCHEMA_LOAD_FAILED,
                    "Loading multiple normal schemas as a merged one with the same namespace. \
                     Ensure schemas have the withStandard header attribute set",
                ));
            }
            if with_standard != base.with_standard {
                return Err(SchemaLoadError::single(
                    codes::SCHEMA_LOAD_FAILED,
                    &format!(
                        "Merging schemas requires same withStandard value ({} != {}).",
                        with_standard, base.with_standard
                    ),
                ));
            }
            base.version = format!("{},{}", base.version, version);
            base.library = format!("{},{}", base.library, library);
            base
        }
        None => {
            if !with_standard.is_empty() && !file_merged {
                // Partnered, unmerged library loaded standalone: parse the library's own
                // lines into a copy of its partner standard schema.
                let mut base = base_loader(&with_standard).map_err(|e| {
                    SchemaLoadError::single(
                        codes::SCHEMA_LIBRARY_INVALID,
                        &format!(
                            "Cannot load withStandard schema '{}': {}",
                            with_standard, e.message
                        ),
                    )
                })?;
                loading_merged = false;
                base.version = version.clone();
                base.library = library.clone();
                base.with_standard = with_standard.clone();
                base.merged = file_merged;
                base.header_attributes = header.clone();
                base
            } else {
                Schema {
                    version: version.clone(),
                    library: library.clone(),
                    with_standard: with_standard.clone(),
                    merged: file_merged,
                    header_attributes: header.clone(),
                    ..Default::default()
                }
            }
        }
    };

    let ctx = WikiContext {
        appending,
        loading_merged,
        library,
        schema_merged: schema.merged,
        schema_with_standard: schema.with_standard.clone(),
    };

    parse_sections(&lines, &mut schema, &ctx)?;
    schema.finalize();
    Ok(schema)
}

struct WikiContext {
    appending: bool,
    loading_merged: bool,
    /// Library name of the file currently being parsed.
    library: String,
    /// Whether the target schema is a merged-format one.
    schema_merged: bool,
    schema_with_standard: String,
}

fn parse_header(first_line: &str) -> Result<HashMap<String, String>, SchemaLoadError> {
    if !first_line.starts_with("HED") {
        return Err(SchemaLoadError::single(
            codes::SCHEMA_HEADER_INVALID,
            &format!(
                "First line of file should be HED, instead found: {}",
                first_line
            ),
        ));
    }
    let rest = &first_line["HED".len()..];

    if rest.contains('=') {
        let mut attrs = HashMap::new();
        let mut last_end = 0usize;
        let mut unmatched: Vec<String> = Vec::new();
        for cap in HEADER_ATTR_RE.captures_iter(rest) {
            let whole = cap.get(0).unwrap();
            if whole.start() > last_end {
                unmatched.push(rest[last_end..whole.start()].to_string());
            }
            attrs.insert(cap[1].to_string(), cap[2].to_string());
            last_end = whole.end();
        }
        if last_end < rest.len() {
            unmatched.push(rest[last_end..].to_string());
        }
        for m in unmatched {
            let m = m.trim();
            if !m.is_empty() {
                return Err(SchemaLoadError::single(
                    codes::SCHEMA_HEADER_INVALID,
                    &format!("Header line has a malformed attribute {}", m),
                ));
            }
        }
        Ok(attrs)
    } else {
        // Legacy "key:value, key:value" header format.
        let mut attrs = HashMap::new();
        for pair in rest.split(',') {
            let Some(idx) = pair.find(':') else {
                return Err(SchemaLoadError::single(
                    codes::SCHEMA_HEADER_INVALID,
                    &format!("Found poorly matched key:value pair in header: {}", pair),
                ));
            };
            attrs.insert(
                pair[..idx].trim().to_string(),
                pair[idx + 1..].trim().to_string(),
            );
        }
        Ok(attrs)
    }
}

fn validate_header(attrs: &HashMap<String, String>) -> Result<(), SchemaLoadError> {
    if attrs.contains_key("withStandard") && !attrs.contains_key("library") {
        return Err(SchemaLoadError::single(
            codes::SCHEMA_LIBRARY_INVALID,
            "withStandard header attribute found, but no library attribute is present",
        ));
    }

    for (name, value) in attrs {
        match name.as_str() {
            "version" => {
                if !SEMVER_RE.is_match(value) {
                    return Err(SchemaLoadError::single(
                        codes::SCHEMA_VERSION_INVALID,
                        &format!("Invalid version '{}' in header", value),
                    ));
                }
            }
            "library" => {
                for (i, ch) in value.chars().enumerate() {
                    if !ch.is_alphabetic() {
                        return Err(SchemaLoadError::single(
                            codes::SCHEMA_LIBRARY_INVALID,
                            &format!(
                                "Non alpha character '{}' at position {} in library name '{}'",
                                ch, i, value
                            ),
                        ));
                    }
                    if ch.is_uppercase() {
                        return Err(SchemaLoadError::single(
                            codes::SCHEMA_LIBRARY_INVALID,
                            &format!(
                                "Non lowercase character '{}' at position {} in library name '{}'",
                                ch, i, value
                            ),
                        ));
                    }
                }
            }
            _ => {}
        }
        if !VALID_HEADER_ATTRIBUTES.contains(&name.as_str()) {
            return Err(SchemaLoadError::single(
                codes::SCHEMA_HEADER_INVALID,
                &format!("Unknown attribute {} found in header line", name),
            ));
        }
    }

    if !attrs.contains_key("version") {
        return Err(SchemaLoadError::single(
            codes::SCHEMA_VERSION_INVALID,
            "No version attribute found in header",
        ));
    }
    Ok(())
}

/// One accumulated line-level problem, mirroring hed-python's `_add_fatal_error`.
struct FatalErrors(Vec<HedError>);

impl FatalErrors {
    fn add(&mut self, line_number: usize, line: &str, message: &str, code: &str) {
        self.0.push(HedError::error(
            code,
            &format!("Line {}: {} ({})", line_number, message, line.trim()),
            Some(line.trim().to_string()),
        ));
    }
}

type SectionLines = Vec<(usize, String)>;

fn split_into_sections(lines: &[&str]) -> Result<HashMap<u8, SectionLines>, SchemaLoadError> {
    let mut by_section: HashMap<u8, SectionLines> = HashMap::new();
    by_section.insert(SEC_HEADER, Vec::new());
    let mut current: u8 = SEC_HEADER;
    let mut nowiki_issues: Vec<HedError> = Vec::new();

    for (idx, raw_line) in lines.iter().enumerate() {
        if idx == 0 {
            continue; // header line handled separately
        }
        let line_number = idx + 1;
        let stripped = raw_line.trim();

        // Section detection.
        if !stripped.is_empty() {
            if current == SEC_END_HED {
                return Err(SchemaLoadError::single(
                    codes::WIKI_LINE_INVALID,
                    &format!("Found content {} after end of schema", stripped),
                ));
            }
            if stripped.starts_with("'''") || stripped.starts_with("!#") {
                let marker = SECTION_MARKERS
                    .iter()
                    .find(|(_, m)| stripped.starts_with(m))
                    .map(|(n, _)| *n);
                match marker {
                    Some(section) => {
                        if section <= current {
                            return Err(SchemaLoadError::single(
                                codes::SCHEMA_SECTION_MISSING,
                                &format!(
                                    "Found section {} out of order in file",
                                    marker_for(section)
                                ),
                            ));
                        }
                        if by_section.contains_key(&section) {
                            return Err(SchemaLoadError::single(
                                codes::WIKI_SEPARATOR_INVALID,
                                &format!("Found section {} twice", marker_for(section)),
                            ));
                        }
                        by_section.insert(section, Vec::new());
                        current = section;
                        continue;
                    }
                    None if stripped.starts_with("!#") => {
                        return Err(SchemaLoadError::single(
                            codes::WIKI_SEPARATOR_INVALID,
                            &format!("Section separator '{}' is invalid", stripped),
                        ));
                    }
                    None => {}
                }
            }
        }

        if current == SEC_PROLOGUE || current == SEC_EPILOGUE {
            by_section
                .get_mut(&current)
                .unwrap()
                .push((line_number, raw_line.to_string()));
        } else if !stripped.is_empty() {
            let cleaned = remove_nowiki(line_number, stripped, &mut nowiki_issues);
            if !cleaned.is_empty() {
                by_section
                    .get_mut(&current)
                    .unwrap()
                    .push((line_number, cleaned));
            }
        }
    }

    if !nowiki_issues.is_empty() {
        return Err(SchemaLoadError::from_issues(nowiki_issues));
    }

    for required in REQUIRED_SECTIONS {
        if !by_section.contains_key(&required) {
            return Err(SchemaLoadError::single(
                codes::SCHEMA_SECTION_MISSING,
                &format!(
                    "Required section separator '{}' not found in file",
                    marker_for(required)
                ),
            ));
        }
    }

    Ok(by_section)
}

fn remove_nowiki(line_number: usize, line: &str, issues: &mut Vec<HedError>) -> String {
    let i1 = line.find("<nowiki>");
    let i2 = line.find("</nowiki>");
    match (i1, i2) {
        (Some(_), None) | (None, Some(_)) => {
            issues.push(HedError::error(
                codes::WIKI_DELIMITERS_INVALID,
                &format!(
                    "Line {}: Invalid or non matching <nowiki> tags found",
                    line_number
                ),
                Some(line.to_string()),
            ));
        }
        (Some(a), Some(b)) if b <= a => {
            issues.push(HedError::error(
                codes::WIKI_DELIMITERS_INVALID,
                &format!(
                    "Line {}: </nowiki> appears before <nowiki> on a line",
                    line_number
                ),
                Some(line.to_string()),
            ));
        }
        _ => {}
    }
    NOWIKI_RE.replace_all(line, "").into_owned()
}

fn parse_sections(
    lines: &[&str],
    schema: &mut Schema,
    ctx: &WikiContext,
) -> Result<(), SchemaLoadError> {
    let by_section = split_into_sections(lines)?;
    let mut fatal = FatalErrors(Vec::new());

    // Header section must have no stray content.
    for (_, line) in by_section.get(&SEC_HEADER).into_iter().flatten() {
        if !line.trim().is_empty() {
            return Err(SchemaLoadError::single(
                codes::SCHEMA_HEADER_INVALID,
                &format!(
                    "Extra content [{}] between HED line and other sections",
                    line.trim()
                ),
            ));
        }
    }

    schema.prologue = read_text_block(by_section.get(&SEC_PROLOGUE));
    schema.epilogue = read_text_block(by_section.get(&SEC_EPILOGUE));

    read_flat_section(
        by_section.get(&SEC_PROPERTIES),
        sections::PROPERTIES,
        schema,
        ctx,
        &mut fatal,
    );
    read_flat_section(
        by_section.get(&SEC_ATTRIBUTES),
        sections::ATTRIBUTES,
        schema,
        ctx,
        &mut fatal,
    );
    read_flat_section(
        by_section.get(&SEC_UNIT_MODIFIERS),
        sections::UNIT_MODIFIERS,
        schema,
        ctx,
        &mut fatal,
    );
    read_unit_classes(by_section.get(&SEC_UNIT_CLASSES), schema, ctx, &mut fatal);
    read_flat_section(
        by_section.get(&SEC_VALUE_CLASSES),
        sections::VALUE_CLASSES,
        schema,
        ctx,
        &mut fatal,
    );
    read_tag_section(by_section.get(&SEC_SCHEMA), schema, ctx, &mut fatal);
    read_extras(&by_section, schema, ctx);

    if !fatal.0.is_empty() {
        return Err(SchemaLoadError::from_issues(fatal.0));
    }
    Ok(())
}

fn read_text_block(lines: Option<&SectionLines>) -> String {
    let mut text = String::new();
    for (_, line) in lines.into_iter().flatten() {
        text.push_str(line);
        text.push('\n');
    }
    if text.ends_with("\n\n") {
        text.truncate(text.len() - 2);
    } else if text.ends_with('\n') {
        text.truncate(text.len() - 1);
    }
    text
}

/// The parsed parts of one entry line: name + attributes + description.
struct ParsedLine {
    name: String,
    attributes: HashMap<String, Vec<String>>,
    description: String,
}

fn tag_level(line: &str) -> usize {
    let count = line.chars().take_while(|c| *c == '*').count();
    if count == 0 { 1 } else { count }
}

/// Mirrors `_get_tag_name`: returns the entry name and the byte offset at which the
/// attribute/description blocks may begin. `Ok(None)` means "extend here" (an ignorable
/// placeholder that yields an empty name).
fn get_tag_name(line: &str) -> Option<(String, usize)> {
    if line.contains("extend here") {
        return Some((String::new(), 0));
    }
    let cleaned = line.replace("\u{200b}", "");
    let caps = TAG_NAME_RE.captures(&cleaned)?;
    let name = caps
        .get(2)
        .map(|m| m.as_str().trim().to_string())
        .unwrap_or_default();
    if name.is_empty() {
        return None;
    }
    let index = caps.get(4).map(|m| m.start()).unwrap_or(cleaned.len());
    Some((name, index))
}

/// Mirrors `_get_line_section` exactly, including its Python-slice quirks when a delimiter
/// appears only *before* `starting_index` (a negative `find` result feeds Python's negative
/// indexing) — fixture outcomes depend on this behavior. Returns `None` on mismatched
/// delimiter counts; `Some(("", idx))` when the delimiters simply aren't present at all.
fn get_line_section(
    line: &str,
    starting_index: usize,
    open: char,
    close: char,
) -> Option<(String, usize)> {
    let count_open = line.matches(open).count();
    let count_close = line.matches(close).count();
    if count_open != count_close || count_open > 1 {
        return None;
    }

    let slice = &line[starting_index.min(line.len())..];
    let i1: i64 = slice.find(open).map(|i| i as i64).unwrap_or(-1);
    let i2: i64 = slice.find(close).map(|i| i as i64).unwrap_or(-1);
    if i2 < i1 {
        return None;
    }
    if count_open == 0 {
        return Some((String::new(), starting_index));
    }

    // Python: `row[index1 + 1 : index2]` with -1 meaning "from the start" / "up to the
    // last character" via negative indexing.
    let len = slice.len() as i64;
    let start = (i1 + 1).max(0);
    let end = if i2 >= 0 { i2 } else { (len + i2).max(0) };
    let content = if end > start {
        slice[start as usize..end as usize].to_string()
    } else {
        String::new()
    };
    let next_index = (i2 + starting_index as i64).max(0) as usize;
    Some((content, next_index))
}

fn parse_attribute_block(attr_string: &str) -> Result<HashMap<String, Vec<String>>, String> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    if attr_string.is_empty() {
        return Ok(out);
    }
    for token in attr_string.split(',') {
        let token = token.trim();
        if !ATTRIBUTE_TOKEN_RE.is_match(token) {
            return Err(format!(
                "Malformed attribute {}. Valid formatting is: attribute, or attribute=\"value\"",
                token
            ));
        }
        match token.split_once('=') {
            Some((key, value)) => out
                .entry(key.to_string())
                .or_default()
                .push(value.to_string()),
            None => {
                out.entry(token.to_string()).or_default();
            }
        }
    }
    Ok(out)
}

fn parse_entry_line(line_number: usize, line: &str, fatal: &mut FatalErrors) -> Option<ParsedLine> {
    let Some((name, index)) = get_tag_name(line) else {
        fatal.add(
            line_number,
            line,
            "Schema term is empty or the line is malformed",
            codes::WIKI_DELIMITERS_INVALID,
        );
        return None;
    };
    if name.is_empty() {
        fatal.add(
            line_number,
            line,
            "Schema term is empty or the line is malformed",
            codes::WIKI_DELIMITERS_INVALID,
        );
        return None;
    }

    let Some((attr_string, index)) = get_line_section(line, index, '{', '}') else {
        fatal.add(
            line_number,
            line,
            "Attributes has mismatched delimiters",
            codes::WIKI_DELIMITERS_INVALID,
        );
        return None;
    };
    let attributes = match parse_attribute_block(&attr_string) {
        Ok(a) => a,
        Err(msg) => {
            fatal.add(line_number, line, &msg, codes::WIKI_DELIMITERS_INVALID);
            return None;
        }
    };

    let Some((description, _)) = get_line_section(line, index, '[', ']') else {
        fatal.add(
            line_number,
            line,
            "Description has mismatched delimiters",
            codes::WIKI_DELIMITERS_INVALID,
        );
        return None;
    };

    Some(ParsedLine {
        name,
        attributes,
        description: description.trim().to_string(),
    })
}

/// hed-python's `_add_to_dict_base` entry-admission logic. Returns false if the entry
/// should be skipped entirely; otherwise may stamp `inLibrary` onto it.
fn admit_entry(attributes: &mut HashMap<String, Vec<String>>, ctx: &WikiContext) -> bool {
    let has_in_library = attributes.contains_key("inLibrary");
    if !has_in_library && ctx.appending && ctx.schema_merged {
        return false;
    }
    if !ctx.library.is_empty()
        && (ctx.schema_with_standard.is_empty() || !ctx.schema_merged)
        && !has_in_library
    {
        attributes.insert("inLibrary".to_string(), vec![ctx.library.clone()]);
    }
    true
}

/// Load-time InLibrary sanity check (`_add_to_dict` in wiki2schema.py).
fn check_in_library(
    line_number: usize,
    line: &str,
    attributes: &HashMap<String, Vec<String>>,
    ctx: &WikiContext,
    fatal: &mut FatalErrors,
) {
    if attributes.contains_key("inLibrary") && !ctx.loading_merged && !ctx.appending {
        fatal.add(
            line_number,
            line,
            "Library tag in unmerged schema has InLibrary attribute",
            codes::SCHEMA_LIBRARY_INVALID,
        );
    }
}

fn record_duplicate(schema: &mut Schema, section: &str, name: &str, spans_library: bool) {
    schema
        .duplicate_names
        .entry(section.to_string())
        .or_default()
        .push(super::model::DuplicateName {
            name: name.to_string(),
            spans_library,
        });
}

fn read_flat_section(
    lines: Option<&SectionLines>,
    section: &'static str,
    schema: &mut Schema,
    ctx: &WikiContext,
    fatal: &mut FatalErrors,
) {
    for (line_number, line) in lines.into_iter().flatten() {
        let Some(mut parsed) = parse_entry_line(*line_number, line, fatal) else {
            continue;
        };
        check_in_library(*line_number, line, &parsed.attributes, ctx, fatal);
        if !admit_entry(&mut parsed.attributes, ctx) {
            continue;
        }

        let key = parsed.name.to_lowercase();
        let entry = SchemaEntry {
            name: parsed.name.clone(),
            description: parsed.description.clone(),
            attributes: parsed.attributes.clone(),
        };
        match section {
            sections::PROPERTIES => {
                if let Some(existing) = schema.properties.get(&key) {
                    let spans =
                        existing.has_attribute("inLibrary") != entry.has_attribute("inLibrary");
                    record_duplicate(schema, section, &parsed.name, spans);
                } else {
                    schema.properties.insert(key, entry);
                }
            }
            sections::ATTRIBUTES => {
                if let Some(existing) = schema.schema_attributes.get(&key) {
                    let spans =
                        existing.has_attribute("inLibrary") != entry.has_attribute("inLibrary");
                    record_duplicate(schema, section, &parsed.name, spans);
                } else {
                    schema.schema_attributes.insert(key, entry);
                }
            }
            sections::UNIT_MODIFIERS => {
                if let Some(existing) = schema.unit_modifiers.get(&key) {
                    let spans = existing.entry.has_attribute("inLibrary")
                        != entry.has_attribute("inLibrary");
                    record_duplicate(schema, section, &parsed.name, spans);
                } else {
                    let is_symbol_modifier = entry.has_attribute("SIUnitSymbolModifier");
                    schema.unit_modifiers.insert(
                        key,
                        UnitModifier {
                            name: parsed.name.clone(),
                            is_symbol_modifier,
                            entry,
                        },
                    );
                }
            }
            sections::VALUE_CLASSES => {
                if let Some(existing) = schema
                    .value_classes
                    .get(&parsed.name)
                    .or_else(|| schema.value_classes.get(&key))
                {
                    let spans = existing.entry.has_attribute("inLibrary")
                        != entry.has_attribute("inLibrary");
                    record_duplicate(schema, section, &parsed.name, spans);
                } else {
                    let allowed_characters = entry
                        .attribute_values("allowedCharacter")
                        .iter()
                        .flat_map(|v| v.split(','))
                        .map(super::json_parser::translate_allowed_character)
                        .collect();
                    schema.value_classes.insert(
                        parsed.name.clone(),
                        ValueClass {
                            name: parsed.name.clone(),
                            allowed_characters,
                            entry,
                        },
                    );
                }
            }
            _ => unreachable!("read_flat_section only handles flat sections"),
        }
    }
}

fn read_unit_classes(
    lines: Option<&SectionLines>,
    schema: &mut Schema,
    ctx: &WikiContext,
    fatal: &mut FatalErrors,
) {
    let mut current_class: Option<String> = None;

    for (line_number, line) in lines.into_iter().flatten() {
        let Some(mut parsed) = parse_entry_line(*line_number, line, fatal) else {
            continue;
        };
        check_in_library(*line_number, line, &parsed.attributes, ctx, fatal);
        if !admit_entry(&mut parsed.attributes, ctx) {
            continue;
        }

        let level = tag_level(line);
        if level == 1 {
            // A unit class.
            let name = parsed.name.clone();
            let exists = schema.unit_classes.contains_key(&name);
            if exists {
                // Re-adding an existing class with only an inLibrary marker is a merge
                // convenience, not a duplicate.
                let only_in_library =
                    parsed.attributes.len() == 1 && parsed.attributes.contains_key("inLibrary");
                if !only_in_library {
                    let spans = schema.unit_classes.get(&name).is_some_and(|e| {
                        e.entry.has_attribute("inLibrary")
                            != parsed.attributes.contains_key("inLibrary")
                    });
                    record_duplicate(schema, sections::UNIT_CLASSES, &name, spans);
                }
            } else {
                let default_unit = parsed
                    .attributes
                    .get("defaultUnits")
                    .and_then(|v| v.first())
                    .cloned();
                schema.unit_classes.insert(
                    name.clone(),
                    UnitClass {
                        name: name.clone(),
                        units: Vec::new(),
                        default_unit,
                        entry: SchemaEntry {
                            name: name.clone(),
                            description: parsed.description.clone(),
                            attributes: parsed.attributes.clone(),
                        },
                    },
                );
            }
            current_class = Some(name);
        } else {
            // A unit within the current class.
            let Some(class_name) = current_class.clone() else {
                fatal.add(
                    *line_number,
                    line,
                    "Unit found outside a unit class",
                    codes::WIKI_DELIMITERS_INVALID,
                );
                continue;
            };
            let unit_symbol = parsed.attributes.contains_key("unitSymbol");
            // Units with unitSymbol are case-sensitive; others collide case-insensitively.
            let dup_key = if unit_symbol {
                parsed.name.clone()
            } else {
                parsed.name.to_lowercase()
            };
            let colliding = schema.units.values().find(|u| {
                let existing_key = if u.unit_symbol {
                    u.name.clone()
                } else {
                    u.name.to_lowercase()
                };
                existing_key == dup_key
            });
            if let Some(existing) = colliding {
                let spans = existing.entry.has_attribute("inLibrary")
                    != parsed.attributes.contains_key("inLibrary");
                record_duplicate(schema, sections::UNITS, &parsed.name, spans);
                continue;
            }
            schema.units.insert(
                parsed.name.clone(),
                UnitEntry {
                    name: parsed.name.clone(),
                    si_unit: parsed.attributes.contains_key("SIUnit"),
                    unit_symbol,
                    unit_class: class_name.clone(),
                    entry: SchemaEntry {
                        name: parsed.name.clone(),
                        description: parsed.description.clone(),
                        attributes: parsed.attributes.clone(),
                    },
                },
            );
            if let Some(class) = schema.unit_classes.get_mut(&class_name) {
                class.units.push(parsed.name.clone());
            }
        }
    }
}

fn read_tag_section(
    lines: Option<&SectionLines>,
    schema: &mut Schema,
    ctx: &WikiContext,
    fatal: &mut FatalErrors,
) {
    let mut parent_tags: Vec<String> = Vec::new();
    let mut level_adj: usize = 0;

    for (line_number, line) in lines.into_iter().flatten() {
        if line.starts_with("'''") {
            parent_tags.clear();
            level_adj = 0;
        } else {
            let level = tag_level(line) + level_adj;
            if level < parent_tags.len() {
                parent_tags.truncate(level);
            } else if level > parent_tags.len() {
                fatal.add(
                    *line_number,
                    line,
                    "Line has too many *'s at front. You cannot skip a level.",
                    codes::WIKI_LINE_START_INVALID,
                );
                continue;
            }
        }

        let Some(mut parsed) = parse_entry_line(*line_number, line, fatal) else {
            continue;
        };

        // Rooted-tag handling (library grafting onto the partner standard schema).
        let mut effective_parents = parent_tags.clone();
        if let Some(rooted_values) = parsed.attributes.get("rooted") {
            match check_rooted(rooted_values, &parsed.name, &effective_parents, schema, ctx) {
                Ok(Some(target_path)) => {
                    effective_parents = target_path.split('/').map(|s| s.to_string()).collect();
                    level_adj = effective_parents.len();
                }
                Ok(None) => {}
                Err(msg) => {
                    fatal.add(*line_number, line, &msg, codes::SCHEMA_LIBRARY_INVALID);
                    continue;
                }
            }
        }

        check_in_library(*line_number, line, &parsed.attributes, ctx, fatal);
        if !admit_entry(&mut parsed.attributes, ctx) {
            continue;
        }

        let mut path = effective_parents.clone();
        path.push(parsed.name.clone());

        let node = SchemaNode {
            name: parsed.name.clone(),
            description: parsed.description.clone(),
            attributes: parsed.attributes.clone(),
            children: HashMap::new(),
        };

        if !insert_tag_node(schema, &path, node) {
            let spans = schema
                .find_by_full_path(&path.join("/"))
                .is_some_and(|existing| {
                    existing.attributes.contains_key("inLibrary")
                        != parsed.attributes.contains_key("inLibrary")
                });
            record_duplicate(schema, sections::TAGS, &parsed.name, spans);
        }
        // Even a duplicate still becomes the nesting context for subsequent lines
        // (hed-python keeps parsing children under the duplicated name).
        parent_tags = path;
    }
}

/// Validates a `rooted=` attribute, returning the full path of the target node in the
/// partner standard schema when the current node should be grafted under it.
fn check_rooted(
    rooted_values: &[String],
    short_name: &str,
    parents: &[String],
    schema: &Schema,
    ctx: &WikiContext,
) -> Result<Option<String>, String> {
    if ctx.schema_with_standard.is_empty() {
        return Err(format!(
            "Rooted tag attribute found on '{}' in a standard schema.",
            short_name
        ));
    }
    let Some(rooted_tag) = rooted_values.first() else {
        return Err(format!("Rooted tag '{}' is not a string.", short_name));
    };
    if !parents.is_empty() && !ctx.loading_merged {
        return Err(format!(
            "Found rooted tag '{}' as a non root node.",
            short_name
        ));
    }
    if parents.is_empty() && ctx.loading_merged {
        return Err(format!(
            "Found rooted tag '{}' as a root node in a merged schema.",
            short_name
        ));
    }

    let target_path = if rooted_tag.contains('/') {
        schema
            .find_by_full_path(rooted_tag)
            .map(|_| rooted_tag.to_string())
    } else {
        schema.paths_for_tag(rooted_tag).first().cloned()
    };
    let Some(target_path) = target_path else {
        return Err(format!(
            "Rooted tag '{}' not found in paired standard schema",
            short_name
        ));
    };
    let target = schema.find_by_full_path(&target_path);
    if target.is_none_or(|n| n.has_attribute("inLibrary")) {
        return Err(format!(
            "Rooted tag '{}' not found in paired standard schema",
            short_name
        ));
    }

    if ctx.loading_merged {
        return Ok(None);
    }
    Ok(Some(target_path))
}

/// Inserts `node` at `path` in the tag tree. Returns false when a node already exists at
/// that exact path (a duplicate). A missing intermediate parent also returns false (its own
/// error was reported when the parent line failed).
fn insert_tag_node(schema: &mut Schema, path: &[String], node: SchemaNode) -> bool {
    if path.len() == 1 {
        let key = path[0].to_lowercase();
        if schema.root_nodes.contains_key(&key) {
            return false;
        }
        schema.root_nodes.insert(key, node);
        return true;
    }

    let mut current = match schema.root_nodes.get_mut(&path[0].to_lowercase()) {
        Some(n) => n,
        None => return false,
    };
    for segment in &path[1..path.len() - 1] {
        current = match current.children.get_mut(&segment.to_lowercase()) {
            Some(n) => n,
            None => return false,
        };
    }
    let final_key = path[path.len() - 1].to_lowercase();
    if current.children.contains_key(&final_key) {
        return false;
    }
    current.children.insert(final_key, node);
    true
}

fn read_extras(by_section: &HashMap<u8, SectionLines>, schema: &mut Schema, ctx: &WikiContext) {
    let extras_sections: [(u8, &str); 3] = [
        (SEC_SOURCES, "sources"),
        (SEC_PREFIXES, "prefixes"),
        (SEC_EXTERNAL, "external_annotations"),
    ];
    for (section, key) in extras_sections {
        let Some(lines) = by_section.get(&section) else {
            continue;
        };
        let mut rows = Vec::new();
        for (_, line) in lines {
            let mut row = parse_star_string(line.trim());
            if !ctx.library.is_empty() && !ctx.loading_merged && !row.contains_key("in_library") {
                row.insert("in_library".to_string(), ctx.library.clone());
            }
            rows.push(row);
        }
        if rows.is_empty() {
            continue;
        }
        schema
            .extras
            .entry(key.to_string())
            .or_default()
            .extend(rows);
    }
}

/// Parses a `* [{attr=val}] key=value, key=value` extras row into a map.
fn parse_star_string(s: &str) -> HashMap<String, String> {
    let mut s = s.trim_start_matches(['*', ' ']).trim().to_string();
    let mut result = HashMap::new();

    if s.starts_with('{')
        && let Some(end_brace) = s.find('}')
    {
        let attr_str = s[1..end_brace].to_string();
        s = s[end_brace + 1..].trim().to_string();
        for attr in attr_str.split(',') {
            let attr = attr.trim();
            match attr.split_once('=') {
                Some((k, v)) => {
                    result.insert(k.trim().to_string(), v.trim().to_string());
                }
                None if !attr.is_empty() => {
                    result.insert(attr.to_string(), "True".to_string());
                }
                None => {}
            }
        }
    }

    for pair in s.split(',') {
        if let Some((k, v)) = pair.trim().split_once('=') {
            result.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_base(_v: &str) -> Result<Schema, SchemaLoadError> {
        Err(SchemaLoadError::single(
            codes::SCHEMA_LOAD_FAILED,
            "no base loader in this test",
        ))
    }

    const MINIMAL: &str = r#"HED version="1.0.0"

'''Prologue'''
Test prologue.

!# start schema

'''Test-tag''' <nowiki>[A root tag.]</nowiki>
* Test-child <nowiki>{extensionAllowed} [A child.]</nowiki>
** <nowiki># {takesValue, valueClass=digitClass} [Value here]</nowiki>

!# end schema

'''Unit classes'''
* myUnits <nowiki>{defaultUnits=deg}</nowiki>
** deg <nowiki>{conversionFactor=1.0}</nowiki>

'''Unit modifiers'''

'''Value classes'''
* digitClass <nowiki>{allowedCharacter=digits} [Digits only.]</nowiki>

'''Schema attributes'''
* extensionAllowed <nowiki>{tagDomain, boolRange} [Extension allowed.]</nowiki>
* takesValue <nowiki>{tagDomain, boolRange} [Takes a value.]</nowiki>
* valueClass <nowiki>{tagDomain, valueClassRange} [Value class.]</nowiki>
* defaultUnits <nowiki>{unitClassDomain, unitRange} [Default units.]</nowiki>
* conversionFactor <nowiki>{unitDomain, numericRange} [Conversion.]</nowiki>

'''Properties'''
* tagDomain <nowiki>[Tags.]</nowiki>
* boolRange <nowiki>[Bool.]</nowiki>
* valueClassRange <nowiki>[VC.]</nowiki>
* unitClassDomain <nowiki>[UC domain.]</nowiki>
* unitRange <nowiki>[Unit range.]</nowiki>
* unitDomain <nowiki>[Unit domain.]</nowiki>
* numericRange <nowiki>[Numeric.]</nowiki>

'''Epilogue'''
The end.

!# end hed"#;

    #[test]
    fn parses_minimal_schema() {
        let schema =
            load_wiki_string(MINIMAL, None, &no_base).expect("minimal schema should parse");
        assert_eq!(schema.version, "1.0.0");
        assert_eq!(schema.prologue, "Test prologue.");
        assert_eq!(schema.epilogue, "The end.");
        assert!(schema.has_tag_named("Test-child"));
        let node = schema.tag_entry_by_short_name("Test-child").unwrap();
        assert!(node.has_attribute("extensionAllowed"));
        assert!(node.children.contains_key("#"));
        assert_eq!(schema.unit_classes["myUnits"].units, vec!["deg"]);
        assert!(schema.units.contains_key("deg"));
        assert_eq!(
            schema.value_classes["digitClass"].allowed_characters,
            vec!["digits"]
        );
        assert!(schema.schema_attributes.contains_key("takesvalue"));
        assert!(schema.properties.contains_key("tagdomain"));
        assert!(!schema.has_duplicates());
        // resolve_tag works against wiki-parsed trees
        assert!(matches!(
            schema.resolve_tag("Test-child/5"),
            TagResolution::Value { .. }
        ));
    }

    #[test]
    fn missing_section_is_fatal() {
        let text = MINIMAL.replace("'''Unit modifiers'''\n", "");
        let err = load_wiki_string(&text, None, &no_base).unwrap_err();
        assert_eq!(err.code, codes::SCHEMA_SECTION_MISSING);
    }

    #[test]
    fn out_of_order_section_is_fatal() {
        let text = MINIMAL.replace("'''Unit classes'''", "'''Epilogue'''");
        let err = load_wiki_string(&text, None, &no_base).unwrap_err();
        // Epilogue appears before UnitClasses AND later again; either out-of-order or twice.
        assert!(
            err.code == codes::SCHEMA_SECTION_MISSING || err.code == codes::WIKI_SEPARATOR_INVALID
        );
    }

    #[test]
    fn bad_header_attribute_is_fatal() {
        let text = MINIMAL.replace(
            r#"HED version="1.0.0""#,
            r#"HED version="1.0.0" unknownAttribute=other"#,
        );
        let err = load_wiki_string(&text, None, &no_base).unwrap_err();
        assert_eq!(err.code, codes::SCHEMA_HEADER_INVALID);
    }

    #[test]
    fn with_standard_requires_library() {
        let text = MINIMAL.replace(
            r#"HED version="1.0.0""#,
            r#"HED version="1.0.0" withStandard="8.4.0""#,
        );
        let err = load_wiki_string(&text, None, &no_base).unwrap_err();
        assert_eq!(err.code, codes::SCHEMA_LIBRARY_INVALID);
    }

    #[test]
    fn empty_conversion_factor_is_delimiter_error() {
        let text = MINIMAL.replace("{conversionFactor=1.0}", "{conversionFactor=}");
        let err = load_wiki_string(&text, None, &no_base).unwrap_err();
        assert_eq!(err.code, codes::WIKI_DELIMITERS_INVALID);
    }

    #[test]
    fn duplicate_short_names_recorded() {
        let text = MINIMAL.replace(
            "* Test-child <nowiki>{extensionAllowed} [A child.]</nowiki>",
            "* Test-child <nowiki>[A child.]</nowiki>\n* Test-child <nowiki>[Again.]</nowiki>",
        );
        let schema = load_wiki_string(&text, None, &no_base)
            .expect("dups are compliance issues, not load failures");
        assert!(schema.has_duplicates());
    }
}
