use super::ErrorValidator;
use crate::context::{BidsContext, DatasetContext};

pub struct WrongNewLine;

#[async_trait::async_trait]
impl ErrorValidator for WrongNewLine {
    fn key(&self) -> &'static str {
        "WrongNewLine"
    }

    async fn validate_file(&self, context: &BidsContext, dataset: &DatasetContext) -> bool {
        let Some(file) = dataset.tree.find_file(&context.path) else {
            return false;
        };
        if let Ok(mut file_handle) = tokio::fs::File::open(&file.absolute_path).await {
            use tokio::io::AsyncReadExt;
            let mut buffer = [0; 8192];
            let mut has_r = false;
            let mut has_n = false;
            while let Ok(n) = file_handle.read(&mut buffer).await {
                if n == 0 {
                    break;
                }
                if !has_r && buffer[..n].contains(&b'\r') {
                    has_r = true;
                }
                if !has_n && buffer[..n].contains(&b'\n') {
                    has_n = true;
                }

                // If we found a newline character \n, we can immediately return false
                // because the file contains \n, so it's not strictly \r-only.
                if has_n {
                    return false;
                }
            }
            return has_r && !has_n;
        }
        false
    }
}
