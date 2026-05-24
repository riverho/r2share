use rusqlite::Connection;
use tauri::{
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager,
};
use tokio::sync::Mutex;

mod commands;
mod config;
mod db;
mod r2;

use config::Config;

// ── App state ─────────────────────────────────────────────────────────────────

pub struct AppState {
    pub db: Mutex<Connection>,
    pub config: Mutex<Config>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // ── Data directory & DB ───────────────────────────────────────────
            let data_dir = app.path().app_data_dir().expect("No app data dir");
            std::fs::create_dir_all(&data_dir).expect("Cannot create data dir");

            let db_path = data_dir.join("r2share.db");
            let conn = Connection::open(&db_path).expect("Failed to open SQLite database");
            db::init(&conn).expect("Failed to initialise database schema");

            // ── Config ────────────────────────────────────────────────────────
            let config = Config::load(&data_dir);
            let is_configured = config.is_configured();

            app.manage(AppState {
                db: Mutex::new(conn),
                config: Mutex::new(config),
            });

            if let Some(window) = app.get_webview_window("main") {
                let hide_on_blur = window.clone();
                window.on_window_event(move |event| {
                    if matches!(event, tauri::WindowEvent::Focused(false)) {
                        let _ = hide_on_blur.hide();
                    }
                });
            }

            // ── System tray ───────────────────────────────────────────────────
            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("r2share — click to open")
                .on_tray_icon_event(|tray, event| {
                    // Left-click: toggle window visibility
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let handle = tray.app_handle();
                        if let Some(window) = handle.get_webview_window("main") {
                            if window.is_visible().unwrap_or(false) {
                                let _ = window.hide();
                            } else {
                                position_bottom_right(&window);
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;

            // ── First-run: show window + settings panel ───────────────────────
            if !is_configured {
                if let Some(window) = app.get_webview_window("main") {
                    position_bottom_right(&window);
                    let _ = window.show();
                    // Signal JS to open the settings panel immediately
                    let _ = window.eval("window.__r2shareFirstRun = true;");
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::upload_file,
            commands::upload_clipboard_image,
            commands::list_files,
            commands::delete_file,
            commands::rename_file,
            commands::get_config,
            commands::save_config,
            commands::test_connection,
            commands::hide_window,
            commands::minimize_window,
        ])
        .run(tauri::generate_context!())
        .expect("Error running r2share");
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Position the window 16 px from the bottom-right corner of the primary monitor,
/// leaving ~56 px for the Windows taskbar.
fn position_bottom_right(window: &tauri::WebviewWindow) {
    if let Ok(Some(monitor)) = window.current_monitor() {
        let size = monitor.size();
        let scale = monitor.scale_factor();
        let win = window.outer_size().unwrap_or_default();

        let x = (size.width as f64 / scale) - (win.width as f64 / scale) - 16.0;
        let y = (size.height as f64 / scale) - (win.height as f64 / scale) - 56.0;

        let _ = window.set_position(tauri::LogicalPosition::new(x, y));
    }
}
