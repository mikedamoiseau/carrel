pub mod analytics;
#[cfg(test)]
mod ci_workflow_test;
pub mod commands;
pub mod ipc_metrics;
pub mod observability;
pub mod page_wire;
pub mod plugin_host;
#[cfg(test)]
mod release_workflow_test;
pub mod remote_fs;
#[cfg(test)]
mod tauri_config_test;
pub mod tray;
pub mod update;
pub mod web_server;

// Re-export every module that now lives in folio-core so existing `crate::…`
// call sites in commands.rs, web_server, and tray keep compiling unchanged.
// New code should prefer `use folio_core::…` directly.
pub use folio_core::{
    backup, cbr, cbz, db, enrichment, epub, error, image_util, models, opds, openlibrary,
    page_cache, paths, pdf, providers, sync,
};

use commands::{AppState, LruCache, ProfileState};
use tauri::{Emitter, Manager};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // tauri-plugin-aptabase spawns its background flush task via bare
    // `tokio::spawn` at plugin-init, which requires an *entered* Tokio runtime
    // on the current thread. Folio otherwise uses `tauri::async_runtime::spawn`
    // (handle-based — no entered context needed), so the main thread has no
    // entered runtime by default and the plugin panics ("there is no reactor
    // running"). Enter Tauri's own runtime handle here and hold the guard for
    // the lifetime of `run()` so the plugin can spawn onto that runtime.
    let rt_handle = tauri::async_runtime::handle();
    let _rt_guard = rt_handle.inner().enter();

    #[allow(unused_mut)]
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_aptabase::Builder::new(analytics::APTABASE_APP_KEY).build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ));

    // WebDriver automation plugin — debug builds only
    #[cfg(debug_assertions)]
    {
        builder = builder.plugin(tauri_plugin_webdriver_automation::init());
    }

    builder
        .setup(|app| {
            let db_path = app.path().app_data_dir()?.join("library.db");
            let pool = db::create_pool(&db_path)?;

            // Ensure the library folder exists on first launch.
            {
                let conn = pool.get()?;
                let library_folder = match db::get_setting(&conn, "library_folder").ok().flatten() {
                    Some(f) => f,
                    None => commands::default_library_folder()?,
                };
                let _ = std::fs::create_dir_all(&library_folder);
            }

            // Resolve bundled pdfium library path.
            #[cfg(target_os = "macos")]
            let pdfium_lib_name = "libpdfium.dylib";
            #[cfg(target_os = "linux")]
            let pdfium_lib_name = "libpdfium.so";
            #[cfg(target_os = "windows")]
            let pdfium_lib_name = "pdfium.dll";

            let pdfium_path = app.path().resource_dir().ok().and_then(|d| {
                // Bundled resources preserve their relative path from tauri.conf.json
                let nested = d.join("resources").join(pdfium_lib_name);
                if nested.exists() {
                    return Some(nested);
                }
                // Fallback: flat layout (e.g. custom bundling)
                let flat = d.join(pdfium_lib_name);
                if flat.exists() {
                    return Some(flat);
                }
                // Dev mode fallback: resource_dir() points to target/debug/ where
                // pdfium isn't copied, so check the source resources/ directory.
                let dev_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("resources")
                    .join(pdfium_lib_name);
                if dev_path.exists() {
                    return Some(dev_path);
                }
                None
            });

            pdf::set_pdfium_library_path(pdfium_path);

            // F-2-7: surface provider retry/backoff in the UI. The observer
            // is process-wide and write-once; registered here so every
            // enrichment call site reports without plumbing.
            {
                let handle = app.handle().clone();
                folio_core::http_retry::set_retry_observer(Box::new(move |ev| {
                    let _ = handle.emit("enrichment-retry", ev);
                }));
            }

            // Load existing profile databases
            let data_dir = app.path().app_data_dir()?;
            // Initialize structured logging as early as the data dir allows.
            // Dev logs to stderr; prod writes a daily-rolling file under logs/.
            let log_guard = observability::init_tracing(Some(data_dir.join("logs")));
            let mut profiles = std::collections::HashMap::new();
            if let Ok(entries) = std::fs::read_dir(&data_dir) {
                for entry in entries.flatten() {
                    let fname = entry.file_name().to_string_lossy().to_string();
                    if let Some(name) = fname
                        .strip_prefix("library-")
                        .and_then(|s| s.strip_suffix(".db"))
                    {
                        if let Ok(p) = db::create_pool(&entry.path()) {
                            profiles.insert(name.to_string(), p);
                        }
                    }
                }
            }

            let enrichment_registry = {
                let mut reg = crate::providers::ProviderRegistry::new();
                if let Ok(conn) = pool.get() {
                    if let Ok(Some(json)) = crate::db::get_setting(&conn, "enrichment_providers") {
                        if let Ok(configs) = serde_json::from_str::<
                            std::collections::HashMap<String, crate::providers::ProviderConfig>,
                        >(&json)
                        {
                            for (id, config) in configs {
                                reg.configure_provider(&id, config);
                            }
                        }
                    }
                    if let Ok(Some(order_json)) =
                        crate::db::get_setting(&conn, "enrichment_provider_order")
                    {
                        if let Ok(order) = serde_json::from_str::<Vec<String>>(&order_json) {
                            reg.reorder(&order);
                        }
                    }
                }
                std::sync::Mutex::new(reg)
            };

            // Soft-lock startup check (A-M2, Decision 6 / SB-7): the
            // initially-active profile is entered without a
            // `switch_profile` call, so it must be checked here too —
            // "default" is not exempt. Fail closed on a keychain error
            // (treat as locked, i.e. do NOT auto-unlock) rather than
            // silently exposing the profile (Decision 7). When unlocked,
            // seed the session's unlocked set so a normal, never-locked
            // startup behaves exactly as before this feature existed. The
            // frontend detects a locked startup profile via
            // `profile_lock_status` (checked against `get_profiles`'
            // active entry) and presents the unlock prompt.
            let mut unlocked_profiles = std::collections::HashSet::new();
            if !folio_core::profile_lock::has_lock("default").unwrap_or(true) {
                unlocked_profiles.insert("default".to_string());
            }

            app.manage(AppState {
                shared_active_pool: std::sync::Arc::new(std::sync::Mutex::new(pool.clone())),
                shared_active_profile_name: std::sync::Arc::new(std::sync::Mutex::new(
                    "default".to_string(),
                )),
                shared_pin_hash: std::sync::Arc::new(std::sync::Mutex::new(
                    crate::web_server::auth::load_pin_hash(),
                )),
                unlocked_profiles: std::sync::Arc::new(std::sync::Mutex::new(unlocked_profiles)),
                // B-M1: private mode defaults off — no frontend toggle
                // exists yet (B-M2), so main stays shippable with tracking
                // behaving exactly as before this feature landed.
                private_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                active_foreground_renders: std::sync::Arc::new(
                    std::sync::atomic::AtomicUsize::new(0),
                ),
                profile_lifecycle: std::sync::Arc::new(tokio::sync::Mutex::new(())),
                db: pool,
                profile_state: std::sync::Mutex::new(ProfileState {
                    active: "default".to_string(),
                    pools: profiles,
                }),
                data_dir,
                cache_dir: app.path().app_cache_dir()?,
                epub_cache: std::sync::Arc::new(std::sync::Mutex::new(LruCache::new(5))),
                #[cfg(feature = "mobi")]
                mobi_cache: std::sync::Arc::new(std::sync::Mutex::new({
                    let mut c = LruCache::new(5);
                    // Cap by total bytes — owned MOBI bytes (chapter HTML +
                    // image resources) can run hundreds of MB on illustrated
                    // AZW3s, so entry count alone is not a sufficient guard.
                    // 200 MB matches the PDF cache budget below.
                    c.set_max_bytes(200 * 1024 * 1024);
                    c
                })),
                enrichment_registry,
                web_server_handle: std::sync::Mutex::new(None),
                ipc_metrics: crate::ipc_metrics::IpcMetrics::new(500, 500.0),
                plugin_manager: std::sync::Arc::new(std::sync::Mutex::new(None)),
                _log_guard: log_guard,
                dictionary_pool: std::sync::Mutex::new(None),
                dictionary_downloading: std::sync::atomic::AtomicBool::new(false),
                pending_manual_update_check: std::sync::Mutex::new(false),
                startup_update_check_taken: std::sync::atomic::AtomicBool::new(false),
                update_check: crate::update::UpdateCheckState::new(),
            });

            // Usage analytics (opt-in): fire one anonymous `app_started` per
            // process launch iff the user enabled analytics. App-global consent
            // is read from `{data_dir}/analytics.json`; fail-closed. Direct
            // call (not the event bus) so there is no ordering race and no
            // profile-switch re-fire. `data_dir` was moved into AppState above,
            // so re-derive the path here.
            {
                let analytics_dir = app.path().app_data_dir()?;
                analytics::maybe_track_app_started(app, &analytics_dir);
            }

            // Initialize system tray
            if let Err(e) = tray::setup_tray(&app.handle().clone()) {
                log::error!("Failed to initialize tray: {}", e);
            }

            // Backfill grid thumbnails for covers imported before the
            // thumbnail feature. Runs off-thread so it never blocks startup;
            // re-checks are cheap (header probe only) for already-handled
            // covers, so running every launch is fine.
            let thumb_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let (pool, storage) = {
                    let state = thumb_handle.state::<AppState>();
                    match state.covers_storage() {
                        Ok(s) => (state.db.clone(), s),
                        Err(e) => {
                            log::warn!("thumbnail backfill: no covers storage: {e}");
                            return;
                        }
                    }
                };
                tauri::async_runtime::spawn_blocking(move || {
                    commands::run_thumbnail_backfill(pool, storage);
                });
            });

            // Auto-start web server based on persisted modes. Runs the legacy
            // migration on first launch with new code so existing users with
            // web_server_enabled=true keep getting Both mode.
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = app_handle.state::<AppState>();

                // Migrate legacy setting if present.
                // `active_db_unchecked`: startup web auto-start must read its
                // own config even when the active profile is soft-locked
                // (A-M2). The server it starts is still gated by
                // `profile_lock_gate`, so it stays dark until unlock — this
                // only lets it come up so it serves the moment the profile
                // is unlocked, without an app restart.
                {
                    let conn = match state
                        .active_db_unchecked()
                        .and_then(|p| p.get().map_err(Into::into))
                    {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    let _ = commands::migrate_web_server_setting(&conn);
                }

                // Read the new settings.
                let modes = {
                    let conn = match state
                        .active_db_unchecked()
                        .and_then(|p| p.get().map_err(Into::into))
                    {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    let web_ui = db::get_setting(&conn, "web_ui_enabled")
                        .ok()
                        .flatten()
                        .as_deref()
                        == Some("true");
                    let opds = db::get_setting(&conn, "opds_enabled")
                        .ok()
                        .flatten()
                        .as_deref()
                        == Some("true");
                    web_server::ServerModes { web_ui, opds }
                };

                if !modes.any() {
                    return;
                }

                let port = {
                    let conn = match state
                        .active_db_unchecked()
                        .and_then(|p| p.get().map_err(Into::into))
                    {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    db::get_setting(&conn, "web_server_port")
                        .ok()
                        .flatten()
                        .and_then(|s| s.parse::<u16>().ok())
                        .unwrap_or(web_server::DEFAULT_PORT)
                };
                let pin_hash = web_server::auth::load_pin_hash();
                {
                    let mut ph = match state.shared_pin_hash.lock() {
                        Ok(g) => g,
                        Err(_) => {
                            log::error!("pin-hash mutex poisoned; skipping web-server auto-start");
                            return;
                        }
                    };
                    *ph = pin_hash;
                }
                let web_state = web_server::WebState {
                    pool: state.shared_active_pool.clone(),
                    data_dir: state.data_dir.clone(),
                    pin_hash: state.shared_pin_hash.clone(),
                    sessions: std::sync::Arc::new(std::sync::Mutex::new(
                        std::collections::HashMap::new(),
                    )),
                    login_limiter: std::sync::Arc::new(web_server::auth::RateLimiter::new(5, 300)),
                    active_profile_name: state.shared_active_profile_name.clone(),
                    unlocked_profiles: state.unlocked_profiles.clone(),
                    private_mode: state.private_mode.clone(),
                };
                if let Ok(handle) = web_server::start(web_state, port, modes).await {
                    {
                        let mut h = match state.web_server_handle.lock() {
                            Ok(g) => g,
                            Err(_) => {
                                log::error!("web-server handle mutex poisoned");
                                return;
                            }
                        };
                        *h = Some(handle);
                    }
                    // Drop the guard before rebuild_tray_menu (which locks
                    // the same mutex) — otherwise we deadlock at boot.
                    let _ = tray::rebuild_tray_menu(&app_handle);
                }
            });

            // Plugin/hook system M2: build the plugin manager for the active
            // profile and install ONE forwarding bus subscriber that reads
            // whichever manager currently occupies the shared slot. The slot
            // is swapped on profile switch (the bus has no unsubscribe, so we
            // must never subscribe per-manager). Done BEFORE emitting
            // AppStarted so AppStarted-subscribing plugins receive it.
            // Failure is logged, never fatal.
            {
                let (data_dir, pool, slot, active_unlocked) = {
                    let state = app.state::<AppState>();
                    let active = state
                        .profile_state
                        .lock()
                        .map(|ps| ps.active.clone())
                        .unwrap_or_else(|_| "default".to_string());
                    (
                        state.data_dir.clone(),
                        state.db.clone(),
                        state.plugin_manager.clone(),
                        state.is_unlocked(&active),
                    )
                };
                // Install the single, permanent forwarding subscriber
                // unconditionally — the bus has no unsubscribe, and the slot
                // may be populated later by `unlock_profile` once a
                // locked-at-boot profile is unlocked.
                let bus_slot = slot.clone();
                folio_core::events::bus().subscribe(Box::new(
                    move |event: &folio_core::events::FolioEvent| {
                        let mgr = bus_slot.lock().ok().and_then(|g| g.clone());
                        if let Some(mgr) = mgr {
                            mgr.handle_event(event);
                        }
                    },
                ));

                // Soft-lock (A-M2, D-6/SB-7): only build the plugin manager
                // and emit `AppStarted` when the active profile is unlocked.
                // If the startup profile is locked, an `AppStarted`-subscribing
                // plugin with `read:library`/`read:highlights`/write would
                // otherwise read or mutate the locked profile before unlock —
                // the same bypass the `active_db()` gate closes for IPC.
                // `unlock_profile` rebuilds the manager and emits `AppStarted`
                // once the active profile is unlocked.
                if active_unlocked {
                    let manager = plugin_host::init_manager(&app.handle().clone(), &data_dir, pool);
                    if let Ok(mut guard) = slot.lock() {
                        *guard = manager;
                    }
                    // Plugin/hook system M1: app-lifecycle hook point.
                    folio_core::events::bus().emit(folio_core::events::FolioEvent::AppStarted);
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::import_book,
            commands::get_supported_formats,
            commands::check_dir_writable,
            commands::get_library,
            commands::get_library_grid,
            commands::get_recently_read,
            commands::update_book_metadata,
            commands::set_want_to_read,
            commands::get_all_tags,
            commands::get_book_tags,
            commands::add_tag_to_book,
            commands::remove_tag_from_book,
            commands::get_all_book_tags,
            commands::scan_folder_for_books,
            commands::add_highlight,
            commands::get_highlights,
            commands::get_chapter_highlights,
            commands::update_highlight_note,
            commands::remove_highlight,
            commands::export_highlights_markdown,
            commands::search_highlights,
            commands::record_reading_session,
            commands::get_reading_stats,
            commands::get_book_reading_time,
            commands::get_book_reading_stats,
            commands::export_collection_markdown,
            commands::export_collection_json,
            commands::export_library,
            commands::import_library_backup,
            commands::get_book,
            commands::remove_book,
            commands::get_chapter_content,
            commands::get_all_chapters,
            commands::get_chapter_word_counts,
            commands::get_chapter_metadata_batch,
            commands::search_book_content,
            commands::get_toc,
            commands::get_reading_progress,
            commands::get_all_reading_progress,
            commands::save_reading_progress,
            commands::set_private_mode,
            commands::get_private_mode,
            commands::get_bookmarks,
            commands::add_bookmark,
            commands::remove_bookmark,
            commands::update_bookmark,
            commands::get_comic_page_count,
            commands::get_comic_page_bytes,
            commands::check_pdf_support,
            commands::get_pdf_page_count,
            commands::get_pdf_page_bytes,
            commands::get_pdf_page_glyphs,
            commands::get_pdf_page_text,
            commands::prepare_pdf,
            commands::create_collection,
            commands::update_collection,
            commands::get_collections,
            commands::delete_collection,
            commands::add_book_to_collection,
            commands::remove_book_from_collection,
            commands::get_books_in_collection,
            commands::get_books_in_collection_grid,
            commands::get_library_folder,
            commands::get_library_folder_info,
            commands::set_library_folder,
            commands::get_profiles,
            commands::create_profile,
            commands::switch_profile,
            commands::delete_profile,
            commands::set_profile_lock,
            commands::remove_profile_lock,
            commands::unlock_profile,
            commands::reset_profile_lock,
            commands::profile_lock_status,
            plugin_host::plugin_list,
            plugin_host::plugin_enable,
            plugin_host::plugin_disable,
            plugin_host::plugin_reload,
            plugin_host::plugin_run_now,
            plugin_host::plugin_remove_data,
            plugin_host::plugin_open_dir,
            plugin_host::plugin_list_examples,
            plugin_host::plugin_install_example,
            commands::search_openlibrary,
            commands::enrich_book_from_openlibrary,
            commands::get_opds_catalogs,
            commands::add_opds_catalog,
            commands::remove_opds_catalog,
            commands::get_discover_books,
            commands::browse_opds,
            commands::search_all_catalogs,
            commands::download_opds_book,
            commands::get_backup_providers,
            commands::save_backup_config,
            commands::test_backup_connection,
            commands::get_backup_config,
            commands::run_backup,
            commands::get_backup_status,
            commands::start_scan,
            commands::cancel_scan,
            commands::start_files_import,
            commands::start_folder_import,
            commands::cancel_import,
            commands::is_import_running,
            commands::scan_single_book,
            commands::queue_book_for_scan,
            commands::get_setting_value,
            commands::set_setting_value,
            analytics::get_analytics_consent,
            analytics::set_analytics_consent,
            commands::get_dictionary_status,
            commands::download_dictionary,
            commands::delete_dictionary,
            commands::lookup_word,
            commands::log_vocabulary_word,
            commands::list_vocabulary,
            commands::delete_vocabulary_word,
            commands::clear_vocabulary,
            commands::get_due_vocabulary,
            commands::record_vocabulary_review,
            commands::get_feature_flags,
            commands::set_feature_flag,
            commands::get_feature_flag_value,
            commands::delete_feature_flag,
            commands::get_enrichment_providers,
            commands::set_enrichment_provider_config,
            commands::set_enrichment_provider_order,
            commands::get_activity_log,
            commands::export_activity_log,
            commands::prune_activity_log,
            commands::get_login_history,
            commands::preview_collection_rules,
            commands::get_collection_suggestions,
            commands::import_custom_font,
            commands::get_custom_fonts,
            commands::remove_custom_font,
            commands::get_series,
            commands::copy_to_library,
            commands::check_file_exists,
            commands::cleanup_library,
            commands::list_auto_backups,
            commands::prepare_comic,
            commands::get_unified_cache_stats,
            commands::clear_all_caches,
            commands::clear_page_cache,
            commands::sync_pull_book,
            commands::sync_push_book,
            commands::web_server_set_modes,
            commands::web_server_status,
            commands::web_server_set_pin,
            commands::web_server_get_qr,
            commands::bulk_delete_books,
            commands::bulk_add_to_collection,
            commands::bulk_add_tag,
            commands::bulk_update_metadata,
            commands::get_autostart_enabled,
            commands::set_autostart_enabled,
            commands::get_ipc_metrics,
            commands::save_quote_card_png,
            commands::check_for_update,
            commands::take_pending_manual_update_check,
            commands::take_startup_update_check,
        ])
        .on_window_event(|window, event| {
            match event {
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    use tauri_plugin_autostart::ManagerExt;
                    let autostart_enabled = window
                        .app_handle()
                        .autolaunch()
                        .is_enabled()
                        .unwrap_or(false);
                    let tray_available = window.app_handle().tray_by_id("main").is_some();

                    if autostart_enabled && tray_available {
                        api.prevent_close();
                        // Minimize instead of hide — minimized windows keep
                        // the macOS event loop alive so the tray menu works.
                        let _ = window.minimize();
                    }
                }
                tauri::WindowEvent::Destroyed => {
                    // R5-1: Graceful shutdown — stop web server when app exits
                    let state = window.state::<AppState>();
                    let handle = state
                        .web_server_handle
                        .lock()
                        .ok()
                        .and_then(|mut h| h.take());
                    if let Some(h) = handle {
                        web_server::stop(h);
                    }
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let tauri::RunEvent::ExitRequested { api, code, .. } = &event {
                // code == None means macOS auto-exit because no visible windows.
                // Prevent this when autostart is enabled (tray should stay alive).
                if code.is_none() {
                    use tauri_plugin_autostart::ManagerExt;
                    let autostart_enabled = app_handle.autolaunch().is_enabled().unwrap_or(false);
                    let tray_available = app_handle.tray_by_id("main").is_some();
                    if autostart_enabled && tray_available {
                        api.prevent_exit();
                    }
                }
            }
        });
}
