use base64::Engine;
use tauri::{Manager, State};

use crate::{
    config::Config,
    db,
    r2::{generate_key, generate_named_key, R2Client, UploadResult},
    AppState,
};

// ── Upload ────────────────────────────────────────────────────────────────────

/// Upload a file from a local filesystem path (from the file picker or drag-drop).
#[tauri::command]
pub async fn upload_file(path: String, state: State<'_, AppState>) -> Result<UploadResult, String> {
    let config = state.config.lock().await.clone();
    require_configured(&config)?;

    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");
    let key = generate_key(ext);

    let client = R2Client::new(&config).await?;
    let result = client.upload_path(&path, &key).await?;

    let db = state.db.lock().await;
    db::insert(
        &db,
        &result.key,
        &result.display_name,
        result.size,
        &result.content_type,
        &result.url,
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
    let config = state.config.lock().await.clone();
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
    )
    .map_err(|e| e.to_string())?;

    Ok(result)
}

// ── History ───────────────────────────────────────────────────────────────────

/// Return all upload records from local SQLite, newest first.
#[tauri::command]
pub async fn list_files(state: State<'_, AppState>) -> Result<Vec<db::FileRecord>, String> {
    let db = state.db.lock().await;
    db::list(&db).map_err(|e| e.to_string())
}

/// Delete an object from R2 and remove it from local history.
/// If R2 credentials are not configured, only removes the local record.
#[tauri::command]
pub async fn delete_file(key: String, state: State<'_, AppState>) -> Result<(), String> {
    let config = state.config.lock().await.clone();

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

    let config = state.config.lock().await.clone();
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
    };

    let db = state.db.lock().await;
    db::rename(&db, &old_key, &new_key, display_name, &new_url).map_err(|e| e.to_string())?;
    drop(db);

    let _ = client.delete(&old_key).await;

    Ok(updated)
}

// ── Config ────────────────────────────────────────────────────────────────────

/// Return current config (all fields, including keys — shown masked in UI).
#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> Result<Config, String> {
    Ok(state.config.lock().await.clone())
}

/// Persist updated config and reload the in-memory copy.
#[tauri::command]
pub async fn save_config(
    config: Config,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    config.save(&data_dir)?;
    *state.config.lock().await = config;
    Ok(())
}

/// Verify R2 credentials by hitting HeadBucket.
#[tauri::command]
pub async fn test_connection(state: State<'_, AppState>) -> Result<(), String> {
    let config = state.config.lock().await.clone();
    require_configured(&config)?;
    let client = R2Client::new(&config).await?;
    client.test().await
}

// ── Window ────────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn hide_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("main") {
        w.hide().map_err(|e| e.to_string())?;
    }
    Ok(())
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
