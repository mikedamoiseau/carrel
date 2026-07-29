//! `plugin.toml` parsing and validation (spec §4.2).
//!
//! A manifest that fails validation marks the plugin "invalid" in the UI and
//! can never be enabled — validation errors carry the reason verbatim.

use super::permissions::Permission;
use crate::error::{FolioError, FolioResult};
use crate::events::FolioEvent;

/// Parsed, validated plugin manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub min_app_version: Option<String>,
    /// Validated event names from `[events] subscribe`.
    pub subscribe: Vec<String>,
    /// Validated permissions from `[permissions] required`.
    pub permissions: Vec<Permission>,
    /// Host allowlist — present iff `network` permission is requested.
    pub network_hosts: Vec<String>,
}

/// Raw TOML shape — deserialized first, validated into [`PluginManifest`].
#[derive(serde::Deserialize)]
struct RawManifest {
    plugin: RawPlugin,
    events: RawEvents,
    permissions: RawPermissions,
}

#[derive(serde::Deserialize)]
struct RawPlugin {
    id: String,
    name: String,
    version: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    min_app_version: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawEvents {
    subscribe: Vec<String>,
}

#[derive(serde::Deserialize)]
struct RawPermissions {
    required: Vec<String>,
    #[serde(default)]
    network: Option<RawNetwork>,
}

#[derive(serde::Deserialize)]
struct RawNetwork {
    #[serde(default)]
    hosts: Vec<String>,
}

/// Parse and validate the raw contents of a `plugin.toml`.
pub fn parse_manifest(raw: &str) -> FolioResult<PluginManifest> {
    let raw: RawManifest = toml::from_str(raw)
        .map_err(|e| FolioError::invalid(format!("plugin.toml parse error: {e}")))?;

    if !is_valid_plugin_id(&raw.plugin.id) {
        return Err(FolioError::invalid(format!(
            "invalid plugin id '{}': must be 3–64 chars of [a-z0-9-]",
            raw.plugin.id
        )));
    }

    if raw.events.subscribe.is_empty() {
        return Err(FolioError::invalid(
            "[events] subscribe must list at least one event",
        ));
    }
    for event in &raw.events.subscribe {
        if !FolioEvent::ALL_NAMES.contains(&event.as_str()) {
            return Err(FolioError::invalid(format!(
                "unknown event '{event}' in [events] subscribe"
            )));
        }
    }

    let mut permissions = Vec::with_capacity(raw.permissions.required.len());
    for p in &raw.permissions.required {
        let parsed = Permission::parse(p)
            .ok_or_else(|| FolioError::invalid(format!("unknown permission '{p}'")))?;
        if !permissions.contains(&parsed) {
            permissions.push(parsed);
        }
    }

    let network_hosts = raw.permissions.network.map(|n| n.hosts).unwrap_or_default();
    let wants_network = permissions.contains(&Permission::Network);
    if wants_network && network_hosts.is_empty() {
        return Err(FolioError::invalid(
            "the 'network' permission requires [permissions.network] hosts",
        ));
    }
    if !wants_network && !network_hosts.is_empty() {
        return Err(FolioError::invalid(
            "[permissions.network] hosts given without the 'network' permission",
        ));
    }

    Ok(PluginManifest {
        id: raw.plugin.id,
        name: raw.plugin.name,
        version: raw.plugin.version,
        description: raw.plugin.description,
        author: raw.plugin.author,
        min_app_version: raw.plugin.min_app_version,
        subscribe: raw.events.subscribe,
        permissions,
        network_hosts,
    })
}

/// Plugin ids are folder-safe slugs: `[a-z0-9-]`, 3–64 chars.
pub fn is_valid_plugin_id(id: &str) -> bool {
    (3..=64).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
[plugin]
id = "highlight-exporter"
name = "Highlight Exporter"
version = "1.0.0"
description = "Appends new highlights to a Markdown file"
author = "Carrel"
min_app_version = "2.3.0"

[events]
subscribe = ["HighlightCreated", "HighlightUpdated"]

[permissions]
required = ["read:highlights", "read:library", "write:files"]
"#;

    #[test]
    fn parses_a_valid_manifest() {
        let m = parse_manifest(VALID).unwrap();
        assert_eq!(m.id, "highlight-exporter");
        assert_eq!(m.name, "Highlight Exporter");
        assert_eq!(m.version, "1.0.0");
        assert_eq!(m.min_app_version.as_deref(), Some("2.3.0"));
        assert_eq!(m.subscribe, vec!["HighlightCreated", "HighlightUpdated"]);
        assert_eq!(
            m.permissions,
            vec![
                Permission::ReadHighlights,
                Permission::ReadLibrary,
                Permission::WriteFiles,
            ]
        );
        assert!(m.network_hosts.is_empty());
    }

    #[test]
    fn min_app_version_is_optional() {
        let raw = VALID.replace("min_app_version = \"2.3.0\"\n", "");
        let m = parse_manifest(&raw).unwrap();
        assert_eq!(m.min_app_version, None);
    }

    #[test]
    fn rejects_invalid_id() {
        for bad in ["AB", "Has Space", "UPPER", "x", &"a".repeat(65)] {
            let raw = VALID.replace("highlight-exporter", bad);
            let err = parse_manifest(&raw).unwrap_err().to_string();
            assert!(err.contains("id"), "expected id error, got: {err}");
        }
    }

    #[test]
    fn rejects_unknown_event() {
        let raw = VALID.replace("HighlightCreated", "BookExploded");
        let err = parse_manifest(&raw).unwrap_err().to_string();
        assert!(err.contains("BookExploded"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_permission() {
        let raw = VALID.replace("write:files", "root:everything");
        let err = parse_manifest(&raw).unwrap_err().to_string();
        assert!(err.contains("root:everything"), "got: {err}");
    }

    #[test]
    fn rejects_network_permission_without_hosts() {
        let raw = VALID.replace("\"write:files\"", "\"network\"");
        let err = parse_manifest(&raw).unwrap_err().to_string();
        assert!(err.contains("hosts"), "got: {err}");
    }

    #[test]
    fn rejects_hosts_without_network_permission() {
        let raw = format!("{VALID}\n[permissions.network]\nhosts = [\"example.org\"]\n");
        let err = parse_manifest(&raw).unwrap_err().to_string();
        assert!(err.contains("network"), "got: {err}");
    }

    #[test]
    fn accepts_network_permission_with_hosts() {
        let raw = format!(
            "{}\n[permissions.network]\nhosts = [\"standardebooks.org\"]\n",
            VALID.replace("\"write:files\"", "\"network\"")
        );
        let m = parse_manifest(&raw).unwrap();
        assert!(m.permissions.contains(&Permission::Network));
        assert_eq!(m.network_hosts, vec!["standardebooks.org"]);
    }

    #[test]
    fn rejects_empty_subscribe_list() {
        let raw = VALID.replace(
            "subscribe = [\"HighlightCreated\", \"HighlightUpdated\"]",
            "subscribe = []",
        );
        let err = parse_manifest(&raw).unwrap_err().to_string();
        assert!(err.contains("subscribe"), "got: {err}");
    }

    #[test]
    fn rejects_garbage_toml() {
        assert!(parse_manifest("not toml at {{{").is_err());
    }

    #[test]
    fn valid_plugin_id_rules() {
        assert!(is_valid_plugin_id("abc"));
        assert!(is_valid_plugin_id("auto-tagger-2"));
        assert!(!is_valid_plugin_id("ab"));
        assert!(!is_valid_plugin_id("Auto"));
        assert!(!is_valid_plugin_id("a_b_c"));
        assert!(!is_valid_plugin_id(&"a".repeat(65)));
    }
}
