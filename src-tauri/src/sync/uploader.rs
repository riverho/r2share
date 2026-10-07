//! Uploader trait — real R2 adapter + in-memory mock for tests.
//! Intentionally has **no delete** method: sync never removes remote objects.

use async_trait::async_trait;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::config::Config;
use crate::r2::{ProgressFn, R2Client};

#[derive(Debug, Clone)]
pub struct UploadOutcome {
    pub key: String,
    pub url: String,
    pub size: i64,
    pub content_type: String,
    pub display_name: String,
}

#[async_trait]
pub trait Uploader: Send + Sync {
    async fn put_file(&self, local: &Path, remote_key: &str) -> Result<UploadOutcome, String>;
}

/// Production uploader wrapping [`R2Client`].
pub struct R2Uploader {
    client: R2Client,
}

impl R2Uploader {
    pub async fn new(config: &Config) -> Result<Self, String> {
        Ok(Self {
            client: R2Client::new(config).await?,
        })
    }
}

#[async_trait]
impl Uploader for R2Uploader {
    async fn put_file(&self, local: &Path, remote_key: &str) -> Result<UploadOutcome, String> {
        let path_str = local
            .to_str()
            .ok_or_else(|| format!("non-utf8 path: {}", local.display()))?;
        let noop: ProgressFn = Arc::new(|_, _| {});
        let r = self.client.upload_path(path_str, remote_key, noop).await?;
        Ok(UploadOutcome {
            key: r.key,
            url: r.url,
            size: r.size,
            content_type: r.content_type,
            display_name: r.display_name,
        })
    }
}

/// Records puts; panics / tracks if delete is ever requested (it can't be).
#[derive(Default)]
pub struct MockUploader {
    pub puts: Mutex<Vec<(String, String)>>, // (local, remote_key)
    pub delete_calls: Mutex<u32>,
}

impl MockUploader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put_keys(&self) -> Vec<String> {
        self.puts
            .lock()
            .unwrap()
            .iter()
            .map(|(_, k)| k.clone())
            .collect()
    }

    /// Test helper — sync must never call this. Exists only so tests can assert
    /// the counter stays at zero.
    pub fn record_delete(&self) {
        *self.delete_calls.lock().unwrap() += 1;
    }
}

#[async_trait]
impl Uploader for MockUploader {
    async fn put_file(&self, local: &Path, remote_key: &str) -> Result<UploadOutcome, String> {
        let meta = std::fs::metadata(local).map_err(|e| e.to_string())?;
        let display_name = local
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        let content_type = mime_guess::from_path(local)
            .first_or_octet_stream()
            .to_string();
        self.puts
            .lock()
            .unwrap()
            .push((local.display().to_string(), remote_key.to_string()));
        Ok(UploadOutcome {
            key: remote_key.to_string(),
            url: format!("https://mock.example/{}", remote_key),
            size: meta.len() as i64,
            content_type,
            display_name,
        })
    }
}
