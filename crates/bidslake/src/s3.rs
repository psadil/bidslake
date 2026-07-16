//! S3-backed [`BidsFileSystem`] for ingesting datasets straight from object
//! storage (e.g. OpenNeuro's public bucket). Supports anonymous access.
//!
//! Object *listing* and *reads* go through the `aws-sdk-s3` client here; tabular
//! files are read by DuckDB directly over `s3://` via the **httpfs** extension
//! ([`configure_httpfs`]), since `read_csv` needs to open the path itself.

use crate::fs::BidsFileSystem;
use anyhow::{Context, Result};
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_s3::config::Region;
use futures::future;
use std::path::{Path, PathBuf};

/// Last-resort region when neither the environment, the active profile, nor the
/// bucket itself names one. [`S3Client::new`] asks the bucket where it really lives,
/// so this only has to be *a* valid endpoint to ask from.
const FALLBACK_REGION: &str = "us-east-1";

/// Whether S3 requests are signed with AWS credentials or sent anonymously
/// (public buckets like OpenNeuro's). A named type instead of a bare `bool` so
/// call sites read `SigningMode::Anonymous` rather than an opaque `true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigningMode {
    /// Sign requests with resolved AWS credentials.
    Signed,
    /// Send unsigned (anonymous) requests — required for public buckets.
    Anonymous,
}

impl SigningMode {
    /// Whether this is anonymous (unsigned) access.
    pub fn is_anonymous(self) -> bool {
        self == SigningMode::Anonymous
    }
}

/// Build an SDK config for `region`, unsigned when `signing` is anonymous.
async fn load_config(
    region: impl aws_config::meta::region::ProvideRegion + 'static,
    signing: SigningMode,
) -> aws_config::SdkConfig {
    let loader = aws_config::defaults(aws_config::BehaviorVersion::latest()).region(region);
    // For public buckets or when explicitly requested, use anonymous access.
    let loader = if signing.is_anonymous() {
        loader.no_credentials()
    } else {
        loader
    };
    loader.load().await
}

/// The bucket's own region, per the `x-amz-bucket-region` header, or `None` when the
/// client's current region already reaches it (or the bucket declines to say).
///
/// S3 returns that header *with* the `301 PermanentRedirect` it sends to a client aimed
/// at the wrong region (and with `403`/`404`), so a single HeadBucket resolves a bucket's
/// home region — no credentials, and no `GetBucketLocation` grant, which public buckets
/// typically deny anyway.
async fn bucket_region(client: &aws_sdk_s3::Client, bucket: &str) -> Option<String> {
    match client.head_bucket().bucket(bucket).send().await {
        Ok(_) => None, // reached it: the configured region is already right
        Err(err) => err
            .raw_response()
            .and_then(|resp| resp.headers().get("x-amz-bucket-region"))
            .map(str::to_string),
    }
}

/// S3 utilities for accessing OpenNeuro datasets
pub struct S3Client {
    client: aws_sdk_s3::Client,
    bucket: String,
    prefix: String,
    /// Resolved AWS region, reused to configure DuckDB's httpfs.
    region: String,
    /// Anonymous (unsigned) access — public buckets like OpenNeuro's.
    anonymous: bool,
}

impl S3Client {
    /// Create a generic S3 client from a bucket and prefix
    ///
    /// # Arguments
    /// * `bucket` - S3 bucket name
    /// * `prefix` - Object prefix (directory path)
    /// * `signing` - [`SigningMode::Anonymous`] for public buckets (no credentials)
    pub async fn new(bucket: &str, prefix: &str, signing: SigningMode) -> Result<Self> {
        // Start from the standard region chain — AWS_REGION, AWS_DEFAULT_REGION, the
        // active profile, IMDS — rather than AWS_REGION alone.
        let chain = RegionProviderChain::default_provider().or_else(Region::new(FALLBACK_REGION));
        let config = load_config(chain, signing).await;
        let mut region = config
            .region()
            .map(ToString::to_string)
            .unwrap_or_else(|| FALLBACK_REGION.to_string());
        let mut client = aws_sdk_s3::Client::new(&config);

        // Then let the bucket have the last word. Pointing a client at a bucket that
        // lives elsewhere fails with an opaque `PermanentRedirect`, which is a lousy
        // thing to hand someone indexing a public dataset they don't own (OpenNeuro's
        // buckets are us-west-2, while the chain's default is commonly us-east-1). One
        // HeadBucket resolves it, so `bidslake index -i s3://…` just works.
        if let Some(actual) = bucket_region(&client, bucket).await
            && actual != region
        {
            let config = load_config(Region::new(actual.clone()), signing).await;
            client = aws_sdk_s3::Client::new(&config);
            region = actual;
        }

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
            region,
            anonymous: signing.is_anonymous(),
        })
    }

    /// The AWS region httpfs should use.
    pub fn region(&self) -> &str {
        &self.region
    }

    /// Whether reads are anonymous (unsigned).
    pub fn anonymous(&self) -> bool {
        self.anonymous
    }
}

/// Enable DuckDB's httpfs extension on `conn` and point it at S3, so `read_csv`
/// can open `s3://` paths directly.
///
/// **Path-style addressing is required**: a dotted bucket name (e.g.
/// `openneuro.org`) produces a virtual-hosted URL (`openneuro.org.s3.…`) whose
/// host doesn't match the TLS wildcard cert, so reads fail with an SSL error.
/// With `anonymous`, an empty-credential secret makes every request unsigned (for
/// public buckets); otherwise DuckDB's default credential chain applies.
///
/// httpfs is a loadable extension — the first `INSTALL` fetches it from DuckDB's
/// extension repository, so this needs network access the first time.
pub fn configure_httpfs(conn: &duckdb::Connection, region: &str, anonymous: bool) -> Result<()> {
    conn.execute_batch("INSTALL httpfs; LOAD httpfs;")
        .context("install/load the DuckDB httpfs extension")?;
    conn.execute_batch(&format!(
        "SET s3_region='{region}'; SET s3_url_style='path'; SET s3_use_ssl=true;"
    ))
    .context("configure httpfs S3 settings")?;
    if anonymous {
        conn.execute_batch(&format!(
            "CREATE OR REPLACE SECRET bidslake_s3 \
             (TYPE S3, PROVIDER config, KEY_ID '', SECRET '', REGION '{region}');"
        ))
        .context("create anonymous S3 secret")?;
    }
    Ok(())
}

impl BidsFileSystem for S3Client {
    // `.bidsignore` is applied by the parser for the S3 backend (its object listing
    // does not consult it), so `apply_bidsignore` is honored there, not here.
    fn walk(
        &self,
        _pseudo_exts: &[String],
        _apply_bidsignore: bool,
    ) -> future::BoxFuture<'_, Result<Vec<PathBuf>>> {
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
                                key
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

    fn read_head(&self, path: &Path, max_bytes: usize) -> future::BoxFuture<'_, Result<String>> {
        let bucket = self.bucket.clone();
        let key = format!("{}{}", self.prefix, path.to_string_lossy());
        let client = self.client.clone();

        Box::pin(async move {
            // Ranged GET: fetch only the first `max_bytes`, so sniffing a TSV
            // header is a tiny request rather than a full object download.
            let range = format!("bytes=0-{}", max_bytes.saturating_sub(1));
            let response = client
                .get_object()
                .bucket(&bucket)
                .key(&key)
                .range(range)
                .send()
                .await
                .context(format!("Failed to range-download {}", key))?;

            let data = response
                .body
                .collect()
                .await
                .context("Failed to read response body")?
                .into_bytes();

            // The range can split a multi-byte char at the tail; lossy is fine
            // since only complete leading lines (the header) are used.
            Ok(String::from_utf8_lossy(&data).into_owned())
        })
    }

    fn read_csv_source(&self, path: &Path) -> future::BoxFuture<'_, Result<String>> {
        // DuckDB's httpfs reads `s3://` directly (see `configure_httpfs`), so hand
        // back the fully-qualified S3 URL for `read_csv` to open — no download.
        let url = format!(
            "s3://{}/{}{}",
            self.bucket,
            self.prefix,
            path.to_string_lossy()
        );
        Box::pin(async move { Ok(url) })
    }

    fn root(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.prefix)
    }
}
