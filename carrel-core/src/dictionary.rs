//! Offline dictionary — reads a prebuilt WordNet-derived SQLite artifact and
//! resolves word lookups with light morphological normalization. No Tauri
//! dependency.
//!
//! The artifact is a **separate, read-only secondary database** (the first in
//! the codebase) downloaded on demand — the desktop crate installs it via the
//! gzip/download helpers added alongside this module. This file owns the parts
//! that don't touch the network:
//!   - [`ARTIFACT_SCHEMA_VERSION`] — the schema contract the app understands,
//!   - [`inspect`] — a cheap status probe for the settings UI,
//!   - [`open_readonly_pool`] — a small `READ_ONLY` r2d2 pool,
//!   - [`lookup`] + morphy — exact / exception / suffix-rule resolution.
//!
//! The artifact schema (built by `scripts/build_dictionary_db.py`) is:
//! ```sql
//! meta(key TEXT PRIMARY KEY, value TEXT NOT NULL)         -- schema_version, wordnet_version, ...
//! words(id INTEGER PRIMARY KEY, word TEXT UNIQUE)          -- lowercase lemmas
//! senses(id, word_id, pos, sense_num, gloss, examples, synonyms)
//! lemma_exceptions(form, pos, lemma)                       -- morphy exception tables (*.exc)
//! ```
//! `examples` and `synonyms` are `\n`-joined lists (WordNet glosses/examples
//! are single-line, so a newline separator is unambiguous).

use flate2::read::GzDecoder;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use crate::db::DbPool;
use crate::error::{CarrelError, CarrelResult};

/// Schema version this build understands. A mismatch is reported as
/// [`DictionaryState::Corrupt`] so the UI offers a re-download. A future
/// incompatible format ships under a new `dictionary-vN` release tag with new
/// URL/SHA consts — this constant is bumped in lockstep with that layout.
pub const ARTIFACT_SCHEMA_VERSION: i64 = 1;

/// File name of the decompressed artifact inside its directory.
pub const ARTIFACT_FILE_NAME: &str = "dictionary.db";

/// On-disk state of the artifact directory. Serialized lowercase so the
/// frontend receives `"missing" | "ready" | "corrupt"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DictionaryState {
    /// No artifact file present.
    Missing,
    /// Artifact present, opens read-only, schema version matches.
    Ready,
    /// Artifact present but unreadable or schema-mismatched — re-download.
    Corrupt,
}

/// Status of the installed dictionary artifact, surfaced to the settings UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DictionaryStatus {
    pub state: DictionaryState,
    /// WordNet version string from `meta` (only when `Ready`).
    pub wordnet_version: Option<String>,
    /// Size of the decompressed artifact on disk, in bytes.
    pub size_bytes: Option<u64>,
}

/// A single dictionary sense, ready for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DictionarySense {
    /// Part of speech: `n` | `v` | `a` | `r`.
    pub pos: String,
    /// 1-based sense number in WordNet frequency order.
    pub sense_num: i64,
    pub gloss: String,
    pub examples: Vec<String>,
    pub synonyms: Vec<String>,
}

/// Result of a successful lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DictionaryEntry {
    /// The normalized (lowercased) query word.
    pub word: String,
    /// The lemma actually matched. Equals `word` for an exact hit; differs
    /// after morphological normalization (e.g. `"running"` → `"run"`) so the
    /// card can show a "running → run" note.
    pub matched_word: String,
    pub senses: Vec<DictionarySense>,
}

/// Probe the artifact directory. Never errors — any failure to read a present
/// file resolves to [`DictionaryState::Corrupt`].
pub fn inspect(dir: &Path) -> DictionaryStatus {
    let db_path = dir.join(ARTIFACT_FILE_NAME);
    let size_bytes = match std::fs::metadata(&db_path) {
        Ok(m) => m.len(),
        Err(_) => {
            return DictionaryStatus {
                state: DictionaryState::Missing,
                wordnet_version: None,
                size_bytes: None,
            };
        }
    };
    match read_meta(&db_path) {
        Ok(Some(meta)) if meta.schema_version == ARTIFACT_SCHEMA_VERSION => DictionaryStatus {
            state: DictionaryState::Ready,
            wordnet_version: Some(meta.wordnet_version),
            size_bytes: Some(size_bytes),
        },
        _ => DictionaryStatus {
            state: DictionaryState::Corrupt,
            wordnet_version: None,
            size_bytes: Some(size_bytes),
        },
    }
}

/// Open a small read-only connection pool over the installed artifact. Errors
/// with [`CarrelError::NotFound`] when the artifact is missing so callers can
/// route the user to the settings download flow.
pub fn open_readonly_pool(dir: &Path) -> CarrelResult<DbPool> {
    let db_path = dir.join(ARTIFACT_FILE_NAME);
    if !db_path.exists() {
        return Err(CarrelError::not_found("dictionary artifact not installed"));
    }
    // READ_ONLY: never mutate the artifact and never create a fresh empty DB if
    // the file vanished between the check above and open. `max_size(2)` — a
    // secondary pool alongside the main library pool; lookups are quick.
    let manager =
        SqliteConnectionManager::file(&db_path).with_flags(OpenFlags::SQLITE_OPEN_READ_ONLY);
    r2d2::Pool::builder()
        .max_size(2)
        .connection_timeout(Duration::from_secs(5))
        .build(manager)
        .map_err(|e| CarrelError::database(e.to_string()))
}

/// Look a word up. Returns `Ok(None)` for an empty query or a word (and all of
/// its morphological candidates) absent from the artifact.
pub fn lookup(conn: &Connection, word: &str) -> CarrelResult<Option<DictionaryEntry>> {
    let normalized = word.trim().to_lowercase();
    if normalized.is_empty() {
        return Ok(None);
    }
    let Some(matched) = resolve_lemma(conn, &normalized)? else {
        return Ok(None);
    };
    let senses = load_senses(conn, &matched)?;
    if senses.is_empty() {
        return Ok(None);
    }
    Ok(Some(DictionaryEntry {
        word: normalized,
        matched_word: matched,
        senses,
    }))
}

struct ArtifactMeta {
    schema_version: i64,
    wordnet_version: String,
}

fn open_readonly_conn(db_path: &Path) -> CarrelResult<Connection> {
    Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| CarrelError::database(e.to_string()))
}

fn read_meta(db_path: &Path) -> CarrelResult<Option<ArtifactMeta>> {
    let conn = open_readonly_conn(db_path)?;
    let schema_version: Option<i64> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .and_then(|s| s.parse().ok());
    let wordnet_version: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'wordnet_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    match (schema_version, wordnet_version) {
        (Some(schema_version), Some(wordnet_version)) => Ok(Some(ArtifactMeta {
            schema_version,
            wordnet_version,
        })),
        _ => Ok(None),
    }
}

fn word_exists(conn: &Connection, word: &str) -> CarrelResult<bool> {
    let mut stmt = conn.prepare_cached("SELECT 1 FROM words WHERE word = ?1 LIMIT 1")?;
    Ok(stmt.exists([word])?)
}

/// Resolve a normalized query to a lemma that exists in `words`, trying, in
/// order: (1) exact match, (2) `lemma_exceptions` (irregular forms), (3) static
/// suffix-detachment rules. Returns the first candidate present in `words`.
fn resolve_lemma(conn: &Connection, word: &str) -> CarrelResult<Option<String>> {
    if word_exists(conn, word)? {
        return Ok(Some(word.to_string()));
    }

    let mut stmt =
        conn.prepare_cached("SELECT DISTINCT lemma FROM lemma_exceptions WHERE form = ?1")?;
    let exceptions: Vec<String> = stmt
        .query_map([word], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    for lemma in exceptions {
        if word_exists(conn, &lemma)? {
            return Ok(Some(lemma));
        }
    }

    for candidate in morphy_candidates(word) {
        if word_exists(conn, &candidate)? {
            return Ok(Some(candidate));
        }
    }

    Ok(None)
}

fn push_unique(out: &mut Vec<String>, candidate: String) {
    // Reject stems too short to be real lemmas; skip duplicates while keeping
    // first-seen order (order determines which base form wins in `resolve_lemma`).
    if candidate.len() >= 2 && !out.contains(&candidate) {
        out.push(candidate);
    }
}

/// If `stem` ends in a doubled consonant, return it with the final letter
/// dropped (e.g. `"runn"` → `"run"`). Powers `-ing`/`-ed` de-doubling so
/// `"running"` resolves to `"run"`.
fn undouble_final_consonant(stem: &str) -> Option<String> {
    let bytes = stem.as_bytes();
    let n = bytes.len();
    if n >= 2 {
        let last = bytes[n - 1].to_ascii_lowercase();
        let prev = bytes[n - 2].to_ascii_lowercase();
        if last == prev && last.is_ascii_alphabetic() && !b"aeiou".contains(&last) {
            return Some(stem[..n - 1].to_string());
        }
    }
    None
}

/// Static suffix-detachment candidates mirroring WordNet's morphy. Each is only
/// a *possible* base form; `resolve_lemma` keeps a candidate only if it is
/// present in `words`.
fn morphy_candidates(word: &str) -> Vec<String> {
    // (suffix, replacement) — noun, then verb, then adjective rules.
    const RULES: &[(&str, &str)] = &[
        // noun plurals
        ("ses", "s"),
        ("xes", "x"),
        ("zes", "z"),
        ("ches", "ch"),
        ("shes", "sh"),
        ("men", "man"),
        ("ies", "y"),
        // verb inflections
        ("es", "e"),
        ("es", ""),
        ("ed", "e"),
        ("ed", ""),
        ("ing", "e"),
        ("ing", ""),
        // shared trailing -s (noun plural / 3rd-person verb)
        ("s", ""),
        // comparative / superlative adjectives
        ("er", ""),
        ("er", "e"),
        ("est", ""),
        ("est", "e"),
    ];

    let mut out: Vec<String> = Vec::new();
    for (suffix, replacement) in RULES {
        if let Some(stem) = word.strip_suffix(suffix) {
            push_unique(&mut out, format!("{stem}{replacement}"));
        }
    }
    // Consonant de-doubling for -ing / -ed (running → runn → run).
    for suffix in ["ing", "ed"] {
        if let Some(stem) = word.strip_suffix(suffix) {
            if let Some(undoubled) = undouble_final_consonant(stem) {
                push_unique(&mut out, undoubled);
            }
        }
    }
    out
}

fn split_multi(value: Option<String>) -> Vec<String> {
    value
        .map(|v| {
            v.split('\n')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn load_senses(conn: &Connection, lemma: &str) -> CarrelResult<Vec<DictionarySense>> {
    let mut stmt = conn.prepare_cached(
        "SELECT s.pos, s.sense_num, s.gloss, s.examples, s.synonyms
         FROM senses s JOIN words w ON w.id = s.word_id
         WHERE w.word = ?1
         ORDER BY s.pos, s.sense_num
         LIMIT 40",
    )?;
    let senses = stmt
        .query_map([lemma], |r| {
            Ok(DictionarySense {
                pos: r.get(0)?,
                sense_num: r.get(1)?,
                gloss: r.get(2)?,
                examples: split_multi(r.get::<_, Option<String>>(3)?),
                synonyms: split_multi(r.get::<_, Option<String>>(4)?),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(senses)
}

/// Staging file name for the still-compressed download.
const PART_FILE_NAME: &str = "dictionary.db.part";
/// Staging file name for the decompressed-but-unverified artifact.
const TMP_FILE_NAME: &str = "dictionary.db.tmp";
/// Read/copy buffer size for streaming the download and decompression.
const CHUNK_SIZE: usize = 64 * 1024;

/// Install a gzipped artifact read from `src` into `dest_dir`. This is the
/// testable core of the download flow (the network wrapper just supplies a
/// streaming `src`):
///
/// 1. stream compressed bytes to a `.part` file while hashing them,
/// 2. verify the SHA-256 of the compressed bytes against `expected_sha256`,
/// 3. gunzip `.part` → `.tmp`,
/// 4. check the decompressed DB's `meta.schema_version`,
/// 5. atomically rename `.tmp` into place as `dictionary.db`.
///
/// `progress(loaded, total_hint)` is called after each chunk with the running
/// compressed-byte count and the caller's size hint (`0` when unknown). Any
/// failure removes both staging files, leaving the destination untouched (a
/// prior good artifact, if present, is only replaced by the final rename).
pub fn install_from_gz_reader(
    src: &mut dyn Read,
    expected_sha256: &str,
    dest_dir: &Path,
    progress: &mut dyn FnMut(u64, u64),
    total_hint: u64,
) -> CarrelResult<()> {
    std::fs::create_dir_all(dest_dir)?;
    let part_path = dest_dir.join(PART_FILE_NAME);
    let tmp_path = dest_dir.join(TMP_FILE_NAME);
    let final_path = dest_dir.join(ARTIFACT_FILE_NAME);

    // Clear any stragglers from a previously interrupted install.
    let _ = std::fs::remove_file(&part_path);
    let _ = std::fs::remove_file(&tmp_path);

    let result = install_inner(
        src,
        expected_sha256,
        &part_path,
        &tmp_path,
        &final_path,
        progress,
        total_hint,
    );

    // Always clean up staging files, success or failure.
    let _ = std::fs::remove_file(&part_path);
    let _ = std::fs::remove_file(&tmp_path);
    result
}

fn install_inner(
    src: &mut dyn Read,
    expected_sha256: &str,
    part_path: &Path,
    tmp_path: &Path,
    final_path: &Path,
    progress: &mut dyn FnMut(u64, u64),
    total_hint: u64,
) -> CarrelResult<()> {
    // 1. Stream compressed bytes to `.part`, hashing as we go.
    let mut hasher = Sha256::new();
    {
        let mut part = std::fs::File::create(part_path)?;
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut loaded: u64 = 0;
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            part.write_all(&buf[..n])?;
            loaded += n as u64;
            progress(loaded, total_hint);
        }
        part.flush()?;
    }

    // 2. Verify the compressed-bytes checksum.
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(CarrelError::invalid(format!(
            "dictionary checksum mismatch: expected {expected_sha256}, got {actual}"
        )));
    }

    // 3. Decompress `.part` → `.tmp`.
    {
        let part = std::fs::File::open(part_path)?;
        let mut decoder = GzDecoder::new(std::io::BufReader::new(part));
        let mut tmp = std::fs::File::create(tmp_path)?;
        std::io::copy(&mut decoder, &mut tmp)
            .map_err(|e| CarrelError::invalid(format!("dictionary decompression failed: {e}")))?;
        tmp.flush()?;
    }

    // 4. Validate the decompressed artifact's schema version.
    match read_meta(tmp_path)? {
        Some(meta) if meta.schema_version == ARTIFACT_SCHEMA_VERSION => {}
        Some(meta) => {
            return Err(CarrelError::invalid(format!(
                "dictionary schema version {} unsupported (expected {ARTIFACT_SCHEMA_VERSION})",
                meta.schema_version
            )));
        }
        None => return Err(CarrelError::invalid("dictionary artifact is not valid")),
    }

    // 5. Move into place. `fs::rename` fails on Windows if the destination
    // already exists (repair / re-download over a bad artifact), so remove any
    // existing file first. Only the verified `.tmp` reaches this point, so the
    // window between removal and rename never leaves a partial artifact behind.
    match std::fs::remove_file(final_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    std::fs::rename(tmp_path, final_path)?;
    Ok(())
}

/// Download the gzipped artifact from `url` and install it into `dest_dir` via
/// [`install_from_gz_reader`]. Uses a blocking client with a 30s connect
/// timeout but **no** total timeout (slow links must not be killed mid-stream),
/// and re-applies the SSRF guard on every redirect hop so a public URL cannot
/// 302 to a private target (GitHub's 302 to `objects.githubusercontent.com`
/// passes; loopback/private redirects are refused).
pub fn download_and_install(
    url: &str,
    expected_sha256: &str,
    dest_dir: &Path,
    progress: &mut dyn FnMut(u64, u64),
) -> CarrelResult<()> {
    if !crate::opds::is_safe_url_with_trusted(url, &[]) {
        return Err(CarrelError::invalid(
            "URL blocked: only public HTTP/HTTPS URLs are allowed.",
        ));
    }
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        // No total timeout: a large artifact over a slow link must not be
        // aborted mid-download (opds's 120s total would cut it off).
        .redirect(crate::opds::ssrf_redirect_policy())
        .build()
        .map_err(|e| CarrelError::network(format!("HTTP client error: {e}")))?;
    let mut response = client
        .get(url)
        .send()
        .map_err(|e| CarrelError::network(format!("Download failed: {e}")))?;
    if !response.status().is_success() {
        return Err(CarrelError::network(format!("HTTP {}", response.status())));
    }
    let total_hint = response.content_length().unwrap_or(0);
    install_from_gz_reader(
        &mut response,
        expected_sha256,
        dest_dir,
        progress,
        total_hint,
    )
}

/// Remove the installed artifact and any staging stragglers from `dir`. Missing
/// files are not an error (idempotent).
pub fn delete(dir: &Path) -> CarrelResult<()> {
    for name in [ARTIFACT_FILE_NAME, PART_FILE_NAME, TMP_FILE_NAME] {
        match std::fs::remove_file(dir.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(CarrelError::io(format!("failed to remove {name}: {e}"))),
        }
    }
    Ok(())
}

/// Build a tiny synthetic artifact at `dir/dictionary.db` for tests. Contains a
/// handful of words exercising every lookup path: exact match, an irregular
/// exception (`mice` → `mouse`), and suffix rules (`cats` → `cat`,
/// `running` → `run`). Exposed under `test-utils` so the desktop crate's
/// integration tests can build a real artifact without the network.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn write_test_artifact(dir: &Path) -> CarrelResult<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let db_path = dir.join(ARTIFACT_FILE_NAME);
    let conn = Connection::open(&db_path)?;
    conn.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
         CREATE TABLE words (id INTEGER PRIMARY KEY, word TEXT NOT NULL UNIQUE);
         CREATE TABLE senses (
           id INTEGER PRIMARY KEY, word_id INTEGER NOT NULL REFERENCES words(id),
           pos TEXT NOT NULL, sense_num INTEGER NOT NULL,
           gloss TEXT NOT NULL, examples TEXT, synonyms TEXT);
         CREATE INDEX idx_senses_word ON senses(word_id, pos, sense_num);
         CREATE TABLE lemma_exceptions (form TEXT, pos TEXT, lemma TEXT,
           PRIMARY KEY (form, pos, lemma)) WITHOUT ROWID;",
    )?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES
           ('schema_version', ?1), ('wordnet_version', '3.1'),
           ('license', 'WordNet 3.1 License')",
        [ARTIFACT_SCHEMA_VERSION.to_string()],
    )?;
    // (word, pos, sense_num, gloss, examples, synonyms)
    let rows: &[(&str, &str, i64, &str, &str, &str)] = &[
        (
            "cat",
            "n",
            1,
            "feline mammal",
            "the cat sat",
            "feline\nkitty",
        ),
        (
            "run",
            "v",
            1,
            "move fast on foot",
            "he can run",
            "sprint\ndash",
        ),
        ("run", "n", 1, "a score in baseball", "", ""),
        ("mouse", "n", 1, "small rodent", "a quiet mouse", "rodent"),
        ("light", "n", 1, "electromagnetic radiation", "", ""),
        (
            "light",
            "a",
            1,
            "of little weight",
            "a light bag",
            "weightless",
        ),
    ];
    for (word, pos, sense_num, gloss, examples, synonyms) in rows {
        conn.execute("INSERT OR IGNORE INTO words (word) VALUES (?1)", [word])?;
        conn.execute(
            "INSERT INTO senses (word_id, pos, sense_num, gloss, examples, synonyms)
             VALUES ((SELECT id FROM words WHERE word = ?1), ?2, ?3, ?4,
                     NULLIF(?5, ''), NULLIF(?6, ''))",
            rusqlite::params![word, pos, sense_num, gloss, examples, synonyms],
        )?;
    }
    conn.execute(
        "INSERT INTO lemma_exceptions (form, pos, lemma) VALUES ('mice', 'n', 'mouse')",
        [],
    )?;
    Ok(db_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn morphy_generates_expected_candidates() {
        assert!(morphy_candidates("cats").contains(&"cat".to_string()));
        assert!(morphy_candidates("boxes").contains(&"box".to_string()));
        assert!(morphy_candidates("running").contains(&"run".to_string()));
        // -er does not de-double, so "bigger" never yields "big".
        assert!(!morphy_candidates("bigger").contains(&"big".to_string()));
        assert!(morphy_candidates("studies").contains(&"study".to_string()));
    }

    #[test]
    fn inspect_reports_missing_ready_and_corrupt() {
        let dir = tempdir().unwrap();
        assert_eq!(inspect(dir.path()).state, DictionaryState::Missing);

        write_test_artifact(dir.path()).unwrap();
        let status = inspect(dir.path());
        assert_eq!(status.state, DictionaryState::Ready);
        assert_eq!(status.wordnet_version.as_deref(), Some("3.1"));
        assert!(status.size_bytes.unwrap() > 0);

        // Truncate the file to garbage → corrupt.
        std::fs::write(dir.path().join(ARTIFACT_FILE_NAME), b"not a sqlite db").unwrap();
        assert_eq!(inspect(dir.path()).state, DictionaryState::Corrupt);
    }

    #[test]
    fn lookup_resolves_exact_exception_and_suffix_forms() {
        let dir = tempdir().unwrap();
        write_test_artifact(dir.path()).unwrap();
        let pool = open_readonly_pool(dir.path()).unwrap();
        let conn = pool.get().unwrap();

        // Exact.
        let entry = lookup(&conn, "cat").unwrap().unwrap();
        assert_eq!(entry.matched_word, "cat");
        assert_eq!(entry.senses.len(), 1);
        assert_eq!(entry.senses[0].gloss, "feline mammal");
        assert_eq!(entry.senses[0].synonyms, vec!["feline", "kitty"]);

        // Suffix rule: cats → cat.
        assert_eq!(lookup(&conn, "cats").unwrap().unwrap().matched_word, "cat");
        // Suffix rule with de-doubling: running → run.
        let running = lookup(&conn, "running").unwrap().unwrap();
        assert_eq!(running.matched_word, "run");
        // Exception table: mice → mouse.
        assert_eq!(
            lookup(&conn, "mice").unwrap().unwrap().matched_word,
            "mouse"
        );

        // Case + surrounding whitespace normalized.
        assert_eq!(
            lookup(&conn, "  CAT ").unwrap().unwrap().matched_word,
            "cat"
        );
    }

    #[test]
    fn lookup_returns_multiple_pos_ordered() {
        let dir = tempdir().unwrap();
        write_test_artifact(dir.path()).unwrap();
        let pool = open_readonly_pool(dir.path()).unwrap();
        let conn = pool.get().unwrap();

        let run = lookup(&conn, "run").unwrap().unwrap();
        // Two senses: adjective/noun 'n' then verb 'v' (ORDER BY pos).
        assert_eq!(run.senses.len(), 2);
        assert_eq!(run.senses[0].pos, "n");
        assert_eq!(run.senses[1].pos, "v");
    }

    #[test]
    fn lookup_unknown_and_empty_return_none() {
        let dir = tempdir().unwrap();
        write_test_artifact(dir.path()).unwrap();
        let pool = open_readonly_pool(dir.path()).unwrap();
        let conn = pool.get().unwrap();

        assert!(lookup(&conn, "zzzznotaword").unwrap().is_none());
        assert!(lookup(&conn, "   ").unwrap().is_none());
    }

    #[test]
    fn open_readonly_pool_missing_is_not_found() {
        let dir = tempdir().unwrap();
        let err = open_readonly_pool(dir.path()).unwrap_err();
        assert_eq!(err.kind(), "NotFound");
    }

    // ---- install / download / delete ----

    fn gzip_bytes(data: &[u8]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    fn sha256_hex(data: &[u8]) -> String {
        format!("{:x}", Sha256::digest(data))
    }

    /// Raw bytes of a freshly-built valid artifact.
    fn artifact_bytes(dir: &Path) -> Vec<u8> {
        write_test_artifact(dir).unwrap();
        std::fs::read(dir.join(ARTIFACT_FILE_NAME)).unwrap()
    }

    /// Bytes of a minimal but valid SQLite DB carrying a chosen schema_version.
    fn db_with_schema_version(dir: &Path, version: i64) -> Vec<u8> {
        let path = dir.join("custom.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', ?1), ('wordnet_version', '3.1')",
            [version.to_string()],
        )
        .unwrap();
        drop(conn);
        std::fs::read(&path).unwrap()
    }

    #[test]
    fn install_happy_path_produces_ready_artifact() {
        let src = tempdir().unwrap();
        let gz = gzip_bytes(&artifact_bytes(src.path()));
        let sha = sha256_hex(&gz);

        let dest = tempdir().unwrap();
        let mut last = (0u64, 0u64);
        install_from_gz_reader(
            &mut gz.as_slice(),
            &sha,
            dest.path(),
            &mut |loaded, total| last = (loaded, total),
            gz.len() as u64,
        )
        .unwrap();

        assert_eq!(inspect(dest.path()).state, DictionaryState::Ready);
        assert_eq!(last, (gz.len() as u64, gz.len() as u64));
        // The installed artifact is queryable.
        let pool = open_readonly_pool(dest.path()).unwrap();
        assert!(lookup(&pool.get().unwrap(), "cat").unwrap().is_some());
    }

    #[test]
    fn install_overwrites_existing_artifact() {
        // Repair / re-download case: a dictionary.db already exists at the
        // destination. On Windows `fs::rename` fails if the destination exists,
        // so install must remove it first.
        let dest = tempdir().unwrap();
        write_test_artifact(dest.path()).unwrap();
        assert_eq!(inspect(dest.path()).state, DictionaryState::Ready);

        let src = tempdir().unwrap();
        let gz = gzip_bytes(&artifact_bytes(src.path()));
        let sha = sha256_hex(&gz);
        install_from_gz_reader(
            &mut gz.as_slice(),
            &sha,
            dest.path(),
            &mut |_, _| {},
            gz.len() as u64,
        )
        .unwrap();

        assert_eq!(inspect(dest.path()).state, DictionaryState::Ready);
        let pool = open_readonly_pool(dest.path()).unwrap();
        assert!(lookup(&pool.get().unwrap(), "cat").unwrap().is_some());
    }

    #[test]
    fn install_bad_hash_leaves_no_artifact() {
        let src = tempdir().unwrap();
        let gz = gzip_bytes(&artifact_bytes(src.path()));
        let dest = tempdir().unwrap();

        let err = install_from_gz_reader(
            &mut gz.as_slice(),
            &"0".repeat(64),
            dest.path(),
            &mut |_, _| {},
            0,
        )
        .unwrap_err();

        assert_eq!(err.kind(), "InvalidInput");
        assert_eq!(inspect(dest.path()).state, DictionaryState::Missing);
        assert!(!dest.path().join(PART_FILE_NAME).exists());
        assert!(!dest.path().join(TMP_FILE_NAME).exists());
    }

    #[test]
    fn install_truncated_gz_fails_and_cleans_up() {
        let src = tempdir().unwrap();
        let gz = gzip_bytes(&artifact_bytes(src.path()));
        // Hash the truncated bytes so the checksum passes and the failure
        // surfaces at decompression, exercising that branch + its cleanup.
        let truncated = &gz[..gz.len() / 2];
        let sha = sha256_hex(truncated);
        let dest = tempdir().unwrap();

        let err = install_from_gz_reader(&mut &truncated[..], &sha, dest.path(), &mut |_, _| {}, 0)
            .unwrap_err();

        assert_eq!(err.kind(), "InvalidInput");
        assert_eq!(inspect(dest.path()).state, DictionaryState::Missing);
        assert!(!dest.path().join(TMP_FILE_NAME).exists());
    }

    #[test]
    fn install_wrong_schema_version_rejected() {
        let src = tempdir().unwrap();
        let gz = gzip_bytes(&db_with_schema_version(src.path(), 999));
        let sha = sha256_hex(&gz);
        let dest = tempdir().unwrap();

        let err = install_from_gz_reader(&mut gz.as_slice(), &sha, dest.path(), &mut |_, _| {}, 0)
            .unwrap_err();

        assert_eq!(err.kind(), "InvalidInput");
        assert_eq!(inspect(dest.path()).state, DictionaryState::Missing);
    }

    #[test]
    fn install_failure_preserves_existing_artifact() {
        let dest = tempdir().unwrap();
        write_test_artifact(dest.path()).unwrap();
        assert_eq!(inspect(dest.path()).state, DictionaryState::Ready);

        let src = tempdir().unwrap();
        let gz = gzip_bytes(&artifact_bytes(src.path()));
        let err = install_from_gz_reader(
            &mut gz.as_slice(),
            &"0".repeat(64),
            dest.path(),
            &mut |_, _| {},
            0,
        )
        .unwrap_err();

        assert_eq!(err.kind(), "InvalidInput");
        // The prior good artifact is untouched by the failed install.
        assert_eq!(inspect(dest.path()).state, DictionaryState::Ready);
    }

    #[test]
    fn delete_removes_artifact_and_is_idempotent() {
        let dir = tempdir().unwrap();
        write_test_artifact(dir.path()).unwrap();
        // Leave a staging straggler to confirm delete sweeps it too.
        std::fs::write(dir.path().join(PART_FILE_NAME), b"x").unwrap();

        delete(dir.path()).unwrap();
        assert_eq!(inspect(dir.path()).state, DictionaryState::Missing);
        assert!(!dir.path().join(PART_FILE_NAME).exists());

        // Deleting again on an empty dir is not an error.
        delete(dir.path()).unwrap();
    }
}
