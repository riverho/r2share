use rusqlite::Connection;
use tauri::{
    menu::{Menu, MenuItem},
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

            // Hide-on-blur is skipped on Linux: the tray menu / WM steals focus
            // and the window would vanish right after being shown.
            #[cfg(not(target_os = "linux"))]
            if let Some(window) = app.get_webview_window("main") {
                let hide_on_blur = window.clone();
                window.on_window_event(move |event| {
                    if matches!(event, tauri::WindowEvent::Focused(false)) {
                        let _ = hide_on_blur.hide();
                    }
                });
            }

            // ── System tray ───────────────────────────────────────────────────
            // Tray menu (required on Linux, where tray click events are not emitted)
            let show_i = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
            let hide_i = MenuItem::with_id(app, "hide", "Hide", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_i, &hide_i, &quit_i])?;

            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("r2share — click to open")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main(app),
                    "hide" => {
                        if let Some(w) = app.get_webview_window("main") {
                            #[cfg(target_os = "linux")]
                            let _ = w.minimize();
                            #[cfg(not(target_os = "linux"))]
                            let _ = w.hide();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
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
                                show_main(handle);
                            }
                        }
                    }
                })
                .build(app)?;

            // ── Linux: keep a dock/taskbar entry and always show on launch ─────
            // Tray click events don't exist on Linux and tray hosts are flaky,
            // so the dock entry is the reliable way back to the window.
            #[cfg(target_os = "linux")]
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_skip_taskbar(false);
            }
            // Clear stale VNC clipboard test tokens so they can't be pasted.
            #[cfg(target_os = "linux")]
            for sel in [gtk::gdk::SELECTION_CLIPBOARD, gtk::gdk::SELECTION_PRIMARY] {
                gtk::Clipboard::get(&sel).request_text(|cb, text| {
                    if text.map_or(false, commands::is_vnc_test_token) {
                        cb.set_text("");
                    }
                });
            }

            #[cfg(target_os = "linux")]
            let show_on_start = true;
            #[cfg(not(target_os = "linux"))]
            let show_on_start = !is_configured;

            // ── First-run: show window + settings panel ───────────────────────
            if show_on_start {
                if !is_configured {
                    if let Some(window) = app.get_webview_window("main") {
                        // Signal JS to open the settings panel immediately
                        let _ = window.eval("window.__r2shareFirstRun = true;");
                    }
                }
                // Defer the show until the event loop is running; on Linux a
                // show() issued directly from setup() can leave the GTK window unmapped.
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    let h2 = handle.clone();
                    let _ = handle.run_on_main_thread(move || {
                        show_main(&h2);
                    });
                });
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
            commands::read_clipboard_text,
            commands::read_clipboard_image,
        ])
        .run(tauri::generate_context!())
        .expect("Error running r2share");
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn show_main(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        #[cfg(not(target_os = "linux"))]
        position_bottom_right(&window);
        let _ = window.show();
        // Linux WMs ignore positioning of unmapped windows; centre after mapping.
        #[cfg(target_os = "linux")]
        position_bottom_right(&window);
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Position the window 16 px from the bottom-right corner of the primary monitor,
/// leaving ~56 px for the Windows taskbar.
fn position_bottom_right(window: &tauri::WebviewWindow) {
    // Linux panels/docks vary (top bar, bottom dock, Wayland ignores positioning):
    // just centre the window.
    #[cfg(target_os = "linux")]
    {
        // window.center() sees a 0x0 size right after show(); centre manually,
        // falling back to the configured 380x520 logical size.
        if let Ok(Some(monitor)) = window.current_monitor() {
            let scale = monitor.scale_factor();
            let size = monitor.size().to_logical::<f64>(scale);
            let win = window.outer_size().unwrap_or_default().to_logical::<f64>(scale);
            let (w, h) = if win.width < 50.0 { (380.0, 520.0) } else { (win.width, win.height) };
            let _ = window.set_position(tauri::LogicalPosition::new(
                ((size.width - w) / 2.0).max(0.0),
                ((size.height - h) / 2.0).max(0.0),
            ));
        }
        return;
    }
    #[allow(unreachable_code)]
    if let Ok(Some(monitor)) = window.current_monitor() {
        let size = monitor.size();
        let scale = monitor.scale_factor();
        let win = window.outer_size().unwrap_or_default();

        let x = (size.width as f64 / scale) - (win.width as f64 / scale) - 16.0;
        let y = (size.height as f64 / scale) - (win.height as f64 / scale) - 56.0;

        let _ = window.set_position(tauri::LogicalPosition::new(x, y));
    }
}
