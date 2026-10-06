//! One-way folder sync (local → R2). Never deletes remote objects.
//!
//! ## Who owns syncing
//! Each mapping is guarded by an advisory lock file under
//! `{data_dir}/sync-locks/<sha256(path)>.lock`. The first process that
//! acquires the lock (GUI background watcher **or** `r2share-cli sync run`)
//! owns that mapping; the other skips it. `sync rm` only removes the
//! config mapping — remote keys are left alone.
//!
//! ## Remote key layout
//! `<folder-basename>/<relative/path>` — stable keys so changed files
//! overwrite in place (overwrite ≠ delete).

mod engine;
mod ignore;
mod lock;
mod uploader;

pub use engine::{run_once, run_watcher, StatusHandle, SyncOptions, SyncReport, SyncStatus, UploaderFactory};
pub use ignore::{is_ignored, should_ignore_path};
pub use lock::{try_acquire_mapping_lock, MappingLock};
pub use uploader::{MockUploader, R2Uploader, UploadOutcome, Uploader};

use std::path::{Component, Path, PathBuf};

/// Build the stable remote key for a file under a synced root.
pub fn remote_key(root: &Path, file: &Path) -> Result<String, String> {
    let root = root
        .canonicalize()
        .unwrap_or_else(|_| root.to_path_buf());
    let file = if file.exists() {
        file.canonicalize().unwrap_or_else(|_| file.to_path_buf())
    } else {
        file.to_path_buf()
    };
    let rel = file
        .strip_prefix(&root)
        .map_err(|_| format!("{} is not under {}", file.display(), root.display()))?;
    let basename = root
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("cannot derive folder basename from {}", root.display()))?;
    let mut parts: Vec<String> = vec![basename.to_string()];
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => return Err(format!("unsafe relative path: {}", rel.display())),
        }
    }
    Ok(parts.join("/"))
}

/// Ensure the default suggestion folder exists (used when user enables sync).
pub fn ensure_default_folder() -> Result<PathBuf, String> {
    let p = PathBuf::from(crate::config::DEFAULT_SYNC_FOLDER);
    std::fs::create_dir_all(&p).map_err(|e| format!("create {}: {e}", p.display()))?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("r2share-sync-key-{n}"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn remote_key_preserves_relative_path() {
        let root = tmp();
        let nested = root.join("docs");
        fs::create_dir_all(&nested).unwrap();
        let file = nested.join("a.txt");
        fs::write(&file, b"hi").unwrap();
        let key = remote_key(&root, &file).unwrap();
        let base = root.file_name().unwrap().to_str().unwrap();
        assert_eq!(key, format!("{base}/docs/a.txt"));
    }
}
