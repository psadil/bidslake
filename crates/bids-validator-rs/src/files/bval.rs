use crate::filetree::BidsFile;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize, Clone, Default)]
pub struct BFileMeta {
    pub has_double_spaces: bool,
    pub has_non_numeric: bool,
    pub row_lengths: Vec<i32>,
}

pub async fn parse_bfile_meta_from_file(file: &BidsFile) -> Option<BFileMeta> {
    let mut has_double_spaces = false;
    let mut row_lengths = Vec::new();
    let mut has_non_numeric = false;
    let content = file.read_string().await.ok()?;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if line.contains("  ") || line.contains('\t') {
            has_double_spaces = true;
        }
        let mut n_cols = 0;
        for val_str in trimmed.split(' ') {
            let val_trimmed = val_str.trim();
            if !val_trimmed.is_empty() {
                n_cols += 1;
                if val_trimmed.parse::<f64>().is_err() {
                    has_non_numeric = true;
                }
            }
        }

        row_lengths.push(n_cols);
    }
    Some(BFileMeta {
        has_double_spaces,
        has_non_numeric,
        row_lengths,
    })
}

pub async fn parse_bval_bvec(file: &BidsFile) -> Option<Value> {
    let content = file.read_string().await.ok()?;
    let mut row_lengths = Vec::new();
    let mut n_rows = 0;
    let mut values = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        n_rows += 1;
        let mut n_cols = 0;

        for val_str in trimmed.split(' ') {
            let val_trimmed = val_str.trim();
            if !val_trimmed.is_empty() {
                n_cols += 1;
                if let Ok(val) = val_trimmed.parse::<f64>() {
                    values.push(Value::Number(
                        serde_json::Number::from_f64(val).unwrap_or(serde_json::Number::from(0)),
                    ));
                }
            }
        }
        row_lengths.push(n_cols);
    }

    let n_cols = row_lengths.first().copied().unwrap_or(0);

    Some(serde_json::json!({
        "path": file.path,
        "n_rows": n_rows,
        "n_cols": n_cols,
        "values": values,
    }))
}
