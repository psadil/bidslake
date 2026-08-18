//! A minimal, valid NIfTI-1 volume.
//!
//! The original trick was to create imaging files empty, on the grounds that indexing never
//! opens one. That is still true of *indexing* and it is false of everything else: an empty
//! `.nii.gz` is four separate validator errors — `EMPTY_FILE`, `GZ_NOT_GZIPPED`,
//! `NIFTI_HEADER_UNREADABLE`, `NIFTI_TOO_SMALL` — so a tree built that way can never meet the
//! bar this crate sets itself, which is that a generated raw dataset validates cleanly.
//!
//! So imaging files carry a real header and eight voxels instead of nothing. The cost is about
//! a hundred and thirty gzipped bytes each: a hundred-thousand-file tree grows by roughly ten
//! megabytes, which buys a tree that is *valid* rather than merely well-named. Nothing reads the
//! voxels; the point is only that the header parses and the fields the schema's checks reference
//! are populated.

use std::io::Write;

use flate2::Compression;
use flate2::write::GzEncoder;

/// Header size, and the value the loader keys on at offset 0 to decide it is NIfTI-1 and
/// little-endian.
const HEADER_LEN: usize = 348;
/// Where voxels start: the header plus the four-byte extension flags.
const VOX_OFFSET: f32 = 352.0;
/// `DT_UINT8`, so one byte per voxel and no endianness question about the data.
const DATATYPE_UINT8: i16 = 2;
/// `NIFTI_UNITS_MM | NIFTI_UNITS_SEC`, which is what a BIDS volume declares.
const XYZT_UNITS_MM_SEC: u8 = 2 | 8;

/// A 2×2×2 volume, optionally with a time axis.
///
/// `volumes` of `None` gives a 3-D image (anat); `Some(n)` gives a 4-D one whose fourth pixdim is
/// `repetition_time`, which is the field a `RepetitionTime`-versus-header check compares against.
pub fn volume(volumes: Option<usize>, repetition_time: f64) -> Vec<u8> {
    let mut header = vec![0u8; HEADER_LEN];

    let put_i32 =
        |h: &mut Vec<u8>, at: usize, v: i32| h[at..at + 4].copy_from_slice(&v.to_le_bytes());
    let put_i16 =
        |h: &mut Vec<u8>, at: usize, v: i16| h[at..at + 2].copy_from_slice(&v.to_le_bytes());
    let put_f32 =
        |h: &mut Vec<u8>, at: usize, v: f32| h[at..at + 4].copy_from_slice(&v.to_le_bytes());

    put_i32(&mut header, 0, HEADER_LEN as i32);
    // `regular`, which every writer since ANALYZE sets to 'r'.
    header[38] = b'r';

    // dim[0] is the rank, dim[1..] the sizes; unused axes are 1, not 0.
    let rank: i16 = if volumes.is_some() { 4 } else { 3 };
    put_i16(&mut header, 40, rank);
    for axis in 1..=3 {
        put_i16(&mut header, 40 + axis * 2, 2);
    }
    put_i16(&mut header, 48, volumes.unwrap_or(1) as i16);
    for axis in 5..8 {
        put_i16(&mut header, 40 + axis * 2, 1);
    }

    put_i16(&mut header, 70, DATATYPE_UINT8);
    put_i16(&mut header, 72, 8); // bitpix

    // pixdim[0] is the qfac sign convention; 1.0 is the ordinary right-handed case.
    put_f32(&mut header, 76, 1.0);
    for axis in 1..=3 {
        put_f32(&mut header, 76 + axis * 4, 2.0);
    }
    put_f32(&mut header, 92, repetition_time as f32); // pixdim[4]
    for axis in 5..8 {
        put_f32(&mut header, 76 + axis * 4, 1.0);
    }

    put_f32(&mut header, 108, VOX_OFFSET);
    put_f32(&mut header, 112, 1.0); // scl_slope; 0.0 would mean "no scaling declared"
    header[123] = XYZT_UNITS_MM_SEC;

    put_i16(&mut header, 252, 1); // qform_code: NIFTI_XFORM_SCANNER_ANAT
    put_i16(&mut header, 254, 1); // sform_code
    put_f32(&mut header, 280, 2.0); // srow_x[0]
    put_f32(&mut header, 296, 2.0); // srow_y[1]
    put_f32(&mut header, 312, 2.0); // srow_z[2]

    header[344..348].copy_from_slice(b"n+1\0");

    let voxels = 2 * 2 * 2 * volumes.unwrap_or(1);
    let mut out = header;
    out.extend_from_slice(&[0u8; 4]); // extension flags: none
    out.extend(std::iter::repeat_n(0u8, voxels));
    out
}

/// The same volume, gzipped, for a `.nii.gz`.
///
/// **Stored, not deflated**, and that is not an oversight. `NIFTI_TOO_SMALL` compares the
/// **on-disk** byte count against 348 (`crates/bids-validator-rs/src/rules/errors/nifti.rs`),
/// and a header of mostly zeros deflates to about 130 bytes — so a perfectly valid, perfectly
/// readable volume is reported as too small to hold a header it demonstrably holds. Compressing
/// nothing sidesteps it at a cost of roughly 380 bytes a file.
///
/// Worth saying plainly that the check is the thing that is wrong here: file size is a proxy for
/// header size that a compressed container breaks, and the header it is standing in for has
/// already been read by the time the check runs.
pub fn gzipped_volume(volumes: Option<usize>, repetition_time: f64) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::none());
    encoder
        .write_all(&volume(volumes, repetition_time))
        .expect("writing to a Vec cannot fail");
    encoder
        .finish()
        .expect("finishing a Vec encoder cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bids_validator_rs::files::nifti::load_nifti_header_from_path;

    /// The law: the validator's own reader has to be able to parse what this writes. Two
    /// independent implementations — one writing, one reading, neither consulting the other.
    #[test]
    fn the_validators_reader_parses_a_generated_volume() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sub-01_task-rest_bold.nii.gz");
        std::fs::write(&path, gzipped_volume(Some(4), 0.8)).expect("writes");

        let header = load_nifti_header_from_path(&path);

        assert_eq!(
            header.map(|h| (h.dim[0], h.dim[4], h.datatype)),
            Some((4, 4, 2))
        );
    }

    /// A 3-D image declares rank 3, which is what separates an anat from a run.
    #[test]
    fn a_volumeless_image_is_three_dimensional() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sub-01_T1w.nii");
        std::fs::write(&path, volume(None, 1.0)).expect("writes");

        let header = load_nifti_header_from_path(&path);

        assert_eq!(header.map(|h| h.dim[0]), Some(3));
    }

    /// The whole justification for writing bytes at all rather than nothing: it stays cheap
    /// enough that a hundred thousand of them is megabytes, not gigabytes.
    #[test]
    fn a_gzipped_volume_stays_under_a_kilobyte() {
        let bytes = gzipped_volume(Some(8), 0.8).len();

        assert!(bytes < 1024, "a generated volume was {bytes} bytes");
    }

    /// `NIFTI_TOO_SMALL` sizes the file on disk, not the header inside it, so a deflated volume
    /// fails a check about a header it actually contains. Pinned here because the fix is one
    /// word (`Compression::none`) and would be an obvious thing to "optimize" away.
    #[test]
    fn a_gzipped_volume_is_at_least_the_minimum_header_size_on_disk() {
        let bytes = gzipped_volume(Some(4), 0.8).len();

        assert!(bytes >= 348, "a generated volume was {bytes} bytes on disk");
    }
}
