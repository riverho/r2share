//! Scan + watch engine with debounce, stability wait, and skip-unchanged.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::config::FolderMapping;
use crate::db;

use super::ignore::should_ignore_path;
use super::lock::try_acquire_mapping_lock;
use super::remote_key;
use super::uploader::Uploader;

/// Quiet window after the last filesystem event before processing a batch.
pub const DEBOUNCE: Duration = Duration::from_millis(1500);
/// Stability: size must be unchanged across this gap before upload.
pub const STABLE_WAIT: Duration = Duration::from_millis(250);
/// How often to retry advisory locks held by another process (e.g. after GUI quits).
pub const LOCK_RETRY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Default)]
pub struct SyncOptions {
    pub dry_run: bool,
    /// When true, do one scan and exit (no watcher).
    pub once: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub scanned: usize,
    pub uploaded: usize,
    pub skipped: usize,
    pub ignored: usize,
    pub stale_marked: usize,
    pub would_upload: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SyncStatus {
    pub state: String, // idle | syncing | error
    pub syncing_n: usize,
    pub last_error: Option<String>,
    pub last_synced_at: Option<i64>,
}

/// Shared status visible to the GUI.
#[derive(Clone, Default)]
pub struct StatusHandle {
    inner: Arc<Mutex<SyncStatus>>,
}

impl StatusHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self) -> SyncStatus {
        self.inner.lock().unwrap().clone()
    }

    pub fn set_idle(&self) {
        let mut s = self.inner.lock().unwrap();
        s.state = "idle".into();
        s.syncing_n = 0;
    }

    pub fn set_syncing(&self, n: usize) {
        let mut s = self.inner.lock().unwrap();
        s.state = "syncing".into();
        s.syncing_n = n;
    }

    pub fn set_error(&self, msg: &str) {
        let mut s = self.inner.lock().unwrap();
        s.state = "error".into();
        s.last_error = Some(msg.to_string());
    }

    pub fn note_synced(&self) {
        let mut s = self.inner.lock().unwrap();
        s.last_synced_at = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        );
    }
}

fn file_mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn sha256_file(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65_536];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Wait until size is stable across two checks (or file vanishes).
pub fn wait_until_stable(path: &Path) -> Result<Option<std::fs::Metadata>, String> {
    let meta1 = match std::fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        Ok(_) => return Ok(None),
        Err(_) => return Ok(None),
    };
    let size1 = meta1.len();
    std::thread::sleep(STABLE_WAIT);
    let meta2 = match std::fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        _ => return Ok(None),
    };
    if meta2.len() != size1 {
        std::thread::sleep(STABLE_WAIT);
        let meta3 = match std::fs::metadata(path) {
            Ok(m) if m.is_file() => m,
            _ => return Ok(None),
        };
        if meta3.len() != meta2.len() {
            return Err(format!("file still changing: {}", path.display()));
        }
        return Ok(Some(meta3));
    }
    Ok(Some(meta2))
}

enum Decide {
    Skip,
    Upload { hash: String },
}

fn decide_upload(conn: &Connection, path: &Path, size: i64, mtime: i64) -> Result<Decide, String> {
    let path_s = path.display().to_string();
    match db::sync_get(conn, &path_s).map_err(|e| e.to_string())? {
        Some(row) if !row.stale && row.size == size && row.mtime == mtime => Ok(Decide::Skip),
        Some(row) => {
            let hash = sha256_file(path)?;
            if row.sha256.as_deref() == Some(hash.as_str()) && !row.stale {
                // Content same; refresh metadata in DB without re-upload.
                if let (Some(key), Some(url)) = (row.remote_key, row.url) {
                    let _ = db::sync_upsert_ok(
                        conn, &row.vault, &path_s, size, mtime, &hash, &key, &url,
                    );
                }
                Ok(Decide::Skip)
            } else {
                Ok(Decide::Upload { hash })
            }
        }
        None => Ok(Decide::Upload {
            hash: sha256_file(path)?,
        }),
    }
}

struct PendingUpload {
    path: PathBuf,
    key: String,
    hash: String,
    size: i64,
    mtime: i64,
}

fn collect_pending(
    conn: &Connection,
    root: &Path,
    vault: &str,
    path: &Path,
    opts: &SyncOptions,
    report: &mut SyncReport,
) -> Option<PendingUpload> {
    report.scanned += 1;
    if should_ignore_path(path) {
        report.ignored += 1;
        return None;
    }
    let meta = match wait_until_stable(path) {
        Ok(Some(m)) => m,
        Ok(None) => return None,
        Err(e) => {
            report.errors.push(e);
            return None;
        }
    };
    let size = meta.len() as i64;
    let mtime = file_mtime_secs(&meta);
    let decide = match decide_upload(conn, path, size, mtime) {
        Ok(v) => v,
        Err(e) => {
            report.errors.push(e);
            return None;
        }
    };
    let hash = match decide {
        Decide::Skip => {
            report.skipped += 1;
            return None;
        }
        Decide::Upload { hash } => hash,
    };
    let key = match remote_key(root, path) {
        Ok(k) => k,
        Err(e) => {
            report.errors.push(e);
            return None;
        }
    };
    if opts.dry_run {
        report
            .would_upload
            .push(format!("{} -> {}", path.display(), key));
        report.uploaded += 1;
        return None;
    }
    let _ = vault; // used by caller when recording
    Some(PendingUpload {
        path: path.to_path_buf(),
        key,
        hash,
        size,
        mtime,
    })
}

fn list_files(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .collect()
}

fn mark_missing_stale(conn: &Connection, root: &Path, vault: &str, report: &mut SyncReport) {
    let rows = match db::sync_list_for_vault(conn, vault) {
        Ok(r) => r,
        Err(e) => {
            report.errors.push(e.to_string());
            return;
        }
    };
    let root_s = root.display().to_string();
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    for row in rows {
        if row.stale {
            continue;
        }
        let p = PathBuf::from(&row.path);
        let under = p.starts_with(&root_canon)
            || p.starts_with(root)
            || row.path.starts_with(&root_s);
        if under && !p.is_file() {
            let _ = db::sync_mark_stale(conn, &row.path);
            report.stale_marked += 1;
        }
    }
}

/// Single scan of one mapping. DB work never crosses an `.await`.
pub async fn scan_mapping<U: Uploader + ?Sized>(
    data_dir: &Path,
    uploader: &U,
    mapping: &FolderMapping,
    opts: &SyncOptions,
) -> SyncReport {
    let mut report = SyncReport::default();
    let root = PathBuf::from(&mapping.path);
    if !root.is_dir() {
        report
            .errors
            .push(format!("not a directory: {}", root.display()));
        return report;
    }

    let pending: Vec<PendingUpload> = {
        let conn = match db::open(data_dir) {
            Ok(c) => c,
            Err(e) => {
                report.errors.push(e.to_string());
                return report;
            }
        };
        let mut list = Vec::new();
        for f in list_files(&root) {
            if let Some(p) = collect_pending(&conn, &root, &mapping.vault, &f, opts, &mut report) {
                list.push(p);
            }
        }
        mark_missing_stale(&conn, &root, &mapping.vault, &mut report);
        list
    }; // Connection dropped here — before any await

    for item in pending {
        match uploader.put_file(&item.path, &item.key).await {
            Ok(out) => {
                if let Ok(conn) = db::open(data_dir) {
                    let _ = db::sync_upsert_ok(
                        &conn,
                        &mapping.vault,
                        &item.path.display().to_string(),
                        item.size,
                        item.mtime,
                        &item.hash,
                        &out.key,
                        &out.url,
                    );
                    let _ = db::insert(
                        &conn,
                        &out.key,
                        &out.display_name,
                        out.size,
                        &out.content_type,
                        &out.url,
                        &mapping.vault,
                    );
                }
                report.uploaded += 1;
            }
            Err(e) => report.errors.push(format!("{}: {e}", item.path.display())),
        }
    }
    report
}

pub type UploaderFactory = std::sync::Arc<
    dyn Fn(&str) -> Result<std::sync::Arc<dyn Uploader>, String> + Send + Sync,
>;

/// Run sync once for all non-paused mappings that we can lock.
pub async fn run_once(
    data_dir: &Path,
    uploader_for: UploaderFactory,
    mappings: &[FolderMapping],
    opts: SyncOptions,
    status: Option<&StatusHandle>,
) -> SyncReport {
    let mut total = SyncReport::default();
    let active: Vec<_> = mappings.iter().filter(|m| !m.paused).cloned().collect();
    if let Some(s) = status {
        s.set_syncing(active.len());
    }
    for m in &active {
        let _lock = match try_acquire_mapping_lock(data_dir, &m.path) {
            Ok(Some(l)) => l,
            Ok(None) => {
                total.errors.push(format!(
                    "skipped {} — another process owns this mapping lock",
                    m.path
                ));
                continue;
            }
            Err(e) => {
                total.errors.push(e);
                continue;
            }
        };
        let uploader = match uploader_for(&m.vault) {
            Ok(u) => u,
            Err(e) => {
                total.errors.push(format!("{}: {e}", m.path));
                continue;
            }
        };
        let r = scan_mapping(data_dir, uploader.as_ref(), m, &opts).await;
        total.scanned += r.scanned;
        total.uploaded += r.uploaded;
        total.skipped += r.skipped;
        total.ignored += r.ignored;
        total.stale_marked += r.stale_marked;
        total.would_upload.extend(r.would_upload);
        total.errors.extend(r.errors);
    }
    if let Some(s) = status {
        if total.errors.is_empty() {
            s.set_idle();
            s.note_synced();
        } else if let Some(err) = total.errors.last() {
            s.set_error(err);
        }
    }
    total
}

/// Watcher for all mappings. Debounces events, then re-scans affected roots.
///
/// When `upload_cache` is provided, each batch checks `config.json` mtime and
/// reloads mappings/clients if it changed. Locked-out mappings are retried
/// every [`LOCK_RETRY`].
pub async fn run_watcher(
    data_dir: PathBuf,
    uploader_for: UploaderFactory,
    mappings: Arc<Mutex<Vec<FolderMapping>>>,
    opts: SyncOptions,
    status: StatusHandle,
    stop: Arc<AtomicBool>,
    upload_cache: Option<Arc<super::UploaderCache>>,
) {
    {
        let maps = mappings.lock().unwrap().clone();
        let _ = run_once(
            &data_dir,
            uploader_for.clone(),
            &maps,
            SyncOptions {
                dry_run: opts.dry_run,
                once: true,
            },
            Some(&status),
        )
        .await;
    }

    if opts.once || stop.load(Ordering::SeqCst) {
        return;
    }

    let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
    let mut watchers: HashMap<String, RecommendedWatcher> = HashMap::new();

    let refresh_watchers = |watchers: &mut HashMap<String, RecommendedWatcher>,
                            maps: &[FolderMapping],
                            tx: &std::sync::mpsc::Sender<PathBuf>,
                            status: &StatusHandle| {
        let wanted: HashSet<String> = maps
            .iter()
            .filter(|m| !m.paused)
            .map(|m| m.path.clone())
            .collect();
        watchers.retain(|k, _| wanted.contains(k));
        for m in maps.iter().filter(|m| !m.paused) {
            if watchers.contains_key(&m.path) {
                continue;
            }
            let root = PathBuf::from(&m.path);
            if !root.is_dir() {
                continue;
            }
            let txc = tx.clone();
            let root_c = root.clone();
            match RecommendedWatcher::new(
                move |res: Result<notify::Event, notify::Error>| {
                    if let Ok(ev) = res {
                        match ev.kind {
                            EventKind::Create(_)
                            | EventKind::Modify(_)
                            | EventKind::Remove(_)
                            | EventKind::Any => {
                                let _ = txc.send(root_c.clone());
                            }
                            _ => {}
                        }
                    }
                },
                notify::Config::default(),
            ) {
                Ok(mut w) => {
                    if w.watch(&root, RecursiveMode::Recursive).is_ok() {
                        watchers.insert(m.path.clone(), w);
                    }
                }
                Err(e) => status.set_error(&format!("watcher: {e}")),
            }
        }
    };

    {
        let maps = mappings.lock().unwrap().clone();
        refresh_watchers(&mut watchers, &maps, &tx, &status);
    }

    let mut pending: HashSet<PathBuf> = HashSet::new();
    let mut last_lock_retry = Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(root) => {
                pending.insert(root);
                loop {
                    match rx.recv_timeout(DEBOUNCE) {
                        Ok(r) => {
                            pending.insert(r);
                        }
                        Err(_) => break,
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Reload mappings if config.json changed (CLI edits).
                if let Some(cache) = &upload_cache {
                    match cache.prepare_batch() {
                        Ok(Some(maps)) => {
                            *mappings.lock().unwrap() = maps;
                        }
                        Ok(None) => {}
                        Err(e) => status.set_error(&e),
                    }
                }
                let maps = mappings.lock().unwrap().clone();
                refresh_watchers(&mut watchers, &maps, &tx, &status);
                // Periodically retry locks (e.g. GUI quit released them).
                if last_lock_retry.elapsed() >= LOCK_RETRY {
                    last_lock_retry = Instant::now();
                    for m in maps.iter().filter(|m| !m.paused) {
                        pending.insert(PathBuf::from(&m.path));
                    }
                } else {
                    continue;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if pending.is_empty() {
            continue;
        }
        let roots: Vec<PathBuf> = pending.drain().collect();

        if let Some(cache) = &upload_cache {
            match cache.prepare_batch() {
                Ok(Some(maps)) => {
                    *mappings.lock().unwrap() = maps;
                }
                Ok(None) => {}
                Err(e) => status.set_error(&e),
            }
        }

        let maps = mappings.lock().unwrap().clone();
        status.set_syncing(roots.len());

        for root in roots {
            let root_str = root.display().to_string();
            let Some(m) = maps
                .iter()
                .find(|m| m.path == root_str || root.starts_with(Path::new(&m.path)))
            else {
                continue;
            };
            if m.paused {
                continue;
            }
            let _lock = match try_acquire_mapping_lock(&data_dir, &m.path) {
                Ok(Some(l)) => l,
                Ok(None) => continue, // still owned elsewhere; LOCK_RETRY will try again
                Err(e) => {
                    status.set_error(&e);
                    continue;
                }
            };
            let uploader = match uploader_for(&m.vault) {
                Ok(u) => u,
                Err(e) => {
                    status.set_error(&e);
                    continue;
                }
            };
            let r = scan_mapping(&data_dir, uploader.as_ref(), m, &opts).await;
            if let Some(err) = r.errors.last() {
                status.set_error(err);
            }
        }
        status.set_idle();
        status.note_synced();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, FolderMapping, Vault};
    use crate::sync::MockUploader;
    use std::fs;

    fn tmp() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("r2share-engine-{n}"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn setup() -> (PathBuf, PathBuf, Connection, FolderMapping) {
        let data = tmp();
        let folder = data.join("sync-root");
        fs::create_dir_all(&folder).unwrap();
        let conn = db::open(&data).unwrap();
        let m = FolderMapping::new(folder.display().to_string(), "work");
        (data, folder, conn, m)
    }

    #[tokio::test]
    async fn uploads_new_file_and_skips_unchanged() {
        let (data, folder, _conn, m) = setup();
        let f = folder.join("hello.txt");
        fs::write(&f, b"hello").unwrap();
        let mock = MockUploader::new();
        let opts = SyncOptions {
            dry_run: false,
            once: true,
        };
        let r1 = scan_mapping(&data, &mock, &m, &opts).await;
        assert_eq!(r1.uploaded, 1, "{:?}", r1.errors);
        assert_eq!(mock.put_keys().len(), 1);
        assert!(mock.put_keys()[0].ends_with("/hello.txt"));

        let r2 = scan_mapping(&data, &mock, &m, &opts).await;
        assert_eq!(r2.uploaded, 0);
        assert!(r2.skipped >= 1);
        assert_eq!(mock.put_keys().len(), 1);
        assert_eq!(*mock.delete_calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn changed_file_reuploads_same_key() {
        let (data, folder, _conn, m) = setup();
        let f = folder.join("doc.txt");
        fs::write(&f, b"v1").unwrap();
        let mock = MockUploader::new();
        let opts = SyncOptions::default();
        let _ = scan_mapping(&data, &mock, &m, &opts).await;
        let key1 = mock.put_keys()[0].clone();

        std::thread::sleep(Duration::from_millis(20));
        fs::write(&f, b"v2-changed").unwrap();
        let _ = scan_mapping(&data, &mock, &m, &opts).await;
        let keys = mock.put_keys();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], keys[1]);
        assert_eq!(keys[0], key1);
        assert_eq!(*mock.delete_calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn local_delete_marks_stale_never_deletes_remote() {
        let (data, folder, _conn, m) = setup();
        let f = folder.join("gone.txt");
        fs::write(&f, b"x").unwrap();
        let mock = MockUploader::new();
        let opts = SyncOptions::default();
        let _ = scan_mapping(&data, &mock, &m, &opts).await;
        assert_eq!(*mock.delete_calls.lock().unwrap(), 0);
        fs::remove_file(&f).unwrap();
        let r = scan_mapping(&data, &mock, &m, &opts).await;
        assert!(r.stale_marked >= 1);
        // Uploader trait has no delete; counter untouched by engine.
        assert_eq!(*mock.delete_calls.lock().unwrap(), 0);
        assert_eq!(mock.put_keys().len(), 1); // no extra puts either
    }

    #[tokio::test]
    async fn ignores_temp_and_dotfiles() {
        let (data, folder, _conn, m) = setup();
        fs::write(folder.join(".secret"), b"x").unwrap();
        fs::write(folder.join("a.tmp"), b"x").unwrap();
        fs::write(folder.join("ok.txt"), b"x").unwrap();
        let mock = MockUploader::new();
        let _ = scan_mapping(&data, &mock, &m, &SyncOptions::default()).await;
        assert_eq!(mock.put_keys().len(), 1);
        assert!(mock.put_keys()[0].ends_with("/ok.txt"));
        assert_eq!(*mock.delete_calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn dry_run_lists_without_put() {
        let (data, folder, _conn, m) = setup();
        fs::write(folder.join("a.txt"), b"x").unwrap();
        let mock = MockUploader::new();
        let opts = SyncOptions {
            dry_run: true,
            once: true,
        };
        let r = scan_mapping(&data, &mock, &m, &opts).await;
        assert_eq!(r.would_upload.len(), 1);
        assert!(mock.put_keys().is_empty());
    }

    #[tokio::test]
    async fn wait_until_stable_accepts_steady_file() {
        let dir = tmp();
        let f = dir.join("s.txt");
        fs::write(&f, b"abc").unwrap();
        let meta = wait_until_stable(&f).unwrap().unwrap();
        assert_eq!(meta.len(), 3);
    }

    #[test]
    fn mapping_validation_requires_existing_vault() {
        let mut cfg = AppConfig::empty_v2();
        cfg.vaults.push(Vault::new("work"));
        cfg.default_vault = "work".into();
        cfg.folder_mappings
            .push(FolderMapping::new("/tmp/x", "missing"));
        assert!(cfg.validate().is_err());
        cfg.folder_mappings[0].vault = "work".into();
        assert!(cfg.validate().is_ok());
    }

    #[tokio::test]
    async fn advisory_lock_blocks_second_owner() {
        let data = tmp();
        let folder = data.join("root");
        fs::create_dir_all(&folder).unwrap();
        let _l1 = try_acquire_mapping_lock(&data, &folder.display().to_string())
            .unwrap()
            .expect("first lock");
        let l2 = try_acquire_mapping_lock(&data, &folder.display().to_string()).unwrap();
        assert!(l2.is_none());
    }
}
