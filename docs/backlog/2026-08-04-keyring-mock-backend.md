# `keyring` resolves to its mock store — nothing reaches the real OS keychain

**Found:** 2026-08-04, while hand-testing authenticated OPDS catalogs.
**Severity:** every keychain-backed feature silently stores nothing.
**Not** caused by the OPDS auth work; that feature is only what made it visible.

## What's wrong

Both manifests declare the dependency with no features:

```toml
keyring = "3"        # Cargo.toml:38 and src-tauri/Cargo.toml:50
```

`keyring` 3.x moved **every** platform backend behind a feature flag —
`apple-native`, `windows-native`, `sync-secret-service` / `linux-native` — and
ships **no default**. With none enabled the crate falls back to its
platform-independent *mock* store (`keyring-3.6.3/src/lib.rs:68`, and
`pub use mock as default` at `:280`).

Confirmed by the resolved dependency list in `Cargo.lock`: `keyring 3.6.3`
pulls only `log` and `zeroize` — no `security-framework`.

## Why it is silent

The mock store accepts writes and returns `Ok(())`, then a *different* `Entry`
reads back an empty store and returns `Ok(None)`. So:

- nothing errors, nothing rolls back, and DB metadata rows persist correctly;
- the read is a legitimate "no secret stored", not a failure, so no code path
  logs a warning.

Verified on this machine: `security find-generic-password` finds **zero** items
for `carrel-opds-auth`, `carrel-web-server`, or `com.mike.carrel.profile-lock`.
Nothing has ever been written to the login keychain.

## Affected

Every consumer of the keychain, not just OPDS:

| Consumer | Service name |
|---|---|
| OPDS catalog credentials | `carrel-opds-auth` (`src-tauri/src/commands.rs`) |
| Web-UI PIN | `carrel-web-server` (`web_server/auth.rs`) |
| Profile soft-lock passwords | `com.mike.carrel.profile-lock` (`carrel-core/src/profile_lock.rs`) |
| Backup SFTP/S3 credentials | `carrel-backup-{provider}-{key}` (`carrel-core/src/backup.rs`) |

Observed end-to-end for OPDS: adding a catalog with a bearer token succeeds and
writes a correct `opds_auth` row, but every subsequent request goes out with no
`Authorization` header, so the server answers 401 and the UI reports the
credential as rejected. A local listener logging request headers showed
`Authorization: None` on every request.

## Fix

Enable the per-platform backends:

```toml
keyring = { version = "3", features = ["apple-native", "windows-native", "sync-secret-service"] }
```

Both manifests must agree — `carrel-core` and `carrel` each depend on it, and
Cargo unifies features across the graph, so a single un-featured declaration
does not disable the others but a missing one is easy to overlook when only one
manifest is edited.

## Before doing it

- **Unverified:** whether Linux and Windows CI build with these features.
  `sync-secret-service` pulls a dbus chain that may need a system package in
  the ubuntu job; check `.github/workflows/ci.yml` before assuming.
- Decide the Linux backend deliberately: `sync-secret-service` (dbus, needs a
  running secret service) vs `linux-native` (kernel keyutils, non-persistent
  across reboots). They have materially different durability.
- **Tests must keep using the injectable store.** The OPDS code takes
  `&dyn CredentialStore` precisely so tests never touch a real keychain; do not
  "improve" coverage by pointing a test at `KeyringCredentialStore`.
- Once real, first launch will prompt for keychain access on macOS, and a
  denied prompt becomes a live code path for the first time. `opds_context_for`
  already degrades correctly there (keeps provenance, drops the credential,
  logs a warning) — the other three consumers' denial paths are unreviewed.
- Existing installs have no stored secrets to migrate — there is nothing in the
  keychain today, so users re-enter credentials once. No migration needed, but
  it is a visible one-time re-prompt for anyone with a profile lock or backup
  configured.

## The lesson worth keeping

The injectable-store seam that kept the test suite fast and hermetic is exactly
what hid this: `MemoryCredentialStore` stood in everywhere, so
`KeyringCredentialStore` was never once executed by a test. A seam that makes
the real implementation unreachable in tests needs at least one deliberate
integration check that the real backend is wired at all — even just asserting
the crate resolves a platform backend.
