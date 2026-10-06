//! Background folder-sync watcher owned by the GUI process.
//!
//! Per-mapping advisory locks under `{data_dir}/sync-locks/` mean that if the
//! user also runs `r2share-cli sync run`, the first process to lock a mapping
//! owns it; the other skips it. Locks are released when the owning process
//! exits; the other side retries every [`crate::sync::engine::LOCK_RETRY`].
//!
//! Uploader credentials are cached in [`UploaderCache`]. GUI vault edits call
//! [`SyncRuntime::invalidate_uploaders`] immediately; CLI edits are picked up
//! via `config.json` mtime checks before each sync batch.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::config::FolderMapping;
use crate::sync::{self, StatusHandle, SyncOptions, UploaderCache};

pub struct SyncRuntime {
    pub status: StatusHandle,
    pub mappings: Arc<Mutex<Vec<FolderMapping>>>,
    pub stop: Arc<AtomicBool>,
    pub upload_cache: Arc<UploaderCache>,
    data_dir: PathBuf,
}

impl SyncRuntime {
    pub fn new(data_dir: PathBuf, mappings: Vec<FolderMapping>) -> Self {
        Self {
            status: StatusHandle::new(),
            mappings: Arc::new(Mutex::new(mappings)),
            stop: Arc::new(AtomicBool::new(false)),
            upload_cache: Arc::new(UploaderCache::new(data_dir.clone(), false)),
            data_dir,
        }
    }

    pub fn reload_mappings(&self, mappings: Vec<FolderMapping>) {
        *self.mappings.lock().unwrap() = mappings;
    }

    /// Drop cached R2 clients immediately (call after vault save/edit/delete/import).
    pub fn invalidate_uploaders(&self) {
        self.upload_cache.invalidate();
    }

    /// Invalidate uploaders and refresh mappings from the in-memory config snapshot.
    pub fn on_config_changed(&self, mappings: Vec<FolderMapping>) {
        self.invalidate_uploaders();
        self.reload_mappings(mappings);
    }

    pub fn start(self: &Arc<Self>) {
        let data_dir = self.data_dir.clone();
        let mappings = self.mappings.clone();
        let status = self.status.clone();
        let stop = self.stop.clone();
        let cache = self.upload_cache.clone();

        tauri::async_runtime::spawn(async move {
            let factory: sync::UploaderFactory = {
                let cache = cache.clone();
                Arc::new(move |vault_name: &str| cache.get(vault_name))
            };

            sync::run_watcher(
                data_dir,
                factory,
                mappings,
                SyncOptions {
                    dry_run: false,
                    once: false,
                },
                status,
                stop,
                Some(cache),
            )
            .await;
        });
    }

    pub fn stop_all(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}
