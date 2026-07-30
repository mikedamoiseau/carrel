//! Desktop-side glue for the plugin/hook system (spec 2026-06-12).
//!
//! `folio-core` owns the runtime and manager but stays UI-free; this module
//! provides the `HostServices` implementation (OS notifications) and the
//! Tauri command surface for the Plugins settings panel.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_notification::NotificationExt;

use folio_core::activity::ActivityEvent;
use folio_core::plugins::manifest::is_valid_plugin_id;
use folio_core::plugins::permissions::{self, Permission};
use folio_core::plugins::runtime::{HostServices, RuntimeDeps};
use folio_core::plugins::{PluginInfo, PluginManager, PluginStatus};

use crate::commands::{log_event, AppState};
use crate::db::DbPool;
use crate::error::{FolioError, FolioResult};

/// `HostServices` backed by the Tauri notification plugin.
pub struct DesktopHostServices<R: tauri::Runtime> {
    app: AppHandle<R>,
}

impl<R: tauri::Runtime> DesktopHostServices<R> {
    pub fn new(app: AppHandle<R>) -> Self {
        Self { app }
    }
}

impl<R: tauri::Runtime> HostServices for DesktopHostServices<R> {
    fn notify(&self, title: &str, body: &str) {
        if let Err(e) = self
            .app
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show()
        {
            tracing::warn!(error = %e, "plugin notification failed");
        }
    }

    fn import_from_url(&self, url: &str) -> Result<String, String> {
        let state = self.app.state::<AppState>();
        crate::commands::import_book_from_url(&state, url).map_err(|e| e.to_string())
    }
}

/// Resolve the per-profile plugins directory: `{app_data}/plugins`.
pub fn plugins_dir(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("plugins")
}

/// Build a `PluginManager` bound to `pool` (the active profile's DB).
/// Returns `None` (logged) if initialization fails, so a plugin problem
/// never blocks app startup or a profile switch.
///
/// This does NOT subscribe to the event bus — the bus has no unsubscribe,
/// so a single forwarding subscriber is installed once at startup (see
/// `lib.rs`) and reads whichever manager currently occupies the shared slot.
/// Rebuilding here on profile switch therefore can't leak subscribers.
pub fn init_manager<R: tauri::Runtime>(
    app: &AppHandle<R>,
    data_dir: &std::path::Path,
    pool: DbPool,
) -> Option<Arc<PluginManager>> {
    let dir = plugins_dir(data_dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, "cannot create plugins dir");
        return None;
    }
    let deps = RuntimeDeps {
        pool,
        services: Arc::new(DesktopHostServices::new(app.clone())),
    };
    match PluginManager::new(dir, deps) {
        Ok(manager) => Some(manager),
        Err(e) => {
            tracing::error!(error = %e, "plugin manager init failed");
            None
        }
    }
}

/// The shared slot holding the active-profile plugin manager.
pub type ManagerSlot = Arc<std::sync::Mutex<Option<Arc<PluginManager>>>>;

/// Rebuild the plugin manager for a newly-activated profile and swap it into
/// `slot`. Called from `switch_profile`. Failure is logged, never fatal.
pub fn rebuild_for_profile<R: tauri::Runtime>(
    app: &AppHandle<R>,
    data_dir: &std::path::Path,
    pool: DbPool,
    slot: &ManagerSlot,
) {
    let manager = init_manager(app, data_dir, pool);
    if let Ok(mut guard) = slot.lock() {
        *guard = manager;
    }
}

/// Serializable view of one permission for the consent dialog.
#[derive(Serialize)]
pub struct PermissionView {
    /// Wire id, e.g. `write:tags`.
    pub id: String,
    /// i18n key suffix the frontend maps to a plain-language data category.
    pub category_key: String,
}

#[derive(Serialize)]
pub struct PluginView {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub events: Vec<String>,
    pub permissions: Vec<PermissionView>,
    /// Declared host allowlist for `network` (shown in the consent dialog).
    pub network_hosts: Vec<String>,
    /// `active` | `disabled` | `auto_disabled` | `invalid`.
    pub status: String,
    pub invalid_reason: Option<String>,
    pub needs_consent: bool,
    /// True when the plugin subscribes to `AppStarted` (offer "Run now").
    pub can_run_now: bool,
}

fn permission_category_key(p: Permission) -> &'static str {
    match p {
        Permission::ReadLibrary => "read_library",
        Permission::ReadHighlights => "read_highlights",
        Permission::WriteTags => "write_tags",
        Permission::WriteMetadata => "write_metadata",
        Permission::WriteFiles => "write_files",
        Permission::Notify => "notify",
        Permission::Network => "network",
        Permission::ImportBooks => "import_books",
    }
}

fn to_view(info: PluginInfo) -> PluginView {
    let (status, invalid_reason) = match &info.status {
        PluginStatus::Active => ("active".to_string(), None),
        PluginStatus::Disabled => ("disabled".to_string(), None),
        PluginStatus::AutoDisabled => ("auto_disabled".to_string(), None),
        PluginStatus::Invalid(reason) => ("invalid".to_string(), Some(reason.clone())),
    };
    let can_run_now =
        info.status == PluginStatus::Active && info.subscribe.iter().any(|e| e == "AppStarted");
    PluginView {
        id: info.id,
        name: info.name,
        version: info.version,
        description: info.description,
        author: info.author,
        events: info.subscribe,
        permissions: info
            .permissions
            .into_iter()
            .map(|p| PermissionView {
                id: p.as_str().to_string(),
                category_key: permission_category_key(p).to_string(),
            })
            .collect(),
        network_hosts: info.network_hosts,
        status,
        invalid_reason,
        needs_consent: info.needs_consent,
        can_run_now,
    }
}

fn manager(state: &AppState) -> FolioResult<Arc<PluginManager>> {
    state
        .plugin_manager
        .lock()?
        .clone()
        .ok_or_else(|| FolioError::internal("plugin system not initialized"))
}

#[tauri::command]
pub async fn plugin_list(state: State<'_, AppState>) -> FolioResult<Vec<PluginView>> {
    let mgr = manager(&state)?;
    Ok(mgr.list()?.into_iter().map(to_view).collect())
}

/// One approved permission plus its optional parameter (e.g. the chosen
/// export directory for `write:files`).
#[derive(serde::Deserialize)]
pub struct GrantInput {
    pub permission: String,
    #[serde(default)]
    pub params: Option<String>,
}

/// Validate the export-folder parameter on every `write:files` grant before
/// it is recorded. Each such grant must carry a non-empty path that probes as
/// an actually-writable directory; otherwise the grant set is rejected so an
/// unwritable export folder is never persisted. Grants without a path and all
/// other permissions are unaffected.
fn validate_write_grants(grants: &[(Permission, Option<String>)]) -> FolioResult<()> {
    for (perm, params) in grants {
        if *perm != Permission::WriteFiles {
            continue;
        }
        let path = params.as_deref().map(str::trim).unwrap_or("");
        if path.is_empty() {
            return Err(FolioError::invalid(
                "the 'write files' permission requires choosing an export folder",
            ));
        }
        if !crate::commands::probe_dir_writable(std::path::Path::new(path)) {
            return Err(FolioError::permission(format!(
                "the chosen export folder is not writable: {path}"
            )));
        }
    }
    Ok(())
}

/// Enable a plugin, recording consent first. The frontend shows the consent
/// dialog (and a folder picker for `write:files`) and calls this with the
/// approved grants. The grant set must match the manifest exactly.
#[tauri::command]
pub async fn plugin_enable(
    plugin_id: String,
    grants: Vec<GrantInput>,
    state: State<'_, AppState>,
) -> FolioResult<()> {
    if !is_valid_plugin_id(&plugin_id) {
        return Err(FolioError::invalid("invalid plugin id"));
    }
    let mgr = manager(&state)?;

    // Empty `grants` means "re-enable using the already-recorded consent"
    // (the UI does not re-prompt a plugin whose permissions are unchanged).
    // A non-empty list is fresh consent and replaces the recorded grants.
    let approved: Vec<(Permission, Option<String>)> = if grants.is_empty() {
        let conn = state.active_db()?.get()?;
        permissions::grants_for(&conn, &plugin_id)?
            .into_iter()
            .map(|g| (g.permission, g.params))
            .collect()
    } else {
        grants
            .iter()
            .map(|g| {
                Permission::parse(&g.permission)
                    .map(|p| (p, g.params.clone()))
                    .ok_or_else(|| {
                        FolioError::invalid(format!("unknown permission: {}", g.permission))
                    })
            })
            .collect::<FolioResult<_>>()?
    };

    // The grant set must match the manifest exactly: every required
    // permission approved, nothing beyond the manifest recorded. This stops a
    // crafted IPC call from inflating a plugin's recorded grants.
    let info = mgr
        .list()?
        .into_iter()
        .find(|p| p.id == plugin_id)
        .ok_or_else(|| FolioError::not_found(format!("plugin not found: {plugin_id}")))?;
    let required: Vec<Permission> = info.permissions.clone();
    for req in &required {
        if !approved.iter().any(|(p, _)| p == req) {
            return Err(FolioError::invalid(format!(
                "consent missing for required permission: {}",
                req.as_str()
            )));
        }
    }
    for (got, _) in &approved {
        if !required.contains(got) {
            return Err(FolioError::invalid(format!(
                "permission '{}' is not declared in the plugin manifest",
                got.as_str()
            )));
        }
    }

    // write:files must come with a non-empty export directory that is
    // actually writable. Enforced here (not just in the UI) so a direct
    // invoke can't persist an unwritable export folder.
    validate_write_grants(&approved)?;

    let detail = approved
        .iter()
        .map(|(p, _)| p.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let name = info.name.clone();
    {
        let conn = state.active_db()?.get()?;
        permissions::record_grants(&conn, &plugin_id, &approved, now_unix_secs())?;
    }

    mgr.enable(&plugin_id)?;

    let conn = state.active_db()?.get()?;
    log_event(
        &conn,
        ActivityEvent::PluginEnabled {
            id: plugin_id,
            name,
            detail: format!("granted: {detail}"),
        },
    );
    Ok(())
}

#[tauri::command]
pub async fn plugin_disable(plugin_id: String, state: State<'_, AppState>) -> FolioResult<()> {
    if !is_valid_plugin_id(&plugin_id) {
        return Err(FolioError::invalid("invalid plugin id"));
    }
    let mgr = manager(&state)?;
    let name = mgr
        .list()?
        .into_iter()
        .find(|p| p.id == plugin_id)
        .map(|p| p.name)
        .unwrap_or_else(|| plugin_id.clone());
    mgr.disable(&plugin_id)?;
    let conn = state.active_db()?.get()?;
    log_event(
        &conn,
        ActivityEvent::PluginDisabled {
            id: plugin_id,
            name,
        },
    );
    Ok(())
}

#[tauri::command]
pub async fn plugin_reload(state: State<'_, AppState>) -> FolioResult<()> {
    manager(&state)?.reload()
}

/// Manually trigger an `AppStarted`-driven plugin (the "Run now" button).
#[tauri::command]
pub async fn plugin_run_now(plugin_id: String, state: State<'_, AppState>) -> FolioResult<()> {
    if !is_valid_plugin_id(&plugin_id) {
        return Err(FolioError::invalid("invalid plugin id"));
    }
    manager(&state)?.run_now(&plugin_id)
}

/// Wipe all grants and runtime state for a plugin ("Remove plugin data").
#[tauri::command]
pub async fn plugin_remove_data(plugin_id: String, state: State<'_, AppState>) -> FolioResult<()> {
    if !is_valid_plugin_id(&plugin_id) {
        return Err(FolioError::invalid("invalid plugin id"));
    }
    let mgr = manager(&state)?;
    mgr.disable(&plugin_id)?;
    let conn = state.active_db()?.get()?;
    permissions::remove_plugin_data(&conn, &plugin_id)?;
    Ok(())
}

/// One bundled example plugin, for the "Install example" gallery.
#[derive(Serialize)]
pub struct ExamplePlugin {
    pub id: String,
    pub name: String,
    pub description: String,
    /// True if a folder with this id already exists in the user plugins dir.
    pub installed: bool,
}

/// List the example plugins shipped in app resources.
#[tauri::command]
pub async fn plugin_list_examples(
    app: AppHandle,
    state: State<'_, AppState>,
) -> FolioResult<Vec<ExamplePlugin>> {
    let examples_dir = app
        .path()
        .resolve("resources/example-plugins", BaseDirectory::Resource)
        .map_err(|e| FolioError::internal(format!("cannot resolve example plugins: {e}")))?;
    let user_dir = plugins_dir(&state.data_dir);

    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&examples_dir) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let raw = match std::fs::read_to_string(dir.join("plugin.toml")) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let Ok(manifest) = folio_core::plugins::manifest::parse_manifest(&raw) else {
            continue;
        };
        let installed = user_dir.join(&manifest.id).join("plugin.toml").is_file();
        out.push(ExamplePlugin {
            id: manifest.id,
            name: manifest.name,
            description: manifest.description,
            installed,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Copy a bundled example plugin into the user plugins directory, then
/// rescan. The plugin lands disabled — the user enables it (with consent)
/// from the list like any other plugin.
#[tauri::command]
pub async fn plugin_install_example(
    example_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> FolioResult<()> {
    if !is_valid_plugin_id(&example_id) {
        return Err(FolioError::invalid("invalid plugin id"));
    }
    let src = app
        .path()
        .resolve(
            format!("resources/example-plugins/{example_id}"),
            BaseDirectory::Resource,
        )
        .map_err(|e| FolioError::internal(format!("cannot resolve example: {e}")))?;
    if !src.join("plugin.toml").is_file() {
        return Err(FolioError::not_found(format!(
            "example plugin not found: {example_id}"
        )));
    }
    let dest = plugins_dir(&state.data_dir).join(&example_id);
    if dest.exists() {
        return Err(FolioError::invalid(format!(
            "a plugin named '{example_id}' is already installed"
        )));
    }
    std::fs::create_dir_all(&dest)?;
    for file in ["plugin.toml", "main.rhai", "config.toml"] {
        let from = src.join(file);
        if from.is_file() {
            std::fs::copy(&from, dest.join(file))?;
        }
    }
    manager(&state)?.reload()?;
    Ok(())
}

/// Open the plugins directory in the OS file manager.
#[tauri::command]
pub async fn plugin_open_dir(state: State<'_, AppState>) -> FolioResult<()> {
    let dir = plugins_dir(&state.data_dir);
    std::fs::create_dir_all(&dir)?;
    open::that(&dir).map_err(|e| FolioError::internal(format!("cannot open plugins dir: {e}")))?;
    Ok(())
}

fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use folio_core::plugins::manifest::parse_manifest;
    use folio_core::plugins::runtime::PluginRuntime;

    struct NoopServices;
    impl HostServices for NoopServices {
        fn notify(&self, _title: &str, _body: &str) {}
    }

    /// Every bundled example plugin must parse and compile. Guards against
    /// shipping an example with a bad manifest or a Rhai syntax error.
    #[test]
    fn bundled_example_plugins_parse_and_compile() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/example-plugins");
        let tmp = tempfile::tempdir().unwrap();
        let pool = folio_core::db::create_pool(&tmp.path().join("t.db")).unwrap();

        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            let raw = std::fs::read_to_string(p.join("plugin.toml"))
                .unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            let manifest = parse_manifest(&raw)
                .unwrap_or_else(|e| panic!("{} manifest invalid: {e}", p.display()));
            assert_eq!(
                manifest.id,
                p.file_name().unwrap().to_string_lossy(),
                "manifest id must match folder name"
            );
            let script = std::fs::read_to_string(p.join("main.rhai"))
                .unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            // Grant every declared permission (write:files gets a dummy dir)
            // so all referenced host functions exist at compile time.
            let granted: Vec<(Permission, Option<String>)> = manifest
                .permissions
                .iter()
                .map(|perm| {
                    let params = if *perm == Permission::WriteFiles {
                        Some(tmp.path().to_string_lossy().to_string())
                    } else {
                        None
                    };
                    (*perm, params)
                })
                .collect();
            let deps = RuntimeDeps {
                pool: pool.clone(),
                services: Arc::new(NoopServices),
            };
            PluginRuntime::load(&script, &granted, &manifest.network_hosts, deps)
                .unwrap_or_else(|e| panic!("{} script failed to compile: {e}", manifest.id));
            checked += 1;
        }
        assert!(
            checked >= 3,
            "expected at least 3 example plugins, found {checked}"
        );
    }

    #[test]
    fn validate_write_grants_accepts_writable_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let grants = vec![(
            Permission::WriteFiles,
            Some(tmp.path().to_string_lossy().to_string()),
        )];
        assert!(validate_write_grants(&grants).is_ok());
    }

    #[test]
    fn validate_write_grants_rejects_missing_path() {
        let missing = std::env::temp_dir().join("carrel-no-such-dir-1234567890");
        let grants = vec![(
            Permission::WriteFiles,
            Some(missing.to_string_lossy().to_string()),
        )];
        let err = validate_write_grants(&grants).unwrap_err();
        assert_eq!(err.kind(), "PermissionDenied");
    }

    #[test]
    fn validate_write_grants_rejects_empty_path() {
        let grants = vec![(Permission::WriteFiles, Some("   ".to_string()))];
        let err = validate_write_grants(&grants).unwrap_err();
        assert_eq!(err.kind(), "InvalidInput");
    }

    #[test]
    fn validate_write_grants_ignores_other_permissions() {
        // A non-write:files grant with no path must pass untouched.
        let grants = vec![(Permission::ReadLibrary, None)];
        assert!(validate_write_grants(&grants).is_ok());
    }
}
