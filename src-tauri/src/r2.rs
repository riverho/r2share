use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::{
    config::{Builder as S3Builder, Region},
    error::DisplayErrorContext,
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart},
    Client,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::task::JoinSet;

use crate::config::Config;

/// Files larger than this are sent as a multipart upload.
const MULTIPART_THRESHOLD: u64 = 10 * 1024 * 1024;
/// Part size for multipart uploads.  R2 requires every part except the last
/// to be at least 5 MiB and all non-final parts to be the same size.
const PART_SIZE: u64 = 5 * 1024 * 1024;
/// Number of parts uploaded in parallel.
const PART_CONCURRENCY: usize = 4;
/// Attempts per part (on top of the SDK's own retries) before giving up.
const PART_ATTEMPTS: u32 = 4;

/// Called with `(bytes_sent, total_bytes)` as an upload progresses.
pub type ProgressFn = Arc<dyn Fn(u64, u64) + Send + Sync>;

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
    /// Large files are streamed from disk as a multipart upload.
    pub async fn upload_path(
        &self,
        path: &str,
        key: &str,
        on_progress: ProgressFn,
    ) -> Result<UploadResult, String> {
        let p = Path::new(path);
        let size = std::fs::metadata(p)
            .map_err(|e| format!("Cannot read file: {}", e))?
            .len();

        // Visible history name matches the object key's filename (conflict
        // suffixes included). Sync keys are nested (`folder/rel/file.ext`);
        // basename still matches the local file name.
        let display_name = Path::new(key)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(key)
            .to_string();

        let content_type = mime_guess::from_path(p).first_or_octet_stream().to_string();

        on_progress(0, size);
        if size > MULTIPART_THRESHOLD {
            self.multipart_upload(p, key, size, &content_type, &on_progress)
                .await?;
        } else {
            let data = std::fs::read(p).map_err(|e| format!("Cannot read file: {}", e))?;
            self.put_object(key, data, &content_type).await?;
            on_progress(size, size);
        }

        Ok(UploadResult {
            key: key.to_string(),
            url: self.public_url(key),
            display_name,
            size: size as i64,
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

    /// True when an object already exists at `key` (HeadObject).
    pub async fn object_exists(&self, key: &str) -> Result<bool, String> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(err) => {
                let typed_not_found = err
                    .as_service_error()
                    .map(|e| e.is_not_found())
                    .unwrap_or(false);
                // R2/S3 may surface a bare 404 without the typed NotFound variant.
                let status_404 = err
                    .raw_response()
                    .map(|r| r.status().as_u16() == 404)
                    .unwrap_or(false);
                if typed_not_found || status_404 {
                    Ok(false)
                } else {
                    Err(format!(
                        "Existence check failed: {}",
                        DisplayErrorContext(&err)
                    ))
                }
            }
        }
    }

    /// First free visible key for `desired_filename`, skipping `ignore_key`
    /// (used on rename so the object's current name stays available).
    pub async fn allocate_key(
        &self,
        desired_filename: &str,
        ignore_key: Option<&str>,
    ) -> Result<String, String> {
        let name = sanitize_file_name(desired_filename)?;
        let (stem, ext) = split_stem_ext(&name);
        let ext = ext.as_deref();

        for n in 0..=MAX_NAME_CONFLICTS {
            let key = keyed_filename(&stem, ext, if n == 0 { None } else { Some(n) });
            if ignore_key == Some(key.as_str()) {
                return Ok(key);
            }
            if !self.object_exists(&key).await? {
                return Ok(key);
            }
        }
        Err(format!(
            "Too many files named like \"{name}\" (limit {MAX_NAME_CONFLICTS}). Rename or delete older copies."
        ))
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
            .map_err(|e| format!("Upload failed: {}", DisplayErrorContext(&e)))
    }

    /// Create a multipart upload, send the parts, then complete it.
    /// The upload is aborted on failure so no orphaned parts are left billed in R2.
    async fn multipart_upload(
        &self,
        path: &Path,
        key: &str,
        size: u64,
        content_type: &str,
        on_progress: &ProgressFn,
    ) -> Result<(), String> {
        let created = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .content_type(content_type)
            .send()
            .await
            .map_err(|e| format!("Upload failed: {}", DisplayErrorContext(&e)))?;
        let upload_id = created
            .upload_id()
            .ok_or("Upload failed: R2 returned no upload id")?
            .to_string();

        let parts = match self
            .upload_parts(path, key, &upload_id, size, on_progress)
            .await
        {
            Ok(parts) => parts,
            Err(e) => {
                self.abort_multipart(key, &upload_id).await;
                return Err(e);
            }
        };

        let completed = self
            .client
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
            .await;
        if let Err(e) = completed {
            self.abort_multipart(key, &upload_id).await;
            return Err(format!("Upload failed: {}", DisplayErrorContext(&e)));
        }
        Ok(())
    }

    /// Upload every part, `PART_CONCURRENCY` at a time, reporting progress as
    /// each one finishes.  Returns the completed parts in part-number order.
    async fn upload_parts(
        &self,
        path: &Path,
        key: &str,
        upload_id: &str,
        size: u64,
        on_progress: &ProgressFn,
    ) -> Result<Vec<CompletedPart>, String> {
        let part_count = size.div_ceil(PART_SIZE);
        let path = Arc::new(path.to_path_buf());
        let mut tasks = JoinSet::new();
        let mut next = 0u64;
        let mut sent = 0u64;
        let mut parts = Vec::with_capacity(part_count as usize);

        loop {
            while tasks.len() < PART_CONCURRENCY && next < part_count {
                let offset = next * PART_SIZE;
                let len = PART_SIZE.min(size - offset);
                tasks.spawn(upload_part(
                    self.client.clone(),
                    self.bucket.clone(),
                    key.to_string(),
                    upload_id.to_string(),
                    path.clone(),
                    (next + 1) as i32,
                    offset,
                    len,
                ));
                next += 1;
            }

            // Dropping `tasks` on an early return cancels the remaining parts.
            let Some(joined) = tasks.join_next().await else {
                break;
            };
            let (part, len) = joined.map_err(|e| format!("Upload failed: {}", e))??;
            sent += len;
            on_progress(sent, size);
            parts.push(part);
        }

        parts.sort_by_key(|p| p.part_number());
        Ok(parts)
    }

    async fn abort_multipart(&self, key: &str, upload_id: &str) {
        let _ = self
            .client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await;
    }
}

/// Read one part from disk and upload it, retrying with backoff.
#[allow(clippy::too_many_arguments)]
async fn upload_part(
    client: Client,
    bucket: String,
    key: String,
    upload_id: String,
    path: Arc<PathBuf>,
    part_number: i32,
    offset: u64,
    len: u64,
) -> Result<(CompletedPart, u64), String> {
    let mut buf = vec![0u8; len as usize];
    let mut file = tokio::fs::File::open(path.as_ref())
        .await
        .map_err(|e| format!("Cannot read file: {}", e))?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| format!("Cannot read file: {}", e))?;
    file.read_exact(&mut buf)
        .await
        .map_err(|e| format!("Cannot read file: {}", e))?;

    let mut attempt = 1;
    loop {
        let result = client
            .upload_part()
            .bucket(&bucket)
            .key(&key)
            .upload_id(&upload_id)
            .part_number(part_number)
            .body(ByteStream::from(buf.clone()))
            .send()
            .await;

        match result {
            Ok(out) => {
                let part = CompletedPart::builder()
                    .part_number(part_number)
                    .set_e_tag(out.e_tag().map(str::to_string))
                    .build();
                return Ok((part, len));
            }
            Err(_) if attempt < PART_ATTEMPTS => {
                tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
                attempt += 1;
            }
            Err(e) => {
                return Err(format!(
                    "Upload failed on part {}: {}",
                    part_number,
                    DisplayErrorContext(&e)
                ))
            }
        }
    }
}

// ── key generation (manual uploads / rename) ─────────────────────────────────

/// Max `(N)` suffix before allocate gives up.
const MAX_NAME_CONFLICTS: u32 = 9999;

/// Pure allocator: desired display filename → first free object key.
/// Pattern: `name.ext` → `name (1).ext` → `name (2).ext` … up to 9999.
/// `exists` returns true when that key is already taken.
pub fn allocate_unique_key(
    desired_filename: &str,
    exists: impl Fn(&str) -> bool,
) -> Result<String, String> {
    let name = sanitize_file_name(desired_filename)?;
    let (stem, ext) = split_stem_ext(&name);
    let ext = ext.as_deref();

    for n in 0..=MAX_NAME_CONFLICTS {
        let key = keyed_filename(&stem, ext, if n == 0 { None } else { Some(n) });
        if !exists(&key) {
            return Ok(key);
        }
    }
    Err(format!(
        "Too many files named like \"{name}\" (limit {MAX_NAME_CONFLICTS}). Rename or delete older copies."
    ))
}

fn keyed_filename(stem: &str, ext: Option<&str>, conflict: Option<u32>) -> String {
    match (ext, conflict) {
        (Some(e), None) => format!("{stem}.{e}"),
        (Some(e), Some(n)) => format!("{stem} ({n}).{e}"),
        (None, None) => stem.to_string(),
        (None, Some(n)) => format!("{stem} ({n})"),
    }
}

fn split_stem_ext(filename: &str) -> (String, Option<String>) {
    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("file")
        .to_string();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_string());
    (stem, ext)
}

/// Sanitize a user/local filename for use as a visible R2 object key.
/// Allows spaces and parentheses so `name (1).ext` round-trips.
pub fn sanitize_file_name(display_name: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut last_was_dash = false;

    for ch in display_name.trim().chars() {
        let allowed =
            ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | ' ' | '(' | ')');
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn allocate_returns_desired_when_free() {
        let key = allocate_unique_key("photo.jpg", |_| false).unwrap();
        assert_eq!(key, "photo.jpg");
    }

    #[test]
    fn allocate_appends_numeric_suffix_on_conflict() {
        let taken: HashSet<&str> = ["photo.jpg", "photo (1).jpg"].into();
        let key = allocate_unique_key("photo.jpg", |k| taken.contains(k)).unwrap();
        assert_eq!(key, "photo (2).jpg");
    }

    #[test]
    fn allocate_preserves_spaces_and_parens_in_sanitize() {
        let key = allocate_unique_key("my photo (final).png", |_| false).unwrap();
        assert_eq!(key, "my photo (final).png");
    }

    #[test]
    fn allocate_handles_extensionless_names() {
        let taken: HashSet<&str> = ["README"].into();
        let key = allocate_unique_key("README", |k| taken.contains(k)).unwrap();
        assert_eq!(key, "README (1)");
    }

    #[test]
    fn allocate_uses_path_stem_and_extension() {
        let taken: HashSet<&str> = ["archive.tar.gz"].into();
        let key = allocate_unique_key("archive.tar.gz", |k| taken.contains(k)).unwrap();
        // Path::extension → "gz", file_stem → "archive.tar"
        assert_eq!(key, "archive.tar (1).gz");
    }

    #[test]
    fn allocate_errors_when_cap_exhausted() {
        let err = allocate_unique_key("x.bin", |_| true).unwrap_err();
        assert!(err.contains("9999"), "unexpected error: {err}");
    }

    #[test]
    fn sanitize_rejects_empty_after_cleanup() {
        assert!(sanitize_file_name("***").is_err());
        assert!(sanitize_file_name("   ").is_err());
    }

    #[test]
    fn sanitize_replaces_disallowed_chars() {
        assert_eq!(sanitize_file_name("a/b:c?.txt").unwrap(), "a-b-c-.txt");
    }
}
