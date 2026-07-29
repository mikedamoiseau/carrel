use opendal::blocking::Operator;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

use crate::error::{FolioError, FolioResult};

static FALLBACK_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Identify which config fields contain secrets (passwords/keys).
pub fn secret_keys(provider_type: &ProviderType) -> Vec<&'static str> {
    match provider_type {
        ProviderType::S3 => vec!["access_key_id", "secret_access_key"],
        ProviderType::Ftp => vec!["password"],
        ProviderType::Sftp => vec![],
        ProviderType::Webdav => vec!["password"],
        ProviderType::Fs => vec![],
    }
}

/// Store secret values in the OS keychain. Returns the config with secrets removed.
/// Returns an error if any keychain write fails — caller should NOT save config to DB.
///
/// The `folio-backup-…` service prefix used here and in [`load_secrets`] /
/// [`remove_secrets`] survives the Carrel rename on purpose: it keys entries
/// already in every user's keychain. See CLAUDE.md, "Legacy `folio` identifiers".
pub fn store_secrets(config: &BackupConfig) -> FolioResult<BackupConfig> {
    let secrets = secret_keys(&config.provider_type);
    let mut clean = config.clone();
    for key in &secrets {
        if let Some(value) = config.values.get(*key) {
            if value.is_empty() {
                continue;
            }
            let service = format!("folio-backup-{:?}-{}", config.provider_type, key);
            let entry = keyring::Entry::new(&service, "default").map_err(|e| match e {
                keyring::Error::PlatformFailure(err) => {
                    FolioError::internal(format!("Keychain unavailable for {key}: {err}"))
                }
                _ => FolioError::internal(format!("Failed to access keychain for {key}: {e}")),
            })?;
            entry.set_password(value).map_err(|e| match e {
                keyring::Error::PlatformFailure(err) => FolioError::permission(format!(
                    "Keychain access denied while storing '{key}': {err}"
                )),
                _ => {
                    FolioError::internal(format!("Failed to store secret '{key}' in keychain: {e}"))
                }
            })?;
            clean.values.remove(*key);
        }
    }
    Ok(clean)
}

/// Load secret values from the OS keychain into a config.
pub fn load_secrets(config: &mut BackupConfig) -> FolioResult<()> {
    let secrets = secret_keys(&config.provider_type);
    for key in &secrets {
        if config.values.contains_key(*key) {
            continue; // already populated (e.g. test config)
        }
        let service = format!("folio-backup-{:?}-{}", config.provider_type, key);
        let entry = keyring::Entry::new(&service, "default").map_err(|e| {
            FolioError::internal(format!("Failed to access keychain for {key}: {e}"))
        })?;
        let pw = entry.get_password().map_err(|e| match e {
            keyring::Error::NoEntry => {
                FolioError::not_found(format!("Secret '{key}' not found in keychain"))
            }
            _ => FolioError::internal(format!("Failed to load secret '{key}' from keychain: {e}")),
        })?;
        config.values.insert(key.to_string(), pw);
    }
    Ok(())
}

pub fn remove_secrets(config: &BackupConfig) -> FolioResult<()> {
    let secrets = secret_keys(&config.provider_type);
    for key in &secrets {
        let service = format!("folio-backup-{:?}-{}", config.provider_type, key);
        if let Ok(entry) = keyring::Entry::new(&service, "default") {
            let _ = entry.delete_credential();
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderType {
    S3,
    Ftp,
    Sftp,
    Webdav,
    Fs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigField {
    pub key: String,
    pub label: String,
    pub field_type: String,
    pub required: bool,
    pub placeholder: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub provider_type: ProviderType,
    pub label: String,
    pub fields: Vec<ConfigField>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupConfig {
    pub provider_type: ProviderType,
    pub values: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum ConnectionTestResult {
    Ok { latency_ms: u64 },
    AuthFailed { message: String },
    PermissionDenied { message: String },
    NetworkError { message: String },
    Timeout,
}

pub fn classify_opendal_error(err: &opendal::Error) -> ConnectionTestResult {
    match err.kind() {
        opendal::ErrorKind::PermissionDenied => ConnectionTestResult::AuthFailed {
            message: err.to_string(),
        },
        opendal::ErrorKind::ConfigInvalid => ConnectionTestResult::NetworkError {
            message: err.to_string(),
        },
        opendal::ErrorKind::NotFound => ConnectionTestResult::NetworkError {
            message: err.to_string(),
        },
        _ => ConnectionTestResult::NetworkError {
            message: err.to_string(),
        },
    }
}

pub fn test_connection(config: &BackupConfig) -> ConnectionTestResult {
    let start = std::time::Instant::now();

    // Pre-flight: verify SSH key file exists if configured
    if config.provider_type == ProviderType::Sftp {
        if let Some(key_path) = config.values.get("key") {
            if !key_path.is_empty() {
                let expanded = if key_path.starts_with("~/") {
                    dirs::home_dir()
                        .map(|h| h.join(&key_path[2..]))
                        .unwrap_or_else(|| std::path::PathBuf::from(key_path))
                } else {
                    std::path::PathBuf::from(key_path)
                };
                if !expanded.exists() {
                    return ConnectionTestResult::AuthFailed {
                        message: format!("SSH key not found: {}", expanded.display()),
                    };
                }
            }
        }
    }

    let op = match build_operator(config) {
        Ok(op) => op,
        Err(e) => {
            return ConnectionTestResult::NetworkError {
                message: e.to_string(),
            }
        }
    };

    // Step 1: List root — confirms authentication
    if let Err(e) = op.list("/") {
        return classify_opendal_error(&e);
    }

    // Step 2: Write sentinel — confirms write permission
    let probe_path = format!(
        ".folio-connection-test/{}.tmp",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    if let Err(e) = op.write(&probe_path, b"ok".to_vec()) {
        return ConnectionTestResult::PermissionDenied {
            message: e.to_string(),
        };
    }

    // Step 3: Cleanup — best-effort, don't fail on delete errors
    let _ = op.delete(&probe_path);

    let latency_ms = start.elapsed().as_millis() as u64;
    ConnectionTestResult::Ok { latency_ms }
}

pub fn provider_schemas() -> Vec<ProviderInfo> {
    vec![
        ProviderInfo {
            provider_type: ProviderType::S3,
            label: "Amazon S3".to_string(),
            fields: vec![
                ConfigField {
                    key: "bucket".into(),
                    label: "Bucket".into(),
                    field_type: "text".into(),
                    required: true,
                    placeholder: "my-carrel-backups".into(),
                },
                ConfigField {
                    key: "region".into(),
                    label: "Region".into(),
                    field_type: "text".into(),
                    required: true,
                    placeholder: "us-east-1".into(),
                },
                ConfigField {
                    key: "access_key_id".into(),
                    label: "Access Key ID".into(),
                    field_type: "password".into(),
                    required: true,
                    placeholder: "".into(),
                },
                ConfigField {
                    key: "secret_access_key".into(),
                    label: "Secret Access Key".into(),
                    field_type: "password".into(),
                    required: true,
                    placeholder: "".into(),
                },
                ConfigField {
                    key: "root".into(),
                    label: "Path prefix".into(),
                    field_type: "text".into(),
                    required: false,
                    placeholder: "/carrel-backup".into(),
                },
            ],
        },
        ProviderInfo {
            provider_type: ProviderType::Ftp,
            label: "FTP Server".to_string(),
            fields: vec![
                ConfigField {
                    key: "endpoint".into(),
                    label: "Server".into(),
                    field_type: "text".into(),
                    required: true,
                    placeholder: "ftp.example.com".into(),
                },
                ConfigField {
                    key: "user".into(),
                    label: "Username".into(),
                    field_type: "text".into(),
                    required: false,
                    placeholder: "anonymous".into(),
                },
                ConfigField {
                    key: "password".into(),
                    label: "Password".into(),
                    field_type: "password".into(),
                    required: false,
                    placeholder: "".into(),
                },
                ConfigField {
                    key: "use_tls".into(),
                    label: "Use TLS (FTPS)".into(),
                    field_type: "checkbox".into(),
                    required: false,
                    placeholder: "".into(),
                },
                ConfigField {
                    key: "root".into(),
                    label: "Remote path".into(),
                    field_type: "text".into(),
                    required: false,
                    placeholder: "/carrel-backup".into(),
                },
            ],
        },
        ProviderInfo {
            provider_type: ProviderType::Sftp,
            label: "SFTP (SSH)".to_string(),
            fields: vec![
                ConfigField {
                    key: "endpoint".into(),
                    label: "Server".into(),
                    field_type: "text".into(),
                    required: true,
                    placeholder: "sftp.example.com:22".into(),
                },
                ConfigField {
                    key: "user".into(),
                    label: "Username".into(),
                    field_type: "text".into(),
                    required: true,
                    placeholder: "".into(),
                },
                ConfigField {
                    key: "key".into(),
                    label: "SSH private key path".into(),
                    field_type: "text".into(),
                    required: false,
                    placeholder: "~/.ssh/id_rsa".into(),
                },
                ConfigField {
                    key: "root".into(),
                    label: "Remote path".into(),
                    field_type: "text".into(),
                    required: false,
                    placeholder: "/home/user/carrel-backup".into(),
                },
                ConfigField {
                    key: "known_hosts_strategy".into(),
                    label: "Skip host key verification (insecure)".into(),
                    field_type: "checkbox".into(),
                    required: false,
                    placeholder: "".into(),
                },
            ],
        },
        ProviderInfo {
            provider_type: ProviderType::Webdav,
            label: "WebDAV (Nextcloud, etc.)".to_string(),
            fields: vec![
                ConfigField {
                    key: "endpoint".into(),
                    label: "URL".into(),
                    field_type: "text".into(),
                    required: true,
                    placeholder: "https://cloud.example.com/remote.php/dav/files/user/".into(),
                },
                ConfigField {
                    key: "username".into(),
                    label: "Username".into(),
                    field_type: "text".into(),
                    required: true,
                    placeholder: "".into(),
                },
                ConfigField {
                    key: "password".into(),
                    label: "Password".into(),
                    field_type: "password".into(),
                    required: true,
                    placeholder: "".into(),
                },
                ConfigField {
                    key: "root".into(),
                    label: "Remote path".into(),
                    field_type: "text".into(),
                    required: false,
                    placeholder: "/carrel-backup".into(),
                },
            ],
        },
    ]
}

/// Build a blocking OpenDAL operator from a BackupConfig.
///
/// The blocking::Operator wraps the async Operator and requires a tokio runtime
/// to be active. In tests we spin up a dedicated runtime; in Tauri commands
/// the Tauri runtime satisfies this requirement.
pub fn build_operator(config: &BackupConfig) -> FolioResult<Operator> {
    // Helper: enter the current tokio handle if one exists, otherwise spin up a
    // temporary single-threaded runtime so that blocking::Operator::new succeeds
    // from any context (including unit tests with no ambient runtime).
    fn make_blocking(async_op: opendal::Operator) -> FolioResult<Operator> {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let _guard = handle.enter();
            Operator::new(async_op).map_err(|e| {
                FolioError::internal(format!("Failed to create blocking operator: {e}"))
            })
        } else {
            let rt = FALLBACK_RUNTIME.get_or_init(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to build fallback tokio runtime")
            });
            let _guard = rt.enter();
            Operator::new(async_op).map_err(|e| {
                FolioError::internal(format!("Failed to create blocking operator: {e}"))
            })
        }
    }

    match config.provider_type {
        ProviderType::S3 => {
            let mut builder = opendal::services::S3::default();
            if let Some(v) = config.values.get("bucket") {
                builder = builder.bucket(v);
            }
            if let Some(v) = config.values.get("region") {
                builder = builder.region(v);
            }
            if let Some(v) = config.values.get("access_key_id") {
                builder = builder.access_key_id(v);
            }
            if let Some(v) = config.values.get("secret_access_key") {
                builder = builder.secret_access_key(v);
            }
            if let Some(v) = config.values.get("root") {
                if !v.is_empty() {
                    builder = builder.root(v);
                }
            }
            let async_op = opendal::Operator::new(builder)
                .map(|b| b.finish())
                .map_err(|e| FolioError::network(format!("Failed to create S3 operator: {e}")))?;
            make_blocking(async_op)
        }
        ProviderType::Ftp => {
            let mut builder = opendal::services::Ftp::default();
            if let Some(v) = config.values.get("endpoint") {
                // Prepend ftps:// if TLS is enabled and endpoint has no scheme
                let endpoint = if config.values.get("use_tls").is_some_and(|t| t == "true")
                    && !v.starts_with("ftps://")
                    && !v.starts_with("ftp://")
                {
                    format!("ftps://{v}")
                } else {
                    v.clone()
                };
                builder = builder.endpoint(&endpoint);
            }
            if let Some(v) = config.values.get("user") {
                builder = builder.user(v);
            }
            if let Some(v) = config.values.get("password") {
                builder = builder.password(v);
            }
            if let Some(v) = config.values.get("root") {
                if !v.is_empty() {
                    builder = builder.root(v);
                }
            }
            let async_op = opendal::Operator::new(builder)
                .map(|b| b.finish())
                .map_err(|e| FolioError::network(format!("Failed to create FTP operator: {e}")))?;
            make_blocking(async_op)
        }
        #[cfg(feature = "sftp")]
        ProviderType::Sftp => {
            let mut builder = opendal::services::Sftp::default();
            if let Some(v) = config.values.get("endpoint") {
                builder = builder.endpoint(v);
            }
            if let Some(v) = config.values.get("user") {
                builder = builder.user(v);
            }
            if let Some(v) = config.values.get("key") {
                if !v.is_empty() {
                    builder = builder.key(v);
                }
            }
            if let Some(v) = config.values.get("root") {
                if !v.is_empty() {
                    builder = builder.root(v);
                }
            }
            let skip_host_key = config
                .values
                .get("known_hosts_strategy")
                .is_some_and(|v| v == "true");
            builder = builder.known_hosts_strategy(if skip_host_key { "accept" } else { "strict" });
            let async_op = opendal::Operator::new(builder)
                .map(|b| b.finish())
                .map_err(|e| FolioError::network(format!("Failed to create SFTP operator: {e}")))?;
            make_blocking(async_op)
        }
        #[cfg(not(feature = "sftp"))]
        ProviderType::Sftp => Err(FolioError::invalid(
            "SFTP is not available on this platform",
        )),
        ProviderType::Webdav => {
            let mut builder = opendal::services::Webdav::default();
            if let Some(v) = config.values.get("endpoint") {
                builder = builder.endpoint(v);
            }
            if let Some(v) = config.values.get("username") {
                builder = builder.username(v);
            }
            if let Some(v) = config.values.get("password") {
                builder = builder.password(v);
            }
            if let Some(v) = config.values.get("root") {
                if !v.is_empty() {
                    builder = builder.root(v);
                }
            }
            let async_op = opendal::Operator::new(builder)
                .map(|b| b.finish())
                .map_err(|e| {
                    FolioError::network(format!("Failed to create WebDAV operator: {e}"))
                })?;
            make_blocking(async_op)
        }
        ProviderType::Fs => {
            let mut builder = opendal::services::Fs::default();
            if let Some(v) = config.values.get("root") {
                builder = builder.root(v);
            }
            let async_op = opendal::Operator::new(builder)
                .map(|b| b.finish())
                .map_err(|e| FolioError::io(format!("Failed to create FS operator: {e}")))?;
            make_blocking(async_op)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncManifest {
    pub last_sync_at: i64,
    pub device_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    pub books_pushed: u32,
    pub progress_pushed: u32,
    pub bookmarks_pushed: u32,
    pub highlights_pushed: u32,
    pub collections_pushed: u32,
    pub files_pushed: u32,
    pub warnings: Vec<String>,
}

pub fn read_manifest(op: &Operator) -> SyncManifest {
    match op.read("manifest.json") {
        Ok(data) => serde_json::from_slice(&data.to_vec()).unwrap_or_default(),
        Err(_) => SyncManifest::default(),
    }
}

pub fn write_manifest(op: &Operator, manifest: &SyncManifest) -> FolioResult<()> {
    let json = serde_json::to_string_pretty(manifest)?;
    op.write("manifest.json", json.into_bytes())
        .map(|_| ())
        .map_err(|e| FolioError::network(format!("Failed to write manifest: {e}")))
}

pub fn push_json(op: &Operator, path: &str, data: &impl Serialize) -> FolioResult<()> {
    let json = serde_json::to_string(data)?;
    op.write(path, json.into_bytes())
        .map(|_| ())
        .map_err(|e| FolioError::network(format!("Failed to write {path}: {e}")))
}

pub fn pull_json<T: serde::de::DeserializeOwned>(op: &Operator, path: &str) -> FolioResult<T> {
    let data = op
        .read(path)
        .map_err(|e| FolioError::network(format!("Failed to read {path}: {e}")))?;
    serde_json::from_slice(&data.to_vec())
        .map_err(|e| FolioError::invalid(format!("Failed to parse {path}: {e}")))
}

pub fn push_file_if_missing(
    op: &Operator,
    remote_path: &str,
    local_path: &str,
) -> FolioResult<bool> {
    let local_size = std::fs::metadata(local_path)
        .map(|m| m.len())
        .map_err(|e| FolioError::io(format!("Cannot read {local_path}: {e}")))?;
    // Skip upload if remote file exists and size matches (catches partial uploads)
    if let Ok(meta) = op.stat(remote_path) {
        if meta.content_length() == local_size {
            return Ok(false);
        }
    }
    let data = std::fs::read(local_path)
        .map_err(|e| FolioError::io(format!("Cannot read {local_path}: {e}")))?;
    op.write(remote_path, data)
        .map_err(|e| FolioError::network(format!("Failed to upload {remote_path}: {e}")))?;
    Ok(true)
}

/// Upload the file at `key` in `storage` to `remote_path` unless the remote
/// already has a file of the same size. Backend-agnostic counterpart to
/// [`push_file_if_missing`]: checks `storage.size(key)` against the remote
/// `stat` *before* materializing bytes, so unchanged books never get fully
/// read — essential for large files and remote `Storage` backends.
pub fn push_storage_file_if_missing(
    op: &Operator,
    remote_path: &str,
    storage: &dyn crate::storage::Storage,
    key: &str,
) -> FolioResult<bool> {
    let local_size = storage.size(key)?;
    if let Ok(meta) = op.stat(remote_path) {
        if meta.content_length() == local_size {
            return Ok(false);
        }
    }
    let bytes = storage.get(key)?;
    op.write(remote_path, bytes)
        .map_err(|e| FolioError::network(format!("Failed to upload {remote_path}: {e}")))?;
    Ok(true)
}

pub fn run_incremental_backup(
    op: &Operator,
    conn: &rusqlite::Connection,
    library_storage: Option<&dyn crate::storage::Storage>,
) -> FolioResult<SyncResult> {
    run_incremental_backup_with_progress(op, conn, library_storage, &|_, _, _| {})
}

pub fn run_incremental_backup_with_progress(
    op: &Operator,
    conn: &rusqlite::Connection,
    library_storage: Option<&dyn crate::storage::Storage>,
    on_progress: &dyn Fn(&str, u32, u32),
) -> FolioResult<SyncResult> {
    let mut manifest = read_manifest(op);
    let since = manifest.last_sync_at;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let mut result = SyncResult {
        books_pushed: 0,
        progress_pushed: 0,
        bookmarks_pushed: 0,
        highlights_pushed: 0,
        collections_pushed: 0,
        files_pushed: 0,
        warnings: Vec::new(),
    };

    // Helper: collect rows, logging failures as warnings instead of silently dropping
    fn collect_rows<T>(
        rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
        entity: &str,
        warnings: &mut Vec<String>,
    ) -> Vec<T> {
        let mut items = Vec::new();
        for (i, row) in rows.enumerate() {
            match row {
                Ok(item) => items.push(item),
                Err(e) => warnings.push(format!("Skipped {} row {}: {}", entity, i, e)),
            }
        }
        items
    }

    // Books — always write full metadata, but only upload files for changed books
    let book_query = "SELECT id, title, author, file_path, cover_path, total_chapters, added_at, format, file_hash, description, genres, rating, isbn, openlibrary_key, enrichment_status, series, volume, language, publisher, publish_year, is_imported, want_to_read FROM books";
    let book_mapper = |row: &rusqlite::Row| {
        let format_str: String = row.get(7)?;
        Ok(crate::models::Book {
            id: row.get(0)?,
            title: row.get(1)?,
            author: row.get(2)?,
            file_path: row.get(3)?,
            cover_path: row.get(4)?,
            total_chapters: row.get(5)?,
            added_at: row.get(6)?,
            format: format_str
                .parse()
                .unwrap_or(crate::models::BookFormat::Epub),
            file_hash: row.get(8)?,
            description: row.get(9)?,
            genres: row.get(10)?,
            rating: row.get(11)?,
            isbn: row.get(12)?,
            openlibrary_key: row.get(13)?,
            enrichment_status: row.get(14)?,
            series: row.get(15)?,
            volume: row.get(16)?,
            language: row.get(17)?,
            publisher: row.get(18)?,
            publish_year: row.get(19)?,
            is_imported: row.get::<_, i32>(20).unwrap_or(1) != 0,
            want_to_read: row.get::<_, i32>(21).unwrap_or(0) != 0,
        })
    };

    // Full set for metadata JSON
    let all_books: Vec<crate::models::Book> = {
        let mut stmt = conn.prepare(book_query)?;
        let rows = stmt.query_map([], book_mapper)?;
        collect_rows(rows, "book", &mut result.warnings)
    };
    if !all_books.is_empty() {
        on_progress("Syncing metadata", 0, 0);
        push_json(op, "metadata/books.json", &all_books)?;
    }

    // Changed books only — upload files
    let changed_books: Vec<crate::models::Book> = {
        let query_with_filter = format!("{} WHERE updated_at > ?1", book_query);
        let mut stmt = conn.prepare(&query_with_filter)?;
        let rows = stmt.query_map(rusqlite::params![since], book_mapper)?;
        collect_rows(rows, "book", &mut result.warnings)
    };
    if !changed_books.is_empty() {
        result.books_pushed = changed_books.len() as u32;
        let total_files = changed_books.len() as u32;

        for (i, book) in changed_books.iter().enumerate() {
            on_progress("Uploading books", (i + 1) as u32, total_files);
            // Skip file upload for linked books — they're not in the library folder
            if !book.is_imported {
                continue;
            }
            if let Some(ref hash) = book.file_hash {
                let ext = std::path::Path::new(&book.file_path)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("epub");
                let remote_path = format!("files/{}.{}", hash, ext);

                // Imported books may carry either a storage key (post-M4)
                // or a legacy absolute path. Storage keys read through the
                // caller-owned `library_storage` so backups work against
                // any `Storage` backend; absolute paths still read through
                // `std::fs` since they point outside library storage.
                if std::path::Path::new(&book.file_path).is_absolute() {
                    if push_file_if_missing(op, &remote_path, &book.file_path)? {
                        result.files_pushed += 1;
                    }
                } else if let Some(storage) = library_storage {
                    if push_storage_file_if_missing(op, &remote_path, storage, &book.file_path)? {
                        result.files_pushed += 1;
                    }
                } else {
                    // No library storage passed and a key-form `file_path` —
                    // we have no way to resolve the key to bytes. Skip with
                    // a clear warning so the caller knows to wire up storage.
                    result.warnings.push(format!(
                        "Book '{}' has a storage-key file_path but no library storage was provided — skipped",
                        book.title
                    ));
                    continue;
                }
            } else {
                result.warnings.push(format!(
                    "Book '{}' has no file hash — file not uploaded",
                    book.title
                ));
            }
            if let Some(ref cover) = book.cover_path {
                let ext = std::path::Path::new(cover)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("jpg");
                let remote_path = format!("covers/{}.{}", book.id, ext);
                if let Err(e) = push_file_if_missing(op, &remote_path, cover) {
                    result
                        .warnings
                        .push(format!("Cover upload failed for '{}': {}", book.title, e));
                }
            }
        }
    }

    // Reading progress — always full set
    let progress: Vec<crate::models::ReadingProgress> = {
        let mut stmt = conn.prepare(
            "SELECT book_id, chapter_index, scroll_position, last_read_at FROM reading_progress",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(crate::models::ReadingProgress {
                book_id: row.get(0)?,
                chapter_index: row.get(1)?,
                scroll_position: row.get(2)?,
                last_read_at: row.get(3)?,
            })
        })?;
        collect_rows(rows, "progress", &mut result.warnings)
    };
    if !progress.is_empty() {
        result.progress_pushed = progress.len() as u32;
        on_progress("Syncing reading progress", 0, 0);
        push_json(op, "metadata/progress.json", &progress)?;
    }

    // Bookmarks — always full set
    let bookmarks: Vec<crate::models::Bookmark> = {
        let mut stmt = conn.prepare(
            "SELECT id, book_id, chapter_index, scroll_position, name, note, created_at, updated_at, deleted_at FROM bookmarks WHERE deleted_at IS NULL",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(crate::models::Bookmark {
                id: row.get(0)?,
                book_id: row.get(1)?,
                chapter_index: row.get(2)?,
                scroll_position: row.get(3)?,
                name: row.get(4)?,
                note: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                deleted_at: row.get(8)?,
            })
        })?;
        collect_rows(rows, "bookmark", &mut result.warnings)
    };
    if !bookmarks.is_empty() {
        result.bookmarks_pushed = bookmarks.len() as u32;
        on_progress("Syncing bookmarks", 0, 0);
        push_json(op, "metadata/bookmarks.json", &bookmarks)?;
    }

    // Highlights — always full set
    let highlights: Vec<crate::models::Highlight> = {
        let mut stmt = conn.prepare(
            "SELECT id, book_id, chapter_index, text, color, note, start_offset, end_offset, created_at, updated_at, deleted_at FROM highlights WHERE deleted_at IS NULL",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(crate::models::Highlight {
                id: row.get(0)?,
                book_id: row.get(1)?,
                chapter_index: row.get(2)?,
                text: row.get(3)?,
                color: row.get(4)?,
                note: row.get(5)?,
                start_offset: row.get(6)?,
                end_offset: row.get(7)?,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
                deleted_at: row.get(10)?,
            })
        })?;
        collect_rows(rows, "highlight", &mut result.warnings)
    };
    if !highlights.is_empty() {
        result.highlights_pushed = highlights.len() as u32;
        on_progress("Syncing highlights", 0, 0);
        push_json(op, "metadata/highlights.json", &highlights)?;
    }

    // Collections (always push full set)
    let collections = crate::db::list_collections(conn)?;
    if !collections.is_empty() {
        result.collections_pushed = collections.len() as u32;
        on_progress("Syncing collections", 0, 0);
        push_json(op, "metadata/collections.json", &collections)?;
    }

    on_progress("Finalizing", 0, 0);

    manifest.last_sync_at = now;
    if manifest.device_id.is_empty() {
        manifest.device_id = uuid::Uuid::new_v4().to_string();
    }
    write_manifest(op, &manifest)?;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_restore_preserves_want_to_read() {
        // Backup side: incremental backup serializes books.json via the
        // positional book SELECT + mapper. Restore side: books.json is read
        // back and each row is re-inserted via `db::insert_book` (the same
        // path `import_library_backup` uses).
        let remote_dir = tempfile::tempdir().unwrap();
        let config = BackupConfig {
            provider_type: ProviderType::Fs,
            values: [(
                "root".to_string(),
                remote_dir.path().to_string_lossy().to_string(),
            )]
            .into(),
        };
        let op = build_operator(&config).unwrap();

        let src_dir = tempfile::tempdir().unwrap();
        let src = crate::db::init_db(src_dir.path().join("src.db").as_path()).unwrap();
        for id in ["b1", "b2"] {
            src.execute(
                "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format, updated_at) VALUES (?1, 'T', 'A', ?2, 1, 100, 'epub', 100)",
                rusqlite::params![id, format!("/nonexistent/{id}.epub")],
            )
            .unwrap();
        }
        crate::db::set_want_to_read(&src, "b1", true).unwrap();

        run_incremental_backup(&op, &src, None).unwrap();
        let books: Vec<crate::models::Book> = pull_json(&op, "metadata/books.json").unwrap();

        let dst_dir = tempfile::tempdir().unwrap();
        let dst = crate::db::init_db(dst_dir.path().join("dst.db").as_path()).unwrap();
        for book in &books {
            crate::db::insert_book(&dst, book).unwrap();
        }

        let b1: i32 = dst
            .query_row("SELECT want_to_read FROM books WHERE id='b1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let b2: i32 = dst
            .query_row("SELECT want_to_read FROM books WHERE id='b2'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(b1, 1, "flagged book restores flagged");
        assert_eq!(b2, 0, "unflagged book restores unflagged");
    }

    #[test]
    fn provider_schemas_returns_all_providers() {
        let schemas = provider_schemas();
        assert_eq!(schemas.len(), 4);
        assert_eq!(schemas[0].provider_type, ProviderType::S3);
        assert_eq!(schemas[1].provider_type, ProviderType::Ftp);
        assert_eq!(schemas[2].provider_type, ProviderType::Sftp);
        assert_eq!(schemas[3].provider_type, ProviderType::Webdav);
    }

    #[test]
    fn s3_schema_has_required_fields() {
        let schemas = provider_schemas();
        let s3 = &schemas[0];
        let keys: Vec<&str> = s3.fields.iter().map(|f| f.key.as_str()).collect();
        assert!(keys.contains(&"bucket"));
        assert!(keys.contains(&"region"));
        assert!(keys.contains(&"access_key_id"));
        assert!(keys.contains(&"secret_access_key"));
    }

    #[test]
    fn backup_config_serde_roundtrip() {
        let config = BackupConfig {
            provider_type: ProviderType::S3,
            values: [("bucket".to_string(), "test".to_string())].into(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: BackupConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.provider_type, ProviderType::S3);
        assert_eq!(back.values.get("bucket").unwrap(), "test");
    }

    #[test]
    fn build_fs_operator_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let config = BackupConfig {
            provider_type: ProviderType::Fs,
            values: [("root".to_string(), dir.path().to_string_lossy().to_string())].into(),
        };
        assert!(build_operator(&config).is_ok());
    }

    #[test]
    fn build_s3_operator_succeeds_with_config() {
        let config = BackupConfig {
            provider_type: ProviderType::S3,
            values: [
                ("bucket".to_string(), "test-bucket".to_string()),
                ("region".to_string(), "us-east-1".to_string()),
                ("access_key_id".to_string(), "AKID".to_string()),
                ("secret_access_key".to_string(), "SECRET".to_string()),
            ]
            .into(),
        };
        assert!(build_operator(&config).is_ok());
    }

    #[test]
    fn manifest_roundtrip_via_fs_operator() {
        let dir = tempfile::tempdir().unwrap();
        let config = BackupConfig {
            provider_type: ProviderType::Fs,
            values: [("root".to_string(), dir.path().to_string_lossy().to_string())].into(),
        };
        let op = build_operator(&config).unwrap();
        let m = read_manifest(&op);
        assert_eq!(m.last_sync_at, 0);
        let m2 = SyncManifest {
            last_sync_at: 12345,
            device_id: "dev1".into(),
        };
        write_manifest(&op, &m2).unwrap();
        let m3 = read_manifest(&op);
        assert_eq!(m3.last_sync_at, 12345);
        assert_eq!(m3.device_id, "dev1");
    }

    #[test]
    fn push_and_pull_json() {
        let dir = tempfile::tempdir().unwrap();
        let config = BackupConfig {
            provider_type: ProviderType::Fs,
            values: [("root".to_string(), dir.path().to_string_lossy().to_string())].into(),
        };
        let op = build_operator(&config).unwrap();
        let data = vec!["hello", "world"];
        push_json(&op, "test.json", &data).unwrap();
        let back: Vec<String> = pull_json(&op, "test.json").unwrap();
        assert_eq!(back, vec!["hello", "world"]);
    }

    #[test]
    fn push_file_if_missing_uploads_once() {
        let dir = tempfile::tempdir().unwrap();
        let config = BackupConfig {
            provider_type: ProviderType::Fs,
            values: [("root".to_string(), dir.path().to_string_lossy().to_string())].into(),
        };
        let op = build_operator(&config).unwrap();
        let local = dir.path().join("local.txt");
        std::fs::write(&local, b"hello").unwrap();
        let local_str = local.to_string_lossy().to_string();
        assert!(push_file_if_missing(&op, "remote.txt", &local_str).unwrap());
        assert!(!push_file_if_missing(&op, "remote.txt", &local_str).unwrap());
    }

    #[test]
    fn incremental_backup_with_fs_operator() {
        let remote_dir = tempfile::tempdir().unwrap();
        let config = BackupConfig {
            provider_type: ProviderType::Fs,
            values: [(
                "root".to_string(),
                remote_dir.path().to_string_lossy().to_string(),
            )]
            .into(),
        };
        let op = build_operator(&config).unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let conn = crate::db::init_db(db_dir.path().join("test.db").as_path()).unwrap();
        conn.execute(
            "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format, updated_at) VALUES ('b1', 'Test Book', 'Author', '/nonexistent.epub', 5, 100, 'epub', 100)",
            [],
        )
        .unwrap();
        let result = run_incremental_backup(&op, &conn, None).unwrap();
        assert_eq!(result.books_pushed, 1);
        let remote_books: Vec<crate::models::Book> = pull_json(&op, "metadata/books.json").unwrap();
        assert_eq!(remote_books.len(), 1);
        assert_eq!(remote_books[0].title, "Test Book");
        let manifest = read_manifest(&op);
        assert!(manifest.last_sync_at > 0);
        let result2 = run_incremental_backup(&op, &conn, None).unwrap();
        assert_eq!(result2.books_pushed, 0);
    }

    /// Regression test for #64 M4: imported books store a storage key in
    /// `file_path` rather than an absolute filesystem path. Incremental
    /// backup must resolve the key through the caller-provided
    /// `library_storage` before reading the file.
    #[test]
    fn incremental_backup_resolves_m4_storage_keys() {
        // Remote backup destination.
        let remote_dir = tempfile::tempdir().unwrap();
        let config = BackupConfig {
            provider_type: ProviderType::Fs,
            values: [(
                "root".to_string(),
                remote_dir.path().to_string_lossy().to_string(),
            )]
            .into(),
        };
        let op = build_operator(&config).unwrap();

        // Local library folder with one book file at `{folder}/b1.epub`.
        let library_dir = tempfile::tempdir().unwrap();
        let library_folder = library_dir.path().to_string_lossy().to_string();
        std::fs::write(library_dir.path().join("b1.epub"), b"epub-bytes").unwrap();
        let library_storage = crate::storage::LocalStorage::new(&library_folder).unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let conn = crate::db::init_db(db_dir.path().join("test.db").as_path()).unwrap();
        conn.execute(
            "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format, file_hash, is_imported, updated_at) VALUES ('b1', 'Test Book', 'Author', 'b1.epub', 5, 100, 'epub', 'deadbeef', 1, 100)",
            [],
        )
        .unwrap();

        let result = run_incremental_backup(
            &op,
            &conn,
            Some(&library_storage as &dyn crate::storage::Storage),
        )
        .unwrap();
        assert_eq!(result.books_pushed, 1);
        assert_eq!(
            result.files_pushed, 1,
            "file should have been uploaded via resolved storage key"
        );

        // Verify the file landed at the expected remote path.
        let remote_file = remote_dir.path().join("files").join("deadbeef.epub");
        assert!(
            remote_file.exists(),
            "remote file should exist at {:?}",
            remote_file
        );
        assert_eq!(std::fs::read(&remote_file).unwrap(), b"epub-bytes");
    }

    #[test]
    fn incremental_backup_passes_legacy_absolute_paths_unchanged() {
        // A pre-M4 imported row still holds an absolute path (its library
        // folder changed so the migration left it alone). Backup must not
        // rewrite it through the library storage — `Path::is_absolute`
        // short-circuits to the local-fs read path.
        let remote_dir = tempfile::tempdir().unwrap();
        let config = BackupConfig {
            provider_type: ProviderType::Fs,
            values: [(
                "root".to_string(),
                remote_dir.path().to_string_lossy().to_string(),
            )]
            .into(),
        };
        let op = build_operator(&config).unwrap();

        let book_dir = tempfile::tempdir().unwrap();
        let book_path = book_dir.path().join("legacy.epub");
        std::fs::write(&book_path, b"legacy-bytes").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let conn = crate::db::init_db(db_dir.path().join("test.db").as_path()).unwrap();
        conn.execute(
            "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format, file_hash, is_imported, updated_at) VALUES ('b1', 'Legacy', 'Author', ?1, 5, 100, 'epub', 'legacyhash', 1, 100)",
            [book_path.to_string_lossy().to_string()],
        )
        .unwrap();

        // No library storage required: absolute `file_path` uses the local-fs branch.
        let result = run_incremental_backup(&op, &conn, None).unwrap();
        assert_eq!(result.files_pushed, 1);
        let remote_file = remote_dir.path().join("files").join("legacyhash.epub");
        assert_eq!(std::fs::read(&remote_file).unwrap(), b"legacy-bytes");
    }

    /// Minimal in-memory `Storage` that records every `get`/`size` call. Used
    /// to prove the backup loop routes key-form reads through the trait
    /// rather than `std::fs`.
    struct MockStorage {
        data: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
        get_calls: std::sync::Mutex<Vec<String>>,
        size_calls: std::sync::Mutex<Vec<String>>,
    }

    impl MockStorage {
        fn new() -> Self {
            Self {
                data: std::sync::Mutex::new(std::collections::HashMap::new()),
                get_calls: std::sync::Mutex::new(Vec::new()),
                size_calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn insert(&self, key: &str, bytes: Vec<u8>) {
            self.data.lock().unwrap().insert(key.to_string(), bytes);
        }

        fn get_calls(&self) -> Vec<String> {
            self.get_calls.lock().unwrap().clone()
        }
    }

    impl crate::storage::Storage for MockStorage {
        fn put(&self, _key: &str, _bytes: &[u8]) -> FolioResult<()> {
            unimplemented!("MockStorage does not support put")
        }
        fn get(&self, key: &str) -> FolioResult<Vec<u8>> {
            self.get_calls.lock().unwrap().push(key.to_string());
            self.data
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .ok_or_else(|| FolioError::not_found(format!("mock missing {key}")))
        }
        fn exists(&self, key: &str) -> FolioResult<bool> {
            Ok(self.data.lock().unwrap().contains_key(key))
        }
        fn delete(&self, _key: &str) -> FolioResult<()> {
            unimplemented!("MockStorage does not support delete")
        }
        fn list(&self, _prefix: &str) -> FolioResult<Vec<String>> {
            unimplemented!("MockStorage does not support list")
        }
        fn size(&self, key: &str) -> FolioResult<u64> {
            self.size_calls.lock().unwrap().push(key.to_string());
            self.data
                .lock()
                .unwrap()
                .get(key)
                .map(|v| v.len() as u64)
                .ok_or_else(|| FolioError::not_found(format!("mock missing {key}")))
        }
        fn local_path(&self, _key: &str) -> FolioResult<std::path::PathBuf> {
            Err(FolioError::invalid(
                "MockStorage is in-memory — no local path",
            ))
        }
    }

    /// Regression: key-form `file_path` reads must go through
    /// `library_storage.get` — never through `std::fs::read` on a resolved
    /// local path. A `MockStorage` with nothing on disk proves it: if the
    /// read were still going through the filesystem, there would be no file
    /// to read and the upload would fail.
    #[test]
    fn incremental_backup_reads_book_bytes_through_storage_trait() {
        let remote_dir = tempfile::tempdir().unwrap();
        let config = BackupConfig {
            provider_type: ProviderType::Fs,
            values: [(
                "root".to_string(),
                remote_dir.path().to_string_lossy().to_string(),
            )]
            .into(),
        };
        let op = build_operator(&config).unwrap();

        let mock = MockStorage::new();
        mock.insert("b1.epub", b"mock-bytes".to_vec());

        let db_dir = tempfile::tempdir().unwrap();
        let conn = crate::db::init_db(db_dir.path().join("test.db").as_path()).unwrap();
        conn.execute(
            "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format, file_hash, is_imported, updated_at) VALUES ('b1', 'Mock Book', 'Author', 'b1.epub', 5, 100, 'epub', 'mockhash', 1, 100)",
            [],
        )
        .unwrap();

        let result =
            run_incremental_backup(&op, &conn, Some(&mock as &dyn crate::storage::Storage))
                .unwrap();
        assert_eq!(result.files_pushed, 1, "file should have been uploaded");
        assert!(
            mock.get_calls().iter().any(|k| k == "b1.epub"),
            "storage.get must have been called for the book key, got: {:?}",
            mock.get_calls()
        );

        // The remote file must contain the mock's bytes, not anything else.
        let remote_file = remote_dir.path().join("files").join("mockhash.epub");
        assert_eq!(std::fs::read(&remote_file).unwrap(), b"mock-bytes");
    }

    /// When a key-form `file_path` is encountered but no library storage
    /// was provided, the backup must skip the file with a warning instead
    /// of erroring out. Metadata sync should still succeed.
    #[test]
    fn incremental_backup_warns_and_skips_when_library_storage_missing() {
        let remote_dir = tempfile::tempdir().unwrap();
        let config = BackupConfig {
            provider_type: ProviderType::Fs,
            values: [(
                "root".to_string(),
                remote_dir.path().to_string_lossy().to_string(),
            )]
            .into(),
        };
        let op = build_operator(&config).unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let conn = crate::db::init_db(db_dir.path().join("test.db").as_path()).unwrap();
        conn.execute(
            "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format, file_hash, is_imported, updated_at) VALUES ('b1', 'Orphan', 'Author', 'b1.epub', 5, 100, 'epub', 'orphanhash', 1, 100)",
            [],
        )
        .unwrap();

        let result = run_incremental_backup(&op, &conn, None).unwrap();
        assert_eq!(result.books_pushed, 1, "metadata still pushes");
        assert_eq!(
            result.files_pushed, 0,
            "no file should have been uploaded without library storage"
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("no library storage")),
            "expected a 'no library storage' warning, got: {:?}",
            result.warnings
        );
    }

    /// Regression: when the remote backup already has a matching-size file
    /// for a key-form book, the backup must skip it *without* reading the
    /// bytes through `Storage::get`. Editing only book metadata (title,
    /// rating) bumps `updated_at` and enters the changed set, so this path
    /// runs often — pulling the full file each time would make backups
    /// slow, memory-heavy, or network-heavy against remote storage.
    #[test]
    fn incremental_backup_skips_storage_get_when_remote_file_matches() {
        let remote_dir = tempfile::tempdir().unwrap();
        let config = BackupConfig {
            provider_type: ProviderType::Fs,
            values: [(
                "root".to_string(),
                remote_dir.path().to_string_lossy().to_string(),
            )]
            .into(),
        };
        let op = build_operator(&config).unwrap();

        // Pre-populate the remote with a same-size file at the expected path.
        let book_bytes = b"remote-already-has-these".to_vec();
        std::fs::create_dir_all(remote_dir.path().join("files")).unwrap();
        std::fs::write(
            remote_dir.path().join("files").join("skiphash.epub"),
            &book_bytes,
        )
        .unwrap();

        let mock = MockStorage::new();
        mock.insert("b1.epub", book_bytes.clone());

        let db_dir = tempfile::tempdir().unwrap();
        let conn = crate::db::init_db(db_dir.path().join("test.db").as_path()).unwrap();
        conn.execute(
            "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format, file_hash, is_imported, updated_at) VALUES ('b1', 'Unchanged', 'Author', 'b1.epub', 5, 100, 'epub', 'skiphash', 1, 100)",
            [],
        )
        .unwrap();

        let result =
            run_incremental_backup(&op, &conn, Some(&mock as &dyn crate::storage::Storage))
                .unwrap();
        assert_eq!(
            result.files_pushed, 0,
            "remote already has matching-size file, upload should be skipped"
        );
        assert!(
            mock.get_calls().is_empty(),
            "storage.get must NOT be called when remote size matches, got: {:?}",
            mock.get_calls()
        );
    }

    /// Tests for connectivity verification
    #[test]
    fn test_connection_ok_with_fs_provider() {
        let dir = tempfile::tempdir().unwrap();
        let mut values = std::collections::HashMap::new();
        values.insert("root".to_string(), dir.path().to_string_lossy().to_string());
        let config = BackupConfig {
            provider_type: ProviderType::Fs,
            values,
        };
        let result = test_connection(&config);
        match result {
            ConnectionTestResult::Ok { latency_ms } => {
                assert!(latency_ms < 5000, "latency should be reasonable");
            }
            other => panic!("Expected Ok, got {:?}", other),
        }
        // Sentinel dir should be cleaned up (probe uses unique filename under it)
        let probe_dir = dir.path().join(".folio-connection-test");
        let has_leftover = probe_dir.exists()
            && std::fs::read_dir(&probe_dir)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false);
        assert!(!has_leftover, "probe file should be cleaned up");
    }

    #[test]
    #[cfg(unix)]
    fn test_connection_network_error_bad_path() {
        let mut values = std::collections::HashMap::new();
        values.insert(
            "root".to_string(),
            "/nonexistent/path/that/does/not/exist".to_string(),
        );
        let config = BackupConfig {
            provider_type: ProviderType::Fs,
            values,
        };
        let result = test_connection(&config);
        match result {
            ConnectionTestResult::NetworkError { .. }
            | ConnectionTestResult::PermissionDenied { .. } => {}
            ConnectionTestResult::Ok { .. } => panic!("Expected error for nonexistent path"),
            other => panic!("Expected NetworkError or PermissionDenied, got {:?}", other),
        }
    }
}
