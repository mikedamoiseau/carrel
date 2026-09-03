//! Rhai script runtime for plugins (spec §4.4).
//!
//! One engine per plugin, built at load time with ONLY the host functions
//! the user granted — an ungranted capability is not an authorization error,
//! it simply does not exist in the script's namespace. Execution is bounded
//! by an operation budget and a wall-clock watchdog; a misbehaving script
//! aborts its invocation, never the app.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rhai::{Array, Dynamic, Engine, EvalAltResult, Map as RhaiMap, Scope, AST};

use super::permissions::Permission;
use crate::db::{self, DbPool};
use crate::error::{CarrelError, CarrelResult};
use crate::events::CarrelEvent;
use crate::models::{Book, Highlight};

/// Cap on what `find_books` hands a plugin, documented in `docs/PLUGINS.md`
/// as "array (max 50)". Applied as the query's `LIMIT` so the cap is enforced
/// in SQL rather than by truncating a whole-table read in the host.
const FIND_BOOKS_LIMIT: usize = 50;

/// Capabilities that only the embedding app can provide (OS notifications,
/// the book-import pipeline). `carrel-core` stays UI-free; the desktop shell
/// injects an implementation.
pub trait HostServices: Send + Sync {
    fn notify(&self, title: &str, body: &str);

    /// Download and import a book from `url` into the active library,
    /// reusing the app's normal import path (dedup, copy-on-import). Returns
    /// the imported book id, or an error string. Backs the `import:books`
    /// host function; default errors so non-desktop hosts opt out.
    fn import_from_url(&self, _url: &str) -> Result<String, String> {
        Err("import is not available in this context".to_string())
    }
}

/// Everything host functions may need, injected at load time.
#[derive(Clone)]
pub struct RuntimeDeps {
    pub pool: DbPool,
    pub services: Arc<dyn HostServices>,
}

/// A loaded, compiled plugin script with its capability-scoped engine.
pub struct PluginRuntime {
    engine: Engine,
    ast: AST,
    /// Reset before every dispatch; read by the engine's progress hook.
    invocation_start: Arc<Mutex<Instant>>,
}

impl PluginRuntime {
    /// Operation budget per invocation (spec §4.4).
    pub const MAX_OPERATIONS: u64 = 1_000_000;
    /// Wall-clock budget per invocation (spec §4.4).
    pub const MAX_WALL_CLOCK: Duration = Duration::from_secs(5);
    /// Cap on strings a script can build (spec §4.4).
    pub const MAX_STRING_SIZE: usize = 1024 * 1024;
    /// Call-depth cap (spec §4.4).
    pub const MAX_CALL_LEVELS: usize = 64;

    /// Per-write cap for `write:files` host functions (spec §7, text-only).
    /// Matches `MAX_STRING_SIZE`: the string-size cap already bounds any text
    /// a script can build, so this is the same ceiling enforced at the write
    /// boundary as defense-in-depth for non-script callers.
    pub const MAX_FILE_WRITE_BYTES: usize = Self::MAX_STRING_SIZE;

    /// Compile `script` with host functions for exactly `granted`. Each entry
    /// is a permission plus its optional grant parameter (e.g. the
    /// user-picked export directory for `write:files`). `network_hosts` is the
    /// manifest's declared host allowlist for the `network` permission.
    pub fn load(
        script: &str,
        granted: &[(Permission, Option<String>)],
        network_hosts: &[String],
        deps: RuntimeDeps,
    ) -> CarrelResult<Self> {
        let mut engine = Engine::new();
        engine.set_max_operations(Self::MAX_OPERATIONS);
        engine.set_max_call_levels(Self::MAX_CALL_LEVELS);
        engine.set_max_string_size(Self::MAX_STRING_SIZE);
        engine.disable_symbol("eval");

        let invocation_start = Arc::new(Mutex::new(Instant::now()));
        let watchdog_start = Arc::clone(&invocation_start);
        engine.on_progress(move |_ops| {
            let start = *watchdog_start
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if start.elapsed() > Self::MAX_WALL_CLOCK {
                Some("wall-clock budget exceeded".into())
            } else {
                None
            }
        });

        for (permission, params) in granted {
            register_host_fns(
                &mut engine,
                *permission,
                params.as_deref(),
                network_hosts,
                &deps,
            );
        }

        let ast = engine
            .compile(script)
            .map_err(|e| CarrelError::invalid(format!("script compile error: {e}")))?;

        Ok(Self {
            engine,
            ast,
            invocation_start,
        })
    }

    /// Run the script's `on_event(event)` for one event. Errors when the
    /// script has no `on_event`, throws, or exceeds its budgets.
    pub fn dispatch(&self, event: &CarrelEvent) -> CarrelResult<()> {
        *self
            .invocation_start
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();

        let mut scope = Scope::new();
        // Accept any return type — a script may end on an expression; we
        // simply ignore whatever `on_event` evaluates to.
        self.engine
            .call_fn::<Dynamic>(&mut scope, &self.ast, "on_event", (event_to_map(event),))
            .map(|_| ())
            .map_err(|e| CarrelError::internal(format!("plugin dispatch error: {e}")))
    }
}

/// Register the host functions one permission unlocks. Capability scoping
/// happens HERE: an ungranted permission's functions are never registered,
/// so calling one is a function-not-found error inside the script.
fn register_host_fns(
    engine: &mut Engine,
    permission: Permission,
    params: Option<&str>,
    network_hosts: &[String],
    deps: &RuntimeDeps,
) {
    match permission {
        Permission::ReadLibrary => {
            let pool = deps.pool.clone();
            engine.register_fn(
                "get_book",
                move |id: &str| -> Result<Dynamic, Box<EvalAltResult>> {
                    let conn = pool.get().map_err(host_err)?;
                    Ok(match db::get_book(&conn, id).map_err(host_err)? {
                        Some(book) => Dynamic::from_map(book_to_map(&book)),
                        None => Dynamic::UNIT,
                    })
                },
            );
            let pool = deps.pool.clone();
            engine.register_fn(
                "find_books",
                move |query: &str| -> Result<Array, Box<EvalAltResult>> {
                    let conn = pool.get().map_err(host_err)?;
                    // The title-or-author match and the 50-item cap are the
                    // database's job, not this closure's — see the query
                    // module in `db.rs`. Doing it here meant loading the whole
                    // `books` table into the plugin host on every call, and
                    // keeping a fifth copy of a predicate that exists once.
                    let page = db::query_books(
                        &conn,
                        &db::BookQuery {
                            q: Some(query.to_string()),
                            limit: Some(FIND_BOOKS_LIMIT),
                            ..Default::default()
                        },
                    )
                    .map_err(host_err)?;
                    Ok(page
                        .items
                        .iter()
                        .map(|b| Dynamic::from_map(book_to_map(b)))
                        .collect())
                },
            );
        }
        Permission::WriteTags => {
            let pool = deps.pool.clone();
            engine.register_fn(
                "add_tag",
                move |book_id: &str, name: &str| -> Result<(), Box<EvalAltResult>> {
                    let conn = pool.get().map_err(host_err)?;
                    let tag_id = match db::get_tag_by_name(&conn, name).map_err(host_err)? {
                        Some(id) => id,
                        None => {
                            let id = uuid::Uuid::new_v4().to_string();
                            db::get_or_create_tag(&conn, &id, name).map_err(host_err)?;
                            id
                        }
                    };
                    db::add_tag_to_book(&conn, book_id, &tag_id).map_err(host_err)
                },
            );
            let pool = deps.pool.clone();
            engine.register_fn(
                "remove_tag",
                move |book_id: &str, name: &str| -> Result<(), Box<EvalAltResult>> {
                    let conn = pool.get().map_err(host_err)?;
                    if let Some(tag_id) = db::get_tag_by_name(&conn, name).map_err(host_err)? {
                        db::remove_tag_from_book(&conn, book_id, &tag_id).map_err(host_err)?;
                    }
                    Ok(())
                },
            );
        }
        Permission::Notify => {
            let services = Arc::clone(&deps.services);
            engine.register_fn("notify", move |title: &str, body: &str| {
                services.notify(title, body);
            });
        }
        Permission::ReadHighlights => {
            let pool = deps.pool.clone();
            engine.register_fn(
                "get_highlights",
                move |book_id: &str| -> Result<Array, Box<EvalAltResult>> {
                    let conn = pool.get().map_err(host_err)?;
                    Ok(db::list_highlights(&conn, book_id)
                        .map_err(host_err)?
                        .iter()
                        .map(|h| Dynamic::from_map(highlight_to_map(h)))
                        .collect())
                },
            );
        }
        Permission::WriteFiles => {
            // The grant parameter is the user-picked export directory. With
            // no directory granted, no write functions are registered — the
            // capability is inert rather than rooted at an unsafe default.
            if let Some(root) = params.map(|p| p.to_string()) {
                let write_root = root.clone();
                engine.register_fn(
                    "write_file",
                    move |rel: &str, text: &str| -> Result<(), Box<EvalAltResult>> {
                        write_into_root(&write_root, rel, text, false)
                    },
                );
                engine.register_fn(
                    "append_file",
                    move |rel: &str, text: &str| -> Result<(), Box<EvalAltResult>> {
                        write_into_root(&root, rel, text, true)
                    },
                );
            }
        }
        Permission::Network => {
            // Allowlist comes from the manifest (source of truth), never from
            // script input. With no declared hosts, no function is registered.
            let allow: Vec<String> = network_hosts.to_vec();
            if !allow.is_empty() {
                engine.register_fn(
                    "http_get",
                    move |url: &str| -> Result<String, Box<EvalAltResult>> {
                        http_get(url, &allow)
                    },
                );
            }
        }
        Permission::ImportBooks => {
            let services = Arc::clone(&deps.services);
            engine.register_fn(
                "import_from_url",
                move |url: &str| -> Result<String, Box<EvalAltResult>> {
                    services.import_from_url(url).map_err(host_err)
                },
            );
        }
        // No host functions yet; granting registers nothing.
        Permission::WriteMetadata => {}
    }
}

/// GET `url` as text. Two gates: the URL's host must be in the plugin's
/// manifest allowlist, AND it must pass the SSRF guard (public HTTP/HTTPS
/// only — no LAN relaxation for plugins). Routed through `send_with_retry`.
fn http_get(url: &str, allow: &[String]) -> Result<String, Box<EvalAltResult>> {
    let host_port = crate::opds::host_port_from_url(url).ok_or_else(|| host_err("invalid URL"))?;
    // Match either "host" or "host:port" against the allowlist.
    let host_only = host_port.split(':').next().unwrap_or(&host_port);
    let allowed = allow
        .iter()
        .any(|h| h.eq_ignore_ascii_case(&host_port) || h.eq_ignore_ascii_case(host_only));
    if !allowed {
        return Err(host_err(format!(
            "host '{host_only}' is not in the plugin's declared network hosts"
        )));
    }
    // SSRF guard with no trusted entries: blocks private/loopback targets.
    if !crate::opds::is_safe_url_with_trusted(url, &[]) {
        return Err(host_err(
            "URL blocked: only public HTTP/HTTPS URLs are allowed",
        ));
    }

    // Redirects must stay within the allowlist AND keep passing the SSRF
    // guard — otherwise an allowlisted host could 302 to a private/loopback
    // or off-allowlist target.
    let allow_redirect = allow.to_vec();
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.error("too many redirects");
        }
        let u = attempt.url();
        let host = u.host_str().unwrap_or("");
        let hp = match u.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.to_string(),
        };
        let on_allowlist = allow_redirect
            .iter()
            .any(|h| h.eq_ignore_ascii_case(host) || h.eq_ignore_ascii_case(&hp));
        if on_allowlist && crate::opds::is_safe_url_with_trusted(u.as_str(), &[]) {
            attempt.follow()
        } else {
            attempt.error("redirect outside the plugin's declared hosts")
        }
    });
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("Carrel-Plugin/1.0")
        .redirect(policy)
        .build()
        .map_err(host_err)?;
    let resp = crate::http_retry::send_with_retry(
        client.get(url),
        "plugin",
        &crate::http_retry::RetryPolicy::default(),
    )
    .map_err(host_err)?;
    if !resp.status().is_success() {
        return Err(host_err(format!("HTTP {}", resp.status())));
    }
    resp.text().map_err(host_err)
}

/// Write or append `text` to `rel` resolved under `root`, rejecting any path
/// that escapes `root` (absolute paths, `..`, and symlinked parent dirs or
/// leaf files). Text-only, size-capped per write.
fn write_into_root(
    root: &str,
    rel: &str,
    text: &str,
    append: bool,
) -> Result<(), Box<EvalAltResult>> {
    use std::path::{Component, Path};

    if text.len() > PluginRuntime::MAX_FILE_WRITE_BYTES {
        return Err(format!(
            "write exceeds {}-byte limit",
            PluginRuntime::MAX_FILE_WRITE_BYTES
        )
        .into());
    }

    let rel_path = Path::new(rel);
    // Reject empty, absolute, and any component that could escape the root.
    if rel_path.as_os_str().is_empty() {
        return Err("path must name a file".into());
    }
    if rel_path.is_absolute() {
        return Err("path must be relative to the export folder".into());
    }
    for comp in rel_path.components() {
        match comp {
            Component::Normal(_) => {}
            _ => return Err("path may not contain '..' or absolute segments".into()),
        }
    }

    let root_path = Path::new(root);
    let canonical_root = root_path
        .canonicalize()
        .map_err(|e| host_err(format!("export folder unavailable: {e}")))?;
    let target = canonical_root.join(rel_path);

    // Create parent dirs inside the root, then verify the resolved parent is
    // still within the canonical root (defends against a symlinked subdir).
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(host_err)?;
        let canonical_parent = parent.canonicalize().map_err(host_err)?;
        if !canonical_parent.starts_with(&canonical_root) {
            return Err("resolved path escapes the export folder".into());
        }
    }

    // If the leaf already exists, it may be a symlink pointing outside the
    // root — canonicalize and re-check before opening so a write can't be
    // redirected past the parent-dir guard. (A residual check-then-open
    // TOCTOU window remains; closing it fully needs O_NOFOLLOW.)
    if target.exists() {
        let canonical_target = target.canonicalize().map_err(host_err)?;
        if !canonical_target.starts_with(&canonical_root) {
            return Err("resolved path escapes the export folder".into());
        }
    }

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(&target)
        .map_err(host_err)?;
    file.write_all(text.as_bytes()).map_err(host_err)?;
    Ok(())
}

fn host_err(e: impl std::fmt::Display) -> Box<EvalAltResult> {
    e.to_string().into()
}

/// Convert an event to the map handed to `on_event`. Every map carries a
/// `type` key matching [`CarrelEvent::name`]; payload fields keep their
/// Rust names.
pub fn event_to_map(event: &CarrelEvent) -> RhaiMap {
    let mut map = RhaiMap::new();
    map.insert("type".into(), event.name().into());
    match event {
        CarrelEvent::AppStarted => {}
        CarrelEvent::BookImported {
            book_id,
            format,
            source,
        } => {
            map.insert("book_id".into(), book_id.clone().into());
            map.insert("format".into(), format.to_string().into());
            map.insert("source".into(), format!("{source:?}").into());
        }
        CarrelEvent::BookOpened { book_id }
        | CarrelEvent::BookClosed { book_id }
        | CarrelEvent::BookFinished { book_id } => {
            map.insert("book_id".into(), book_id.clone().into());
        }
        CarrelEvent::HighlightCreated {
            book_id,
            highlight_id,
        } => {
            map.insert("book_id".into(), book_id.clone().into());
            map.insert("highlight_id".into(), highlight_id.clone().into());
        }
        CarrelEvent::HighlightUpdated { highlight_id }
        | CarrelEvent::HighlightDeleted { highlight_id } => {
            map.insert("highlight_id".into(), highlight_id.clone().into());
        }
        CarrelEvent::BookmarkCreated {
            book_id,
            bookmark_id,
        } => {
            map.insert("book_id".into(), book_id.clone().into());
            map.insert("bookmark_id".into(), bookmark_id.clone().into());
        }
        CarrelEvent::MetadataEnriched { book_id, provider } => {
            map.insert("book_id".into(), book_id.clone().into());
            map.insert("provider".into(), provider.clone().into());
        }
        CarrelEvent::BackupCompleted { provider, success } => {
            map.insert("provider".into(), provider.clone().into());
            map.insert("success".into(), (*success).into());
        }
        CarrelEvent::SyncCompleted { direction, success } => {
            map.insert("direction".into(), format!("{direction:?}").into());
            map.insert("success".into(), (*success).into());
        }
    }
    map
}

fn book_to_map(book: &Book) -> RhaiMap {
    let mut map = RhaiMap::new();
    map.insert("id".into(), book.id.clone().into());
    map.insert("title".into(), book.title.clone().into());
    map.insert("author".into(), book.author.clone().into());
    map.insert("format".into(), book.format.to_string().into());
    map.insert("total_chapters".into(), (book.total_chapters as i64).into());
    map.insert(
        "series".into(),
        book.series.clone().map(Into::into).unwrap_or(Dynamic::UNIT),
    );
    map.insert(
        "volume".into(),
        book.volume
            .map(|v| Dynamic::from(v as i64))
            .unwrap_or(Dynamic::UNIT),
    );
    map.insert(
        "language".into(),
        book.language
            .clone()
            .map(Into::into)
            .unwrap_or(Dynamic::UNIT),
    );
    map.insert(
        "rating".into(),
        book.rating.map(Dynamic::from).unwrap_or(Dynamic::UNIT),
    );
    map
}

fn highlight_to_map(h: &Highlight) -> RhaiMap {
    let mut map = RhaiMap::new();
    map.insert("id".into(), h.id.clone().into());
    map.insert("book_id".into(), h.book_id.clone().into());
    map.insert("chapter_index".into(), (h.chapter_index as i64).into());
    map.insert("text".into(), h.text.clone().into());
    map.insert("color".into(), h.color.clone().into());
    map.insert(
        "note".into(),
        h.note.clone().map(Into::into).unwrap_or(Dynamic::UNIT),
    );
    map.insert("created_at".into(), h.created_at.into());
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::ImportSource;
    use crate::models::BookFormat;

    struct MockServices {
        notes: Mutex<Vec<(String, String)>>,
        imports: Mutex<Vec<String>>,
    }

    impl HostServices for MockServices {
        fn notify(&self, title: &str, body: &str) {
            self.notes
                .lock()
                .unwrap()
                .push((title.to_string(), body.to_string()));
        }

        fn import_from_url(&self, url: &str) -> Result<String, String> {
            self.imports.lock().unwrap().push(url.to_string());
            Ok(format!("book-for-{url}"))
        }
    }

    struct Fixture {
        deps: RuntimeDeps,
        services: Arc<MockServices>,
        _dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::create_pool(&dir.path().join("t.db")).unwrap();
        let services = Arc::new(MockServices {
            notes: Mutex::new(Vec::new()),
            imports: Mutex::new(Vec::new()),
        });
        Fixture {
            deps: RuntimeDeps {
                pool,
                services: services.clone(),
            },
            services,
            _dir: dir,
        }
    }

    fn sample_book(id: &str, title: &str, author: &str) -> Book {
        Book {
            id: id.into(),
            title: title.into(),
            author: author.into(),
            file_path: format!("{id}.epub"),
            cover_path: None,
            total_chapters: 10,
            added_at: 0,
            format: BookFormat::Epub,
            file_hash: None,
            description: None,
            genres: None,
            rating: None,
            isbn: None,
            openlibrary_key: None,
            enrichment_status: None,
            series: None,
            volume: None,
            language: None,
            publisher: None,
            publish_year: None,
            is_imported: true,
            want_to_read: false,
        }
    }

    fn imported(book_id: &str) -> CarrelEvent {
        CarrelEvent::BookImported {
            book_id: book_id.into(),
            format: BookFormat::Epub,
            source: ImportSource::Manual,
        }
    }

    /// Grant a set of permissions with no parameters (test convenience).
    fn perms(ps: &[Permission]) -> Vec<(Permission, Option<String>)> {
        ps.iter().map(|p| (*p, None)).collect()
    }

    #[test]
    fn event_map_carries_type_and_payload_fields() {
        let map = event_to_map(&imported("b1"));
        assert_eq!(map["type"].clone().into_string().unwrap(), "BookImported");
        assert_eq!(map["book_id"].clone().into_string().unwrap(), "b1");
        assert_eq!(map["format"].clone().into_string().unwrap(), "epub");
        assert_eq!(map["source"].clone().into_string().unwrap(), "Manual");

        let map = event_to_map(&CarrelEvent::SyncCompleted {
            direction: crate::events::SyncDirection::Push,
            success: false,
        });
        assert_eq!(map["type"].clone().into_string().unwrap(), "SyncCompleted");
        assert_eq!(map["direction"].clone().into_string().unwrap(), "Push");
        assert!(!map["success"].as_bool().unwrap());
    }

    #[test]
    fn granted_notify_reaches_host_services() {
        let f = fixture();
        let rt = PluginRuntime::load(
            r#"fn on_event(event) { notify("hi", event.type); }"#,
            &perms(&[Permission::Notify]),
            &[],
            f.deps.clone(),
        )
        .unwrap();

        rt.dispatch(&imported("b1")).unwrap();
        assert_eq!(
            f.services.notes.lock().unwrap().as_slice(),
            &[("hi".to_string(), "BookImported".to_string())]
        );
    }

    #[test]
    fn ungranted_function_is_absent_from_scope() {
        let f = fixture();
        let rt = PluginRuntime::load(
            r#"fn on_event(event) { notify("hi", "there"); }"#,
            &perms(&[]), // notify NOT granted
            &[],
            f.deps.clone(),
        )
        .unwrap();

        let err = rt.dispatch(&imported("b1")).unwrap_err().to_string();
        assert!(err.contains("notify"), "got: {err}");
        assert!(f.services.notes.lock().unwrap().is_empty());
    }

    #[test]
    fn get_book_returns_map_and_unit_for_missing() {
        let f = fixture();
        {
            let conn = f.deps.pool.get().unwrap();
            db::insert_book(&conn, &sample_book("b1", "Dune", "Herbert")).unwrap();
        }
        let rt = PluginRuntime::load(
            r#"fn on_event(event) {
                let b = get_book(event.book_id);
                notify(b.title, b.author);
                let missing = get_book("nope");
                if missing == () { notify("missing", "unit"); }
            }"#,
            &perms(&[Permission::ReadLibrary, Permission::Notify]),
            &[],
            f.deps.clone(),
        )
        .unwrap();

        rt.dispatch(&imported("b1")).unwrap();
        let notes = f.services.notes.lock().unwrap();
        assert_eq!(notes[0], ("Dune".to_string(), "Herbert".to_string()));
        assert_eq!(notes[1], ("missing".to_string(), "unit".to_string()));
    }

    #[test]
    fn find_books_filters_by_title_or_author() {
        let f = fixture();
        {
            let conn = f.deps.pool.get().unwrap();
            db::insert_book(&conn, &sample_book("b1", "Dune", "Frank Herbert")).unwrap();
            db::insert_book(&conn, &sample_book("b2", "Hyperion", "Dan Simmons")).unwrap();
            db::insert_book(&conn, &sample_book("b3", "Dune Messiah", "Frank Herbert")).unwrap();
        }
        let rt = PluginRuntime::load(
            r#"fn on_event(event) {
                let dune = find_books("dune");
                notify("dune", dune.len().to_string());
                let herbert = find_books("herbert");
                notify("herbert", herbert.len().to_string());
            }"#,
            &perms(&[Permission::ReadLibrary, Permission::Notify]),
            &[],
            f.deps.clone(),
        )
        .unwrap();

        rt.dispatch(&imported("b1")).unwrap();
        let notes = f.services.notes.lock().unwrap();
        assert_eq!(notes[0].1, "2");
        assert_eq!(notes[1].1, "2");
    }

    /// `docs/PLUGINS.md` promises "array (max 50)". That cap used to be a
    /// `.take(50)` over a whole-table read and is now the query's `LIMIT`;
    /// this pins that the promise still holds across the move. Note what it
    /// does *not* prove: a `.take(50)` over an unlimited query would pass it
    /// identically, so it says nothing about the host having stopped
    /// materialising every book. That part is visible in the SQL, not here.
    #[test]
    fn find_books_caps_results_at_fifty() {
        let f = fixture();
        {
            let conn = f.deps.pool.get().unwrap();
            for i in 0..60 {
                db::insert_book(
                    &conn,
                    &sample_book(&format!("b{i:02}"), "Dune Chronicle", "Frank Herbert"),
                )
                .unwrap();
            }
        }
        let rt = PluginRuntime::load(
            r#"fn on_event(event) {
                notify("count", find_books("dune").len().to_string());
            }"#,
            &perms(&[Permission::ReadLibrary, Permission::Notify]),
            &[],
            f.deps.clone(),
        )
        .unwrap();

        rt.dispatch(&imported("b00")).unwrap();
        let notes = f.services.notes.lock().unwrap();
        assert_eq!(notes[0].1, "50", "60 matches must be capped at 50");
    }

    /// Every fixture book here shares `added_at = 0`, so before this went
    /// through the query module the 50 that survived `.take(50)` were whichever
    /// 50 SQLite happened to return. `BookSort::DateAdded` ends in `id`, so the
    /// selection is now deterministic — the first page by id among the ties.
    #[test]
    fn find_books_is_deterministic_across_added_at_ties() {
        let f = fixture();
        {
            let conn = f.deps.pool.get().unwrap();
            for i in (0..60).rev() {
                db::insert_book(
                    &conn,
                    &sample_book(&format!("b{i:02}"), "Dune Chronicle", "Frank Herbert"),
                )
                .unwrap();
            }
        }
        let rt = PluginRuntime::load(
            r#"fn on_event(event) {
                let found = find_books("dune");
                notify("first", found[0].id);
                notify("last", found[49].id);
            }"#,
            &perms(&[Permission::ReadLibrary, Permission::Notify]),
            &[],
            f.deps.clone(),
        )
        .unwrap();

        rt.dispatch(&imported("b00")).unwrap();
        let notes = f.services.notes.lock().unwrap();
        // Inserted in descending id order, so insertion order cannot produce
        // this — only the `ORDER BY ... b.id` tie-break can.
        assert_eq!(notes[0].1, "b00");
        assert_eq!(notes[1].1, "b49");
    }

    /// Rust's `to_lowercase` is Unicode-aware and SQLite's `LOWER()` is not, so
    /// moving this predicate into SQL is only equivalent because of the
    /// `carrel_lower` UDF. A plugin searching an accented title must still
    /// find it.
    #[test]
    fn find_books_folds_unicode_case() {
        let f = fixture();
        {
            let conn = f.deps.pool.get().unwrap();
            db::insert_book(
                &conn,
                &sample_book("u1", "Éducation sentimentale", "Gustave Flaubert"),
            )
            .unwrap();
            db::insert_book(&conn, &sample_book("u2", "Hyperion", "Dan Simmons")).unwrap();
        }
        let rt = PluginRuntime::load(
            r#"fn on_event(event) {
                notify("accented", find_books("éducation").len().to_string());
                notify("ascii", find_books("education").len().to_string());
            }"#,
            &perms(&[Permission::ReadLibrary, Permission::Notify]),
            &[],
            f.deps.clone(),
        )
        .unwrap();

        rt.dispatch(&imported("u1")).unwrap();
        let notes = f.services.notes.lock().unwrap();
        assert_eq!(
            notes[0].1, "1",
            "accented needle must match the accented title"
        );
        assert_eq!(notes[1].1, "0", "to_lowercase does not fold accents away");
    }

    #[test]
    fn add_and_remove_tag_mutate_db() {
        let f = fixture();
        {
            let conn = f.deps.pool.get().unwrap();
            db::insert_book(&conn, &sample_book("b1", "Dune", "Herbert")).unwrap();
        }
        let rt = PluginRuntime::load(
            r#"fn on_event(event) {
                add_tag(event.book_id, "sci-fi");
                add_tag(event.book_id, "epic");
                remove_tag(event.book_id, "epic");
            }"#,
            &perms(&[Permission::WriteTags]),
            &[],
            f.deps.clone(),
        )
        .unwrap();

        rt.dispatch(&imported("b1")).unwrap();
        let conn = f.deps.pool.get().unwrap();
        let tags: Vec<String> = db::get_book_tags(&conn, "b1")
            .unwrap()
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        assert_eq!(tags, vec!["sci-fi"]);
    }

    #[test]
    fn runaway_script_is_aborted_by_budget() {
        let f = fixture();
        let rt = PluginRuntime::load(
            r#"fn on_event(event) { let x = 0; loop { x += 1; } }"#,
            &perms(&[]),
            &[],
            f.deps.clone(),
        )
        .unwrap();

        let start = Instant::now();
        assert!(rt.dispatch(&imported("b1")).is_err());
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "budget did not abort the loop in time"
        );
    }

    #[test]
    fn script_without_on_event_errors_on_dispatch() {
        let f = fixture();
        let rt =
            PluginRuntime::load(r#"fn other() { 1 }"#, &perms(&[]), &[], f.deps.clone()).unwrap();
        let err = rt.dispatch(&imported("b1")).unwrap_err().to_string();
        assert!(err.contains("on_event"), "got: {err}");
    }

    #[test]
    fn syntax_error_fails_at_load() {
        let f = fixture();
        assert!(PluginRuntime::load("fn on_event(e) {", &perms(&[]), &[], f.deps).is_err());
    }

    fn sample_highlight(id: &str, book_id: &str, text: &str) -> Highlight {
        Highlight {
            id: id.into(),
            book_id: book_id.into(),
            chapter_index: 0,
            text: text.into(),
            color: "yellow".into(),
            note: None,
            start_offset: 0,
            end_offset: text.len() as u32,
            created_at: 0,
            updated_at: 0,
            deleted_at: None,
        }
    }

    #[test]
    fn get_highlights_returns_book_highlights() {
        let f = fixture();
        {
            let conn = f.deps.pool.get().unwrap();
            db::insert_book(&conn, &sample_book("b1", "Dune", "Herbert")).unwrap();
            db::insert_highlight(&conn, &sample_highlight("h1", "b1", "spice")).unwrap();
            db::insert_highlight(&conn, &sample_highlight("h2", "b1", "sandworm")).unwrap();
        }
        let rt = PluginRuntime::load(
            r#"fn on_event(event) {
                let hs = get_highlights(event.book_id);
                notify("count", hs.len().to_string());
                notify("first", hs[0].text);
            }"#,
            &perms(&[Permission::ReadHighlights, Permission::Notify]),
            &[],
            f.deps.clone(),
        )
        .unwrap();

        rt.dispatch(&imported("b1")).unwrap();
        let notes = f.services.notes.lock().unwrap();
        assert_eq!(notes[0].1, "2");
        assert_eq!(notes[1].1, "spice");
    }

    #[test]
    fn write_files_absent_without_granted_directory() {
        let f = fixture();
        // write:files granted but with no directory parameter → no host fn.
        let rt = PluginRuntime::load(
            r#"fn on_event(event) { write_file("x.txt", "hi"); }"#,
            &[(Permission::WriteFiles, None)],
            &[],
            f.deps.clone(),
        )
        .unwrap();
        let err = rt.dispatch(&imported("b1")).unwrap_err().to_string();
        assert!(err.contains("write_file"), "got: {err}");
    }

    #[test]
    fn write_and_append_file_within_granted_dir() {
        let f = fixture();
        let export = f._dir.path().join("export");
        std::fs::create_dir_all(&export).unwrap();
        let dir = export.to_string_lossy().to_string();

        let rt = PluginRuntime::load(
            r##"fn on_event(event) {
                write_file("notes.md", "# Notes\n");
                append_file("notes.md", "- one\n");
                append_file("sub/deep.md", "nested\n");
            }"##,
            &[(Permission::WriteFiles, Some(dir))],
            &[],
            f.deps.clone(),
        )
        .unwrap();

        rt.dispatch(&imported("b1")).unwrap();
        assert_eq!(
            std::fs::read_to_string(export.join("notes.md")).unwrap(),
            "# Notes\n- one\n"
        );
        assert_eq!(
            std::fs::read_to_string(export.join("sub/deep.md")).unwrap(),
            "nested\n"
        );
    }

    #[test]
    fn write_file_rejects_path_traversal() {
        let f = fixture();
        let export = f._dir.path().join("export");
        std::fs::create_dir_all(&export).unwrap();
        let dir = export.to_string_lossy().to_string();

        for bad in ["../escape.txt", "/etc/evil", "a/../../escape.txt"] {
            let rt = PluginRuntime::load(
                &format!(r#"fn on_event(event) {{ write_file("{bad}", "x"); }}"#),
                &[(Permission::WriteFiles, Some(dir.clone()))],
                &[],
                f.deps.clone(),
            )
            .unwrap();
            assert!(
                rt.dispatch(&imported("b1")).is_err(),
                "traversal not rejected for {bad}"
            );
        }
        // Nothing escaped the export dir.
        assert!(!f._dir.path().join("escape.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn write_file_rejects_symlinked_leaf_escaping_root() {
        let f = fixture();
        let export = f._dir.path().join("export");
        std::fs::create_dir_all(&export).unwrap();
        let outside = f._dir.path().join("outside.txt");
        std::fs::write(&outside, "original").unwrap();
        // A symlink inside the export dir pointing at a file outside it.
        std::os::unix::fs::symlink(&outside, export.join("link.txt")).unwrap();
        let dir = export.to_string_lossy().to_string();

        let rt = PluginRuntime::load(
            r#"fn on_event(event) { write_file("link.txt", "pwned"); }"#,
            &[(Permission::WriteFiles, Some(dir))],
            &[],
            f.deps.clone(),
        )
        .unwrap();

        assert!(rt.dispatch(&imported("b1")).is_err());
        // The outside file is untouched.
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "original");
    }

    #[test]
    fn write_file_rejects_oversized_write() {
        let f = fixture();
        let export = f._dir.path().join("export");
        std::fs::create_dir_all(&export).unwrap();
        let dir = export.to_string_lossy().to_string();

        let rt = PluginRuntime::load(
            // 2 MB string exceeds the 1 MB string-size cap, so the write is
            // rejected before any bytes hit disk.
            r#"fn on_event(event) {
                let big = "x".repeat(2 * 1024 * 1024);
                write_file("big.txt", big);
            }"#,
            &[(Permission::WriteFiles, Some(dir))],
            &[],
            f.deps.clone(),
        )
        .unwrap();
        assert!(rt.dispatch(&imported("b1")).is_err());
        assert!(!export.join("big.txt").exists());
    }

    #[test]
    fn http_get_absent_without_declared_hosts() {
        let f = fixture();
        // network granted but the manifest declared no hosts → no host fn.
        let rt = PluginRuntime::load(
            r#"fn on_event(event) { http_get("https://example.org"); }"#,
            &[(Permission::Network, None)],
            &[],
            f.deps.clone(),
        )
        .unwrap();
        let err = rt.dispatch(&imported("b1")).unwrap_err().to_string();
        assert!(err.contains("http_get"), "got: {err}");
    }

    #[test]
    fn http_get_rejects_host_outside_allowlist() {
        let f = fixture();
        let rt = PluginRuntime::load(
            r#"fn on_event(event) { http_get("https://evil.example/x"); }"#,
            &[(Permission::Network, None)],
            &["standardebooks.org".to_string()],
            f.deps.clone(),
        )
        .unwrap();
        let err = rt.dispatch(&imported("b1")).unwrap_err().to_string();
        assert!(
            err.contains("not in the plugin's declared network hosts"),
            "got: {err}"
        );
    }

    #[test]
    fn http_get_rejects_private_address_even_if_allowlisted() {
        let f = fixture();
        // Allowlisting a private host must still be blocked by the SSRF guard.
        let rt = PluginRuntime::load(
            r#"fn on_event(event) { http_get("http://127.0.0.1:7788/opds"); }"#,
            &[(Permission::Network, None)],
            &["127.0.0.1".to_string()],
            f.deps.clone(),
        )
        .unwrap();
        let err = rt.dispatch(&imported("b1")).unwrap_err().to_string();
        assert!(err.contains("blocked"), "got: {err}");
    }

    #[test]
    fn http_get_loopback_blocked_even_when_allowlisted_with_port() {
        // host:port allowlist entry must still not defeat the SSRF guard.
        let f = fixture();
        let rt = PluginRuntime::load(
            r#"fn on_event(event) { http_get("http://localhost:8080/x"); }"#,
            &[(Permission::Network, None)],
            &["localhost:8080".to_string()],
            f.deps.clone(),
        )
        .unwrap();
        let err = rt.dispatch(&imported("b1")).unwrap_err().to_string();
        assert!(err.contains("blocked"), "got: {err}");
    }

    #[test]
    fn import_from_url_routes_to_host_services() {
        let f = fixture();
        let rt = PluginRuntime::load(
            r#"fn on_event(event) { import_from_url("https://standardebooks.org/x.epub"); }"#,
            &[(Permission::ImportBooks, None)],
            &[],
            f.deps.clone(),
        )
        .unwrap();
        rt.dispatch(&imported("b1")).unwrap();
        assert_eq!(
            f.services.imports.lock().unwrap().as_slice(),
            &["https://standardebooks.org/x.epub".to_string()]
        );
    }

    #[test]
    fn import_from_url_absent_without_permission() {
        let f = fixture();
        let rt = PluginRuntime::load(
            r#"fn on_event(event) { import_from_url("https://x.test/y"); }"#,
            &perms(&[]),
            &[],
            f.deps.clone(),
        )
        .unwrap();
        let err = rt.dispatch(&imported("b1")).unwrap_err().to_string();
        assert!(err.contains("import_from_url"), "got: {err}");
        assert!(f.services.imports.lock().unwrap().is_empty());
    }
}
