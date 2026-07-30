//! Permission taxonomy and grant/state persistence for plugins.
//!
//! Deny-by-default: a plugin's script scope only receives host functions for
//! permissions the user has granted. Grants and per-plugin runtime state live
//! in two module-owned tables created by [`ensure_plugin_schema`].

use rusqlite::{params, Connection, OptionalExtension, Result};

/// Every permission a manifest may request. M2 implements host functions for
/// `ReadLibrary`, `WriteTags`, and `Notify`; the rest parse and persist so
/// later milestones don't need a schema or manifest change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    ReadLibrary,
    ReadHighlights,
    WriteTags,
    WriteMetadata,
    WriteFiles,
    Notify,
    Network,
    ImportBooks,
}

impl Permission {
    /// Wire format used in `plugin.toml` and the `plugin_grants` table.
    pub fn as_str(&self) -> &'static str {
        match self {
            Permission::ReadLibrary => "read:library",
            Permission::ReadHighlights => "read:highlights",
            Permission::WriteTags => "write:tags",
            Permission::WriteMetadata => "write:metadata",
            Permission::WriteFiles => "write:files",
            Permission::Notify => "notify",
            Permission::Network => "network",
            Permission::ImportBooks => "import:books",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Permission::ALL.into_iter().find(|p| p.as_str() == s)
    }

    pub const ALL: [Permission; 8] = [
        Permission::ReadLibrary,
        Permission::ReadHighlights,
        Permission::WriteTags,
        Permission::WriteMetadata,
        Permission::WriteFiles,
        Permission::Notify,
        Permission::Network,
        Permission::ImportBooks,
    ];
}

/// One recorded grant.
#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    pub permission: Permission,
    /// Permission-specific parameters as JSON (e.g. export dir, host list).
    pub params: Option<String>,
    pub granted_at: i64,
}

/// Per-plugin runtime state row.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginState {
    pub plugin_id: String,
    pub enabled: bool,
    pub consecutive_errors: u32,
    pub auto_disabled: bool,
}

/// Create the module-owned tables. Additive and idempotent; called when the
/// plugin manager initializes (intentionally not part of `db::run_schema`).
pub fn ensure_plugin_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS plugin_grants (
            plugin_id  TEXT NOT NULL,
            permission TEXT NOT NULL,
            params     TEXT,
            granted_at INTEGER NOT NULL,
            PRIMARY KEY (plugin_id, permission)
        );
        CREATE TABLE IF NOT EXISTS plugin_state (
            plugin_id          TEXT PRIMARY KEY,
            enabled            INTEGER NOT NULL DEFAULT 0,
            consecutive_errors INTEGER NOT NULL DEFAULT 0,
            auto_disabled      INTEGER NOT NULL DEFAULT 0
        );",
    )
}

/// Replace the grant set for a plugin (consent dialog approval).
pub fn record_grants(
    conn: &Connection,
    plugin_id: &str,
    grants: &[(Permission, Option<String>)],
    granted_at: i64,
) -> Result<()> {
    conn.execute(
        "DELETE FROM plugin_grants WHERE plugin_id = ?1",
        params![plugin_id],
    )?;
    let mut stmt = conn.prepare(
        "INSERT INTO plugin_grants (plugin_id, permission, params, granted_at)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (permission, p) in grants {
        stmt.execute(params![plugin_id, permission.as_str(), p, granted_at])?;
    }
    Ok(())
}

pub fn grants_for(conn: &Connection, plugin_id: &str) -> Result<Vec<Grant>> {
    let mut stmt = conn
        .prepare("SELECT permission, params, granted_at FROM plugin_grants WHERE plugin_id = ?1")?;
    let rows = stmt.query_map(params![plugin_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut grants = Vec::new();
    for row in rows {
        let (permission, params, granted_at) = row?;
        // Unknown permission strings (e.g. written by a newer app version)
        // are skipped rather than failing the whole read.
        if let Some(permission) = Permission::parse(&permission) {
            grants.push(Grant {
                permission,
                params,
                granted_at,
            });
        }
    }
    Ok(grants)
}

/// True when `requested` contains a permission that has no recorded grant —
/// i.e. the consent dialog must be shown (again).
pub fn needs_consent(conn: &Connection, plugin_id: &str, requested: &[Permission]) -> Result<bool> {
    let granted: Vec<Permission> = grants_for(conn, plugin_id)?
        .into_iter()
        .map(|g| g.permission)
        .collect();
    Ok(requested.iter().any(|p| !granted.contains(p)))
}

pub fn set_plugin_enabled(conn: &Connection, plugin_id: &str, enabled: bool) -> Result<()> {
    // Re-enabling a plugin clears the auto_disabled flag and error counter so
    // it gets a fresh run of attempts.
    conn.execute(
        "INSERT INTO plugin_state (plugin_id, enabled, consecutive_errors, auto_disabled)
         VALUES (?1, ?2, 0, 0)
         ON CONFLICT(plugin_id) DO UPDATE SET
            enabled = ?2, consecutive_errors = 0, auto_disabled = 0",
        params![plugin_id, enabled as i64],
    )?;
    Ok(())
}

pub fn plugin_state(conn: &Connection, plugin_id: &str) -> Result<Option<PluginState>> {
    conn.query_row(
        "SELECT plugin_id, enabled, consecutive_errors, auto_disabled
         FROM plugin_state WHERE plugin_id = ?1",
        params![plugin_id],
        |row| {
            Ok(PluginState {
                plugin_id: row.get(0)?,
                enabled: row.get::<_, i64>(1)? != 0,
                consecutive_errors: row.get::<_, i64>(2)? as u32,
                auto_disabled: row.get::<_, i64>(3)? != 0,
            })
        },
    )
    .optional()
}

/// Increment the consecutive-error counter and return the new value.
pub fn record_plugin_error(conn: &Connection, plugin_id: &str) -> Result<u32> {
    conn.execute(
        "INSERT INTO plugin_state (plugin_id, enabled, consecutive_errors, auto_disabled)
         VALUES (?1, 0, 1, 0)
         ON CONFLICT(plugin_id) DO UPDATE SET
            consecutive_errors = consecutive_errors + 1",
        params![plugin_id],
    )?;
    conn.query_row(
        "SELECT consecutive_errors FROM plugin_state WHERE plugin_id = ?1",
        params![plugin_id],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n as u32)
}

/// Reset the consecutive-error counter (successful dispatch).
pub fn reset_plugin_errors(conn: &Connection, plugin_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE plugin_state SET consecutive_errors = 0 WHERE plugin_id = ?1",
        params![plugin_id],
    )?;
    Ok(())
}

/// Mark a plugin auto-disabled after repeated errors (also disables it).
pub fn set_auto_disabled(conn: &Connection, plugin_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE plugin_state SET enabled = 0, auto_disabled = 1 WHERE plugin_id = ?1",
        params![plugin_id],
    )?;
    Ok(())
}

/// Wipe all grants and state for a plugin ("Remove plugin data").
pub fn remove_plugin_data(conn: &Connection, plugin_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM plugin_grants WHERE plugin_id = ?1",
        params![plugin_id],
    )?;
    conn.execute(
        "DELETE FROM plugin_state WHERE plugin_id = ?1",
        params![plugin_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        ensure_plugin_schema(&c).unwrap();
        c
    }

    #[test]
    fn permission_wire_format_roundtrips_for_all() {
        for p in Permission::ALL {
            assert_eq!(Permission::parse(p.as_str()), Some(p));
        }
        assert_eq!(
            Permission::parse("read:library"),
            Some(Permission::ReadLibrary)
        );
        assert_eq!(Permission::parse("write:tags"), Some(Permission::WriteTags));
        assert_eq!(Permission::parse("notify"), Some(Permission::Notify));
        assert_eq!(Permission::parse("bogus"), None);
    }

    #[test]
    fn ensure_schema_is_idempotent() {
        let c = conn();
        ensure_plugin_schema(&c).unwrap();
        ensure_plugin_schema(&c).unwrap();
    }

    #[test]
    fn record_and_read_grants() {
        let c = conn();
        record_grants(
            &c,
            "auto-tagger",
            &[
                (Permission::ReadLibrary, None),
                (Permission::WriteTags, None),
            ],
            1000,
        )
        .unwrap();

        let grants = grants_for(&c, "auto-tagger").unwrap();
        assert_eq!(grants.len(), 2);
        assert!(grants
            .iter()
            .any(|g| g.permission == Permission::ReadLibrary));
        assert!(grants.iter().any(|g| g.permission == Permission::WriteTags));
        assert!(grants.iter().all(|g| g.granted_at == 1000));
    }

    #[test]
    fn record_grants_replaces_previous_set() {
        let c = conn();
        record_grants(&c, "p", &[(Permission::Notify, None)], 1).unwrap();
        record_grants(&c, "p", &[(Permission::ReadLibrary, None)], 2).unwrap();

        let grants = grants_for(&c, "p").unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].permission, Permission::ReadLibrary);
    }

    #[test]
    fn grant_params_persist() {
        let c = conn();
        record_grants(
            &c,
            "exporter",
            &[(Permission::WriteFiles, Some("{\"dir\":\"/tmp/x\"}".into()))],
            5,
        )
        .unwrap();
        let grants = grants_for(&c, "exporter").unwrap();
        assert_eq!(grants[0].params.as_deref(), Some("{\"dir\":\"/tmp/x\"}"));
    }

    #[test]
    fn needs_consent_true_when_ungranted_permission_requested() {
        let c = conn();
        assert!(needs_consent(&c, "p", &[Permission::Notify]).unwrap());

        record_grants(&c, "p", &[(Permission::Notify, None)], 1).unwrap();
        assert!(!needs_consent(&c, "p", &[Permission::Notify]).unwrap());

        // Manifest update adds a permission → re-consent for the delta.
        assert!(needs_consent(&c, "p", &[Permission::Notify, Permission::WriteTags]).unwrap());
    }

    #[test]
    fn enabled_state_round_trips_and_defaults_off() {
        let c = conn();
        assert!(plugin_state(&c, "p").unwrap().is_none());

        set_plugin_enabled(&c, "p", true).unwrap();
        let s = plugin_state(&c, "p").unwrap().unwrap();
        assert!(s.enabled);
        assert_eq!(s.consecutive_errors, 0);
        assert!(!s.auto_disabled);

        set_plugin_enabled(&c, "p", false).unwrap();
        assert!(!plugin_state(&c, "p").unwrap().unwrap().enabled);
    }

    #[test]
    fn error_counter_increments_and_resets() {
        let c = conn();
        set_plugin_enabled(&c, "p", true).unwrap();
        assert_eq!(record_plugin_error(&c, "p").unwrap(), 1);
        assert_eq!(record_plugin_error(&c, "p").unwrap(), 2);
        reset_plugin_errors(&c, "p").unwrap();
        assert_eq!(record_plugin_error(&c, "p").unwrap(), 1);
    }

    #[test]
    fn auto_disable_flips_enabled_off_and_sets_flag() {
        let c = conn();
        set_plugin_enabled(&c, "p", true).unwrap();
        set_auto_disabled(&c, "p").unwrap();
        let s = plugin_state(&c, "p").unwrap().unwrap();
        assert!(!s.enabled);
        assert!(s.auto_disabled);
        // Re-enabling clears the auto_disabled flag.
        set_plugin_enabled(&c, "p", true).unwrap();
        assert!(!plugin_state(&c, "p").unwrap().unwrap().auto_disabled);
    }

    #[test]
    fn remove_plugin_data_wipes_grants_and_state() {
        let c = conn();
        record_grants(&c, "p", &[(Permission::Notify, None)], 1).unwrap();
        set_plugin_enabled(&c, "p", true).unwrap();

        remove_plugin_data(&c, "p").unwrap();
        assert!(grants_for(&c, "p").unwrap().is_empty());
        assert!(plugin_state(&c, "p").unwrap().is_none());
        assert!(needs_consent(&c, "p", &[Permission::Notify]).unwrap());
    }
}
