use crate::files::bval::parse_bval_bvec;
use crate::files::tsv::load_tsv_columns;
use crate::filetree::{BidsFile, FileTree};
use crate::inheritance::read_sidecars;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Container for dynamically loaded BIDS associated files.
///
/// **Important Note on Serialization**:
/// The `#[serde(skip_serializing_if = "Option::is_none")]` annotations are required for
/// all fields in this struct. BIDS schema rules often check for the existence of an
/// association using the `"key" in object` expression (e.g., `"bval" in associations`).
/// If `Option::is_none` fields are not skipped during serialization, they will be serialized
/// into the context as `"key": null`. The schema expression evaluator will see the key
/// exists and evaluate `"key" in associations` to `true`, breaking missing file checks
/// like `DWI_MISSING_BVAL`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BidsAssociations {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<EventsAssociation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aslcontext: Option<AslContextAssociation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub m0scan: Option<M0ScanAssociation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub magnitude: Option<MagnitudeAssociation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub magnitude1: Option<Magnitude1Association>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bval: Option<BvalAssociation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bvec: Option<BvecAssociation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<ChannelsAssociation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub electrodes: Option<ElectrodesAssociation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordsystem: Option<CoordsystemAssociation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordsystems: Option<CoordsystemsAssociation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physio: Option<PhysioAssociation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub atlas_description: Option<AtlasDescriptionAssociation>,
}

impl BidsAssociations {
    pub async fn load(&mut self, assoc_name: &str, file: &BidsFile, tree: &FileTree) {
        match assoc_name {
            "events" => self.events = Some(EventsAssociation::from_file(file, tree).await),
            "aslcontext" => self.aslcontext = Some(AslContextAssociation::from_file(file).await),
            "m0scan" => self.m0scan = Some(M0ScanAssociation::from_file(file)),
            "magnitude" => self.magnitude = Some(MagnitudeAssociation::from_file(file)),
            "magnitude1" => self.magnitude1 = Some(Magnitude1Association::from_file(file)),
            "bval" => self.bval = Some(BvalAssociation::from_file(file).await),
            "bvec" => self.bvec = Some(BvecAssociation::from_file(file).await),
            "channels" => self.channels = Some(ChannelsAssociation::from_file(file).await),
            "electrodes" => self.electrodes = Some(ElectrodesAssociation::from_file(file)),
            "coordsystem" => self.coordsystem = Some(CoordsystemAssociation::from_file(file)),
            "physio" => self.physio = Some(PhysioAssociation::from_file(file, tree).await),
            "atlas_description" => {
                self.atlas_description = Some(AtlasDescriptionAssociation::from_file(file))
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsAssociation {
    pub path: String,
    pub onset: Option<Vec<f64>>,
    pub sidecar: Option<Value>,
}

impl EventsAssociation {
    pub async fn from_file(file: &BidsFile, tree: &FileTree) -> Self {
        let columns = load_tsv_columns(file).await.unwrap_or_default();
        let onset = columns
            .get("onset")
            .map(|c| c.iter().filter_map(|s| s.parse::<f64>().ok()).collect());
        let sidecar = read_sidecars(file, tree).await.0;

        let sidecar = if sidecar.is_null() {
            None
        } else {
            Some(sidecar)
        };

        Self {
            path: file.path.clone(),
            onset,
            sidecar,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AslContextAssociation {
    pub path: String,
    pub n_rows: i64,
    pub volume_type: Option<Vec<String>>,
}

impl AslContextAssociation {
    pub async fn from_file(file: &BidsFile) -> Self {
        let columns = load_tsv_columns(file).await.unwrap_or_default();
        let volume_type = columns.get("volume_type").cloned();
        let n_rows = volume_type.as_ref().map(|v| v.len() as i64).unwrap_or(0);

        Self {
            path: file.path.clone(),
            n_rows,
            volume_type,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct M0ScanAssociation {
    pub path: String,
}

impl M0ScanAssociation {
    pub fn from_file(file: &BidsFile) -> Self {
        Self {
            path: file.path.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MagnitudeAssociation {
    pub path: String,
}

impl MagnitudeAssociation {
    pub fn from_file(file: &BidsFile) -> Self {
        Self {
            path: file.path.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Magnitude1Association {
    pub path: String,
}

impl Magnitude1Association {
    pub fn from_file(file: &BidsFile) -> Self {
        Self {
            path: file.path.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BvalAssociation {
    pub path: String,
    pub n_cols: i64,
    pub n_rows: i64,
    pub values: Vec<f64>,
}

impl BvalAssociation {
    pub async fn from_file(file: &BidsFile) -> Self {
        if let Some(parsed) = parse_bval_bvec(file).await {
            let n_cols = parsed.get("n_cols").and_then(|v| v.as_i64()).unwrap_or(0);
            let n_rows = parsed.get("n_rows").and_then(|v| v.as_i64()).unwrap_or(0);

            let values = parsed
                .get("values")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_f64()).collect())
                .unwrap_or_default();

            Self {
                path: file.path.clone(),
                n_cols,
                n_rows,
                values,
            }
        } else {
            Self {
                path: file.path.clone(),
                n_cols: 0,
                n_rows: 0,
                values: Vec::new(),
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BvecAssociation {
    pub path: String,
    pub n_cols: i64,
    pub n_rows: i64,
}

impl BvecAssociation {
    pub async fn from_file(file: &BidsFile) -> Self {
        if let Some(parsed) = parse_bval_bvec(file).await {
            let n_cols = parsed.get("n_cols").and_then(|v| v.as_i64()).unwrap_or(0);
            let n_rows = parsed.get("n_rows").and_then(|v| v.as_i64()).unwrap_or(0);

            Self {
                path: file.path.clone(),
                n_cols,
                n_rows,
            }
        } else {
            Self {
                path: file.path.clone(),
                n_cols: 0,
                n_rows: 0,
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelsAssociation {
    pub path: String,
    pub r#type: Option<Vec<String>>,
    pub short_channel: Option<Vec<String>>,
    pub sampling_frequency: Option<Vec<String>>,
}

impl ChannelsAssociation {
    pub async fn from_file(file: &BidsFile) -> Self {
        let columns = load_tsv_columns(file).await.unwrap_or_default();
        Self {
            path: file.path.clone(),
            r#type: columns.get("type").cloned(),
            short_channel: columns.get("short_channel").cloned(),
            sampling_frequency: columns.get("sampling_frequency").cloned(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElectrodesAssociation {
    pub path: String,
}

impl ElectrodesAssociation {
    pub fn from_file(file: &BidsFile) -> Self {
        Self {
            path: file.path.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordsystemAssociation {
    pub path: String,
}

impl CoordsystemAssociation {
    pub fn from_file(file: &BidsFile) -> Self {
        Self {
            path: file.path.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordsystemsAssociation {
    pub paths: Vec<String>,
    pub spaces: Vec<String>,
    #[serde(rename = "ParentCoordinateSystems")]
    pub parent_coordinate_systems: Vec<String>,
}

impl CoordsystemsAssociation {
    /// Build from multiple coordsystem files, extracting the `space` entity
    /// from each filename to populate the `spaces` field.
    pub fn from_files(files: &[BidsFile]) -> Self {
        use crate::entities::read_entities;

        let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
        let spaces: Vec<String> = files
            .iter()
            .filter_map(|f| {
                let parts = read_entities(&f.name);
                parts.entities.get("space").cloned()
            })
            .collect();

        Self {
            paths,
            spaces,
            parent_coordinate_systems: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhysioAssociation {
    pub path: String,
    pub sidecar: Option<Value>,
}

impl PhysioAssociation {
    pub async fn from_file(file: &BidsFile, tree: &FileTree) -> Self {
        let sidecar = read_sidecars(file, tree).await.0;
        let sidecar = if sidecar.is_null() {
            None
        } else {
            Some(sidecar)
        };
        Self {
            path: file.path.clone(),
            sidecar,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtlasDescriptionAssociation {
    pub path: String,
}

impl AtlasDescriptionAssociation {
    pub fn from_file(file: &BidsFile) -> Self {
        Self {
            path: file.path.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn test_bids_associations_matches_schema() {
        // Load the build-time embedded schema.
        let schema_str = include_str!(concat!(env!("OUT_DIR"), "/schema.json"));
        let schema: Value = serde_json::from_str(schema_str).expect("Failed to parse schema");

        // Extract associations from schema
        let schema_associations =
            schema["meta"]["context"]["properties"]["associations"]["properties"]
                .as_object()
                .expect("Schema associations should be an object");

        // Create a dummy JSON object with all association keys
        let mut dummy_obj = serde_json::Map::new();
        for (key, _schema_def) in schema_associations.iter() {
            // "coordsystems" expects "paths" and "spaces", others expect "path"
            let mut assoc_val = serde_json::Map::new();
            if key == "coordsystems" {
                assoc_val.insert("paths".to_string(), serde_json::json!(["dummy"]));
                assoc_val.insert("spaces".to_string(), serde_json::json!(["dummy"]));
                assoc_val.insert(
                    "ParentCoordinateSystems".to_string(),
                    serde_json::json!(["dummy"]),
                );
            } else {
                assoc_val.insert("path".to_string(), serde_json::json!("dummy"));
                // Add required fields for specific associations based on schema
                if key == "aslcontext" || key == "bval" || key == "bvec" {
                    assoc_val.insert("n_rows".to_string(), serde_json::json!(0));
                }
                if key == "bval" || key == "bvec" {
                    assoc_val.insert("n_cols".to_string(), serde_json::json!(0));
                }
                if key == "bval" {
                    assoc_val.insert("values".to_string(), serde_json::json!([]));
                }
            }
            dummy_obj.insert(key.clone(), serde_json::Value::Object(assoc_val));
        }

        let dummy_json = serde_json::Value::Object(dummy_obj);

        // This should deserialize successfully and no fields should be None
        let parsed: BidsAssociations = serde_json::from_value(dummy_json.clone())
            .expect("Failed to deserialize BidsAssociations with all fields");

        let serialized = serde_json::to_value(&parsed).unwrap();
        let serialized_obj = serialized.as_object().unwrap();

        for (key, _schema_def) in schema_associations {
            assert!(
                serialized_obj.contains_key(key),
                "BidsAssociations is missing field '{}' from the schema",
                key
            );
        }

        // We can also verify that there are no extra fields
        for key in serialized_obj.keys() {
            assert!(
                schema_associations.contains_key(key),
                "BidsAssociations has extra field '{}' not in the schema",
                key
            );
        }
    }
}
