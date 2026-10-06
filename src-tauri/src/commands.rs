use base64::Engine;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Emitter, Manager, State};

use crate::{
    config::{self, Config, ImportMode, ImportResult},
    db,
    r2::{generate_key, generate_named_key, ProgressFn, R2Client, UploadResult},
    AppState,
};

// ── Upload ────────────────────────────────────────────────────────────────────

/// Upload a file from a local filesystem path (from the file picker or drag-drop).
/// Emits `upload-progress` events (`{ sent, total }` in bytes) while uploading.
#[tauri::command]
pub async fn upload_file(
    path: String,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<UploadResult, String> {
    let (config, vault_name) = {
        let app_cfg = state.app_config.lock().await;
        (app_cfg.default_flat(), app_cfg.default_vault.clone())
    };
    require_configured(&config)?;

    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");
    let key = generate_key(ext);

    let client = R2Client::new(&config).await?;
    let on_progress: ProgressFn = Arc::new(move |sent, total| {
        let _ = app.emit("upload-progress", serde_json::json!({ "sent": sent, "total": total }));
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

/// Upload a clipboard image delivered as a base64-encoded blob from the frontend.
#[tauri::command]
pub async fn upload_clipboard_image(
    data: String,
    mime_type: String,
    state: State<'_, AppState>,
) -> Result<UploadResult, String> {
    let (config, vault_name) = {
        let app_cfg = state.app_config.lock().await;
        (app_cfg.default_flat(), app_cfg.default_vault.clone())
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

/// Return all upload records from local SQLite, newest first.
#[tauri::command]
pub async fn list_files(state: State<'_, AppState>) -> Result<Vec<db::FileRecord>, String> {
    let db = state.db.lock().await;
    db::list(&db, None, None).map_err(|e| e.to_string())
}

/// Delete an object from R2 and remove it from local history.
/// If R2 credentials are not configured, only removes the local record.
#[tauri::command]
pub async fn delete_file(key: String, state: State<'_, AppState>) -> Result<(), String> {
    let config = state.app_config.lock().await.default_flat();

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
    state: State<'_, AppState>,
) -> Result<db::FileRecord, String> {
    let display_name = new_display_name.trim();
    if display_name.is_empty() {
        return Err("Enter a filename before saving.".to_string());
    }

    let config = state.app_config.lock().await.default_flat();
    require_configured(&config)?;

    let old_record = {
        let db = state.db.lock().await;
        db::get(&db, &old_key).map_err(|e| e.to_string())?
    };

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

// ── Config ────────────────────────────────────────────────────────────────────

/// Return the default vault as a flat Config (Settings UI unchanged).
#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> Result<Config, String> {
    Ok(state.app_config.lock().await.default_flat())
}

/// Persist updated default-vault credentials and reload the in-memory copy.
#[tauri::command]
pub async fn save_config(
    config: Config,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let data_dir = app_data_dir(&app)?;
    let mut app_cfg = state.app_config.lock().await;
    app_cfg.set_default_flat(config);
    config::save(&data_dir, &app_cfg)?;
    Ok(())
}

/// Verify R2 credentials by hitting HeadBucket on the default vault.
#[tauri::command]
pub async fn test_connection(state: State<'_, AppState>) -> Result<(), String> {
    let config = state.app_config.lock().await.default_flat();
    require_configured(&config)?;
    let client = R2Client::new(&config).await?;
    client.test().await
}

/// Export vaults to a JSON file (0600 on Unix). No UI yet — for CLI / future Settings.
#[tauri::command]
pub async fn export_vaults(
    path: String,
    include_secrets: bool,
    names: Option<Vec<String>>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let data_dir = app_data_dir(&app)?;
    config::export_vaults(&data_dir, PathBuf::from(path).as_path(), include_secrets, names)
}

/// Import vaults from a JSON file (`mode`: "overwrite" | "skip"). Refreshes in-memory config.
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
    // Reload so the GUI sees merged vaults / default credentials.
    *state.app_config.lock().await = config::load(&data_dir);
    Ok(result)
}

fn app_data_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path().app_data_dir().map_err(|e| e.to_string())
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

// ── helpers ───────────────────────────────────────────────────────────────────

fn require_configured(config: &Config) -> Result<(), String> {
    if config.is_configured() {
        Ok(())
    } else {
        Err("R2 not configured. Open Settings and enter your credentials.".to_string())
    }
}
