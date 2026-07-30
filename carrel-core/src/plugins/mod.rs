//! Plugin/hook system (spec 2026-06-12). M2 ships manifest parsing, the
//! permission/grant model, the Rhai script runtime, and the manager that
//! wires plugins onto the event bus.
//!
//! Grant/state persistence manages its own two tables (`plugin_grants`,
//! `plugin_state`) via `permissions::ensure_plugin_schema` instead of
//! `db::run_schema` — additive `CREATE TABLE IF NOT EXISTS` executed when the
//! plugin manager initializes, so the module stays self-contained.

pub mod manifest;
pub mod permissions;
pub mod runtime;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::activity::ActivityEvent;
use crate::db;
use crate::error::{CarrelError, CarrelResult};
use crate::events::{CarrelEvent, EventBus};
use crate::models::ActivityEntry;
use manifest::{parse_manifest, PluginManifest};
use permissions::Permission;
use runtime::{PluginRuntime, RuntimeDeps};

/// Consecutive dispatch errors before a plugin is auto-disabled (spec §4.4).
pub const ERROR_THRESHOLD: u32 = 5;

/// UI-facing status of a discovered plugin.
#[derive(Debug, Clone, PartialEq)]
pub enum PluginStatus {
    Active,
    Disabled,
    AutoDisabled,
    Invalid(String),
}

/// UI-facing listing entry for one plugin folder.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub subscribe: Vec<String>,
    pub permissions: Vec<Permission>,
    /// Declared host allowlist for the `network` permission (empty otherwise).
    pub network_hosts: Vec<String>,
    pub status: PluginStatus,
    /// True when enabling requires the consent dialog (ungranted perms).
    pub needs_consent: bool,
}

struct Discovered {
    id: String,
    manifest: Result<PluginManifest, String>,
}

struct ActivePlugin {
    /// `Arc` so `handle_event` can snapshot the runtime and dispatch the
    /// script WITHOUT holding the manager lock (a slow script must not
    /// freeze list/enable/disable/reload). Rhai's `sync` feature makes the
    /// runtime `Send + Sync`.
    runtime: Arc<PluginRuntime>,
    subscribe: HashSet<String>,
}

struct Inner {
    discovered: Vec<Discovered>,
    active: HashMap<String, ActivePlugin>,
}

/// Discovers plugin folders, loads enabled plugins, and dispatches bus
/// events into their scripts.
pub struct PluginManager {
    plugins_dir: PathBuf,
    deps: RuntimeDeps,
    inner: Mutex<Inner>,
}

impl PluginManager {
    /// Scan `plugins_dir`, ensure the grant schema, and load every plugin
    /// that was enabled in a previous session.
    pub fn new(plugins_dir: PathBuf, deps: RuntimeDeps) -> CarrelResult<Arc<Self>> {
        {
            let conn = deps.pool.get()?;
            permissions::ensure_plugin_schema(&conn)?;
        }
        let manager = Arc::new(Self {
            plugins_dir,
            deps,
            inner: Mutex::new(Inner {
                discovered: Vec::new(),
                active: HashMap::new(),
            }),
        });
        manager.reload()?;
        Ok(manager)
    }

    /// All discovered plugins (valid and invalid) joined with their state.
    pub fn list(&self) -> CarrelResult<Vec<PluginInfo>> {
        let conn = self.deps.pool.get()?;
        let inner = self.lock_inner();
        let mut out = Vec::with_capacity(inner.discovered.len());
        for d in &inner.discovered {
            let info = match &d.manifest {
                Err(reason) => PluginInfo {
                    id: d.id.clone(),
                    name: d.id.clone(),
                    version: String::new(),
                    description: String::new(),
                    author: String::new(),
                    subscribe: Vec::new(),
                    permissions: Vec::new(),
                    network_hosts: Vec::new(),
                    status: PluginStatus::Invalid(reason.clone()),
                    needs_consent: false,
                },
                Ok(m) => {
                    let state = permissions::plugin_state(&conn, &d.id)?;
                    let status = if inner.active.contains_key(&d.id) {
                        PluginStatus::Active
                    } else if state.as_ref().is_some_and(|s| s.auto_disabled) {
                        PluginStatus::AutoDisabled
                    } else {
                        PluginStatus::Disabled
                    };
                    PluginInfo {
                        id: m.id.clone(),
                        name: m.name.clone(),
                        version: m.version.clone(),
                        description: m.description.clone(),
                        author: m.author.clone(),
                        subscribe: m.subscribe.clone(),
                        permissions: m.permissions.clone(),
                        network_hosts: m.network_hosts.clone(),
                        status,
                        needs_consent: permissions::needs_consent(&conn, &d.id, &m.permissions)?,
                    }
                }
            };
            out.push(info);
        }
        Ok(out)
    }

    /// Enable a plugin. Fails when the manifest is invalid or consent for
    /// its permissions has not been recorded yet (`needs_consent`).
    pub fn enable(&self, plugin_id: &str) -> CarrelResult<()> {
        let conn = self.deps.pool.get()?;
        let mut inner = self.lock_inner();
        let manifest = match inner.discovered.iter().find(|d| d.id == plugin_id) {
            Some(d) => match &d.manifest {
                Ok(m) => m.clone(),
                Err(reason) => {
                    return Err(CarrelError::invalid(format!(
                        "plugin '{plugin_id}' is invalid: {reason}"
                    )))
                }
            },
            None => {
                return Err(CarrelError::not_found(format!(
                    "plugin not found: {plugin_id}"
                )))
            }
        };
        if permissions::needs_consent(&conn, plugin_id, &manifest.permissions)? {
            return Err(CarrelError::invalid(format!(
                "plugin '{plugin_id}' requires consent for its permissions"
            )));
        }

        let granted: Vec<(Permission, Option<String>)> = permissions::grants_for(&conn, plugin_id)?
            .into_iter()
            .filter(|g| manifest.permissions.contains(&g.permission))
            .map(|g| (g.permission, g.params))
            .collect();
        let active = load_active(&self.plugins_dir, &manifest, &granted, &self.deps)?;
        permissions::set_plugin_enabled(&conn, plugin_id, true)?;
        inner.active.insert(plugin_id.to_string(), active);
        Ok(())
    }

    /// Disable a plugin and unload its runtime.
    pub fn disable(&self, plugin_id: &str) -> CarrelResult<()> {
        let conn = self.deps.pool.get()?;
        let mut inner = self.lock_inner();
        inner.active.remove(plugin_id);
        permissions::set_plugin_enabled(&conn, plugin_id, false)?;
        Ok(())
    }

    /// Manually fire `AppStarted` to one active plugin (the "Run now" button
    /// for `AppStarted`-triggered plugins, e.g. the OPDS auto-downloader,
    /// which v1 has no scheduler for). Errors if the plugin is not active.
    pub fn run_now(&self, plugin_id: &str) -> CarrelResult<()> {
        let runtime = {
            let inner = self.lock_inner();
            let plugin = inner.active.get(plugin_id).ok_or_else(|| {
                CarrelError::invalid(format!("plugin '{plugin_id}' is not active"))
            })?;
            if !plugin.subscribe.contains("AppStarted") {
                return Err(CarrelError::invalid(format!(
                    "plugin '{plugin_id}' does not subscribe to AppStarted"
                )));
            }
            Arc::clone(&plugin.runtime)
        };
        runtime
            .dispatch(&CarrelEvent::AppStarted)
            .map_err(|e| CarrelError::internal(format!("plugin run failed: {e}")))
    }

    /// Rescan the plugins directory and reload enabled plugins.
    pub fn reload(&self) -> CarrelResult<()> {
        let conn = self.deps.pool.get()?;
        let discovered = discover(&self.plugins_dir);

        let mut active = HashMap::new();
        for d in &discovered {
            let Ok(manifest) = &d.manifest else { continue };
            let enabled = permissions::plugin_state(&conn, &d.id)?
                .map(|s| s.enabled)
                .unwrap_or(false);
            if !enabled {
                continue;
            }
            let granted: Vec<(Permission, Option<String>)> = permissions::grants_for(&conn, &d.id)?
                .into_iter()
                .filter(|g| manifest.permissions.contains(&g.permission))
                .map(|g| (g.permission, g.params))
                .collect();
            match load_active(&self.plugins_dir, manifest, &granted, &self.deps) {
                Ok(plugin) => {
                    active.insert(d.id.clone(), plugin);
                }
                Err(e) => {
                    tracing::error!(plugin = %d.id, error = %e, "failed to load enabled plugin");
                }
            }
        }

        let mut inner = self.lock_inner();
        inner.discovered = discovered;
        inner.active = active;
        Ok(())
    }

    /// Dispatch one event to every active plugin subscribed to it. Errors
    /// count per plugin (the `plugin_state` table is the single source of
    /// truth, so a `reload` can't reset the auto-disable timer); at
    /// [`ERROR_THRESHOLD`] consecutive failures the plugin is auto-disabled
    /// and unloaded.
    pub fn handle_event(&self, event: &CarrelEvent) {
        let name = event.name();

        // Snapshot the subscribed runtimes, then drop the lock before
        // running any script — a 5s wall-clock-budget script must not block
        // list/enable/disable/reload. Re-acquire only to mutate `active`.
        let targets: Vec<(String, Arc<PluginRuntime>)> = {
            let inner = self.lock_inner();
            inner
                .active
                .iter()
                .filter(|(_, p)| p.subscribe.contains(name))
                .map(|(id, p)| (id.clone(), Arc::clone(&p.runtime)))
                .collect()
        };

        let mut to_disable: Vec<String> = Vec::new();
        for (id, runtime) in &targets {
            match runtime.dispatch(event) {
                Ok(()) => {
                    if let Ok(conn) = self.deps.pool.get() {
                        let _ = permissions::reset_plugin_errors(&conn, id);
                    }
                }
                Err(e) => {
                    tracing::error!(plugin = %id, error = %e, "plugin dispatch failed");
                    let count = self
                        .deps
                        .pool
                        .get()
                        .ok()
                        .and_then(|conn| permissions::record_plugin_error(&conn, id).ok())
                        .unwrap_or(0);
                    if count >= ERROR_THRESHOLD {
                        to_disable.push(id.clone());
                    }
                }
            }
        }

        if to_disable.is_empty() {
            return;
        }
        let mut inner = self.lock_inner();
        for id in to_disable {
            inner.active.remove(&id);
            if let Ok(conn) = self.deps.pool.get() {
                let _ = permissions::set_auto_disabled(&conn, &id);
                let fields = ActivityEvent::PluginAutoDisabled {
                    id: id.clone(),
                    detail: format!("disabled after {ERROR_THRESHOLD} consecutive errors"),
                }
                .into_fields();
                let _ = db::insert_activity(
                    &conn,
                    &ActivityEntry {
                        id: uuid::Uuid::new_v4().to_string(),
                        timestamp: now_unix_secs(),
                        action: fields.action.to_string(),
                        entity_type: fields.entity_type.to_string(),
                        entity_id: fields.entity_id,
                        entity_name: fields.entity_name,
                        detail: fields.detail,
                    },
                );
            }
        }
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Subscribe this manager to a bus. Call once at startup.
    pub fn attach_to_bus(self: &Arc<Self>, bus: &EventBus) {
        let manager = Arc::clone(self);
        bus.subscribe(Box::new(move |event: &CarrelEvent| {
            manager.handle_event(event);
        }));
    }
}

/// Read every direct subdirectory of `plugins_dir` as a plugin candidate.
fn discover(plugins_dir: &std::path::Path) -> Vec<Discovered> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(plugins_dir) else {
        return out;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    for dir in dirs {
        let folder = dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let manifest = validate_plugin_dir(&dir, &folder);
        out.push(Discovered {
            id: folder,
            manifest,
        });
    }
    out
}

fn validate_plugin_dir(dir: &std::path::Path, folder: &str) -> Result<PluginManifest, String> {
    let raw = std::fs::read_to_string(dir.join("plugin.toml"))
        .map_err(|e| format!("cannot read plugin.toml: {e}"))?;
    let manifest = parse_manifest(&raw).map_err(|e| e.to_string())?;
    if manifest.id != folder {
        return Err(format!(
            "manifest id '{}' does not match folder name '{folder}'",
            manifest.id
        ));
    }
    if !dir.join("main.rhai").is_file() {
        return Err("main.rhai is missing".to_string());
    }
    if let Some(min) = &manifest.min_app_version {
        if version_lt(env!("CARGO_PKG_VERSION"), min) {
            return Err(format!(
                "requires app version {min} or newer (this is {})",
                env!("CARGO_PKG_VERSION")
            ));
        }
    }
    Ok(manifest)
}

/// Naive numeric semver comparison — enough for `min_app_version` gating.
fn version_lt(current: &str, min: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split('.')
            .map(|part| {
                part.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect()
    };
    let (c, m) = (parse(current), parse(min));
    for i in 0..c.len().max(m.len()) {
        let (a, b) = (
            c.get(i).copied().unwrap_or(0),
            m.get(i).copied().unwrap_or(0),
        );
        if a != b {
            return a < b;
        }
    }
    false
}

fn load_active(
    plugins_dir: &std::path::Path,
    manifest: &PluginManifest,
    granted: &[(Permission, Option<String>)],
    deps: &RuntimeDeps,
) -> CarrelResult<ActivePlugin> {
    let script = std::fs::read_to_string(plugins_dir.join(&manifest.id).join("main.rhai"))?;
    let runtime = PluginRuntime::load(&script, granted, &manifest.network_hosts, deps.clone())?;
    Ok(ActivePlugin {
        runtime: Arc::new(runtime),
        subscribe: manifest.subscribe.iter().cloned().collect(),
    })
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
    use crate::events::ImportSource;
    use crate::models::BookFormat;
    use runtime::HostServices;
    use std::time::Duration;

    struct MockServices {
        notes: Mutex<Vec<(String, String)>>,
    }

    impl HostServices for MockServices {
        fn notify(&self, title: &str, body: &str) {
            self.notes
                .lock()
                .unwrap()
                .push((title.to_string(), body.to_string()));
        }
    }

    struct Fixture {
        plugins_dir: PathBuf,
        deps: RuntimeDeps,
        services: Arc<MockServices>,
        _dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        let pool = db::create_pool(&dir.path().join("t.db")).unwrap();
        let services = Arc::new(MockServices {
            notes: Mutex::new(Vec::new()),
        });
        Fixture {
            plugins_dir,
            deps: RuntimeDeps {
                pool,
                services: services.clone(),
            },
            services,
            _dir: dir,
        }
    }

    fn write_plugin(f: &Fixture, id: &str, manifest: &str, script: &str) {
        let dir = f.plugins_dir.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.toml"), manifest).unwrap();
        std::fs::write(dir.join("main.rhai"), script).unwrap();
    }

    fn notify_manifest(id: &str) -> String {
        format!(
            r#"
[plugin]
id = "{id}"
name = "Test {id}"
version = "1.0.0"

[events]
subscribe = ["BookImported"]

[permissions]
required = ["notify"]
"#
        )
    }

    fn grant_notify(f: &Fixture, id: &str) {
        let conn = f.deps.pool.get().unwrap();
        permissions::ensure_plugin_schema(&conn).unwrap();
        permissions::record_grants(&conn, id, &[(Permission::Notify, None)], 1).unwrap();
    }

    fn imported(book_id: &str) -> CarrelEvent {
        CarrelEvent::BookImported {
            book_id: book_id.into(),
            format: BookFormat::Epub,
            source: ImportSource::Manual,
        }
    }

    #[test]
    fn discovery_lists_valid_plugin_disabled_with_consent_needed() {
        let f = fixture();
        write_plugin(
            &f,
            "test-notify",
            &notify_manifest("test-notify"),
            r#"fn on_event(e) { notify("t", e.book_id); }"#,
        );
        let mgr = PluginManager::new(f.plugins_dir.clone(), f.deps.clone()).unwrap();
        let list = mgr.list().unwrap();
        assert_eq!(list.len(), 1);
        let p = &list[0];
        assert_eq!(p.id, "test-notify");
        assert_eq!(p.status, PluginStatus::Disabled);
        assert!(p.needs_consent);
        assert_eq!(p.permissions, vec![Permission::Notify]);
    }

    #[test]
    fn invalid_manifest_is_listed_with_reason() {
        let f = fixture();
        write_plugin(&f, "broken-one", "not toml {{{", "fn on_event(e) {}");
        let mgr = PluginManager::new(f.plugins_dir.clone(), f.deps.clone()).unwrap();
        let list = mgr.list().unwrap();
        assert_eq!(list.len(), 1);
        match &list[0].status {
            PluginStatus::Invalid(reason) => assert!(!reason.is_empty()),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn folder_and_manifest_id_mismatch_is_invalid() {
        let f = fixture();
        write_plugin(
            &f,
            "folder-name",
            &notify_manifest("other-name"),
            "fn on_event(e) {}",
        );
        let mgr = PluginManager::new(f.plugins_dir.clone(), f.deps.clone()).unwrap();
        match &mgr.list().unwrap()[0].status {
            PluginStatus::Invalid(reason) => assert!(reason.contains("folder"), "got: {reason}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn missing_script_is_invalid() {
        let f = fixture();
        let dir = f.plugins_dir.join("no-script");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.toml"), notify_manifest("no-script")).unwrap();
        let mgr = PluginManager::new(f.plugins_dir.clone(), f.deps.clone()).unwrap();
        match &mgr.list().unwrap()[0].status {
            PluginStatus::Invalid(reason) => {
                assert!(reason.contains("main.rhai"), "got: {reason}")
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn future_min_app_version_is_invalid() {
        let f = fixture();
        let manifest = notify_manifest("too-new").replace(
            "version = \"1.0.0\"",
            "version = \"1.0.0\"\nmin_app_version = \"99.0.0\"",
        );
        write_plugin(&f, "too-new", &manifest, "fn on_event(e) {}");
        let mgr = PluginManager::new(f.plugins_dir.clone(), f.deps.clone()).unwrap();
        match &mgr.list().unwrap()[0].status {
            PluginStatus::Invalid(reason) => assert!(reason.contains("99.0.0"), "got: {reason}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn enable_without_consent_fails_and_with_consent_dispatches() {
        let f = fixture();
        write_plugin(
            &f,
            "test-notify",
            &notify_manifest("test-notify"),
            r#"fn on_event(e) { notify("got", e.book_id); }"#,
        );
        let mgr = PluginManager::new(f.plugins_dir.clone(), f.deps.clone()).unwrap();

        assert!(mgr.enable("test-notify").is_err());

        grant_notify(&f, "test-notify");
        mgr.enable("test-notify").unwrap();
        assert_eq!(mgr.list().unwrap()[0].status, PluginStatus::Active);

        mgr.handle_event(&imported("b1"));
        assert_eq!(
            f.services.notes.lock().unwrap().as_slice(),
            &[("got".to_string(), "b1".to_string())]
        );
    }

    #[test]
    fn events_outside_subscription_are_not_dispatched() {
        let f = fixture();
        write_plugin(
            &f,
            "test-notify",
            &notify_manifest("test-notify"),
            r#"fn on_event(e) { notify("got", e.type); }"#,
        );
        grant_notify(&f, "test-notify");
        let mgr = PluginManager::new(f.plugins_dir.clone(), f.deps.clone()).unwrap();
        mgr.enable("test-notify").unwrap();

        mgr.handle_event(&CarrelEvent::AppStarted); // not subscribed
        assert!(f.services.notes.lock().unwrap().is_empty());
    }

    #[test]
    fn disable_stops_dispatch() {
        let f = fixture();
        write_plugin(
            &f,
            "test-notify",
            &notify_manifest("test-notify"),
            r#"fn on_event(e) { notify("got", e.book_id); }"#,
        );
        grant_notify(&f, "test-notify");
        let mgr = PluginManager::new(f.plugins_dir.clone(), f.deps.clone()).unwrap();
        mgr.enable("test-notify").unwrap();
        mgr.disable("test-notify").unwrap();

        mgr.handle_event(&imported("b1"));
        assert!(f.services.notes.lock().unwrap().is_empty());
        assert_eq!(mgr.list().unwrap()[0].status, PluginStatus::Disabled);
    }

    #[test]
    fn failing_plugin_auto_disables_at_threshold() {
        let f = fixture();
        write_plugin(
            &f,
            "test-notify",
            &notify_manifest("test-notify"),
            r#"fn on_event(e) { throw "boom"; }"#,
        );
        grant_notify(&f, "test-notify");
        let mgr = PluginManager::new(f.plugins_dir.clone(), f.deps.clone()).unwrap();
        mgr.enable("test-notify").unwrap();

        for _ in 0..ERROR_THRESHOLD {
            mgr.handle_event(&imported("b1"));
        }

        assert_eq!(mgr.list().unwrap()[0].status, PluginStatus::AutoDisabled);
        // Further events are ignored without panicking.
        mgr.handle_event(&imported("b2"));

        // Auto-disable is persisted and audit-logged.
        let conn = f.deps.pool.get().unwrap();
        let state = permissions::plugin_state(&conn, "test-notify")
            .unwrap()
            .unwrap();
        assert!(state.auto_disabled);
        let log = db::get_all_activity(&conn).unwrap();
        assert!(log.iter().any(|e| e.action == "plugin_auto_disabled"));
    }

    #[test]
    fn reload_does_not_reset_the_auto_disable_counter() {
        // Regression: the error count lives in plugin_state (the DB), not in
        // per-load in-memory state, so a reload mid-sequence can't reset the
        // auto-disable timer.
        let f = fixture();
        write_plugin(
            &f,
            "test-notify",
            &notify_manifest("test-notify"),
            r#"fn on_event(e) { throw "boom"; }"#,
        );
        grant_notify(&f, "test-notify");
        let mgr = PluginManager::new(f.plugins_dir.clone(), f.deps.clone()).unwrap();
        mgr.enable("test-notify").unwrap();

        // One error short of the threshold, then reload.
        for _ in 0..(ERROR_THRESHOLD - 1) {
            mgr.handle_event(&imported("b1"));
        }
        assert_eq!(mgr.list().unwrap()[0].status, PluginStatus::Active);
        mgr.reload().unwrap();
        assert_eq!(mgr.list().unwrap()[0].status, PluginStatus::Active);

        // One more error must cross the threshold despite the reload.
        mgr.handle_event(&imported("b1"));
        assert_eq!(mgr.list().unwrap()[0].status, PluginStatus::AutoDisabled);
    }

    #[test]
    fn successful_dispatch_resets_the_error_counter() {
        let f = fixture();
        // Throws only for book "bad"; succeeds otherwise.
        write_plugin(
            &f,
            "test-notify",
            &notify_manifest("test-notify"),
            r#"fn on_event(e) { if e.book_id == "bad" { throw "boom"; } notify("ok", e.book_id); }"#,
        );
        grant_notify(&f, "test-notify");
        let mgr = PluginManager::new(f.plugins_dir.clone(), f.deps.clone()).unwrap();
        mgr.enable("test-notify").unwrap();

        for _ in 0..(ERROR_THRESHOLD - 1) {
            mgr.handle_event(&imported("bad"));
        }
        // A success resets the counter, so the next batch of errors must
        // start over rather than tipping straight into auto-disable.
        mgr.handle_event(&imported("good"));
        for _ in 0..(ERROR_THRESHOLD - 1) {
            mgr.handle_event(&imported("bad"));
        }
        assert_eq!(mgr.list().unwrap()[0].status, PluginStatus::Active);
    }

    #[test]
    fn previously_enabled_plugin_loads_on_startup() {
        let f = fixture();
        write_plugin(
            &f,
            "test-notify",
            &notify_manifest("test-notify"),
            r#"fn on_event(e) { notify("got", e.book_id); }"#,
        );
        grant_notify(&f, "test-notify");
        {
            let mgr = PluginManager::new(f.plugins_dir.clone(), f.deps.clone()).unwrap();
            mgr.enable("test-notify").unwrap();
        }

        // Fresh manager (new app session) — plugin must come back active.
        let mgr = PluginManager::new(f.plugins_dir.clone(), f.deps.clone()).unwrap();
        assert_eq!(mgr.list().unwrap()[0].status, PluginStatus::Active);
        mgr.handle_event(&imported("b9"));
        assert_eq!(f.services.notes.lock().unwrap().len(), 1);
    }

    #[test]
    fn run_now_rejects_plugin_not_subscribed_to_app_started() {
        let f = fixture();
        // Subscribes to BookImported, not AppStarted.
        write_plugin(
            &f,
            "test-notify",
            &notify_manifest("test-notify"),
            r#"fn on_event(e) { notify("got", e.book_id); }"#,
        );
        grant_notify(&f, "test-notify");
        let mgr = PluginManager::new(f.plugins_dir.clone(), f.deps.clone()).unwrap();
        mgr.enable("test-notify").unwrap();

        let err = mgr.run_now("test-notify").unwrap_err().to_string();
        assert!(err.contains("AppStarted"), "got: {err}");
        // The plugin's on_event was never invoked.
        assert!(f.services.notes.lock().unwrap().is_empty());
    }

    #[test]
    fn bus_attachment_delivers_events_to_plugins() {
        let f = fixture();
        write_plugin(
            &f,
            "test-notify",
            &notify_manifest("test-notify"),
            r#"fn on_event(e) { notify("bus", e.book_id); }"#,
        );
        grant_notify(&f, "test-notify");
        let mgr = PluginManager::new(f.plugins_dir.clone(), f.deps.clone()).unwrap();
        mgr.enable("test-notify").unwrap();

        let bus = EventBus::new();
        mgr.attach_to_bus(&bus);
        bus.emit(imported("b1"));

        // Bus dispatch is async on its own thread — poll briefly.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if !f.services.notes.lock().unwrap().is_empty() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "event never arrived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
