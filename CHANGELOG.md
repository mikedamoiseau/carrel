# Changelog

All notable changes to this project will be documented in this file.
This project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Fixed
- **Pinch-to-zoom in the web reader no longer zooms the whole page.** A
  two-finger pinch is meant to zoom the book page (in image/page mode), but
  the browser's own full-page magnification sometimes fired alongside it,
  leaving the interface zoomed and off-centre. The web UI now suppresses the
  browser's native page zoom, so a pinch only ever zooms the book.

## [3.0.4] - 2026-08-06

### Fixed
- **A broken table-of-contents entry can no longer mislabel the first
  chapter.** When an e-book's table of contents pointed at a file that
  isn't part of the book's reading order (a malformed or hand-edited EPUB),
  that entry was silently attached to the first chapter, overwriting its
  title. Such entries are now skipped, so the first chapter keeps its real
  name and only genuine contents entries are shown.
- **Search now works when you browse Carrel's own catalog from another
  e-reader app.** Carrel's OPDS catalog did advertise a search facility, but in
  a shorthand form that only Carrel itself understood. Other reader apps either
  saw no search at all, or tried to search for the literal text
  `{searchTerms}`. The catalog now also publishes its search the standard way
  (an OpenSearch description at `/opds/opensearch.xml`), alongside the existing
  form — so third-party readers get a working search box, and nothing that
  worked before changes.

## [3.0.3] - 2026-08-04

### Added
- **Catalogs that require signing in.** Adding an OPDS catalog now has an
  optional sign-in section for servers that don't allow anonymous access —
  either a username and password, or a single API key sent as a Bearer token.
  Carrel Server's own `/opds` feed offers both: your account email and
  sign-in password, or a `ck_live_…` key you can generate for it. The
  credential itself is stored in your OS keychain, never in Carrel's own
  database, and is only ever sent to the catalog it was configured for — a
  feed that links elsewhere doesn't get it. Sending a credential over an
  unencrypted `http://` connection (other than to your own machine) now asks
  you to confirm first, since that traffic isn't encrypted.

### Fixed
- **Saved passwords, PINs and keys are now actually stored in your system
  keychain.** They never were. The library Carrel uses to talk to the keychain
  stopped enabling any platform support by default in its version 3, and Carrel
  had not asked for it — so it quietly fell back to a stand-in that accepts a
  password, reports success, and keeps nothing. Because storing appeared to
  work, nothing ever reported an error; the value simply wasn't there the next
  time it was needed. This affected every password Carrel keeps: catalog
  sign-ins, the web-interface PIN, profile locks, and backup credentials.
  If you had set any of these, you will be asked for them once more, and macOS
  will ask your permission the first time Carrel reaches the keychain.
- **A catalog that needs a password no longer looks unreachable.** Adding or
  browsing a catalog that answered with "please sign in" (HTTP 401/403) used
  to be reported the same way as a catalog that couldn't be reached at all —
  "Couldn't reach this feed — Could not connect to the server." Carrel now
  recognizes this case for what it is and offers a sign-in panel on the spot,
  instead of leaving you to guess why a perfectly reachable server "couldn't
  connect."
- **Search on local-network and same-machine catalogs.** Some catalogs publish
  their search feature as a separate "OpenSearch" description rather than a
  direct link. For any catalog on your LAN or on the same machine, Carrel was
  discarding that description outright, so the search box would simply do
  nothing. It's now resolved the same way as any other catalog.

### Security
- **A PDF can no longer make Carrel allocate unbounded memory when rendering a
  page.** A PDF page declares its own size, and Carrel rendered every page at a
  fixed target *width*, leaving the height to follow the page's proportions. A
  page declared absurdly tall and narrow therefore turned into an absurdly tall
  image: a 421-byte file was enough to make a single page render as a
  1.15 GB bitmap — and because Carrel renders one page at a time behind a
  single lock, that stalled every other page in the app while it happened. Such
  a page then failed to save anyway, since images that tall cannot be written as
  JPEG, so the memory was spent for nothing. Page renders are now capped at a
  total pixel count and a maximum height and width: an unusually long page is
  rendered smaller instead of refused, so it still opens, and a page whose
  proportions no size could ever accommodate is now rejected immediately rather
  than after the work. Ordinary pages, at every zoom level Carrel offers, are
  unaffected.

## [3.0.2] - 2026-08-01

### Security
- **A malicious e-book can no longer stall the app while its details are being
  read.** The XML parser Carrel uses (quick-xml) had a flaw where checking a
  single tag for repeated attributes took time proportional to the *square* of
  how many attributes that tag had. Because an EPUB's structure files come out
  of the book file itself, a book crafted with a tag carrying tens of thousands
  of attributes could pin a CPU core for minutes to hours — long enough to make
  the app appear frozen, and not something an I/O timeout can interrupt. The
  parser has been updated to a version that does this check in linear time.

  Scope, stated plainly so it is not mis-remembered later: `cargo audit`
  reported **two** advisories against the old version, and only **one of them
  was ever reachable from this project**.

  - **RUSTSEC-2026-0194** (quadratic attribute checking) *was* reachable. Every
    place Carrel reads XML attributes — the EPUB container, the package file,
    both flavours of table of contents, and OPDS catalogue feeds — went through
    the affected code path with the check enabled by default, on input that
    ultimately comes from an untrusted file or a remote server.
  - **RUSTSEC-2026-0195** (unbounded namespace allocation) was **not** reachable.
    It only affects quick-xml's namespace-resolving reader, which this project
    has never used. Upgrading did not close an exploitable namespace bug here,
    because there was not one to close.

  Two things did *not* change, both deliberately. Duplicate-attribute checking
  is still enabled: turning it off would also have avoided the flaw, but it
  changes which malformed documents are accepted, and the upgrade removes the
  cost anyway. And no new attribute-count limit was added: with the check now
  linear, the existing 16 MB cap on how much of a book's structure files Carrel
  will read is already an effective bound.

### Fixed
- **Ampersands and accents in chapter titles and catalogue entries.** Escaped
  characters such as `&amp;` or `&#233;` are now decoded correctly everywhere
  they appear in a table of contents or an OPDS feed — previously a title like
  *Résumé* or *Law & Order* could come back with the character dropped or an
  unwanted space in the middle of a word.

### Changed
- **Internal:** `carrel-core` no longer converts a `quick_xml::Error` into a
  `CarrelError`. The conversion was unused, and it was the only place the XML
  library appeared in this crate's public API — removing it means future parser
  upgrades no longer change that API.

## [3.0.1] - 2026-07-30

### Changed
- **Internal cleanup only — nothing changes in the app.** The last places that
  still said "Folio" behind the scenes now say Carrel: the crate names, the
  application identifier, and the various keys used to store your settings and
  offline data. There is no new behaviour and nothing to do.

## [3.0.0] - 2026-07-30

The first release under the Carrel name. The major version reflects the one
thing this release asks of you: on most platforms the installer leaves the old
Folio app in place, so you have to remove it yourself. Nothing about your
library changes.

### Changed
- **Folio is now called Carrel.** The app, its window title, its icon, and its
  GitHub home have all been renamed. Your library, reading progress, highlights,
  settings, profiles, and offline downloads are untouched and carry over as-is —
  nothing to migrate and nothing to re-import.
  - Most installers treat a renamed app as a new one, so Carrel generally arrives
    *next to* Folio instead of replacing it. The Windows `.msi` is the exception
    and upgrades in place. Everywhere else, remove the old app once Carrel is
    running — drag `Folio.app` to the Trash on macOS, uninstall "Folio" from
    **Settings → Apps** if you used the Windows `-setup.exe`, or
    `sudo apt remove folio` on Linux. Removing it does not touch your library.
  - If you connect to the app from a phone or tablet, the web reader is
    unchanged — the same address, the same PIN, and anything you saved for
    offline reading stays saved.

### Added
- **A User Guide link in the app.** A **?** button in the top bar and a
  **User Guide** item in the tray menu open the online user guide in your
  browser.
- **The web API can list and switch profiles.** `GET /api/profiles` reports each
  profile with whether it's active, has a lock, and can be switched into;
  `POST /api/profile` switches the active profile for the whole server (desktop
  included — there is one shared active profile). A locked profile stays
  off-limits over the network: the profile password is never accepted over HTTP,
  so a locked profile can only be entered remotely if it was already unlocked in
  the desktop app this session, and otherwise returns `423 Locked`. See
  `docs/WEB_SERVER_API.md`.
- **Switch profiles from the phone/web reader.** With more than one profile, the
  web UI header gets a profile control: it shows the profile you're in and lists
  the others, so you can move the library between profiles from a phone or tablet
  without walking to the computer. Switching takes effect everywhere at once —
  the desktop app and every other web/OPDS client share one active profile. A
  locked profile is listed but greyed out ("Unlock on the desktop to use over the
  network"), because its password is never sent over the network; unlock it once
  in the desktop app and it becomes available remotely for that session.

### Fixed
- **A profile switch now reaches every open client.** Because one active profile
  is shared by the desktop app and every browser/OPDS client, a switch made from
  a phone used to leave other windows showing the previous profile's library
  (with book ids that no longer matched it). The desktop app now follows a
  switch made anywhere, and a stale browser tab reloads itself into the new
  profile on its next request.
- **Books saved for offline reading in the web app are now kept per profile.**
  Each profile has its own book ids, so a book saved offline under one profile
  could be served in place of a *different* book that happened to share that id
  in another profile. Offline downloads are now namespaced per profile: switching
  profiles shows only that profile's downloads, and downloads made under other
  profiles are kept intact rather than discarded.

## [2.11.1] - 2026-07-28

### Performance
- **Zooming a PDF or comic in the desktop reader is now instant.** Zooming
  scales the page you're looking at immediately (as it always did), but the
  reader used to re-render the page through the backend on every step of a
  wheel or held-key zoom — a burst of expensive renders that made zooming feel
  sluggish, and flashed a loading spinner over the page each time. Now a zoom
  burst re-renders once, after the zoom settles, and the sharper page swaps in
  quietly without a spinner — so zooming stays smooth and the page you're
  viewing never disappears behind a loading overlay while you adjust it.

## [2.11.0] - 2026-07-28

### Fixed
- **PDF pages render one at a time, preventing rare crashes.** The underlying
  PDF library isn't safe to call from multiple threads at once; with background
  and foreground rendering now able to overlap, PDF page rendering is serialized
  behind a lock. This also completes the fix above — a foreground page never
  renders at the same time as a background one, so it can't be slowed by it.
- **Reading a large PDF no longer bogs down mid-session.** After opening a PDF,
  the app renders the remaining pages into its cache in the background. That
  pass now pauses whenever you're actively viewing or turning a page, so it
  yields the disk/network and CPU to the page you're waiting on instead of
  competing with it — previously, on a large PDF over a network drive, the two
  fought for the file and a page could exceed the reader's load timeout.
- **The reader stays responsive while a page renders.** Comic and PDF page
  renders now run on a background thread instead of the request handler, so a
  slow page — a large PDF page fetched over a network drive can take a few
  seconds — no longer blocks other actions (navigation, menus, the page you're
  waiting on) until it finishes.
- **Opening a large PDF no longer freezes the reader.** `prepare_pdf` used to
  render the first 10 pages synchronously before the reader could show anything
  — tens of seconds on a large PDF stored on a network drive. It now renders
  only the page you're opening on, then hands the rest to the existing
  background pass (and on-demand rendering as you navigate), so the reader opens
  promptly instead of stalling on pages you may never see.

### Performance
- **Books on a network drive open to a fast local copy.** When you open a PDF or
  comic whose file lives on a network share (e.g. an SMB/NAS library kept in
  link mode), the app now copies it to a local cache in the background on open,
  then renders every page from that local copy. Previously each page was read by
  random access over the network at render time, which could take many seconds
  per page; the one-time background copy is sequential (fast) and pays for itself
  after the first page or two. Already-local books are untouched. The local copies
  are kept within a disk budget (least-recently-opened evicted first) and a
  book's copy is removed when you delete the book, so disk use stays bounded.
  This now applies to the web reader too: reading a network-hosted PDF or comic
  over the built-in web server serves pages from the same local copy (staged on
  the first page you open), so it's fast there as well — not just in the desktop
  app.
- **Smoother comic/PDF page turns.** The desktop reader now warms neighboring
  pages (two on each side, next pages first) as soon as you settle on a page,
  during browser idle time and without flooding a network-mounted library, so a
  forward turn lands on an already-rendered page instead of waiting for one.
  Preloading only promotes pages that are already cached — it never starts a
  cold render in the background, so it can't slow down the page you're on.
- **Fewer redundant page re-renders after a window resize.** The desktop reader
  now quantizes the requested render width coarsely, so small window-size
  changes no longer produce a new cache key and invalidate every already-loaded
  page. The in-memory page-image cache also holds more pages, keeping
  back-and-forth navigation within a chapter instant.
- **Rendered-page cache retains more books, for longer.** The on-disk page cache
  now keeps up to 20 books (was 5) for 30 days (was 7), so cycling through a
  comic series no longer evicts an album's rendered pages between sittings. The
  500 MB size cap (`page_cache_max_size_mb`) remains the effective limiter, so
  disk use is unchanged in the steady state.

## [2.10.0] - 2026-07-27

### Security
- **EPUB archive limits now apply at the crate's API boundary.** The entry-count
  and per-entry size caps were only enforced by `CachedEpubArchive::open` and by
  callers that invoked `validate_archive` themselves, so the five path-based
  helpers in `folio_core::epub` — `parse_epub_metadata`, `get_chapter_content`,
  `get_chapter_list`, `extract_cover`, `get_toc` — parsed hostile archives
  unchecked, including the LAN web server's chapter and chapter-list routes. All
  five now open through one internal `open_validated` helper that validates
  before parsing. The `*_from_archive` / `*_from_cache` variants still expect
  their caller to have validated, so cached readers do not re-scan per chapter.
- **Bounded archive entry reads (EPUB and CBZ).** Entry contents are read through
  a hard byte cap via `Read::take` — 16 MB for text entries (OPF,
  `container.xml`, XHTML, NCX/nav) and 100 MB for binary ones (cover art, inline
  images, comic pages) — instead of growing a buffer to whatever the entry
  decompresses to. The central-directory pre-scan only sees *declared* sizes, and
  the zip crate bounds a read by an entry's **compressed** size, so an entry that
  understated its decompressed size could previously expand at deflate's full
  ratio (~1032:1) before the trailing CRC check noticed.
- **CBZ page and ComicInfo reads capped.** `cbz.rs` pre-scanned the archive but
  then read `ComicInfo.xml` and page bytes unbounded — reachable from the web
  server's comic routes and from the page cache. Both now go through the capped
  reader; a non-UTF-8 `ComicInfo.xml` remains non-fatal as before.
- CBR needed no change: unrar truncates decompressed output at the header's
  `unpacked_size`, which `cbr::validate_archive` already pre-checks. MOBI has no
  archive layer; its bounds live inside libmobi (see the trust-boundary note atop
  `folio-core/src/mobi/mod.rs`).
- Corrected an inaccurate claim in `validate_archive`'s documentation (and in
  ROADMAP #51): the zip crate does **not** bound decompressed output during a
  read, so the pre-scan was never a second line of defence. The capped reads now
  are.

### Changed
- **Breaking (`folio-core` API):** `EpubError` gained a `LimitExceeded(String)`
  variant and is now marked `#[non_exhaustive]`. Adding the variant breaks any
  exhaustive `match` on `EpubError`, hence the minor bump; the attribute makes
  future variants additive. Downstream matches need a wildcard arm.
- Archive-limit rejections use `EpubError::LimitExceeded` instead of
  `MissingFile`, so they map to invalid-input (HTTP 400) rather than not-found
  (404).
- New public item `folio_core::epub::MAX_TEXT_ENTRY_SIZE` (16 MB). The value is a
  concurrency budget rather than a parser limit — worst-case allocation is the cap
  times concurrent chapter reads. Known cost: fixed-layout EPUBs that inline
  base64 images inside a single XHTML file over 16 MB are now rejected; tests pin
  the boundary at 15 MB (parses) and 17 MB (refused).

## [2.9.0] - 2026-07-25

### Added
- **Text highlighting in the web reader.** Select text while reading an EPUB
  or MOBI to highlight it in one of 5 colors, add notes, and manage everything
  from a new highlights drawer (🖍 in the reader toolbar): jump to a
  highlight, recolor it, edit its note, or delete it — tapping a highlight in
  the text offers the same actions. Highlights sync with the desktop app's
  (each side sees the other's); online-only — not available while reading
  offline-saved books. Backed by new web endpoints
  (`GET`/`POST /api/books/:id/highlights`,
  `PUT`/`DELETE /api/books/:id/highlights/:highlight_id`).
- **Bookmarks in the web reader.** The phone/web reader now has a bookmark
  button (🔖) in its toolbar, in every format. It opens a slide-in drawer:
  **Add bookmark here** saves your current spot (and immediately lets you name
  it), each entry shows its chapter (or page) and progress, tapping one jumps
  straight back, ✏️ renames it, and ✕ deletes it.
  Bookmarks are book-scoped, persist regardless of private mode (matching the
  desktop app), and sync across devices. Backed by new web endpoints
  (`GET`/`POST /api/books/:id/bookmarks`,
  `PUT`/`DELETE /api/books/:id/bookmarks/:bookmark_id`).
- **Table of contents in the web reader.** The phone/web reader's chapter
  toolbar now has a **Contents** button (reflowable books only — EPUB and
  MOBI) in place of the old numeric chapter slider. It opens a slide-in panel
  listing the book's chapters; tap one to jump straight there, with the
  current chapter highlighted. Books with no usable table of contents (a
  single chapter, or a TOC that can't be read) show a plain chapter label
  instead. (Page-image formats — PDF, CBZ, CBR — keep their page slider.)
- **Adjustable reading typography in the web reader.** The phone/web reader
  now has an **Aa** button in its toolbar (reflowable books only — EPUB and
  MOBI) opening a popover with four controls: **font size** (14–24 px),
  **line spacing** (1.2–2.4), **reading font** (Lora, Literata, DM Sans, and
  the dyslexia-friendly OpenDyslexic — all embedded and served by Folio, no
  external fonts), and **column width** (Narrow / Medium / Wide). Changes
  apply live and are remembered across books and sessions; your reading
  position is preserved when the text reflows. The four fonts are precached
  so a saved-offline book still renders in your chosen face on a secure
  context (HTTPS/localhost); over plain-HTTP LAN they load on first use.
- **"Want to read" flag.** Mark any book as want-to-read from its detail
  modal or the hover bookmark on its library card. A bookmark filter next to
  the reading-status filter narrows the grid to flagged books, and an
  optional "Want to Read" home shelf (toggled in Settings → General, off by
  default) surfaces them at the top of the library. The flag is independent
  of computed reading status and is preserved across backup/restore. The
  phone web UI has the same feature: mark or unmark a book from its book
  page, an always-visible "Want to read" toggle in the filter bar narrows
  the grid, flagged books show a 🔖 badge on their cover, and a "Want to
  read" shelf appears on the home view.
- **Save books for offline reading in the web reader.** On
  HTTPS-served web UIs (e.g. behind a Tailscale/reverse-proxy certificate;
  service workers require a secure context), a book's detail page now offers
  "Save offline": chapters, images, and comic/PDF pages (downscaled to
  1080 px wide via the new optional `?width=` parameter on the page-image
  endpoint) are downloaded into browser storage, with a progress counter, an
  "available offline" badge on the library grid, and a "Saved · size /
  Remove offline copy" state on the detail page. Reading a saved book falls back
  to the offline copy automatically when the server is unreachable, and
  opening the installed web app with no connection now boots straight into a
  library of your downloaded books (with an "Offline — showing downloaded
  books" banner and a Retry) instead of a dead-end error — saved books open
  and read fully offline, and a saved book's own URL deep-links to it
  directly. Reading progress made while offline is queued and synced back to
  the library when the connection returns — using a compare-then-push rule
  so a book you also read on another device in the meantime is never
  overwritten by a stale offline position. If the browser evicts a download
  under storage pressure, Folio notices on next launch, drops the stale
  "available offline" badge, and tells you; deleting a book on the desktop
  removes its offline copy on next connect. Requires the web UI to be served
  over a secure context (HTTPS or localhost).

### Fixed
- **Mobile web reader: streamlined book-detail actions that no longer clip off-screen, and long titles wrap.** The detail page now shows a single always-visible primary button — **Continue** (or **Read**, from the start) — with the rest of the actions (**Start Over**, **Save offline**, **Download**) tucked into a **⋯ More** menu of icon-and-label rows, so the row fits any phone width instead of pushing its leftmost button past the edge. A long unbroken title (e.g. an underscore-heavy filename) that used to widen the info column past the viewport now wraps within its column.
- **Book counts now read "1 book", not "1 books".** The library section headers
  and the series-stack cards said "1 books" (and "1 livres" in French) when a
  section or series held a single book; they now use the correct singular in
  both languages.

### Security
- **Second sanitization layer in the desktop reader.** EPUB chapter HTML was
  already sanitized in Rust (ammonia) before reaching the UI; the desktop
  reader now also runs it through DOMPurify in the renderer, so a gap in
  either layer alone can't put script into the page. Documented as a
  defence-in-depth measure — the server-side pass remains the primary one.
- **Fixed an EPUB image-rewriting edge case that could drop sanitization.**
  The `<img>` `src` rewriter that runs after sanitization scanned for tag
  boundaries without accounting for quoted attribute values, so a `>`
  inside an attribute could end a tag early and corrupt the surrounding
  markup. The scan is now quote-aware.
- **Documented the libmobi trust boundary.** MOBI/AZW parsing goes through
  libmobi (C) over `unsafe` FFI on untrusted input; `folio-core/src/mobi/mod.rs`
  now states the boundary, the pinned version per build, and the bump process.

## [2.8.0] - 2026-07-17

### Added
- **Check for app updates.** Folio can now tell you when a newer version is
  available on GitHub. Choose "Check for Updates" from the tray menu at any
  time, and Folio also checks quietly once when it starts — you can turn the
  startup check off under Settings → General. When a newer release exists, a
  window shows the new version and its release notes, with a button to open the
  download page on GitHub and a link to the full changelog; if you're already up
  to date, a manual check tells you so. The check only reads GitHub's public
  release list and never downloads or installs anything for you.
- **Filter and collapse the Collections panel.** The Collections panel now has
  a filter box at the top: start typing to narrow both your collections and your
  series to the ones whose names match, live as you type. Matching ignores case,
  and a clear button empties the box in one click. The Collections and Series
  lists each have a header you can click to collapse or expand that list
  independently, so a long list can be tucked away while you work in the other.
  Typing in the filter temporarily reveals matches inside a collapsed list;
  clearing it restores your collapse choice. "All Books" always stays in view,
  and the filter and collapse state reset when you close the panel.
- **A roomier Collections panel that floats over your library.** Opening
  Collections in the desktop app now slides a wider panel over the library
  instead of squeezing the book grid to one side — the grid stays put, and the
  panel has more room for collection names and controls. The library dims
  behind it; click anywhere outside the panel, or press `Esc`, to close it.
  Dragging a book onto a collection still works.
- **A bottom tab bar in the web app on phones and tablets.** On a phone,
  tablet, or installed web app, the primary destinations — Library,
  Collections, and Reading Stats — now live in a fixed bar along the bottom of
  the screen, within easy thumb reach, with the current section highlighted.
  The old top-corner icons for Collections and Stats step aside on these
  devices (the theme toggle stays in the header), and the bar tucks away while
  you're reading. On a desktop browser nothing changes — the header icons stay
  exactly where they were.
- **Zoom into pages in the web reader.** Comic and PDF pages in the browser
  reader can now be zoomed: hold Ctrl and scroll (or pinch on a trackpad)
  to zoom up to 5×, then scroll to pan around the page. On a phone or
  tablet, pinch with two fingers and drag with one to pan; swiping to turn
  pages still works when not zoomed. Double-tap (or double-click) jumps to
  2.5× at that spot and double-tapping again zooms back out. Zoom resets
  when you turn the page or switch fit mode.
- **Select, copy, and highlight text in PDFs.** PDFs now have a selectable
  text layer in the desktop reader, just like EPUBs: drag to select, copy to
  the clipboard, or highlight in any of the five colors. Highlights are saved
  per page, reappear as colored bands when you return, and can be removed —
  and they flow into the same Highlights panel, cross-book search, and
  shareable quote cards as EPUB highlights. Selection stays within a single
  page (each page of a two-page spread selects on its own).
- **Faster PDF search, every session.** The text Folio extracts to search a
  PDF is now saved alongside the book's cached pages, so full-text search is
  instant from the very first search of a session — not just after the book
  has been searched once. The index builds quietly in the background when you
  open a PDF and is reused on every later open; deleting the book clears it.
- **Set a daily reading-minutes goal.** Alongside the yearly books goal, the
  Reading Stats screen now has a "Reading goals" card where you can set how many
  minutes you want to read each day. A slim progress bar fills as you read and
  shows a "Goal met!" note once you reach it, resetting each day. It reads from
  reading time already tracked — no extra setup, and nothing leaves your device.
- **Share a highlight as an image.** Turn any highlight — or a fresh selection
  while reading — into a styled quote card and share it. **Share as image** (in
  the reader selection popup and per-highlight in the Highlights panel) opens a
  dialog with a live preview: pick a style (Light, Sepia, or Dark), optionally
  include the book's cover thumbnail and a small Folio wordmark, then **Copy
  image** to your clipboard or **Save as PNG…** to a file. The card shows the
  quote with the book title and author; long quotes are trimmed to fit, and a
  missing cover or author is simply left off. Rendered locally on the desktop
  reader.
- **Look up words while you read (offline).** Select a single word in the
  desktop reader and hit **Define** to see its definition without leaving the
  page — parts of speech, numbered senses, an example, and synonyms, in an
  anchored card next to your selection. Definitions come from Princeton
  WordNet 3.1, packaged as a ~7 MB offline dictionary you download once from
  **Settings → Dictionary** (and can delete anytime to reclaim the space).
  Everything runs locally — no network lookups, no accounts. Inflected forms
  resolve to their base word (e.g. selecting "running" defines "run"). This is
  the offline v1; an online fallback and user-loaded dictionaries remain on the
  roadmap.
- **Build a vocabulary list from the words you look up.** Opt in from
  **Settings → Dictionary** ("Build my vocabulary list", off by default) and
  every word you Define is saved to a personal, per-profile list — with its
  definition, the book it came from, and the sentence you found it in. A new
  **Vocabulary** screen lists your saved words and offers a lightweight
  flashcard review: words come up when they're due, and marking "Got it" or
  "Missed" schedules the next review using spaced-repetition boxes. Delete
  words individually or clear the whole list. Everything stays on your device;
  the saved words keep their definitions even if you later delete the
  dictionary or the source book. Turning the setting off stops saving but keeps
  what you've collected. Filter the list as you type, and click any saved word to
  jump straight back to the book at the spot where you looked it up.
- **Usage analytics (opt-in).** Folio can send a single anonymous `app_started`
  event per launch to help gauge how many people use the app. Off by default —
  nothing is sent until you opt in via the first-run prompt or Settings. No
  personal data, book titles, or library contents are ever transmitted, and no
  user or install identifier is sent. See docs/PRIVACY.md.

### Changed
- **The Discover shelf hides books you already own and won't show duplicates.**
  Recommendations that match a book already in your library — by title and
  author, ignoring case and spacing — are filtered out, and the same book
  surfaced by more than one catalog appears only once. Each recommendation now
  shows an "Adding…" state while it downloads so you can tell it's working, and
  once you've added every current pick the shelf tells you so instead of
  looking broken.
- **The installed web app runs edge-to-edge on notched phones.** When Folio's
  web UI is added to the home screen on a device with a notch or a home
  indicator, the header now clears the status bar, the bottom tab bar sits
  above the home indicator, and nothing hides behind the rounded corners or the
  camera cutout. The status bar stays readable in both light and dark themes.
  On a normal browser or a device without a notch, nothing changes.
- **The web reader feels less like a web page on phones and tablets.** Pulling
  down past the top of a list no longer triggers the browser's pull-to-refresh
  (which used to reload the whole app), and over-scrolling no longer rubber-bands
  to reveal the page edges. The reader now sizes itself to the real visible area,
  so a page is never cut off behind the browser's collapsing address bar.
- **Book covers no longer stick "lifted" after a tap on phones and tablets.**
  In the web UI the subtle raise-on-hover for book covers is a mouse gesture;
  on a touchscreen it used to latch on after a tap and leave the cover stuck in
  the lifted state until you touched something else. The lift is now limited to
  devices with a hovering mouse or trackpad, so it no longer sticks after a tap
  on a phone or tablet. On a desktop browser nothing changes.
- **Bigger, easier-to-hit buttons on phones and tablets.** In the web UI the
  small controls — the header icons and sort menu, the filter buttons and chip
  removes, the reader's page buttons — now grow to at least the 44-pixel
  touch-target size recommended for fingers, so they're harder to miss. Only
  the tappable area grows; the colors and text stay the same, and a desktop
  browser is unchanged.
- **Long-pressing the web UI's chrome no longer pops a selection menu.**
  Pressing and holding a header, a book cover, the bottom tab bar, or a toolbar
  in the web UI used to select text or raise the browser's copy/callout menu,
  the way a web page does. Those surfaces now ignore the long-press. Actual
  reading content still selects normally — you can press-and-hold to select and
  copy chapter text or a book's description.
- **Comics open instantly, even large ones.** Opening a CBZ/CBR now paints the
  first page in tens of milliseconds instead of waiting seconds for the whole
  archive to extract. Folio extracts just the first page (plus your resume page)
  up front and returns immediately, then streams the remaining pages into the
  disk cache on a background task behind a dismissible "preparing pages N/total"
  bar. Navigation stays correct throughout — jumping to a page that hasn't been
  extracted yet reads it on demand — so the bar is pure feedback you can ignore
  or close.
- **PDFs keep warming up behind you.** The PDF counterpart to the comics change
  above. Opening a PDF still paints its first pages instantly, and now Folio
  renders the *rest* of the book's pages into the disk cache on a background
  task — behind the same dismissible "caching pages N/total" bar — so jumping to
  any page or scrubbing the thumbnail strip becomes instant instead of waiting
  on the PDF engine. The pass is bounded by the page-cache size limit (on very
  large PDFs it stops once the cap is reached and the remaining pages are still
  rendered on demand), and it's skipped entirely while "don't track this
  session" is on.
- **Instant chapter turns in the paginated reader.** The paginated EPUB/MOBI
  reader now prefetches the adjacent chapters (current ±1) in the background, so
  pressing Previous/Next renders the next chapter synchronously from an
  in-memory cache instead of waiting on a `get_chapter_content` round-trip. The
  cache is scoped to the open book and bounded to a small window around your
  position, so it never grows unbounded or leaks one book's content into
  another.
- **Instant chapter turns in the web reader, too.** The embedded web/phone
  reader now gets the same treatment: after a chapter renders it prefetches the
  neighbouring chapters (current ±2) into a book-scoped in-memory cache and
  warms their inline image URLs, so paging forward on a phone renders straight
  from cache instead of waiting on a network round-trip. Brings the web reader
  to parity with the desktop reader above.
- **Live feedback while searching catalogs.** Searching all catalogs now shows a
  per-catalog checklist that ticks each source off (with its result count, or a
  "Failed" marker) the moment it responds, instead of a single static
  "Searching all catalogs…" message. Searching a single catalog names it
  ("Searching Project Gutenberg…") rather than showing a bare "Loading…".

### Fixed
- **Library covers pin to the left on wide screens.** The library grid and the
  series/skeleton grids used to center each row, so a sparse row floated to the
  middle of a wide window with large gaps on both sides. Covers now align to the
  left, matching a standard library layout.

## [2.7.0] - 2026-07-08

A reading-insights and privacy release: the stats screen gains a year view and
a goal to read toward, book details show how you actually read each title, and
two new privacy controls — a per-profile lock and a "don't track this session"
mode — let you decide what the app records and shows.

### Added
- **Year-long reading heatmap.** The Reading Stats screen adds a GitHub-style
  calendar heatmap of the last 365 days (intensity = minutes read that day),
  alongside the existing 30-day bar chart. Hover or focus a day to see its date
  and reading time. Month labels track the visible window; days with no reading
  read as empty cells.
- **Yearly reading goal.** Set a target number of books to finish this calendar
  year and track it with a progress ring on the stats screen, a pace indicator
  ("3 ahead of schedule" / "on track" / "2 behind"), and a completed state when
  you cross the goal. Backed by a new `reading_progress.finished_at` timestamp
  so the count reflects when a book was actually finished (re-opening an old
  finished book no longer inflates this year's total).
- **Per-book reading insights.** The book details view now shows how you read a
  specific title — total time spent, number of sessions, date started, date
  finished, and average session length — from the same session data that feeds
  the global dashboard. Rows appear only when there's data, so a book finished
  on another device (synced, no local sessions) still shows its finished date.
- **Profile lock.** Optionally protect a profile with a password: switching into
  it (and reaching it at startup) prompts for the password, and the LAN web /
  OPDS server won't serve a locked profile until it's unlocked on the desktop.
  The password is hashed with Argon2id in your OS keychain. This is a
  **deterrent that hides a profile from casual view — it does not encrypt** your
  books, database, or cached pages; anyone with access to your files can still
  read them, and the in-app copy says so. A lock can be set when creating a new
  profile (a "Lock this profile" option in the create dialog) or later from
  Settings, and there's a deliberate "Can't sign in?" recovery that clears the
  lock (safe, since nothing is encrypted).
- **"Don't track this session" mode.** An app-wide toggle with a persistent
  indicator that pauses passive tracking — reading position, session stats and
  streaks, recently-read, and reading entries in the activity log — while it's
  on. Your highlights and bookmarks are still saved and the book stays in your
  library (an info popover spells out exactly what pauses and what doesn't).
  Suppression covers every path data would otherwise leak through, including the
  plugin event bus, the on-disk page cache, and outbound sync. A book reopened
  within the same session still resumes from where you were (held in memory
  only, never written to disk). Resets off on every app restart.

## [2.6.1] - 2026-07-06

A web-UI patch for mobile.

### Fixed
- **Long book titles on mobile.** The web reader and book-detail headers now wrap long titles across lines (instead of overflowing the header or being truncated) and use a smaller, understated title so the book's content leads. Navigation icons stay pinned right.
- **LAN updates were hidden by HTTP caching.** Shell assets (`app.js`, `app.css`, `index.html`, `manifest.json`) are now served `Cache-Control: no-cache` (revalidate each load) instead of `max-age=3600`. On the plain-HTTP LAN URL the service worker never registers, so a long `max-age` could hide UI updates for up to an hour.

## [2.6.0] - 2026-07-05

A web-UI release: the built-in browser reader (the LAN/remote surface on
`:7788`) was rebuilt to match the desktop app, plus reader and metadata
polish on the desktop side. See `docs/web-ui-improvements.md` for the full
per-item breakdown and decision log.

### Added
- **Web UI overhaul.** The embedded web reader now matches the desktop app's design (warm paper/terracotta palette, serif/sans type) with a **light/dark/system theme toggle**, **keyboard shortcuts** (`/` to search, reader arrow navigation, a shortcuts overlay), a **paginated infinite-scroll library** with server-side search, series/collection filters, and sort, **home shelves** ("Continue Reading" + "Recently Added"), **reading-progress sync** with **progress badges** on grid and shelf cards, a richer book detail page (progress bar, Continue / Start-over), an **animated swipe page-turn** in the page-image reader on touch devices, **cover thumbnails** for faster grids, and loading skeletons / friendly empty states / broken-cover placeholders.
- **Installable web app (PWA).** The web UI ships a manifest + service worker (app-shell caching) and supports **iOS "Add to Home Screen"** for an app-like launch. (Service-worker offline caching only activates on a secure context; Add-to-Home-Screen still works over the plain-HTTP LAN URL.)
- **Reader "book details" popup (desktop).** An **(i)** button in the reader toolbar opens a read-only popup with the book's title, author, format, series, publisher, year, language, and full description — without leaving the page.
- **Committed end-to-end tests.** A Playwright web-UI suite (`e2e/`) runs against a seeded harness (`src-tauri/examples/web_e2e_server.rs`) as a new CI job.
- **New web API endpoints.** `GET /api/reading-progress`, `GET /api/books/continue-reading`, `GET`/`PUT /api/books/:id/progress`; `/api/books` gained `?series=`, `?sort=`, `?limit=&offset=` (with an `X-Total-Count` header); `/api/books/:id/cover?size=thumb` serves a downscaled thumbnail.

### Fixed
- **HTML entities in book metadata.** ComicInfo `<Summary>` and EPUB `<dc:*>` values are now entity-decoded on import, so descriptions/titles/series no longer render literal `&gt;`, `&lt;`, `&amp;`. (Numeric/identifier fields are left as-is; decoding falls back to the raw value on malformed input.)
- **Book description newlines.** The desktop book-detail description now preserves paragraph breaks instead of collapsing them into a run-on block.

## [2.5.0] - 2026-07-02

A trust-and-feedback release driven by a full UX audit of the first-run →
import → organize → read → catalog/settings path. The themes: destructive
actions are now reversible or confirmed (never silent), every async action
reports its outcome, and error/empty states are built rather than blank.

### Added
- **Undo for deletes.** Deleting a book, deleting a multi-selection, and removing a book from a collection now show a brief **Undo** toast; the book is hidden immediately and the actual removal only fires after the window, so an undo reverses it before anything irreversible happens (no file is deleted).
- **Settings search.** A search box at the top of Settings filters the collapsible sections by name and keyword (e.g. "pin", "css", "backup") and expands matches.
- **Reader header overflow menu.** The reader header is grouped (navigate / content / display) with low-frequency actions tucked into a `⋯` menu instead of a flat row of icons.
- **Continuous-load progress.** Continuous-scroll reading shows a real "Loaded X / N chapters" counter (backed by per-chapter progress events) instead of an indeterminate spinner.
- **Catalog connection test.** Adding a custom OPDS catalog validates the URL and runs a pre-flight fetch/parse (including private/LAN feeds) before saving, so a bad or unreachable feed is caught at add time. A no-catalogs empty state offers a shortcut to the preset picker.
- **OPDS download size.** Catalog download links show the file size when the feed reports it.
- **Plugin folder writability check.** Granting a plugin a write folder now verifies the folder is actually writable (enforced in `plugin_enable`, not just the UI) before recording the grant.

### Changed
- **Confirmations are styled, not native.** A reusable `ConfirmDialog` replaces the browser `confirm()` for destructive decisions — profile delete (also now disabled for the active/default profile), bulk delete (with count), and catalog removal.
- **Bulk edit is opt-in per field.** Each field has an explicit checkbox; only checked fields are written, with a banner and per-field warning when the selection has differing values — no more silent mass-overwrite. Mixed detection runs over the whole selection, not just the visible subset.
- **Save feedback everywhere.** Editing book metadata confirms with a "Saved" toast; settings toggles that previously swallowed persistence errors now revert and surface the failure; the web-server PIN shows an "Unsaved" indicator and saves on blur so it isn't lost on close.
- **Backup Save vs Test split.** The single "Save & Test" button is now separate **Save** and **Test connection** actions with independent results, so a save failure is distinguishable from a connection failure.
- **Import errors are actionable.** Failures show a friendly message (not a raw backend string), persist instead of vanishing in 4s, and offer **Retry**; partial-batch failures highlight the failed count and stay visible; the onboarding import step shows a banner + retry on empty/error/cancelled instead of getting silently stuck.
- **Reader recovery & polish.** Chapter-load errors show a recoverable card with **Try again**; the missing-file dialog is a single consolidated prompt; a content skeleton renders while a chapter loads; a just-created highlight can be removed from its toast; the settings button shows an open state.
- **Grid/organize.** Dragging a book onto a collection confirms with a toast; the delete confirmation shows the cover and full title; the selection is preserved while in selection mode; the select checkbox no longer overlaps the card action buttons; tag-filter counts respect the other active filters; empty results distinguish "no books yet" from "filters hide everything"; the edit-dialog error is a sticky top banner.

### Fixed
- **Clear-filters now clears tag filters too** — previously a tag-only filter survived "clear all filters".
- **Invalid nested button** in the catalog row (a `<button>` inside a `<button>`) split into siblings, fixing keyboard/click behavior.
- **Web-server port** out-of-range values show an inline range error instead of silently clamping to the boundary.
- **Blank pages in reader.** Page images are delivered as `blob:` URLs, but the CSP `img-src` never allowed `blob:`, so under enforced CSP (production builds) every page rendered as a broken image (a blank page with just a "Page N of M" box) — all formats, all profiles. `blob:` added to `img-src`. Worked in `tauri dev` (relaxed CSP), which is why it shipped.
- **Silent page-load failures.** The page `<img>` had no `onError` handler, so an image that failed to render showed only the browser's broken-image state with no visible error. It now surfaces the error overlay.

## [2.4.0] - 2026-06-18

A backup-and-restore release. Library restore now reconstructs the whole
library — not just books — and exported backups are far smaller. Several
restore paths that silently dropped data (or failed outright) are fixed.

### Added
- **Full restore.** Restoring a backup now brings back reading progress, bookmarks, highlights, collections, and tags in addition to books and covers. Restore is a best-effort, non-destructive merge: rows referencing a book that wasn't imported are skipped, and re-importing the same backup is safe (idempotent). Backed by a new `restore_secondary_data` helper in `folio-core`.
- **Linked books in restore.** Linked books (not copied into the library) are now restored as links to their original absolute path. The source volume must be mounted at the same path on the restoring machine. Previously they were silently dropped.

### Changed
- **Smaller backups.** Library exports now ship the lightweight grid thumbnails rather than full-resolution covers — on a ~2,000-book library the cover payload drops from ~1.1 GB to ~150 MB. Restored covers are the 320px thumbnail; full-resolution covers are re-derivable by re-importing from source files.
- **Large-file exports.** Book files ≥4 GB no longer abort the export mid-write (ZIP64 is now forced for stored entries), so full backups of large libraries produce a valid, extractable archive.
- **Cleaner PDF metadata.** PDF import now ignores junk embedded metadata from tool-generated files: a bare-UUID Title falls back to the filename, and a URL Author (e.g. an ImageMagick homepage) is dropped.

### Fixed
- **Restore worked at all.** `library.json` is written as an object (`{ version, books, ... }`) but restore parsed it as a bare array, so every restore errored and the UI silently bounced back to the file picker. Restore now parses the object (and still accepts a bare array for older backups).
- **Library refreshes after restore.** The grid now re-fetches automatically once a restore completes, instead of showing stale contents until the next manual reload.

## [2.3.0] - 2026-06-17

An extensibility release. Folio gains a sandboxed plugin system (Rhai scripts
with an explicit, consent-gated permission model), a typed lifecycle event bus
underpinning it, and resilient network behaviour for metadata enrichment and
OPDS. Imports get a fast skip-before-hash path for unchanged files, and caches
are unified behind a single managed abstraction with stats and a clear-all
control.

### Added
- **Plugin system.** Folio can now run user-installed plugins written in [Rhai](https://rhai.rs), scoped by an explicit permission model and gated behind a consent dialog. Plugins declare capabilities in a manifest and are granted them per-install; a new **Settings → Plugins** panel (EN/FR) lists installed plugins, surfaces requested permissions, and manages consent. The desktop host exposes plugin commands over IPC and ships example plugins.
  - **Capabilities** landed incrementally: `read:highlights` and `write:files` (with a highlight-exporter example), then `import:books` plus network access, enabling an OPDS auto-download plugin that pulls books from a remote feed.
  - Built on a typed **lifecycle event bus** in `folio-core` — command paths emit structured events (import, enrich, etc.) that plugins and internal observers subscribe to, replacing ad-hoc hooks.
- **Library book counts.** The library view shows the total book count and an imported-vs-linked breakdown.
- **OPDS conditional requests.** Book feeds now send weak ETags and honour `304 Not Modified`, so unchanged feeds skip re-downloads. Backed by a `book_etag_pairs` DB helper.

### Changed
- **Fast re-import (skip-before-hash).** Re-importing an unchanged source file now skips before hashing when the source path, size, and mtime are unchanged — much faster folder re-scans on large libraries. New `source_path` / `size` / `mtime` columns back the fast path, which self-heals on mtime drift and falls through to hash dedup when the cheap check misses.
- **Resilient enrichment HTTP.** All metadata-provider requests route through a `send_with_retry` loop with backoff and `Retry-After` handling; a new `RateLimited` error variant surfaces exhausted 429 retries. The scan UI shows provider-retry feedback during enrichment so backoff is visible rather than looking like a hang.
- **Unified cache abstraction.** Memory and disk page caches now sit behind a single `ManagedCache` trait and registry (`MemoryCacheAdapter`, `DiskPageCacheAdapter`). Settings gains a unified cache-stats view and a clear-all control wired over IPC.

### Fixed
- **macOS SMB accented-filename imports.** Imports/reads of files with accented (non-ASCII) names from an SMB share could fail with `os error 2`; this is a known macOS smbfs Unicode bug, and the import/read error now explains the cause and suggests mounting over NFS instead of presenting it as a Folio failure.

### Internal
- **CI hardening.** Lint and formatting are now enforced workspace-wide: `cargo clippy --workspace --all-targets` and `cargo fmt --all --check` cover both `folio` and `folio-core`. The Rust toolchain is pinned to `1.96.0` in `rust-toolchain.toml` and matched in CI so local and CI never drift. A `docs-on-merge` workflow keeps in-repo docs in sync after PR merges.
- **Documentation.** Added a plugin-system architecture guide and documented the workspace-wide fmt/clippy checks and toolchain pin.

## [2.2.1] - 2026-06-02

### Fixed
- **arm64 macOS app crashed on launch unless `brew install libmobi` was present.** The Apple Silicon release dynamically linked libmobi against the absolute Homebrew path `/opt/homebrew/opt/libmobi/lib/libmobi.0.dylib`, so any user without that exact install hit a `dyld: Library not loaded` abort before the app even started. The arm64 macOS build now builds libmobi from source as a static archive (mirroring the Windows build — `BUILD_SHARED_LIBS=OFF`, bundled miniz merged in) and links it statically, so the `.app` is self-contained and needs no Homebrew install. `folio-core/build.rs` gains a `LIBMOBI_STATIC` opt-in for this; local dev and Linux keep dynamic linkage.

## [2.2.0] - 2026-06-02

A performance release focused on large libraries: cover images and the book
grid no longer scale their cost with the number of books, so scrolling stays
smooth into the thousands.

### Performance
- **Cover thumbnails for the library grid.** Covers are now downscaled to a 320 px-wide JPEG thumbnail (`{book_id}/thumb.jpg`) on import and served to the grid, instead of decoding the full-resolution cover — often 1,500–1,900 px wide (~5 MP) — just to paint a 160 px card. Existing libraries are backfilled in a background thread at startup; covers already at or below 320 px are left untouched (a cheap header probe, no full decode), so only the genuinely large ones are re-encoded. The full-resolution cover is still used in the book detail view. Cuts cover decode work by roughly 95 % and, on a ~1,800-book library, total cover storage from ~950 MB to a few tens of MB.
- **Virtualized library grid.** The main library view renders only the rows near the viewport instead of mounting every book card into the DOM at once. A new windowed grid (built on `react-virtuoso`; it chunks the flat book list into rows whose column count tracks the window width and reuses the page's existing scroll container, so the Continue Reading / Discover headers still scroll above it) keeps DOM size, style recalculation, and paint cost proportional to what is on screen rather than to library size — scrolling stays smooth into the thousands of books. Library cards were also lightened: the hover action buttons mount only on hover/focus, and the badge backdrop-blur (expensive to composite) was dropped.

## [2.1.0] - 2026-05-30

A feature release on top of the 2.0 platform: side-by-side reading, richer
library cues, and a production-hardened remote-access server (audit trails, a
GDPR data export, and backup pre-flight checks). The `2.0.1`–`2.0.3` tags in
between were `folio-core` crate point-releases; this is the next user-facing app
release.

### Performance
- **PDF page disk cache** (ROADMAP "perf + comics" #3). Rendered PDF pages now survive app restarts. On first open of a PDF, `prepare_pdf` renders the first ten pages at a fixed canonical width (2400 px) into the shared `page-cache/{hash}/` namespace and returns the page count so the reader can skip a second `get_pdf_page_count` round-trip. Subsequent reads hit disk and resize down to the viewport width, bypassing pdfium entirely. Cache misses render at the canonical width, write best-effort, and trigger a coalesced background eviction every 25 lazy writes. Eviction reads filesystem-truth via `book_disk_size_bytes` so a stale manifest snapshot cannot drift the size budget. Shares the same Settings size cap and LRU / 7-day eviction as the comic cache. Linked / unhashed PDFs (or storage errors) gracefully fall back to live render at the viewport width — pre-spec performance preserved.
- **Page images served at viewport resolution over binary IPC** (ROADMAP P2). PDF / CBZ / CBR pages are now resized to the viewport width on the Rust side, transmitted as raw bytes through Tauri IPC, and wrapped as `Blob` + `URL.createObjectURL` in the frontend. Cuts IPC payloads by roughly 70–90 % on typical pages, removes the base64 encode/decode round-trip, and lowers steady-state renderer memory. Landed across m1–m4: viewport-resize support in `folio-core`, binary page commands, frontend blob URLs with revoke-on-eviction, and retirement of the legacy data-URI commands.
- **Reader screen code-splitting** (F-4-6). The Reader route is lazy-loaded via a Vite dynamic import, so the library/home view no longer ships the reader bundle up front — smaller initial download and faster first paint.

### Added
- **Split view** (ROADMAP #40). Read two books side-by-side. A new header button (or the `\` shortcut) toggles split mode; the companion pane opens a library picker so the pairing can be any two books. Each pane writes its own reading progress (the persistence guard collapses to primary-only when both panes happen to show the same book). The active pane gets a subtle accent ring so keyboard navigation routes there; click the other pane to swap focus. Split state and companion bookId persist per book in `localStorage` so reopening restores the layout. Includes a swap-panes button on the primary header (navigates to the companion bookId and seeds the new primary's split state) and an X to close the companion pane from the companion header. Built from a structural extraction that split the 2200-line Reader screen into a thin shell + a reusable `ReaderPane` component, then layered the layout shell + book picker + focus routing on top across four milestones.
- **Page-thumbnail strip** for image-based formats (CBZ / CBR / PDF). A toggleable horizontal strip below the reader shows every page; clicking a thumbnail jumps to that page (and stamps navigation history). Header button + `m` shortcut. Per-book open/closed state persists in `localStorage`.
  - Virtualized: only thumbnails inside the visible window plus overscan render as DOM nodes, so a 1000-page book stays cheap.
  - Module-level per-book blob-URL cache survives strip close/reopen — second open is instant.
  - Directional prefetch + distance-from-current load ordering: pages near the current page decode first, and a scroll-direction-biased prefetch window keeps the next viewport already decoded by the time it lands.
  - Per-tile loading / error / loaded states with retry-on-click for failed tiles. Empty tiles render transparent (no border / background) so the strip stays quiet while many pages decode.
  - Subtle motion: strip slide-up enter, per-tile fade-up, active-tile shadow + accent number label, edge mask fading thumbs into the surface. All animations honour `prefers-reduced-motion`.
- **Reading status indicators** (F-1-4). Each library card's top-right pill now conveys reading status by colour: **Active** (sage, shows %) for books read within the last 14 days, **Paused** (amber, shows %) for in-progress books idle longer than that, and **Finished** (a checkmark) for completed books. Unread books show no pill. Status is derived at render time from existing progress + last-read data — no new storage, no database writes. A pure `getReadingStatus` helper carries the logic with unit tests for every state and the 14-day boundary.
- **Smart collection auto-suggestions** (F-1-6). Folio proposes collections based on your reading history and library shape, bridging the gap between manual collections and rule-based smart collections.

### Security & remote access
- **GDPR data export endpoint** (F-3-6). `GET /api/data-export` on the embedded web server returns a timestamped ZIP of your personal data — books metadata, reading progress, bookmarks, highlights, the activity log, and settings — as a single JSON document. Credentials are never exported (backup configuration and metadata-provider API keys are redacted; the web PIN lives in the OS keyring). The endpoint requires authentication and is refused entirely unless a web PIN is configured, so it never serves your data on an open server.
- **Web server login audit trail** (F-3-1). Login attempts against the remote-access server are recorded to a dedicated `web_session_log` (timestamp, IP, user-agent, outcome) so you can review access. Web PIN-screen attempts log all outcomes; OPDS reader-app connections log only failures. The PIN is never written. Entries are pruned after 90 days / 5,000 rows, and the trail is readable via `GET /api/audit/login-history`. Logging is best-effort and never blocks or fails a login.
- **Backup connectivity verification & secret rotation** (F-3-7). Backup credentials are tested before they are saved, with an atomic DB + keychain update and rollback on failure, so silent backup misconfiguration no longer goes unnoticed.

### Internal
- **Structured activity audit log** (F-2-2). A typed `ActivityEvent` enum replaces loose string-based activity writes and is the single source of truth for the action/entity wire contract consumed by the frontend; adds activity-log export and configurable pruning.
- **Observability primitives** (F-2-3). Structured logging via `tracing` is initialised at startup (with a retained appender guard) and previously-silent `eprintln` warnings are routed through it; key operations (`import_book`, `enrich_book`) are instrumented.
- **IPC response metrics middleware** (F-4-8). A ring-buffer metrics layer times hot-path Tauri commands (count, avg, p95, max, slow-call warnings) and exposes them via a `get_ipc_metrics` command, with panic-safe, poison-recovering aggregation.

### Fixed
- **PageViewer re-animated the current page on layout reflow.** The slide-in animation re-fired when the load-spread effect re-ran for reasons other than a real page turn (for example, the thumbnail strip mounting and shifting the page-image cache key). Tracked the last-animated page index so the animation only plays on actual navigation.
- **Split-view overlay scoping, focus trap, and swap symmetry.** Post-review fixes on top of the initial split-view ship: the TOC focus trap now uses a ref instead of `getElementById("toc-sidebar")` so two ReaderPanes can render a sidebar without colliding on the same DOM id; the TOC sidebar/backdrop and the missing-file dialog scope to their pane (`absolute` over a `relative` pane root) instead of the whole viewport, so opening the companion's TOC no longer plants the sidebar over the primary pane; `swapPanes` leaves the old primary's pairing intact (`companion-A = B`) so navigating back to A restores the same split layout instead of degenerating into a same-book split. The localStorage contract moved into `src/lib/splitView.ts` with 14 unit tests covering key derivation, read/write, swap round-trip, effective companion fallback, and the persistence collapse.

## [2.0.3] - 2026-05-18

### Added
- `folio_core::opds_feed` — public primitives for rendering OPDS Atom feeds: `xml_escape`, `mobi_ext_and_mime`, `cover_mime`, `book_to_entry`, `wrap_feed`, `EntryUrls`, `FeedKind`, and the two content-type constants. Lets external tooling render OPDS feeds from `Book` rows without depending on the desktop app's `web_server` module.

## [2.0.2] - 2026-05-18

### Added
- `folio_core::db::provision_library(path)` — public entry point for creating a library file and applying the canonical schema without taking a connection-pool handle. Idempotent.

## [2.0.0] - 2026-05-03

A milestone release. The 1.x line shipped the reader and the library; 2.0 is the platform underneath it. The desktop app now sits on top of `folio-core`, a separately-tested Rust crate with a pluggable `Storage` trait and structured errors — the same machinery that powers the embedded web server. New formats (MOBI / AZW / AZW3), a back/forward navigation stack, a curated OPDS preset picker, and a refactored remote-access toggle round out the user-facing additions. UX has had a measurable consistency pass (4 px spacing grid, clustered animation durations, normalized icon strokes, codified error surfaces).

### Added
- **MOBI / AZW / AZW3 reading** (ROADMAP #34) — Mobipocket and Kindle formats via libmobi, with a parsed-book in-memory cache, capped memory, and word-count metadata. Available on Linux, arm64 macOS, and Windows (statically linked, no separate libmobi install). Intel macOS remains unsupported.
- **Navigation history** (ROADMAP #36) — back/forward stack across the HTML reader (EPUB / MOBI) and the image/PDF reader. Same-position pushes truncate the forward branch correctly; same-chapter and search-driven jumps stamp history; state resets on book switch so navigation cannot leak between books.
- **OPDS preset picker** — curated catalog of 13+ vetted OPDS feeds (multilingual: English, French, Hungarian, Bulgarian) addable in one click from an inline picker in the catalog browser. Includes Project Gutenberg, Standard Ebooks, Wikisource, Elephant Editions, Feedbooks, ManyBooks, ebooksgratuits, and others. Pure preset filter and facet helpers behind the UI.
- **Independent Web UI / OPDS toggles** — the Remote Access settings replace the single start/stop button with two checkboxes. Web UI and OPDS can be enabled independently and the embedded server reconciles itself accordingly. Existing single-toggle settings auto-migrate on first launch.
- **Library section toggles + collapsible series groups** — Continue Reading and Discover sections can each be hidden, and grouped series are collapsible.

### Changed
- **`folio-core` crate extraction** (ROADMAP #63) — `db`, `models`, `error`, `paths`, the format parsers (EPUB / PDF / CBZ / CBR / MOBI), `page_cache`, `enrichment`, providers, `opds`, `openlibrary`, `backup`, and `sync` now live in a separately-tested crate. The Tauri layer (`src-tauri/`) owns commands, the tray, and the embedded web server; everything else is reusable Rust.
- **Pluggable `Storage` trait** (ROADMAP #64) — book file I/O, cover images, page cache, EPUB inline images, and backup file reads all go through a `Storage` trait with atomic overwrites and key-validation guards. The DB `file_path` column now stores storage keys rather than raw paths. Foundation for cloud-backed storage backends without touching command handlers.
- **Structured error types across the Rust backend** (ROADMAP #55) — every Tauri command returns a typed `FolioError` enum (`NotFound`, `PermissionDenied`, `InvalidInput`, `Network`, `Database`, `Io`, `Serialization`, `Internal`) serialized at the IPC boundary as `{kind, message}`. `friendlyError()` routes by `kind` first, with all 8 categories translated in English and French. Web-server HTTP handlers map error kinds to correct status codes (404 / 403 / 400 / 502 / 500) instead of always returning 500.
- **UX consistency pass** — spacing locked to a 4 px grid (scanner test), SVG `strokeWidth` normalized to 1.5 / 2 (spinner exempt), Tailwind animation durations clustered at 150 / 200 / 300 ms, toast / inline / dialog error surfaces codified, dark-mode coverage scanner with Library red-banner fixes.
- **Settings reorg** — orphan Activity Log launcher folded into the Library section.
- **macOS tray responsiveness** — closing the window now minimizes instead of hiding so the macOS event loop stays alive and the tray menu remains responsive. `ExitRequested` handler prevents auto-exit when autostart and tray are enabled. The tray *Show* action recreates the window if destroyed.
- **Backup running flag via RAII guard** — `BACKUP_RUNNING` is now released through a guard so an early return or panic cannot leave the flag stuck.

### Fixed
- **Web server deadlock on auto-start** — the auto-start path held the `web_server_handle` mutex while calling `rebuild_tray_menu`, which also locks the same mutex. Since `std::sync::Mutex` is not reentrant, this deadlocked on every launch with the web server enabled, hanging all web-server IPC calls.
- **App no longer panics on startup DB failures** — database initialisation errors now propagate through the Tauri setup closure instead of crashing via `.expect()`.
- **Web-server auto-start survives poisoned locks** — a poisoned mutex at launch logs a warning and skips web-server auto-start rather than crashing.
- **Correct translations for archive corruption, chapter loading, keychain failures, JSON parse errors** — several mis-wired error kinds and translation keys were silently falling through to raw English messages. French-locale users now see localised copy for these paths.
- **External EPUB links open in the default browser** — previously they tried to navigate inside the reader iframe.
- **OPDS catalogs over LAN / loopback** — user-added catalogs are trusted so cover images render correctly from LAN / loopback hosts; UA now uses a Mozilla-prefixed string accepted by legitimate catalog servers.
- **OPDS preset URL hygiene** — broken / unreachable presets pruned, working ones (Feedbooks, ManyBooks) restored once verified end-to-end.
- **MOBI hardening** — cache memory cap honored, OPDS cover MIME tightened to webp, MSVC build fixed by casting `MOBIFiletype` enum tail through `u32`, word-count error mapping corrected.
- **Library multi-select state visibility** — selection mode now shows clearly; missing i18n key added; series sections refresh live after edits.
- **Settings server status sync** — server status refreshes on focus and the checkbox state syncs back on a failed start.
- **Library file migration warning** — opting out of file migration when changing the library folder now warns the user before proceeding.
- **EPUB inline image keys disambiguated** — inline images from different EPUBs no longer collide in the cache; keys now hash the resolved zip path.

## [1.4.1] - 2026-04-15

### Added
- **Tag filter in library toolbar** — searchable multi-select combobox to filter books by tags. Select one or more tags; books must have all selected tags to appear (AND logic). Selection persists to localStorage.
- **Chip-on-comma tag input** — in the Edit Book dialog, typing a comma immediately creates a tag. Pressing Enter also works. Clicking Save commits any pending tag text before saving metadata. Supports comma-separated batch input (e.g., "japan, manga" creates two tags).
- **Eager tag loading** — tags and book-tag associations are loaded alongside the library for instant client-side filtering.

### Fixed
- **Tags not saving in Edit Book dialog** — tags typed in the input were silently lost because the Save button didn't commit pending tag text. Only pressing Enter (with no visual cue) would save tags.
- **Web server deadlock on auto-start** — the auto-start code held the `web_server_handle` mutex while calling `rebuild_tray_menu`, which also locks the same mutex. Since `std::sync::Mutex` is not reentrant, this deadlocked on every app launch with web server enabled, making all web server IPC calls (status, start, stop) hang forever.
- **System tray responsiveness on macOS** — window close now minimizes instead of hiding, keeping the macOS event loop alive so the tray menu stays responsive. Added `ExitRequested` handler to prevent auto-exit when autostart and tray are enabled. Tray "Show" recreates the window if destroyed.

## [1.4.0] - 2026-04-11

### Added
- **Remote Access (Web Server)** — browse and read your library from any device on the local network. Embeds an HTTP server with PIN authentication, JSON API, OPDS catalog, and a built-in web UI. See `docs/WEB_SERVER_API.md` for full documentation.
  - JSON REST API for books, covers, chapters, pages, downloads, collections
  - OPDS Atom XML catalog (compatible with KOReader, Calibre, Moon+ Reader)
  - Embedded web UI (login, responsive book grid, EPUB/PDF/comic reader)
  - PIN-based auth with OS keychain storage, session tokens, HTTP Basic Auth for OPDS
  - Rate limiting on login (5 attempts / 5 min per IP)
  - QR code for easy mobile access
  - Auto-start on app launch if previously enabled
  - Graceful shutdown when app closes
  - Settings panel with PIN, port, start/stop toggle, URL + QR display
- Security headers on all web server responses (CSP, X-Frame-Options, X-Content-Type-Options)
- EPUB HTML sanitization for web serving (ammonia, prevents XSS)
- Path traversal protection on image endpoints
- Streamed file downloads (no memory exhaustion on large files)
- OPDS pagination (50 books per page)
- **Bulk book actions** — select multiple books in the library grid, then delete in bulk. Selection mode with select all/deselect all.
- **Unified toast notifications** — consistent bottom-center toast system replacing ad-hoc notification patterns. Auto-dismiss with pause-on-hover.
- **Screen reader live regions** — aria-live announcements for chapter changes, bookmark confirmations, and import progress.
- **Database migration versioning** — schema_version table tracks applied migrations for safe future schema changes.
- **PDF cache memory limits** — LRU cache now evicts by total memory (200 MB cap) in addition to entry count.
- **Bounded background threads** — background operations (enrichment, backup, sync) use tokio's bounded thread pool instead of unbounded OS threads.
- **Highlight popup smart positioning** — color picker popup detects both top and bottom viewport edges to avoid clipping.
- **User-created themes (#48)** — save, name, load, rename, and delete custom visual themes. Each theme captures color tokens, font family, font size, and typography settings. Settings panel restructured: typography controls merged under Appearance accordion. Up to 50 saved themes with full validation and case-insensitive naming.
- **Web server favicon** — Folio app icon served as favicon on the web UI.
- **Accordion animation** — settings panel accordions now animate open/close with smooth height transitions.
- **Accordion content panels** — subtle background on expanded accordion sections for better visual separation.

## [1.3.0] - 2026-04-02

### Added
- **Comic page cache (CBZ/CBR)** — pages are extracted to a disk cache on first open. Subsequent page loads read from disk (~1-5ms vs ~50-500ms from archive). Three-layer eviction: LRU by book count (5), configurable size cap (default 500 MB), age expiry (7 days). Manage in Settings > Library.
- **PDF text search** — Cmd/Ctrl+F now works in PDFs using pdfium text extraction, with the same search UI as EPUB (snippets, click-to-navigate, match highlighting).
- **Page turn animations** — optional slide animation when turning pages in PDF/CBZ/CBR. Configurable in Settings > Page Layout. Adjacent pages preloaded in background for smooth transitions.
- **Page load timeout with retry** — pages that take too long show a "taking longer than usual" hint at 8s, with a retry button at 30s. Retry is often instant since background rendering continues and caches the result.
- **Loading skeleton placeholders** — library grid shows shimmer skeletons while books load, replacing the blank loading state.
- **Provider priority ordering** — drag enrichment providers up/down in Settings to control priority order.
- **Comic Vine enrichment provider** — comprehensive comics metadata (American, European, manga). Requires free API key.
- **BnF (Bibliothèque nationale de France) enrichment provider** — excellent coverage for French editions via SRU API, no key needed.
- **Linked books** — option to reference books at their original location without copying. Link badge on cards, source filter, "Copy to library" action in edit dialog.
- **Library cleanup** — Settings > Library > "Check for missing files" scans for broken entries and removes them with automatic backup.
- **Backup restore picker** — restore from automated backups via dropdown or manual backup via file picker.
- **Multi-language support (i18n)** — English and French translations across all components, with flag dropdown language switcher.
- **Diagnostic page logging** — enable with `FOLIO_DEBUG_PAGES=1` (backend) or `localStorage.setItem("folio-debug-pages", "1")` (frontend) for page load pipeline debugging.
- **Route transition animation** — subtle fade + slide-up when navigating between Library and Reader.
- **Empty state entrance animation** — staggered book stack pop-in when library is empty.
- **Progress bar fill animation** — BookCard progress bars animate from zero on mount.
- **Catalog loading spinner** — spinner overlay when browsing to an OPDS catalog.

### Changed
- **SFTP backup provider** — added alongside existing S3 and FTP providers.
- **Backup progress** — real-time step and file count reporting during backup.
- **Context-aware library sections** — "Continue Reading" and "Discover" hidden when viewing a collection or series.
- **Sharp comic zoom** — physical DOM resizing instead of CSS scale for sharp images at any zoom level.
- **PDF rendering** — JPEG encoding (quality 90) for faster page loads and smaller transfers.

### Fixed
- **In-flight request deduplication** — concurrent page requests for the same page share a single IPC invoke, preventing pdfium render queue buildup.
- **Preload debounce** — adjacent page preloads wait 500ms to prevent queue buildup during fast navigation.
- **Consistent page turn animation** — spread div stays mounted during loading so animation plays for both cached and uncached pages.
- **Backdrop blur standardized** — all 16 modal/panel overlays now use consistent `backdrop-blur-sm`.
- **Button radius standardized** — main action buttons unified to `rounded-xl`.
- **SVG icon strokes normalized** — strokeWidth 1.75/2.5 → 2, icon sizes 17×17 → 18×18 across 7 files.
- **BookmarkToast colors** — replaced hardcoded blue with design system accent tokens.
- **Form input focus glow** — subtle accent ring on focus for better visibility.
- **Library filter focus contrast** — upgraded from `border-accent/40` to full `border-accent`.
- Highlight popup smart positioning (viewport-aware clamping).
- Search results navigation with match counter and prev/next arrows.
- Archive decompression limits (zip bomb protection for EPUB/CBZ/CBR).
- Transaction boundaries for book import (prevents orphaned files on DB failure).
- Backup secret atomicity (keychain errors now propagated instead of silently ignored).
- OPDS URL resolution via RFC-compliant `url::Url::join()`.
- Activity log pruning combined count+age query.
- Scroll-to-match for in-book search results.
- CBR archive validation (entry count and size limits).
- PDF search result caching for faster repeated searches.

### Security
- Archive decompression limits: max 10,000 entries, 100 MB per entry for EPUB/CBZ/CBR.
- Backup secret atomicity: keychain write failures now return errors instead of creating config/secret desync.
- OPDS URL resolution hardened against protocol-relative URL injection.

## [1.2.0] - 2026-03-28

### Added
- **Dual-page spread / Manga mode** — side-by-side two-page view for all formats (CBZ, CBR, PDF, EPUB). Cover page displayed solo, subsequent pages paired. Manga mode swaps page order and arrow key direction for RTL reading. Toggle in reader header and Settings > Page Layout.
- **Series grouping** — books with series metadata are automatically grouped in the sidebar and via a "Series" sort option in the library grid, sorted by volume.
- **Custom user fonts** — import TTF/OTF/WOFF2 font files via Settings. Custom fonts appear alongside built-in options in the font picker.
- **Literata font** — added as a built-in reading font (designed by Google for e-reading).
- **Bookmark naming & editing** — name bookmarks via an expanding toast after creation (`B` key), or edit names inline in the bookmarks panel.

### Changed
- **Settings panel reorganized** — grouped into fewer accordions: Appearance (theme + custom CSS), Text & Typography (font size + font + line height/margins/etc.), Page Layout (paginated/continuous + dual-page + manga).

### Fixed
- Clipboard copy and JSON export for collection sharing
- Page-based bookmark progress calculation for CBZ/CBR/PDF

## [1.1.0] - 2026-03-26

### Added
- **CBR format support** — RAR-based comic book archives
- **PDF support** — page-by-page rendering via bundled pdfium
- **CBZ cover extraction** — first page used as cover thumbnail
- **Page viewer** — unified component for PDF/CBZ/CBR with zoom (0.5×–4×), pan, and keyboard/mouse wheel navigation
- **Collections** — manual and automated collections with sidebar, drag-and-drop, custom icons and colors, export as Markdown/JSON
- **Sort & filter** — sort by date added, title, author, last read, progress, rating, format; filter by format, status, rating
- **Tags** — freeform labels with autocomplete
- **Highlights & annotations** — inline text highlighting (5 colors) with notes, export as Markdown
- **Book metadata editing** — edit title, author, cover, series, language, publisher, year, tags
- **Keyboard shortcuts** — library and reader shortcuts with `?` help overlay
- **Focus mode** — hide all UI chrome with `D`, edge-reveal controls, auto-hide cursor
- **Page zoom** — Ctrl+scroll or Cmd+/- to zoom, pan when zoomed, reset on page change
- **Mouse wheel navigation** — scroll to turn pages in PDF/CBZ/CBR (300ms debounce)
- **Copy-on-import** — books copied into managed library folder with configurable path
- **Multi-file import** — bulk file picker with progress indicator
- **Bulk folder import** — recursive scan for supported formats
- **Remote file import** — import from URL (direct download)
- **OPDS catalog browsing** — browse Project Gutenberg, Standard Ebooks, and custom OPDS catalogs with search, navigation, and one-click download
- **Library export/backup** — metadata-only or full backup as ZIP, import from backup
- **Remote backup** — incremental sync to S3 and FTP via OpenDAL
- **Reading stats dashboard** — time spent reading, pages/chapters per day, books finished, reading streaks, 30-day bar chart
- **OpenLibrary integration** — pull descriptions, genres, ratings; auto-match by title+author
- **Auto-enrichment** — ISBN lookup, title+author search, filename parsing, background scan queue with progress and cancel
- **Multi-provider enrichment** — EnrichmentProvider trait, Google Books API provider, provider settings in Settings
- **ComicInfo.xml parsing** — extract metadata from CBZ comic archives
- **Recently opened** — top 5 most recently read books shown at library top
- **Share collections** — export as Markdown or JSON
- **Book recommendations** — Discover section with popular books from configured OPDS catalogs
- **Multiple profiles** — separate libraries, each with own database, library folder, and settings
- **Sepia theme** — warm parchment preset alongside light and dark
- **Custom color themes** — pick background + text color, auto-derive remaining tokens
- **OpenDyslexic font** — bundled accessibility font with weighted letterforms
- **Star ratings** — 1-5 star rating per book, sort and filter by rating
- **Full-text search** — Cmd/Ctrl+F to search EPUB content with highlighted matches
- **Advanced typography** — line height, page margins, text alignment, paragraph spacing, hyphenation
- **Custom CSS override** — inject CSS into EPUB rendering
- **Continuous scroll mode** — all EPUB chapters in one scrollable document
- **Estimated time to finish** — WPM-based reading time estimate in EPUB reader footer
- **Activity log** — persistent log of all data-changing operations, filterable in Settings

### Fixed
- Path traversal prevention in cover image extraction
- Cover image extension allowlisting
- DOMPurify removed (redundant with ammonia backend sanitization)
- Bookmarks table index for query performance
- Chapter index and scroll position validation
- Scroll restoration tied to specific chapter to prevent race conditions
- Keyboard handler conflicts between reader and panels
- Focus outlines and disabled button contrast (accessibility)
- User-friendly error messages for backend failures
- Book file existence validation before reading
- Loading overlay during import to prevent race conditions
- Focus trap and ARIA attributes on TOC sidebar
- Font size slider accessibility (aria-valuetext)
- Base64 image encoding replaced with asset protocol to prevent memory issues
- EPUB zip archive caching to avoid reopening on every page turn
- DB connection pool size and timeout configuration
- Book import timeout/size guard

## [1.0.0] - 2026-03-25

### Added
- EPUB 2 & 3 import via file picker and drag-and-drop (Tauri v2 native events)
- Library screen with book grid, cover art, reading progress indicator
- Search/filter books by title or author
- Remove books from library with confirmation
- Reader screen with chapter navigation (buttons + keyboard shortcuts)
- Table of Contents sidebar
- Reading progress auto-saved to SQLite and restored on reopen
- Light / dark theme toggle with system preference detection
- Adjustable font size (14–24px) and font family (serif/sans-serif)
- XSS sanitization of EPUB HTML via `ammonia`
- Duplicate EPUB detection (UNIQUE constraint on file path)
- GitHub Actions CI/CD: lint, test, cross-platform release builds
