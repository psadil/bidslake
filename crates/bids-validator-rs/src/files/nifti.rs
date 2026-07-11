//! NIfTI header reading.
//!
//! Extracts header fields that the BIDS schema checks reference, such as
//! dim, pixdim, xyzt_units, qform_code, sform_code, etc.
use crate::filetree::BidsFile;
use flate2::read::GzDecoder;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde::{Serialize, Serializer, ser::SerializeStruct};

#[derive(Debug, Clone, Default)]
pub struct NiftiHeader {
    pub dim_info: u8,
    pub dim: [i64; 8],
    pub datatype: i64,
    pub pixdim: [f64; 8],
    pub vox_offset: f64,
    pub scl_slope: f64,
    pub scl_inter: f64,
    pub cal_max: f64,
    pub cal_min: f64,
    pub xyzt_units: i32,
    pub qform_code: i32,
    pub sform_code: i32,
    pub quatern_b: f64,
    pub quatern_c: f64,
    pub quatern_d: f64,
    pub qoffset_x: f64,
    pub qoffset_y: f64,
    pub qoffset_z: f64,
    pub srow_x: [f64; 4],
    pub srow_y: [f64; 4],
    pub srow_z: [f64; 4],
}

impl Serialize for NiftiHeader {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("NiftiHeader", 13)?;
        state.serialize_field(
            "dim_info",
            &serde_json::json!({
                "freq": self.dim_info & 0x03,
                "phase": (self.dim_info >> 2) & 0x03,
                "slice": (self.dim_info >> 4) & 0x03,
            }),
        )?;
        state.serialize_field("dim", &self.dim.to_vec())?;
        state.serialize_field("datatype", &self.datatype)?;
        let rounded_pixdim: Vec<f64> = self
            .pixdim
            .iter()
            .map(|&x| (x * 1000.0).round() / 1000.0)
            .collect();
        state.serialize_field("pixdim", &rounded_pixdim)?;

        let shape = if self.dim[0] > 0 {
            self.dim[1..=self.dim[0] as usize].to_vec()
        } else {
            vec![]
        };
        state.serialize_field("shape", &shape)?;
        let voxel_sizes = if self.dim[0] > 0 {
            self.pixdim[1..=self.dim[0] as usize].to_vec()
        } else {
            vec![]
        };
        state.serialize_field("voxel_sizes", &voxel_sizes)?;

        state.serialize_field("vox_offset", &self.vox_offset)?;
        state.serialize_field("scl_slope", &self.scl_slope)?;
        state.serialize_field("scl_inter", &self.scl_inter)?;
        state.serialize_field("cal_max", &self.cal_max)?;
        state.serialize_field("cal_min", &self.cal_min)?;

        state.serialize_field(
            "xyzt_units",
            &serde_json::json!({
                "xyz": match self.xyzt_units & 0x07 {
                    1 => "meter",
                    2 => "mm",
                    3 => "um",
                    _ => "unknown",
                },
                "t": match (self.xyzt_units >> 3) & 0x07 {
                    1 => "sec",
                    2 => "msec",
                    3 => "usec",
                    _ => "unknown",
                }
            }),
        )?;
        state.serialize_field("qform_code", &self.qform_code)?;
        state.serialize_field("sform_code", &self.sform_code)?;
        state.serialize_field(
            "axis_codes",
            &axis_codes(
                self.qform_code as i64,
                self.sform_code as i64,
                self.quatern_b,
                self.quatern_c,
                self.quatern_d,
                &self.pixdim,
                &self.srow_x,
                &self.srow_y,
                &self.srow_z,
            ),
        )?;
        state.end()
    }
}

/// Read NIfTI header fields from a `.nii` or `.nii.gz` file,
/// returning them as a JSON Value for use in the expression evaluator.
pub async fn load_nifti_header(file: &BidsFile) -> Option<NiftiHeader> {
    let absolute_path = file.absolute_path.clone();
    tokio::task::spawn_blocking(move || load_nifti_header_from_path(&absolute_path))
        .await
        .ok()
        .flatten()
}

/// Read NIfTI header from a path.
pub fn load_nifti_header_from_path(path: &Path) -> Option<NiftiHeader> {
    let mut file = File::open(path).ok()?;

    // Read up to 540 bytes (max header size for NIfTI-2)
    let mut buffer = [0u8; 540];
    let bytes_read = if path.extension().and_then(|s| s.to_str()) == Some("gz") {
        let mut gz = GzDecoder::new(file);
        let mut total = 0;
        while total < 540 {
            match gz.read(&mut buffer[total..]) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        }
        total
    } else {
        let mut total = 0;
        while total < 540 {
            match file.read(&mut buffer[total..]) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        }
        total
    };

    if bytes_read < 348 {
        return None;
    }

    let sizeof_hdr = i32::from_le_bytes(buffer[0..4].try_into().unwrap());

    let (is_little_endian, is_nifti2) = if sizeof_hdr == 348 {
        (true, false)
    } else if sizeof_hdr == 348i32.swap_bytes() {
        (false, false)
    } else if sizeof_hdr == 540 {
        if bytes_read < 540 {
            return None;
        }
        (true, true)
    } else if sizeof_hdr == 540i32.swap_bytes() {
        if bytes_read < 540 {
            return None;
        }
        (false, true)
    } else {
        return None;
    };

    let read_i16 = |offset: usize| -> i16 {
        let bytes = buffer[offset..offset + 2].try_into().unwrap();
        if is_little_endian {
            i16::from_le_bytes(bytes)
        } else {
            i16::from_be_bytes(bytes)
        }
    };

    let read_i32 = |offset: usize| -> i32 {
        let bytes = buffer[offset..offset + 4].try_into().unwrap();
        if is_little_endian {
            i32::from_le_bytes(bytes)
        } else {
            i32::from_be_bytes(bytes)
        }
    };

    let read_i64 = |offset: usize| -> i64 {
        let bytes = buffer[offset..offset + 8].try_into().unwrap();
        if is_little_endian {
            i64::from_le_bytes(bytes)
        } else {
            i64::from_be_bytes(bytes)
        }
    };

    let read_f32 = |offset: usize| -> f32 {
        let bytes = buffer[offset..offset + 4].try_into().unwrap();
        if is_little_endian {
            f32::from_le_bytes(bytes)
        } else {
            f32::from_be_bytes(bytes)
        }
    };

    let read_f64 = |offset: usize| -> f64 {
        let bytes = buffer[offset..offset + 8].try_into().unwrap();
        if is_little_endian {
            f64::from_le_bytes(bytes)
        } else {
            f64::from_be_bytes(bytes)
        }
    };

    let hdr = if !is_nifti2 {
        let mut dim = [0i64; 8];
        for (i, d) in dim.iter_mut().enumerate() {
            *d = read_i16(40 + i * 2) as i64;
        }
        let mut pixdim = [0f64; 8];
        for (i, p) in pixdim.iter_mut().enumerate() {
            *p = read_f32(76 + i * 4) as f64;
        }

        NiftiHeader {
            dim_info: buffer[39],
            dim,
            datatype: read_i16(70) as i64,
            pixdim,
            vox_offset: read_f32(108) as f64,
            scl_slope: read_f32(112) as f64,
            scl_inter: read_f32(116) as f64,
            cal_max: read_f32(124) as f64,
            cal_min: read_f32(128) as f64,
            xyzt_units: buffer[123] as i32,
            qform_code: read_i16(252) as i32,
            sform_code: read_i16(254) as i32,
            quatern_b: read_f32(256) as f64,
            quatern_c: read_f32(260) as f64,
            quatern_d: read_f32(264) as f64,
            qoffset_x: read_f32(268) as f64,
            qoffset_y: read_f32(272) as f64,
            qoffset_z: read_f32(276) as f64,
            srow_x: [
                read_f32(280) as f64,
                read_f32(284) as f64,
                read_f32(288) as f64,
                read_f32(292) as f64,
            ],
            srow_y: [
                read_f32(296) as f64,
                read_f32(300) as f64,
                read_f32(304) as f64,
                read_f32(308) as f64,
            ],
            srow_z: [
                read_f32(312) as f64,
                read_f32(316) as f64,
                read_f32(320) as f64,
                read_f32(324) as f64,
            ],
        }
    } else {
        let mut dim = [0i64; 8];
        for (i, d) in dim.iter_mut().enumerate() {
            *d = read_i64(16 + i * 8);
        }
        let mut pixdim = [0f64; 8];
        for (i, p) in pixdim.iter_mut().enumerate() {
            *p = read_f64(104 + i * 8);
        }

        NiftiHeader {
            datatype: read_i16(12) as i64,
            dim,
            pixdim,
            vox_offset: read_i64(168) as f64,
            scl_slope: read_f64(176),
            scl_inter: read_f64(184),
            cal_max: read_f64(192),
            cal_min: read_f64(200),
            qform_code: read_i32(344),
            sform_code: read_i32(348),
            quatern_b: read_f64(352),
            quatern_c: read_f64(360),
            quatern_d: read_f64(368),
            qoffset_x: read_f64(376),
            qoffset_y: read_f64(384),
            qoffset_z: read_f64(392),
            srow_x: [read_f64(400), read_f64(408), read_f64(416), read_f64(424)],
            srow_y: [read_f64(432), read_f64(440), read_f64(448), read_f64(456)],
            srow_z: [read_f64(464), read_f64(472), read_f64(480), read_f64(488)],
            xyzt_units: read_i32(500),
            dim_info: buffer[524],
        }
    };

    Some(hdr)
}

#[allow(clippy::too_many_arguments)]
fn axis_codes(
    qform_code: i64,
    sform_code: i64,
    quatern_b: f64,
    quatern_c: f64,
    quatern_d: f64,
    pixdim: &[f64; 8],
    srow_x: &[f64; 4],
    srow_y: &[f64; 4],
    srow_z: &[f64; 4],
) -> Option<Vec<String>> {
    let mut affine = [[0.0f64; 4]; 4];
    if sform_code != 0 {
        affine[0] = *srow_x;
        affine[1] = *srow_y;
        affine[2] = *srow_z;
        affine[3] = [0.0, 0.0, 0.0, 1.0];
    } else if qform_code != 0 {
        let b = quatern_b;
        let c = quatern_c;
        let d = quatern_d;
        let a = (1.0_f64 - (b * b + c * c + d * d)).max(0.0).sqrt();

        let xd = pixdim[1];
        let yd = pixdim[2];
        let mut zd = pixdim[3];
        if pixdim[0] < 0.0 {
            zd = -zd;
        }

        affine[0][0] = (a * a + b * b - c * c - d * d) * xd;
        affine[0][1] = 2.0 * (b * c - a * d) * yd;
        affine[0][2] = 2.0 * (b * d + a * c) * zd;
        affine[1][0] = 2.0 * (b * c + a * d) * xd;
        affine[1][1] = (a * a + c * c - b * b - d * d) * yd;
        affine[1][2] = 2.0 * (c * d - a * b) * zd;
        affine[2][0] = 2.0 * (b * d - a * c) * xd;
        affine[2][1] = 2.0 * (c * d + a * b) * yd;
        affine[2][2] = (a * a + d * d - c * c - b * b) * zd;
    } else {
        // Fallback to pixdim
        affine[0][0] = pixdim[1];
        affine[1][1] = pixdim[2];
        affine[2][2] = pixdim[3];
    }

    for row in &affine {
        for val in row {
            if !val.is_finite() {
                return None;
            }
        }
    }

    let cos_x = [affine[0][0], affine[1][0], affine[2][0]];
    let cos_y = [affine[0][1], affine[1][1], affine[2][1]];
    let cos_z = [affine[0][2], affine[1][2], affine[2][2]];

    let norm = |v: &[f64; 3]| {
        let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        if len == 0.0 {
            [0.0, 0.0, 0.0]
        } else {
            [v[0] / len, v[1] / len, v[2] / len]
        }
    };
    let dot = |a: &[f64; 3], b: &[f64; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let scale = |v: &[f64; 3], s: f64| [v[0] * s, v[1] * s, v[2] * s];
    let sub = |a: &[f64; 3], b: &[f64; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    let add = |a: &[f64; 3], b: &[f64; 3]| [a[0] + b[0], a[1] + b[1], a[2] + b[2]];

    let orth_x = norm(&cos_x);
    let orth_y = norm(&sub(&cos_y, &scale(&orth_x, dot(&orth_x, &cos_y))));
    let orth_z = norm(&sub(
        &cos_z,
        &add(
            &scale(&orth_x, dot(&orth_x, &cos_z)),
            &scale(&orth_y, dot(&orth_y, &cos_z)),
        ),
    ));

    let basis = [orth_x, orth_y, orth_z];
    let mut magnitudes = [
        [orth_x[0].abs(), orth_x[1].abs(), orth_x[2].abs()],
        [orth_y[0].abs(), orth_y[1].abs(), orth_y[2].abs()],
        [orth_z[0].abs(), orth_z[1].abs(), orth_z[2].abs()],
    ];

    let max_mags = [
        magnitudes[0][0].max(magnitudes[0][1]).max(magnitudes[0][2]),
        magnitudes[1][0].max(magnitudes[1][1]).max(magnitudes[1][2]),
        magnitudes[2][0].max(magnitudes[2][1]).max(magnitudes[2][2]),
    ];

    let mut dims: Vec<usize> = vec![0, 1, 2];
    dims.sort_by(|&a, &b| {
        max_mags[b]
            .partial_cmp(&max_mags[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let codes = [["R", "L"], ["A", "P"], ["S", "I"]];
    let mut result = vec!["".to_string(), "".to_string(), "".to_string()];

    let arg_max = |arr: &[f64; 3]| {
        if arr[1] > arr[0] && arr[1] > arr[2] {
            1
        } else if arr[2] > arr[0] && arr[2] > arr[1] {
            2
        } else {
            0
        }
    };

    for dim in dims {
        let idx = arg_max(&magnitudes[dim]);
        for row in &mut magnitudes {
            row[idx] = 0.0;
        }
        result[dim] = codes[idx][if basis[dim][idx] > 0.0 { 0 } else { 1 }].to_string();
    }

    Some(result)
}
