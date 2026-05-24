use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::{
    config::{Builder as S3Builder, Region},
    primitives::ByteStream,
    Client,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::config::Config;

/// Returned to the frontend after a successful upload.
#[derive(Debug, Serialize, Deserialize)]
pub struct UploadResult {
    pub key: String,
    pub url: String,
    pub display_name: String,
    /// Size in bytes.
    pub size: i64,
    pub content_type: String,
}

/// Thin wrapper around the AWS S3 client configured for Cloudflare R2.
pub struct R2Client {
    client: Client,
    bucket: String,
    public_url_base: String,
}

impl R2Client {
    /// Build an R2-compatible S3 client from the saved config.
    pub async fn new(config: &Config) -> Result<Self, String> {
        let creds = Credentials::new(
            &config.access_key_id,
            &config.secret_access_key,
            None,
            None,
            "r2share",
        );

        // R2 endpoint: https://<account_id>.r2.cloudflarestorage.com
        // R2 requires path-style addressing (force_path_style = true).
        // Region must be "auto" for R2.
        let s3_cfg = S3Builder::new()
            .credentials_provider(creds)
            .region(Region::new("auto"))
            .endpoint_url(format!(
                "https://{}.r2.cloudflarestorage.com",
                config.account_id
            ))
            .force_path_style(true)
            .behavior_version(BehaviorVersion::latest())
            .build();

        Ok(Self {
            client: Client::from_conf(s3_cfg),
            bucket: config.bucket.clone(),
            public_url_base: config.public_url_base.trim_end_matches('/').to_string(),
        })
    }

    /// Ping the bucket to verify credentials work.
    pub async fn test(&self) -> Result<(), String> {
        self.client
            .head_bucket()
            .bucket(&self.bucket)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| format!("Connection failed: {}", e))
    }

    /// Upload a file from a local path.  The caller supplies the R2 key.
    pub async fn upload_path(&self, path: &str, key: &str) -> Result<UploadResult, String> {
        let p = Path::new(path);
        let data = std::fs::read(p).map_err(|e| format!("Cannot read file: {}", e))?;
        let size = data.len() as i64;

        let display_name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();

        let content_type = mime_guess::from_path(p).first_or_octet_stream().to_string();

        self.put_object(key, data, &content_type).await?;

        Ok(UploadResult {
            key: key.to_string(),
            url: self.public_url(key),
            display_name,
            size,
            content_type,
        })
    }

    /// Upload raw bytes (e.g. clipboard image).
    pub async fn upload_bytes(
        &self,
        key: &str,
        data: Vec<u8>,
        content_type: &str,
        display_name: &str,
    ) -> Result<UploadResult, String> {
        let size = data.len() as i64;
        self.put_object(key, data, content_type).await?;
        Ok(UploadResult {
            key: key.to_string(),
            url: self.public_url(key),
            display_name: display_name.to_string(),
            size,
            content_type: content_type.to_string(),
        })
    }

    /// Delete an object by key.
    pub async fn delete(&self, key: &str) -> Result<(), String> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| format!("Delete failed: {}", e))
    }

    /// Copy an object to a new key.
    pub async fn copy(&self, old_key: &str, new_key: &str) -> Result<(), String> {
        self.client
            .copy_object()
            .bucket(&self.bucket)
            .copy_source(format!("{}/{}", self.bucket, old_key))
            .key(new_key)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| format!("Rename failed: {}", e))
    }

    pub fn public_url(&self, key: &str) -> String {
        format!("{}/{}", self.public_url_base, key)
    }

    // ── private ──────────────────────────────────────────────────────────────

    async fn put_object(&self, key: &str, data: Vec<u8>, content_type: &str) -> Result<(), String> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(data))
            .content_type(content_type)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| format!("Upload failed: {}", e))
    }
}

// ── key generation ────────────────────────────────────────────────────────────

/// Generate a collision-resistant R2 object key.
/// Pattern: `{unix_ms}-{6_random_alphanum}.{ext}`
pub fn generate_key(ext: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();

    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let suffix: String = (0..6)
        .map(|_| {
            let idx = (rand::random::<u8>() as usize) % CHARS.len();
            CHARS[idx] as char
        })
        .collect();

    format!("{}-{}.{}", ms, suffix, ext)
}

/// Generate a collision-resistant key that keeps the saved filename visible.
pub fn generate_named_key(display_name: &str) -> Result<String, String> {
    let name = sanitize_file_name(display_name)?;
    Ok(format!("{}-{}", key_prefix(), name))
}

fn key_prefix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();

    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let suffix: String = (0..6)
        .map(|_| {
            let idx = (rand::random::<u8>() as usize) % CHARS.len();
            CHARS[idx] as char
        })
        .collect();

    format!("{}-{}", ms, suffix)
}

fn sanitize_file_name(display_name: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut last_was_dash = false;

    for ch in display_name.trim().chars() {
        let allowed = ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-');
        let replacement = !allowed;

        if replacement {
            if !last_was_dash {
                out.push('-');
                last_was_dash = true;
            }
        } else if ch.is_ascii() {
            out.push(ch);
            last_was_dash = ch == '-';
        }
    }

    let name = out.trim_matches(['-', '.']).to_string();
    if name.is_empty() {
        Err("Enter a filename before saving.".to_string())
    } else {
        Ok(name)
    }
}
