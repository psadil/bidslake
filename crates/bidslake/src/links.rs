//! Cross-dataset identity normalization.
//!
//! A dataset declares where it came from in several interchangeable forms — a bare
//! DOI, a `https://doi.org/…` URL, a repository URL, a filesystem/S3 path, or (via
//! bidslake's `--source-dataset` escape hatch) another dataset's id. Two datasets
//! that name the *same* source in *different* forms must resolve to the *same*
//! identity, or they won't be recognized as co-derivatives.
//!
//! [`canonicalize`] maps any declared reference to a stable [`Identity`]. It is the
//! single point that makes MRIQC's bare DOI and fMRIPrep's `https://doi.org/…` URL
//! collide — the whole cross-dataset feature turns on that one normalization.
//!
//! [`canonicalize_relative_to`] is the same thing for a reference that is **not**
//! self-contained. `dataset_links` holds two kinds of statement (see
//! `CREATE_DATASET_LINKS_TABLE`): *provenance* — "came from S", always an absolute
//! reference — and *naming* — "here, `fs` refers to L", which BIDS `DatasetLinks` usually
//! writes as a path relative to the dataset root. A relative reference has no meaning
//! without that root, so the two entry points differ only in whether one is supplied.
//!
//! Nothing is ever rejected: an unrecognizable reference becomes an [`IdentityKind::Opaque`]
//! identity rather than an error, mirroring the best-effort, keep-everything contract
//! of `file_associations` (see `schema.rs`).

/// The kind of a canonicalized dataset identity. Stored in `dataset_links.identity_kind`
/// so a consumer can prefer the reliable kinds (DOI) over the fragile ones (a local path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityKind {
    /// A DOI, e.g. `doi:10.18112/openneuro.ds001761.v2.0.1` (the most reliable).
    Doi,
    /// An `http(s)` URL that is not a DOI (a code repository, a landing page).
    Url,
    /// A `file://` or `s3://` location (absolute to the ingesting host — least reliable).
    File,
    /// Another catalog dataset named by its `dataset_id`, e.g. `dataset:ds001761-fmriprep`.
    Dataset,
    /// An unrecognized reference, kept verbatim so identical strings still collide.
    Opaque,
}

impl IdentityKind {
    /// The lowercase token stored in `dataset_links.identity_kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            IdentityKind::Doi => "doi",
            IdentityKind::Url => "url",
            IdentityKind::File => "file",
            IdentityKind::Dataset => "dataset",
            IdentityKind::Opaque => "opaque",
        }
    }
}

/// A canonicalized dataset identity: a comparable `value`, its `kind`, and a
/// version-stripped `base` (used only to *warn* about version drift; the relation
/// views match on `value`, never `base`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub value: String,
    pub kind: IdentityKind,
    pub base: String,
}

/// The DOI-resolver prefixes we strip before recognizing a DOI. Matched
/// case-insensitively; a bare `10.…/…` DOI carries none of them.
const DOI_PREFIXES: [&str; 5] = [
    "https://doi.org/",
    "http://doi.org/",
    "https://dx.doi.org/",
    "http://dx.doi.org/",
    "doi:",
];

/// Canonicalize a `DatasetLinks` value, which is **relative to the dataset root** unless it
/// carries a scheme.
///
/// BIDS writes these as URIs, and in practice almost always as a path relative to the
/// dataset root — `"derivatives/fmriprep"`, `"../sourcedata/freesurfer"`. [`canonicalize`]
/// alone has no root to resolve against, so such a value falls through to its bare-token
/// branch and becomes the identity `dataset:derivatives/fmriprep`: a `Dataset` identity no
/// `dataset_identity` row can ever hold, so the link is stored and never resolves. Joining
/// it to the root first is what makes it a `file://`/`s3://` identity that matches the
/// target's own `root_uri`.
///
/// A value that *is* absolute — a DOI, an `http(s)`/`file`/`s3` URL, bidslake's own
/// `dataset:<id>` escape hatch, or an absolute path — is passed through untouched, so the
/// hand-written forms keep working exactly as before.
pub fn canonicalize_relative_to(declared: &str, root_uri: &str) -> Identity {
    let s = declared.trim();
    if s.is_empty() {
        return opaque("");
    }
    if is_absolute_reference(s) {
        return canonicalize(s);
    }
    canonicalize(&join_uri(root_uri, s))
}

/// Whether a reference stands on its own, or needs a root to be meaningful.
fn is_absolute_reference(s: &str) -> bool {
    s.starts_with('/')
        || s.contains("://")
        || s.starts_with("dataset:")
        || strip_doi_prefix(s).is_some()
        || (s.starts_with("10.") && s.contains('/'))
}

/// Join a root URI and a relative path, resolving `.`/`..` textually.
///
/// Textually, not through the filesystem: the root may be `s3://`, and even a local one may
/// name a path that does not exist on the machine reading the catalog. `..` that would climb
/// above the root is dropped rather than escaping it.
fn join_uri(root_uri: &str, rel: &str) -> String {
    let root = root_uri.trim_end_matches('/');
    let (scheme, rest) = match root.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, root),
    };
    let mut segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    // A leading empty segment is the root of an absolute `file:///…` path; remember whether
    // to re-emit it so `file:///data/x` does not come back as `file://data/x`.
    let absolute = rest.starts_with('/');
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let joined = segments.join("/");
    match (scheme, absolute) {
        (Some(scheme), true) => format!("{scheme}:///{joined}"),
        (Some(scheme), false) => format!("{scheme}://{joined}"),
        (None, true) => format!("/{joined}"),
        (None, false) => joined,
    }
}

/// Canonicalize a declared source reference into a comparable [`Identity`].
pub fn canonicalize(declared: &str) -> Identity {
    let s = declared.trim();
    if s.is_empty() {
        return opaque("");
    }

    // DOI — strip any resolver prefix, then require the `10.<registrant>/<suffix>`
    // shape. DOIs are case-insensitive (Handle spec), so lowercase the whole thing:
    // this is what makes the bare-DOI and URL-DOI forms of one source collide.
    let doi_candidate = strip_doi_prefix(s).unwrap_or(s);
    if doi_candidate.starts_with("10.") && doi_candidate.contains('/') {
        let value = format!("doi:{}", doi_candidate.to_ascii_lowercase());
        let base = strip_version(&value);
        return Identity {
            value,
            kind: IdentityKind::Doi,
            base,
        };
    }

    // Explicit dataset reference (the `--source-dataset dataset:<id>` disambiguator).
    // Dataset ids are case-sensitive `Name`s, so this is not lowercased.
    if let Some(rest) = s.strip_prefix("dataset:") {
        return exact(format!("dataset:{rest}"), IdentityKind::Dataset);
    }

    // Object-store and filesystem locations. `s3://` is kept verbatim; a `file://`
    // URL and a bare absolute path both canonicalize to `file://…`.
    if s.starts_with("s3://") {
        return exact(s.trim_end_matches('/').to_string(), IdentityKind::File);
    }
    if let Some(rest) = s.strip_prefix("file://") {
        return exact(
            format!("file://{}", rest.trim_end_matches('/')),
            IdentityKind::File,
        );
    }
    if s.starts_with("http://") || s.starts_with("https://") {
        return exact(normalize_url(s), IdentityKind::Url);
    }
    if s.starts_with('/') {
        return exact(
            format!("file://{}", s.trim_end_matches('/')),
            IdentityKind::File,
        );
    }

    // An unknown scheme (`ftp://…`) is kept opaque rather than mis-typed.
    if s.contains("://") {
        return opaque(s);
    }

    // A bare token with no whitespace is treated as a dataset id (the common
    // `--source-dataset ds001761-fmriprep`); anything else is opaque.
    if !s.chars().any(char::is_whitespace) {
        return exact(format!("dataset:{s}"), IdentityKind::Dataset);
    }
    opaque(s)
}

/// An identity whose `base` equals its `value` (only DOIs carry a version to strip).
fn exact(value: String, kind: IdentityKind) -> Identity {
    Identity {
        base: value.clone(),
        value,
        kind,
    }
}

fn opaque(raw: &str) -> Identity {
    exact(format!("opaque:{raw}"), IdentityKind::Opaque)
}

/// Strip a matching DOI-resolver prefix (case-insensitive), returning the remainder.
fn strip_doi_prefix(s: &str) -> Option<&str> {
    DOI_PREFIXES.iter().find_map(|p| {
        (s.len() >= p.len() && s[..p.len()].eq_ignore_ascii_case(p)).then(|| &s[p.len()..])
    })
}

/// Lowercase the `scheme://host` of a URL (leaving the path case intact), drop any
/// `#fragment`, and strip a trailing `/`.
fn normalize_url(s: &str) -> String {
    let no_frag = s.split('#').next().unwrap_or(s).trim_end_matches('/');
    match no_frag.find("://") {
        Some(scheme_end) => {
            let host_start = scheme_end + 3;
            let host_end = no_frag[host_start..]
                .find('/')
                .map_or(no_frag.len(), |i| host_start + i);
            format!(
                "{}{}",
                no_frag[..host_end].to_ascii_lowercase(),
                &no_frag[host_end..]
            )
        }
        None => no_frag.to_string(),
    }
}

/// Strip a trailing `.v<digits>(.<digits>)*` (an OpenNeuro-style version) from an
/// identity value, so `…ds001761.v2.0.1` and `…ds001761.v2.0.0` share a base.
fn strip_version(value: &str) -> String {
    if let Some(idx) = value.rfind(".v") {
        let tail = &value[idx + 2..];
        let is_version = tail.chars().next().is_some_and(|c| c.is_ascii_digit())
            && tail.chars().all(|c| c.is_ascii_digit() || c == '.');
        if is_version {
            return value[..idx].to_string();
        }
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_doi_and_url_doi_collide() {
        // The load-bearing case: MRIQC declares the bare DOI, fMRIPrep the URL form.
        let bare = canonicalize("10.18112/openneuro.ds001761.v2.0.1");
        let url = canonicalize("https://doi.org/10.18112/openneuro.ds001761.v2.0.1");
        let pfx = canonicalize("doi:10.18112/openneuro.ds001761.v2.0.1");
        assert_eq!(bare.value, "doi:10.18112/openneuro.ds001761.v2.0.1");
        assert_eq!(bare, url);
        assert_eq!(bare, pfx);
        assert_eq!(bare.kind, IdentityKind::Doi);
    }

    #[test]
    fn doi_is_case_folded() {
        assert_eq!(
            canonicalize("https://doi.org/10.18112/OpenNeuro.DS001761.V2.0.1").value,
            "doi:10.18112/openneuro.ds001761.v2.0.1"
        );
    }

    #[test]
    fn doi_base_strips_version() {
        let id = canonicalize("10.18112/openneuro.ds001761.v2.0.1");
        assert_eq!(id.base, "doi:10.18112/openneuro.ds001761");
        // Different versions share a base but not a value.
        let older = canonicalize("10.18112/openneuro.ds001761.v2.0.0");
        assert_eq!(id.base, older.base);
        assert_ne!(id.value, older.value);
    }

    #[test]
    fn bare_token_is_a_dataset() {
        let id = canonicalize("ds001761-fmriprep");
        assert_eq!(id.value, "dataset:ds001761-fmriprep");
        assert_eq!(id.kind, IdentityKind::Dataset);
    }

    #[test]
    fn explicit_dataset_prefix_keeps_case() {
        assert_eq!(
            canonicalize("dataset:MyStudy_Derivative").value,
            "dataset:MyStudy_Derivative"
        );
    }

    #[test]
    fn url_lowercases_host_not_path() {
        let id = canonicalize("https://GitHub.com/Nipreps/MRIQC/");
        assert_eq!(id.value, "https://github.com/Nipreps/MRIQC");
        assert_eq!(id.kind, IdentityKind::Url);
    }

    #[test]
    fn paths_become_file_uris() {
        assert_eq!(
            canonicalize("/data/ds001761/").value,
            "file:///data/ds001761"
        );
        assert_eq!(canonicalize("/data/ds001761/").kind, IdentityKind::File);
        assert_eq!(
            canonicalize("s3://bucket/prefix/").value,
            "s3://bucket/prefix"
        );
        assert_eq!(canonicalize("file:///data/ds/").value, "file:///data/ds");
    }

    #[test]
    fn unrecognized_is_opaque_not_dropped() {
        assert_eq!(canonicalize("ftp://host/x").kind, IdentityKind::Opaque);
        assert_eq!(canonicalize("some free text").kind, IdentityKind::Opaque);
        // Two identical opaque strings still collide.
        assert_eq!(
            canonicalize("some free text"),
            canonicalize("  some free text ")
        );
    }

    // -- DatasetLinks: values are relative to the dataset root -------------------

    #[test]
    fn a_relative_dataset_link_resolves_against_the_root() {
        // The case that motivated `canonicalize_relative_to`: bare `canonicalize` turns this
        // into `dataset:derivatives/fmriprep`, an identity nothing can ever hold.
        let id = canonicalize_relative_to("derivatives/fmriprep", "file:///data/study");
        assert_eq!(id.value, "file:///data/study/derivatives/fmriprep");
        assert_eq!(id.kind, IdentityKind::File);
        assert_eq!(
            canonicalize("derivatives/fmriprep").value,
            "dataset:derivatives/fmriprep",
            "and this is what it used to produce"
        );
    }

    #[test]
    fn a_relative_dataset_link_matches_the_target_root_uri() {
        // The property that makes the whole thing work: the identity a *link* canonicalizes
        // to must equal the identity the *target dataset* records for its own root.
        let link = canonicalize_relative_to("../freesurfer", "file:///data/study/fmriprep");
        let target_root = canonicalize("file:///data/study/freesurfer");
        assert_eq!(link.value, target_root.value);
    }

    #[test]
    fn dot_segments_resolve_and_cannot_climb_past_the_root() {
        assert_eq!(
            canonicalize_relative_to("./sourcedata/./freesurfer", "file:///data/s").value,
            "file:///data/s/sourcedata/freesurfer"
        );
        // `..` past the top is dropped rather than escaping into nonsense.
        assert_eq!(
            canonicalize_relative_to("../../../../x", "file:///data/s").value,
            "file:///x"
        );
    }

    #[test]
    fn s3_roots_join_without_growing_a_slash() {
        assert_eq!(
            canonicalize_relative_to("derivatives/fmriprep", "s3://bucket/prefix").value,
            "s3://bucket/prefix/derivatives/fmriprep"
        );
        assert_eq!(
            canonicalize_relative_to("derivatives/x", "s3://bucket/prefix/").value,
            "s3://bucket/prefix/derivatives/x"
        );
    }

    #[test]
    fn absolute_references_pass_through_untouched() {
        // Every form that stands on its own must be unaffected by the root, so the
        // hand-written escape hatches keep working.
        for reference in [
            "dataset:studyB",
            "10.18112/openneuro.ds001761.v2.0.1",
            "https://doi.org/10.18112/openneuro.ds001761.v2.0.1",
            "s3://bucket/other",
            "file:///elsewhere/x",
            "/elsewhere/x",
            "https://example.org/repo",
        ] {
            assert_eq!(
                canonicalize_relative_to(reference, "file:///data/study").value,
                canonicalize(reference).value,
                "{reference} must not be joined to the root"
            );
        }
    }
}
