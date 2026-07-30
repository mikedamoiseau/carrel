---
name: add-tauri-command
description: Use when adding a new Tauri IPC command to Carrel — exposing backend Rust to the React frontend, wiring invoke(), or when a newly written command returns "command not found" / isn't callable from the frontend.
---

# Add a Tauri Command

## Overview

A command is callable from the frontend ONLY after it is registered in the
`generate_handler!` macro. Defining the function is not enough — the
easy-to-forget step is registration in `lib.rs`. Three edits, in order.

## Steps

### 1. Define the handler — `src-tauri/src/commands.rs`

```rust
#[tauri::command]
pub async fn my_command(arg: String, state: State<'_, AppState>) -> CarrelResult<MyReturn> {
    let conn = state.pool.get().map_err(...)?;   // r2d2 pooled connection
    db::do_thing(&conn, &arg)
}
```

- Return `CarrelResult<T>` (alias for `Result<T, CarrelError>`). `CarrelError`
  serializes to the frontend automatically — do NOT hand-map to `String`.
- Take `state: State<'_, AppState>` to reach the DB pool; never open a
  connection directly.
- DB work belongs in `carrel-core/src/db.rs`, not inline in the command.
- Match the surrounding commands: many wrap `state.ipc_metrics.time("name")`
  and/or `#[tracing::instrument(...)]` — copy a nearby sibling.

### 2. Register it — `src-tauri/src/lib.rs`

Add the path inside `tauri::generate_handler![ ... ]` (around line 325):

```rust
.invoke_handler(tauri::generate_handler![
    commands::import_book,
    // ...
    commands::my_command,   // <-- add here
])
```

**Skipping this = the command compiles but the frontend gets "command not
found" at runtime.**

### 3. Call it — frontend

```typescript
import { invoke } from "@tauri-apps/api/core";
const result = await invoke<MyReturn>("my_command", { arg: "value" });
```

Argument keys are camelCase on the JS side; Tauri maps them to the Rust
snake_case params.

## Verify

```bash
cargo test                                              # from src-tauri/
cargo clippy --workspace --all-targets -- -D warnings   # from repo root
npm run type-check
```

## Common Mistakes

| Mistake | Symptom |
|---------|---------|
| Forgot `generate_handler!` registration | "command not found" at runtime, compiles fine |
| Returned `Result<T, String>` | Inconsistent error shape; use `CarrelResult<T>` |
| Opened own DB connection | Bypasses the r2d2 pool; use `State<AppState>` |
| Put CRUD logic in commands.rs | DB logic belongs in `carrel-core/src/db.rs` |
