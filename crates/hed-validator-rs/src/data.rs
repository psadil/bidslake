//! Columnar input types: BIDS JSON [`Sidecar`] definitions and [`TabularInput`] event
//! tables, with the JSON parsing that produces them.

use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum HedColumnDef {
    /// A single HED string containing a '#' placeholder to be replaced by the value in the events file
    Value(String),
    /// A dictionary mapping categorical values in the column to HED strings
    Categorical(HashMap<String, String>),
}

#[derive(Debug, Clone)]
pub struct Sidecar {
    /// Maps column names to their HED definitions
    pub columns: HashMap<String, HedColumnDef>,
}

impl Sidecar {
    pub fn parse(json: &Value) -> Result<Self, String> {
        let mut columns = HashMap::new();

        if let Some(obj) = json.as_object() {
            for (col_name, col_meta) in obj {
                if let Some(meta_obj) = col_meta.as_object()
                    && let Some(hed_val) = meta_obj.get("HED")
                {
                    if let Some(s) = hed_val.as_str() {
                        columns.insert(col_name.clone(), HedColumnDef::Value(s.to_string()));
                    } else if let Some(dict) = hed_val.as_object() {
                        let mut cat_map = HashMap::new();
                        for (cat_key, cat_val) in dict {
                            if let Some(cat_s) = cat_val.as_str() {
                                cat_map.insert(cat_key.clone(), cat_s.to_string());
                            } else {
                                return Err(format!(
                                    "Invalid HED string for categorical value '{}' in column '{}'",
                                    cat_key, col_name
                                ));
                            }
                        }
                        columns.insert(col_name.clone(), HedColumnDef::Categorical(cat_map));
                    } else {
                        return Err(format!("Invalid HED format for column '{}'", col_name));
                    }
                }
            }
        } else {
            return Err("Sidecar must be a JSON object".to_string());
        }

        Ok(Sidecar { columns })
    }
}

#[derive(Debug, Clone)]
pub struct TabularInput {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl TabularInput {
    pub fn parse(data: &[Vec<Value>]) -> Result<Self, String> {
        if data.is_empty() {
            return Ok(TabularInput {
                headers: Vec::new(),
                rows: Vec::new(),
            });
        }

        let mut headers = Vec::new();
        for h in &data[0] {
            if let Some(s) = h.as_str() {
                headers.push(s.to_string());
            } else {
                return Err("Header row contains non-string value".to_string());
            }
        }

        let mut rows = Vec::new();
        for row in data.iter().skip(1) {
            let mut row_data = Vec::new();
            for cell in row {
                if let Some(s) = cell.as_str() {
                    row_data.push(s.to_string());
                } else if let Some(n) = cell.as_f64() {
                    row_data.push(n.to_string());
                } else if cell.is_null() {
                    row_data.push("n/a".to_string());
                } else {
                    return Err(format!("Unsupported cell type: {:?}", cell));
                }
            }
            rows.push(row_data);
        }

        Ok(TabularInput { headers, rows })
    }
}
