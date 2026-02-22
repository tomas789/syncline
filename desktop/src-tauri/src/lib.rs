use std::sync::Arc;
use tauri::{
    menu::{Menu, MenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager,
};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::Mutex;

struct ServerState {
    handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let server_state = ServerState {
        handle: Arc::new(Mutex::new(None)),
    };

    tauri::Builder::default()
        .manage(server_state)
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let version_i = MenuItem::with_id(
                app,
                "version",
                &format!("Syncline v{}", env!("CARGO_PKG_VERSION")),
                false,
                None::<&str>,
            )?;
            let status_i =
                MenuItem::with_id(app, "status", "Status: Stopped", false, None::<&str>)?;
            let sep1 = tauri::menu::PredefinedMenuItem::separator(app)?;

            let stats_files_i =
                MenuItem::with_id(app, "stats_files", "Files: 0", false, None::<&str>)?;
            let stats_size_i =
                MenuItem::with_id(app, "stats_size", "DB Size: 0.00 MB", false, None::<&str>)?;
            let stats_updated_i = MenuItem::with_id(
                app,
                "stats_updated",
                "Last Update: Never",
                false,
                None::<&str>,
            )?;
            let open_db_i = MenuItem::with_id(
                app,
                "open_db",
                "Open Database Folder...",
                true,
                None::<&str>,
            )?;

            let sep2 = tauri::menu::PredefinedMenuItem::separator(app)?;
            let start_server_i =
                MenuItem::with_id(app, "start_server", "Start Server", true, None::<&str>)?;
            let stop_server_i =
                MenuItem::with_id(app, "stop_server", "Stop Server", false, None::<&str>)?;
            let separator = tauri::menu::PredefinedMenuItem::separator(app)?;
            let separator_quit = tauri::menu::PredefinedMenuItem::separator(app)?; // Added
            let check_updates_i = MenuItem::with_id(app, "check_updates", "Check for Updates...", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let mut ip_items = Vec::new();

            // Always add localhost first
            let localhost_url = "ws://127.0.0.1:3030/sync";
            if let Ok(item) = MenuItem::with_id(
                app,
                format!("copy_{}", localhost_url).as_str(),
                localhost_url,
                true,
                None::<&str>,
            ) {
                ip_items.push(item);
            }

            // Then add all non-loopback network interfaces
            if let Ok(interfaces) = get_if_addrs::get_if_addrs() {
                for iface in interfaces {
                    let ip = iface.ip();
                    if !iface.is_loopback() && ip.is_ipv4() {
                        let url = format!("ws://{}:3030/sync", ip);
                        if let Ok(item) = MenuItem::with_id(
                            app,
                            format!("copy_{}", url).as_str(),
                            url.as_str(),
                            true,
                            None::<&str>,
                        ) {
                            ip_items.push(item);
                        }
                    }
                }
            }

            let mut ip_item_refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = vec![];
            for item in &ip_items {
                ip_item_refs.push(item);
            }
            let ips_submenu = Submenu::with_items(app, "Copy Connection URL", true, &ip_item_refs)?;

            let menu = Menu::with_items(app, &[
                &version_i,
                &status_i,
                &sep1,
                &stats_files_i,
                &stats_size_i,
                &stats_updated_i,
                &open_db_i,
                &sep2,
                &start_server_i,
                &stop_server_i,
                &separator,
                &ips_submenu,
                &separator_quit,
                &check_updates_i,
                &quit_i
            ])?;

            let sf = stats_files_i.clone();
            let ss = stats_size_i.clone();
            let su = stats_updated_i.clone();
            let app_handle_bg = app.handle().clone();

            tauri::async_runtime::spawn(async move {
                let db_path = app_handle_bg
                    .path()
                    .app_data_dir()
                    .unwrap_or_else(|_| std::path::PathBuf::from("."))
                    .join("syncline.db");

                let conn_str = format!("sqlite://{}", db_path.to_str().unwrap());
                let mut db_opt = None;
                let mut last_size = 0;

                loop {
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

                    if let Ok(m) = std::fs::metadata(&db_path) {
                        let size = m.len();

                        if let Ok(modified) = m.modified() {
                            if let Ok(elapsed) = modified.elapsed() {
                                let secs = elapsed.as_secs();
                                let time_str = if secs < 60 {
                                    format!("{}s ago", secs)
                                } else if secs < 3600 {
                                    format!("{}m ago", secs / 60)
                                } else if secs < 86400 {
                                    format!("{}h ago", secs / 3600)
                                } else {
                                    format!("{}d ago", secs / 86400)
                                };
                                let _ = su.set_text(&format!("Last Update: {}", time_str));
                            }
                        }

                        if size != last_size {
                            last_size = size;
                            let size_mb = size as f64 / 1024.0 / 1024.0;
                            let _ = ss.set_text(&format!("DB Size: {:.2} MB", size_mb));

                            if db_opt.is_none() {
                                db_opt = syncline::server::db::Db::new(&conn_str).await.ok();
                            }
                            if let Some(db) = &db_opt {
                                if let Ok(count) = db.count_docs().await {
                                    let _ = sf.set_text(&format!("Files: {}", count));
                                }
                            }
                        }
                    }
                }
            });

            let s1_status = status_i.clone();
            let s1_start = start_server_i.clone();
            let s1_stop = stop_server_i.clone();

            let s2_status = status_i.clone();
            let s2_start = start_server_i.clone();
            let s2_stop = stop_server_i.clone();

            let app_state = app.state::<ServerState>();
            let startup_handle = app_state.handle.clone();
            let startup_app = app.handle().clone();
            let startup_status = status_i.clone();
            let startup_start = start_server_i.clone();
            let startup_stop = stop_server_i.clone();

            tauri::async_runtime::spawn(async move {
                let mut handle_lock = startup_handle.lock().await;
                if handle_lock.is_none() {
                    let db_path = startup_app
                        .path()
                        .app_data_dir()
                        .unwrap_or_else(|_| std::path::PathBuf::from("."))
                        .join("syncline.db");

                    if let Some(parent) = db_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if !db_path.exists() {
                        let _ = std::fs::File::create(&db_path);
                    }

                    let conn_str = format!("sqlite://{}", db_path.to_str().unwrap());
                    if let Ok(db) = syncline::server::db::Db::new(&conn_str).await {
                        let h = tokio::spawn(async move {
                            let _ = syncline::server::server::run_server(db, 3030).await;
                        });
                        *handle_lock = Some(h);
                        let _ = startup_status.set_text("Status: Running");
                        let _ = startup_start.set_enabled(false);
                        let _ = startup_stop.set_enabled(true);
                        println!("Server started automatically on app launch");
                    }
                }
            });

            let _tray = TrayIconBuilder::with_id("main")
                .menu(&menu)
                // Use default app icon
                .icon(app.default_window_icon().unwrap().clone())
                .on_menu_event(move |app, event| {
                    let state = app.state::<ServerState>();
                    match event.id.as_ref() {
                        "start_server" => {
                            let handle_clone = state.handle.clone();
                            let app_handle = app.clone();
                            let s_status = s1_status.clone();
                            let s_start = s1_start.clone();
                            let s_stop = s1_stop.clone();
                            tauri::async_runtime::spawn(async move {
                                let mut handle_lock = handle_clone.lock().await;
                                if handle_lock.is_none() {
                                    let db_path = app_handle
                                        .path()
                                        .app_data_dir()
                                        .unwrap_or_else(|_| std::path::PathBuf::from("."))
                                        .join("syncline.db");

                                    // Ensure the data dir exists
                                    if let Some(parent) = db_path.parent() {
                                        let _ = std::fs::create_dir_all(parent);
                                    }

                                    // Make sure file exists so sqlx doesn't complain
                                    if !db_path.exists() {
                                        let _ = std::fs::File::create(&db_path);
                                    }

                                    let conn_str =
                                        format!("sqlite://{}", db_path.to_str().unwrap());
                                    if let Ok(db) = syncline::server::db::Db::new(&conn_str).await {
                                        let h = tokio::spawn(async move {
                                            let _ = syncline::server::server::run_server(db, 3030)
                                                .await;
                                        });
                                        *handle_lock = Some(h);
                                        let _ = s_status.set_text("Status: Running");
                                        let _ = s_start.set_enabled(false);
                                        let _ = s_stop.set_enabled(true);
                                        println!("Server started on port 3030");
                                    }
                                }
                            });
                        }
                        "stop_server" => {
                            let handle_clone = state.handle.clone();
                            let s_status = s2_status.clone();
                            let s_start = s2_start.clone();
                            let s_stop = s2_stop.clone();
                            tauri::async_runtime::spawn(async move {
                                let mut handle_lock = handle_clone.lock().await;
                                if let Some(h) = handle_lock.take() {
                                    h.abort();
                                    let _ = s_status.set_text("Status: Stopped");
                                    let _ = s_start.set_enabled(true);
                                    let _ = s_stop.set_enabled(false);
                                    println!("Server stopped");
                                }
                            });
                        }
                        "open_db" => {
                            let app_handle = app.clone();
                            let db_path = app_handle
                                .path()
                                .app_data_dir()
                                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                                .join("syncline.db");
                            if let Some(parent) = db_path.parent() {
                                if let Err(e) = app_handle
                                    .opener()
                                    .open_path(parent.to_str().unwrap(), None::<&str>)
                                {
                                    eprintln!("Failed to open db folder: {}", e);
                                }
                            }
                        }
                        "check_updates" => {
                            let app_clone = app.clone();
                            tauri::async_runtime::spawn(async move {
                                if let Ok(updater) = app_clone.updater() {
                                    match updater.check().await {
                                        Ok(Some(update)) => {
                                            let msg = format!(
                                                "A new version ({}) is available (current: {}).\n\nWould you like to download and install it?",
                                                update.version, update.current_version
                                            );

                                            app_clone.dialog()
                                                .message(msg)
                                                .title("Update Available")
                                                .kind(tauri_plugin_dialog::MessageDialogKind::Info)
                                                .buttons(tauri_plugin_dialog::MessageDialogButtons::OkCancelCustom("Install".to_string(), "Cancel".to_string()))
                                                .show(move |result| {
                                                    if result {
                                                        tauri::async_runtime::spawn(async move {
                                                            let _ = update.download_and_install(|_, _| {}, || {}).await;
                                                            app_clone.restart();
                                                        });
                                                    }
                                                });
                                        }
                                        Ok(None) => {
                                            app_clone.dialog()
                                                .message("You are already on the latest version.")
                                                .title("Up to date")
                                                .kind(tauri_plugin_dialog::MessageDialogKind::Info)
                                                .show(|_| {});
                                        }
                                        Err(e) => {
                                            app_clone.dialog()
                                                .message(format!("Failed to check for updates: {}", e))
                                                .title("Update Error")
                                                .kind(tauri_plugin_dialog::MessageDialogKind::Error)
                                                .show(|_| {});
                                        }
                                    }
                                }
                            });
                        }
                        "quit" => {
                            app.exit(0);
                        }
                        other_id if other_id.starts_with("copy_") => {
                            let url = other_id.strip_prefix("copy_").unwrap();
                            if let Err(e) = app.clipboard().write_text(url.to_string()) {
                                eprintln!("Failed to copy to clipboard: {}", e);
                            } else {
                                println!("Copied {} to clipboard", url);
                            }
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                })
                .build(app)?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
