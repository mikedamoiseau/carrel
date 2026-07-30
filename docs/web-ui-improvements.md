# Web UI Improvement Report

Audit date: 2026-07-04. Based on code review of `src-tauri/src/web_server/` and live Playwright inspection of the running web UI (`http://localhost:7788`).

This document is written so each numbered item can be delegated to a separate agent as a self-contained work package. Read the **Shared Context** section first — it applies to every item.

---

## Shared Context (read before starting any item)

### Architecture

The web UI is a hand-written mini-SPA, completely independent from the React desktop app. It shares **no code, no components, and no styling** with `src/`.

| File | Size | Role |
|------|------|------|
| `src-tauri/src/web_server/static/index.html` | 14 lines | Bare shell: `<div id="app">` + `<script src="/app.js">` + `<link href="/app.css">` |
| `src-tauri/src/web_server/static/app.js` | ~587 lines | Vanilla JS IIFE. Hash-based router (`route()` fn). Views: `showLogin`, `showLibrary`, `showDetail`, `showReader`, `showStats`, `showCollections`. Template-literal HTML injection with an `esc()` HTML-escaper. Debounced (300ms) search. `fetch` with `credentials: "same-origin"`. |
| `src-tauri/src/web_server/static/app.css` | ~122 lines | Plain CSS. Custom properties in `:root`: `--bg:#111; --fg:#eee; --accent:#6c9; --card-bg:#1a1a2e; --border:#333`. Single `@media (max-width: 600px)` block. Dark-only, no theme toggle. |
| `src-tauri/src/web_server/web_ui.rs` | 63 lines | Serves the static files via `include_str!`/`include_bytes!` (assets are **embedded in the Rust binary**). Routes: `GET /`, `/app.js`, `/app.css`, `/favicon.png`, `/favicon.ico`. |
| `src-tauri/src/web_server/api.rs` | ~997 lines | JSON API consumed by app.js (see endpoint list below). |
| `src-tauri/src/web_server/auth.rs` | ~589 lines | Optional PIN auth. Session cookie `carrel_session` (24h TTL, in-memory) or Basic Auth. Public (unauthenticated) route carve-out: `/api/auth`, `/api/health`, `/`, `/app.js`, `/app.css`, `/favicon.ico`, `/favicon.png`. Per-IP rate limiting. |
| `src-tauri/src/web_server/mod.rs` | ~1010 lines | `WebState`, `build_router`, security-headers middleware, `DEFAULT_PORT = 7788`. ~300 lines of `#[cfg(test)]` integration tests. |
| `src-tauri/src/web_server/opds_feed.rs` | ~945 lines | OPDS 1.2 catalog (separate surface, mostly out of scope here). |

### Existing API endpoints (api.rs `routes()`)

- `GET /api/health`, `POST /api/auth` (PIN login)
- `GET /api/books` (supports `?q=`, `?series=`, `?sort=`), `GET /api/books/{id}`
- `GET /api/books/{id}/cover`, `/chapters`, `/chapters/{index}` (sanitized HTML for EPUB/MOBI), `/images/{chapter}/{filename}`, `/pages/{index}` (rasterized page image for PDF/CBZ/CBR), `/page-count`, `/download`, `/download/{filename}`
- `GET /api/stats`, `/api/series`, `/api/collections`, `/api/collections/{id}/books`
- `GET /api/audit/login-history`, `GET /api/data-export`

### Desktop app design tokens (target look)

Defined in `src/index.css` (Tailwind v4, `@theme` mapping) and managed by `src/context/ThemeContext.tsx` (modes: light/dark/system/sepia/custom).

```css
/* Light */
--paper: #faf8f3;  --surface: #fff;     --ink: #2c2218;   --ink-muted: #8c7b6e;
--warm-border: #e5ddd4;  --warm-subtle: #f0ead8;
--accent: #c2714e;  --accent-hover: #a85f3f;  --accent-light: #f7ede6;

/* Dark (.dark override) */
--paper: #1a1614;  --surface: #231f1b;  --ink: #e8e2d9;  --accent: #d4886a;
```

Fonts: `--font-sans: "DM Sans Variable"`, `--font-serif: "Playfair Display Variable"`, `--font-reading: "Lora Variable"`. The web UI currently uses the system sans stack and mint-green `#6c9` accent — visually unrelated to the app.

### Rules that apply to every item

1. **Static assets are compiled into the binary.** Any change to `static/*` requires a Rust rebuild to show up in the served UI (`cargo build` / restart of `npm run tauri dev`). There is no frontend build step, no bundler, no npm deps for the web UI — keep it that way (vanilla JS/CSS only, no framework).
2. **New static files (e.g. `manifest.json`, `sw.js`, font files) need three registrations:** an `include_str!`/`include_bytes!` route in `web_ui.rs`, **and** an entry in the auth middleware's public-route carve-out in `auth.rs` (otherwise PIN-protected setups will 401 them), and correct `Content-Type`.
3. **Security invariants — do not weaken:** chapter HTML is sanitized server-side (`sanitize_chapter_html` in api.rs) before serving; keep using `esc()` for any user/book data interpolated into template literals in app.js; filename params go through `is_safe_filename` (api.rs); security headers come from `security_headers_middleware` (mod.rs) — check CSP there before adding inline scripts/styles (external files preferred).
4. **New API endpoints** go in `api.rs::routes()`. DB access goes through functions in `carrel-core/src/db.rs` (receive `&Connection`, never manage pool lifecycle). New tables/columns: additive migration in `carrel-core/src/db.rs::run_schema()`.
5. **Testing/verification:** web server has integration tests in `mod.rs` `#[cfg(test)]` — add coverage there for new endpoints. Before pushing: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` (repo root), `cargo test` (in `src-tauri/`), `npm run type-check && npm run test` (root). Visual verification: Playwright is available in the repo's `node_modules` — run scripts with `NODE_PATH=<repo>/node_modules node script.js` against `http://localhost:7788`.
6. **Surgical changes.** Match existing app.js/app.css style (vanilla, small, no abstractions). Don't refactor unrelated code.

### Observed defects (evidence from live inspection)

- Arrow keys do nothing in the reader (verified: pressed `ArrowRight` on page 1/50, page did not change).
- Reader page image is taller than the viewport; Prev/Next buttons are small and sit at the top — you must scroll back up to turn a page.
- Book detail page is nearly empty: cover, title, "Format: PDF", Read + Download buttons. No author, no description, no series info, no progress.
- Series filter bar is a single horizontal scroll strip of pills — already overflowing with ~25 series.
- One book with a missing cover renders as a plain white rectangle (no placeholder treatment).
- Card titles truncate ("Wunderwaffen - T2…" ×20) with no tooltip, indistinguishable in series-heavy libraries.
- Icon-only header buttons (collections folder, stats chart) have no `aria-label`/tooltip.
- Stats page is read-only and says "Start reading on the desktop app to see your progress here" — web reading is never recorded.

---

## Item 1 — Port the desktop design system to the web UI

**Priority: highest. Do this first — items 5, 6, 8, 10 build on the resulting CSS.**

### Goal
Web UI looks like Folio: warm paper/terracotta palette, same type hierarchy, light+dark variants — instead of the current generic `#111`/mint-green hacker theme.

### Scope
- Rewrite `app.css` around the desktop token set (see Shared Context). Define both palettes:
  - `:root` → light tokens, `[data-theme="dark"]` (attribute on `<html>`) → dark tokens.
  - Keep variable *names* semantic and aligned with `src/index.css` (`--paper`, `--surface`, `--ink`, `--ink-muted`, `--warm-border`, `--accent`, `--accent-hover`, `--accent-light`) so future maintenance can diff the two files.
- Typography: use the same families with graceful fallback. Two acceptable approaches — pick one and note it in the PR:
  - (a) Embed WOFF2 subsets of DM Sans + Lora as new static assets (see Shared Context rule 2; ~100–200 KB binary growth), or
  - (b) `font-family: "DM Sans", -apple-system, ... sans-serif` relying on local availability, styling headings with the serif stack `"Playfair Display", Georgia, serif`.
- Restyle all existing views to the new tokens: header, search input, filter pills, book grid/cards, detail page, reader chrome, stats page, login screen. Card hover states, button styles (primary = accent, secondary = surface + border) should visually match the desktop app's `src/components/BookCard.tsx` / buttons.
- Do **not** add the theme toggle here (that's Item 6) — but structure the CSS so Item 6 only has to flip the `data-theme` attribute.

### Files
`src-tauri/src/web_server/static/app.css` (main), small class-name touch-ups in `app.js` templates if needed, `web_ui.rs` + `auth.rs` only if font files are added.

### Acceptance criteria
- Side-by-side screenshot of web UI vs desktop app reads as the same product (palette, type, spacing).
- Both light and dark palettes fully defined in CSS (even though toggle ships in Item 6, default can follow `prefers-color-scheme`).
- No layout regressions at 1440px and 390px widths (Playwright screenshots at both).
- No new JS dependencies, no build step introduced.

---

## Item 2 — Keyboard shortcuts

**Priority: high. Small, independent. Verified gap: arrow keys currently do nothing.**

### Goal
Full keyboard operation of library and reader.

### Scope
Single `keydown` listener on `document` in app.js, dispatching on current route (the hash router already knows the active view). Ignore events when `event.target` is an input/select/textarea. Suggested map:

| Context | Key | Action |
|---------|-----|--------|
| Reader (page mode: PDF/CBZ/CBR) | `←` / `→` | Prev / next page (same handlers as the Prev/Next buttons) |
| Reader (chapter mode: EPUB/MOBI) | `←` / `→` | Prev / next chapter |
| Reader | `Home` / `End` | First / last page or chapter |
| Reader | `f` | Toggle fullscreen (`document.documentElement.requestFullscreen()` / `exitFullscreen`) |
| Reader / Detail | `Esc` or `Backspace` | Back (mirror the `←` header button) |
| Library | `/` | Focus search input (`preventDefault` so `/` isn't typed) |
| Library | `Esc` | Clear search / blur input |
| Anywhere | `?` | Overlay listing the shortcuts (simple dismissible `<div>`) |

Optional stretch: `←`/`→` grid navigation with a visible focus ring in the library (needs cards to be focusable — coordinate with Item 10's focus-state work).

### Files
`src-tauri/src/web_server/static/app.js` (listener + shortcut overlay), `app.css` (overlay styling).

### Acceptance criteria
- Playwright test-style verification: open a PDF book, press `ArrowRight`, page indicator changes from `Page 1 / N` to `Page 2 / N`.
- Typing `/` in the search box does not trigger shortcut handling.
- `?` overlay opens and closes.

---

## Item 3 — Comic/PDF reader ergonomics

**Priority: high. The library is comic-heavy (PDF/CBZ/CBR page-image books), and the current reader is the weakest screen.**

### Current behavior
`showReader` in app.js renders (for page-based formats) an `<img>` from `/api/books/{id}/pages/{index}` with small Prev/Next buttons *above* the image. Image renders at natural width, taller than the viewport → user scrolls down to read, scrolls back up to click Next. Every page turn is a fresh server-side raster + network round-trip with no preloading. No zoom, no fit modes, no touch gestures, no fullscreen.

### Scope
1. **Fit modes:** toolbar toggle with `fit-height` (default: whole page visible, `max-height: calc(100vh - header)`) and `fit-width` modes. Persist choice in `localStorage`.
2. **Click-to-turn:** clicking the left third of the page image = prev, right third = next, middle third = toggle chrome (header/toolbar) visibility for an immersive mode.
3. **Touch gestures:** horizontal swipe (`touchstart`/`touchend`, threshold ~50px) = prev/next on mobile.
4. **Preloading:** after rendering page N, `new Image().src = pageUrl(N+1)` (and N-1) so turns feel instant. Keep it simple — no cache eviction logic needed, the browser HTTP cache handles it. Check whether `/api/books/{id}/pages/{index}` responses carry cache headers; if not, add `Cache-Control: private, max-age=3600` in api.rs (page images are immutable per book file).
5. **Progress indicator:** thin bar or `page/total` readout in the toolbar; a range slider (`<input type="range">`) for jumping to a page is a cheap win.
6. **Move/duplicate Prev/Next controls** to a fixed overlay or bottom bar so they're reachable without scrolling (mostly obsolete once click-to-turn exists, but keep visible buttons for discoverability).
7. Keep the EPUB/MOBI chapter reader working — the reader has two modes; page-image ergonomics apply to the image mode, chapter mode gets fit-width text column + same chrome-toggle.

### Files
`app.js` (`showReader`), `app.css`, possibly `api.rs` (cache headers only).

### Dependencies
Coordinates with Item 2 (shares reader navigation handlers — build handlers once, call from both keys and clicks/swipes).

### Acceptance criteria
- Full page visible without scrolling at 1440×900 in fit-height mode.
- Click right side of image → next page. Swipe works on a 390px-wide touch-emulated Playwright page.
- Page N+1 request observable in network log *before* user turns the page.
- Chrome hides/shows on middle-click; no dead ends (chrome can always be brought back).

---

## Item 4 — Two-way reading progress sync

**Priority: high. Backend + frontend. Turns the web UI from a viewer into a real reading surface.**

### Current behavior
Progress flows one way: desktop app writes to SQLite; web stats page reads it. The web reader always opens at page/chapter 0 and records nothing. Stats page explicitly says "Start reading on the desktop app to see your progress here."

### Scope
1. **API — read:** `GET /api/books/{id}/progress` → current position (existing `ReadingProgress` model in `carrel-core/src/models.rs`; check for an existing read function in `carrel-core/src/db.rs` — the desktop app already loads progress, reuse that function).
2. **API — write:** `PUT /api/books/{id}/progress` with JSON body (position: chapter index + scroll offset for EPUB/MOBI, page index for PDF/CBZ/CBR — mirror whatever shape the desktop app persists; **grep every consumer of the progress table before changing any field semantics**). Auth-protected (default middleware). Reuse the desktop's upsert function in `carrel-core/src/db.rs` if one exists; add one there if not.
3. **Frontend:** on reader open, fetch progress and jump to the saved position (offer "Resume at page 23 / Start over" if saved position > 0). While reading, debounce-save position (e.g. 2s after last page turn, plus `visibilitychange`/`pagehide` flush).
4. **Reading sessions (optional, only if cheap):** if the stats tables track sessions/time, consider a lightweight session record for web reads so the stats page reflects web activity. If the schema makes this non-trivial, skip — position sync is the core deliverable; note the decision in the PR.
5. **Conflict policy:** last-write-wins on `updated_at`. No merge logic.

### Files
`api.rs` (2 routes + handlers), `carrel-core/src/db.rs` (reuse/add progress read+upsert), `app.js` (`showReader` + detail page "Continue" button), `mod.rs` (integration tests for both endpoints, including 401-when-PIN-set and write-then-read roundtrip).

### Acceptance criteria
- Integration tests in `mod.rs` pass: PUT then GET returns the written position; unauthenticated PUT is rejected when a PIN is configured.
- Read 5 pages in the web reader, close tab, reopen book → resumes at page 6 (or offers resume).
- Desktop app shows the updated position for the same book (verify by reading the DB row).
- `cargo test` (src-tauri) and clippy clean.

### Dependency
Item 8's "Continue" button and Item 5's "Continue Reading" shelf both consume `GET .../progress` — build this first.

---

## Item 5 — Home screen: "Continue Reading" + "Recently Added" shelves

**Priority: medium-high. Depends on Item 4 for real value (web-side positions), but can ship against desktop-written progress immediately.**

### Current behavior
Home = flat wall of every cover, sorted by "Recent" dropdown. No resume affordance; finding the book you're mid-way through means remembering its title.

### Scope
1. **API:** either extend `GET /api/books` with `?filter=reading` / `?filter=recent` params, or add `GET /api/books/continue-reading` (books with progress > 0 and < 100%, ordered by `last_read_at` desc, limit ~12). Check what the desktop Library screen uses in `carrel-core/src/db.rs` — a query probably already exists; expose it, don't duplicate it.
2. **Frontend:** `showLibrary` renders (when unfiltered/unsearched):
   - **Continue Reading** — horizontal shelf, cards show a progress bar (percent from progress data) and open the reader directly at the saved position (not the detail page).
   - **Recently Added** — horizontal shelf, last ~12 imports.
   - **All Books** — the existing grid below.
   - When a search query or series/collection filter is active, hide the shelves and show only the filtered grid (current behavior).
3. Progress bar on shelf cards: thin accent-colored bar at the card bottom (matches desktop `BookCard` which shows reading progress).

### Files
`app.js` (`showLibrary`), `app.css` (shelf layout: horizontal scroll with snap points, `scroll-snap-type: x mandatory`), `api.rs` (+ `carrel-core/src/db.rs` if a new query is needed), `mod.rs` tests for any new endpoint.

### Acceptance criteria
- With ≥1 in-progress book: home shows shelves + grid; clicking a Continue card lands in the reader at the saved position.
- With zero progress: Continue shelf hidden entirely (no empty-state noise).
- Searching hides shelves; clearing search restores them.
- Mobile (390px): shelves scroll horizontally with snap; no horizontal body scroll.

---

## Item 6 — Theme toggle: light / dark / system (+ sepia)

**Priority: medium. Small. Hard dependency on Item 1 (needs both palettes defined).**

### Scope
1. Header button cycling light → dark → system (system = follow `prefers-color-scheme` via `matchMedia`, react to changes live). Persist choice in `localStorage` (`carrel_theme`).
2. Apply by setting `data-theme="light|dark"` on `<html>` before first paint — inline the tiny bootstrap script in `index.html` `<head>` (check the CSP in `security_headers_middleware` in mod.rs allows it; if `script-src` lacks `'unsafe-inline'`, either add a hash for this one script or accept a first-paint flash and set it at the top of app.js — prefer the hash approach, do not broadly enable `unsafe-inline`).
3. Optional: sepia as a third palette (desktop has it; tokens in `src/lib/themes` — port values if trivial, otherwise skip and note it).
4. Icon reflects current mode (sun/moon/auto glyph — inline SVG, no icon library).

### Files
`index.html` (bootstrap script), `app.js` (toggle + persistence), `app.css` (nothing new if Item 1 done right), possibly `mod.rs` (CSP hash).

### Acceptance criteria
- Toggle cycles and persists across reloads; no flash-of-wrong-theme on reload (or documented decision to accept it).
- System mode follows OS setting change live (Playwright: `page.emulateMedia({ colorScheme: 'dark' })`).
- All views legible in both palettes (spot-check screenshots: library, detail, reader, stats, login).

---

## Item 7 — Library navigation at scale (filters, series grouping, URL state)

**Priority: medium-high. Largest frontend item. The current filter strip is already unusable at ~25 series.**

### Current behavior
One horizontal `overflow-x: auto` strip of pill buttons: "All Books", then collections, then every series with a count. Filter/sort/search state lives only in JS variables — back button, reload, and bookmarks all lose state.

### Scope
1. **URL as source of truth:** encode state in the hash route, e.g. `#/library?q=asterix&series=Yakari&sort=title`. `route()` parses it; every filter/search/sort change rewrites the hash (`history.replaceState` for keystrokes, hash change for filter clicks so back button steps back sensibly). Reload/back/bookmark all restore the exact view. **Do this part first — it's the foundation.**
2. **Replace the pill strip** with a compact filter bar:
   - "Collections ▾" and "Series ▾" dropdown panels with a type-to-filter input inside (25+ entries must be searchable), entries show counts, active selections render as removable chips next to the search box.
   - Keep "All Books" as a one-click reset.
3. **Series stacks in the grid (optional stretch, propose before building):** when unfiltered, collapse books of the same series into one stacked-cover card ("Wunderwaffen · 21 books") that clicks through to `?series=...`. This fixes the "20 identical truncated covers" wall. If it conflicts with the shelves from Item 5, series stacks win in the All Books grid and shelves stay flat.
4. **Sort dropdown** stays, but selection is reflected in URL (`sort=recent|title|author|last-read|rating`).

### Files
`app.js` (router + `showLibrary` + `renderFilterBar` rewrite), `app.css` (dropdown panels, chips, stack cards). No backend changes — `/api/books?q=&series=&sort=` and `/api/series`, `/api/collections` already exist.

### Acceptance criteria
- Reload mid-search restores query, filter, and sort exactly.
- Back button returns to previous filter state, not out of the app.
- Series dropdown filters as you type; selecting adds a chip; chip × removes it.
- Works at 390px (dropdowns become full-width sheets or remain usable).

---

## Item 8 — Richer book detail page

**Priority: medium. Depends on Item 4 for the progress/Continue elements; everything else is independent.**

### Current behavior
Cover, title, "Format: PDF", Read + Download buttons, in an otherwise empty screen. `GET /api/books/{id}` likely already returns more fields than the page renders (author, series, description, rating, page count, file size, added date — check the `Book` model in `carrel-core/src/models.rs` and the api.rs handler; extend the API response only if fields are missing and the data is in the DB).

### Scope
1. **Metadata block:** author, series name + volume, description (render as text — it may contain publisher HTML; keep escaping with `esc()`, or sanitize server-side if HTML rendering is wanted), format badge, page/chapter count, file size, date added, rating (read-only stars).
2. **Progress:** progress bar + "page 23 of 50 · 46%" if progress exists. Primary button becomes **Continue** (jumps to saved position); secondary "Start over". (Needs Item 4's GET endpoint.)
3. **Series navigation:** if the book belongs to a series, show "Series: Wunderwaffen (12/21)" linking to the filtered library, plus Prev/Next volume buttons resolved by series + volume ordering — `/api/books?series=X` already returns the set; resolve neighbors client-side.
4. **Layout:** two-column on desktop (cover left; meta right), stacked on mobile — the responsive pattern already exists in the 600px media query.

### Files
`app.js` (`showDetail`), `app.css`, possibly `api.rs` (add missing fields to the book-detail response).

### Acceptance criteria
- Detail page for a series book shows author, description, series position, working prev/next volume links.
- In-progress book shows Continue as primary action landing at saved position.
- No raw HTML injection from description (verify with a book whose description contains tags).
- Mobile layout stacks cleanly.

---

## Item 9 — PWA: installable, app-like on phones

**Priority: medium-low. Independent. Cheap because all assets are already static and embedded.**

### Scope
1. **`manifest.json`:** name "Folio", `display: standalone`, theme/background colors from the Item 1 palette, icons (192px + 512px PNG — derive from the existing app icon assets in `src-tauri/icons/`).
2. **Service worker (`sw.js`):** minimal cache-first for the app shell (`/`, `/app.js`, `/app.css`, `/favicon.png`, manifest, icons) with a cache-version bump strategy (bump a `CACHE_VERSION` string whenever static assets change — document this in a comment in web_ui.rs next to the embeds so future asset edits remember to bump it). **Network-only for `/api/*`** — do not cache API responses or book content in v1 (auth and freshness complexity not worth it; note offline book caching as explicit future work).
3. **Registration:** feature-detected `navigator.serviceWorker.register('/sw.js')` in app.js.
4. **New static routes:** `manifest.json`, `sw.js`, both icons — remember all three registrations per Shared Context rule 2 (web_ui.rs route + auth.rs public carve-out + Content-Type; `sw.js` must be served with `Content-Type: application/javascript`).
5. `<link rel="manifest">` + `<meta name="theme-color">` in index.html.

### Caveat to document in the PR
Install prompts and service workers require a secure context: `localhost` works for testing, but LAN-IP access over plain HTTP (the primary real-world use: phone → `http://192.168.x.x:7788`) will **not** allow SW registration or install on most browsers. The manifest + icons still improve add-to-homescreen on iOS Safari. State this limitation honestly; do not add TLS in this item.

### Acceptance criteria
- Lighthouse PWA checks pass on `http://localhost:7788` (manifest valid, SW registered, icons served).
- App-shell loads from SW cache on reload (verify in DevTools/Playwright network log).
- `/api/*` requests always hit the network.
- PIN-protected mode: manifest/sw/icons load without auth (carve-out verified by integration test in mod.rs).

---

## Item 10 — Polish & accessibility pass

**Priority: medium. Best done last — touches every view; earlier items reduce rework.**

### Scope (checklist)

**Covers & images**
- [ ] Cover placeholder: books with missing/failed covers get a styled placeholder (surface bg, book title in serif, subtle border) instead of a white rectangle / gray box. Single shared `onerror` handler + same markup for the no-cover case.
- [ ] `loading="lazy"` + explicit `width`/`height` (or `aspect-ratio` CSS) on all grid cover `<img>`s — kills layout shift and speeds first paint on large libraries.

**Feedback & states**
- [ ] Loading skeletons for library grid, detail, reader page loads (CSS shimmer on placeholder cards; the desktop app has a shimmer keyframe in `src/index.css` to imitate).
- [ ] Fetch error handling: visible toast/banner ("Couldn't reach Folio server — retry") instead of silent empty views. One small `showToast(msg)` helper.
- [ ] Empty states: empty library, empty search results ("No books match 'xyz'"), empty collection — each with a short friendly message.

**Navigation details**
- [ ] Preserve library scroll position when returning from detail/reader (save `scrollY` per hash in `sessionStorage`, restore on back).
- [ ] `document.title` reflects view: "Folio", "Book Title — Folio", "Reading: Book Title".
- [ ] Truncated card titles get `title` attribute tooltips (full name on hover).

**Accessibility**
- [ ] `aria-label` on all icon-only buttons (back arrow, collections folder, stats chart, future theme toggle).
- [ ] Visible `:focus-visible` ring (accent-colored outline) on every interactive element; book cards keyboard-focusable (`tabindex="0"` + Enter/Space activate) — coordinate with Item 2's grid navigation if built.
- [ ] Search input has a `<label>` (visually hidden is fine) or `aria-label`.
- [ ] Reader page images get meaningful `alt` ("Page 3 of 50").
- [ ] Color-contrast check of final Item 1 palette (WCAG AA for text on `--paper`/`--surface`).
- [ ] `prefers-reduced-motion` media query disabling skeleton shimmer and any transitions (desktop app already does this — mirror it).

### Files
`app.js`, `app.css` throughout.

### Acceptance criteria
- Keyboard-only walkthrough: reach and operate search, filters, a book card, detail actions, reader controls — visible focus at every step.
- Playwright screenshot of library with a missing-cover book shows styled placeholder.
- Killing the server mid-session and clicking around produces the error toast, not blank screens.

---

## Item 11 — Serve cover thumbnails in library/shelf grids

**Priority: medium-high (performance). Added 2026-07-04 after the initial audit. Independent of other items.**

### Current behavior
`GET /api/books/{id}/cover` (api.rs `get_cover`, ~line 420) reads `book.cover_path` — the **full-size** `cover.jpg` — and the web UI uses that one endpoint everywhere: library grid, shelf cards, and detail page. With a ~2000-book library the grid downloads thousands of full-resolution covers to render ~160-200px cards.

The infrastructure for the fix already exists and is unused by the web server:
- `carrel-core/src/image_util.rs::make_thumbnail(bytes, target_width)` — produces a q80 JPEG clamped to a target width; returns `Ok(None)` when the source is already at/below the target ("use the original").
- The desktop app already persists `thumb.jpg` next to `cover.jpg` in `{app_data}/covers/{book_id}/` — verified ~2347 `thumb.jpg` files on disk on this machine. Find the desktop write path (grep `make_thumbnail` / `thumb.jpg` consumers in commands.rs / import pipeline) and mirror its exact naming + target width.

### Scope
1. **API:** extend `GET /api/books/{id}/cover` with `?size=thumb` (default `full` keeps current behavior — OPDS and detail page stay untouched). `size=thumb` resolution order:
   - serve `thumb.jpg` from the cover directory if present;
   - else generate it once via `image_util::make_thumbnail` from the full cover (same target width the desktop uses), persist it as `thumb.jpg` (best-effort — serve from memory even if the disk write fails), and serve it;
   - `make_thumbnail` → `Ok(None)` (cover already small) or any generation error → fall back to the full cover bytes.
   Follow the established cache-header scheme: `no-store` when a PIN is configured, `private, max-age=3600` otherwise (thumbnails are immutable per cover).
   Generation is CPU+IO work — run it in `tokio::task::spawn_blocking` and do not hold a DB pool connection across it (same pattern as the `file_size` stat fix in Item 8's review round).
2. **Frontend:** grid cards and shelf cards request `/cover?size=thumb`; detail page keeps the full cover. The existing `onerror` → styled placeholder path must keep working.
3. **No schema change.** No new endpoint — a query param on the existing route keeps the auth/public-carve-out story unchanged.

### TDD
Integration tests in mod.rs first (red→green), with a synthetic large cover fixture:
- `?size=thumb` returns 200 image/jpeg, and the returned image is at most the desktop thumbnail width (decode dimensions in the test with the `image` crate — already a workspace dependency).
- After the first thumb request, `thumb.jpg` exists in the cover dir; a second request serves it (no regeneration — assert e.g. via mtime stability or a cheap marker).
- Small-cover book: `?size=thumb` returns the original bytes unchanged.
- No `size` param → byte-identical to previous behavior.
- Book without a cover → 404 both sizes.
- Cache headers follow the PIN/no-PIN scheme.

### Acceptance criteria
- Library grid network transfer for covers drops by roughly an order of magnitude on a large library (spot-check via Playwright request sizes: grid cover responses each well under ~50 KB vs the full-size baseline).
- Grid/shelf visual quality unchanged at card size (screenshot review light+dark).
- Detail page still shows the full-resolution cover.
- All existing regression scripts stay green.

---

## Item 12 — Animated page turns on swipe (mobile)

**Priority: medium (UX delight). Added 2026-07-05. Depends on Items 2+3 (reader touch handling) being merged.**

### Current behavior
Swiping left/right in the page-image reader (Item 3) turns the page instantly: `img.src` swaps with no motion. The desktop app never hard-cuts — its panels/modals all use `slide-in-left`/`slide-in-right` keyframes (`src/index.css` lines 98-106: `translateX(±100%) + opacity`, `0.22s cubic-bezier(0.22, 1, 0.36, 1)`). The web reader should speak the same motion language.

### Scope
1. **Drag-follow during swipe (touch only):** while the finger is down and moving horizontally, the current page translates with the finger (`transform: translateX(dx)`, no transition, `will-change: transform`). Vertical-dominant gestures (scrolling a fit-width page) must NOT trigger drag-follow — use an axis lock (first ~10px decides). Release past the existing ~50px threshold commits the turn; below it, the page animates back to `translateX(0)` (spring-ish: same 0.22s cubic-bezier).
2. **Commit animation:** on a committed turn — from swipe, click zones, arrow keys, or slider is OPTIONAL (slider jumps stay instant) — the incoming page slides in from the right (next) or left (prev) using the app's exact timing: `0.22s cubic-bezier(0.22, 1, 0.36, 1)`, transform + opacity like the desktop keyframes. Implementation freedom: two stacked `<img>` elements (current + incoming) or a single element re-triggering a CSS animation class; pick the simplest that avoids layout shift and works with fit-height/fit-width.
3. **Preload interaction:** the slide must not present a blank/broken incoming page. If the target page image is already loaded (preload cache from Item 3) animate immediately; otherwise show the existing loading state and skip the animation (hard cut once loaded is fine — never animate an unloaded image in).
4. **Chapter mode (EPUB/MOBI):** apply the same slide-in on chapter transitions (content container, not per-image). Cheap — one animation class on the freshly rendered stage.
5. **Reduced motion:** `prefers-reduced-motion` disables drag-follow translation feedback AND commit animations entirely (instant swap, current behavior).
6. **No regressions:** page-turn latency for keyboard users must not increase (animation runs on the incoming render, never blocks input); rapid turns (key-repeat) must not queue/stack animations — interrupt cleanly (cancel running animation, jump to final state, start the new one).

### Files
`static/app.js` (touch handlers, turn pipeline in the reader), `static/app.css` (keyframes mirroring `src/index.css` values, will-change, reduced-motion block). No Rust changes. `sw.js` CACHE_VERSION hash must be regenerated (CI test enforces).

### Acceptance criteria
- Playwright (touch-emulated 390px): drag 100px left → committed turn, page indicator advances; drag 30px → snap back, same page. Axis lock: vertical drag on a fit-width page scrolls, doesn't turn.
- Committed turn shows a translateX transition on the incoming element (assert computed transform/animation mid-flight, or animationstart event fired).
- Rapid ArrowRight ×5: final page correct, no stuck mid-animation state (assert final transform is identity/none).
- `prefers-reduced-motion`: no animation events fire on turn.
- All existing regression scripts stay green (reader scripts especially: verify-item23*, verify-item910*).

---

## Suggested delegation batches

| Batch | Items | Rationale |
|-------|-------|-----------|
| 1 | **1** (design system) | Foundation for everything visual; do alone to avoid merge conflicts in app.css |
| 2 | **2 + 3** (keyboard + reader ergonomics) | Same file region (`showReader`), shared navigation handlers — one agent |
| 3 | **4** (progress sync) | Backend-flavored, independent of CSS work — can run parallel to batch 2 |
| 4 | **5 + 8** (shelves + detail page) | Both consume Item 4's progress endpoint |
| 5 | **6 + 7** (theme toggle + navigation) | Toggle is trivial after Item 1; navigation rewrite is the big frontend chunk |
| 6 | **9 + 10** (PWA + polish) | Final pass over stabilized UI |
| 7 | **11** (cover thumbnails) | Added later; independent perf item, safe to run after the UI stabilizes |

Each batch: feature branch, run full CI suite locally before push (see Shared Context rule 5), PR to main.

---

# Implementation Decision Log

All 11 items were implemented 2026-07-04/05 on the epic branch `feature/web-ui-revamp` (sub-branches `web-ui/01-design-system`, `web-ui/02-reader-ux`, `web-ui/04-progress-sync`, `web-ui/05-shelves-detail`, `web-ui/06-theme-navigation`, `web-ui/09-pwa-polish`, `web-ui/11-cover-thumbnails`, each merged after a two-reviewer loop: workflow-based `/code-review` at high effort + Codex review, with all confirmed findings fixed before merge). Final merged state passes the full CI suite: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`, 236 src-tauri tests, 503 carrel-core tests, `npm run type-check`, 580 frontend tests. **The branch has not been pushed** — pushing/PR was not part of the brief.

Decisions taken autonomously, for later review:

## Design / UX
1. **Typography without embedded fonts** (Item 1): font stacks reference "DM Sans"/"Playfair Display" with system fallbacks; no WOFF2 files embedded (no binary growth, no new static assets). Reviewers noted most visitors will get the fallback fonts. Revisit if exact font parity matters.
2. **Light-mode `--ink-muted` darkened to `#6e6055`** (diverges from the desktop token `#8c7b6e`) to meet WCAG AA on small text. Other tokens ported verbatim.
3. **Primary buttons use `--accent-hover` as background with a new `--on-accent` text token** (white in light, near-black in dark) — the only scheme that passed ≥4.5:1 in both palettes while keeping the terracotta look.
4. **Sepia theme skipped** (Item 6) — desktop sepia tokens live in `src/lib/themes` and porting them was disproportionate; toggle is light/dark/system only.
5. **Series stacks in the grid skipped** (Item 7 stretch) — conflicts with Item 5's shelves; large blast radius.
6. **"All Books" / go-home clears filters only, preserves search query and sort** — matches old pill behavior; full reset felt like data loss.
7. **`theme-color` meta is static light accent** (`#c2714e`); live theme-matching would require touching the CSP-hashed bootstrap script.

## API / backend
8. **Progress API shape** (Item 4): `chapter_index` doubles as page index for page-based formats (mirrors desktop persistence); web writes real scroll ratio for chapter mode. PUT accepts any non-negative index — the upper-bound check against `books.total_chapters` was removed because the reader navigates by live page-count and stale DB values caused silently-dropped saves; clients clamp on read.
9. **Completion side effects** unified in `commands.rs::apply_reading_progress(Option<&AppHandle>)` — web PUTs run the same completion-detection/activity-log path as desktop saves; the only thing a web-driven completion skips is the desktop-only window toast.
10. **Web reading sessions skipped** (Item 4 optional): `reading_sessions` exists but wiring session lifecycle into the web reader was not "near-free". Stats page still shows desktop-derived stats only for time-based metrics.
11. **Continue Reading predicate** excludes `total_chapters = 0` books, aligned with `get_reading_stats`' finished-predicate, so no book can be simultaneously "finished" in stats and "in progress" on the shelf.
12. **Page-image/page-count cache headers are PIN-aware** (`no-store` with PIN, `private, max-age=3600` without) — protects book content after session expiry on shared browsers, at the cost of losing HTTP-cache reuse for preloaded pages on PIN setups.
13. **Cover cache headers are NOT PIN-aware** (`private, max-age=86400` both sizes): cover artwork is far less sensitive than page content, and `no-store` would force OPDS e-reader clients (Basic Auth per request; this machine runs OPDS+PIN) to re-download megabytes of covers per catalog view.
14. **Thumbnails** (Item 11) reuse the desktop's `THUMB_WIDTH = 320` / `thumb.jpg` convention; on-demand generation validates the write path against the covers root, uses atomic temp+rename writes, mtime freshness + TOCTOU re-check, and falls back to full bytes on any failure. Unknown/malformed `?size=` values are treated as `full` (lenient, last value wins) so OPDS clients/proxies appending params never 400.
15. **`GET /api/books/{id}` gained `file_size`** stat'd from disk at request time via `spawn_blocking` (no schema change); `null` on any error.

## Frontend architecture
16. **URL is the source of truth for library state** (`#/library?q=&series=&collection=&sort=`); bare `#` = canonical unfiltered home. Keystrokes use `history.replaceState`; discrete actions push real hash entries. Params are parsed from `location.href`'s raw fragment, not `location.hash` (Firefox percent-decodes the latter).
17. **`esc()` escapes `"` and `'`** in addition to text-node entities — one escaper for both text and attribute contexts.
18. **`api()` returns `null` only for handled 401s** (login already rendered); network/HTTP failures throw a typed error that every view renders visibly (toast + inline error, differentiated messages for unreachable vs HTTP error vs bad payload).
19. **All localStorage/sessionStorage access goes through `safeStorage*` helpers** — storage-denied browser configs must not crash the app.
20. **Service worker caches the app shell only**; `/api` and `/opds` are never intercepted. `CACHE_VERSION` embeds a content hash of the shell assets, enforced by a CI test that recomputes the hash — shipping changed assets without bumping fails the build. Install/runtime fetches bypass the HTTP cache (`cache: 'reload'`/`no-cache`).
21. **PWA secure-context caveat**: service worker + install require https or localhost; the primary LAN use case (`http://192.168.x.x:7788`) gets manifest/icons benefits (iOS add-to-homescreen) but no offline shell. Documented in sw.js; no TLS added.
22. **CSP**: theme bootstrap inline script is allowed via sha256 hash (no `unsafe-inline`); the hash is verified against the actual `index.html` script text by a CI test.

## Process notes
23. TDD was strict red→green for all Rust endpoints/queries; frontend "tests" for Items 1–13 were Playwright acceptance scripts (9 scripts, ~240 assertions total, in the session scratchpad — session artifacts, not committed). Two agents noted writing script and implementation concurrently rather than strictly test-first; red state was still demonstrated for the load-bearing assertions. **Superseded in the second batch:** a durable Playwright suite now lives in `e2e/`, driven by a seeded harness (`src-tauri/examples/web_e2e_server.rs`) and wired into CI as the "Web UI E2E" job — run with `npm run test:e2e`.
24. Known pre-existing issues surfaced but NOT fixed (out of scope): `cargo clippy --features mobi` fails locally on missing `mobi.h` (environment gap, reproduced on base commit); `/api/books` took ~8s under load with a ~2000-book library (serialization + no pagination) — **later resolved by Item 14** (server-side pagination + infinite scroll); pdfium page rendering is serialized server-side (~3.5s/page under contention).
25. A pre-existing bug was fixed opportunistically in Item 5's round: `showLibrary()` destroyed the collections screen's series-row click-through by unconditionally resetting filter state (broken since the initial web UI commit).

## Item 12 (added 2026-07-05)
26. **Swipe page-turn animation** mirrors the desktop's motion tokens exactly (`0.22s cubic-bezier(0.22, 1, 0.36, 1)`, `slide-in-left/right`). Implemented as two stacked `<img>` elements: an absolute incoming overlay slides in and is promoted to the current image on `animationend`, with an idempotent `setTimeout(280ms)` + `animationcancel` fallback so a backgrounded/throttled tab can never leave a stuck overlay. Turn bookkeeping (index, `recordLocalProgress`, `scheduleProgressSave`, render-generation guards) is unchanged — animation is a pure presentation layer.
27. **Drag-follow** tracks the finger with an axis lock (first ~10px decides horizontal vs vertical); vertical gestures fall through to native scroll (`touchmove` `preventDefault` only when horizontally locked). `touchcancel` + multi-touch (`touches.length !== 1`) abort the gesture cleanly — a stray second-finger lift can no longer commit a bogus turn.
28. **Fit-width scrolled-stage animation**: when the stage is scrolled, `scrollTop` is reset to 0 immediately before the slide so the animation stays on-screen (no-op in fit-height, which never scrolls) — chosen over branching fit-width into a hard-cut.
29. **Preload reshaped** from anonymous `new Image()` calls into an index-keyed cache bounded to center±2 (same requests, now queryable) so a turn only animates when the target image is already loaded; unloaded targets fall back to the prior hard-cut + loading state.
30. **Reduced motion** disables both drag-follow translation and commit animations entirely (instant swap); slider jumps are always instant regardless of motion preference.

## Item 13 (added 2026-07-05)
31. **iOS install** (`Item 13`): added `apple-touch-icon` (reusing `/icon-192.png` — iOS upscales; no dedicated 180px asset, avoids a new static route + carve-out + binary-asset churn), `apple-mobile-web-app-capable`, `apple-mobile-web-app-status-bar-style=default`, plus the standardized `mobile-web-app-capable`. iOS relies on these Apple tags rather than `manifest.json` for the home-screen icon and standalone launch. Every `index.html` shell-asset edit forces a `CACHE_VERSION` content-hash bump (CI-enforced); the CSP-pinned inline theme script was left byte-identical. **Known accepted tradeoff:** `status-bar-style=default` gives a light status bar even in dark mode (consistent with the already-static light `theme-color`); `black-translucent` + a painted safe-area would fix it but wasn't worth the complexity. Add-to-Home-Screen works over the plain-HTTP LAN URL; service-worker offline still does not (secure-context requirement, unchanged from Item 9).

## Item 14 (added 2026-07-05)
32. **Grid pagination = server-side slice + infinite scroll.** `/api/books` gained optional `limit`/`offset` applied *after* the existing in-memory filter+sort pipeline (semantics unchanged); absent `limit` returns the full list so OPDS/desktop are untouched. Total is returned via an `X-Total-Count` **header**, keeping the body a bare array (no `{items,total}` wrapper that would break existing callers). Page size 60. Collections (`/api/collections/{id}/books`) stay unpaginated (small sets; flow through the same render path but never spawn a sentinel).
33. **Shelves stopped fetching the whole library.** "Recently Added" was a client-side re-sort of the full array; it now uses `books.slice(0,12)` on the default sort and a bounded `/api/books?sort=date_added&limit=12` fetch only on other sorts. Continue Reading keeps its own endpoint. Shelf fetches are `Promise.allSettled`-style independent — one shelf failing never suppresses the other or the grid (review fix).
34. **Scroll-restore replays pages in one request.** Returning from a book replays the previously-loaded page count via a single `limit = pages*60` fetch (not N serial round trips); legacy plain-number saved offsets (pre-Item-14) use a height-doubling loop so a deep offset actually reaches its position instead of clamping to page 1 (review fixes).
35. **Stable sort tiebreaker.** Offset pagination needs a total order or ties (second-granularity `added_at`, concurrent imports) cause dup/skip across page boundaries. Added `id` as the final tiebreaker in `list_books_grid`'s `ORDER BY` and in every in-memory `sort_by` (`.then_with(|| a.id.cmp(&b.id))`) — mirrors the existing `resolveSeriesNav` precedent.
36. **Generation-scoped in-flight guard.** `libraryLoadingPage` is only cleared in `loadNextPage`'s `finally` when the captured `gen` is still current, so an abandoned old-generation fetch can't clear the live generation's guard and let a duplicate page append (review fix — Codex).

## Item 15 (added 2026-07-05)
37. **Bulk progress endpoint, not a model change.** `GET /api/reading-progress` is a thin wrapper over the pre-existing `db::get_all_reading_progress` (already used by the `last_read` sort — returns full `ReadingProgress` rows, so no new query was needed). `BookGridItem` was deliberately NOT widened (shared `carrel-core` model; would ripple to the desktop `get_library` path). Correctly PIN-gated (not in the public carve-out). Frontend fetches once, merges client-side.
38. **One canonical progress resolver for grid + shelf.** `effectiveProgress()` mirrors `mergeProgress`'s rule — the F8 local (this-tab) value wins only when its `.ts` is newer than the server row's `last_read_at` — and treats `chapter_index <= 0` as "no progress" (matches `showDetail`'s `hasProgress` and `get_continue_reading_books`'s `chapter_index > 0` filter). Both the grid card and the Continue Reading shelf card render from this one source, so the same book can never show two different percentages on one screen. A "Start Over"-reset book (local chapter 0) resolves to **no bar** rather than a misleading `1/total` bar (review fixes).
39. **Badge reuses the shelf bar; guarded for `total_chapters=0`.** Extracted `progressBarHtml(chapterIndex, totalChapters)` — single source for the `.shelf-progress` markup, returns `""` when `total_chapters <= 0` (matches the continue-reading exclusion; no more empty 0%-width track). No new CSS.
40. **Best-effort, resilient, fetched once per entry.** `refreshProgressByBook` never rejects (guards that the body is an array, wraps the merge) so a garbage/`null` progress response can never leave the grid stuck on skeletons; a failed fetch retains the last-known map instead of wiping all badges; and progress is fetched once per fresh library entry (coming from another view) and reused across in-library sort/search/filter re-renders rather than re-fetching the whole table on every keystroke (review fixes).

---

## Second batch (added 2026-07-05)

Four candidates were proposed; **two were already shipped** by earlier items and are NOT re-implemented:

- ~~Library search / filter / sort~~ — already done in **Item 7** (server-side `/api/books?q=&series=&sort=`, URL-state hash routing, 300ms-debounced search, filter dropdowns + chips).
- ~~Loading skeletons~~ — already done in **Item 10** (`skeletonGridHtml`/`detailSkeletonHtml`/`readerSkeletonHtml`, shimmer keyframe, `prefers-reduced-motion` off-switch).

The two genuinely-missing items follow.

---

## Item 14 — Paginate the library grid (infinite scroll)

**Priority: high. Medium size. Backend + frontend. Independent of Item 15 but they share the grid render path — coordinate if built concurrently.**

### Current behavior (the bug)

`list_books` (`api.rs:300`) calls `db::list_books_grid(&conn)` which loads **every** book row, then filters (`series`, `q`) and sorts **in memory**, and returns the entire `Vec<BookGridItem>` as a bare JSON array. The frontend `loadBooks` (`app.js`) fetches the whole list in one request and renders every card. On a ~2000-book library this is the documented ~8s stall (Process note 24). The home/shelves view is **also** affected: even when it renders shelves, `loadBooks` still fetches the full list (it derives the "Recently Added" shelf client-side by re-sorting the full `books` array — see Finding H comment).

### Decisions (locked)

- **UX pattern: infinite scroll.** Auto-load the next page when the user scrolls near the bottom, via an `IntersectionObserver` sentinel appended after the grid. No "load more" button, no numbered pager. (User choice.)
- **Backend contract stays backward-compatible.** `/api/books` gains **optional** `limit` + `offset` query params. When `limit` is **absent**, behavior is unchanged (returns everything) so OPDS / desktop / any other consumer is untouched. Pagination is applied **after** the existing in-memory filter+sort pipeline so filtering/sorting semantics are byte-identical to today — only a slice is returned.
- **Total count via response header, not a body-shape change.** Return the post-filter total in an `X-Total-Count` response header and keep the body a bare `Vec<BookGridItem>`. This avoids a `{items, total}` wrapper that would break the existing array-shaped contract and every current caller. Frontend reads the header to know when to stop.
- **Page size: 60.** One decision point; revisit only if profiling says otherwise. Frontend always sends `limit=60`.
- **Collections endpoint is out of scope.** `/api/collections/{id}/books` stays unpaginated — collections are small and the 8s case is the all-books grid. Document this.
- **Shelves view keeps its own data sources.** On the unfiltered home view, do **not** fetch the full list to build shelves. "Continue Reading" already has its dedicated `/api/books/continue-reading?limit=12` endpoint. "Recently Added" must switch to a bounded fetch (`/api/books?sort=date_added&limit=12&offset=0`) instead of client-side re-sorting the full array. The paginated infinite-scroll "All Books" grid renders below the shelves and drives further page loads.

### Scope

1. **Backend (`api.rs` `list_books` + `ListBooksParams`):** add `limit: Option<usize>`, `offset: Option<usize>`. After the sort step, if `limit` is `Some`, compute `total = books.len()`, slice `books[offset.min(len)..(offset+limit).min(len)]`, and attach `X-Total-Count: {total}` to the response. Use `axum`'s response-header mechanism (return `impl IntoResponse` / `([(header, value)], Json(slice))`). When `limit` is `None`, return the full `Json(books)` exactly as today (no header required, but harmless to include). Guard against `offset` past the end (empty slice, not a panic).
2. **Frontend (`app.js`):**
   - Add module-scoped pagination state: `libraryPageOffset`, `libraryTotal`, `libraryLoadingPage` (re-entrancy guard), and the current fetch's `libraryRenderGen` already exists — reuse it to abandon stale page loads.
   - `loadBooks` fetches page 0 (`limit=60&offset=0`), reads `X-Total-Count`, renders the first page, and if more remain appends an IntersectionObserver **sentinel** element after the grid.
   - A `loadNextPage()` appends the next 60 cards (reuse `bookCardHtml` + `bindGridCardHandlers` on only the new nodes — do **not** re-bind the whole grid), advances `libraryPageOffset`, and removes the sentinel when `offset >= total`.
   - Any filter/sort/search change (every `showLibrary` re-entry) resets pagination to page 0 (offset 0, disconnect the old observer).
   - The client-side search-within-collection path and the series/collection filtered paths funnel through the same paginated grid.
3. **Scroll restoration interaction (Item 10):** returning from a book detail/reader currently restores the saved scroll offset. With infinite scroll, a deep offset requires the intervening pages to exist. Restore by **replaying pages**: persist the number of pages that were loaded alongside the scroll offset (extend `libraryScrollKey` payload), and on return re-fetch that many pages *before* restoring `scrollTop`. Bounded and correct. If replay fails (fewer results now), clamp to the max available scroll.

### TDD (backend, red→green in `mod.rs` integration tests)

- `list_books` with `limit=2&offset=0` returns 2 items **and** an `X-Total-Count` header equal to the full filtered count.
- `offset` past the end returns an empty array + correct total (no 500).
- `limit` absent returns the full list and is byte-identical to the pre-change response (guard the backward-compat contract).
- `limit` composes with `q=`/`series=`/`sort=` — the total reflects the **filtered** set, not the whole table, and the slice is taken after sort (page 0 of `sort=title` starts at "A…").

### Acceptance criteria

- Home and filtered grids issue a first request capped at 60 items; scrolling loads more seamlessly; the ~8s first-paint stall on a large library is gone.
- No duplicate/skipped cards across page boundaries; rapid filter changes never interleave a stale page into a newer grid (render-gen guard).
- Back-from-book restores scroll position on a multi-page grid.
- OPDS and any non-paginating caller of `/api/books` are unaffected (no `limit` sent → full list).

---

## Item 15 — Reading-progress badge on library grid cards

**Priority: medium. Small–medium. Backend (one bulk endpoint) + frontend. Shares the grid card + Item 14's page-append path.**

### Current behavior

`bookCardHtml` (`app.js:1036`) renders cover + title + author + format only. **Shelf** cards already show a progress bar — `shelfCardHtml` emits `<div class="shelf-progress"><div class="shelf-progress-fill" style="width:${progressPercent(chapter_index, total_chapters)}%">` (CSS at `app.css:150`), fed by the `continue-reading` payload which carries `chapter_index`. Grid cards have no equivalent because `BookGridItem` carries no progress.

### Decisions (locked)

- **Bulk progress endpoint, not a model change.** Add `GET /api/reading-progress` returning all progress rows the web session can see (`Vec<ReadingProgress>` — `book_id`, `chapter_index`, `scroll_position`, `last_read_at`). Do **not** add a progress field to `BookGridItem`: it is a shared `carrel-core` model consumed by the desktop `get_library` path, and widening it ripples outside the web surface (per the "grep consumers before schema changes" rule). The web frontend fetches progress once and merges it onto cards client-side. `db::get_all_reading_progress` already exists for the `last_read` sort but returns only `book_id → last_read_at`; either add a `db::list_all_reading_progress() -> Vec<ReadingProgress>` or reuse an existing full-row query — grep `db.rs` first.
- **Reuse the shelf visual + helper verbatim.** Grid badge = the same thin `--accent` fill bar (`shelf-progress`/`shelf-progress-fill` style) and the same `progressPercent(chapter_index, total_chapters)` helper. Consistency with shelves, zero new visual language.
- **Percent semantics mirror the shelf/desktop convention.** `progressPercent` already clamps; `chapter_index` doubles as page index for page-based formats (see Decision-log 8). No new math.
- **No badge when there's no progress.** Books with no progress row render exactly as today (no bar). A "finished" book (100%) shows a full bar — no separate checkmark/state in v1 (log it as a deliberate omission).
- **Bulk fetch is best-effort.** A failed `/api/reading-progress` fetch must never block or error the grid — cards just render without bars (same resilience pattern as the shelves' best-effort fetch, Finding F).

### Scope

1. **Backend:** `GET /api/reading-progress` handler in `api.rs`, registered in `routes()`, PIN-gated like the other `/api/books*` reads (it is not a public shell asset). Returns `Json(Vec<ReadingProgress>)`. Add the `db.rs` query if a full-row variant doesn't already exist.
2. **Frontend (`app.js`):**
   - Fetch `/api/reading-progress` once per library entry (best-effort, cached in a module-scoped `Map<book_id, chapter_index>` — call it `progressByBook`). Reuse it across pages.
   - Extend `bookCardHtml` to emit the `shelf-progress` bar when `progressByBook` has an entry for the book (needs `total_chapters`, already on `BookGridItem`). Because the card HTML is a pure template, the map must be populated **before** the grid renders — fetch progress in `loadBooks` alongside the first page (can run in parallel with the page-0 fetch; render once both resolve, or render grid immediately and paint bars when progress arrives — pick the simpler that doesn't cause layout shift; prefer awaiting both before first paint since skeletons already cover the wait).
   - Item 14 interaction: appended pages (`loadNextPage`) must also read `progressByBook` so later cards get bars too. Since the map is module-scoped and populated once, appended `bookCardHtml` calls just work.
3. **Optimistic sync:** the reader already tracks per-book local progress (`F8` note — "last progress this tab knows about per book"). On returning to the library, prefer the locally-known progress over the fetched map for that book so a just-read book shows fresh progress without a round-trip. Merge local into `progressByBook` before render.

### TDD

- **Backend (red→green, `mod.rs`):** `GET /api/reading-progress` returns rows for books that have progress and is empty for a fresh DB; is PIN-gated (401 without session when auth is on); shape matches `ReadingProgress`.
- **Frontend (Playwright acceptance, scratchpad — not committed):** a book with saved progress shows a `.shelf-progress` bar on its grid card with width matching `chapter_index/total_chapters`; a book with none shows no bar; a book read this session reflects fresh progress on return without reload.

### Acceptance criteria

- Grid cards for in-progress books show the same accent progress bar as shelf cards; untouched books show none.
- Works on infinitely-scrolled pages (Item 14), not just the first page.
- Progress fetch failure degrades to bar-less cards, never a broken grid.

---

## Suggested delegation batches (second batch)

| Order | Item(s) | Rationale |
|-------|---------|-----------|
| 1 | **14** (pagination) | Structural; reshapes `loadBooks`/grid append path that Item 15 decorates. Land first. |
| 2 | **15** (progress badges) | Builds on 14's page-append path; small once 14's grid plumbing exists. |
