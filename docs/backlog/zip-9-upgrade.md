# Upgrade the `zip` crate from 2.x to 9.x

**Status:** deferred (2026-10-22). zip 2.4.2's three extraction advisories
(RUSTSEC-2026-0367/0368/0369) are accepted in `.cargo/audit.toml` because
nothing here calls `ZipArchive::extract` or `ZipStreamReader::extract`. This
upgrade is the exit condition for that entry.

## What

Bump `zip = "2"` to `"9"` in the workspace `Cargo.toml` and
`src-tauri/Cargo.toml`. A trial compile on 2026-10-22 gave 8 errors, all from
one API change: `ZipFile::name()` returns `ZipResult<Cow<'_, str>>` instead of
`&str`. The call sites are `cbz::collect_image_entries`,
`epub::validate_archive`, the two case-insensitive lookups in
`read_zip_entry_string` / `read_zip_entry_bytes`, the href resolver in
`epub.rs`, and the `ZipError::InvalidArchive` test in `error.rs`, which now
takes a `Cow`.

## Worth knowing before picking it up

- **Name decoding changed, not just the signature.** 2.x decoded a name as
  UTF-8 (lossily) only when the entry's UTF-8 flag was set, and as CP437
  otherwise. 9.x tries UTF-8 first for every name and falls back to CP437. A
  zip made without the flag but with UTF-8 names (common for archives built
  on macOS) gets different strings from the same file.
- **That can reorder comic pages.** `collect_image_entries` sorts by name and
  the page cache is keyed by page index. If a comic's names sort differently
  under the new decoding, pages cached by the old build and pages extracted by
  the new one disagree about which image is page N. Decide whether to
  invalidate comic page-cache entries on upgrade, or prove the order can't
  change, for example by sorting on `name_raw()` bytes.
- **Carrel Server sees the type.** `epub::validate_archive` takes
  `&mut ZipArchive<File>`, so carrel-core's public API names the zip crate.
  If Carrel Server calls it with its own zip 2 archive, the repin breaks.
  Check the Carrel Server side in its own session, not from this repo.
- Decide per call site what an `Err` from `name()` means (skip the entry, or
  fail the archive). In `validate_archive`, skipping would weaken the
  pre-scan.
