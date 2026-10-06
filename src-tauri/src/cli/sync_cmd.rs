//! `r2share-cli sync` subcommands.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::args::SyncCmd;
use crate::config::{self, AppConfig, FolderMapping};
use crate::db;
use crate::sync::{self, StatusHandle, SyncOptions, UploaderCache};

fn load_cfg(data_dir: &Path) -> AppConfig {
    config::load(data_dir)
}

fn require_configured(config: &crate::config::Config) -> Result<(), String> {
    if config.is_configured() {
        Ok(())
    } else {
        Err("vault is not fully configured (need account_id, access_key_id, secret_access_key, public_url_base)".into())
    }
}

pub async fn cmd_sync(data_dir: &Path, cmd: SyncCmd) -> Result<(), String> {
    match cmd {
        SyncCmd::Add { path, vault } => sync_add(data_dir, path, vault),
        SyncCmd::Rm { path } => sync_rm(data_dir, path),
        SyncCmd::List { json } => sync_list(data_dir, json),
        SyncCmd::Status { json } => sync_status(data_dir, json),
        SyncCmd::Run { once, dry_run } => sync_run(data_dir, once, dry_run).await,
    }
}

fn sync_add(data_dir: &Path, path: PathBuf, vault: Option<String>) -> Result<(), String> {
    let mut cfg = load_cfg(data_dir);
    let vault_name = vault.unwrap_or_else(|| cfg.default_vault.clone());
    if cfg.vault_by_name(&vault_name).is_none() {
        return Err(format!("vault not found: {vault_name}"));
    }
    std::fs::create_dir_all(&path).map_err(|e| format!("create {}: {e}", path.display()))?;
    let canon = path
        .canonicalize()
        .map_err(|e| format!("canonicalize {}: {e}", path.display()))?;
    let path_s = canon.display().to_string();
    if cfg.folder_mappings.iter().any(|m| m.path == path_s) {
        return Err(format!("mapping already exists: {path_s}"));
    }
    cfg.folder_mappings
        .push(FolderMapping::new(path_s.clone(), vault_name.clone()));
    cfg.validate()?;
    config::save(data_dir, &cfg)?;
    println!("added {} → vault {}", path_s, vault_name);
    Ok(())
}

fn sync_rm(data_dir: &Path, path: PathBuf) -> Result<(), String> {
    let mut cfg = load_cfg(data_dir);
    let raw = path.display().to_string();
    let canon = path.canonicalize().ok().map(|p| p.display().to_string());
    let before = cfg.folder_mappings.len();
    cfg.folder_mappings.retain(|m| {
        let same = m.path == raw
            || canon.as_ref().is_some_and(|c| c == &m.path)
            || Path::new(&m.path)
                .canonicalize()
                .ok()
                .map(|p| p == path || canon.as_ref().is_some_and(|c| p.display().to_string() == *c))
                .unwrap_or(false);
        !same
    });
    if cfg.folder_mappings.len() == before {
        return Err(format!("mapping not found: {raw}"));
    }
    config::save(data_dir, &cfg)?;
    println!("removed mapping (remote objects kept)");
    Ok(())
}

fn sync_list(data_dir: &Path, json: bool) -> Result<(), String> {
    let cfg = load_cfg(data_dir);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&cfg.folder_mappings).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    if cfg.folder_mappings.is_empty() {
        println!("(no folder mappings)");
    }
    for m in &cfg.folder_mappings {
        let pause = if m.paused { " [paused]" } else { "" };
        println!("{}\t→ {}{}", m.path, m.vault, pause);
    }
    println!(
        "# tip: suggested default is {} → {}",
        config::DEFAULT_SYNC_FOLDER,
        cfg.default_vault
    );
    Ok(())
}

fn sync_status(data_dir: &Path, json: bool) -> Result<(), String> {
    let cfg = load_cfg(data_dir);
    let conn = db::open(data_dir).map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    for m in &cfg.folder_mappings {
        let states = db::sync_list_for_vault(&conn, &m.vault).unwrap_or_default();
        let under: Vec<_> = states
            .iter()
            .filter(|s| s.path.starts_with(&m.path))
            .collect();
        let active = under.iter().filter(|s| !s.stale).count();
        let stale = under.iter().filter(|s| s.stale).count();
        rows.push(serde_json::json!({
            "path": m.path,
            "vault": m.vault,
            "paused": m.paused,
            "active_files": active,
            "stale_files": stale,
        }));
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())?
        );
    } else {
        for r in &rows {
            println!(
                "{}\t→ {}\tactive={} stale={}{}",
                r["path"].as_str().unwrap_or(""),
                r["vault"].as_str().unwrap_or(""),
                r["active_files"],
                r["stale_files"],
                if r["paused"].as_bool().unwrap_or(false) {
                    " [paused]"
                } else {
                    ""
                }
            );
        }
    }
    Ok(())
}


async fn sync_run(data_dir: &Path, once: bool, dry_run: bool) -> Result<(), String> {
    let cfg = load_cfg(data_dir);
    if cfg.folder_mappings.is_empty() {
        return Err("no folder mappings — add one with: r2share-cli sync add <path>".into());
    }

    let data_dir_b = data_dir.to_path_buf();
    let cache = Arc::new(UploaderCache::new(data_dir_b.clone(), dry_run));
    if !dry_run {
        for m in cfg.folder_mappings.iter().filter(|m| !m.paused) {
            let v = cfg
                .vault_by_name(&m.vault)
                .ok_or_else(|| format!("vault not found: {}", m.vault))?;
            require_configured(&v.to_flat())?;
            let _ = cache.get(&m.vault)?;
        }
    }
    let uploader_for: sync::UploaderFactory = {
        let cache = cache.clone();
        Arc::new(move |vault_name: &str| cache.get(vault_name))
    };

    let opts = SyncOptions { dry_run, once };

    if once {
        let report =
            sync::run_once(&data_dir_b, uploader_for, &cfg.folder_mappings, opts, None).await;
        if dry_run {
            for line in &report.would_upload {
                println!("would upload: {line}");
            }
            println!(
                "dry-run: {} would upload, {} skipped, {} ignored, {} errors",
                report.uploaded,
                report.skipped,
                report.ignored,
                report.errors.len()
            );
        } else {
            println!(
                "done: {} uploaded, {} skipped, {} stale, {} errors",
                report.uploaded,
                report.skipped,
                report.stale_marked,
                report.errors.len()
            );
        }
        for e in &report.errors {
            eprintln!("error: {e}");
        }
        if !report.errors.is_empty() {
            return Err(format!("{} sync error(s)", report.errors.len()));
        }
        return Ok(());
    }

    let stop = Arc::new(AtomicBool::new(false));
    let stop_c = stop.clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        stop_c.store(true, Ordering::SeqCst);
    });

    println!(
        "watching {} mapping(s). Ownership: advisory lock per mapping under data_dir/sync-locks/ \
(GUI or this process — first wins; retries every 30s if locked). \
Stop with SIGINT/SIGTERM (stdin EOF is ignored).",
        cfg.folder_mappings.iter().filter(|m| !m.paused).count()
    );
    let maps = Arc::new(Mutex::new(cfg.folder_mappings.clone()));
    let status = StatusHandle::new();
    sync::run_watcher(
        data_dir_b,
        uploader_for,
        maps,
        opts,
        status,
        stop,
        Some(cache),
    )
    .await;
    Ok(())
}

/// Block until SIGINT (Ctrl+C) or SIGTERM. Does **not** treat stdin EOF as stop.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
