# PRD: Switch profile from the web UI (remote profile switching)

- **Status:** Draft
- **Date:** 2026-07-26
- **Area:** Web server (`src-tauri/src/web_server`), profiles (`commands.rs`), web UI (`src/`)

## Problem

Folio's profiles (each with its own database, library folder, settings, and
reading progress) can only be switched from the desktop app. A user reading in
the browser on a phone/tablet who wants to move from, say, `Books` to
`Magazines` must physically go to the computer and switch there. The web UI
should let them switch the active profile remotely.

## Goal

Let an authenticated web client list the available profiles and switch the
active profile, without touching the desktop — subject to the existing profile
security model.

### Non-goals

- **Per-session / per-client profiles.** Out of scope (see Decision 1). The
  server keeps a single active profile shared by desktop + all web/OPDS clients.
- **Unlocking a locked profile over HTTP.** Explicitly excluded (Decision 2) —
  the profile password never crosses the network.
- **Creating, renaming, deleting, or locking profiles remotely.** Switching only.

## Background: how it works today (verified in source)

- The embedded web server runs **in-process** inside the Tauri app.
- `WebState` holds `Arc<Mutex<>>` handles that are **clones of `AppState`'s**:
  `pool` (active DB pool), `active_profile_name`, `unlocked_profiles`,
  `private_mode` (`src-tauri/src/lib.rs` WebState construction;
  `web_server/mod.rs:16-49`). The web server therefore always reads whatever
  profile is currently active.
- `switch_profile` (`commands.rs:4633`) is the desktop Tauri command. It:
  1. Takes the `profile_lifecycle` async lock (serializes against
     `delete_profile`).
  2. Validates the profile exists.
  3. Applies the **soft-lock gate**: `profile_lock::access_allowed(has_lock,
     is_unlocked)` — a locked profile not unlocked this session is refused.
  4. Sets `profile_state.active`, then `mark_unlocked`.
  5. **Swaps the shared handles** the web server reads:
     `*shared_active_pool = new_pool`, `*shared_active_profile_name = name`.
  6. Rebuilds the plugin host for the new profile
     (`plugin_host::rebuild_for_profile`, needs `AppHandle`).
  7. Updates the tray menu (needs `AppHandle`) and logs a `ProfileSwitched`
     activity event.
- Steps that actually change what the web server serves (2-5) need **no**
  `AppHandle`. Only the plugin-host rebuild and tray update (6-7) do.
- The web server currently holds **no `AppHandle`** — this is the one missing
  dependency.
- The web soft-lock gate `profile_lock_gate` (`mod.rs:255`) already blocks all
  requests when the active profile is locked-and-not-unlocked, checking
  `unlocked_profiles` membership only (never the keychain), because the profile
  password is never accepted over HTTP (Decision 5 in the existing design).

## Proposed design

### Backend

1. **Extract a shared switch core.** Factor the body of `switch_profile` into a
   function that takes the pieces both callers can supply — the `AppState`
   handles plus an `AppHandle` — so both the Tauri command and a new HTTP
   handler call the same validated sequence. No behavior change for the desktop
   command.

2. **Give `WebState` an `AppHandle`.** Tauri `AppHandle` is `Clone + Send +
   Sync`; store a clone on `WebState` at construction (`lib.rs`) so the HTTP
   handler can run the plugin-host rebuild and tray update. This is the only new
   dependency the web layer gains.

3. **New endpoints** (mirror existing `api.rs` handler + auth patterns; both sit
   behind PIN auth and the profile-lock gate):

   | Method | Endpoint | Description |
   |--------|----------|-------------|
   | GET | `/api/profiles` | List switchable profiles: `[{ "name": string, "active": bool, "locked": bool, "switchable": bool }]`. `active` = current. `locked` = has a stored lock. `switchable` = `access_allowed(locked, is_unlocked(name))` — i.e. no lock, or already unlocked on the desktop this session. |
   | POST | `/api/profile` | Body `{ "name": string }`. Switch the active profile. |

   `POST /api/profile` responses:
   - `200 OK` — switched; returns `{ "active": name }`.
   - `404 Not Found` — no such profile.
   - `409 Conflict` (or `423 Locked`) — profile is locked and not unlocked this
     session; body explains it must be unlocked once on the desktop. Reuses
     `profile_lock::access_allowed`; the password is never accepted here.
   - `401` — not authenticated (existing auth layer).

4. **Concurrency.** The HTTP handler acquires the same `profile_lifecycle` lock
   as the desktop command, so remote and desktop switches (and `delete_profile`)
   can't interleave.

### Frontend (web UI)

- A profile switcher in the web UI header/menu, mirroring desktop
  `ProfileSwitcher.tsx`. Calls `GET /api/profiles`, shows the active one,
  lists switchable profiles, and disables (with a tooltip) profiles that are
  `locked && !switchable` — "Unlock on the desktop to use over the network."
- On switch: `POST /api/profile`, then refetch the library (the served data set
  has changed). Handle the locked-profile error with the explanatory message.

## Key decisions

1. **Single shared active profile, not per-session.** A remote switch changes
   the active profile for the desktop and every other web/OPDS client at once.
   Acceptable for the primary single-user case; true per-client profiles would
   require the server to stop keying off one shared pool — a much larger change,
   deferred.

2. **Locked profiles are not switchable over HTTP.** The password never crosses
   the network. A locked profile can only be entered remotely if it was already
   unlocked on the desktop this session. `GET /api/profiles` marks these so the
   UI can show them as disabled rather than silently hiding them.

3. **Reuse, don't reimplement, the gate.** Both the list (`switchable`) and the
   switch use `profile_lock::access_allowed`, matching the desktop command and
   the existing `profile_lock_gate` exactly.

## Security considerations

- Both endpoints require PIN auth (existing middleware) and sit behind the
  profile-lock gate.
- `POST /api/profile` is a global state mutation reachable by anyone with the
  PIN on the LAN — the same trust boundary as every other write endpoint
  (progress, highlights, bookmarks). No new privilege.
- No path allows unlocking a locked profile or reading a locked profile's data
  without a desktop-side unlock.
- Consider whether `GET /api/profiles` leaking profile *names* to an
  authenticated LAN client is acceptable (it is, under the current model — the
  client already holds the PIN). Locked profiles' names are shown but their data
  stays dark. Flag for review.

## Open questions

1. Response code for the locked case: `409 Conflict` vs `423 Locked`? Pick one
   and document in `WEB_SERVER_API.md`.
2. Should switching be blocked while a book is open in another web client, or
   just let the library refetch under them? (Leaning: allow, refetch.)
3. Does the OPDS surface need anything, or is this web-UI only? (Leaning: OPDS
   already follows the active profile; no change.)
4. Activity log: a remote switch should still log `ProfileSwitched` — confirm
   the shared core logs it regardless of caller.

## Effort estimate

Small-to-moderate. The pool/name swap already flows through shared handles; the
work is (a) refactor `switch_profile` into a shared core, (b) thread an
`AppHandle` into `WebState`, (c) two small endpoints, (d) a web-UI switcher.
No schema changes, no new sync surface.

## Test plan

- Unit: shared switch core swaps pool + name; rejects unknown profile; rejects
  locked-not-unlocked; allows no-lock and already-unlocked.
- Web integration (extend `web_server` tests + `web_e2e_server.rs`): `GET
  /api/profiles` shape and flags; `POST /api/profile` happy path swaps served
  data; locked path returns the chosen status; unauthenticated returns 401.
- Concurrency: remote switch contends correctly on `profile_lifecycle` with
  `delete_profile`.
- Manual: read on phone, switch `Books` → `Magazines`, confirm library refetches
  and desktop reflects the switch; confirm a locked profile is disabled with the
  explanatory tooltip.
