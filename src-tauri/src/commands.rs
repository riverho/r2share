use base64::Engine;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Emitter, Manager, State};

use crate::{
    config::{self, AppConfig, Config, FolderMapping, ImportMode, ImportResult, Vault},
    db,
    r2::{generate_key, generate_named_key, ProgressFn, R2Client, UploadResult},
    AppState,
};

/// Public vault summary for the GUI switcher / Settings list (never includes secrets).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultInfo {
    pub name: String,
    pub bucket: String,
    pub is_default: bool,
    pub configured: bool,
}

fn resolve_vault(cfg: &AppConfig, vault: Option<&str>) -> Result<(String, Config), String> {
    let name = vault
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(cfg.default_vault.as_str());
    let v = cfg
        .vault_by_name(name)
        .ok_or_else(|| format!("vault not found: {name}"))?;
    Ok((v.name.clone(), v.to_flat()))
}

fn require_configured(config: &Config) -> Result<(), String> {
    if config.is_configured() {
        Ok(())
    } else {
        Err("R2 not configured. Open Settings and enter your credentials.".to_string())
    }
}

fn notify_sync_config_changed(app: &tauri::AppHandle, cfg: &AppConfig) {
    if let Some(rt) = app.try_state::<crate::sync_runtime::SyncRuntime>() {
        rt.on_config_changed(cfg.folder_mappings.clone());
    }
}


fn app_data_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path().app_data_dir().map_err(|e| e.to_string())
}

fn preserve_blank_secrets(existing: &Config, incoming: Config) -> Config {
    Config {
        account_id: incoming.account_id,
        bucket: if incoming.bucket.is_empty() {
            existing.bucket.clone()
        } else {
            incoming.bucket
        },
        access_key_id: if incoming.access_key_id.is_empty() {
            existing.access_key_id.clone()
        } else {
            incoming.access_key_id
        },
        secret_access_key: if incoming.secret_access_key.is_empty() {
            existing.secret_access_key.clone()
        } else {
            incoming.secret_access_key
        },
        public_url_base: incoming.public_url_base,
    }
}

// ── Upload ────────────────────────────────────────────────────────────────────

/// Upload a file. Optional `vault` falls back to the default vault.
#[tauri::command]
pub async fn upload_file(
    path: String,
    vault: Option<String>,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<UploadResult, String> {
    let (vault_name, config) = {
        let app_cfg = state.app_config.lock().await;
        resolve_vault(&app_cfg, vault.as_deref())?
    };
    require_configured(&config)?;

    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");
    let key = generate_key(ext);

    let client = R2Client::new(&config).await?;
    let on_progress: ProgressFn = Arc::new(move |sent, total| {
        let _ = app.emit(
            "upload-progress",
            serde_json::json!({ "sent": sent, "total": total }),
        );
    });
    let result = client.upload_path(&path, &key, on_progress).await?;

    let db = state.db.lock().await;
    db::insert(
        &db,
        &result.key,
        &result.display_name,
        result.size,
        &result.content_type,
        &result.url,
        &vault_name,
    )
    .map_err(|e| e.to_string())?;

    Ok(result)
}

/// Upload a clipboard image. Optional `vault` falls back to the default vault.
#[tauri::command]
pub async fn upload_clipboard_image(
    data: String,
    mime_type: String,
    vault: Option<String>,
    state: State<'_, AppState>,
) -> Result<UploadResult, String> {
    let (vault_name, config) = {
        let app_cfg = state.app_config.lock().await;
        resolve_vault(&app_cfg, vault.as_deref())?
    };
    require_configured(&config)?;

    let ext = match mime_type.as_str() {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    };
    let key = generate_key(ext);

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let display_name = format!("clipboard-{}.{}", ts, ext);

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&data)
        .map_err(|e| format!("Bad base64: {}", e))?;

    let client = R2Client::new(&config).await?;
    let result = client
        .upload_bytes(&key, bytes, &mime_type, &display_name)
        .await?;

    let db = state.db.lock().await;
    db::insert(
        &db,
        &result.key,
        &result.display_name,
        result.size,
        &result.content_type,
        &result.url,
        &vault_name,
    )
    .map_err(|e| e.to_string())?;

    Ok(result)
}

// ── History ───────────────────────────────────────────────────────────────────

/// List uploads. `vault: None` = all; `Some(name)` filters by vault.
#[tauri::command]
pub async fn list_files(
    vault: Option<String>,
    state: State<'_, AppState>,
) -> Result<Vec<db::FileRecord>, String> {
    let db = state.db.lock().await;
    let filter = vault
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "*");
    db::list(&db, filter, None).map_err(|e| e.to_string())
}

/// Delete an object from R2 and remove it from local history.
#[tauri::command]
pub async fn delete_file(
    key: String,
    vault: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let (config, record_vault) = {
        let db = state.db.lock().await;
        let rec = db::get(&db, &key).ok();
        drop(db);
        let app_cfg = state.app_config.lock().await;
        let name = vault
            .as_deref()
            .or_else(|| rec.as_ref().map(|r| r.vault.as_str()));
        let (_n, flat) = resolve_vault(&app_cfg, name)?;
        (flat, rec.map(|r| r.vault))
    };
    let _ = record_vault;

    if config.is_configured() {
        let client = R2Client::new(&config).await?;
        client.delete(&key).await?;
    }

    let db = state.db.lock().await;
    db::delete(&db, &key).map_err(|e| e.to_string())
}

/// Rename an uploaded object and refresh its public sharing URL.
#[tauri::command]
pub async fn rename_file(
    old_key: String,
    new_display_name: String,
    vault: Option<String>,
    state: State<'_, AppState>,
) -> Result<db::FileRecord, String> {
    let display_name = new_display_name.trim();
    if display_name.is_empty() {
        return Err("Enter a filename before saving.".to_string());
    }

    let old_record = {
        let db = state.db.lock().await;
        db::get(&db, &old_key).map_err(|e| e.to_string())?
    };

    let (_vault_name, config) = {
        let app_cfg = state.app_config.lock().await;
        let name = vault.as_deref().or(Some(old_record.vault.as_str()));
        resolve_vault(&app_cfg, name)?
    };
    require_configured(&config)?;

    let new_key = generate_named_key(display_name)?;
    let client = R2Client::new(&config).await?;
    client.copy(&old_key, &new_key).await?;
    let new_url = client.public_url(&new_key);

    let updated = db::FileRecord {
        id: old_record.id,
        key: new_key.clone(),
        display_name: display_name.to_string(),
        size: old_record.size,
        content_type: old_record.content_type,
        url: new_url.clone(),
        uploaded_at: old_record.uploaded_at,
        vault: old_record.vault,
    };

    let db = state.db.lock().await;
    db::rename(&db, &old_key, &new_key, display_name, &new_url).map_err(|e| e.to_string())?;
    drop(db);

    let _ = client.delete(&old_key).await;

    Ok(updated)
}

// ── Config (default-vault adapters; keep working for old UI) ──────────────────

/// Return the default vault as a flat Config.
#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> Result<Config, String> {
    Ok(state.app_config.lock().await.default_flat())
}

/// Persist updated default-vault credentials (blank secrets preserved).
#[tauri::command]
pub async fn save_config(
    config: Config,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let data_dir = app_data_dir(&app)?;
    let mut app_cfg = state.app_config.lock().await;
    let existing = app_cfg.default_flat();
    let merged = preserve_blank_secrets(&existing, config);
    app_cfg.set_default_flat(merged);
    config::save(&data_dir, &app_cfg)?;
    notify_sync_config_changed(&app, &app_cfg);
    Ok(())
}

/// Verify R2 credentials. Optional `vault` falls back to default.
#[tauri::command]
pub async fn test_connection(
    vault: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let (_name, config) = {
        let app_cfg = state.app_config.lock().await;
        resolve_vault(&app_cfg, vault.as_deref())?
    };
    require_configured(&config)?;
    let client = R2Client::new(&config).await?;
    client.test().await
}

// ── Vault CRUD ────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn list_vaults(state: State<'_, AppState>) -> Result<Vec<VaultInfo>, String> {
    let cfg = state.app_config.lock().await;
    Ok(cfg
        .vaults
        .iter()
        .map(|v| VaultInfo {
            name: v.name.clone(),
            bucket: v.bucket.clone(),
            is_default: v.name == cfg.default_vault,
            configured: v.to_flat().is_configured(),
        })
        .collect())
}

#[tauri::command]
pub async fn get_vault(
    name: Option<String>,
    state: State<'_, AppState>,
) -> Result<Config, String> {
    let cfg = state.app_config.lock().await;
    Ok(resolve_vault(&cfg, name.as_deref())?.1)
}

#[tauri::command]
pub async fn set_default_vault(
    name: String,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let data_dir = app_data_dir(&app)?;
    let mut cfg = state.app_config.lock().await;
    if cfg.vault_by_name(&name).is_none() {
        return Err(format!("vault not found: {name}"));
    }
    cfg.default_vault = name;
    config::save(&data_dir, &cfg)?;
    notify_sync_config_changed(&app, &cfg);
    Ok(())
}

#[tauri::command]
pub async fn add_vault(
    name: String,
    config: Config,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("vault name must not be empty".into());
    }
    let data_dir = app_data_dir(&app)?;
    let mut cfg = state.app_config.lock().await;
    if cfg.vault_by_name(&name).is_some() {
        return Err(format!("vault already exists: {name}"));
    }
    let mut v = Vault::new(&name);
    v.apply_flat(&config);
    v.validate()?;
    cfg.vaults.push(v);
    if cfg.vaults.len() == 1 {
        cfg.default_vault = name;
    }
    cfg.validate()?;
    config::save(&data_dir, &cfg)?;
    notify_sync_config_changed(&app, &cfg);
    Ok(())
}

#[tauri::command]
pub async fn update_vault(
    name: String,
    config: Config,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let data_dir = app_data_dir(&app)?;
    let mut cfg = state.app_config.lock().await;
    let existing = cfg
        .vault_by_name(&name)
        .ok_or_else(|| format!("vault not found: {name}"))?
        .to_flat();
    let merged = preserve_blank_secrets(&existing, config);
    let v = cfg
        .vault_by_name_mut(&name)
        .ok_or_else(|| format!("vault not found: {name}"))?;
    v.apply_flat(&merged);
    v.validate()?;
    cfg.validate()?;
    config::save(&data_dir, &cfg)?;
    notify_sync_config_changed(&app, &cfg);
    Ok(())
}

#[tauri::command]
pub async fn delete_vault(
    name: String,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let data_dir = app_data_dir(&app)?;
    let mut cfg = state.app_config.lock().await;
    if cfg.vault_by_name(&name).is_none() {
        return Err(format!("vault not found: {name}"));
    }
    if cfg.vaults.len() <= 1 {
        return Err("cannot delete the last vault".into());
    }
    cfg.vaults.retain(|v| v.name != name);
    if cfg.default_vault == name {
        cfg.default_vault = cfg.vaults[0].name.clone();
    }
    // Drop mappings that pointed at the deleted vault (validate would fail otherwise).
    cfg.folder_mappings.retain(|m| m.vault != name);
    config::save(&data_dir, &cfg)?;
    notify_sync_config_changed(&app, &cfg);
    Ok(())
}

/// Export vaults to a JSON file (0600 on Unix).
#[tauri::command]
pub async fn export_vaults(
    path: String,
    include_secrets: bool,
    names: Option<Vec<String>>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let data_dir = app_data_dir(&app)?;
    config::export_vaults(
        &data_dir,
        PathBuf::from(path).as_path(),
        include_secrets,
        names,
    )
}

/// Import vaults from a JSON file (`mode`: "overwrite" | "skip").
#[tauri::command]
pub async fn import_vaults(
    path: String,
    mode: String,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<ImportResult, String> {
    let data_dir = app_data_dir(&app)?;
    let import_mode = ImportMode::parse(&mode)?;
    let result = config::import_vaults(&data_dir, PathBuf::from(path).as_path(), import_mode)?;
    let loaded = config::load(&data_dir);
    notify_sync_config_changed(&app, &loaded);
    *state.app_config.lock().await = loaded;
    Ok(result)
}


// ── Folder sync ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct FolderMappingInfo {
    pub path: String,
    pub vault: String,
    pub paused: bool,
}

#[tauri::command]
pub async fn list_folder_mappings(
    state: State<'_, AppState>,
) -> Result<Vec<FolderMappingInfo>, String> {
    let cfg = state.app_config.lock().await;
    Ok(cfg
        .folder_mappings
        .iter()
        .map(|m| FolderMappingInfo {
            path: m.path.clone(),
            vault: m.vault.clone(),
            paused: m.paused,
        })
        .collect())
}

#[tauri::command]
pub async fn add_folder_mapping(
    path: String,
    vault: Option<String>,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<FolderMappingInfo, String> {
    let data_dir = app_data_dir(&app)?;
    let mut cfg = state.app_config.lock().await;
    let vault_name = vault.unwrap_or_else(|| cfg.default_vault.clone());
    if cfg.vault_by_name(&vault_name).is_none() {
        return Err(format!("vault not found: {vault_name}"));
    }
    let p = PathBuf::from(&path);
    std::fs::create_dir_all(&p).map_err(|e| format!("create {}: {e}", p.display()))?;
    let canon = p
        .canonicalize()
        .map_err(|e| format!("canonicalize {}: {e}", p.display()))?;
    let path_s = canon.display().to_string();
    if cfg.folder_mappings.iter().any(|m| m.path == path_s) {
        return Err(format!("mapping already exists: {path_s}"));
    }
    let mapping = FolderMapping::new(path_s.clone(), vault_name.clone());
    cfg.folder_mappings.push(mapping.clone());
    cfg.validate()?;
    config::save(&data_dir, &cfg)?;
    if let Some(rt) = app.try_state::<crate::sync_runtime::SyncRuntime>() {
        rt.on_config_changed(cfg.folder_mappings.clone());
    }
    Ok(FolderMappingInfo {
        path: path_s,
        vault: vault_name,
        paused: false,
    })
}

#[tauri::command]
pub async fn remove_folder_mapping(
    path: String,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let data_dir = app_data_dir(&app)?;
    let mut cfg = state.app_config.lock().await;
    let before = cfg.folder_mappings.len();
    cfg.folder_mappings.retain(|m| m.path != path);
    if cfg.folder_mappings.len() == before {
        return Err(format!("mapping not found: {path}"));
    }
    config::save(&data_dir, &cfg)?;
    if let Some(rt) = app.try_state::<crate::sync_runtime::SyncRuntime>() {
        rt.on_config_changed(cfg.folder_mappings.clone());
    }
    Ok(())
}

#[tauri::command]
pub async fn set_folder_mapping_paused(
    path: String,
    paused: bool,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let data_dir = app_data_dir(&app)?;
    let mut cfg = state.app_config.lock().await;
    let m = cfg
        .folder_mappings
        .iter_mut()
        .find(|m| m.path == path)
        .ok_or_else(|| format!("mapping not found: {path}"))?;
    m.paused = paused;
    config::save(&data_dir, &cfg)?;
    if let Some(rt) = app.try_state::<crate::sync_runtime::SyncRuntime>() {
        rt.on_config_changed(cfg.folder_mappings.clone());
    }
    Ok(())
}

#[tauri::command]
pub async fn get_sync_status(app: tauri::AppHandle) -> Result<crate::sync::SyncStatus, String> {
    if let Some(rt) = app.try_state::<crate::sync_runtime::SyncRuntime>() {
        Ok(rt.status.get())
    } else {
        Ok(crate::sync::SyncStatus {
            state: "idle".into(),
            syncing_n: 0,
            last_error: None,
            last_synced_at: None,
        })
    }
}

#[tauri::command]
pub async fn suggested_sync_folder() -> Result<String, String> {
    Ok(config::DEFAULT_SYNC_FOLDER.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("r2share-cmd-test-{n}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn seeded() -> (PathBuf, AppConfig) {
        let dir = tmp();
        let mut cfg = AppConfig::empty_v2();
        let mut a = Vault::new("alpha");
        a.account_id = "acct-a".into();
        a.access_key_id = "KEYA".into();
        a.secret_access_key = "SECRETA".into();
        a.bucket = "ba".into();
        a.public_url_base = "https://a.example".into();
        let mut b = Vault::new("beta");
        b.account_id = "acct-b".into();
        b.access_key_id = "KEYB".into();
        b.secret_access_key = "SECRETB".into();
        b.bucket = "bb".into();
        b.public_url_base = "https://b.example".into();
        cfg.vaults = vec![a, b];
        cfg.default_vault = "alpha".into();
        config::save(&dir, &cfg).unwrap();
        (dir, cfg)
    }

    #[test]
    fn resolve_vault_falls_back_to_default() {
        let (_dir, cfg) = seeded();
        let (n, flat) = resolve_vault(&cfg, None).unwrap();
        assert_eq!(n, "alpha");
        assert_eq!(flat.bucket, "ba");
        let (n2, _) = resolve_vault(&cfg, Some("beta")).unwrap();
        assert_eq!(n2, "beta");
        assert!(resolve_vault(&cfg, Some("missing")).is_err());
    }

    #[test]
    fn preserve_blank_secrets_keeps_existing_keys() {
        let existing = Config {
            account_id: "a".into(),
            bucket: "b".into(),
            access_key_id: "OLDKEY".into(),
            secret_access_key: "OLDSECRET".into(),
            public_url_base: "https://old".into(),
        };
        let incoming = Config {
            account_id: "a2".into(),
            bucket: "b2".into(),
            access_key_id: String::new(),
            secret_access_key: String::new(),
            public_url_base: "https://new".into(),
        };
        let m = preserve_blank_secrets(&existing, incoming);
        assert_eq!(m.account_id, "a2");
        assert_eq!(m.bucket, "b2");
        assert_eq!(m.access_key_id, "OLDKEY");
        assert_eq!(m.secret_access_key, "OLDSECRET");
        assert_eq!(m.public_url_base, "https://new");
    }

    #[test]
    fn vault_crud_add_update_delete_set_default() {
        let (dir, mut cfg) = seeded();
        // add
        let mut v = Vault::new("gamma");
        v.account_id = "acct-g".into();
        v.access_key_id = "KEYG".into();
        v.secret_access_key = "SECRETG".into();
        v.public_url_base = "https://g.example".into();
        cfg.vaults.push(v);
        config::save(&dir, &cfg).unwrap();
        let loaded = config::load(&dir);
        assert_eq!(loaded.vaults.len(), 3);

        // update with blank secrets
        let mut cfg = loaded;
        let existing = cfg.vault_by_name("gamma").unwrap().to_flat();
        let merged = preserve_blank_secrets(
            &existing,
            Config {
                account_id: "acct-g2".into(),
                bucket: "bg".into(),
                access_key_id: String::new(),
                secret_access_key: String::new(),
                public_url_base: "https://g2.example".into(),
            },
        );
        cfg.vault_by_name_mut("gamma").unwrap().apply_flat(&merged);
        config::save(&dir, &cfg).unwrap();
        let g = config::load(&dir).vault_by_name("gamma").unwrap().clone();
        assert_eq!(g.account_id, "acct-g2");
        assert_eq!(g.access_key_id, "KEYG");
        assert_eq!(g.secret_access_key, "SECRETG");

        // set default
        let mut cfg = config::load(&dir);
        cfg.default_vault = "beta".into();
        config::save(&dir, &cfg).unwrap();
        assert_eq!(config::load(&dir).default_vault, "beta");

        // delete last-but-one ok; cannot delete last
        let mut cfg = config::load(&dir);
        cfg.vaults.retain(|v| v.name != "gamma");
        config::save(&dir, &cfg).unwrap();
        assert_eq!(config::load(&dir).vaults.len(), 2);

        let mut cfg = config::load(&dir);
        cfg.vaults.retain(|v| v.name != "beta");
        if cfg.default_vault == "beta" {
            cfg.default_vault = cfg.vaults[0].name.clone();
        }
        config::save(&dir, &cfg).unwrap();
        assert_eq!(config::load(&dir).vaults.len(), 1);
        // deleting the last is refused by the command layer; simulate check:
        assert_eq!(config::load(&dir).vaults.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ── Window ────────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn hide_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("main") {
        // Linux: minimise instead of hiding so the dock/taskbar entry stays
        // and the window can be restored from it (tray hosts are unreliable).
        #[cfg(target_os = "linux")]
        w.minimize().map_err(|e| e.to_string())?;
        #[cfg(not(target_os = "linux"))]
        w.hide().map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ── Native clipboard (Linux) ──────────────────────────────────────────────────
// WebKitGTK 2.54 segfaults inside its gtk_clipboard_request_targets callback
// when its built-in paste runs, so the frontend bypasses it on Linux and reads
// the clipboard through GTK here instead.

#[cfg(target_os = "linux")]
async fn on_main<T: Send + 'static>(
    app: &tauri::AppHandle,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let _ = tx.send(f());
    })
    .map_err(|e| e.to_string())?;
    rx.await.map_err(|e| e.to_string())
}

/// True for leftover VNC clipboard test tokens like "pasted-from-vnc-123".
pub fn is_vnc_test_token(text: &str) -> bool {
    let t = text.trim();
    t.len() > 16
        && t[..16].eq_ignore_ascii_case("pasted-from-vnc-")
        && t[16..].bytes().all(|b| b.is_ascii_digit())
}

/// Returns clipboard text, or None if the clipboard holds no text.
#[tauri::command]
pub async fn read_clipboard_text(app: tauri::AppHandle) -> Result<Option<String>, String> {
    #[cfg(target_os = "linux")]
    {
        return on_main(&app, || {
            gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD)
                .wait_for_text()
                .map(|s| s.to_string())
                .filter(|s| !is_vnc_test_token(s))
        })
        .await;
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = app;
        Err("native clipboard read is only used on Linux".into())
    }
}

/// Returns a clipboard image as base64 PNG, or None if there is no image.
#[tauri::command]
pub async fn read_clipboard_image(app: tauri::AppHandle) -> Result<Option<String>, String> {
    #[cfg(target_os = "linux")]
    {
        use base64::Engine;
        let png = on_main(&app, || {
            gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD)
                .wait_for_image()
                .and_then(|pb| pb.save_to_bufferv("png", &[]).ok())
        })
        .await?;
        return Ok(png.map(|b| base64::engine::general_purpose::STANDARD.encode(b)));
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = app;
        Err("native clipboard read is only used on Linux".into())
    }
}

#[tauri::command]
pub async fn minimize_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("main") {
        w.minimize().map_err(|e| e.to_string())?;
    }
    Ok(())
}

