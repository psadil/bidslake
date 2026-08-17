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
    /// The comparable form, stored in `dataset_links.identity` and `dataset_identity.identity`.
    /// String equality on this column is the *entire* linking mechanism (ADR 0003): the
    /// resolver views join on it, so two datasets are co-derivatives exactly when their
    /// declarations reduce to the same `value` and are unrelated otherwise.
    ///
    /// Always carries the kind's marker — `doi:`, `dataset:`, `file://`, `s3://`, the URL
    /// scheme, or `opaque:` — so a reference of one kind can never collide with the same text
    /// read as another. Re-canonicalizing one is a fixed point for every kind but
    /// [`IdentityKind::Opaque`], where the prefix is re-applied and the kind flips to
    /// [`IdentityKind::Dataset`]; a stored `identity` is a result, not a reference to feed back
    /// in, and no call site does.
    pub value: String,
    /// Which branch of [`canonicalize`] recognized the reference, stored in
    /// `dataset_links.identity_kind`. Its use is triage, not matching: an [`IdentityKind::Doi`]
    /// link means the same thing on any machine, while an [`IdentityKind::File`] one is a path
    /// on whichever host ran the ingest and stops resolving the moment the catalog moves.
    pub kind: IdentityKind,
    /// `value` with a trailing OpenNeuro-style `.v<digits>(.<digits>)*` removed, stored in
    /// `dataset_links.identity_base`. Equal to `value` for every kind but [`IdentityKind::Doi`],
    /// which is the only one that versions this way.
    ///
    /// Read by nothing that links: `bidslake link list` uses it to *report* two datasets whose
    /// DOIs differ only in version, precisely because the views will not join them. Two
    /// versions of one dataset are deliberately different identities; `--source-dataset` is how
    /// a user overrides that.
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
    use proptest::prelude::*;
    use rstest::rstest;

    /// Every reference form, and the `(value, kind)` it canonicalizes to.
    ///
    /// These were eight functions checking different subsets — three asserted only `value`,
    /// one only `kind`, and the collision cases asserted two calls were *equal to each other*,
    /// which holds just as well when both are wrong. Pinning the exact value per case is the
    /// stronger statement: the three DOI spellings collide because all three are named here as
    /// the same string, not because they agree.
    #[rstest]
    // The load-bearing case: MRIQC declares the bare DOI, fMRIPrep the URL form, and the
    // `doi:` prefix is the third spelling of the one identity.
    #[case::bare_doi(
        "10.18112/openneuro.ds001761.v2.0.1",
        "doi:10.18112/openneuro.ds001761.v2.0.1",
        IdentityKind::Doi
    )]
    #[case::url_doi(
        "https://doi.org/10.18112/openneuro.ds001761.v2.0.1",
        "doi:10.18112/openneuro.ds001761.v2.0.1",
        IdentityKind::Doi
    )]
    #[case::prefixed_doi(
        "doi:10.18112/openneuro.ds001761.v2.0.1",
        "doi:10.18112/openneuro.ds001761.v2.0.1",
        IdentityKind::Doi
    )]
    // DOIs are case-insensitive per the Handle spec, so the whole thing folds down.
    #[case::case_folded_doi(
        "https://doi.org/10.18112/OpenNeuro.DS001761.V2.0.1",
        "doi:10.18112/openneuro.ds001761.v2.0.1",
        IdentityKind::Doi
    )]
    #[case::bare_token(
        "ds001761-fmriprep",
        "dataset:ds001761-fmriprep",
        IdentityKind::Dataset
    )]
    // Dataset ids are case-sensitive `Name`s, unlike DOIs.
    #[case::explicit_dataset_prefix(
        "dataset:MyStudy_Derivative",
        "dataset:MyStudy_Derivative",
        IdentityKind::Dataset
    )]
    // The host folds, the path does not.
    #[case::url(
        "https://GitHub.com/Nipreps/MRIQC/",
        "https://github.com/Nipreps/MRIQC",
        IdentityKind::Url
    )]
    #[case::absolute_path("/data/ds001761/", "file:///data/ds001761", IdentityKind::File)]
    #[case::s3_uri("s3://bucket/prefix/", "s3://bucket/prefix", IdentityKind::File)]
    #[case::file_uri("file:///data/ds/", "file:///data/ds", IdentityKind::File)]
    // An unknown scheme is kept opaque rather than mis-typed as a URL...
    #[case::unknown_scheme("ftp://host/x", "opaque:ftp://host/x", IdentityKind::Opaque)]
    // ...and free text is kept verbatim, so two identical strings still collide — including
    // when one of them arrives padded.
    #[case::free_text("some free text", "opaque:some free text", IdentityKind::Opaque)]
    #[case::padded_free_text("  some free text ", "opaque:some free text", IdentityKind::Opaque)]
    fn canonicalize_maps_a_reference_to_one_identity(
        #[case] declared: &str,
        #[case] value: &str,
        #[case] kind: IdentityKind,
    ) {
        let id = canonicalize(declared);

        assert_eq!((id.value.as_str(), id.kind), (value, kind));
    }

    /// Two versions of one dataset are different identities that share a base — the
    /// relationship between the two calls is the behaviour, so both are made here.
    #[test]
    fn doi_base_strips_version() {
        let id = canonicalize("10.18112/openneuro.ds001761.v2.0.1");
        let older = canonicalize("10.18112/openneuro.ds001761.v2.0.0");

        assert_eq!(id.base, "doi:10.18112/openneuro.ds001761");
        assert_eq!(id.base, older.base);
        assert_ne!(id.value, older.value);
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

    /// A reference that stands on its own, in any of the shapes that can.
    ///
    /// `label` is deliberately not restricted to what OpenNeuro emits: these strings come
    /// from a dataset's own `DatasetLinks`/`Sources`, so the parser meets whatever an author
    /// wrote.
    fn an_absolute_reference() -> impl Strategy<Value = String> {
        let label = "[0-9A-Za-z._-]{1,8}";
        prop_oneof![
            label.prop_map(|s| format!("dataset:{s}")),
            (label, label).prop_map(|(a, b)| format!("10.{a}/{b}")),
            (label, label).prop_map(|(a, b)| format!("https://doi.org/10.{a}/{b}")),
            (label, label).prop_map(|(a, b)| format!("doi:10.{a}/{b}")),
            (label, label).prop_map(|(a, b)| format!("s3://{a}/{b}")),
            (label, label).prop_map(|(a, b)| format!("file:///{a}/{b}")),
            (label, label).prop_map(|(a, b)| format!("/{a}/{b}")),
            (label, label).prop_map(|(a, b)| format!("https://{a}.org/{b}")),
        ]
    }

    /// The one kind that is not a fixed point, pinned so the boundary is documented rather
    /// than merely excluded from the property above.
    ///
    /// An opaque value is a *result*, not a reference. Re-canonicalizing `"opaque:"` does not
    /// merely double the prefix — the bare token has no whitespace, so it is read as a dataset
    /// id and the kind flips `Opaque` -> `Dataset`, which is the part that would be hard to
    /// diagnose from a `dataset_links` row.
    #[test]
    fn an_opaque_value_is_not_itself_a_valid_reference() {
        let once = canonicalize("");

        let twice = canonicalize(&once.value);

        assert_eq!(
            (
                once.value.as_str(),
                once.kind,
                twice.value.as_str(),
                twice.kind
            ),
            (
                "opaque:",
                IdentityKind::Opaque,
                "dataset:opaque:",
                IdentityKind::Dataset
            )
        );
    }

    /// Every reference form, including the ones that reduce to [`IdentityKind::Opaque`]: an
    /// unknown scheme, free text with whitespace, and the empty string. These are what a
    /// `Sources` entry looks like when an author wrote prose instead of an identifier, so they
    /// are the *common* bad input rather than an exotic one.
    fn any_reference() -> impl Strategy<Value = String> {
        let label = "[0-9A-Za-z._-]{1,8}";
        prop_oneof![
            an_absolute_reference(),
            (label, label).prop_map(|(a, b)| format!("ftp://{a}/{b}")),
            (label, label).prop_map(|(a, b)| format!("{a} {b}")),
            label.prop_map(|s| s.to_string()),
            Just(String::new()),
        ]
    }

    proptest! {
        /// Every form that stands on its own is unaffected by the root, so the hand-written
        /// escape hatches keep working.
        ///
        /// Was seven fixed strings. The assertion was already the metamorphic relation —
        /// "joining it to a root changes nothing" — so the rows were seven points of a law
        /// this states directly, over generated hosts, DOI registrants and path segments.
        #[test]
        fn an_absolute_reference_passes_through_untouched(reference in an_absolute_reference()) {
            let joined = canonicalize_relative_to(&reference, "file:///data/study");

            prop_assert_eq!(joined.value, canonicalize(&reference).value);
        }

        /// Canonicalizing a canonical identity returns it unchanged.
        ///
        /// The point of the type: two spellings of one source collide because both reduce to
        /// the same `value`. That only holds if reducing is a fixed point — otherwise a value
        /// that has been through the function once is not comparable with one that has been
        /// through it twice, and which of those a `dataset_links` row holds depends on the
        /// path it took to get there.
        ///
        /// Over *every* reference form, not just the absolute ones — restricted to those, the
        /// property passes without ever reaching the one kind where it fails.
        ///
        /// `Opaque` is excluded, and the exclusion is the finding. `opaque:` is a prefix this
        /// function *adds* rather than recognizes, so feeding an opaque value back in prefixes
        /// it again and re-kinds it (see
        /// `an_opaque_value_is_not_itself_a_valid_reference`). That is a boundary, not a bug:
        /// no caller re-canonicalizes: every call site passes a string declared by a dataset,
        /// a CLI argument, or a root URI, never a stored `identity_value`. Stated here so the
        /// boundary is visible to whoever first thinks of round-tripping one.
        #[test]
        fn canonicalizing_an_identity_again_leaves_it_unchanged(reference in any_reference()) {
            let once = canonicalize(&reference);

            let twice = canonicalize(&once.value);

            prop_assert!(
                once.kind == IdentityKind::Opaque || twice == once,
                "{reference:?} canonicalized to {once:?} then {twice:?}"
            );
        }
    }
}
