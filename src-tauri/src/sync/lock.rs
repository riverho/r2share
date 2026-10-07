//! Per-mapping advisory lock so GUI and `sync run` don't double-upload.

use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

pub struct MappingLock {
    _file: File,
    pub path: PathBuf,
}

fn lock_file_path(data_dir: &Path, mapping_path: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(mapping_path.as_bytes());
    let digest = hex::encode(hasher.finalize());
    data_dir.join("sync-locks").join(format!("{digest}.lock"))
}

/// Try to acquire an exclusive lock for this mapping. Returns `None` if another
/// process already owns syncing for it.
pub fn try_acquire_mapping_lock(
    data_dir: &Path,
    mapping_path: &str,
) -> Result<Option<MappingLock>, String> {
    let path = lock_file_path(data_dir, mapping_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create sync-locks: {e}"))?;
    }
    let mut opts = OpenOptions::new();
    opts.create(true).read(true).write(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let file = opts
        .open(&path)
        .map_err(|e| format!("open sync lock {}: {e}", path.display()))?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(MappingLock { _file: file, path })),
        Err(_) => Ok(None),
    }
}
