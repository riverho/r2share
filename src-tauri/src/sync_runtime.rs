//! Background folder-sync watcher owned by the GUI process.
//!
//! Per-mapping advisory locks under `{data_dir}/sync-locks/` mean that if the
//! user also runs `r2share-cli sync run`, the first process to lock a mapping
//! owns it; the other skips that mapping.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::config::{self, FolderMapping};
use crate::sync::{self, R2Uploader, StatusHandle, SyncOptions, Uploader};

pub struct SyncRuntime {
    pub status: StatusHandle,
    pub mappings: Arc<Mutex<Vec<FolderMapping>>>,
    pub stop: Arc<AtomicBool>,
    data_dir: PathBuf,
}

impl SyncRuntime {
    pub fn new(data_dir: PathBuf, mappings: Vec<FolderMapping>) -> Self {
        Self {
            status: StatusHandle::new(),
            mappings: Arc::new(Mutex::new(mappings)),
            stop: Arc::new(AtomicBool::new(false)),
            data_dir,
        }
    }

    pub fn reload_mappings(&self, mappings: Vec<FolderMapping>) {
        *self.mappings.lock().unwrap() = mappings;
    }

    pub fn start(self: &Arc<Self>) {
        let data_dir = self.data_dir.clone();
        let mappings = self.mappings.clone();
        let status = self.status.clone();
        let stop = self.stop.clone();

        tauri::async_runtime::spawn(async move {
            loop {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                // Refresh clients from config each outer cycle.
                let cfg = config::load(&data_dir);
                let mut clients: HashMap<String, Arc<dyn Uploader>> = HashMap::new();
                for m in cfg.folder_mappings.iter().filter(|m| !m.paused) {
                    if clients.contains_key(&m.vault) {
                        continue;
                    }
                    if let Some(v) = cfg.vault_by_name(&m.vault) {
                        let flat = v.to_flat();
                        if flat.is_configured() {
                            match R2Uploader::new(&flat).await {
                                Ok(u) => {
                                    clients
                                        .insert(m.vault.clone(), Arc::new(u) as Arc<dyn Uploader>);
                                }
                                Err(e) => {
                                    status.set_error(&format!("vault {}: {e}", m.vault));
                                }
                            }
                        }
                    }
                }
                let clients = Arc::new(clients);
                let factory: Arc<
                    dyn Fn(&str) -> Result<Arc<dyn Uploader>, String> + Send + Sync,
                > = Arc::new(move |vault_name: &str| {
                    clients
                        .get(vault_name)
                        .cloned()
                        .ok_or_else(|| format!("no uploader for vault {vault_name}"))
                });

                // Run watcher until stop; it returns when once=true or stop.
                // We pass a child stop that we can also trip to refresh clients.
                let child_stop = Arc::new(AtomicBool::new(false));
                let child = child_stop.clone();
                let parent = stop.clone();
                let refresh = tokio::spawn(async move {
                    // Refresh clients every 5 minutes by stopping the inner watcher.
                    for _ in 0..300 {
                        if parent.load(Ordering::SeqCst) {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                    child.store(true, Ordering::SeqCst);
                });

                sync::run_watcher(
                    data_dir.clone(),
                    factory,
                    mappings.clone(),
                    SyncOptions {
                        dry_run: false,
                        once: false,
                    },
                    status.clone(),
                    child_stop,
                )
                .await;
                refresh.abort();
                if stop.load(Ordering::SeqCst) {
                    break;
                }
            }
        });
    }

    pub fn stop_all(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}
