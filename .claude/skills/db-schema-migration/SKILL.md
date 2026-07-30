---
name: db-schema-migration
description: Use when changing Carrel's SQLite schema — adding a table or column, altering the books/collections/etc. tables, or modifying db.rs::run_schema. Also when an existing install must keep working after the change (no destructive migrations).
---

# Database Schema Migration

## Overview

The schema lives in `carrel-core/src/db.rs::run_schema`, which runs on every app
startup. It must be **idempotent and additive**: existing installs auto-migrate
in place, and `library.db` is never dropped to apply a change. The DB only
auto-recreates when the file is deleted (a manual reset), not on schema change.

## Steps

### 1. Add additive SQL — `carrel-core/src/db.rs::run_schema`

`run_schema` is a single `conn.execute_batch(...)` of `CREATE TABLE IF NOT
EXISTS` statements. To add:

- **New table:** add a `CREATE TABLE IF NOT EXISTS my_table (...)` block.
- **New column on an existing table:** SQLite has no `ADD COLUMN IF NOT EXISTS`.
  Use a guarded `ALTER TABLE`, following the existing helper pattern
  (`migrate_file_path_to_key` is the in-repo example of a conditional
  migration). Check for the column / a `schema_version` marker before altering
  so re-runs are no-ops.

Never write `DROP TABLE`, `DROP COLUMN`, or anything that discards user data on
startup.

### 2. Grep every consumer BEFORE loosening a contract

If you remove a column, make one nullable, or change its meaning, find and
update every reader/writer first:

```bash
grep -rn "column_name" carrel-core/src src-tauri/src src
```

Verify each hit handles the new shape (Rust `models.rs` structs, `db.rs` row
mapping, frontend types). A loosened contract with an unupdated consumer is a
silent runtime break, not a compile error.

### 3. Update the model struct — `carrel-core/src/models.rs`

Add/adjust the field on `Book` / `Bookmark` / `Collection` / etc. and the
`rusqlite` row-mapping in `db.rs` (column index or name must match the new
schema).

## Verify

```bash
cargo test -p carrel-core                                # db.rs has fixture tests (tempfile)
cargo clippy --workspace --all-targets -- -D warnings
```

Then prove the migration on a REAL pre-existing DB, not just a fresh one:

1. Launch the app against an existing `library.db` (see CLAUDE.local.md for the
   path) and confirm startup succeeds + existing books still load.
2. For a fresh-install check, the wipe-from-scratch block in CLAUDE.local.md
   resets state; relaunch rebuilds an empty schema.

## Common Mistakes

| Mistake | Symptom |
|---------|---------|
| `CREATE TABLE` without `IF NOT EXISTS` | Startup error on second launch ("table already exists") |
| `ALTER TABLE ADD COLUMN` unguarded | Errors on re-run; SQLite has no `IF NOT EXISTS` for columns |
| Changed a column without grepping consumers | Silent runtime break in an unupdated reader/writer |
| Only tested a fresh DB | Migration path on existing installs untested — the risky case |
| Destructive migration (DROP/recreate) | User data loss on upgrade |
