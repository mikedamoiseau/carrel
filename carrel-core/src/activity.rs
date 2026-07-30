//! Typed activity-log events. `ActivityEvent::into_fields` is the single
//! source of truth for the `action`/`entity_type` strings consumed by the
//! frontend (`src/components/ActivityLog.tsx`). Do not change those strings
//! without updating the frontend in lockstep.

/// Resolved columns for one `activity_log` row.
pub struct ActivityFields {
    pub action: &'static str,
    pub entity_type: &'static str,
    pub entity_id: Option<String>,
    pub entity_name: Option<String>,
    pub detail: Option<String>,
}

/// A typed activity event. One variant per real call site action.
pub enum ActivityEvent {
    BookImported {
        id: String,
        title: String,
        format: String,
        author: String,
    },
    BookDeleted {
        id: String,
        title: Option<String>,
    },
    BookUpdated {
        id: String,
        title: String,
        detail: String,
    },
    BookEnriched {
        id: String,
    },
    BookScanned {
        id: String,
        title: String,
        detail: String,
    },
    BookCompleted {
        id: String,
        title: String,
    },
    BookRemovedCleanup {
        id: String,
        title: String,
    },
    BulkEdit {
        count: usize,
    },
    BulkDelete {
        count: usize,
    },
    SyncPullSuccess {
        book_id: String,
        title: String,
        detail: String,
    },
    SyncPullFailed {
        book_id: String,
        title: String,
        detail: String,
    },
    SyncPushSuccess {
        book_id: String,
        title: String,
        detail: String,
    },
    SyncPushFailed {
        book_id: String,
        title: String,
        detail: String,
    },
    CollectionCreated {
        id: String,
        name: String,
    },
    CollectionUpdated {
        id: String,
        name: String,
    },
    CollectionDeleted {
        id: String,
    },
    CollectionModified {
        id: String,
        detail: String,
    },
    LibraryExported {
        detail: String,
    },
    LibraryImported {
        detail: String,
    },
    BackupCompleted {
        detail: String,
    },
    BackupFailed {
        detail: String,
    },
    ProfileSwitched {
        name: String,
    },
    WebServerModesChanged {
        detail: String,
    },
    PluginEnabled {
        id: String,
        name: String,
        detail: String,
    },
    PluginDisabled {
        id: String,
        name: String,
    },
    PluginAutoDisabled {
        id: String,
        detail: String,
    },
}

impl ActivityEvent {
    /// Map this event to the legacy activity_log columns. Consumes self so
    /// owned Strings move into the result without cloning.
    pub fn into_fields(self) -> ActivityFields {
        use ActivityEvent::*;
        match self {
            BookImported {
                id,
                title,
                format,
                author,
            } => ActivityFields {
                action: "book_imported",
                entity_type: "book",
                entity_id: Some(id),
                entity_name: Some(title),
                detail: Some(format!("{format} by {author}")),
            },
            BookDeleted { id, title } => ActivityFields {
                action: "book_deleted",
                entity_type: "book",
                entity_id: Some(id),
                entity_name: title,
                detail: None,
            },
            BookUpdated { id, title, detail } => ActivityFields {
                action: "book_updated",
                entity_type: "book",
                entity_id: Some(id),
                entity_name: Some(title),
                detail: Some(detail),
            },
            BookEnriched { id } => ActivityFields {
                action: "book_enriched",
                entity_type: "book",
                entity_id: Some(id),
                entity_name: None,
                detail: Some("Enriched from OpenLibrary".to_string()),
            },
            BookScanned { id, title, detail } => ActivityFields {
                action: "book_scanned",
                entity_type: "book",
                entity_id: Some(id),
                entity_name: Some(title),
                detail: Some(detail),
            },
            BookCompleted { id, title } => ActivityFields {
                action: "book_completed",
                entity_type: "book",
                entity_id: Some(id),
                entity_name: Some(title),
                detail: None,
            },
            BookRemovedCleanup { id, title } => ActivityFields {
                action: "book_removed_cleanup",
                entity_type: "book",
                entity_id: Some(id),
                entity_name: Some(title),
                detail: None,
            },
            BulkEdit { count } => ActivityFields {
                action: "bulk_edit",
                entity_type: "book",
                entity_id: None,
                entity_name: None,
                detail: Some(format!("{count} books updated")),
            },
            BulkDelete { count } => ActivityFields {
                action: "bulk_delete",
                entity_type: "book",
                entity_id: None,
                entity_name: None,
                detail: Some(format!("{count} books deleted")),
            },
            SyncPullSuccess {
                book_id,
                title,
                detail,
            } => ActivityFields {
                action: "sync_pull_success",
                entity_type: "book",
                entity_id: Some(book_id),
                entity_name: Some(title),
                detail: Some(detail),
            },
            SyncPullFailed {
                book_id,
                title,
                detail,
            } => ActivityFields {
                action: "sync_pull_failed",
                entity_type: "book",
                entity_id: Some(book_id),
                entity_name: Some(title),
                detail: Some(detail),
            },
            SyncPushSuccess {
                book_id,
                title,
                detail,
            } => ActivityFields {
                action: "sync_push_success",
                entity_type: "book",
                entity_id: Some(book_id),
                entity_name: Some(title),
                detail: Some(detail),
            },
            SyncPushFailed {
                book_id,
                title,
                detail,
            } => ActivityFields {
                action: "sync_push_failed",
                entity_type: "book",
                entity_id: Some(book_id),
                entity_name: Some(title),
                detail: Some(detail),
            },
            CollectionCreated { id, name } => ActivityFields {
                action: "collection_created",
                entity_type: "collection",
                entity_id: Some(id),
                entity_name: Some(name),
                detail: None,
            },
            CollectionUpdated { id, name } => ActivityFields {
                action: "collection_updated",
                entity_type: "collection",
                entity_id: Some(id),
                entity_name: Some(name),
                detail: None,
            },
            CollectionDeleted { id } => ActivityFields {
                action: "collection_deleted",
                entity_type: "collection",
                entity_id: Some(id),
                entity_name: None,
                detail: None,
            },
            CollectionModified { id, detail } => ActivityFields {
                action: "collection_modified",
                entity_type: "collection",
                entity_id: Some(id),
                entity_name: None,
                detail: Some(detail),
            },
            LibraryExported { detail } => ActivityFields {
                action: "library_exported",
                entity_type: "library",
                entity_id: None,
                entity_name: None,
                detail: Some(detail),
            },
            LibraryImported { detail } => ActivityFields {
                action: "library_imported",
                entity_type: "library",
                entity_id: None,
                entity_name: None,
                detail: Some(detail),
            },
            BackupCompleted { detail } => ActivityFields {
                action: "backup_completed",
                entity_type: "library",
                entity_id: None,
                entity_name: None,
                detail: Some(detail),
            },
            BackupFailed { detail } => ActivityFields {
                action: "backup_failed",
                entity_type: "library",
                entity_id: None,
                entity_name: None,
                detail: Some(detail),
            },
            ProfileSwitched { name } => ActivityFields {
                action: "profile_switched",
                entity_type: "profile",
                entity_id: None,
                entity_name: Some(name),
                detail: None,
            },
            WebServerModesChanged { detail } => ActivityFields {
                action: "web_server_modes_changed",
                entity_type: "system",
                entity_id: None,
                entity_name: None,
                detail: Some(detail),
            },
            PluginEnabled { id, name, detail } => ActivityFields {
                action: "plugin_enabled",
                entity_type: "plugin",
                entity_id: Some(id),
                entity_name: Some(name),
                detail: Some(detail),
            },
            PluginDisabled { id, name } => ActivityFields {
                action: "plugin_disabled",
                entity_type: "plugin",
                entity_id: Some(id),
                entity_name: Some(name),
                detail: None,
            },
            PluginAutoDisabled { id, detail } => ActivityFields {
                action: "plugin_auto_disabled",
                entity_type: "plugin",
                entity_id: Some(id),
                entity_name: None,
                detail: Some(detail),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(e: ActivityEvent) -> ActivityFields {
        e.into_fields()
    }

    #[test]
    fn book_imported_maps_to_legacy_contract() {
        let r = f(ActivityEvent::BookImported {
            id: "b1".into(),
            title: "T".into(),
            format: "EPUB".into(),
            author: "A".into(),
        });
        assert_eq!(r.action, "book_imported");
        assert_eq!(r.entity_type, "book");
        assert_eq!(r.entity_id.as_deref(), Some("b1"));
        assert_eq!(r.entity_name.as_deref(), Some("T"));
        assert_eq!(r.detail.as_deref(), Some("EPUB by A"));
    }

    #[test]
    fn action_and_entity_strings_match_legacy_values() {
        let cases: Vec<(ActivityEvent, &str, &str)> = vec![
            (
                ActivityEvent::BookImported {
                    id: "i".into(),
                    title: "t".into(),
                    format: "f".into(),
                    author: "a".into(),
                },
                "book_imported",
                "book",
            ),
            (
                ActivityEvent::BookDeleted {
                    id: "i".into(),
                    title: Some("t".into()),
                },
                "book_deleted",
                "book",
            ),
            (
                ActivityEvent::BookUpdated {
                    id: "i".into(),
                    title: "t".into(),
                    detail: "d".into(),
                },
                "book_updated",
                "book",
            ),
            (
                ActivityEvent::BookCompleted {
                    id: "i".into(),
                    title: "t".into(),
                },
                "book_completed",
                "book",
            ),
            (
                ActivityEvent::BookEnriched { id: "i".into() },
                "book_enriched",
                "book",
            ),
            (
                ActivityEvent::BookScanned {
                    id: "i".into(),
                    title: "t".into(),
                    detail: "d".into(),
                },
                "book_scanned",
                "book",
            ),
            (
                ActivityEvent::BookRemovedCleanup {
                    id: "i".into(),
                    title: "t".into(),
                },
                "book_removed_cleanup",
                "book",
            ),
            (ActivityEvent::BulkEdit { count: 3 }, "bulk_edit", "book"),
            (
                ActivityEvent::BulkDelete { count: 3 },
                "bulk_delete",
                "book",
            ),
            (
                ActivityEvent::SyncPullSuccess {
                    book_id: "i".into(),
                    title: "t".into(),
                    detail: "d".into(),
                },
                "sync_pull_success",
                "book",
            ),
            (
                ActivityEvent::SyncPullFailed {
                    book_id: "i".into(),
                    title: "t".into(),
                    detail: "d".into(),
                },
                "sync_pull_failed",
                "book",
            ),
            (
                ActivityEvent::SyncPushSuccess {
                    book_id: "i".into(),
                    title: "t".into(),
                    detail: "d".into(),
                },
                "sync_push_success",
                "book",
            ),
            (
                ActivityEvent::SyncPushFailed {
                    book_id: "i".into(),
                    title: "t".into(),
                    detail: "d".into(),
                },
                "sync_push_failed",
                "book",
            ),
            (
                ActivityEvent::CollectionCreated {
                    id: "i".into(),
                    name: "n".into(),
                },
                "collection_created",
                "collection",
            ),
            (
                ActivityEvent::CollectionUpdated {
                    id: "i".into(),
                    name: "n".into(),
                },
                "collection_updated",
                "collection",
            ),
            (
                ActivityEvent::CollectionDeleted { id: "i".into() },
                "collection_deleted",
                "collection",
            ),
            (
                ActivityEvent::CollectionModified {
                    id: "i".into(),
                    detail: "d".into(),
                },
                "collection_modified",
                "collection",
            ),
            (
                ActivityEvent::LibraryExported { detail: "d".into() },
                "library_exported",
                "library",
            ),
            (
                ActivityEvent::LibraryImported { detail: "d".into() },
                "library_imported",
                "library",
            ),
            (
                ActivityEvent::BackupCompleted { detail: "d".into() },
                "backup_completed",
                "library",
            ),
            (
                ActivityEvent::BackupFailed { detail: "d".into() },
                "backup_failed",
                "library",
            ),
            (
                ActivityEvent::ProfileSwitched { name: "n".into() },
                "profile_switched",
                "profile",
            ),
            (
                ActivityEvent::WebServerModesChanged { detail: "d".into() },
                "web_server_modes_changed",
                "system",
            ),
            (
                ActivityEvent::PluginEnabled {
                    id: "i".into(),
                    name: "n".into(),
                    detail: "d".into(),
                },
                "plugin_enabled",
                "plugin",
            ),
            (
                ActivityEvent::PluginDisabled {
                    id: "i".into(),
                    name: "n".into(),
                },
                "plugin_disabled",
                "plugin",
            ),
            (
                ActivityEvent::PluginAutoDisabled {
                    id: "i".into(),
                    detail: "d".into(),
                },
                "plugin_auto_disabled",
                "plugin",
            ),
        ];
        for (event, action, entity) in cases {
            let r = event.into_fields();
            assert_eq!(r.action, action, "action mismatch for {action}");
            assert_eq!(r.entity_type, entity, "entity mismatch for {action}");
        }
    }

    #[test]
    fn enriched_detail_is_fixed_string() {
        let r = ActivityEvent::BookEnriched { id: "b".into() }.into_fields();
        assert_eq!(r.entity_name, None);
        assert_eq!(r.detail.as_deref(), Some("Enriched from OpenLibrary"));
    }

    #[test]
    fn collection_deleted_has_no_name_or_detail() {
        let r = ActivityEvent::CollectionDeleted { id: "c".into() }.into_fields();
        assert_eq!(r.entity_id.as_deref(), Some("c"));
        assert_eq!(r.entity_name, None);
        assert_eq!(r.detail, None);
    }

    #[test]
    fn bulk_delete_detail_formats_count() {
        let r = ActivityEvent::BulkDelete { count: 5 }.into_fields();
        assert_eq!(r.entity_id, None);
        assert_eq!(r.detail.as_deref(), Some("5 books deleted"));
    }
}
