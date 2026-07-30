use tauri::{
    menu::{MenuBuilder, MenuEvent, MenuItemBuilder, PredefinedMenuItem},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder,
};

use crate::commands::AppState;

/// damoiseau.xyz path is mid-migration from `folio` to `carrel` — this is the
/// post-migration URL, prepared ahead of the site catching up.
const USER_GUIDE_URL: &str = "https://damoiseau.xyz/docs/carrel/user-guide/";

/// Build (or rebuild) the tray menu showing both surface toggles.
pub fn build_tray_menu(
    app: &AppHandle,
    web_ui_enabled: bool,
    opds_enabled: bool,
    server_running: bool,
) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    let show_item = MenuItemBuilder::with_id("show", "Show Carrel").build(app)?;
    let whats_new = MenuItemBuilder::with_id("whats_new", "What's New").build(app)?;
    let check_update = MenuItemBuilder::with_id("check_update", "Check for Updates").build(app)?;
    let user_guide = MenuItemBuilder::with_id("user_guide", "User Guide").build(app)?;

    let open_webui = MenuItemBuilder::with_id("open_webui", "Open Web UI")
        .enabled(server_running && web_ui_enabled)
        .build(app)?;

    let sep1 = PredefinedMenuItem::separator(app)?;

    let web_ui_label = if web_ui_enabled {
        "Web UI: ON"
    } else {
        "Web UI: OFF"
    };
    let web_ui_toggle = MenuItemBuilder::with_id("toggle_web_ui", web_ui_label).build(app)?;

    let opds_label = if opds_enabled {
        "OPDS: ON"
    } else {
        "OPDS: OFF"
    };
    let opds_toggle = MenuItemBuilder::with_id("toggle_opds", opds_label).build(app)?;

    let sep2 = PredefinedMenuItem::separator(app)?;

    let quit_item = MenuItemBuilder::with_id("quit", "Quit Carrel").build(app)?;

    MenuBuilder::new(app)
        .item(&show_item)
        .item(&whats_new)
        .item(&check_update)
        .item(&user_guide)
        .item(&open_webui)
        .item(&sep1)
        .item(&web_ui_toggle)
        .item(&opds_toggle)
        .item(&sep2)
        .item(&quit_item)
        .build()
}

/// Initialize the tray icon and attach menu event handlers.
pub fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let (web_ui_enabled, opds_enabled, server_running) = {
        let state = app.state::<AppState>();
        let server_running = state.web_server_handle.lock().unwrap().is_some();
        let conn = state.active_db().and_then(|p| p.get().map_err(Into::into));
        let (web_ui, opds) = match conn {
            Ok(c) => (
                crate::db::get_setting(&c, "web_ui_enabled")
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some("true"),
                crate::db::get_setting(&c, "opds_enabled")
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some("true"),
            ),
            Err(_) => (false, false),
        };
        (web_ui, opds, server_running)
    };

    let menu = build_tray_menu(app, web_ui_enabled, opds_enabled, server_running)?;

    let _tray = TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().unwrap().clone())
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event: MenuEvent| match event.id().as_ref() {
            "show" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.unminimize();
                    let _ = window.show();
                    let _ = window.set_focus();
                } else {
                    // Window was destroyed — recreate it
                    let _ = WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
                        .title("Carrel")
                        .inner_size(800.0, 600.0)
                        .build();
                }
            }
            "whats_new" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.unminimize();
                    let _ = window.show();
                    let _ = window.set_focus();
                    let _ = window.emit("whats-new-open", ());
                } else {
                    let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
                        .title("Carrel")
                        .inner_size(800.0, 600.0)
                        .build();
                    if let Ok(w) = window {
                        let _ = w.emit("whats-new-open", ());
                    }
                }
            }
            "check_update" => {
                {
                    let state = app.state::<AppState>();
                    *state.pending_manual_update_check.lock().unwrap() = true;
                }
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.unminimize();
                    let _ = window.show();
                    let _ = window.set_focus();
                    let _ = window.emit("check-update-open", ());
                } else {
                    let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
                        .title("Carrel")
                        .inner_size(800.0, 600.0)
                        .build();
                    if let Ok(w) = window {
                        let _ = w.emit("check-update-open", ());
                    }
                }
            }
            "user_guide" => {
                let _ = open::that(USER_GUIDE_URL);
            }
            "open_webui" => {
                let state = app.state::<AppState>();
                let url = {
                    let handle = state.web_server_handle.lock().unwrap();
                    handle.as_ref().map(|h| h.url.clone())
                };
                if let Some(url) = url {
                    let _ = open::that(&url);
                }
            }
            "toggle_web_ui" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    toggle_mode(&app, ToggleWhich::WebUi).await;
                });
            }
            "toggle_opds" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    toggle_mode(&app, ToggleWhich::Opds).await;
                });
            }
            "quit" => {
                std::process::exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|_tray, event| {
            // macOS: activate the app on any tray interaction so the menu
            // can open even when the window is minimized.
            #[cfg(target_os = "macos")]
            {
                use tauri::tray::TrayIconEvent;
                if matches!(event, TrayIconEvent::Click { .. }) {
                    use objc2::MainThreadMarker;
                    use objc2_app_kit::NSApplication;
                    if let Some(mtm) = MainThreadMarker::new() {
                        let ns_app = NSApplication::sharedApplication(mtm);
                        ns_app.activate();
                    }
                }
            }
            let _ = &event; // suppress unused warning on non-macOS
        })
        .build(app)?;

    Ok(())
}

#[derive(Clone, Copy)]
enum ToggleWhich {
    WebUi,
    Opds,
}

/// Toggle one surface from the tray, mirroring the same start/stop
/// machinery that the Settings panel triggers via `web_server_set_modes`.
async fn toggle_mode(app: &AppHandle, which: ToggleWhich) {
    let state = app.state::<AppState>();

    let (current_web_ui, current_opds, current_port) = {
        let conn = match state.active_db().and_then(|p| p.get().map_err(Into::into)) {
            Ok(c) => c,
            Err(_) => return,
        };
        let web_ui = crate::db::get_setting(&conn, "web_ui_enabled")
            .ok()
            .flatten()
            .as_deref()
            == Some("true");
        let opds = crate::db::get_setting(&conn, "opds_enabled")
            .ok()
            .flatten()
            .as_deref()
            == Some("true");
        let port = crate::db::get_setting(&conn, "web_server_port")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(crate::web_server::DEFAULT_PORT);
        (web_ui, opds, port)
    };

    let (next_web_ui, next_opds) = match which {
        ToggleWhich::WebUi => (!current_web_ui, current_opds),
        ToggleWhich::Opds => (current_web_ui, !current_opds),
    };

    // Persist the flip.
    {
        let conn = match state.active_db().and_then(|p| p.get().map_err(Into::into)) {
            Ok(c) => c,
            Err(_) => return,
        };
        let _ = crate::db::set_setting(&conn, "web_ui_enabled", &next_web_ui.to_string());
        let _ = crate::db::set_setting(&conn, "opds_enabled", &next_opds.to_string());
    }

    let modes = crate::web_server::ServerModes {
        web_ui: next_web_ui,
        opds: next_opds,
    };

    // Stop existing handle.
    let prev = { state.web_server_handle.lock().unwrap().take() };
    if let Some(h) = prev {
        crate::web_server::stop(h);
    }

    // Restart if anything is on.
    if modes.any() {
        let pin_hash = crate::web_server::auth::load_pin_hash();
        {
            let mut ph = state.shared_pin_hash.lock().unwrap();
            *ph = pin_hash;
        }
        let web_state = crate::web_server::WebState {
            pool: state.shared_active_pool.clone(),
            data_dir: state.data_dir.clone(),
            cache_dir: state.cache_dir.clone(),
            pin_hash: state.shared_pin_hash.clone(),
            sessions: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            login_limiter: std::sync::Arc::new(crate::web_server::auth::RateLimiter::new(5, 300)),
            active_profile_name: state.shared_active_profile_name.clone(),
            unlocked_profiles: state.unlocked_profiles.clone(),
            private_mode: state.private_mode.clone(),
            profile_host: Some(crate::profile_host::for_app(app)),
        };
        if let Ok(handle) = crate::web_server::start(web_state, current_port, modes).await {
            let mut h = state.web_server_handle.lock().unwrap();
            *h = Some(handle);
        }
    }

    let _ = rebuild_tray_menu(app);
}

/// Rebuild the tray menu to reflect the current state.
pub fn rebuild_tray_menu(app: &AppHandle) -> tauri::Result<()> {
    let state = app.state::<AppState>();
    let server_running = state.web_server_handle.lock().unwrap().is_some();

    let (web_ui_enabled, opds_enabled) = {
        let conn = state.active_db().and_then(|p| p.get().map_err(Into::into));
        match conn {
            Ok(c) => (
                crate::db::get_setting(&c, "web_ui_enabled")
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some("true"),
                crate::db::get_setting(&c, "opds_enabled")
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some("true"),
            ),
            Err(_) => (false, false),
        }
    };

    let menu = build_tray_menu(app, web_ui_enabled, opds_enabled, server_running)?;

    if let Some(tray) = app.tray_by_id("main") {
        tray.set_menu(Some(menu))?;
    }

    Ok(())
}
