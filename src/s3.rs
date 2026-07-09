use crate::fs::BidsFileSystem;
use anyhow::{Context, Result};
use futures::future;
use std::env;
use std::path::{Path, PathBuf};

/// S3 utilities for accessing OpenNeuro datasets
pub struct S3Client {
    client: aws_sdk_s3::Client,
    bucket: String,
    prefix: String,
}

impl S3Client {
    /// Create a generic S3 client from a bucket and prefix
    ///
    /// # Arguments
    /// * `bucket` - S3 bucket name
    /// * `prefix` - Object prefix (directory path)
    /// * `no_sign_request` - If true, use anonymous access (no AWS credentials)
    pub async fn new(bucket: &str, prefix: &str, no_sign_request: bool) -> Result<Self> {
        let region_provider = aws_config::meta::region::RegionProviderChain::first_try(
            env::var("AWS_REGION")
                .ok()
                .map(aws_sdk_s3::config::Region::new),
        )
        .or_default_provider()
        .or_else(aws_sdk_s3::config::Region::new("us-east-1"));

        let config_loader =
            aws_config::defaults(aws_config::BehaviorVersion::latest()).region(region_provider);

        // For public buckets or when explicitly requested, use anonymous access
        let config_loader = if no_sign_request {
            config_loader.no_credentials()
        } else {
            config_loader
        };

        let config = config_loader.load().await;
        let client = aws_sdk_s3::Client::new(&config);

        // Ensure prefix ends with / if not empty
        let prefix = if !prefix.is_empty() && !prefix.ends_with('/') {
            format!("{}/", prefix)
        } else {
            prefix.to_string()
        };

        Ok(Self {
            client,
            bucket: bucket.to_string(),
            prefix,
        })
    }
}

impl BidsFileSystem for S3Client {
    fn walk(&self) -> future::BoxFuture<'_, Result<Vec<PathBuf>>> {
        let bucket = self.bucket.clone();
        let prefix = self.prefix.clone();
        let client = self.client.clone();

        Box::pin(async move {
            let mut files = Vec::new();
            let mut paginator = client
                .list_objects_v2()
                .bucket(&bucket)
                .prefix(&prefix)
                .into_paginator()
                .send();

            while let Some(page) = paginator.next().await {
                let page = page.context("Failed to list S3 objects")?;

                if let Some(contents) = page.contents {
                    for object in contents {
                        if let Some(key) = object.key {
                            // Remove prefix to get relative path
                            let relative_key = if !prefix.is_empty() && key.starts_with(&prefix) {
                                key[prefix.len()..].to_string()
                            } else {
                                key.clone()
                            };

                            // Skip directories (keys ending in /)
                            if !relative_key.ends_with('/') {
                                files.push(PathBuf::from(relative_key));
                            }
                        }
                    }
                }
            }
            Ok(files)
        })
    }

    fn read_to_string(&self, path: &Path) -> future::BoxFuture<'_, Result<String>> {
        let bucket = self.bucket.clone();
        // Construct full key
        let key = format!("{}{}", self.prefix, path.to_string_lossy());
        let client = self.client.clone();

        Box::pin(async move {
            let response = client
                .get_object()
                .bucket(&bucket)
                .key(&key)
                .send()
                .await
                .context(format!("Failed to download {}", key))?;

            let data = response
                .body
                .collect()
                .await
                .context("Failed to read response body")?
                .into_bytes();

            let content = String::from_utf8(data.to_vec())
                .context("Failed to convert S3 object to string")?;

            Ok(content)
        })
    }

    fn root(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.prefix)
    }
}
