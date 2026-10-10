use anyhow::{Context, Result};
use aws_credential_types::Credentials;
use aws_sdk_s3::{
    config::{BehaviorVersion, Region},
    error::SdkError,
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart},
    Client,
};
use std::path::Path;

use crate::config::R2Config;

/// Objects at or above this size are uploaded via multipart. A single
/// PutObject caps out at 5 GiB and the upload endpoint has no body limit,
/// so anything large has to go up in parts.
const MULTIPART_THRESHOLD: u64 = 128 * 1024 * 1024;
const PART_SIZE: u64 = 64 * 1024 * 1024;

/// R2 object storage. Keys are the SHA-256 content hash of the blob, so
/// identical content always lands at the same key and dedup is a property
/// of the key space rather than something we track.
#[derive(Clone)]
pub struct Storage {
    client: Client,
    bucket: String,
}

impl Storage {
    pub fn new(cfg: &R2Config) -> Self {
        let creds = Credentials::new(
            &cfg.access_key_id,
            &cfg.secret_access_key,
            None,
            None,
            "zigi-config",
        );
        let s3 = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(cfg.region.clone()))
            .endpoint_url(&cfg.endpoint)
            .credentials_provider(creds)
            // R2 has no virtual-hosted-style addressing for the S3 API endpoint.
            .force_path_style(true)
            .build();
        Self {
            client: Client::from_conf(s3),
            bucket: cfg.bucket.clone(),
        }
    }

    /// True when an object with this key already exists in the bucket.
    pub async fn exists(&self, key: &str) -> Result<bool> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(SdkError::ServiceError(e)) if e.err().is_not_found() => Ok(false),
            // R2 answers HeadObject on a missing key with a bare 404 that the
            // SDK cannot model as NotFound, so check the raw status too.
            Err(SdkError::ServiceError(e)) if e.raw().status().as_u16() == 404 => Ok(false),
            Err(e) => Err(anyhow::Error::new(e).context(format!("head {key}"))),
        }
    }

    /// Upload `path` under `key`, unless an object is already there. Returns
    /// true when bytes were actually written (a fresh object).
    pub async fn put_file_if_absent(
        &self,
        key: &str,
        path: &Path,
        content_type: Option<&str>,
    ) -> Result<bool> {
        if self.exists(key).await? {
            return Ok(false);
        }
        let len = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("stat {}", path.display()))?
            .len();
        if len >= MULTIPART_THRESHOLD {
            self.put_multipart(key, path, len, content_type).await?;
        } else {
            let body = ByteStream::from_path(path)
                .await
                .with_context(|| format!("read {}", path.display()))?;
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .body(body)
                .set_content_type(content_type.map(str::to_string))
                .send()
                .await
                .with_context(|| format!("put {key}"))?;
        }
        Ok(true)
    }

    async fn put_multipart(
        &self,
        key: &str,
        path: &Path,
        len: u64,
        content_type: Option<&str>,
    ) -> Result<()> {
        let create = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .set_content_type(content_type.map(str::to_string))
            .send()
            .await
            .with_context(|| format!("create multipart {key}"))?;
        let upload_id = create
            .upload_id()
            .context("multipart upload returned no upload id")?
            .to_string();

        match self
            .upload_parts(key, path, len, &upload_id)
            .await
            .context("uploading parts")
        {
            Ok(parts) => {
                self.client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .multipart_upload(
                        CompletedMultipartUpload::builder()
                            .set_parts(Some(parts))
                            .build(),
                    )
                    .send()
                    .await
                    .with_context(|| format!("complete multipart {key}"))?;
                Ok(())
            }
            Err(e) => {
                // Leave no half-finished upload behind to accrue storage cost.
                let _ = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .send()
                    .await;
                Err(e)
            }
        }
    }

    async fn upload_parts(
        &self,
        key: &str,
        path: &Path,
        len: u64,
        upload_id: &str,
    ) -> Result<Vec<CompletedPart>> {
        let mut parts = Vec::new();
        let mut offset = 0u64;
        let mut part_number = 1i32;
        while offset < len {
            let size = PART_SIZE.min(len - offset);
            let body = ByteStream::read_from()
                .path(path)
                .offset(offset)
                .length(aws_sdk_s3::primitives::Length::Exact(size))
                .build()
                .await
                .with_context(|| format!("read part {part_number} of {}", path.display()))?;
            let out = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(part_number)
                .body(body)
                .send()
                .await
                .with_context(|| format!("upload part {part_number} of {key}"))?;
            parts.push(
                CompletedPart::builder()
                    .part_number(part_number)
                    .set_e_tag(out.e_tag().map(str::to_string))
                    .build(),
            );
            offset += size;
            part_number += 1;
        }
        Ok(parts)
    }

    /// Fetch an object body for streaming to a client. `Ok(None)` means the
    /// key is not in the bucket.
    pub async fn get(&self, key: &str) -> Result<Option<ByteStream>> {
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(out) => Ok(Some(out.body)),
            Err(SdkError::ServiceError(e)) if e.err().is_no_such_key() => Ok(None),
            Err(SdkError::ServiceError(e)) if e.raw().status().as_u16() == 404 => Ok(None),
            Err(e) => Err(anyhow::Error::new(e).context(format!("get {key}"))),
        }
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("delete {key}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips a small object against a real bucket to check the pieces
    /// R2 is picky about: path-style addressing, SigV4 with region "auto",
    /// and whatever checksum behaviour the SDK defaults to.
    ///
    /// Ignored by default; needs live credentials:
    ///   R2_ENDPOINT=... R2_BUCKET=... R2_ACCESS_KEY_ID=... R2_SECRET_ACCESS_KEY=... \
    ///     cargo test --bin zigi-zip -- --ignored roundtrip
    #[tokio::test]
    #[ignore]
    async fn roundtrip() {
        let cfg = R2Config {
            endpoint: std::env::var("R2_ENDPOINT").unwrap(),
            bucket: std::env::var("R2_BUCKET").unwrap(),
            access_key_id: std::env::var("R2_ACCESS_KEY_ID").unwrap(),
            secret_access_key: std::env::var("R2_SECRET_ACCESS_KEY").unwrap(),
            region: "auto".into(),
        };
        let store = Storage::new(&cfg);
        let key = "zigi-selftest";
        let path = std::env::temp_dir().join(key);
        tokio::fs::write(&path, b"hello r2").await.unwrap();

        let _ = store.delete(key).await;
        assert!(!store.exists(key).await.unwrap(), "key should start absent");
        assert!(store
            .put_file_if_absent(key, &path, Some("text/plain"))
            .await
            .unwrap());
        assert!(store.exists(key).await.unwrap());
        assert!(
            !store.put_file_if_absent(key, &path, None).await.unwrap(),
            "second put should dedup"
        );

        let body = store.get(key).await.unwrap().expect("object present");
        let bytes = body.collect().await.unwrap().into_bytes();
        assert_eq!(&bytes[..], b"hello r2");

        store.delete(key).await.unwrap();
        assert!(!store.exists(key).await.unwrap());
        assert!(store.get(key).await.unwrap().is_none());
        let _ = tokio::fs::remove_file(&path).await;
    }
}
