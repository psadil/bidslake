//! The shapes a real tree has and a uniform one does not.
//!
//! Every hazard here was observed on a 54 GB fMRIPrep + FreeSurfer + MELODIC/FIX tree, and every
//! one of them breaks the property the scale knobs exist to preserve: with hazards off the file
//! count is exactly linear in `--subjects`, so a superlinear ingest cost shows up as a bend in a
//! one-at-a-time sweep. [`Hazards::ragged`] alone makes `files(n)` a step function.
//!
//! So they are opt-in, named one at a time rather than behind a single boolean, and recorded in
//! the [`Manifest`](crate::Manifest). A benchmark number quoted without saying which were on is
//! not a number — and the reason to name them individually rather than take `--realistic` is
//! that a bench can then turn on exactly one and attribute the difference to it.

use std::path::Path;

use anyhow::{Context as _, Result};

use crate::{Claim, PlannedFile};

/// Which pathological shapes to include. Construct from [`Hazards::NONE`] and set fields, or
/// parse a comma-separated list with [`Hazards::parse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Hazards {
    /// `.surf.gii`, `.dtseries.nii` — extensions a naive `splitext` truncates to `.gii`/`.nii`.
    pub compound_ext: bool,
    /// `melodic_mix`, `eigenvalues_percent` — 30 of them in one MELODIC directory, all with no
    /// extension for the first-dot rule to find.
    pub extensionless: bool,
    /// The eight relative symlinks a `recon-all` `surf/` carries, whose `stat` size is the
    /// length of the target string rather than of any file.
    pub symlink: bool,
    /// `tmp/` and `trash/`, which `recon-all` leaves behind empty.
    pub empty_dir: bool,
    /// `fsaverage/mri.2mm/` — a directory whose name contains a dot.
    pub dotted_dir: bool,
    /// Subjects that are missing files their siblings have. The realistic case, and the one that
    /// makes a per-subject count meaningless.
    pub ragged: bool,
    /// A TSV whose values carry embedded double quotes, as fMRIPrep's `desc-aseg_dseg.tsv` does.
    pub quoted_tsv: bool,
    /// Zero-byte files — three `_desc-validation_bold.html` per subject in the observed tree.
    pub zero_byte: bool,
    /// Job artifacts sitting *beside* the dataset root rather than in it: a `.tar`, a `.err`, a
    /// `.out`, a `_rank-N.log`.
    pub loose_artifacts: bool,
}

impl Hazards {
    /// No hazards. The default, and never implied by any other flag.
    pub const NONE: Hazards = Hazards {
        compound_ext: false,
        extensionless: false,
        symlink: false,
        empty_dir: false,
        dotted_dir: false,
        ragged: false,
        quoted_tsv: false,
        zero_byte: false,
        loose_artifacts: false,
    };

    /// Every hazard.
    pub const ALL: Hazards = Hazards {
        compound_ext: true,
        extensionless: true,
        symlink: true,
        empty_dir: true,
        dotted_dir: true,
        ragged: true,
        quoted_tsv: true,
        zero_byte: true,
        loose_artifacts: true,
    };

    /// The name of every hazard, for `--help` and for error messages.
    pub const NAMES: &'static [&'static str] = &[
        "compound-ext",
        "extensionless",
        "symlink",
        "empty-dir",
        "dotted-dir",
        "ragged",
        "quoted-tsv",
        "zero-byte",
        "loose-artifacts",
    ];

    /// Parse a comma-separated list, or the word `all`.
    pub fn parse(spec: &str) -> Result<Hazards> {
        if spec.trim() == "all" {
            return Ok(Hazards::ALL);
        }
        let mut hazards = Hazards::NONE;
        for name in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match name {
                "compound-ext" => hazards.compound_ext = true,
                "extensionless" => hazards.extensionless = true,
                "symlink" => hazards.symlink = true,
                "empty-dir" => hazards.empty_dir = true,
                "dotted-dir" => hazards.dotted_dir = true,
                "ragged" => hazards.ragged = true,
                "quoted-tsv" => hazards.quoted_tsv = true,
                "zero-byte" => hazards.zero_byte = true,
                "loose-artifacts" => hazards.loose_artifacts = true,
                other => anyhow::bail!(
                    "unknown hazard {other:?}; known hazards are {} (or `all`)",
                    Hazards::NAMES.join(", ")
                ),
            }
        }
        Ok(hazards)
    }

    /// The enabled hazards' names, sorted, for the manifest line.
    pub fn enabled(&self) -> Vec<&'static str> {
        let flags = [
            (self.compound_ext, "compound-ext"),
            (self.extensionless, "extensionless"),
            (self.symlink, "symlink"),
            (self.empty_dir, "empty-dir"),
            (self.dotted_dir, "dotted-dir"),
            (self.ragged, "ragged"),
            (self.quoted_tsv, "quoted-tsv"),
            (self.zero_byte, "zero-byte"),
            (self.loose_artifacts, "loose-artifacts"),
        ];
        flags
            .into_iter()
            .filter_map(|(on, name)| on.then_some(name))
            .collect()
    }

    /// Whether any hazard is on.
    pub fn any(&self) -> bool {
        *self != Hazards::NONE
    }
}

/// Add (and, for `ragged`, remove) the planned files each enabled hazard contributes.
///
/// The subject a hazard attaches to is the *first* one in path order, deterministically, because
/// a benchmark tree has to be byte-identical across runs.
pub(crate) fn apply(files: &mut Vec<PlannedFile>, hazards: Hazards) {
    if !hazards.any() {
        return;
    }
    let subject = files
        .iter()
        .filter_map(|f| f.rel_path.split('/').next())
        .find(|s| s.starts_with("sub-"))
        .unwrap_or("sub-0001")
        .to_string();

    if hazards.compound_ext {
        for extension in [".surf.gii", ".dtseries.nii", ".shape.gii", ".label.gii"] {
            files.push(PlannedFile::empty(
                format!("{subject}/anat/{subject}_hemi-L_white{extension}"),
                Claim::Unclaimed,
            ));
        }
    }
    if hazards.extensionless {
        for name in ["melodic_mix", "eigenvalues_percent", "melodic_ICstats"] {
            files.push(PlannedFile::empty(
                format!("{subject}/{name}"),
                Claim::Unclaimed,
            ));
        }
    }
    if hazards.quoted_tsv {
        files.push(PlannedFile::text(
            "desc-aseg_dseg.tsv",
            Claim::Unclaimed,
            "index\tname\tcolor\n\
             0\t\"Unknown\"\t#000000\n\
             1\t\"Left-Cerebral-Exterior\"\t#4682b4\n",
        ));
    }
    if hazards.zero_byte {
        files.push(PlannedFile::empty(
            format!("{subject}/figures/{subject}_desc-validation_bold.html"),
            Claim::Unclaimed,
        ));
    }
    if hazards.dotted_dir {
        files.push(PlannedFile::empty(
            format!("{subject}/mri.2mm/T1.mgz"),
            Claim::Unclaimed,
        ));
    }
    if hazards.loose_artifacts {
        // Deliberately at the root of the *generated* directory, which for a nested producer is
        // beside the dataset root rather than inside it.
        for name in [
            "fmriprep_rank-0.log",
            "cdcd6f64-4dc6-4cda-9625-e4b1b6adba28-007.err",
            "cdcd6f64-4dc6-4cda-9625-e4b1b6adba28-007.out",
        ] {
            files.push(PlannedFile::empty(name.to_string(), Claim::Unclaimed));
        }
    }
    if hazards.ragged {
        // Drop the last subject's runs past the first, which is the observed shape: one subject
        // with the full `space-` cross-product, another with only native-space intermediates.
        let last = files
            .iter()
            .filter_map(|f| f.rel_path.split('/').next())
            .filter(|s| s.starts_with("sub-"))
            .max()
            .unwrap_or("")
            .to_string();
        if !last.is_empty() {
            files.retain(|f| !(f.rel_path.starts_with(&last) && f.rel_path.contains("_run-02")));
        }
    }
}

/// Create what a file list cannot express: symlinks, and directories with nothing in them.
pub(crate) fn materialize(root: &Path, hazards: Hazards) -> Result<()> {
    if hazards.empty_dir {
        for name in ["tmp", "trash"] {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
    }
    if hazards.symlink {
        let dir = root.join("surf");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let target = dir.join("lh.white.preaparc.H");
        std::fs::write(&target, b"").with_context(|| format!("writing {}", target.display()))?;
        let link = dir.join("lh.white.H");
        if !std::fs::symlink_metadata(&link).is_ok() {
            symlink_relative("lh.white.preaparc.H", &link)
                .with_context(|| format!("linking {}", link.display()))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn symlink_relative(target: &str, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_relative(target: &str, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_names_parse() {
        let spec = Hazards::NAMES.join(",");

        let hazards = Hazards::parse(&spec).expect("parses");

        assert_eq!(hazards, Hazards::ALL);
    }

    #[test]
    fn an_unknown_hazard_names_the_known_ones() {
        let error = Hazards::parse("nosuchhazard").expect_err("refused");

        assert!(
            error.to_string().contains("compound-ext"),
            "message was {error}"
        );
    }

    /// The empty spec is `NONE`, not an error, so `--hazards ''` from a script that built the
    /// list conditionally does the safe thing.
    #[test]
    fn an_empty_spec_enables_nothing() {
        assert_eq!(Hazards::parse("").expect("parses"), Hazards::NONE);
    }
}
