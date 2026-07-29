# Writing Carrel Plugins

Carrel plugins are small scripts that react to events in the app — tagging
books on import, exporting highlights, sending a notification when you finish
a book. They run in a sandbox and can only do what you explicitly allow.

> **Trust:** a plugin is local code you install yourself. Only install plugins
> from sources you trust. Carrel's permission model limits what a plugin can
> reach, but it cannot make untrusted code safe.

## Installing a plugin

A plugin is a folder dropped into your profile's plugins directory:

```
{app data}/plugins/{plugin-id}/
├── plugin.toml      # manifest (required)
└── main.rhai        # script (required)
```

Open the folder from **Settings → Plugins → Open plugins folder**, drop the
plugin folder in, then click **Reload plugins**. The plugin appears in the
list, disabled. Enable it (and approve its permissions) from there.

The bundled examples (Auto Tagger, Stats Notifier, Highlight Exporter) can be
installed in one click from the **Example plugins** gallery in the same panel.

## The manifest — `plugin.toml`

```toml
[plugin]
id = "my-plugin"            # must equal the folder name; [a-z0-9-], 3–64 chars
name = "My Plugin"
version = "1.0.0"
description = "What it does."
author = "Your name"
min_app_version = "2.3.0"   # optional; plugin is hidden as invalid on older apps

[events]
subscribe = ["BookImported", "HighlightCreated"]   # at least one

[permissions]
required = ["read:library", "write:tags"]

# Only when "network" is requested:
# [permissions.network]
# hosts = ["example.org"]
```

A manifest that fails validation (bad id, unknown event, unknown permission,
`network` without hosts, etc.) shows up in the list as **Invalid** with the
reason and can never be enabled.

## The script — `main.rhai`

Carrel calls a function named `on_event(event)` once per subscribed event.
Scripts are written in [Rhai](https://rhai.rs).

```rhai
fn on_event(event) {
    if event.type == "BookImported" {
        add_tag(event.book_id, "new");
    }
}
```

`event` is a map with a `type` field (the event name) plus that event's
payload fields.

### Events

| Event | Payload fields |
|-------|----------------|
| `AppStarted` | — |
| `BookImported` | `book_id`, `format` (`epub`/`pdf`/`cbz`/`cbr`/`mobi`), `source` (`Manual`/`FolderScan`/`Download`) |
| `BookOpened` | `book_id` |
| `BookClosed` | `book_id` |
| `BookFinished` | `book_id` |
| `HighlightCreated` | `book_id`, `highlight_id` |
| `HighlightUpdated` | `highlight_id` |
| `HighlightDeleted` | `highlight_id` |
| `BookmarkCreated` | `book_id`, `bookmark_id` |
| `MetadataEnriched` | `book_id`, `provider` |
| `BackupCompleted` | `provider`, `success` |
| `SyncCompleted` | `direction` (`Pull`/`Push`), `success` |

### Permissions and host functions

A permission only exists in your script if you declared it in the manifest
**and** the user approved it. Calling a function for an ungranted permission
is a "function not found" error.

| Permission | Functions | Consent shows as |
|-----------|-----------|------------------|
| `read:library` | `get_book(id)` → book map or `()`; `find_books(query)` → array (max 50) | Read your book metadata |
| `read:highlights` | `get_highlights(book_id)` → array of highlight maps | Read your highlights and notes |
| `write:tags` | `add_tag(book_id, tag)`; `remove_tag(book_id, tag)` | Add and remove book tags |
| `write:files` | `write_file(rel, text)`; `append_file(rel, text)` — relative to a folder you pick at enable time | Write files to a folder you choose |
| `notify` | `notify(title, body)` → desktop notification | Show desktop notifications |
| `network` | `http_get(url)` → response body text — only for hosts in `[permissions.network] hosts` | Contact these websites |
| `import:books` | `import_from_url(url)` → downloads and imports a book (dedup applies) | Download books into your library |
| `write:metadata` | reserved (no host functions yet) | — |

**`network` safety:** `http_get` only reaches hosts you declared in
`[permissions.network] hosts`; the SSRF guard additionally blocks private and
loopback addresses, so a plugin can't reach your LAN or a metadata endpoint
even if it lists one.

**Book map:** `id`, `title`, `author`, `format`, `total_chapters`, `series`,
`volume`, `language`, `rating` (missing optional fields are `()`).

**Highlight map:** `id`, `book_id`, `chapter_index`, `text`, `color`, `note`
(or `()`), `created_at`.

**`write:files` safety:** paths are relative to the folder you chose; absolute
paths and `..` are rejected, and writes are text-only and size-capped. A
plugin can never write outside its export folder.

## Limits

Each `on_event` call is bounded:

- ~1,000,000 operations and a 5-second wall-clock budget — a runaway script is
  aborted (that invocation only).
- Strings are capped at 1 MB; call depth at 64; `eval` is disabled.
- Plugins are dispatched one event at a time; keep `on_event` quick.
- A plugin that errors on 5 consecutive events is automatically disabled. Fix
  it and re-enable from Settings → Plugins.

## Scheduling

There is no recurring scheduler yet. A plugin that subscribes to `AppStarted`
runs once at launch and can also be triggered on demand with the **Run now**
button next to it in Settings → Plugins. (The bundled OPDS Auto-Download
plugin works this way.)

## Example: tag comics on import

```toml
# plugin.toml
[plugin]
id = "comic-tagger"
name = "Comic Tagger"
version = "1.0.0"
description = "Tags CBZ/CBR imports as comics."

[events]
subscribe = ["BookImported"]

[permissions]
required = ["write:tags"]
```

```rhai
// main.rhai
fn on_event(event) {
    if event.format == "cbz" || event.format == "cbr" {
        add_tag(event.book_id, "comic");
    }
}
```

See the bundled examples under the app's `resources/example-plugins/` for
fully-commented, working plugins.
