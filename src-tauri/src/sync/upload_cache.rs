//! Shared R2 uploader cache with immediate invalidate + config.json mtime reload.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::config::{self, FolderMapping};
use crate::sync::{MockUploader, R2Uploader, Uploader};
use std::sync::Arc;

fn config_mtime(data_dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(config::config_path(data_dir))
        .ok()
        .and_then(|m| m.modified().ok())
}

struct Inner {
    clients: HashMap<String, Arc<dyn Uploader>>,
    config_mtime: Option<SystemTime>,
}

/// Thread-safe uploader cache. Call [`invalidate`](Self::invalidate) after GUI
/// vault edits; [`prepare_batch`](Self::prepare_batch) also reloads when
/// `config.json` mtime changes (CLI edits while the GUI is running).
pub struct UploaderCache {
    data_dir: PathBuf,
    dry_run: bool,
    inner: Mutex<Inner>,
}

impl UploaderCache {
    pub fn new(data_dir: PathBuf, dry_run: bool) -> Self {
        Self {
            data_dir,
            dry_run,
            inner: Mutex::new(Inner {
                clients: HashMap::new(),
                config_mtime: None,
            }),
        }
    }

    /// Drop all cached clients so the next get rebuilds from disk.
    pub fn invalidate(&self) {
        let mut g = self.inner.lock().unwrap();
        g.clients.clear();
        g.config_mtime = None;
    }

    /// If `config.json` mtime changed, clear clients and return the fresh
    /// folder_mappings (so the watcher can reload). Otherwise `None`.
    pub fn prepare_batch(&self) -> Result<Option<Vec<FolderMapping>>, String> {
        let mt = config_mtime(&self.data_dir);
        let mut g = self.inner.lock().unwrap();
        if g.config_mtime == mt && mt.is_some() {
            return Ok(None);
        }
        g.clients.clear();
        g.config_mtime = mt;
        drop(g);
        let cfg = config::load(&self.data_dir);
        Ok(Some(cfg.folder_mappings))
    }

    /// Return an uploader for `vault`, building it if needed.
    /// Must be called from an async context (uses `block_in_place` + current
    /// runtime handle when constructing `R2Uploader`).
    pub fn get(&self, vault_name: &str) -> Result<Arc<dyn Uploader>, String> {
        // Opportunistic mtime check (ignore mapping return — caller uses prepare_batch).
        let _ = self.prepare_batch()?;

        {
            let g = self.inner.lock().unwrap();
            if let Some(u) = g.clients.get(vault_name) {
                return Ok(u.clone());
            }
        }

        if self.dry_run {
            let u: Arc<dyn Uploader> = Arc::new(MockUploader::new());
            self.inner
                .lock()
                .unwrap()
                .clients
                .insert(vault_name.to_string(), u.clone());
            return Ok(u);
        }

        let cfg = config::load(&self.data_dir);
        let v = cfg
            .vault_by_name(vault_name)
            .ok_or_else(|| format!("vault not found: {vault_name}"))?;
        let flat = v.to_flat();
        if !flat.is_configured() {
            return Err(format!("vault {vault_name} is not fully configured"));
        }

        let handle = tokio::runtime::Handle::try_current()
            .map_err(|e| format!("no tokio runtime: {e}"))?;
        let u = tokio::task::block_in_place(|| {
            handle.block_on(async { R2Uploader::new(&flat).await })
        })?;
        let arc: Arc<dyn Uploader> = Arc::new(u);
        self.inner
            .lock()
            .unwrap()
            .clients
            .insert(vault_name.to_string(), arc.clone());
        Ok(arc)
    }
}
