(function() {
  "use strict";
  const $ = (s) => document.querySelector(s);
  const $$ = (s) => document.querySelectorAll(s);
  const app = () => $("#app");

  // R3-4: Use httpOnly cookies only — no localStorage token storage
  let authenticated = false;

  // Item 7: URL (location.hash) is the source of truth for library state —
  // these are a parse cache of the current hash, refreshed by route() on
  // every navigation (see parseLibraryParams/showLibrary). Code that wants to
  // change the filter/search/sort must build a new hash (libraryHash()) and
  // navigate()/history.replaceState to it; never mutate these directly and
  // expect the view to follow.
  const DEFAULT_SORT = "date_added";
  let activeCollectionId = null;
  let activeSeries = null;
  let activeQuery = "";
  let activeSort = DEFAULT_SORT;
  let activeWantToRead = false;

  // Item 6: theme mode is "light" | "dark" | "system", persisted to
  // localStorage. "system" means no data-theme attribute is set at all —
  // the CSS `@media (prefers-color-scheme: dark)` block then governs the
  // palette and updates live on its own when the OS preference changes, no
  // JS re-render needed. The index.html bootstrap script applies the same
  // stored value before first paint to avoid a flash of the wrong theme.
  const THEME_STORAGE_KEY = "carrel_theme";

  // Finding 1: a bare localStorage.getItem/setItem call can throw
  // (SecurityError) under some browser configurations (e.g. Chrome "block
  // all cookies") — unguarded, that would abort this whole IIFE and
  // permanently blank the page. Mirrors the try/catch already used by
  // index.html's bootstrap script; every localStorage call site in this file
  // goes through these two helpers.
  function safeStorageGet(key) {
    try { return localStorage.getItem(key); } catch (e) { return null; }
  }
  function safeStorageSet(key, value) {
    try { localStorage.setItem(key, value); } catch (e) { /* best-effort */ }
  }

  // Web reader typography (font size / line spacing / font family / column
  // width). One JSON localStorage key, global across books, applied as inline
  // styles on #reader-content for reflowable books (mode !== "page"). Held in
  // an in-memory object so a change still applies for the session even when
  // the persist is denied (private mode / quota). Values are validated on the
  // way in — clamp to range THEN snap to the control's step grid.
  const TYPO_KEY = "carrel-web-typography";
  const TYPO_DEFAULTS = { fontSize: 18, lineHeight: 1.8, fontFamily: "lora", columnWidth: 700 };
  const FONT_STACKS = {
    lora: '"Lora Variable", Georgia, serif',
    literata: '"Literata Variable", Georgia, serif',
    "dm-sans": '"DM Sans Variable", system-ui, sans-serif',
    opendyslexic: '"OpenDyslexic", sans-serif',
  };
  const COLUMN_WIDTHS = [600, 700, 860];
  const okNum = (n) => typeof n === "number" && Number.isFinite(n);

  function validateTypography(raw) {
    const o = (raw && typeof raw === "object" && !Array.isArray(raw)) ? raw : {};
    // font size: clamp to [14,24] then snap to even
    let fs = okNum(o.fontSize) ? Math.min(24, Math.max(14, o.fontSize)) : TYPO_DEFAULTS.fontSize;
    fs = Math.round(fs / 2) * 2;
    // line height: clamp to [1.2,2.4] then snap to the 0.2 grid
    let lh = okNum(o.lineHeight) ? Math.min(2.4, Math.max(1.2, o.lineHeight)) : TYPO_DEFAULTS.lineHeight;
    lh = Math.round(lh * 5) / 5;
    return {
      fontSize: fs,
      lineHeight: Math.round(lh * 10) / 10,
      fontFamily: Object.prototype.hasOwnProperty.call(FONT_STACKS, o.fontFamily)
        ? o.fontFamily
        : TYPO_DEFAULTS.fontFamily,
      columnWidth: COLUMN_WIDTHS.includes(o.columnWidth) ? o.columnWidth : TYPO_DEFAULTS.columnWidth,
    };
  }

  // In-memory source of truth; initialized once from storage on first read.
  let typoState = null;
  function getTypography() {
    if (!typoState) {
      let parsed = null;
      try { parsed = JSON.parse(safeStorageGet(TYPO_KEY)); } catch (e) { parsed = null; }
      typoState = validateTypography(parsed);
    }
    return typoState;
  }
  function setTypography(patch) {
    typoState = validateTypography({ ...getTypography(), ...patch }); // live regardless of storage
    safeStorageSet(TYPO_KEY, JSON.stringify(typoState));              // best-effort persist
    return typoState;
  }
  // Apply the current typography to a reflowable chapter column as inline
  // styles. Inline styles on #reader-content survive renderReaderContent()'s
  // innerHTML swaps (only the children are replaced), so a single apply at
  // chrome-build time persists across chapter turns.
  function applyTypography(el) {
    if (!el) return;
    const t = getTypography();
    el.style.fontSize = t.fontSize + "px";
    el.style.lineHeight = String(t.lineHeight);
    el.style.maxWidth = t.columnWidth + "px";
    el.style.fontFamily = FONT_STACKS[t.fontFamily];
  }
  // Reading-position preservation across a typography reflow. Changing font
  // size/spacing/family/width reflows the chapter, which would otherwise shift
  // whatever the reader was looking at. We keep the paragraph at the top of the
  // scroll viewport pinned to its offset, measured against the SCROLL CONTAINER
  // (#reader-stage), not #reader-content (the non-scrolling inner column).
  // A unique token identifying the reanchor currently allowed to fire. Each
  // schedule (a typography change OR the initial-load font restore) overwrites
  // it, so an older deferred callback finds its token superseded and bails; a
  // genuine user scroll sets it to null, cancelling any pending reanchor.
  let pendingReanchor = null;

  // Whether the Aa typography popover is open. Reset on every chrome (re)build
  // and on chrome-hide so it can't outlive its DOM.
  let typoPanelOpen = false;
  // Whether the table-of-contents panel is open. Same lifecycle as above.
  let tocPanelOpen = false;
  // Whether the bookmarks drawer is open. Same lifecycle as above.
  let bookmarkPanelOpen = false;
  // Whether the highlights drawer is open. Same lifecycle as above.
  let hlPanelOpen = false;
  // Whether the saved-words (vocabulary) drawer is open. Same lifecycle as
  // above, but this one ALSO closes on ordinary navigation (route change),
  // not just chrome rebuild — see the route() backstop.
  let vocabPanelOpen = false;
  // True while an inline note editor is open in the highlights drawer —
  // opportunistic re-renders must skip while set (see bookmarkRenaming).
  let hlNoteEditing = false;
  // True while an inline rename input is focused/editing — opportunistic
  // re-renders (a late loadBookmarks/loadToc completion) must skip while this
  // is set, or they'd rebuild the list innerHTML and destroy the field mid-edit.
  let bookmarkRenaming = false;
  // Bookmarks for the current book: { bookId, loaded, items }. Fetched lazily
  // on first panel open; cleared on every chrome (re)build.
  let bookmarksState = null;
  // Saved words (vocabulary) for the current book: { bookId, items, loaded,
  // failed }. Fetched lazily on first panel open (bookmark precedent), and
  // re-fetched on every open (unlike bookmarks/highlights, a word can be
  // saved from the Define popover elsewhere while the drawer sits unopened,
  // so there is no "already loaded" short-circuit). Reset on every chrome
  // (re)build.
  let vocabState = { bookId: null, items: [], loaded: false, failed: false };

  const TYPO_FAMILY_ORDER = ["lora", "literata", "dm-sans", "opendyslexic"];
  const TYPO_FAMILY_LABELS = {
    lora: "Lora",
    literata: "Literata",
    "dm-sans": "DM Sans",
    opendyslexic: "OpenDyslexic",
  };
  const TYPO_WIDTH_LABELS = { 600: "Narrow", 700: "Medium", 860: "Wide" };
  const TYPO_FS_MIN = 14, TYPO_FS_MAX = 24, TYPO_FS_STEP = 2;
  const TYPO_LH_MIN = 1.2, TYPO_LH_MAX = 2.4, TYPO_LH_STEP = 0.2;

  function captureAnchor(stage, content) {
    const stageTop = stage.getBoundingClientRect().top;
    const kids = content.children;
    for (let i = 0; i < kids.length; i++) {
      const r = kids[i].getBoundingClientRect();
      if (r.bottom > stageTop) return { el: kids[i], offset: r.top - stageTop };
    }
    const denom = stage.scrollHeight - stage.clientHeight;
    return { ratio: denom > 0 ? stage.scrollTop / denom : 0 };
  }

  // Arm suppressScrollSave ONLY when the write actually changes scrollTop —
  // otherwise no scroll event fires and the stale flag would swallow the user's
  // next real scroll. Clamp the target to the real scrollable range FIRST so a
  // request the browser would clamp to the current position (e.g. an anchor
  // overshoot after content shrinks near the bottom) doesn't arm the flag with
  // no scroll event to consume it.
  function setScrollTop(stage, target) {
    const max = Math.max(0, stage.scrollHeight - stage.clientHeight);
    const next = Math.round(Math.min(Math.max(target, 0), max));
    if (next === Math.round(stage.scrollTop)) return;
    readerState.suppressScrollSave = true; // one-shot, consumed by the listener
    stage.scrollTop = next;
  }

  // Re-derive the saved progress ratio from the current scroll offset. A
  // reflow (typography change) keeps the reader at the same paragraph but
  // changes chapter height, so the pre-reflow ratio no longer points there —
  // recompute it, or a later nav-away flush would persist a stale location.
  function syncScrollPosition(stage) {
    if (!readerState) return;
    const max = stage.scrollHeight - stage.clientHeight;
    readerState.scrollPosition = max > 0 ? clampScrollRatio(stage.scrollTop / max) : 0;
  }

  function restoreAnchor(stage, content, a) {
    if (a.el && content.contains(a.el)) {
      const stageTop = stage.getBoundingClientRect().top;
      const cur = a.el.getBoundingClientRect().top - stageTop;
      setScrollTop(stage, stage.scrollTop + (cur - a.offset));
    } else if (typeof a.ratio === "number") {
      setScrollTop(stage, a.ratio * (stage.scrollHeight - stage.clientHeight));
    }
  }

  function changeTypography(patch) {
    const stage = $("#reader-stage");
    const content = $("#reader-content");
    if (!stage || !content) { setTypography(patch); return; }
    const gen = readerState.renderGen;               // reader's own render generation
    const anchor = captureAnchor(stage, content);
    setTypography(patch);
    applyTypography(content);
    restoreAnchor(stage, content, anchor);           // after synchronous layout
    syncScrollPosition(stage);                        // saved ratio now matches the reflowed layout
    const establishedTop = Math.round(stage.scrollTop);
    const token = (pendingReanchor = {});            // unique per change — a newer change/user scroll supersedes
    document.fonts.ready.then(() => {
      // bail if: superseded by a newer schedule or a user scroll (token no
      // longer ours), the reader re-rendered/changed book (renderGen bumped),
      // it left chapter mode, or the content left the DOM.
      if (pendingReanchor !== token) return;
      if (!readerState || readerState.renderGen !== gen || readerState.mode !== "chapter" || !content.isConnected) { pendingReanchor = null; return; }
      // Only re-anchor if the offset is still exactly where we left it. A pure
      // font-metric reflow leaves scrollTop numerically unchanged (paragraph
      // drifts, we correct it); a genuine user scroll OR the browser's own
      // scroll-anchoring moves it — in either case, don't fight it. This is
      // robust even if a coalesced scroll event slipped past suppressScrollSave.
      if (Math.round(stage.scrollTop) !== establishedTop) { pendingReanchor = null; return; }
      restoreAnchor(stage, content, anchor);
      syncScrollPosition(stage);
      pendingReanchor = null;
    });
  }
  // Test-only hook: app.js is an IIFE, so nothing is reachable from
  // Playwright's page.evaluate unless explicitly exposed. app.js is served
  // byte-identically to production and the e2e harness, so this is gated on a
  // flag the specs set via addInitScript before load — it never ships to the
  // production reader (whose M3 controls call changeTypography directly).
  if (window.__carrelExposeTypoHook) {
    window.__carrelTypo = { validate: validateTypography, get: getTypography, set: setTypography, change: changeTypography };
  }

  // Item 10: same guarded pattern as safeStorageGet/Set, but backed by
  // sessionStorage — used for the per-hash library scroll-position memory
  // (tab-scoped, meant to be forgotten across sessions).
  function safeSessionGet(key) {
    try { return sessionStorage.getItem(key); } catch (e) { return null; }
  }
  function safeSessionSet(key, value) {
    try { sessionStorage.setItem(key, value); } catch (e) { /* best-effort */ }
  }

  // ── Offline mode: core storage ─────────────────
  // Spec: docs/superpowers/specs/2026-07-17-web-reader-offline-design.md.
  // Feature-gated on the full secure-context toolchain (service worker +
  // Cache Storage + IndexedDB + crypto.subtle). On plain-HTTP LAN none of
  // these register/exist, every offline affordance stays hidden, and
  // behavior is exactly the pre-offline app.
  const OFFLINE_PAGE_WIDTH = 1080; // page-image download width; change here (e.g. 1600)
  const OFFLINE_CACHE_PREFIX = "carrel-offline-book-"; // must mirror sw.js
  const OFFLINE_MANIFEST_VERSION = 1;
  // Profile scoping: book ids are per-profile DB id spaces, so a cache or
  // manifest keyed by bare book id can serve profile A's saved content for a
  // *different* book with the same id in profile B. Everything offline is
  // therefore namespaced by a short hash of the active profile name.
  //
  // The active profile's token also has to reach the service worker, which
  // does the offline serving and has no access to the page's variables. It's
  // published in a one-entry Cache Storage entry (readable from both contexts;
  // localStorage is not) and pushed eagerly by postMessage on change. Both
  // constants must mirror sw.js.
  const OFFLINE_SCOPE_CACHE = "carrel-offline-scope";
  const OFFLINE_SCOPE_URL = "/__offline_scope";
  // The page's own copy of the scope. localStorage rather than the marker cache
  // because it is synchronous, survives a cold offline launch, and isn't subject
  // to Cache Storage quota failures — the marker is strictly the worker's copy.
  const OFFLINE_SCOPE_STORAGE_KEY = "carrel-offline-scope";
  // The default profile keeps the unscoped names offline mode shipped with, so
  // existing downloads survive this change; every other profile gets a
  // 12-hex-char prefix. A bare book id can never be mistaken for a scoped one:
  // ids are UUIDs (8 hex chars then `-`), never 12.
  let offlineScope = "";
  // Set when the worker's scope marker could not be published. The page and the
  // worker would then disagree about which profile's caches are current, and the
  // worker is the one serving — so offline is switched off for the session
  // instead of risking one profile's content under another's book ids.
  let offlineUnavailable = false;
  // Set when the active profile could not be established, so the namespace
  // every offline read and write would use is unconfirmed. Unlike
  // `offlineUnavailable` this is cleared the moment a profile request succeeds
  // — a transient failure must not cost this document its offline mode.
  let offlineProfileUnknown = false;

  function offlineSupported() {
    return !offlineUnavailable &&
      !offlineProfileUnknown &&
      "serviceWorker" in navigator &&
      !!window.indexedDB &&
      !!window.caches &&
      !!(window.crypto && window.crypto.subtle);
  }

  function offlineCacheName(id) { return OFFLINE_CACHE_PREFIX + offlineScope + id; }

  /// Scope token for a profile: "" for `default` (legacy names), else the
  /// first 12 hex chars of its SHA-256 plus a separator. Hashed rather than
  /// used verbatim because profile names are arbitrary text (only "non-empty"
  /// and "not default" are enforced), so a raw name could make two different
  /// (profile, id) pairs produce one cache name.
  async function profileScopeToken(profileName) {
    if (!profileName || profileName === "default") return "";
    const buf = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(profileName));
    return Array.from(new Uint8Array(buf)).slice(0, 6).map((b) => b.toString(16).padStart(2, "0")).join("") + "-";
  }

  /// The scope published for the service worker, or "" if none is stored yet.
  async function readPublishedScope() {
    try {
      const resp = await (await caches.open(OFFLINE_SCOPE_CACHE)).match(OFFLINE_SCOPE_URL);
      return resp ? await resp.text() : "";
    } catch (e) { return ""; }
  }

  /// Point every offline read/write at `profileName`'s namespace. Returns
  /// whether the scope actually changed, so callers can re-derive the state
  /// they read from the old one.
  ///
  /// Publishing to the marker cache (for a cold-started SW) and postMessage
  /// (for a running one) are both needed: a SW can start after this page did,
  /// and a running SW never re-reads the marker on its own.
  async function applyOfflineScope(profileName) {
    if (!offlineSupported()) return false;
    let token;
    try { token = await profileScopeToken(profileName); } catch (e) { return false; }

    // Publish to the worker BEFORE adopting the scope here. The worker does the
    // offline serving and reads only the marker, so a page that moved first
    // would be reading profile B's manifests while the worker answered out of
    // profile A's caches. Republished even when this page's token is unchanged:
    // a worker can outlive a switch, or have been installed by another tab.
    //
    // Deliberately does NOT wait on `navigator.serviceWorker.ready`: that only
    // settles once a worker is active, which on a first visit means after the
    // whole shell + font precache, and boot must never block on it (an earlier
    // version did, and stalled the app until the install finished).
    try {
      await (await caches.open(OFFLINE_SCOPE_CACHE)).put(OFFLINE_SCOPE_URL, new Response(token));
    } catch (e) {
      // Can't publish, so the two sides cannot be kept in agreement. Fail
      // closed: drop the marker so the worker refuses to serve any offline
      // content at all (it treats "no marker" as "namespace unknown"), and
      // disable offline for this session. Better to lose the offline copies
      // until the next load than to serve one profile's book as another's.
      try { await caches.delete(OFFLINE_SCOPE_CACHE); } catch (err) { /* nothing left to try */ }
      offlineUnavailable = true;
      return false;
    }

    const changed = token !== offlineScope;
    if (changed) {
      offlineScope = token;
      safeStorageSet(OFFLINE_SCOPE_STORAGE_KEY, token);
      // Drop the connection to the previous profile's database; the next
      // offlineDb() opens the new one.
      if (offlineDbPromise) {
        offlineDbPromise.then((db) => db.close()).catch(() => {});
        offlineDbPromise = null;
      }
    }
    return changed;
  }

  /// Adopt the last known scope before any offline read. Used on boot,
  /// including an offline launch where the active profile can't be fetched.
  ///
  /// Read from localStorage, not the marker: this must work on a cold offline
  /// launch and must not depend on a Cache Storage write having succeeded. The
  /// marker is then re-published from it so a worker that has been evicted (or
  /// was installed by another tab) agrees with this page — and if that can't be
  /// done, offline goes off rather than out of sync.
  async function loadPublishedOfflineScope() {
    if (!offlineSupported()) return;
    const stored = safeStorageGet(OFFLINE_SCOPE_STORAGE_KEY);
    offlineScope = stored === null ? await readPublishedScope() : stored;
    try {
      await (await caches.open(OFFLINE_SCOPE_CACHE)).put(
        OFFLINE_SCOPE_URL,
        new Response(offlineScope)
      );
    } catch (e) {
      try { await caches.delete(OFFLINE_SCOPE_CACHE); } catch (err) { /* nothing left to try */ }
      offlineUnavailable = true;
    }
  }

  /// Re-scope to the server's active profile. Returns "changed", "unchanged",
  /// or "unknown".
  ///
  /// "unknown" is the case a boolean could not express: loadProfiles() swallows
  /// a failed or non-OK /api/profiles and returns whatever it had cached, and
  /// applyOfflineScope() reports its own publish failures as false too — so a
  /// plain false meant either "nothing to do" or "no idea which profile is
  /// active", and a caller about to permit offline *writes* needs to tell those
  /// apart.
  async function syncOfflineScopeWithServer() {
    const active = (await loadProfiles()).find((p) => p.active);
    // A stale cache can name an active profile that moved since it was built,
    // so the outcome — not the list — decides whether this is knowledge.
    if (profilesOutcome !== "ok" || !active) return "unknown";
    // The profile is known again: lift a previous "unknown" (a transient
    // /api/profiles failure must not disable offline for the whole document's
    // life). Deliberately does NOT touch `offlineUnavailable`, which means the
    // page and the worker could not be kept in agreement about the namespace —
    // that one is only recoverable by a reload.
    offlineProfileUnknown = false;
    if (!offlineSupported()) return "unchanged"; // nothing to scope
    // applyOfflineScope reports a failed marker publish as false as well, but
    // it sets offlineUnavailable when it does, which disables offline entirely
    // — so a false here is either "already correct" or "offline is now off",
    // and both are safe to call unchanged.
    return (await applyOfflineScope(active.name)) ? "changed" : "unchanged";
  }

  // Lazy singleton connection; a failed open clears the promise so a later
  // call can retry (e.g. transient quota pressure at first open). The open
  // is raced against a timeout: some browsers' IndexedDB can hang without
  // ever firing success OR error (Safari lazy-init/private-mode bugs), and
  // the library/detail render paths await reads on this — a never-settling
  // open must degrade to "no offline features", never a blank UI.
  let offlineDbPromise = null;
  function offlineDb() {
    if (!offlineDbPromise) {
      // `settled` guards every side effect below so a late timeout or a late
      // request event can't null out (or otherwise disturb) a promise that
      // has already resolved/rejected — including a NEWER singleton created
      // by a subsequent offlineDb() call after this one was cleared.
      let settled = false;
      let timer = null;
      const thisPromise = new Promise((resolve, reject) => {
        const clear = () => { if (timer !== null) { clearTimeout(timer); timer = null; } };
        // Scoped per profile (see `offlineScope`): a separate database per
        // profile keeps every store's keys — manifests, pending saves, queued
        // progress — in their own id space without rewriting each key.
        const req = indexedDB.open(offlineScope ? "carrel-offline-" + offlineScope.slice(0, -1) : "carrel-offline", 1);
        req.onupgradeneeded = () => {
          const db = req.result;
          if (!db.objectStoreNames.contains("books")) db.createObjectStore("books", { keyPath: "id" });
          if (!db.objectStoreNames.contains("pendingSaves")) db.createObjectStore("pendingSaves", { keyPath: "bookId" });
          if (!db.objectStoreNames.contains("progressQueue")) db.createObjectStore("progressQueue", { keyPath: "bookId" });
          if (!db.objectStoreNames.contains("meta")) db.createObjectStore("meta", { keyPath: "key" });
        };
        req.onsuccess = () => {
          if (settled) { req.result.close(); return; } // timed out already — don't leak this connection
          settled = true;
          clear();
          const db = req.result;
          // A future schema bump in another tab must not be blocked forever
          // by this connection — close so the upgrade can proceed; the next
          // offlineDb() call reopens at the new version. Only clear the
          // singleton if it's still THIS promise.
          db.onversionchange = () => { db.close(); if (offlineDbPromise === thisPromise) offlineDbPromise = null; };
          // The browser can force-close the connection (storage eviction,
          // site-data clear in another tab); drop the cached promise so the
          // next call reopens instead of throwing InvalidStateError forever.
          db.onclose = () => { if (offlineDbPromise === thisPromise) offlineDbPromise = null; };
          resolve(db);
        };
        req.onblocked = () => { /* an old tab holds the DB; the open resumes when it closes */ };
        req.onerror = () => {
          if (settled) return;
          settled = true;
          clear();
          if (offlineDbPromise === thisPromise) offlineDbPromise = null;
          reject(req.error);
        };
        timer = setTimeout(() => {
          if (settled) return;
          settled = true;
          if (offlineDbPromise === thisPromise) offlineDbPromise = null;
          reject(new Error("IndexedDB open timed out"));
        }, 5000);
      });
      offlineDbPromise = thisPromise;
      // A synchronous throw from indexedDB.open (e.g. SecurityError when
      // storage access is blocked) rejects thisPromise before any handler
      // above runs and would otherwise cache the rejection for the whole
      // session; null the singleton on any rejection so a later call retries.
      thisPromise.catch(() => { if (offlineDbPromise === thisPromise) offlineDbPromise = null; });
    }
    return offlineDbPromise;
  }

  function idbOp(storeName, mode, fn) {
    return offlineDb().then((db) => new Promise((resolve, reject) => {
      const tx = db.transaction(storeName, mode);
      const req = fn(tx.objectStore(storeName));
      let result;
      req.onsuccess = () => { result = req.result; };
      // Resolve on transaction commit, not request success — a readwrite
      // transaction can still abort (e.g. quota) after the request's
      // onsuccess, and the save flow's manifest-then-delete-pending ordering
      // must never act on an uncommitted write.
      tx.oncomplete = () => resolve(result);
      tx.onabort = () => reject(tx.error || new Error("IndexedDB transaction aborted"));
      tx.onerror = () => reject(tx.error);
    }));
  }
  function idbGet(store, key) { return idbOp(store, "readonly", (s) => s.get(key)); }
  function idbPut(store, value) { return idbOp(store, "readwrite", (s) => s.put(value)); }
  function idbDelete(store, key) { return idbOp(store, "readwrite", (s) => s.delete(key)); }
  function idbGetAll(store) { return idbOp(store, "readonly", (s) => s.getAll()); }

  // Canonical inventory hash: deduplicated, sorted, newline-joined URL set →
  // hex SHA-256. Save completion and the boot eviction check compare this
  // against the cache's actual key set, so a half-evicted (or key-
  // substituted) cache can never masquerade as a fully saved book.
  async function inventoryHash(urls) {
    const canonical = Array.from(new Set(urls)).sort().join("\n");
    const buf = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(canonical));
    return Array.from(new Uint8Array(buf)).map((b) => b.toString(16).padStart(2, "0")).join("");
  }

  // cache.keys() → the same canonical form (origin-relative pathname+search)
  // the inventory uses at save time.
  async function cachedUrlSet(cache) {
    const reqs = await cache.keys();
    return reqs.map((r) => { const u = new URL(r.url); return u.pathname + u.search; });
  }

  // ── Offline mode: save / unsave engine ─────────
  // A book is "saved" iff its manifest row exists in the `books` store; the
  // row is written last, after every inventory URL is cached and the cache's
  // key set hash-matches the inventory. A partial cache plus its
  // `pendingSaves` row is the resume state; retrying skips already-cached
  // URLs (safe because only response.ok bodies are ever cached).
  const activeOfflineSaves = {}; // id -> { cancelled: boolean }

  // Text bodies the save loop parses for follow-up URLs (TOC, chapters,
  // page-count) — single source for the classifier used in two places.
  const OFFLINE_TEXT_BODY_RE = /\/chapters(\/\d+)?$|\/page-count$/;
  function offlineUrlIsTextBody(url) {
    return OFFLINE_TEXT_BODY_RE.test(url.split("?")[0]);
  }

  function cancelOfflineSave(id) {
    if (activeOfflineSaves[id]) activeOfflineSaves[id].cancelled = true;
  }

  function getOfflineManifest(id) {
    return offlineSupported() ? idbGet("books", id).catch(() => undefined) : Promise.resolve(undefined);
  }
  function getAllOfflineManifests() {
    return offlineSupported() ? idbGetAll("books").catch(() => []) : Promise.resolve([]);
  }

  // Best-effort storage persistence: asked once, before the first save;
  // denial just raises eviction risk, which the boot integrity check covers.
  async function ensurePersistence() {
    try {
      if (!navigator.storage || !navigator.storage.persist) return;
      if (await idbGet("meta", "persistResult")) return;
      const granted = (await navigator.storage.persisted()) || (await navigator.storage.persist());
      await idbPut("meta", { key: "persistResult", value: granted });
    } catch (e) { /* best-effort */ }
  }

  // Fetch one inventory URL and cache it. Only response.ok bodies are ever
  // put — resume trusts existing entries on that invariant. A 401 aborts the
  // whole save (tagged so the UI can say "log in"); a 404 on an `optional`
  // URL (covers — many imports have none) is a skip, not a failure; other
  // failures retry ×2. Returns { bytes, text, skipped } — text only for
  // chapter/TOC/page-count bodies the save loop parses for follow-up URLs.
  async function offlineFetchIntoCache(cache, url, optional) {
    let lastErr;
    for (let attempt = 0; attempt < 3; attempt++) {
      try {
        const resp = await fetch(url, { credentials: "same-origin" });
        if (resp.status === 401) {
          const e = new Error("Session expired — log in and retry");
          e.auth = true;
          throw e;
        }
        if (optional && resp.status === 404) return { bytes: 0, text: null, skipped: true };
        if (!resp.ok) {
          // A non-ok HTTP response means the server's view of this book
          // changed (e.g. a re-import shrank the page count → 404) — the
          // resume state is genuinely stale, so tag it for self-heal. A
          // network reject (below) is transient and must NOT wipe progress.
          const e = new Error(`Server error (${resp.status})`);
          e.stale = true;
          throw e;
        }
        // One body materialization: buffer once, derive byte count and (for
        // TOC/chapter/page-count bodies) the parseable text from it, and put
        // a rebuilt response — never clone the stream three ways.
        const buf = await resp.arrayBuffer();
        const text = offlineUrlIsTextBody(url) ? new TextDecoder().decode(buf) : null;
        await cache.put(new Request(url), new Response(buf, {
          status: resp.status,
          statusText: resp.statusText,
          headers: resp.headers,
        }));
        return { bytes: buf.byteLength, text };
      } catch (e) {
        if (e.auth) throw e;
        // Preserve a stale classification across retries: a later transient
        // network reject must not mask an earlier "server changed" (non-ok
        // HTTP) signal, or the resume self-heal would be skipped for a run
        // that is genuinely stale.
        if (!lastErr || !lastErr.stale) lastErr = e;
      }
    }
    throw lastErr;
  }

  async function saveBookOffline(book, onProgress) {
    const id = book.id;
    const isChapterMode = book.format === "epub" || book.format === "mobi";
    // A chapter-mode book with an unknown chapter count (total_chapters = 0
    // is a legitimate "not known yet" state — see Finding G in the reader)
    // would pass the hash gate having cached zero chapters and publish a
    // phantom "saved" book that can't actually be read offline. Refuse.
    if (isChapterMode && !(book.total_chapters > 0)) {
      throw new Error("Chapter count unknown — open the book online once, then retry");
    }
    // Single-flight per book: a second concurrent save (navigate away and
    // back mid-save re-renders an enabled button) would orphan the first
    // run's cancellation marker and let a removed save resurrect its
    // manifest.
    if (activeOfflineSaves[id]) {
      const e = new Error("Already downloading");
      e.silent = true; // the running save owns the state; UI says nothing
      throw e;
    }
    const marker = { cancelled: false };
    activeOfflineSaves[id] = marker;
    let wasResume = false;
    let published = false;
    try {
      await ensurePersistence();

      // Resume the same generation if a pending row exists; otherwise start
      // fresh — and delete any stale cache first, so a re-save (e.g. after
      // OFFLINE_PAGE_WIDTH changed) can never leave old variants behind for
      // the SW's ignoreSearch page matching to trip over.
      let pending = await idbGet("pendingSaves", id);
      wasResume = !!pending;
      if (!pending) {
        await caches.delete(offlineCacheName(id));
        const seed = [`/api/books/${id}`, `/api/books/${id}/cover`, `/api/books/${id}/cover?size=thumb`];
        seed.push(isChapterMode ? `/api/books/${id}/chapters` : `/api/books/${id}/page-count`);
        pending = {
          bookId: id,
          generation: `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`,
          urls: seed,
          completed: 0,
        };
        await idbPut("pendingSaves", pending);
      }
      const cache = await caches.open(offlineCacheName(id));

      const inventory = new Set(pending.urls);
      const queue = Array.from(inventory);
      const addUrl = (u) => {
        if (!inventory.has(u)) { inventory.add(u); queue.push(u); }
      };
      let bytes = 0;
      let done = 0;

      while (queue.length) {
        if (marker.cancelled) {
          const e = new Error("Save cancelled");
          e.cancelled = true;
          throw e;
        }
        const url = queue.shift();
        // Covers are optional: a 404 (book imported without one) drops the
        // URL from the inventory instead of failing the save — the grid/
        // detail placeholders already handle the missing-cover render.
        const optional = /\/cover(\?|$)/.test(url);
        const cached = await cache.match(new Request(url));
        let text = null;
        if (cached) {
          const buf = await cached.clone().arrayBuffer();
          bytes += buf.byteLength;
          if (offlineUrlIsTextBody(url)) text = new TextDecoder().decode(buf);
        } else {
          const got = await offlineFetchIntoCache(cache, url, optional);
          if (got.skipped) {
            inventory.delete(url);
            continue;
          }
          bytes += got.bytes;
          text = got.text;
        }

        // Follow-up URL discovery.
        if (url === `/api/books/${id}/chapters`) {
          for (let i = 0; i < (book.total_chapters || 0); i++) addUrl(`/api/books/${id}/chapters/${i}`);
        } else if (/\/chapters\/\d+$/.test(url) && text != null) {
          chapterImageUrls(id, text).forEach(addUrl);
        } else if (/\/page-count$/.test(url) && text != null) {
          let count = 0;
          try { count = (JSON.parse(text) || {}).count || 0; } catch (e) { /* leave 0 */ }
          for (let i = 0; i < count; i++) addUrl(`/api/books/${id}/pages/${i}?width=${OFFLINE_PAGE_WIDTH}`);
        }

        done++;
        if (onProgress) onProgress({ done, total: inventory.size });
      }

      // Crash-safe resume state: persist the fully-grown inventory.
      pending.urls = Array.from(inventory);
      pending.completed = done;
      await idbPut("pendingSaves", pending);

      // Publication gate: the cached key set must hash-match the inventory,
      // and a cancellation (incl. one from removeBookOffline racing this
      // loop's last iteration) must never publish a manifest for a cache
      // that was just deleted.
      if (marker.cancelled) {
        const e = new Error("Save cancelled");
        e.cancelled = true;
        throw e;
      }
      const expectHash = await inventoryHash(inventory);
      const gotHash = await inventoryHash(await cachedUrlSet(cache));
      if (expectHash !== gotHash) {
        // The cache holds keys outside the final inventory (or is missing
        // some) — e.g. stale ?width= variants from a pre-update generation.
        // Genuinely stale resume state, so tag for self-heal.
        const e = new Error("Download incomplete — retry");
        e.stale = true;
        throw e;
      }

      // Progress baseline for M5's compare-then-push replay (best-effort).
      let baseline = null;
      try {
        const pr = await fetch(`/api/books/${id}/progress`, { credentials: "same-origin" });
        if (pr.ok) {
          const j = await pr.json();
          baseline = j && j.last_read_at ? j.last_read_at : null;
        }
      } catch (e) { /* baseline stays null */ }

      // Final cancellation gate, immediately before the manifest write — a
      // Cancel click or removeBookOffline() during the hash/baseline awaits
      // above must not publish (or resurrect) a manifest for a cache that
      // was just deleted. The window between this check and the write is a
      // single microtask; removeBookOffline deletes the manifest last, so a
      // resurrected row would still be a bug, but the marker check closes
      // the realistic (human-timed) race.
      if (marker.cancelled) {
        const e = new Error("Save cancelled");
        e.cancelled = true;
        throw e;
      }

      await idbPut("books", {
        id,
        title: book.title,
        author: book.author,
        format: book.format,
        totalChapters: book.total_chapters || 0,
        pageCount: isChapterMode ? 0 : Array.from(inventory).filter((u) => u.startsWith(`/api/books/${id}/pages/`)).length,
        savedAt: Date.now(),
        bytes,
        inventoryHash: expectHash,
        generation: pending.generation,
        baselineLastReadAt: baseline,
        version: OFFLINE_MANIFEST_VERSION,
      });
      published = true;
      await idbDelete("pendingSaves", id);
      offlineBookIds.add(id);
      offlineBookIdsGen++; // invalidate any in-flight refresh snapshot
    } catch (e) {
      // Self-heal a poisoned resume: only when a RESUMED run fails because
      // the state is genuinely STALE (a server-side change → HTTP error, or
      // a leftover-key hash mismatch), and only before publishing. A
      // transient network drop is NOT stale — wiping the partial cache then
      // would throw away every already-downloaded chapter/page and force a
      // full re-download on the next attempt, so those keep the resume state.
      if (wasResume && !published && e.stale) {
        await caches.delete(offlineCacheName(id)).catch(() => {});
        await idbDelete("pendingSaves", id).catch(() => {});
      }
      throw e;
    } finally {
      // Delete only our own marker — a newer save's marker must survive.
      if (activeOfflineSaves[id] === marker) delete activeOfflineSaves[id];
    }
  }

  async function removeBookOffline(id) {
    cancelOfflineSave(id);
    await caches.delete(offlineCacheName(id)).catch(() => {});
    await idbDelete("books", id).catch(() => {});
    await idbDelete("pendingSaves", id).catch(() => {});
    offlineBookIds.delete(id);
    offlineBookIdsGen++; // invalidate any in-flight refresh snapshot
  }

  // ── Offline mode: eviction integrity + library reconciliation ──
  // Eviction honesty (spec §Eviction honesty): the browser can evict Cache
  // Storage under pressure while leaving the IndexedDB manifest behind, so a
  // manifest row is only as true as its cache. On boot (online OR offline),
  // for each saved book compare the cache's current key set against the
  // manifest's inventory hash — one cache.keys() per book, no per-URL sweep.
  // A mismatch (or a vanished cache) means the download is no longer whole:
  // drop BOTH the row and the cache (a post-eviction partial cache has no
  // pendingSaves row, so per the save-engine garbage rule it must not linger
  // as fake resume state; a fresh save rebuilds it).
  async function verifyOfflineIntegrity() {
    if (!offlineSupported()) return;
    let rows;
    try { rows = await idbGetAll("books"); } catch (e) { return; }
    let dropped = 0;
    for (const row of rows) {
      // A save re-building this book's cache deletes then repopulates it; a
      // mid-rebuild snapshot would hash-mismatch and wrongly drop a book the
      // user is actively (re-)saving. Skip it — the save publishes a fresh,
      // verified manifest itself, and the next boot re-checks.
      if (activeOfflineSaves[row.id]) continue;
      let ok = false;
      try {
        if (await caches.has(offlineCacheName(row.id))) {
          const cache = await caches.open(offlineCacheName(row.id));
          ok = (await inventoryHash(await cachedUrlSet(cache))) === row.inventoryHash;
        }
      } catch (e) { ok = false; }
      if (!ok) {
        await caches.delete(offlineCacheName(row.id)).catch(() => {});
        await idbDelete("books", row.id).catch(() => {});
        offlineBookIds.delete(row.id);
        dropped++;
      }
    }
    if (dropped) {
      offlineBookIdsGen++;
      showToast("Your browser removed some downloaded books");
    }
  }

  // Online reconciliation (spec §Library reconciliation): after a successful
  // boot, compare saved manifests against the live library. A book gone from
  // the server loses its offline copy; a book still present has its display
  // metadata + cached detail/cover refreshed unconditionally (covers a
  // cover-only change with no drift detection). Refresh writes are ok-gated
  // so a transient 401/503 can never poison a valid cached entry.
  async function reconcileOfflineBooks(liveBooks) {
    if (!offlineSupported()) return;
    let rows;
    try { rows = await idbGetAll("books"); } catch (e) { return; }
    if (!rows.length) return;
    const liveById = new Map((liveBooks || []).map((b) => [b.id, b]));
    let removed = 0;
    for (const row of rows) {
      // Skip a book the user is actively (re-)saving — reconciling its
      // metadata/cache mid-save would fight the save's own writes.
      if (activeOfflineSaves[row.id]) continue;
      const live = liveById.get(row.id);
      if (!live) { await removeBookOffline(row.id); removed++; continue; }
      // Patch ONLY the display fields, re-reading the current row in one
      // transaction — `rows` is a detached snapshot, so writing the whole
      // snapshot back would clobber a fresh inventoryHash/generation/baseline
      // a concurrent save published between the snapshot read and here.
      await patchOfflineDisplayFields(row.id, live.title, live.author, live.format).catch(() => {});
      const cache = await caches.open(offlineCacheName(row.id)).catch(() => null);
      if (!cache) continue;
      for (const url of [
        `/api/books/${row.id}`,
        `/api/books/${row.id}/cover`,
        `/api/books/${row.id}/cover?size=thumb`,
      ]) {
        try {
          const resp = await fetch(url, { credentials: "same-origin" });
          if (resp.ok) await cache.put(new Request(url), resp);
        } catch (e) { /* keep the existing cached entry */ }
      }
    }
    if (removed) { offlineBookIdsGen++; showToast("Removed offline copies of deleted books"); }
  }

  // Grid badges read this set (mirrors progressByBook's pattern); refreshed
  // by showLibrary and kept current by save/unsave above. The manifest read
  // in refreshOfflineBookIds is async and can resolve after a save/unsave
  // has already mutated the set in place — a generation token drops any
  // snapshot that a later mutation has superseded, so a stale read can't
  // restore a removed id or drop a just-saved one.
  let offlineBookIds = new Set();
  let offlineBookIdsGen = 0;
  async function refreshOfflineBookIds() {
    const gen = ++offlineBookIdsGen;
    const ids = (await getAllOfflineManifests()).map((r) => r.id);
    if (gen !== offlineBookIdsGen) return; // a save/unsave superseded this snapshot
    offlineBookIds = new Set(ids);
  }

  // ── Offline mode: reading-progress queue + replay ──
  // Progress made while offline is queued (one row per book, latest wins) and
  // replayed on reconnect with compare-then-push: push only if the server's
  // progress hasn't advanced past the baseline recorded when the book was
  // last in sync — so reading the same book elsewhere meanwhile is never
  // clobbered by a stale offline position.

  // Record the server's current last_read_at as the sync baseline for a saved
  // book (called after any successful server progress read/write). Best-effort.
  async function updateOfflineBaseline(id, lastReadAt) {
    if (!offlineSupported() || !lastReadAt) return;
    try {
      // Read-modify-write in ONE transaction: if removeBookOffline deleted
      // the row in a prior transaction, the get returns undefined and we
      // don't put — so a baseline update can never resurrect a removed
      // manifest. (A get-then-separate-put would race that delete.)
      const db = await offlineDb();
      await new Promise((resolve, reject) => {
        const tx = db.transaction("books", "readwrite");
        const store = tx.objectStore("books");
        const getReq = store.get(id);
        getReq.onsuccess = () => {
          const row = getReq.result;
          if (row) { row.baselineLastReadAt = lastReadAt; store.put(row); }
        };
        tx.oncomplete = () => resolve();
        tx.onabort = () => reject(tx.error);
        tx.onerror = () => reject(tx.error);
      });
    } catch (e) { /* best-effort */ }
  }

  // Patch only a saved book's display fields, re-reading the current row in
  // one transaction so a concurrent save's newer inventoryHash/generation/
  // baselineLastReadAt is preserved (never clobbered by a stale snapshot).
  async function patchOfflineDisplayFields(id, title, author, format) {
    const db = await offlineDb();
    await new Promise((resolve, reject) => {
      const tx = db.transaction("books", "readwrite");
      const store = tx.objectStore("books");
      const getReq = store.get(id);
      getReq.onsuccess = () => {
        const cur = getReq.result;
        if (cur) { cur.title = title; cur.author = author; cur.format = format; store.put(cur); }
      };
      tx.oncomplete = () => resolve();
      tx.onabort = () => reject(tx.error);
      tx.onerror = () => reject(tx.error);
    });
  }

  // Atomic revision-guarded delete of a queue row: re-reads and deletes in one
  // transaction, so a newer offline write that lands between a separate
  // read and delete is never dropped.
  async function deleteQueueRowIfRevision(id, revision) {
    const db = await offlineDb();
    await new Promise((resolve, reject) => {
      const tx = db.transaction("progressQueue", "readwrite");
      const store = tx.objectStore("progressQueue");
      const getReq = store.get(id);
      getReq.onsuccess = () => {
        const cur = getReq.result;
        if (cur && cur.revision === revision) store.delete(id);
      };
      tx.oncomplete = () => resolve();
      tx.onabort = () => reject(tx.error);
      tx.onerror = () => reject(tx.error);
    });
  }

  // Enqueue an offline progress write (the sendProgress network-failure path).
  async function queueOfflineProgress(id, chapterIndex, scrollPosition) {
    if (!offlineSupported() || !offlineBookIds.has(id)) return;
    try {
      const row = await idbGet("books", id);
      const prev = await idbGet("progressQueue", id);
      await idbPut("progressQueue", {
        bookId: id,
        chapterIndex,
        scrollPosition,
        queuedAt: Date.now(),
        baselineLastReadAt: row ? row.baselineLastReadAt : null,
        revision: (prev ? prev.revision : 0) + 1,
      });
    } catch (e) { /* best-effort — a dropped offline save matches pre-offline behavior */ }
  }

  // Replay every queued row, serialized per book through saveChains so a
  // replay can't interleave with a live save. Triggered on reconnect (the
  // `online` event and each successful init()).
  let replayInFlight = false;
  async function replayProgressQueue() {
    if (!offlineSupported()) return;
    // init() and the `online` listener can both fire on a launch-after-
    // reconnect; a guard avoids replaying every queued row twice (the second
    // pass would just waste a GET per book hitting the already-drained path).
    if (replayInFlight) return;
    replayInFlight = true;
    try {
      const rows = await idbGetAll("progressQueue");
      const chains = rows.map((row) => {
        const prev = saveChains[row.bookId] || Promise.resolve();
        const next = prev.then(() => replayOneProgress(row)).catch(() => {});
        saveChains[row.bookId] = next;
        return next;
      });
      Promise.all(chains).finally(() => { replayInFlight = false; });
    } catch (e) {
      // idbGetAll rejected, or a synchronous throw while wiring the chains —
      // clear the guard so a later trigger can retry (never wedge replay off
      // for the whole session).
      replayInFlight = false;
    }
  }

  async function replayOneProgress(row) {
    let resp;
    try {
      resp = await fetch(`/api/books/${row.bookId}/progress`, { credentials: "same-origin" });
    } catch (e) { return; }                 // still offline — keep the row
    // Before any status handling: under a moved profile this book id belongs
    // to the other library, so the GET 404s and the branch below would delete
    // the queued row — discarding an offline reading position that is still
    // valid in the profile it was recorded in.
    if (noteProfileTag(resp)) return;      // keep the row; the page is reloading
    if (resp.status === 401) {
      // Session expired — surface login (same as a live save) and keep the
      // row so replay retries after re-auth, instead of failing silently.
      authenticated = false;
      showLogin();
      return;
    }
    if (resp.status === 404) { await idbDelete("progressQueue", row.bookId).catch(() => {}); return; }
    if (!resp.ok) return;                    // 5xx — keep, retry next trigger
    let server = null;
    try { server = await resp.json(); } catch (e) { return; } // malformed — keep
    const serverTs = server && server.last_read_at ? server.last_read_at : null;
    // Push only if the server hasn't advanced past our baseline (nobody read
    // this book elsewhere while we were offline). Otherwise discard — server
    // wins (the locked compare-then-push decision).
    //
    // Accepted limitations (single-user app; spec §Progress sync): the
    // GET→PUT gap is a non-atomic check that another client could interleave,
    // and last_read_at has 1-second precision so a same-second write from
    // another device is indistinguishable from "unchanged". Both windows are
    // milliseconds/one-second wide for one person and the cost is a stale
    // reading position they can page back from — closing them needs a
    // server-side conditional (CAS) write, judged disproportionate for v1.
    if (serverTs === row.baselineLastReadAt || (!serverTs && !row.baselineLastReadAt)) {
      try {
        // Direct PUT (not sendProgress) — a queued row is the book's latest
        // offline intent, so it must bypass the live-session monotonic guard
        // (an offline Start Over to page 0 must replay, not be dropped).
        const put = await apiWrite(`/api/books/${row.bookId}/progress`, {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ chapter_index: row.chapterIndex, scroll_position: row.scrollPosition }),
          credentials: "same-origin",
        });
        if (!put.ok) return;                 // keep the row; retry next trigger
        // Advance the live-session high-water mark so a later stale live save
        // (lower index) can't regress the position replay just committed.
        lastSentIndex[row.bookId] = row.chapterIndex;
        const saved = await put.json().catch(() => null);
        if (saved && saved.last_read_at) await updateOfflineBaseline(row.bookId, saved.last_read_at);
      } catch (e) { return; }               // network dropped mid-replay — keep
    }
    // Pushed or discarded: atomically delete the row only if a newer offline
    // write hasn't superseded it since we read it (revision guard in one tx).
    await deleteQueueRowIfRevision(row.bookId, row.revision).catch(() => {});
  }


  // Finding 11: a hand-edited or corrupted stored value must never flow
  // straight into data-theme/aria-label — coerce anything outside the known
  // set to "system".
  const VALID_THEME_MODES = ["light", "dark", "system"];
  function readStoredThemeMode() {
    const stored = safeStorageGet(THEME_STORAGE_KEY);
    return VALID_THEME_MODES.includes(stored) ? stored : "system";
  }
  let themeMode = readStoredThemeMode();

  function systemPrefersDark() {
    return !!(window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches);
  }

  function applyTheme() {
    if (themeMode === "system") {
      document.documentElement.removeAttribute("data-theme");
    } else {
      document.documentElement.setAttribute("data-theme", themeMode);
    }
  }

  function themeAriaLabel() {
    if (themeMode === "system") return `Theme: system (${systemPrefersDark() ? "dark" : "light"})`;
    return `Theme: ${themeMode}`;
  }

  function updateThemeButtonLabel() {
    const btn = $("#theme-toggle-btn");
    if (!btn) return;
    const label = themeAriaLabel();
    btn.title = label;
    btn.setAttribute("aria-label", label);
  }

  // Applied immediately (idempotent with the index.html bootstrap script,
  // which already set data-theme for an explicit light/dark choice before
  // first paint) so "system" mode is also correctly reflected even if the
  // bootstrap script ever gets out of sync.
  applyTheme();

  // Item 6: "system" mode has no data-theme attribute at all, so the CSS
  // `@media (prefers-color-scheme: dark)` block already swaps the palette
  // live with no JS involvement. This listener only keeps the toggle
  // button's aria-label/title (which names the resolved scheme while in
  // system mode) in sync with that live change.
  if (window.matchMedia) {
    const schemeQuery = window.matchMedia("(prefers-color-scheme: dark)");
    const onSchemeChange = () => { if (themeMode === "system") updateThemeButtonLabel(); };
    if (schemeQuery.addEventListener) schemeQuery.addEventListener("change", onSchemeChange);
    else if (schemeQuery.addListener) schemeQuery.addListener(onSchemeChange); // Safari <14
  }

  // Item 5: bumped on every loadBooks() call; a response is only rendered if
  // it still matches the counter captured at call time — guards against a
  // slow request (now with more awaits, thanks to the shelf fetches)
  // clobbering a faster, later one (e.g. rapid search typing / filter
  // clicks).
  let libraryRenderGen = 0;

  // Item 14: infinite-scroll pagination state for the "All Books" grid.
  // `libraryTotal` is `null` whenever the current grid isn't paginated at all
  // (the collections endpoint, or a legacy server response with no
  // `X-Total-Count` header) — every pagination code path treats `null` as
  // "nothing more to load, don't set up a sentinel". `libraryPageOffset` also
  // doubles as "how many books are rendered so far" since pages are always
  // fetched contiguously from 0. Reset to page 0 on every `loadBooks()` call
  // (i.e. every showLibrary re-entry: filter/sort/search change, back/
  // forward) via `resetLibraryPagination()`.
  const LIBRARY_PAGE_SIZE = 60;
  let libraryPageOffset = 0;
  let libraryTotal = null;
  let libraryPagesLoaded = 0;
  let libraryLoadingPage = false;
  let libraryScrollObserver = null;

  function resetLibraryPagination() {
    if (libraryScrollObserver) { libraryScrollObserver.disconnect(); libraryScrollObserver = null; }
    libraryPageOffset = 0;
    libraryTotal = null;
    libraryPagesLoaded = 0;
    libraryLoadingPage = false;
  }

  // Finding 3: module-scoped (not a closure local inside showLibrary) so it
  // can be cancelled from setSearchQuery and from showLibrary's own re-entry
  // path — a closure-local timer survives navigations that happen before its
  // 300ms elapses (filter click, Esc-clear, view switch), letting a stale
  // typed value fire afterward and clobber whatever was just navigated to.
  let searchDebounceTimer = null;

  // R2-3/R3-1: current view + reader state, used by the global keyboard
  // shortcut dispatcher and by the reader's own nav handlers.
  let currentView = null; // "login" | "library" | "detail" | "reader" | "stats" | "collections" | "vocabulary"
  let readerState = null; // set while currentView === "reader"; see showReader()
  let shortcutsOverlayOpen = false;

  // F10: true while the "You left off at..." resume prompt is on screen.
  // currentView is "reader" at that point but readerState is still null (the
  // book hasn't been entered yet), so the normal reader keyboard branch can't
  // handle Enter/Esc/Backspace — this flag lets the dispatcher special-case it.
  let resumePromptActive = false;
  let resumePromptBookId = null;

  // Item 4: set by the detail page's Continue/Start Over buttons just before
  // navigating to the reader, so showReader() can skip its own resume prompt
  // — the user already made that choice on the detail page. Consumed once.
  // scrollPosition carries the saved in-chapter offset for "continue" so the
  // reader can restore it (F2b) — read AND cleared at the very top of
  // showReader (F7) so an early return can never leak it.
  let readerEntryIntent = null; // { id, action: "continue" | "restart", scrollPosition } | null

  // F8: last progress this tab knows about per book, updated optimistically
  // on every reader navigation/scroll and confirmed on every successful PUT.
  // showDetail() prefers this over a GET that may have raced an in-flight
  // save. F9: lastSentIndex is this session's high-water mark per book — a
  // save carrying a lower index is dropped as out-of-order, except an
  // explicit "Start Over" (which resets the mark and legitimately writes 0).
  let lastKnownProgress = {}; // bookId -> { chapterIndex, scrollPosition, ts }
  let lastSentIndex = {};     // bookId -> last chapter_index confirmed sent this session
  let saveChains = {};        // bookId -> promise tail; serializes PUTs per book (F9)

  // Item 15: bookId -> chapter_index for every book with a saved progress
  // row, populated once per library entry (loadBooks) from the bulk
  // `/api/reading-progress` endpoint and reused across infinite-scroll pages
  // — bookCardHtml reads it directly, so appended pages get badges for free.
  let progressByBook = new Map();

  // Item 10 / Finding 7: dismissible, aria-live toast for surfacing fetch
  // failures that would otherwise render as a silent empty view. A single
  // shared container stacks multiple toasts; each auto-dismisses after 6s or
  // on click of its own close button. The container is created once, up
  // front (see the `ensureToastContainer()` call near the bottom of this
  // file) rather than lazily inside the first showToast() call — a screen
  // reader needs the aria-live region already present in the DOM before the
  // announcement lands, or the very first toast can go unannounced.
  function ensureToastContainer() {
    let container = $("#toast-container");
    if (!container) {
      container = document.createElement("div");
      container.id = "toast-container";
      container.className = "toast-container";
      container.setAttribute("role", "status");
      container.setAttribute("aria-live", "polite");
      document.body.appendChild(container);
    }
    return container;
  }

  function showToast(msg) {
    // This document is being replaced (see `noteProfileTag`). Every write
    // that was in flight when the profile move was noticed lands in its
    // caller's failure branch and asks for a toast, so without this the last
    // thing the user sees before the reload is a stack of "couldn't reach the
    // server" messages describing a server that answered fine.
    if (reloadPending) return;
    const container = ensureToastContainer();
    const toast = document.createElement("div");
    toast.className = "toast";
    toast.innerHTML = `<span>${esc(msg)}</span><button type="button" class="toast-close" aria-label="Dismiss">&times;</button>`;
    container.appendChild(toast);
    const remove = () => toast.remove();
    toast.querySelector(".toast-close").onclick = remove;
    setTimeout(remove, 6000);
  }

  // Finding 1: api() must let its many callers distinguish a *handled* 401
  // (showLogin() already ran — every caller just returns, nothing to show)
  // from a genuine failure that must be surfaced instead of leaving a
  // skeleton/spinner running forever. A `fetch()` throw (offline, the server
  // process died, DNS hiccup on a LAN) is rethrown as ApiNetworkError rather
  // than collapsing to the same `null` a handled 401 returns — every caller
  // below is wrapped in a try/catch that either renders a visible error (the
  // library, detail, reader, stats, collections views) or, for genuinely
  // best-effort call sites (shelves, filter bar, the resume-position check),
  // swallows it and degrades gracefully, exactly as it already does for a
  // 404/500 on the same endpoint. A non-network HTTP failure (4xx/5xx other
  // than 401) is NOT thrown here — it comes back as a normal, non-ok
  // Response so callers that render a primary view can show the status code.
  class ApiNetworkError extends Error {}

  // Stale-profile detection. One active profile is shared by the desktop app and
  // every web/OPDS client, so it can move while this tab sits open — leaving the
  // page showing another profile's library, with book ids that no longer mean
  // what it thinks. Every response carries `x-carrel-profile` (see
  // `web_server::profile_tag`); the first one seen defines this page's profile,
  // and a later mismatch means the ground moved. Comparison only — the value is
  // never decoded, and needs no crypto (so it works on a plain-HTTP LAN too).
  let seenProfileTag = null;
  let profileMoveHandled = false;
  // Set only where a document replacement has actually been requested. Kept
  // separate from profileMoveHandled on purpose: gating user feedback on
  // "a move was handled" would silence the app permanently the first time
  // some future path handles a move *without* replacing the document.
  let reloadPending = false;

  // Returns true when this response came back from a *different* profile than
  // the page booted into — i.e. its data must not be acted on. The reload is
  // fired once (later mismatches still report true: `location.reload()` does
  // not stop the caller that is already running, so every stale caller has to
  // be told, not just the first one).
  function noteProfileTag(resp) {
    let tag;
    try { tag = resp && resp.headers ? resp.headers.get("x-carrel-profile") : null; } catch (e) { return false; }
    if (!tag) return false; // pre-header server, or a response the SW synthesized
    if (seenProfileTag === null) { seenProfileTag = tag; return false; }
    if (tag === seenProfileTag) return false;
    if (!profileMoveHandled) {
      // Reload rather than patch: books, ids, collections, series, progress and
      // the offline namespace all belong to the profile this page booted into.
      // Guarded so a burst of in-flight requests can't reload repeatedly.
      profileMoveHandled = true;
      reloadPending = true;
      location.reload();
    }
    return true;
  }

  // A write whose response came from another profile. Thrown by apiWrite so a
  // stale caller stops instead of committing side effects: location.reload()
  // only *queues* a navigation, so without this the caller runs on — 200-OK
  // branches, optimistic-state commits, and (worst) the offline queue's
  // "the push succeeded, drop the row" delete all execute against the wrong
  // profile before the browser gets around to unloading the document.
  class ProfileMovedError extends Error {}

  async function api(path) {
    let resp;
    try {
      resp = await fetch(path, { credentials: "same-origin" });
    } catch (e) {
      throw new ApiNetworkError(e && e.message ? e.message : String(e));
    }
    noteProfileTag(resp);
    if (resp.status === 401) { authenticated = false; showLogin(); return null; }
    return resp;
  }

  // Write-path counterpart to api(): every write must also see
  // noteProfileTag, since a write issued after the active profile moved
  // would otherwise commit under the new profile with ids that belonged to
  // the old one. Unlike api(), this does not interpret the response — write
  // call sites each handle 401/403/404/409/423/204-vs-body differently, so
  // it just forwards fetch()'s own resolution/rejection and lets the caller's
  // existing try/catch and status checks run unchanged.
  async function apiWrite(path, options) {
    const resp = await fetch(path, options);
    if (noteProfileTag(resp)) throw new ProfileMovedError("active profile moved");
    return resp;
  }

  // Finding 4: short, differentiated failure text depending on *why* a fetch
  // failed, instead of one blanket "Couldn't reach Carrel server" message for
  // network errors, HTTP errors, and bad JSON alike.
  function apiFailureToastMessage(e) {
    return e instanceof ApiNetworkError ? "Couldn't reach Carrel server" : "Unexpected response";
  }
  function httpErrorToastMessage(status) {
    return `Server error (${status}) — check the Carrel app`;
  }

  // Finding 1: shared "this whole view failed to load" fallback for views
  // that render into the full #app container (detail, reader) rather than a
  // sub-container that already has its own empty/error styling (library,
  // stats, collections use `.empty` on their existing content element
  // instead — see showLibraryLoadError/showStats/showCollections). Reuses
  // the reader loader's pre-existing bare `class="error"` convention.
  function renderViewError(message) {
    app().innerHTML = `<div class="error">${esc(message)}</div>`;
  }

  // ── Loading Skeletons (Item 10) ────────────────
  // CSS-shimmer placeholders (mirrors the desktop app's `animate-shimmer`
  // keyframe in src/index.css) shown while the library grid, detail page, or
  // reader are loading, instead of a bare "Loading..." string.
  function skeletonCardHtml() {
    return `
      <div class="skeleton-card">
        <div class="skeleton-cover"></div>
        <div class="skeleton-line"></div>
        <div class="skeleton-line short"></div>
      </div>`;
  }

  function skeletonGridHtml(count) {
    let html = "";
    for (let i = 0; i < (count || 12); i++) html += skeletonCardHtml();
    return `<div class="skeleton-grid">${html}</div>`;
  }

  function detailSkeletonHtml() {
    return `
      <div class="detail">
        <div class="meta">
          <div class="cover"><div class="skeleton-cover" style="border-radius:var(--radius)"></div></div>
          <div class="info" style="flex:1">
            <div class="skeleton-line" style="width:70%;height:22px;margin-bottom:14px"></div>
            <div class="skeleton-line" style="width:40%"></div>
            <div class="skeleton-line" style="width:90%;margin-top:16px"></div>
            <div class="skeleton-line" style="width:80%"></div>
          </div>
        </div>
      </div>`;
  }

  function readerSkeletonHtml() {
    return `<div class="reader-skeleton"><div class="skeleton-cover"></div></div>`;
  }

  // ── Router ────────────────────────────────────
  function navigate(hash) {
    window.location.hash = hash;
  }

  // Finding 2: Firefox percent-decodes the `location.hash` property itself
  // (Chromium does not) — reading it directly can silently corrupt a
  // URLSearchParams parse of a value containing %/&/=/+ (e.g. a series name).
  // `location.href`'s fragment isn't affected by that quirk, so every read of
  // the current hash for parsing purposes goes through this instead.
  function rawHash() {
    const href = window.location.href;
    const i = href.indexOf("#");
    return i >= 0 ? href.slice(i) : "#";
  }

  // Item 7: parse `#/library?q=...&series=...&collection=...&sort=...` (also
  // used for the bare `#`/`#/` home route, which parses to all defaults).
  function parseLibraryParams(hash) {
    const qIndex = hash.indexOf("?");
    const params = new URLSearchParams(qIndex >= 0 ? hash.slice(qIndex + 1) : "");
    return {
      q: params.get("q") || "",
      series: params.get("series") || null,
      collection: params.get("collection") || null,
      sort: params.get("sort") || DEFAULT_SORT,
      want_to_read: params.get("want_to_read") === "true",
    };
  }

  // Item 7: the inverse of parseLibraryParams — the single place that turns
  // a desired library state into a hash string. Collapses to the bare "#"
  // home route when every param is at its default, so "no filters" always
  // has one canonical URL (goHome() and "All Books" both rely on this).
  function libraryHash(state) {
    const params = new URLSearchParams();
    if (state.q) params.set("q", state.q);
    if (state.series) params.set("series", state.series);
    if (state.collection) params.set("collection", state.collection);
    if (state.sort && state.sort !== DEFAULT_SORT) params.set("sort", state.sort);
    // Presence-only: emit `want_to_read=true` to enable, omit it to disable.
    if (state.want_to_read) params.set("want_to_read", "true");
    const qs = params.toString();
    return qs ? "#/library?" + qs : "#";
  }

  // Finding 10: the `{ q, series, collection, sort }` library-state object is
  // hand-copied at every filter-mutating call site — this is the one source
  // of "current state", spread and overridden by callers that only change
  // one or two fields.
  function currentLibraryState() {
    return { q: activeQuery, series: activeSeries, collection: activeCollectionId, sort: activeSort, want_to_read: activeWantToRead };
  }

  // Finding C: every explicit "go back to the home screen" action (header
  // back buttons, Esc/Backspace from detail) must clear any active
  // collection/series/want-to-read filter — otherwise a filter set on a
  // previous library visit silently survives into an unrelated round trip
  // through detail/stats/collections, landing back on a filtered grid with the
  // shelves suppressed. This is deliberately distinct from navigations that
  // set a filter on purpose right before going home (showDetail's series
  // link, showCollections' collection/series rows) — those must keep working
  // and do NOT go through this helper.
  // Finding 4: this must NOT also wipe the active query/sort — "leave this
  // view" only means dropping the collection/series filter, not resetting
  // the rest of the library state. libraryHash() collapses back to the bare
  // "#" on its own when query/sort are also at their defaults.
  function goHome() {
    navigate(libraryHash({ ...currentLibraryState(), series: null, collection: null, want_to_read: false }));
  }

  // Bumped on every route() call; showOfflineLibrary's async re-probe checks
  // it after each await so a stale continuation can't render the offline
  // library over a newer navigation (or over a successful reconnect).
  let routeGen = 0;

  function route() {
    // K4: the shortcuts overlay is appended to document.body and must not
    // survive a navigation — it would block the next view and swallow
    // shortcuts on it.
    closeShortcutsOverlay();
    // Same document.body-survival hazard: a Define result card would float
    // over the next view (Backspace-goBack reaches here without the Esc
    // branch's popover check, and browser back skips keydown entirely).
    closeDefinePopover();
    // Unlike the highlights/bookmarks/TOC drawers (which persist across a
    // same-book chapter turn), the saved-words drawer closes on ANY
    // navigation — same transience as the Define popover above.
    if (vocabPanelOpen) closeVocabPanel(false);
    const gen = ++routeGen;
    const hash = rawHash();
    // Highlight-jump backstop (spec §4): ANY navigation that isn't the jump's
    // own — the one still carrying the active token's navId, which
    // renderReaderContent hasn't captured yet at this point — supersedes and
    // cancels the pending jump. The reader handlers (Prev/Next, TOC,
    // bookmarks, close) clear it themselves; this catches history/back, nav
    // icons, login round-trips, and direct hash edits.
    if (pendingHlJump && hlJumpNavId !== pendingHlJump.navId) clearHlJump();
    // Same backstop for a pending vocabulary jump.
    if (pendingVocabJump && vocabJumpNavId !== pendingVocabJump.navId) clearVocabJump();
    // Offline: only a SAVED book's detail/reader work (their content is
    // served from the M2 cache). Everything else — a top-level destination
    // (library/stats/collections, none of which have offline data) OR a
    // deep-link to a book that isn't downloaded — routes to the offline
    // library instead of dead-ending on a bare error page. showOfflineLibrary
    // also re-probes the network, so ordinary navigation recovers the full
    // app once connectivity returns (not only the banner's Retry button).
    if (offlineMode) {
      const bm = hash.match(/^#\/book\/([^/]+)/);
      const savedBook = bm && offlineBookIds.has(decodeURIComponent(bm[1]));
      if (!savedBook) {
        if (bm) showToast("That book isn't available offline");
        return showOfflineLibrary(gen);
      }
    }
    if (hash === "#" || hash === "#/" || hash.startsWith("#/library")) {
      return showLibrary(parseLibraryParams(hash));
    }
    if (hash === "#/stats") return showStats();
    if (hash === "#/collections") return showCollections();
    if (hash === "#/vocabulary") return showVocabularyScreen();
    if (hash.startsWith("#/book/") && hash.includes("/read")) {
      const parts = hash.replace("#/book/", "").replace("/read", "").split("/");
      return showReader(parts[0], parseInt(parts[1] || "0"));
    }
    if (hash.startsWith("#/book/")) return showDetail(hash.replace("#/book/", ""));
    showLibrary(parseLibraryParams(hash));
  }

  window.addEventListener("hashchange", route);

  // ── Suppress native page pinch-zoom ───────────────────────────────────
  // The reader implements its own pinch-zoom (page mode); the browser's
  // whole-page magnification must never fire alongside it. `maximum-scale=1,
  // user-scalable=no` in the viewport meta covers Android, desktop, and
  // iOS-standalone (PWA), but mobile Safari in a tab ignores user-scalable —
  // there the only lever is preventing its proprietary GestureEvents. The
  // reader stage already does this for gestures over itself; this document-
  // level backstop also catches a pinch whose fingers land off the stage (on
  // the chrome or margins) and covers non-reader screens. No-op on browsers
  // without GestureEvent (everything but Safari).
  document.addEventListener("gesturestart", (e) => e.preventDefault(), { passive: false });
  document.addEventListener("gesturechange", (e) => e.preventDefault(), { passive: false });

  // ── Keyboard Shortcuts ────────────────────────
  // Single listener, dispatches on `currentView`. See docs/web-ui-improvements.md
  // Item 2 for the key map.
  function isTypingTarget(el) {
    if (!el) return false;
    const tag = el.tagName;
    // K2: the range slider is an <input> but isn't a "typing" surface — treat
    // it separately so shortcuts like Escape/Backspace/f still work after
    // interacting with it (native Arrow/Home/End stepping is handled before
    // this check runs; see the keydown listener below).
    if (tag === "INPUT" && el.type === "range") return false;
    return tag === "INPUT" || tag === "SELECT" || tag === "TEXTAREA";
  }

  function isRangeInput(el) {
    return !!el && el.tagName === "INPUT" && el.type === "range";
  }

  function toggleFullscreen() {
    if (!document.fullscreenElement) {
      document.documentElement.requestFullscreen().catch(() => {});
    } else {
      document.exitFullscreen().catch(() => {});
    }
  }

  function openShortcutsOverlay() {
    if (shortcutsOverlayOpen) return;
    shortcutsOverlayOpen = true;
    const div = document.createElement("div");
    div.className = "shortcuts-overlay";
    div.id = "shortcuts-overlay";
    div.innerHTML = `
      <div class="shortcuts-panel">
        <h2>Keyboard Shortcuts</h2>
        <dl>
          <dt>&larr; / &rarr;</dt><dd>Prev / next page or chapter</dd>
          <dt>Home / End</dt><dd>First / last page or chapter</dd>
          <dt>f</dt><dd>Toggle fullscreen</dd>
          <dt>Esc / Backspace</dt><dd>Back</dd>
          <dt>/</dt><dd>Focus search</dd>
          <dt>?</dt><dd>Show this overlay</dd>
        </dl>
        <button id="shortcuts-close">Close</button>
      </div>`;
    document.body.appendChild(div);
    $("#shortcuts-close").addEventListener("click", closeShortcutsOverlay);
    div.addEventListener("click", (e) => { if (e.target === div) closeShortcutsOverlay(); });
  }

  function closeShortcutsOverlay() {
    shortcutsOverlayOpen = false;
    const div = $("#shortcuts-overlay");
    if (div) div.remove();
  }

  // Re-clamp the zoom pan against the new stage size on resize/orientation
  // change so a zoomed image can't get stuck showing a gap.
  window.addEventListener("resize", () => {
    if (readerState && readerState.mode === "page" && isZoomed()) {
      clampZoomPan();
      applyZoomTransform();
    }
  });

  document.addEventListener("keydown", (e) => {
    // K3: never hijack modified shortcuts (Cmd/Ctrl+F find, Cmd+ArrowLeft
    // history back, etc.) — bail before any preventDefault.
    if (e.ctrlKey || e.metaKey || e.altKey) return;

    if (e.key === "?" && !isTypingTarget(e.target)) {
      e.preventDefault();
      openShortcutsOverlay();
      return;
    }

    if (shortcutsOverlayOpen) {
      if (e.key === "Escape") { e.preventDefault(); closeShortcutsOverlay(); }
      return;
    }

    // Esc closes an open highlight or Define popover first — before the
    // reader's Esc-goes-Back branch below.
    if (e.key === "Escape" && (hlPopoverEl || hlEditPopoverEl || dictPopoverEl)) {
      e.preventDefault();
      closeHlPopover();
      closeHlEditPopover();
      closeDefinePopover();
      return;
    }

    // Item 7: Esc closes an open filter dropdown panel first, even when
    // focus is inside its type-to-filter input (which is otherwise a
    // "typing target" the dispatcher ignores below).
    if (e.key === "Escape" && $(".filter-panel:not([hidden])")) {
      e.preventDefault();
      closeAllFilterPanels();
      return;
    }

    // Esc closes an open detail-view "More" menu first (before the detail
    // view's Esc-goes-Back handler), returning focus to the More button.
    const detailMenuEl = $("#detail-menu");
    if (e.key === "Escape" && detailMenuEl && !detailMenuEl.hidden) {
      e.preventDefault();
      closeDetailMenu();
      const mb = $("#detail-more-btn");
      if (mb) mb.focus();
      return;
    }

    if (e.key === "Escape" && currentView === "library" && e.target && e.target.id === "search") {
      e.preventDefault();
      e.target.value = "";
      e.target.blur();
      setSearchQuery("");
      return;
    }

    // K2: the range slider keeps native Arrow/Home/End stepping; every other
    // shortcut key falls through to the normal handling below.
    if (isRangeInput(e.target) && ["ArrowLeft", "ArrowRight", "Home", "End"].includes(e.key)) {
      return;
    }

    if (isTypingTarget(e.target)) return;

    if (currentView === "library") {
      if (e.key === "/") {
        e.preventDefault();
        const s = $("#search");
        if (s) s.focus();
      }
      return;
    }

    // F10: keyboard must keep working while the resume prompt is up
    // (readerState is still null at that point, so the branch below can't
    // handle it). Enter accepts (resume), Esc/Backspace decline back to
    // detail — mirrors the prompt's own buttons.
    if (currentView === "reader" && resumePromptActive) {
      if (e.key === "Enter") {
        e.preventDefault();
        const btn = $("#resume-btn");
        if (btn) btn.click();
      } else if (e.key === "Escape" || e.key === "Backspace") {
        e.preventDefault();
        const bookId = resumePromptBookId;
        resumePromptActive = false;
        if (bookId) navigate("#/book/" + bookId);
      }
      return;
    }

    if (currentView === "reader" && readerState) {
      if (e.key === "ArrowRight") { e.preventDefault(); readerState.handlers.next(); }
      else if (e.key === "ArrowLeft") { e.preventDefault(); readerState.handlers.prev(); }
      else if (e.key === "Home") { e.preventDefault(); readerState.handlers.first(); }
      else if (e.key === "End") { e.preventDefault(); readerState.handlers.last(); }
      else if (e.key === "f" || e.key === "F") { e.preventDefault(); toggleFullscreen(); }
      else if (e.key === " " || e.key === "Spacebar") {
        // K5: fallback scroll in case the stage doesn't have native focus.
        e.preventDefault();
        scrollReaderStage(e.shiftKey ? -1 : 1);
      }
      else if (e.key === "Escape" || e.key === "Backspace") {
        // K1: let the browser exit fullscreen natively; don't also navigate
        // back and lose the reading position.
        if (e.key === "Escape" && document.fullscreenElement) return;
        e.preventDefault();
        readerState.handlers.goBack();
      }
      return;
    }

    if (currentView === "detail") {
      if (e.key === "Escape" || e.key === "Backspace") { e.preventDefault(); goHome(); }
    }
  });

  // ── Login ─────────────────────────────────────
  function showLogin() {
    currentView = "login";
    readerState = null;
    resumePromptActive = false;
    app().innerHTML = `
      <div class="login">
        <h1>Carrel</h1>
        <input type="password" id="pin" placeholder="PIN" maxlength="8" autofocus>
        <button id="login-btn">Enter</button>
        <div class="error" id="login-error"></div>
      </div>`;
    const pinInput = $("#pin");
    const btn = $("#login-btn");
    const err = $("#login-error");
    async function doLogin() {
      const pin = pinInput.value;
      if (!pin) return;
      btn.disabled = true;
      try {
        const resp = await apiWrite("/api/auth", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ pin }),
          credentials: "same-origin"
        });
        if (!resp.ok) {
          err.textContent = resp.status === 429 ? "Too many attempts. Try again later." : "Invalid PIN";
          btn.disabled = false;
          return;
        }
        authenticated = true;
        // A PIN-gated boot 401s out of init() into this login, so the profile
        // list and the offline scope are still unresolved here — resolve them
        // before the first render, or the header would have no switcher and
        // offline saves could land in the wrong profile's namespace.
        const loginScope = await syncOfflineScopeWithServer();
        // Same reasoning as init() below.
        if (loginScope === "unknown") offlineProfileUnknown = true;
        else if (loginScope === "changed") await verifyOfflineIntegrity();
        route();
      } catch(e) { err.textContent = "Connection error"; btn.disabled = false; }
    }
    btn.onclick = doLogin;
    pinInput.onkeydown = (e) => { if (e.key === "Enter") doLogin(); };
  }

  // ── Filter Bar (collections + series) ────────
  // Item 7: replaces the old horizontal pill strip (unusable past ~25
  // series) with two searchable dropdown panels + removable chips. Selecting
  // an entry (or removing a chip) always goes through libraryHash()+navigate
  // — a real hash push, since these are "filter clicks" per the Item 7 spec,
  // not keystrokes.
  let cachedCollections = [];
  let cachedSeries = [];

  function filterDropdownHtml(key, label) {
    return `
      <div class="filter-dropdown">
        <button class="filter-dropdown-btn" id="${key}-dropdown-btn" aria-haspopup="true" aria-expanded="false">${label} &#9662;</button>
        <div class="filter-panel" id="${key}-panel" hidden>
          <input type="text" class="filter-panel-search" id="${key}-panel-input" placeholder="Filter ${label.toLowerCase()}..." aria-label="Filter ${label.toLowerCase()}">
          <div class="filter-panel-list" id="${key}-panel-list"></div>
        </div>
      </div>`;
  }

  function closeAllFilterPanels() {
    $$(".filter-panel").forEach(p => { p.hidden = true; });
    $$(".filter-dropdown-btn").forEach(b => b.setAttribute("aria-expanded", "false"));
  }

  // A single document-level listener (bound once, not per render) closes any
  // open panel when the click lands outside it.
  document.addEventListener("click", (e) => {
    if (!e.target.closest(".filter-dropdown")) closeAllFilterPanels();
    if (!e.target.closest(".detail-more")) closeDetailMenu();
    if (!e.target.closest(".profile-switcher")) closeProfilePanel();
  });

  function selectFilter(key, value) {
    // Choosing a collection/series from the dropdown is collection/series
    // navigation, so it clears the top-level want-to-read filter (same rule as
    // goHome and the detail-view series link) — otherwise the spread would
    // carry it into an unintended combined filter.
    const next = { ...currentLibraryState(), series: null, collection: null, want_to_read: false };
    next[key] = value;
    const hash = libraryHash(next);
    // Finding 5: re-selecting the already-active filter produces the exact
    // same hash, so no `hashchange` fires and the grid would silently go
    // stale — refetch directly instead of navigating in that case.
    if (hash === rawHash()) {
      refreshLibrary(activeQuery);
    } else {
      navigate(hash);
    }
  }

  function bindFilterDropdown(key, items, mapItem) {
    const btn = $(`#${key}-dropdown-btn`);
    const panel = $(`#${key}-panel`);
    const input = $(`#${key}-panel-input`);
    const list = $(`#${key}-panel-list`);
    if (!btn || !panel || !input || !list) return;

    const mapped = items.map(mapItem);

    function renderList(filterText) {
      const q = filterText.toLowerCase();
      const filtered = mapped.filter(it => !q || it.label.toLowerCase().includes(q));
      if (filtered.length === 0) {
        list.innerHTML = '<div class="filter-panel-empty">No matches</div>';
        return;
      }
      list.innerHTML = filtered.map(it => `
        <button type="button" class="filter-panel-item" data-index="${filtered.indexOf(it)}">
          <span class="filter-panel-item-name">${esc(it.label)}</span>
          <span class="filter-panel-item-count">${it.count != null ? it.count : ""}</span>
        </button>`).join("");
      list.querySelectorAll("[data-index]").forEach(el => {
        el.onclick = () => {
          selectFilter(key, filtered[parseInt(el.dataset.index, 10)].value);
          closeAllFilterPanels();
        };
      });
    }

    btn.onclick = (e) => {
      e.stopPropagation();
      const wasOpen = !panel.hidden;
      closeAllFilterPanels();
      if (!wasOpen) {
        panel.hidden = false;
        btn.setAttribute("aria-expanded", "true");
        input.value = "";
        renderList("");
        input.focus();
      }
    };
    panel.addEventListener("click", (e) => e.stopPropagation());
    input.oninput = () => renderList(input.value);
  }

  function chipHtml(key, label) {
    return `<span class="chip">${esc(label)}<button type="button" class="chip-remove" data-remove="${key}" aria-label="Remove ${esc(label)} filter">&times;</button></span>`;
  }

  // Renders the removable chips + the dropdown buttons' "active" styling
  // from the current activeCollectionId/activeSeries — called both after a
  // full filter-bar render and whenever the URL changes without a DOM
  // rebuild (e.g. back/forward while still viewing the library).
  // Finding 8: guards renderFilterBar() (triggered below when the chip can't
  // resolve activeCollectionId against the cache) against refetching forever
  // if the collection is genuinely gone — only one refresh is attempted per
  // distinct activeCollectionId value.
  let chipRefreshAttemptedForCollectionId = null;

  function renderFilterChips() {
    const chips = $("#filter-chips");
    if (chips) {
      let html = "";
      if (activeCollectionId) {
        const c = cachedCollections.find(c => c.id === activeCollectionId);
        if (c) {
          html += chipHtml("collection", c.name);
        } else {
          // Finding 8: never show the raw UUID — the cache may simply be
          // stale (the collection was deleted/renamed elsewhere since the
          // filter bar was last rendered). Refresh once and re-render.
          html += chipHtml("collection", "Collection");
          if (chipRefreshAttemptedForCollectionId !== activeCollectionId) {
            chipRefreshAttemptedForCollectionId = activeCollectionId;
            renderFilterBar();
          }
        }
      }
      if (activeSeries) html += chipHtml("series", activeSeries);
      chips.innerHTML = html;
      chips.querySelectorAll("[data-remove]").forEach(btn => {
        btn.onclick = () => {
          const next = currentLibraryState();
          next[btn.dataset.remove] = null;
          navigate(libraryHash(next));
        };
      });
    }
    const collBtn = $("#collection-dropdown-btn");
    if (collBtn) collBtn.classList.toggle("active", !!activeCollectionId);
    const seriesBtn = $("#series-dropdown-btn");
    if (seriesBtn) seriesBtn.classList.toggle("active", !!activeSeries);
    // Back/forward (and any hash change without a DOM rebuild) re-syncs the
    // always-visible "Want to read" toggle's active state from the URL.
    const wantBtn = $("#filter-want-btn");
    if (wantBtn) {
      wantBtn.classList.toggle("active", activeWantToRead);
      wantBtn.setAttribute("aria-pressed", activeWantToRead ? "true" : "false");
    }
  }

  async function renderFilterBar() {
    const bar = $("#filter-bar");
    if (!bar) return;

    // Best-effort: the filter bar simply doesn't render if these fail (same
    // as an empty collections/series result) — never throws.
    const [collectionsResp, seriesResp] = await Promise.all([
      api("/api/collections").catch(() => undefined),
      api("/api/series").catch(() => undefined),
    ]);

    cachedCollections = collectionsResp && collectionsResp.ok ? await collectionsResp.json() : [];
    cachedSeries = seriesResp && seriesResp.ok ? await seriesResp.json() : [];

    // The bar always renders: the "Want to read" toggle is always available,
    // even for a library with no collections/series (otherwise it would
    // vanish for the many libraries that have neither). The collection/series
    // dropdowns only appear when there's something to put in them.
    bar.innerHTML = `
      <button type="button" class="filter-reset" id="filter-reset-btn">All Books</button>
      <button type="button" class="filter-want-toggle" id="filter-want-btn" aria-pressed="false">🔖 Want to read</button>
      ${cachedCollections.length > 0 ? filterDropdownHtml("collection", "Collections") : ""}
      ${cachedSeries.length > 0 ? filterDropdownHtml("series", "Series") : ""}
      <div class="filter-chips" id="filter-chips"></div>`;

    // Finding 4: "All Books" clears the collection/series filter ONLY —
    // it must preserve the active query/sort, not reset the whole library
    // state. libraryHash() collapses back to the bare "#" on its own when
    // query/sort are also at their defaults. It also clears "Want to read".
    $("#filter-reset-btn").onclick = () => navigate(libraryHash({ ...currentLibraryState(), series: null, collection: null, want_to_read: false }));
    $("#filter-want-btn").onclick = () => navigate(libraryHash({ ...currentLibraryState(), want_to_read: !activeWantToRead }));

    if (cachedCollections.length > 0) {
      bindFilterDropdown("collection", cachedCollections, (c) => ({ value: c.id, label: c.name, count: c.bookCount }));
    }
    if (cachedSeries.length > 0) {
      bindFilterDropdown("series", cachedSeries, (s) => ({ value: s.name, label: s.name, count: s.count }));
    }

    renderFilterChips();
  }

  // ── Library ───────────────────────────────────
  // Item 7: `params` is the already-parsed URL state (see
  // parseLibraryParams/route()) — the single source of truth. It's copied
  // into the active* module vars (a parse cache other code reads to build
  // the *next* hash) on every call, whether the DOM is being built fresh or
  // just synced to a new hash while already in the library view.
  async function showLibrary(params) {
    currentView = "library";
    document.title = "Carrel";
    flushProgressSave();
    readerState = null;
    resumePromptActive = false;
    // Finding 3: cancel any pending debounced search — every re-entry into
    // the library view (filter click, sort change, back/forward, self-heal)
    // is a point where an abandoned keystroke's stale value must not be
    // allowed to fire later and clobber what the user just navigated to.
    clearTimeout(searchDebounceTimer);

    params = params || {};
    activeQuery = params.q || "";
    activeSeries = params.series || null;
    activeCollectionId = params.collection || null;
    activeSort = params.sort || DEFAULT_SORT;
    activeWantToRead = !!params.want_to_read;

    const existing = $("#search");
    // Fix 6: only a fresh entry into the library (from detail/reader/stats/
    // collections, or the very first load — #app was just replaced, so
    // #search doesn't exist yet) re-fetches the bulk progress table. A hash
    // change while #search already exists is a filter/sort/search re-render,
    // which reuses the cached progressByBook (see loadBooks).
    const enteringLibraryFresh = !existing;
    if (!existing) {
      app().innerHTML = `
        <div class="header">
          <h1>Carrel</h1>
          <input type="search" id="search" placeholder="Search books..." aria-label="Search books" value="${esc(activeQuery)}">
          <select id="sort-select" aria-label="Sort by">
            <option value="date_added">Recent</option>
            <option value="title">Title</option>
            <option value="author">Author</option>
            <option value="last_read">Last Read</option>
            <option value="rating">Rating</option>
          </select>
          ${navIconsHtml("")}
        </div>
        <div class="filter-bar" id="filter-bar"></div>
        <div id="library-content">${skeletonGridHtml()}</div>
        ${tabBarHtml("library")}`;

      const sortSelect = $("#sort-select");
      sortSelect.value = activeSort;
      sortSelect.onchange = () => {
        // Item 7: a sort change is a "filter change", not a keystroke — a
        // real hash push, so back can step back to the previous sort.
        navigate(libraryHash({ ...currentLibraryState(), sort: sortSelect.value }));
      };

      $("#search").oninput = (e) => {
        // Finding 3: searchDebounceTimer is module-scoped (not a closure
        // local) so it can be cancelled from setSearchQuery/showLibrary too —
        // see their comments for why a closure-local timer here missed the
        // navigate-away-before-300ms case.
        clearTimeout(searchDebounceTimer);
        const value = e.target.value;
        searchDebounceTimer = setTimeout(() => setSearchQuery(value), 300);
      };

      bindNavIcons();
      bindTabBar();
      ensureDictionaryStatus().then(applyVocabNavVisibility);
      renderFilterBar();
    } else {
      // Item 7: DOM already exists — this is a hash change while still
      // viewing the library (back/forward, a filter click, a sort change,
      // or the self-heal path in loadBooks). Sync every control to the
      // newly-parsed state instead of rebuilding.
      if (existing.value !== activeQuery) existing.value = activeQuery;
      const sortSelect = $("#sort-select");
      if (sortSelect && sortSelect.value !== activeSort) sortSelect.value = activeSort;
      renderFilterChips();
      const contentEl = $("#library-content");
      if (contentEl) contentEl.innerHTML = skeletonGridHtml();
    }

    // Finding 5: scroll restore now happens inside loadBooks() itself, only
    // after a real successful render — never unconditionally here, which
    // would also fire on a 401/failed load and scroll the wrong view.
    await loadBooks(activeQuery, enteringLibraryFresh);
  }

  // Item 10: preserves the library's scroll position across a detail/reader
  // round trip. Saved keyed by the exact hash being left (so a different
  // filter/search/sort state never restores the wrong scroll offset).
  function libraryScrollKey(hash) {
    return "carrel_scroll_" + hash;
  }
  // Item 14: persists the page count alongside the scroll offset — with
  // infinite scroll, a deep `scrollY` is only meaningful once the pages that
  // produced it are loaded again. `pages` defaults to 1 (a single, unpaginated
  // grid, e.g. a collection) whenever pagination isn't active.
  function saveLibraryScrollPosition() {
    if (currentView !== "library") return;
    const payload = JSON.stringify({ y: window.scrollY, pages: Math.max(libraryPagesLoaded, 1) });
    safeSessionSet(libraryScrollKey(rawHash()), payload);
  }
  async function restoreLibraryScrollPosition() {
    // Finding 5: guard against restoring onto a view the user has since
    // navigated away from (e.g. a slow load that resolves after they already
    // opened a book) — only ever scroll while still on the library.
    if (currentView !== "library") return;
    const key = libraryScrollKey(rawHash());
    const saved = safeSessionGet(key);
    if (saved == null) return;
    // Finding 6 (codex): don't consume the key on restore — a Forward
    // navigation after a Back would otherwise find it already gone and lose
    // the saved position a second time. saveLibraryScrollPosition()
    // overwrites this same key on every departure instead; unbounded growth
    // isn't a concern (sessionStorage, one small key per distinct hash,
    // forgotten at the end of the tab's session).
    let y = null;
    // Fix C: `pages` stays `null` for a legacy plain-number save (from
    // before Item 14) — there's no way to know how many pages produced that
    // `y`, so it must NOT default to 1 (which silently clamped a deep legacy
    // offset to the 60-item first page). The legacy branch below instead
    // grows the replay range until it covers `y`.
    let pages = null;
    try {
      const parsed = JSON.parse(saved);
      if (parsed && typeof parsed === "object") {
        y = parsed.y;
        pages = parsed.pages || 1;
      } else if (typeof parsed === "number") {
        y = parsed; // legacy shape from before Item 14, pre-pagination
      }
    } catch (e) {
      const legacy = parseInt(saved, 10); // legacy plain-number shape
      if (Number.isFinite(legacy)) y = legacy;
    }

    // Item 14 / Fix C: replay pages loaded before the user left, so there's
    // actually something to scroll into — as a SINGLE bounded fetch instead
    // of N serial round trips. If the library has since shrunk,
    // replayLibraryPages() just returns fewer books than asked for — the
    // scrollTo below clamps to whatever height resulted.
    const gen = libraryRenderGen;
    if (libraryTotal !== null) {
      if (pages !== null) {
        if (libraryPagesLoaded < pages) await replayLibraryPages(gen, pages);
      } else if (Number.isFinite(y)) {
        // Legacy save with no known page count: grow the replay range
        // (doubling — still one request per attempt) until the document is
        // tall enough to contain `y`, or the library runs out. Never runs at
        // all if what's already rendered (page 0) is already tall enough.
        let pagesToTry = Math.max(libraryPagesLoaded, 1);
        while (true) {
          const tallEnough = document.documentElement.scrollHeight - window.innerHeight >= y;
          const exhausted = libraryPageOffset >= libraryTotal;
          if (tallEnough || exhausted) break;
          pagesToTry *= 2;
          const ok = await replayLibraryPages(gen, pagesToTry);
          if (gen !== libraryRenderGen) return;
          if (!ok) break;
        }
      }
      if (gen !== libraryRenderGen) return;
    }

    if (Number.isFinite(y)) {
      const maxY = Math.max(0, document.documentElement.scrollHeight - window.innerHeight);
      window.scrollTo(0, Math.min(y, maxY));
    }
  }

  // Item 7: keystroke search updates use history.replaceState (not a real
  // hash push) so the back button doesn't step through every character
  // typed — only the state from before the user started typing is a
  // back-stop. Shared by the debounced search input and the Esc-to-clear
  // shortcut.
  function setSearchQuery(value) {
    // Finding 3: covers the Esc-to-clear shortcut, which calls this directly
    // — the pending debounced call (if any) is now moot.
    clearTimeout(searchDebounceTimer);
    activeQuery = value;
    const hash = libraryHash(currentLibraryState());
    if (window.location.hash !== hash) history.replaceState(null, "", hash);
    refreshLibrary(value);
  }

  async function refreshLibrary(query) {
    const contentEl = $("#library-content");
    if (contentEl) contentEl.innerHTML = skeletonGridHtml();
    await loadBooks(query);
  }

  // Finding C: best-effort self-heal for a collection/series filter whose
  // underlying entity was deleted elsewhere (e.g. the desktop app) since the
  // filter bar was last rendered — without this, the active filter's fetch
  // legitimately returns zero books and the library gets stuck empty with no
  // "All Books" pill visible to escape it. Re-fetches the current
  // collections/series lists (they may have changed since the filter bar was
  // built) and reports whether the active filter's id/name is still present.
  // Never throws: an inconclusive check (network error, non-OK response)
  // reports "not missing" so a merely-empty-but-still-valid filter is left
  // alone.
  async function activeFilterEntityMissing() {
    // Finding 1: api() now throws (ApiNetworkError) instead of returning
    // null for a network failure — this check's doc comment above already
    // promises to never throw, so that must be caught here too.
    try {
      if (activeCollectionId) {
        const resp = await api("/api/collections");
        if (!resp || !resp.ok) return false;
        const collections = await resp.json();
        return !collections.some(c => c.id === activeCollectionId);
      }
      if (activeSeries) {
        const resp = await api("/api/series");
        if (!resp || !resp.ok) return false;
        const series = await resp.json();
        return !series.some(s => s.name === activeSeries);
      }
      return false;
    } catch (e) {
      return false;
    }
  }

  // Item 14: builds the `/api/books` query string shared by the first page
  // (offset 0, called from loadBooks), every subsequent page (loadNextPage),
  // and Fix C's bounded scroll-restore replay (a caller-supplied `limit`
  // wider than one page) — series/q/sort must stay identical across pages or
  // the slice boundaries wouldn't line up.
  function booksPageParams(query, offset, limit) {
    const params = new URLSearchParams();
    if (activeSeries) params.set("series", activeSeries);
    if (query) params.set("q", query);
    if (activeSort && activeSort !== "date_added") params.set("sort", activeSort);
    if (activeWantToRead) params.set("want_to_read", "true");
    params.set("limit", String(limit || LIBRARY_PAGE_SIZE));
    params.set("offset", String(offset));
    return params;
  }

  // Item 14: fetches and appends the next 60 books to the currently-rendered
  // grid. Shared by the IntersectionObserver sentinel (real scrolling) and by
  // restoreLibraryScrollPosition's page-replay. Returns `true` if a
  // non-empty page was appended, `false` on any reason to stop (already
  // loading, nothing left, a superseded/failed/network-errored request) —
  // callers use this to decide whether to keep going.
  async function loadNextPage(gen) {
    if (libraryLoadingPage) return false;
    if (libraryTotal === null || libraryPageOffset >= libraryTotal) return false;
    if (gen !== libraryRenderGen) return false;
    libraryLoadingPage = true;
    try {
      const url = "/api/books?" + booksPageParams(activeQuery, libraryPageOffset).toString();
      let resp;
      try {
        resp = await api(url);
      } catch (e) {
        return false; // best-effort — a later scroll/replay attempt can retry
      }
      if (gen !== libraryRenderGen) return false;
      if (!resp || !resp.ok) return false;
      const totalHeader = resp.headers.get("X-Total-Count");
      let pageBooks;
      try {
        pageBooks = await resp.json();
      } catch (e) {
        return false;
      }
      if (gen !== libraryRenderGen) return false;
      if (totalHeader !== null) libraryTotal = parseInt(totalHeader, 10);
      if (pageBooks.length === 0) return false;

      const contentEl = $("#library-content");
      if (contentEl) appendBooksPage(contentEl, pageBooks);
      libraryPageOffset += pageBooks.length;
      libraryPagesLoaded += 1;

      if (libraryScrollObserver && libraryTotal !== null && libraryPageOffset >= libraryTotal) {
        libraryScrollObserver.disconnect();
        libraryScrollObserver = null;
        const sentinel = contentEl && contentEl.querySelector(".library-sentinel");
        if (sentinel) sentinel.remove();
      }
      return true;
    } finally {
      // Fix A: an abandoned old-generation fetch resolving after a filter
      // change must not clear the NEW generation's in-flight guard — a
      // fresh loadNextPage() for the new gen may already be running, and
      // clearing the flag out from under it lets a second sentinel fire
      // start a duplicate concurrent fetch (same page appended twice).
      if (gen === libraryRenderGen) libraryLoadingPage = false;
    }
  }

  // Fix C: single-request replacement for the old "replay one page at a
  // time" loop in restoreLibraryScrollPosition — fetches the whole replay
  // range in one round trip (offset 0, limit = pagesToReplay * page size)
  // and replaces the grid content with it, instead of N serial
  // loadNextPage() calls. Updates the same pagination bookkeeping loadBooks/
  // loadNextPage rely on so setupInfiniteScroll() picks up from the right
  // offset afterward. Returns `true` on success, `false` on any reason to
  // stop (superseded gen, network/HTTP/parse failure) — the caller then
  // falls back to whatever was already rendered.
  async function replayLibraryPages(gen, pagesToReplay) {
    if (gen !== libraryRenderGen) return false;
    const limit = pagesToReplay * LIBRARY_PAGE_SIZE;
    let resp;
    try {
      resp = await api("/api/books?" + booksPageParams(activeQuery, 0, limit).toString());
    } catch (e) {
      return false;
    }
    if (gen !== libraryRenderGen) return false;
    if (!resp || !resp.ok) return false;
    const totalHeader = resp.headers.get("X-Total-Count");
    let pageBooks;
    try {
      pageBooks = await resp.json();
    } catch (e) {
      return false;
    }
    if (gen !== libraryRenderGen) return false;

    const contentEl = $("#library-content");
    const grid = contentEl && contentEl.querySelector(".grid");
    if (grid) {
      grid.innerHTML = pageBooks.map(bookCardHtml).join("");
      bindCardHandlersOn(Array.from(grid.children));
    }
    if (totalHeader !== null) libraryTotal = parseInt(totalHeader, 10);
    libraryPageOffset = pageBooks.length;
    libraryPagesLoaded = Math.max(1, Math.ceil(pageBooks.length / LIBRARY_PAGE_SIZE));
    return true;
  }

  // Item 14: appends a page of cards as raw DOM nodes into the existing
  // `.grid` element and binds handlers on only those new nodes — re-running
  // bindGridCardHandlers() here would double-bind every already-rendered
  // card (each call adds a fresh closure-based listener).
  function appendBooksPage(contentEl, books) {
    const grid = contentEl.querySelector(".grid");
    if (!grid) return;
    const beforeCount = grid.children.length;
    grid.insertAdjacentHTML("beforeend", books.map(bookCardHtml).join(""));
    bindCardHandlersOn(Array.from(grid.children).slice(beforeCount));
  }

  // Item 14: appends the IntersectionObserver sentinel after the grid and
  // wires it to loadNextPage(). No-op when the current grid isn't paginated
  // (libraryTotal === null — the collections endpoint, or a legacy server
  // response with no X-Total-Count header) or already fully loaded.
  function setupInfiniteScroll(contentEl) {
    if (!contentEl || libraryTotal === null || libraryPageOffset >= libraryTotal) return;
    const grid = contentEl.querySelector(".grid");
    if (!grid) return;
    const sentinel = document.createElement("div");
    sentinel.className = "library-sentinel";
    sentinel.setAttribute("aria-hidden", "true");
    grid.insertAdjacentElement("afterend", sentinel);

    const gen = libraryRenderGen;
    libraryScrollObserver = new IntersectionObserver((entries) => {
      if (entries.some((e) => e.isIntersecting)) loadNextPage(gen);
    }, { rootMargin: "600px" });
    libraryScrollObserver.observe(sentinel);
  }

  // Fix 1 / Item 15: canonical per-book progress resolver, shared by the
  // grid (bookCardHtml) and the Continue Reading shelf (shelfCardHtml) — one
  // source of truth for "what should this book's badge show". Mirrors
  // mergeProgress's "local wins only if newer" rule: `serverRow` is the raw
  // `/api/reading-progress` row (or undefined if the book has none), and a
  // book this tab read more recently (lastKnownProgress) only overrides it
  // when its `.ts` is newer than the server row's `last_read_at` — otherwise
  // a book finished on another device would keep showing this tab's stale
  // local value. Also treats chapter_index 0 as "no progress" (same
  // convention as showDetail's `hasProgress` and get_continue_reading_books'
  // `chapter_index > 0` filter), so resetProgress's locally-recorded 0
  // resolves to no badge instead of a near-empty one. Returns undefined when
  // there's nothing to show.
  function effectiveProgress(id, serverRow) {
    const known = lastKnownProgress[id];
    const serverTs = serverRow ? serverRow.last_read_at * 1000 : 0;
    const chapterIndex = known && known.ts >= serverTs
      ? known.chapterIndex
      : (serverRow ? serverRow.chapter_index : undefined);
    return typeof chapterIndex === "number" && chapterIndex > 0 ? chapterIndex : undefined;
  }

  // Item 15: best-effort bulk progress fetch — a failure or slow response
  // must never block or break the grid, it just renders without badges (same
  // resilience pattern as the shelves' fetchShelfBooks). Guarded by `gen` so
  // a slow fetch from a superseded loadBooks() call can't clobber a newer
  // one's progressByBook map.
  // Fix 2 / Fix 5: never rejects and never clobbers existing badges with an
  // empty map on failure — `rows` is only trusted (and progressByBook only
  // replaced) once it's confirmed to be an array; a network error, a
  // non-ok response, or a malformed (non-array) body all just leave
  // progressByBook exactly as it was.
  async function refreshProgressByBook(gen) {
    let rows;
    try {
      const resp = await api("/api/reading-progress");
      rows = resp && resp.ok ? await resp.json() : undefined;
    } catch (e) {
      rows = undefined; // best-effort — keep whatever's already cached
    }
    if (gen !== libraryRenderGen) return;
    if (!Array.isArray(rows)) return;

    // Fix 2: belt-and-suspenders — a malformed row (e.g. `null` in the
    // array) must not throw and reject this promise; loadBooks' unguarded
    // `await progressPromise` must always resolve. Falls back to keeping the
    // existing map, same as any other best-effort failure above.
    try {
      // Fix 1: resolve every book that has either a server row or a local
      // (F8) record — a book read this tab whose PUT hasn't landed
      // server-side yet has no server row at all, but still needs to show up.
      const serverByBook = new Map(rows.map(p => [p.book_id, p]));
      const ids = new Set([...serverByBook.keys(), ...Object.keys(lastKnownProgress)]);
      const merged = new Map();
      for (const id of ids) {
        const value = effectiveProgress(id, serverByBook.get(id));
        if (value !== undefined) merged.set(id, value);
      }
      progressByBook = merged;
    } catch (e) {
      // best-effort — keep whatever's already cached
    }
  }

  async function loadBooks(query, refreshProgress) {
    // Item 5: captured once, checked after every await below — see the
    // libraryRenderGen declaration for why.
    const gen = ++libraryRenderGen;
    // Item 14: every loadBooks() call is a fresh library entry (filter/sort/
    // search change, or a plain re-render) — always starts back at page 0.
    resetLibraryPagination();

    // Fix 6: the bulk progress table only needs re-fetching when actually
    // entering the library fresh (from detail/reader/stats/collections, or
    // the very first load) — a pure sort/search/filter re-render reuses the
    // cached progressByBook untouched. A book just read in this tab is still
    // reflected immediately regardless, via the F8 lastKnownProgress overlay
    // effectiveProgress() applies whenever progressByBook IS rebuilt.
    // Item 15: kicked off in parallel with the books fetch below and awaited
    // just before the first render — skeletons already cover the wait, so
    // paint once with badges rather than painting bar-less cards and
    // repainting when progress arrives (avoids a layout shift).
    const progressPromise = (refreshProgress || progressByBook.size === 0)
      ? refreshProgressByBook(gen)
      : Promise.resolve();
    // Offline badges: same paint-once-with-badges rationale as the progress
    // bar above; a failed read just leaves the previous set.
    const offlineIdsPromise = offlineSupported() ? refreshOfflineBookIds().catch(() => {}) : Promise.resolve();

    let url;
    let paginated = false;
    if (activeCollectionId) {
      // Item 14: the collections endpoint stays unpaginated (out of scope —
      // collections are small; see docs/web-ui-improvements.md Item 14). M4:
      // q/want_to_read now filter server-side (mirroring /api/books), so the
      // client-side filtering that used to run over the full result below
      // is gone.
      const collectionParams = new URLSearchParams();
      if (query) collectionParams.set("q", query);
      if (activeWantToRead) collectionParams.set("want_to_read", "true");
      const collectionQs = collectionParams.toString();
      url =
        "/api/collections/" + encodeURIComponent(activeCollectionId) + "/books" +
        (collectionQs ? "?" + collectionQs : "");
    } else {
      paginated = true;
      url = "/api/books?" + booksPageParams(query, 0).toString();
    }

    // Item 10: the main library grid is the one fetch in this file whose
    // failure must be loud. Finding 3: a load that resolves after the user
    // has already navigated away from the library (to detail/reader/stats/
    // collections) must not toast or error-render over whatever view they're
    // looking at now — `libraryRenderGen` only catches a *newer library*
    // request superseding this one, not "left the library entirely", so
    // every failure branch below also checks `currentView === "library"`.
    let resp;
    try {
      resp = await api(url);
    } catch (e) {
      // Finding 1: api() throws ApiNetworkError for a network failure
      // instead of returning null (that's reserved for an already-handled
      // 401 — see the plain `if (!resp)` branch below).
      if (gen !== libraryRenderGen) return;
      if (currentView === "library") showLibraryLoadError(apiFailureToastMessage(e));
      return;
    }
    if (gen !== libraryRenderGen) return;
    if (!resp) return; // handled 401 — showLogin() already ran.
    if (!resp.ok) {
      // Finding 4: surface the actual status instead of a generic
      // "couldn't reach the server" — the server IS reachable here.
      if (currentView === "library") showLibraryLoadError(httpErrorToastMessage(resp.status));
      return;
    }
    // Item 14: read the total before consuming the body. Absent whenever
    // pagination isn't in play (collections) or the server hasn't been
    // rebuilt yet with the `limit`/`offset` change (graceful degradation —
    // treat the returned array as the complete set, no sentinel).
    const totalHeader = paginated ? resp.headers.get("X-Total-Count") : null;
    let books;
    try {
      books = await resp.json();
    } catch (e) {
      if (currentView === "library") showLibraryLoadError("Unexpected response");
      return;
    }
    if (gen !== libraryRenderGen) return;
    if (paginated && totalHeader !== null) {
      libraryTotal = parseInt(totalHeader, 10);
      libraryPageOffset = books.length;
      libraryPagesLoaded = 1;
    }

    // Item 15: every render path below (filtered grid, plain grid, shelves)
    // goes through bookCardHtml, which reads progressByBook — wait for it
    // here so the very first paint already has badges.
    // Fix 2: belt-and-suspenders — refreshProgressByBook is guaranteed to
    // resolve (never reject), but this is the one await in loadBooks NOT
    // otherwise wrapped in try/catch, so a rejection here would skip the
    // grid render entirely. Guard it anyway rather than rely solely on that
    // guarantee holding forever.
    try {
      await progressPromise;
    } catch (e) {
      // best-effort — render proceeds with whatever progressByBook already has
    }
    await offlineIdsPromise; // already .catch-guarded above
    if (gen !== libraryRenderGen) return;

    // Finding C: an empty result for an active collection/series filter is
    // ambiguous — genuinely empty, or the filtered entity no longer exists?
    // Only the latter needs healing; a real empty collection should still
    // render as "No books found" with its filter pill active.
    if ((activeCollectionId || activeSeries) && books.length === 0) {
      const missing = await activeFilterEntityMissing();
      if (gen !== libraryRenderGen) return;
      if (missing) {
        // Item 7: URL is the source of truth — heal by navigating to the
        // hash with the dead filter dropped (a real hashchange re-runs
        // showLibrary/loadBooks), rather than mutating the parse-cache vars
        // directly and re-rendering out of step with the address bar.
        // Finding 6: also refresh the filter bar itself — otherwise the now
        // -deleted collection/series would linger in the dropdown lists
        // (built from the stale cachedCollections/cachedSeries) until some
        // unrelated full re-render.
        renderFilterBar();
        navigate(libraryHash({ ...currentLibraryState(), series: null, collection: null }));
        return;
      }
    }

    // M4: `q`/`want_to_read` now filter server-side for a collection too (see
    // the `loadBooks` url-building above), so `books` here is already the
    // filtered set — no client-side re-filtering needed.

    // Item 5: shelves only appear on the unfiltered "home" view — any active
    // search/series/collection filter (or an empty library) falls back to
    // the plain grid, matching the pre-Item-5 behavior exactly.
    const showShelves = !query && !activeCollectionId && !activeSeries && !activeWantToRead && books.length > 0;

    // Finding F: render the plain grid as soon as the main books fetch
    // resolves. The shelves below are strictly best-effort decoration on top
    // of it — a shelf-fetch failure (network error, non-OK response) must
    // never leave the page stuck on "Loading".
    renderBooks(books);
    if (!showShelves) {
      await restoreLibraryScrollPosition();
      if (gen === libraryRenderGen) setupInfiniteScroll($("#library-content"));
      return;
    }

    // Fix B: each shelf fetch is independent — never lets one shelf's
    // failure (network error, non-OK response, bad JSON) suppress the
    // other. A `Promise.all` of two bare api() calls would reject (and
    // abort both shelves) if either one rejected; this never rejects, so a
    // failed shelf just renders empty while the other still shows.
    async function fetchShelfBooks(url) {
      try {
        const resp = await api(url);
        return resp && resp.ok ? await resp.json() : [];
      } catch (e) {
        return []; // best-effort — this shelf just doesn't render
      }
    }

    // Item 14: `books` is now only page 0 of the "All Books" grid (up to 60
    // items), not the full library. Fix E: on the default date_added sort,
    // page 0 is already the most-recently-added books in date order, so
    // "Recently Added" is just the first 12 of it — only fall back to the
    // dedicated date_added-sorted fetch when a different sort is active.
    const [continueBooks, wantBooks, recentBooks] = await Promise.all([
      fetchShelfBooks("/api/books/continue-reading?limit=12"),
      fetchShelfBooks("/api/books?want_to_read=true&limit=12"),
      activeSort === "date_added"
        ? Promise.resolve(books.slice(0, 12))
        : fetchShelfBooks("/api/books?sort=date_added&limit=12&offset=0"),
    ]);
    if (gen !== libraryRenderGen) return;

    renderLibraryWithShelves(books, continueBooks, recentBooks, wantBooks);
    // Finding 5: restore once the layout has settled into its final shape
    // (grid-only or grid+shelves) — restoring earlier, right after the plain
    // grid render, would be undone by the shelves rendering on top of it.
    await restoreLibraryScrollPosition();
    if (gen === libraryRenderGen) setupInfiniteScroll($("#library-content"));
  }

  // Item 10: friendly, context-specific empty states instead of a bare "No
  // books found" — the message depends on *why* the grid is empty (nothing
  // imported yet vs. a search/collection/series that matched nothing).
  function libraryEmptyMessageHtml() {
    if (activeQuery) return `<div class="empty">No books match "${esc(activeQuery)}"</div>`;
    if (activeCollectionId) return '<div class="empty">This collection is empty.</div>';
    if (activeSeries) return '<div class="empty">No books found in this series.</div>';
    if (activeWantToRead) return '<div class="empty">No books marked &ldquo;Want to read&rdquo;.</div>';
    return '<div class="empty">Your library is empty. Import some books to get started.</div>';
  }

  // Finding 4: `message` is the differentiated toast text (network vs. HTTP
  // vs. parse failure) chosen by loadBooks' catch/status branches; callers
  // that don't care (none left) would fall back to the network wording.
  function showLibraryLoadError(message) {
    const contentEl = $("#library-content");
    if (contentEl) contentEl.innerHTML = '<div class="empty">Couldn&rsquo;t load your library.</div>';
    showToast(message || "Couldn't reach Carrel server");
  }

  // Item 15: same `.shelf-progress`/`.shelf-progress-fill` bar the shelf
  // cards use — no new visual language. Absent from `progressByBook` means
  // no effective progress (no row, or resolved to "no progress" — see
  // effectiveProgress), so no bar at all (not even at 0%). Fix 3: also
  // requires `total_chapters > 0` — matches get_continue_reading_books'
  // exclusion, so a book whose chapter count isn't known yet never shows the
  // otherwise-always-0% bar `progressPercent` would produce for it.
  function bookCardHtml(b) {
    const chapterIndex = progressByBook.get(b.id);
    const bar = progressBarHtml(chapterIndex, b.total_chapters);
    // Mirrors progressByBook: offlineBookIds is refreshed by showLibrary and
    // kept current by save/unsave, so the badge always reflects a real
    // manifest row — never an assumption.
    const offlineBadge = offlineBookIds.has(b.id)
      ? '<span class="offline-badge" title="Available offline" aria-label="Available offline">⤓</span>'
      : "";
    // Read-only indicator (the web card has no toggle — the flag is set on the
    // book detail view). Renders straight from the fetched server value.
    const wantBadge = b.want_to_read
      ? '<span class="want-badge" title="Want to read" aria-label="Want to read">🔖</span>'
      : "";
    return `
      <div class="card" data-id="${b.id}" tabindex="0" role="button" aria-label="${esc(`Open ${b.title}`)}">
        <img src="/api/books/${b.id}/cover?size=thumb" alt="" loading="lazy" data-cover-title="${esc(b.title)}">${offlineBadge}${wantBadge}
        <div class="info">
          <div class="title" title="${esc(b.title)}">${esc(b.title)}</div>
          <div class="author">${esc(b.author)}</div>
          <div class="format">${b.format}</div>
        </div>${bar}
      </div>`;
  }

  function gridHtml(books) {
    if (books.length === 0) return libraryEmptyMessageHtml();
    return '<div class="grid">' + books.map(bookCardHtml).join("") + '</div>';
  }

  // Item 10: single onerror mechanism shared by every cover site (grid card,
  // shelf card, detail cover) — replaces a broken/missing cover `<img>` with
  // a styled placeholder (surface bg, serif title) instead of leaving a
  // broken-image icon over a gray box.
  function bindCoverFallback(img) {
    img.addEventListener("error", () => {
      const div = document.createElement("div");
      div.className = "cover-placeholder";
      div.innerHTML = `<span>${esc(img.dataset.coverTitle || "")}</span>`;
      img.replaceWith(div);
    }, { once: true });
  }

  function openBookFromCard(id) {
    saveLibraryScrollPosition();
    navigate("#/book/" + id);
  }

  // Item 14: factored out of bindGridCardHandlers so appendBooksPage() can
  // bind only the newly-inserted cards from an infinite-scroll page load —
  // re-running the old document-wide `$$(".card")` version on every append
  // would attach a second set of listeners to every already-bound card.
  function bindCardHandlersOn(cards) {
    cards.forEach(c => {
      c.addEventListener("click", () => openBookFromCard(c.dataset.id));
      c.addEventListener("keydown", (e) => {
        if (e.key === "Enter" || e.key === " ") { e.preventDefault(); openBookFromCard(c.dataset.id); }
      });
      const img = c.querySelector("img");
      if (img) bindCoverFallback(img);
    });
  }

  function bindGridCardHandlers() {
    bindCardHandlersOn(Array.from($$(".card")));
  }

  function renderBooks(books) {
    const contentEl = $("#library-content");
    if (contentEl) contentEl.innerHTML = gridHtml(books);
    bindGridCardHandlers();
  }

  // Item 5: percent = (chapter_index+1)/total_chapters — the same math the
  // detail page (showDetail) and the desktop app use for progress bars.
  function progressPercent(chapterIndex, totalChapters) {
    return totalChapters > 0 ? Math.round(((chapterIndex + 1) / totalChapters) * 100) : 0;
  }

  // Fix 7: single source for the `.shelf-progress`/`.shelf-progress-fill`
  // bar markup, shared by bookCardHtml and shelfCardHtml (previously
  // duplicated verbatim in both). Fix 3: no bar at all — not even at 0% —
  // when there's no effective progress to show or the chapter/page count
  // isn't known yet (`total_chapters <= 0`, matching
  // get_continue_reading_books' exclusion).
  function progressBarHtml(chapterIndex, totalChapters) {
    if (!(totalChapters > 0) || chapterIndex === undefined || chapterIndex === null) return "";
    return `<div class="shelf-progress"><div class="shelf-progress-fill" style="width:${progressPercent(chapterIndex, totalChapters)}%"></div></div>`;
  }

  // `mode: "continue"` cards jump straight into the reader at the saved
  // position; `mode: "detail"` (Recently Added) cards behave like a normal
  // grid card and open the detail page.
  function shelfCardHtml(b, mode) {
    // Fix 4: consult the same canonical progressByBook map the grid uses —
    // only fall back to this shelf fetch's own (potentially stale, or from
    // before an F8 update) `b.chapter_index` when the book isn't in the map
    // at all, so a book shown in both the shelf and the grid never disagrees.
    const chapterIndex = mode === "continue"
      ? (progressByBook.has(b.id) ? progressByBook.get(b.id) : b.chapter_index)
      : undefined;
    const bar = mode === "continue" ? progressBarHtml(chapterIndex, b.total_chapters) : "";
    const posAttrs = mode === "continue"
      ? ` data-chapter-index="${b.chapter_index}" data-scroll-position="${b.scroll_position || 0}" data-last-read-at="${b.last_read_at || 0}"`
      : "";
    return `
      <div class="shelf-card" data-id="${b.id}" data-mode="${mode}"${posAttrs} tabindex="0" role="button" aria-label="${esc(`Open ${b.title}`)}">
        <img src="/api/books/${b.id}/cover?size=thumb" alt="" loading="lazy" data-cover-title="${esc(b.title)}">
        <div class="shelf-title" title="${esc(b.title)}">${esc(b.title)}</div>
        ${bar}
      </div>`;
  }

  function shelfSectionHtml(title, books, mode) {
    if (books.length === 0) return "";
    return `
      <div class="shelf-section">
        <h2 class="shelf-heading">${esc(title)}</h2>
        <div class="shelf-row">${books.map(b => shelfCardHtml(b, mode)).join("")}</div>
      </div>`;
  }

  function activateShelfCard(c) {
    saveLibraryScrollPosition();
    const id = c.dataset.id;
    if (c.dataset.mode === "continue") {
      // Finding B: the shelf's position was fetched at page-load time and
      // may be stale by the time it's clicked (a later save — including
      // one still in flight via the un-awaited debounced PUT — can have
      // moved this tab's actual position on). Reconcile with
      // lastKnownProgress via the same mergeProgress() the detail page's
      // Continue button uses, instead of trusting the baked-in data
      // attributes outright — same shared code path, no duplicated logic.
      const serverProgress = {
        book_id: id,
        chapter_index: parseInt(c.dataset.chapterIndex, 10) || 0,
        scroll_position: parseFloat(c.dataset.scrollPosition) || 0,
        last_read_at: parseInt(c.dataset.lastReadAt, 10) || 0,
      };
      const merged = mergeProgress(id, serverProgress) || serverProgress;
      const chapterIndex = merged.chapter_index;
      const scrollPosition = typeof merged.scroll_position === "number" ? merged.scroll_position : 0;
      readerEntryIntent = { id, action: "continue", scrollPosition };
      navigate(`#/book/${id}/${chapterIndex}/read`);
    } else {
      navigate("#/book/" + id);
    }
  }

  function bindShelfCardHandlers() {
    $$(".shelf-card").forEach(c => {
      c.addEventListener("click", () => activateShelfCard(c));
      c.addEventListener("keydown", (e) => {
        if (e.key === "Enter" || e.key === " ") { e.preventDefault(); activateShelfCard(c); }
      });
    });
    $$(".shelf-card img").forEach(bindCoverFallback);
  }

  function renderLibraryWithShelves(allBooks, continueBooks, recentBooks, wantBooks) {
    const contentEl = $("#library-content");
    if (!contentEl) return;

    let html = shelfSectionHtml("Continue Reading", continueBooks, "continue");
    // "Want to read" cards open the detail view (like Recently Added). The
    // shelf renders only when non-empty (shelfSectionHtml returns "" for []),
    // and only on the unfiltered home view (the showShelves guard upstream).
    html += shelfSectionHtml("Want to read", wantBooks || [], "detail");
    html += shelfSectionHtml("Recently Added", recentBooks, "detail");
    html += '<h2 class="shelf-heading all-books-heading">All Books</h2>';
    html += gridHtml(allBooks);

    contentEl.innerHTML = html;
    bindShelfCardHandlers();
    bindGridCardHandlers();
  }

  // ── Detail ────────────────────────────────────
  // F6: true while `location.hash` still targets this book's detail route.
  // Re-checked after every await in showDetail so a slow response + Back
  // doesn't let a stale render clobber whatever the user navigated to.
  function hashTargetsDetail(id) {
    return (window.location.hash || "#") === `#/book/${id}`;
  }

  // F8: prefer this tab's own more-recent record of a book's progress over a
  // server GET that may have raced an in-flight PUT (e.g. right after
  // leaving the reader). Falls back to the server value when it's absent or
  // actually newer (e.g. progress made on another device/the desktop app).
  function mergeProgress(id, serverProgress) {
    const known = lastKnownProgress[id];
    if (!known) return serverProgress;
    const serverTs = serverProgress ? serverProgress.last_read_at * 1000 : 0;
    if (known.ts < serverTs) return serverProgress;
    return {
      book_id: id,
      chapter_index: known.chapterIndex,
      scroll_position: known.scrollPosition,
      last_read_at: Math.floor(known.ts / 1000),
    };
  }

  // Item 8 / Finding K: resolves this book's position among its series
  // siblings. `siblings` is the raw `/api/books?series=X` result
  // (BookGridItem[]). If ANY sibling is missing a volume number, the whole
  // series sorts by title instead — a partial volume sort (numbered books
  // first, nulls tacked on after by title) would misplace an unnumbered book
  // that actually belongs earlier in reading order. Title (then id) is
  // always the tie-breaker, so identical/duplicate volume numbers still sort
  // deterministically instead of depending on the API's row order. Returns
  // null if `id` isn't found in the list (e.g. a race with metadata edited
  // elsewhere).
  function resolveSeriesNav(siblings, id) {
    if (!siblings || siblings.length === 0) return null;
    const anyMissingVolume = siblings.some(b => b.volume == null);
    const sorted = siblings.slice().sort((a, b) => {
      if (!anyMissingVolume && a.volume !== b.volume) return a.volume - b.volume;
      return a.title.localeCompare(b.title) || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0);
    });
    const index = sorted.findIndex(b => b.id === id);
    if (index === -1) return null;
    return {
      position: index + 1,
      total: sorted.length,
      prevId: index > 0 ? sorted[index - 1].id : null,
      nextId: index < sorted.length - 1 ? sorted[index + 1].id : null,
    };
  }

  function formatFileSize(bytes) {
    if (typeof bytes !== "number" || !isFinite(bytes)) return null;
    const units = ["B", "KB", "MB", "GB", "TB"];
    let i = 0;
    let val = bytes;
    while (val >= 1024 && i < units.length - 1) { val /= 1024; i++; }
    const precision = i === 0 ? 0 : (val < 10 ? 1 : 0);
    return val.toFixed(precision) + " " + units[i];
  }

  function formatAddedDate(unixSeconds) {
    if (!unixSeconds) return null;
    try {
      return new Date(unixSeconds * 1000).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
    } catch (e) { return null; }
  }

  // Read-only star rating (1-5, rounded) — mirrors the desktop app's
  // StarRating display scale.
  function starsHtml(rating) {
    if (typeof rating !== "number") return "";
    const rounded = Math.max(0, Math.min(5, Math.round(rating)));
    let stars = "";
    for (let i = 1; i <= 5; i++) stars += i <= rounded ? "★" : "☆";
    return `<span class="detail-rating" title="Rating: ${rounded} of 5" aria-label="Rating: ${rounded} out of 5 stars">${stars}</span>`;
  }

  // The one write surface for the flag on the web UI (the cards are
  // read-only). `aria-pressed` carries the on/off state to assistive tech;
  // the label + CSS reflect it visually.
  function wantToReadBtnLabel(on) {
    return `🔖 ${on ? "In Want to read" : "Want to read"}`;
  }
  function wantToReadBtnHtml(on) {
    return `<button type="button" class="btn-secondary want-to-read-btn" id="want-btn" aria-pressed="${on ? "true" : "false"}">${wantToReadBtnLabel(on)}</button>`;
  }

  function seriesNavHtml(book, nav) {
    if (!book.series) return "";
    if (!nav) return `<p class="detail-series-line">Series: ${esc(book.series)}</p>`;
    const prevBtn = nav.prevId ? `<button class="btn-secondary" id="series-prev-btn">&larr; Prev</button>` : "";
    const nextBtn = nav.nextId ? `<button class="btn-secondary" id="series-next-btn">Next &rarr;</button>` : "";
    return `
      <div class="detail-series">
        <a href="#" class="series-link" id="series-link">Series: ${esc(book.series)} (${nav.position}/${nav.total})</a>
        ${(prevBtn || nextBtn) ? `<div class="series-nav-buttons">${prevBtn}${nextBtn}</div>` : ""}
      </div>`;
  }

  function progressBlockHtml(book, progress, hasProgress, isPageBased) {
    if (!hasProgress) return "";
    const total = book.total_chapters || 0;
    const current = progress.chapter_index + 1;
    const unit = isPageBased ? "Page" : "Chapter";
    // Finding G: total_chapters=0 means the page/chapter count isn't known
    // (yet) — "Chapter N of 0 · NaN%" is meaningless, so just show the
    // current position with no total/percent/bar.
    if (total <= 0) {
      return `<div class="detail-progress"><div class="detail-progress-label">${esc(`${unit} ${current}`)}</div></div>`;
    }
    const pct = progressPercent(progress.chapter_index, total);
    return `
      <div class="detail-progress">
        <div class="detail-progress-bar"><div class="detail-progress-fill" style="width:${pct}%"></div></div>
        <div class="detail-progress-label">${esc(`${unit} ${current} of ${total} · ${pct}%`)}</div>
      </div>`;
  }

  // Inline SVGs for the detail action row (primary button + overflow menu).
  // No icon library / build step — hand-authored, colored via currentColor.
  const DETAIL_ICONS = {
    play: '<svg class="mi" viewBox="0 0 24 24" aria-hidden="true"><path d="M7 5l12 7-12 7z" fill="currentColor"/></svg>',
    restart: '<svg class="mi" viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 5v14L8 12z"/><line x1="6" y1="5" x2="6" y2="19"/></svg>',
    cloud: '<svg class="mi" viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M7 18a4 4 0 010-8 5 5 0 019.6-1.3A3.5 3.5 0 0117 18"/><path d="M12 11v6m0 0l-2.5-2.5M12 17l2.5-2.5"/></svg>',
    download: '<svg class="mi" viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12m0 0l-4-4m4 4l4-4"/><path d="M5 21h14"/></svg>',
    trash: '<svg class="mi" viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13"/></svg>',
    more: '<svg class="mi" viewBox="0 0 24 24" aria-hidden="true" fill="currentColor"><circle cx="5" cy="12" r="2"/><circle cx="12" cy="12" r="2"/><circle cx="19" cy="12" r="2"/></svg>',
  };

  // Close the detail-view overflow menu (if open). Bound once at module scope
  // (outside-click + Esc) and reused by showDetail's toggle handler.
  function closeDetailMenu() {
    const menu = $("#detail-menu");
    const btn = $("#detail-more-btn");
    if (menu) menu.hidden = true;
    if (btn) btn.setAttribute("aria-expanded", "false");
  }

  async function showDetail(id) {
    currentView = "detail";
    flushProgressSave();
    readerState = null;
    resumePromptActive = false;
    app().innerHTML = detailSkeletonHtml();

    // Finding 1: the book fetch is the one thing this page can't render
    // without — unlike the best-effort progress/series fetches below, its
    // failure (network error, non-2xx, bad JSON) must replace the skeleton
    // with a visible message + toast instead of leaving it spinning forever.
    let resp;
    try {
      resp = await api("/api/books/" + id);
    } catch (e) {
      if (!hashTargetsDetail(id)) return;
      renderViewError(apiFailureToastMessage(e));
      showToast(apiFailureToastMessage(e));
      return;
    }
    if (!resp || !hashTargetsDetail(id)) return;
    if (!resp.ok) {
      renderViewError(`Couldn't load this book (HTTP ${resp.status})`);
      showToast(httpErrorToastMessage(resp.status));
      return;
    }
    let book;
    try {
      book = await resp.json();
    } catch (e) {
      renderViewError("Couldn't load this book (unexpected response)");
      showToast("Unexpected response");
      return;
    }
    if (!hashTargetsDetail(id)) return;

    const isHtmlBook = book.format === "epub" || book.format === "mobi";
    const isPageBased = ["pdf", "cbz", "cbr"].includes(book.format);
    const isReadable = isHtmlBook || isPageBased;
    const readHash = isReadable ? `#/book/${id}/0/read` : "";

    // Item 4: a book with saved progress > 0 gets Continue (jumps straight to
    // the saved position) + Start Over instead of a plain Read button. A
    // progress fetch that 404s/errors is treated the same as "no progress" —
    // never blocks the detail page from rendering.
    // Item 8: series prev/next, resolved client-side from the series's full
    // book list. Best-effort — a failed fetch just omits the nav buttons.
    // Finding I: these two fetches only depend on the book response already
    // in hand, not on each other — run them concurrently instead of one
    // after the other.
    // Finding 1: these two are genuinely best-effort (per the comments
    // above) — a network failure here must degrade the same way a 404/500
    // already does (no progress / no series nav), not abort the whole page.
    // `.catch(() => undefined)` keeps that network-error outcome distinct
    // from the `null` api() returns for an already-handled 401, which still
    // must abort (see the F5/F6 check below).
    const [progResp, seriesResp] = await Promise.all([
      isReadable ? api(`/api/books/${id}/progress`).catch(() => undefined) : Promise.resolve(null),
      book.series ? api(`/api/books?series=${encodeURIComponent(book.series)}`).catch(() => undefined) : Promise.resolve(null),
    ]);
    // F5/F6: a null response for a fetch that was actually made means api()
    // already redirected to the login screen (401) — continuing would render
    // the detail page over it.
    if (isReadable && progResp === null) return;
    if (book.series && seriesResp === null) return;
    if (!hashTargetsDetail(id)) return;

    let progress = null;
    if (isReadable) {
      if (progResp && progResp.ok) {
        try { progress = await progResp.json(); } catch (e) { progress = null; }
        if (!hashTargetsDetail(id)) return;
        // A successful server read is a fresh observation of the server's
        // position — advance the offline sync baseline so a later offline
        // edit's replay compares against what we actually saw, not a stale
        // save-time value (else replay could wrongly discard a valid edit).
        if (progress && progress.last_read_at) updateOfflineBaseline(id, progress.last_read_at);
      }
      progress = mergeProgress(id, progress);
    }
    const hasProgress = !!(progress && progress.chapter_index > 0);
    const continueHash = isReadable ? `#/book/${id}/${progress ? progress.chapter_index : 0}/read` : "";

    let seriesNav = null;
    if (book.series && seriesResp && seriesResp.ok) {
      try {
        seriesNav = resolveSeriesNav(await seriesResp.json(), id);
      } catch (e) { seriesNav = null; }
      if (!hashTargetsDetail(id)) return;
    }

    // The reading action is the always-visible accent primary button; a book
    // with saved progress > 0 gets "Continue" (jumps to the saved position),
    // otherwise "Read" (from the start). Both carry the ▶ play icon.
    let primaryHtml = "";
    if (isReadable) {
      primaryHtml = hasProgress
        ? `<button class="btn-primary detail-primary" id="continue-btn">${DETAIL_ICONS.play}Continue</button>`
        : `<button class="btn-primary detail-primary" id="read-btn">${DETAIL_ICONS.play}Read</button>`;
    }

    // Everything else lives in the "More" overflow menu as icon+label rows,
    // in order: Start Over (only with progress), the offline state row(s),
    // then Download (always). The ids/handlers are unchanged from the old flat
    // row — the controls are relocated and re-skinned, not reimplemented.
    let menuItems = "";
    if (isReadable && hasProgress) {
      menuItems += `<button class="detail-menu-item" role="menuitem" id="restart-btn">${DETAIL_ICONS.restart}Start Over</button>`;
    }

    // Offline save/remove (readable books on secure contexts only). The
    // manifest read is the source of truth — the row never claims a state
    // storage doesn't hold. Its state machine (Save offline → Saving…/Cancel →
    // Saved · <size> + Remove) is preserved verbatim; only the markup wrapper
    // and icon changed.
    let offlineRow;
    if (isReadable && offlineSupported()) {
      offlineRow = await getOfflineManifest(id);
      if (!hashTargetsDetail(id)) return;
      const offlineSaveable = !isHtmlBook || book.total_chapters > 0;
      if (offlineRow) {
        menuItems += `<span class="detail-menu-info offline-saved-label">Saved · ${esc(formatFileSize(offlineRow.bytes) || "")}</span><button class="detail-menu-item" role="menuitem" id="offline-remove-btn">${DETAIL_ICONS.trash}Remove offline copy</button><span class="detail-menu-info offline-usage" id="offline-usage" role="status"></span>`;
      } else if (activeOfflineSaves[id]) {
        // A save started from a previous render of this page is still
        // running — never offer a second concurrent one, but do offer a
        // Cancel (the running save's own catch re-renders on completion).
        menuItems += `<button class="detail-menu-item" disabled>${DETAIL_ICONS.cloud}Saving…</button><button class="detail-menu-item" role="menuitem" id="offline-cancel-btn">Cancel</button>`;
      } else if (offlineSaveable) {
        menuItems += `<button class="detail-menu-item" role="menuitem" id="offline-save-btn">${DETAIL_ICONS.cloud}Save offline</button>`;
      }
      // Chapter-mode book with unknown chapter count: no save affordance at
      // all — the engine would (correctly) refuse it.
    }

    // Download is available for every book (readable or not).
    menuItems += `<a class="detail-menu-item" role="menuitem" href="/api/books/${id}/download">${DETAIL_ICONS.download}Download</a>`;

    const moreHtml = `<span class="detail-more">
      <button class="btn-secondary detail-more-btn" id="detail-more-btn" aria-haspopup="menu" aria-expanded="false" aria-label="More actions">${DETAIL_ICONS.more}</button>
      <div class="detail-menu" id="detail-menu" role="menu" hidden>${menuItems}</div>
    </span>`;

    const facts = [];
    if (book.total_chapters) facts.push(`${book.total_chapters} ${isPageBased ? "pages" : "chapters"}`);
    const sizeStr = formatFileSize(book.file_size);
    if (sizeStr) facts.push(sizeStr);
    const dateStr = formatAddedDate(book.added_at);
    if (dateStr) facts.push(`Added ${dateStr}`);
    const factsHtml = facts.length ? `<p class="detail-facts">${esc(facts.join(" · "))}</p>` : "";

    document.title = `${book.title} — Carrel`;
    app().innerHTML = `
      <div class="header">
        <button class="back-btn" id="back-btn" aria-label="Back">&larr;</button>
        <h1 class="nav-book-title">${esc(book.title)}</h1>
        ${navIconsHtml("")}
      </div>
      <div class="detail">
        <div class="meta">
          <div class="cover">
            <img src="/api/books/${id}/cover" alt="" data-cover-title="${esc(book.title)}">
          </div>
          <div class="info">
            <h2>${esc(book.title)}</h2>
            <p class="detail-author">${esc(book.author)}</p>
            ${seriesNavHtml(book, seriesNav)}
            <div class="detail-badges">
              <span class="format-badge">${esc(book.format.toUpperCase())}</span>
              ${starsHtml(book.rating)}
            </div>
            ${factsHtml}
            ${book.description ? `<p class="detail-description">${esc(book.description)}</p>` : ""}
            ${progressBlockHtml(book, progress, hasProgress, isPageBased)}
            <div class="actions">
              ${primaryHtml}
              ${moreHtml}
              ${wantToReadBtnHtml(book.want_to_read)}
            </div>
          </div>
        </div>
      </div>`;
    $("#back-btn").addEventListener("click", goHome);
    bindNavIcons();
    ensureDictionaryStatus().then(applyVocabNavVisibility);
    const coverImg = $(".detail .cover img");
    if (coverImg) bindCoverFallback(coverImg);
    // "More" overflow menu. Toggle open/closed; outside-click and Esc dismiss
    // it via the module-scope listeners (see closeDetailMenu). stopPropagation
    // keeps this click from reaching the outside-click listener.
    const moreBtn = $("#detail-more-btn");
    const detailMenu = $("#detail-menu");
    if (moreBtn && detailMenu) {
      moreBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        const opening = detailMenu.hidden;
        detailMenu.hidden = !opening;
        moreBtn.setAttribute("aria-expanded", opening ? "true" : "false");
        if (opening) {
          const first = detailMenu.querySelector("[role='menuitem']:not([disabled])");
          if (first) first.focus();
        }
      });
      // Download navigates the browser to a file response (no re-render), so
      // close the menu explicitly on that click. Every other row either
      // navigates away or re-renders the page, which tears the menu down.
      const dl = detailMenu.querySelector("a[href*='/download']");
      if (dl) dl.addEventListener("click", () => closeDetailMenu());
    }
    const readBtn = $("#read-btn");
    if (readBtn) readBtn.addEventListener("click", () => navigate(readHash));
    const continueBtn = $("#continue-btn");
    if (continueBtn) continueBtn.addEventListener("click", () => {
      // F2b: carry the saved in-chapter scroll offset through so the reader
      // can restore it — the URL only encodes the chapter/page index.
      const scrollPosition = progress && typeof progress.scroll_position === "number" ? progress.scroll_position : 0;
      readerEntryIntent = { id, action: "continue", scrollPosition };
      navigate(continueHash);
    });
    const restartBtn = $("#restart-btn");
    if (restartBtn) restartBtn.addEventListener("click", () => {
      // F9: an explicit "Start Over" legitimately writes 0 even though it's
      // lower than anything already sent this session.
      resetProgress(id);
      readerEntryIntent = { id, action: "restart" };
      navigate(readHash);
    });
    const offlineSaveBtn = $("#offline-save-btn");
    if (offlineSaveBtn) offlineSaveBtn.addEventListener("click", async () => {
      offlineSaveBtn.disabled = true;
      offlineSaveBtn.textContent = "Saving…";
      const prog = document.createElement("span");
      prog.className = "detail-menu-info offline-save-progress";
      prog.setAttribute("role", "status");
      offlineSaveBtn.after(prog);
      // Cancel affordance (the engine's cancellation marker is honored at
      // every loop iteration and before publication).
      const cancelBtn = document.createElement("button");
      cancelBtn.className = "detail-menu-item";
      cancelBtn.id = "offline-cancel-btn";
      cancelBtn.textContent = "Cancel";
      cancelBtn.addEventListener("click", () => {
        cancelBtn.disabled = true;
        cancelOfflineSave(id);
      });
      prog.after(cancelBtn);
      try {
        await saveBookOffline(book, ({ done, total }) => {
          prog.textContent = ` ${done} / ${total}`;
        });
        showToast("Saved for offline reading");
        if (hashTargetsDetail(id)) showDetail(id); // re-render into the saved state
      } catch (e) {
        if (e.silent) return; // a concurrent save owns the UI — say nothing
        showToast(e.cancelled ? "Download cancelled" : (e.message || "Download failed — retry"));
        // Re-render rather than mutate these captured elements: if the user
        // navigated away and back mid-save, showDetail already replaced this
        // page with a disabled "Saving…" button bound to nothing —
        // mutating the detached original would leave that visible button
        // stuck. A fresh render reflects the real (not-saved) state with a
        // working Save button.
        if (hashTargetsDetail(id)) showDetail(id);
      }
    });
    // Cancel button on a mid-save re-render (the save itself was started by
    // a previous render's handler; cancellation is id-based, and that
    // handler's catch re-renders the page when the save unwinds).
    const midSaveCancelBtn = $("#offline-cancel-btn");
    if (midSaveCancelBtn) midSaveCancelBtn.addEventListener("click", () => {
      midSaveCancelBtn.disabled = true;
      cancelOfflineSave(id);
    });
    // Total offline storage in use, across all saved books (spec: shown
    // wherever the saved state is displayed). Best-effort — absent API or a
    // failure just leaves the line blank.
    const usageEl = $("#offline-usage");
    if (usageEl && navigator.storage && navigator.storage.estimate) {
      navigator.storage.estimate().then((est) => {
        if (usageEl.isConnected && typeof est.usage === "number") {
          usageEl.textContent = `Using ${formatFileSize(est.usage) || "0 B"} of offline storage`;
        }
      }).catch(() => {});
    }
    const offlineRemoveBtn = $("#offline-remove-btn");
    if (offlineRemoveBtn) offlineRemoveBtn.addEventListener("click", async () => {
      offlineRemoveBtn.disabled = true;
      try {
        await removeBookOffline(id);
        showToast("Offline copy removed");
      } catch (e) {
        showToast("Couldn't remove the offline copy");
      }
      if (hashTargetsDetail(id)) showDetail(id);
    });
    // "Want to read" toggle — the web UI's only writer of the flag.
    // await-then-set: PUT first, then update the in-memory book + the button
    // only on success. No optimistic pre-flip; on failure the flag is left
    // untouched and a toast explains why. Returning to the library re-fetches
    // from the server (the PUT is already committed), so the badge/filter/
    // shelf converge to truth with no client-side override machinery.
    const wantBtn = $("#want-btn");
    if (wantBtn) {
      let wantInFlight = false;
      wantBtn.addEventListener("click", async () => {
        // Guard re-entry with a flag + aria-busy, NOT the native `disabled`
        // attribute (which would blur the button and drop keyboard focus).
        if (wantInFlight) return;
        const next = !book.want_to_read;
        wantInFlight = true;
        wantBtn.setAttribute("aria-busy", "true");
        let resp;
        try {
          resp = await apiWrite(`/api/books/${id}/want-to-read`, {
            method: "PUT",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ want_to_read: next }),
            credentials: "same-origin",
          });
        } catch (e) {
          wantInFlight = false;
          if (wantBtn.isConnected) wantBtn.removeAttribute("aria-busy");
          showToast("Couldn't reach Carrel server");
          return;
        }
        if (resp.status === 401) { authenticated = false; showLogin(); return; }
        wantInFlight = false;
        if (wantBtn.isConnected) wantBtn.removeAttribute("aria-busy");
        if (!resp.ok) {
          showToast(httpErrorToastMessage(resp.status));
          return;
        }
        book.want_to_read = next;
        if (wantBtn.isConnected) {
          wantBtn.setAttribute("aria-pressed", next ? "true" : "false");
          wantBtn.textContent = wantToReadBtnLabel(next);
        }
      });
    }
    const seriesLink = $("#series-link");
    if (seriesLink) seriesLink.addEventListener("click", (e) => {
      e.preventDefault();
      // Item 7: the URL carries the filter directly — no pending-intent
      // variable needed between this click and the library rendering it.
      navigate(libraryHash({ ...currentLibraryState(), q: "", series: book.series, collection: null, want_to_read: false }));
    });
    const seriesPrevBtn = $("#series-prev-btn");
    if (seriesPrevBtn && seriesNav && seriesNav.prevId) {
      seriesPrevBtn.addEventListener("click", () => navigate("#/book/" + seriesNav.prevId));
    }
    const seriesNextBtn = $("#series-next-btn");
    if (seriesNextBtn && seriesNav && seriesNav.nextId) {
      seriesNextBtn.addEventListener("click", () => navigate("#/book/" + seriesNav.nextId));
    }
  }

  // ── Reader ────────────────────────────────────
  // Two modes: "page" (PDF/CBZ/CBR, images) and "chapter" (EPUB/MOBI, HTML).
  // Chrome (header + bottom toolbar) is built ONCE per book; page/chapter
  // turns within the same book only swap the stage content (renderReaderContent)
  // instead of re-fetching the book/page-count and tearing down the DOM.
  // R2/R3: true while `location.hash` still points at this reader route for
  // this book. Re-checked after every await in `showReader` so a user who
  // navigates away mid-load doesn't get a stale render clobbering whatever
  // they navigated to.
  function hashTargetsReader(id) {
    const hash = window.location.hash || "#";
    return hash.startsWith(`#/book/${id}/`) && hash.includes("/read");
  }

  // R3: clamp a possibly-malformed page/chapter index (NaN, negative,
  // out-of-range) into [0, count-1].
  function clampIndex(index, count) {
    const max = Math.max(count - 1, 0);
    if (!Number.isFinite(index)) return 0;
    return Math.min(Math.max(index, 0), max);
  }

  async function showReader(id, index) {
    currentView = "reader";

    // F7: read AND clear the entry intent right away, before any await can
    // early-return — otherwise a failed fetch below would leak it into a
    // later, unrelated reader entry and wrongly suppress its resume prompt.
    const rawIntent = readerEntryIntent && readerEntryIntent.id === id ? readerEntryIntent : null;
    readerEntryIntent = null;
    const intent = rawIntent ? rawIntent.action : null;
    const intentScroll = rawIntent && typeof rawIntent.scrollPosition === "number" ? rawIntent.scrollPosition : 0;

    const sameBook = readerState && readerState.id === id;

    if (!sameBook) {
      // R2: drop any stale state up front so a concurrent load for a
      // different book can't be mistaken for a "same book" fast path.
      // Item 4: flush first — this book may be a different one than the
      // pending debounced save belongs to.
      flushProgressSave();
      readerState = null;
      resumePromptActive = false;
      app().innerHTML = readerSkeletonHtml();

      // Finding 1: the book fetch is required to render anything at all —
      // unlike the resume-position check below, its failure (network error,
      // non-2xx, bad JSON) must replace the skeleton with a visible message
      // + toast instead of leaving it spinning forever.
      let resp;
      try {
        resp = await api("/api/books/" + id);
      } catch (e) {
        if (!hashTargetsReader(id)) return;
        renderViewError(apiFailureToastMessage(e));
        showToast(apiFailureToastMessage(e));
        return;
      }
      if (!resp || !hashTargetsReader(id)) return;
      if (!resp.ok) {
        renderViewError(`Couldn't load this book (HTTP ${resp.status})`);
        showToast(httpErrorToastMessage(resp.status));
        return;
      }
      let book;
      try {
        book = await resp.json();
      } catch (e) {
        renderViewError("Couldn't load this book (unexpected response)");
        showToast("Unexpected response");
        return;
      }
      if (!hashTargetsReader(id)) return;
      document.title = `Reading: ${book.title} — Carrel`;

      // MOBI and EPUB both render through the chapter-HTML endpoint; the
      // server-side `/api/books/:id/chapters/:index` route dispatches to
      // the right parser.
      const isHtmlBook = book.format === "epub" || book.format === "mobi";
      const mode = isHtmlBook ? "chapter" : "page";

      let count;
      if (isHtmlBook) {
        count = book.total_chapters || 1;
      } else {
        let countResp;
        try {
          countResp = await api(`/api/books/${id}/page-count`);
        } catch (e) {
          if (!hashTargetsReader(id)) return;
          renderViewError(apiFailureToastMessage(e));
          showToast(apiFailureToastMessage(e));
          return;
        }
        if (!countResp || !hashTargetsReader(id)) return;
        if (!countResp.ok) {
          renderViewError(`Couldn't load page count (HTTP ${countResp.status})`);
          showToast(httpErrorToastMessage(countResp.status));
          return;
        }
        let countBody;
        try {
          countBody = await countResp.json();
        } catch (e) {
          renderViewError("Couldn't load page count (unexpected response)");
          showToast("Unexpected response");
          return;
        }
        count = countBody.count;
        if (!hashTargetsReader(id)) return;
      }

      const clamped = clampIndex(index, count);

      // Item 4: offer to resume at the saved position — but only on the
      // canonical "default open" entry point (index 0, what the detail
      // page's plain Read button always requests). A bookmarked/typed URL
      // with a specific (even out-of-range/malformed) index is an explicit
      // request for that position and must just clamp+load like before —
      // not get reinterpreted as "the user wants to resume". Also skipped
      // when the detail page's Continue/Start Over buttons already made
      // this call for this book (readerEntryIntent).
      // Finding 1: this check is best-effort (mirrors showDetail's progress
      // fetch) — a network failure must not block the book from opening, it
      // should just skip the resume prompt, same as a 404 already does.
      let savedIndex = null;
      let savedScroll = 0;
      if (!intent && index === 0) {
        let progResp;
        try {
          progResp = await api(`/api/books/${id}/progress`);
        } catch (e) {
          progResp = undefined;
        }
        // F5: a null (not undefined) response means api() already redirected
        // to the login screen (401) — continuing would render the reader
        // over it. `undefined` (network error, caught above) falls through
        // to "no saved progress" instead.
        if (progResp === null || !hashTargetsReader(id)) return;
        if (progResp && progResp.ok) {
          let progress = null;
          try { progress = await progResp.json(); } catch (e) { progress = null; }
          if (progress && progress.chapter_index > 0) {
            savedIndex = clampIndex(progress.chapter_index, count);
            savedScroll = typeof progress.scroll_position === "number" ? progress.scroll_position : 0;
          }
          // Opening the reader online is a fresh server observation — advance
          // the offline baseline (same reason as the detail-view GET) so a
          // later offline edit isn't wrongly discarded as "server advanced".
          if (progress && progress.last_read_at) updateOfflineBaseline(id, progress.last_read_at);
        }
        if (!hashTargetsReader(id)) return;
      }

      if (savedIndex !== null && savedIndex !== clamped) {
        showResumePrompt(id, book, mode, count, savedIndex, clamped, savedScroll);
        return;
      }

      enterReaderAt(id, book, mode, count, clamped, intent === "continue" ? intentScroll : 0);
      if (clamped !== index) {
        // Normalize the URL; the resulting hashchange re-enters this
        // function on the "same book" fast path below to actually render.
        navigate(`#/book/${id}/${clamped}/read`);
        return;
      }
    } else {
      const clamped = clampIndex(index, readerState.count);
      readerState.index = clamped;
      if (clamped !== index) {
        navigate(`#/book/${id}/${clamped}/read`);
        return;
      }
    }

    await renderReaderContent();
  }

  // F2/F2b: `scrollPosition` (0..1, only meaningful in chapter mode) is the
  // saved in-chapter offset to restore on this specific entry — 0 for a
  // normal fresh open. `suppressNextSave` and `pendingScrollRestore` make
  // sure the very first render of this entry (the "mere open") never itself
  // schedules a progress save; see renderReaderContent().
  function enterReaderAt(id, book, mode, count, index, scrollPosition) {
    readerState = {
      id,
      book,
      mode,
      index,
      count,
      chromeHidden: false,
      fitMode: safeStorageGet("carrel_reader_fit_mode") || "fit-height",
      handlers: null,
      renderGen: 0,
      scrollPosition: mode === "chapter" ? (scrollPosition || 0) : 0,
      pendingScrollRestore: mode === "chapter" ? (scrollPosition || 0) : 0,
      suppressNextSave: true,
      // Item 12: page-turn animation bookkeeping. `lastRenderedPageIndex`/
      // `lastRenderedChapterIndex` start undefined so the very first render
      // of a fresh entry never animates (no previous page to slide from).
      // `preloadCache` (page mode only) tracks which neighbor images have
      // actually finished loading, keyed by index — an animation only ever
      // plays for an index already in this cache (see getPreloadedImage()).
      preloadCache: {},
      // F-4-4: prefetched chapter HTML (chapter mode only), keyed by chapter
      // index. Lives on readerState, which is recreated per book open (see
      // showReader's `!sameBook` branch), so the cache is inherently
      // book-scoped and reset on book change — one book's HTML can never be
      // served for another. `chapterPrefetching` maps an in-flight chapter
      // index to its fetch promise, so a rapid re-entry (and an on-demand
      // render that races an outstanding prefetch) can reuse it instead of
      // firing a duplicate request.
      chapterHtmlCache: {},
      chapterPrefetching: new Map(),
      lastRenderedPageIndex: undefined,
      lastRenderedChapterIndex: undefined,
      chapterAnimCleanup: null,
      turnAnimCleanup: null,
      snapBackCleanup: null,
      pendingInstantTurn: false,
      // Page-image cache recovery (page mode only): the index whose <img>
      // load already got one cache-bypassing retry, so a still-failing page
      // falls through to the error box instead of looping. Reset to null on
      // every successful page load (see the #page-img "load" handler), so a
      // page that later re-fails is retried afresh. See handlePageImageError.
      pageRetryIndex: null,
      // Pinch-zoom (page mode only): scale in [1, 5]; tx/ty are the
      // translate() applied before scale() (transform-origin 0 0, so
      // scaling never moves the image's top-left). This object always
      // mirrors exactly what applyZoomTransform() last wrote to #page-img.
      zoom: { scale: 1, tx: 0, ty: 0 },
    };
    readerState.handlers = makeReaderHandlers(id);
    renderReaderChrome();
  }

  // Item 4: "You left off at page/chapter N" prompt shown on a fresh reader
  // entry when saved progress differs from the requested (usually 0) index.
  // F10: sets resumePromptActive/resumePromptBookId so the global keyboard
  // dispatcher can drive Enter/Esc/Backspace while this is on screen.
  function showResumePrompt(id, book, mode, count, savedIndex, restartIndex, savedScroll) {
    resumePromptActive = true;
    resumePromptBookId = id;
    const unitLabel = mode === "page" ? "Page" : "Chapter";
    app().innerHTML = `
      <div class="resume-prompt">
        <div class="resume-prompt-panel">
          <h2>${esc(book.title)}</h2>
          <p>You left off at ${unitLabel.toLowerCase()} ${savedIndex + 1} of ${count}.</p>
          <div class="resume-actions">
            <button class="btn-primary" id="resume-btn">Resume at ${unitLabel} ${savedIndex + 1}</button>
            <button class="btn-secondary" id="resume-restart-btn">Start Over</button>
          </div>
        </div>
      </div>`;
    $("#resume-btn").addEventListener("click", () => resolveResumePrompt(id, book, mode, count, savedIndex, savedScroll || 0));
    $("#resume-restart-btn").addEventListener("click", () => {
      // F9: an explicit "Start Over" legitimately writes 0 even though it's
      // lower than anything already sent this session — resetProgress()
      // clears the monotonic guard for this book before saving.
      resetProgress(id);
      resolveResumePrompt(id, book, mode, count, restartIndex, 0);
    });
  }

  async function resolveResumePrompt(id, book, mode, count, index, scrollPosition) {
    resumePromptActive = false;
    if (!hashTargetsReader(id)) return;
    // Fix the URL without firing a hashchange (avoids re-fetching book/
    // page-count/progress a second time just to confirm the same choice).
    history.replaceState(null, "", `#/book/${id}/${index}/read`);
    enterReaderAt(id, book, mode, count, index, scrollPosition || 0);
    await renderReaderContent();
  }

  function pageUrl(id, index) {
    return `/api/books/${id}/pages/${index}`;
  }

  function makeReaderHandlers(id) {
    // Every ordinary navigation cancels a pending highlight jump (spec §4) —
    // startHlJump's own navigation calls gotoReaderIndex directly, not these.
    return {
      next: () => { clearHlJump(); clearVocabJump(); gotoReaderIndex(readerState.index + 1); },
      prev: () => { clearHlJump(); clearVocabJump(); gotoReaderIndex(readerState.index - 1); },
      first: () => { clearHlJump(); clearVocabJump(); gotoReaderIndex(0); },
      last: () => { clearHlJump(); clearVocabJump(); gotoReaderIndex(readerState.count - 1); },
      goBack: () => { clearHlJump(); clearVocabJump(); navigate("#/book/" + id); },
      toggleChrome: () => {
        readerState.chromeHidden = !readerState.chromeHidden;
        applyChromeVisibility();
      },
    };
  }

  // Item 12: `opts.instant` marks this turn as one that must never animate
  // (the slider) — consumed once by renderPageTurn()/the chapter-mode
  // animation branch on the render this navigation produces.
  function gotoReaderIndex(newIndex, opts) {
    if (!readerState || newIndex < 0 || newIndex >= readerState.count) return;
    // R1-adjacent: update in-memory state synchronously so rapid successive
    // calls (e.g. holding/repeating ArrowRight) each see the just-updated
    // index rather than all reading the same stale value before the
    // asynchronous `hashchange` round-trip catches up.
    readerState.index = newIndex;
    readerState.pendingInstantTurn = !!(opts && opts.instant);
    navigate("#/book/" + readerState.id + "/" + newIndex + "/read");
  }

  function applyChromeVisibility() {
    const root = $("#reader-root");
    if (root) root.classList.toggle("chrome-hidden", readerState.chromeHidden);
    // The Aa popover and TOC panel live in the bottom chrome; hiding the
    // chrome must also dismiss them (and clear their open flags) so they can't
    // linger invisibly and reappear when the chrome is shown again.
    if (readerState.chromeHidden && typoPanelOpen) closeTypoPanel(false);
    if (readerState.chromeHidden && tocPanelOpen) closeTocPanel(false);
    if (readerState.chromeHidden && bookmarkPanelOpen) closeBookmarkPanel(false);
    if (readerState.chromeHidden && hlPanelOpen) closeHlPanel(false);
    if (readerState.chromeHidden && vocabPanelOpen) closeVocabPanel(false);
    // Showing/hiding the chrome rows resizes the stage without a window
    // resize event — re-clamp a zoomed pan against the new bounds so no
    // gap opens at an edge.
    if (readerState.mode === "page" && isZoomed()) {
      clampZoomPan();
      applyZoomTransform();
    }
  }

  function applyFitMode() {
    const root = $("#reader-root");
    if (!root) return;
    root.classList.remove("fit-height", "fit-width");
    root.classList.add(readerState.fitMode);
    const btn = $("#fit-toggle-btn");
    if (btn) btn.textContent = readerState.fitMode === "fit-height" ? "Fit: Height" : "Fit: Width";
  }

  // ── Pinch-to-zoom (page mode) ────────────────────────────────────────
  // One writer owns #page-img's transform: applyZoomTransform(). The swipe
  // drag-follow below only ever runs at scale 1 (where the zoom transform
  // is empty), so the two never compose — they're mutually exclusive, and
  // resetDragStyles() re-applies the zoom transform after clearing drag
  // leftovers so neither path can wipe the other's state.
  const ZOOM_MIN = 1;
  const ZOOM_MAX = 5;
  // iOS Safari fires touch events AND proprietary GestureEvents for the
  // same physical pinch. The touch pinch (bindSwipe) sets this so the
  // gesture handlers (bindWheelZoom) stand down — otherwise both write the
  // scale each frame from slightly different anchors and the zoom jitters.
  let touchPinchActive = false;
  // Bumped by resetZoom(): an in-flight gesture that captured its base
  // scale before a page turn (or reader re-entry) is stale — without this
  // its next touchmove would re-apply the old scale to the new page.
  let zoomEpoch = 0;
  const ZOOM_DOUBLE_TAP_SCALE = 2.5;
  const DOUBLE_TAP_WINDOW_MS = 275; // also the single-tap defer window
  const DOUBLE_TAP_SLOP_PX = 30;

  function isZoomed() {
    return !!(readerState && readerState.zoom && readerState.zoom.scale > 1);
  }

  function applyZoomTransform() {
    const img = $("#page-img");
    if (!img || !readerState || !readerState.zoom) return;
    const z = readerState.zoom;
    const stage = $("#reader-stage");
    if (z.scale <= 1) {
      img.style.transform = "";
      if (stage) stage.classList.remove("zoom-active");
      return;
    }
    img.style.transform = `translate(${z.tx}px, ${z.ty}px) scale(${z.scale})`;
    if (stage) stage.classList.add("zoom-active");
  }

  function resetZoom() {
    if (!readerState || !readerState.zoom) return;
    zoomEpoch++;
    readerState.zoom = { scale: 1, tx: 0, ty: 0 };
    applyZoomTransform();
  }

  // Clamp one axis: if the scaled image overflows the stage, no gap may
  // open between image edge and stage edge; otherwise center the image on
  // that axis. `base` is the image's untransformed offset inside the stage.
  function clampZoomAxis(t, base, scaledSize, stageSize) {
    if (scaledSize <= stageSize) return (stageSize - scaledSize) / 2 - base;
    return Math.min(-base, Math.max(stageSize - scaledSize - base, t));
  }

  function clampZoomPan() {
    const img = $("#page-img");
    const stage = $("#reader-stage");
    const z = readerState && readerState.zoom;
    if (!img || !stage || !z) return;
    z.tx = clampZoomAxis(z.tx, img.offsetLeft, img.clientWidth * z.scale, stage.clientWidth);
    z.ty = clampZoomAxis(z.ty, img.offsetTop, img.clientHeight * z.scale, stage.clientHeight);
  }

  // Core zoom apply: put the image-local point (localX, localY) under the
  // client point (clientX, clientY) at `newScale`. Shared by zoomAtPoint
  // (anchor = whatever is under the cursor right now) and the touch pinch
  // (anchor captured once at pinch start, so the content that was between
  // the fingers follows a moving midpoint — a constant-spread two-finger
  // drag pans).
  function zoomToAnchor(newScale, localX, localY, clientX, clientY) {
    const img = $("#page-img");
    const stage = $("#reader-stage");
    const z = readerState && readerState.zoom;
    // Error state (#page-error visible, img hidden/collapsed): no-op.
    if (!img || !stage || !z || !img.clientWidth) return;
    const scale = Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, newScale));
    // Entering zoom from 1x in fit-width: the stage may be natively
    // scrolled. Freeze that (same trick as renderPageTurn's F5) — while
    // zoomed, .zoom-active turns off native overflow entirely and our pan
    // owns all movement, so the offset-based math below stays valid (the
    // tx/ty formula assumes scrollTop 0 at apply time).
    if (scale > 1 && z.scale === 1 && stage.scrollTop > 0) stage.scrollTop = 0;
    const sr = stage.getBoundingClientRect();
    z.tx = clientX - localX * scale - (sr.left + img.offsetLeft);
    z.ty = clientY - localY * scale - (sr.top + img.offsetTop);
    z.scale = scale;
    clampZoomPan();
    applyZoomTransform();
  }

  // Image-local (unscaled) coords of a client point, measured from the
  // CURRENT rendered rect — so it reflects any native fit-width scroll and
  // the current transform. Must run before zoomToAnchor's scroll reset.
  function imageLocalPoint(clientX, clientY) {
    const img = $("#page-img");
    const z = readerState && readerState.zoom;
    if (!img || !z || !img.clientWidth) return null;
    const r = img.getBoundingClientRect();
    return { x: (clientX - r.left) / z.scale, y: (clientY - r.top) / z.scale };
  }

  function zoomAtPoint(newScale, clientX, clientY) {
    const local = imageLocalPoint(clientX, clientY);
    if (!local) return;
    zoomToAnchor(newScale, local.x, local.y, clientX, clientY);
  }

  function panZoomBy(dx, dy) {
    const z = readerState && readerState.zoom;
    if (!z || z.scale <= 1) return;
    z.tx += dx;
    z.ty += dy;
    clampZoomPan();
    applyZoomTransform();
  }

  function bindWheelZoom(stage) {
    // Firefox reports mouse-wheel deltas in lines (deltaMode 1, ±3/notch),
    // not pixels — without normalization zoom/pan is ~30x too weak there.
    // Page-mode (deltaMode 2) deltas scale by the stage size on the same
    // axis as the delta, not blanket clientHeight.
    function wheelPx(e, d, axisSize) {
      if (e.deltaMode === 1) return d * 33;
      if (e.deltaMode === 2) return d * (axisSize || 400);
      return d;
    }

    // Safari (macOS) fires proprietary GestureEvents for a trackpad pinch
    // alongside/instead of ctrl+wheel. Handle the gesture directly and mute
    // ctrl+wheel while one is active so browsers that emit both never apply
    // the zoom twice; the preventDefault also stops Safari's native
    // full-page zoom. No-ops on browsers without GestureEvent.
    let inGesture = false;
    let gestureAt = 0; // last gesture activity — staleness backstop below
    let gestureBaseScale = 1;
    let gestureEpoch = 0; // zoomEpoch at (re)base — see the mismatch check
    let gestureBaseFactor = 1; // e.scale at (re)base; e.scale is cumulative
    stage.addEventListener("gesturestart", (e) => {
      if (!readerState || readerState.mode !== "page") return;
      e.preventDefault();
      if (touchPinchActive) return; // touch pinch owns this physical gesture
      inGesture = true;
      gestureAt = Date.now();
      gestureBaseScale = readerState.zoom.scale;
      gestureEpoch = zoomEpoch;
      gestureBaseFactor = 1;
    });
    stage.addEventListener("gesturechange", (e) => {
      if (!readerState || readerState.mode !== "page") return;
      e.preventDefault();
      if (touchPinchActive) return; // touch pinch owns this physical gesture
      inGesture = true;
      gestureAt = Date.now();
      if (typeof e.scale !== "number") return;
      if (gestureEpoch !== zoomEpoch) {
        // A page turn reset the zoom mid-gesture — re-base on the fresh
        // scale instead of re-applying the previous page's. e.scale is
        // cumulative since gesturestart, so remember the factor at the
        // re-base point and zoom by the ratio from here on.
        gestureBaseScale = readerState.zoom.scale;
        gestureEpoch = zoomEpoch;
        gestureBaseFactor = e.scale;
        return;
      }
      zoomAtPoint(gestureBaseScale * (e.scale / gestureBaseFactor), e.clientX, e.clientY);
    });
    stage.addEventListener("gestureend", (e) => {
      e.preventDefault();
      inGesture = false;
    });

    stage.addEventListener("wheel", (e) => {
      if (!readerState || readerState.mode !== "page") return;
      if (e.ctrlKey) {
        // ctrl+wheel (incl. trackpad pinch on Chrome/Edge/Firefox, which
        // deliver it as ctrl+wheel) — zoom about the cursor.
        e.preventDefault();
        if (inGesture) {
          // Safari handles the pinch via gesturechange — don't double-apply.
          // Staleness backstop: a gestureend lost to an app switch or system
          // gesture would otherwise mute ctrl+wheel zoom forever.
          if (Date.now() - gestureAt < 500) return;
          inGesture = false;
        }
        zoomAtPoint(readerState.zoom.scale * Math.exp(-wheelPx(e, e.deltaY, stage.clientHeight) * 0.01), e.clientX, e.clientY);
      } else if (isZoomed()) {
        // Plain wheel while zoomed pans (content follows scroll direction);
        // at 1x native behavior (fit-width scroll) is untouched.
        e.preventDefault();
        panZoomBy(-wheelPx(e, e.deltaX, stage.clientWidth), -wheelPx(e, e.deltaY, stage.clientHeight));
      }
    }, { passive: false });
  }

  // Pending deferred tap-zone action (see bindClickZones). Module-scope so
  // the gesture controller can cancel it: a pinch or pan starting inside
  // the defer window means the taps were gesture noise, not intent.
  let pendingTap = null; // { x, y, zone, epoch, timer }
  function cancelPendingTap() {
    if (!pendingTap) return;
    clearTimeout(pendingTap.timer);
    pendingTap = null;
  }

  // Left third = prev, right third = next, middle third = toggle chrome —
  // but deferred DOUBLE_TAP_WINDOW_MS so a second tap can turn the pair
  // into a double-tap zoom toggle instead. Browsers synthesize click events
  // for taps, so one code path covers touch double-tap and desktop
  // double-click alike. While zoomed, prev/next zones stay inert (panning
  // is the only navigation) and the center zone still toggles chrome.
  function bindClickZones(el) {
    // Zone is classified at click time — measuring the rect when the timer
    // fires would misclassify if the layout changed inside the window
    // (rotation, fit toggle, page turn to a different aspect).
    function zoneAt(clientX) {
      const rect = el.getBoundingClientRect();
      const third = rect.width / 3;
      const rel = clientX - rect.left;
      return rel < third ? "prev" : rel > third * 2 ? "next" : "chrome";
    }

    function runZone(zone) {
      if (!readerState) return;
      if (zone === "prev") { if (!isZoomed()) readerState.handlers.prev(); }
      else if (zone === "next") { if (!isZoomed()) readerState.handlers.next(); }
      else readerState.handlers.toggleChrome();
    }

    el.addEventListener("click", (e) => {
      if (pendingTap
          && Math.abs(e.clientX - pendingTap.x) < DOUBLE_TAP_SLOP_PX
          && Math.abs(e.clientY - pendingTap.y) < DOUBLE_TAP_SLOP_PX) {
        // Second tap in time and in place: double-tap. Cancel the pending
        // single-tap action and toggle zoom at the tap point.
        cancelPendingTap();
        if (isZoomed()) resetZoom();
        else zoomAtPoint(ZOOM_DOUBLE_TAP_SCALE, e.clientX, e.clientY);
        return;
      }
      if (pendingTap) {
        // A second tap far away is not a double-tap — run the first tap's
        // action NOW rather than dropping it (rapid taps across zones, e.g.
        // two quick right-third taps landing >30px apart, must not lose a
        // page turn), then defer the new tap as usual.
        const prev = pendingTap;
        cancelPendingTap();
        if (prev.zone === "chrome" || prev.epoch === zoomEpoch) runZone(prev.zone);
      }
      // Zone and staleness epoch are captured now — after any immediate
      // action above, which may itself have turned the page (epoch bump).
      const zone = zoneAt(e.clientX);
      const epoch = zoomEpoch;
      pendingTap = {
        x: e.clientX,
        y: e.clientY,
        zone,
        epoch,
        timer: setTimeout(() => {
          pendingTap = null;
          // prev/next are stale if anything reset the zoom since the tap —
          // page turn (any means), fit toggle, reader exit/re-entry.
          // Without this, a tap immediately followed by a keyboard turn
          // would fire a second turn 275ms later. The chrome toggle is
          // pure UI, valid on whatever page is showing — never stale (and
          // a turn queued by a rapid earlier tap in another zone must not
          // swallow it).
          if (zone === "chrome" || epoch === zoomEpoch) runZone(zone);
        }, DOUBLE_TAP_WINDOW_MS),
      };
    });
  }

  function reducedMotionEnabled() {
    return !!(window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches);
  }

  // Item 12: clear any drag-follow transform left on the (page-mode)
  // current image — used both when a drag commits (the incoming overlay's
  // slide-in animation takes over immediately, so the reset is instant, no
  // transition) and by finalizeTurnAnimation's cleanup.
  function resetDragStyles(img) {
    if (!img) return;
    img.style.transition = "";
    img.style.willChange = "";
    // Re-applies the zoom transform, or clears the transform at 1x — the
    // pre-zoom behavior. Single-writer rule: never assign transform here.
    applyZoomTransform();
  }

  // F6: cancels any in-flight snap-back on `img` — removes its
  // transitionend listener and its fallback timeout, then resets the drag
  // styles immediately. Called both when a new drag starts (so it doesn't
  // inherit the snap-back's transition, which would make the page lag the
  // finger) and when a drag is aborted outright (F1/F2).
  function cancelSnapBack(img) {
    if (!readerState || !readerState.snapBackCleanup) return;
    const cleanup = readerState.snapBackCleanup;
    readerState.snapBackCleanup = null;
    cleanup();
  }

  // Item 12: below-threshold release — animate the dragged image back to
  // translateX(0) with the same timing as a committed turn, then clean up.
  // F6: cleanup runs off transitionend OR a timeout fallback, whichever
  // fires first — dx===0 leaves img.style.transform already at
  // "translateX(0)" (no-op transition, transitionend never fires), so the
  // timeout is the only thing that ever cleans that case up. A new drag
  // (touchstart) or an abort (touchcancel/multi-touch) cancels this via
  // cancelSnapBack before either fires.
  function snapBackDrag(img) {
    if (!img || !img.style.transform) {
      resetDragStyles(img);
      return;
    }
    img.style.transition = "transform var(--dur-page-turn) var(--ease-page-turn)";
    const rafId = requestAnimationFrame(() => { img.style.transform = "translateX(0)"; });

    let done = false;
    const finish = () => {
      if (done) return;
      done = true;
      cancelAnimationFrame(rafId); // in case cancelled before the rAF ran
      img.removeEventListener("transitionend", onEnd);
      clearTimeout(timer);
      if (readerState && readerState.snapBackCleanup === finish) readerState.snapBackCleanup = null;
      resetDragStyles(img);
    };
    const onEnd = () => finish();
    img.addEventListener("transitionend", onEnd);
    // Slightly longer than --dur-page-turn (0.22s) so it never races a
    // normal transitionend completion.
    const timer = setTimeout(finish, 280);
    if (readerState) readerState.snapBackCleanup = finish;
  }

  // Item 3/12: horizontal swipe (~50px threshold) = prev/next on touch
  // devices. Item 12 adds: axis lock (first ~10px of movement decides
  // horizontal-vs-vertical, so a fit-width vertical scroll is never
  // hijacked), drag-follow (the current page translates with the finger
  // while locked horizontal), and a snap-back animation below the commit
  // threshold. A bare touchstart+touchend with no intervening touchmove
  // (older callers/tests) falls through to the original single-shot delta
  // check unchanged.
  const SWIPE_COMMIT_PX = 50;
  const AXIS_LOCK_PX = 10;
  function bindSwipe(el) {
    let startX = 0, startY = 0, tracking = false, axisLock = null;
    // M2 pinch bookkeeping: finger distance & zoom scale captured when the
    // second finger lands; null when not pinching. Pan bookkeeping: last
    // single-finger position while zoomed; null when not panning.
    let pinch = null;
    let pan = null;

    function touchDist(t0, t1) {
      return Math.hypot(t1.clientX - t0.clientX, t1.clientY - t0.clientY);
    }

    // F1/F2: shared abort path — stops tracking so a stray touchend (from
    // whichever finger lifts) can't commit a bogus swipe, and immediately
    // clears any drag-follow transform/will-change plus any in-flight
    // snap-back so nothing is left stuck mid-gesture. resetDragStyles
    // re-applies the zoom transform, so aborting never disturbs zoom state.
    function abortDrag() {
      tracking = false;
      axisLock = null;
      const img = $("#page-img");
      cancelSnapBack(img);
      resetDragStyles(img);
    }

    el.addEventListener("touchstart", (e) => {
      if (e.touches.length === 2) {
        // Second finger down = pinch begins (this used to be a plain
        // abort). Swipe tracking must die first so its touchend can't fire.
        abortDrag();
        pan = null;
        cancelPendingTap(); // a tap right before a pinch was gesture noise
        const t0 = e.touches[0], t1 = e.touches[1];
        const midX = (t0.clientX + t1.clientX) / 2;
        const midY = (t0.clientY + t1.clientY) / 2;
        // Anchor the image-local point under the starting midpoint for the
        // whole pinch: zooming scales about it AND a moving midpoint drags
        // it along (constant-spread two-finger drag = pan).
        const local = imageLocalPoint(midX, midY);
        pinch = local && {
          dist: touchDist(t0, t1),
          scale: readerState && readerState.zoom ? readerState.zoom.scale : 1,
          localX: local.x,
          localY: local.y,
          epoch: zoomEpoch,
        };
        touchPinchActive = !!pinch;
        return;
      }
      if (e.touches.length !== 1) {
        // 3+ fingers: abort everything.
        abortDrag();
        pinch = null;
        pan = null;
        touchPinchActive = false;
        return;
      }
      pinch = null;
      touchPinchActive = false;
      if (isZoomed()) {
        // One finger while zoomed pans — never a swipe (a swipe would stomp
        // the zoom transform with translateX and turn pages under a user
        // trying to pan).
        cancelPendingTap(); // a tap right before a pan was gesture noise
        pan = { x: e.touches[0].clientX, y: e.touches[0].clientY, epoch: zoomEpoch };
        return;
      }
      pan = null;
      cancelSnapBack($("#page-img")); // F6: a new drag interrupts any snap-back in flight
      startX = e.touches[0].clientX;
      startY = e.touches[0].clientY;
      tracking = true;
      axisLock = null;
    }, { passive: true });

    // Registered non-passive: pinch and zoomed-pan preventDefault() to own
    // the gesture, and a horizontally-locked drag preventDefault()s to stop
    // the page from also being scrolled/selected — but a 1x vertical drag
    // returns before any preventDefault, so native fit-width scrolling is
    // never blocked.
    el.addEventListener("touchmove", (e) => {
      if (pinch && e.touches.length === 2) {
        if (pinch.epoch !== zoomEpoch) {
          // A page turn (resetZoom) happened mid-pinch — the captured base
          // scale/anchor belong to the previous page. Drop the gesture
          // before claiming the event.
          pinch = null;
          touchPinchActive = false;
          return;
        }
        e.preventDefault();
        const t0 = e.touches[0], t1 = e.touches[1];
        const dist = touchDist(t0, t1);
        if (pinch.dist > 0) {
          zoomToAnchor(
            pinch.scale * (dist / pinch.dist),
            pinch.localX,
            pinch.localY,
            (t0.clientX + t1.clientX) / 2,
            (t0.clientY + t1.clientY) / 2,
          );
        }
        return;
      }
      if (pan && e.touches.length === 1) {
        if (!isZoomed() || pan.epoch !== zoomEpoch) {
          // Zoom reset mid-gesture (e.g. a keyboard page turn while the
          // finger is down) — stop claiming the touch. The epoch check
          // also catches a reset-then-rezoom on a new page before the
          // finger's next move: the old pan's deltas belong to the
          // previous page.
          pan = null;
          return;
        }
        e.preventDefault();
        const t = e.touches[0];
        panZoomBy(t.clientX - pan.x, t.clientY - pan.y);
        pan = { x: t.clientX, y: t.clientY, epoch: pan.epoch };
        return;
      }
      if (!tracking) return;
      if (e.touches.length !== 1) {
        // F2: a second finger joined mid-drag — the touchstart above has
        // already flipped this into a pinch; just make sure swipe is dead.
        abortDrag();
        return;
      }
      if (isZoomed()) {
        // Zoom engaged mid-drag from outside the touch path (ctrl+wheel or
        // Safari gesture on a touchscreen laptop): kill the swipe rather
        // than let translateX stomp the zoom.
        abortDrag();
        return;
      }
      const dx = e.touches[0].clientX - startX;
      const dy = e.touches[0].clientY - startY;
      if (axisLock === null) {
        if (Math.abs(dx) < AXIS_LOCK_PX && Math.abs(dy) < AXIS_LOCK_PX) return;
        axisLock = Math.abs(dx) > Math.abs(dy) ? "x" : "y";
        // Real movement — a tap deferred just before this drag wasn't tap
        // intent. Without this, tap-then-swipe inside the 275ms window
        // turns two pages (the timer's next() plus the swipe commit).
        cancelPendingTap();
      }
      if (axisLock !== "x") return;
      e.preventDefault();
      if (reducedMotionEnabled()) return; // gesture still recognized, no visual feedback
      const img = $("#page-img");
      if (img) {
        // Inline hint for the 1x drag window only; the zoomed pan/pinch
        // path gets the same hint from app.css's .zoom-active rule — both
        // are needed, neither covers the other's window.
        img.style.willChange = "transform";
        img.style.transform = `translateX(${dx}px)`;
      }
    }, { passive: false });

    el.addEventListener("touchend", (e) => {
      if (pinch) {
        // Pinch ends when fewer than two fingers remain. If one finger
        // stays down and we're zoomed, hand it to the pan branch.
        if (e.touches.length < 2) {
          pinch = null;
          touchPinchActive = false;
          pan = e.touches.length === 1 && isZoomed()
            ? { x: e.touches[0].clientX, y: e.touches[0].clientY, epoch: zoomEpoch }
            : null;
        }
        return;
      }
      if (pan) {
        if (e.touches.length === 0) pan = null;
        return;
      }
      if (!tracking) return; // only a clean single-touch drag reaches here
      tracking = false;
      if (isZoomed()) {
        // Zoom engaged between the last touchmove and the lift (same race
        // as the touchmove guard): never commit a turn or snap-back that
        // would stomp the zoom transform.
        axisLock = null;
        return;
      }
      const t = e.changedTouches[0];
      const dx = t.clientX - startX;
      const dy = t.clientY - startY;
      const img = $("#page-img");

      if (axisLock === "x") {
        if (Math.abs(dx) > SWIPE_COMMIT_PX) {
          resetDragStyles(img); // instant — the commit slide-in covers the reset
          if (dx < 0) readerState.handlers.next();
          else readerState.handlers.prev();
        } else {
          snapBackDrag(img);
        }
      } else if (axisLock === null) {
        // No touchmove observed (e.g. a synthetic touchstart+touchend with
        // nothing in between) — same check the pre-Item-12 code always did.
        if (Math.abs(dx) > SWIPE_COMMIT_PX && Math.abs(dx) > Math.abs(dy)) {
          if (dx < 0) readerState.handlers.next();
          else readerState.handlers.prev();
        }
      }
      axisLock = null;
    }, { passive: true });

    // F1: a system-cancelled gesture (incoming call, notification shade,
    // browser edge-gesture) fires touchcancel instead of touchend — without
    // this, the drag-follow transform/will-change stays on #page-img
    // indefinitely (only recovered by the next *committed* turn). A
    // cancelled pinch/pan keeps the current zoom (platform convention) but
    // stops all tracking.
    el.addEventListener("touchcancel", () => {
      pinch = null;
      pan = null;
      touchPinchActive = false;
      if (!tracking) return;
      abortDrag();
    }, { passive: true });
  }

  // Build the Aa typography popover markup (reflowable books only). Values and
  // selected/disabled state are set live by renderTypoPanelState() so this can
  // stay a static template.
  function typoPanelHtml() {
    const familyRadios = TYPO_FAMILY_ORDER.map(
      // FONT_STACKS values contain double quotes ('"Lora Variable", …'); the
      // style attribute MUST be single-quoted or the inner quotes truncate it
      // (breaking the in-face preview). No stack contains a single quote.
      (k) =>
        `<button type="button" class="typo-radio" role="radio" data-family="${k}" aria-checked="false" tabindex="-1" style='font-family:${FONT_STACKS[k]}'>${esc(TYPO_FAMILY_LABELS[k])}</button>`
    ).join("");
    const widthSegs = COLUMN_WIDTHS.map(
      (w) =>
        `<button type="button" class="typo-seg" data-width="${w}" aria-pressed="false" aria-label="${esc(TYPO_WIDTH_LABELS[w])} column">${esc(TYPO_WIDTH_LABELS[w])}</button>`
    ).join("");
    return `
      <div class="typo-panel" id="typo-panel" role="dialog" aria-label="Text settings" hidden>
        <div class="typo-row">
          <span class="typo-row-label" id="typo-fontsize-label">Font size</span>
          <div class="typo-stepper" role="group" aria-labelledby="typo-fontsize-label">
            <button type="button" id="typo-fontsize-dec" aria-label="Decrease font size">A&minus;</button>
            <span id="typo-fontsize-val" aria-live="polite"></span>
            <button type="button" id="typo-fontsize-inc" aria-label="Increase font size">A+</button>
          </div>
        </div>
        <div class="typo-row">
          <span class="typo-row-label" id="typo-linespacing-label">Line spacing</span>
          <div class="typo-stepper" role="group" aria-labelledby="typo-linespacing-label">
            <button type="button" id="typo-linespacing-dec" aria-label="Decrease line spacing">&minus;</button>
            <span id="typo-linespacing-val" aria-live="polite"></span>
            <button type="button" id="typo-linespacing-inc" aria-label="Increase line spacing">+</button>
          </div>
        </div>
        <div class="typo-row typo-row-col">
          <span class="typo-row-label" id="typo-family-label">Reading font</span>
          <div class="typo-family" id="typo-family" role="radiogroup" aria-labelledby="typo-family-label">${familyRadios}</div>
        </div>
        <div class="typo-row">
          <span class="typo-row-label" id="typo-width-label">Width</span>
          <div class="typo-width" id="typo-width" role="group" aria-labelledby="typo-width-label">${widthSegs}</div>
        </div>
      </div>`;
  }

  // Sync every control's displayed value / selected / disabled state from the
  // current typography. Updates the radiogroup IN PLACE (never innerHTML —
  // that would drop keyboard focus).
  function renderTypoPanelState() {
    const t = getTypography();
    const fsVal = $("#typo-fontsize-val");
    if (fsVal) fsVal.textContent = `${t.fontSize} px`;
    const lhVal = $("#typo-linespacing-val");
    if (lhVal) lhVal.textContent = t.lineHeight.toFixed(1);
    // Disable at range ends — but if the focused stepper is the one being
    // disabled, hand focus to its still-enabled sibling first (only one end
    // ever disables), so keyboard focus never escapes to <body> mid-adjust.
    const setDisabled = (btn, shouldDisable, sibling) => {
      if (!btn) return;
      if (shouldDisable && document.activeElement === btn && sibling && !sibling.disabled) {
        sibling.focus();
      }
      btn.disabled = shouldDisable;
    };
    const fsDec = $("#typo-fontsize-dec"), fsInc = $("#typo-fontsize-inc");
    setDisabled(fsDec, t.fontSize <= TYPO_FS_MIN, fsInc);
    setDisabled(fsInc, t.fontSize >= TYPO_FS_MAX, fsDec);
    const lhDec = $("#typo-linespacing-dec"), lhInc = $("#typo-linespacing-inc");
    setDisabled(lhDec, t.lineHeight <= TYPO_LH_MIN + 1e-9, lhInc);
    setDisabled(lhInc, t.lineHeight >= TYPO_LH_MAX - 1e-9, lhDec);
    const group = $("#typo-family");
    if (group) {
      group.querySelectorAll('[role="radio"]').forEach((r) => {
        const sel = r.getAttribute("data-family") === t.fontFamily;
        r.setAttribute("aria-checked", sel ? "true" : "false");
        r.tabIndex = sel ? 0 : -1;
      });
    }
    const widthGroup = $("#typo-width");
    if (widthGroup) {
      widthGroup.querySelectorAll("[data-width]").forEach((b) => {
        b.setAttribute("aria-pressed", Number(b.getAttribute("data-width")) === t.columnWidth ? "true" : "false");
      });
    }
  }

  function closeTypoPanel(restoreFocus) {
    const panel = $("#typo-panel");
    const btn = $("#typo-btn");
    if (panel) panel.hidden = true;
    if (btn) btn.setAttribute("aria-expanded", "false");
    typoPanelOpen = false;
    if (restoreFocus && btn) btn.focus();
  }

  function openTypoPanel() {
    const panel = $("#typo-panel");
    const btn = $("#typo-btn");
    if (!panel || !btn) return;
    if (tocPanelOpen) closeTocPanel(false); // only one bottom-chrome panel open
    if (bookmarkPanelOpen) closeBookmarkPanel(false);
    if (hlPanelOpen) closeHlPanel(false);
    if (vocabPanelOpen) closeVocabPanel(false);
    renderTypoPanelState();
    panel.hidden = false;
    btn.setAttribute("aria-expanded", "true");
    typoPanelOpen = true;
    // Move focus into the panel (the selected font radio, tabindex 0).
    const sel = panel.querySelector('[role="radio"][aria-checked="true"]') || panel.querySelector("button");
    if (sel) sel.focus();
  }

  // Roving-tabindex + Space/Enter activation for the font radiogroup. Handled
  // keys call preventDefault + stopPropagation so Arrow keys don't reach the
  // global reader handler (chapter nav) and Space doesn't page-scroll.
  function onTypoFamilyKeydown(e) {
    const keys = ["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", " ", "Spacebar", "Enter"];
    if (!keys.includes(e.key)) return;
    e.preventDefault();
    e.stopPropagation();
    const t = getTypography();
    let i = TYPO_FAMILY_ORDER.indexOf(t.fontFamily);
    if (i < 0) i = 0;
    if (e.key === "ArrowDown" || e.key === "ArrowRight") i = (i + 1) % TYPO_FAMILY_ORDER.length;
    else if (e.key === "ArrowUp" || e.key === "ArrowLeft") i = (i - 1 + TYPO_FAMILY_ORDER.length) % TYPO_FAMILY_ORDER.length;
    const nextFamily = TYPO_FAMILY_ORDER[i];
    if (nextFamily !== t.fontFamily) {
      changeTypography({ fontFamily: nextFamily });
      renderTypoPanelState();
    }
    const nextRadio = $("#typo-family").querySelector(`[data-family="${nextFamily}"]`);
    if (nextRadio) nextRadio.focus();
  }

  // Keep the global reader shortcuts (Arrow = chapter turn, Space = page
  // scroll, Home/End/f) from firing while the user is operating the Aa button
  // or panel. stopPropagation ONLY — never preventDefault — so native button
  // activation (Enter/Space) and the radiogroup's own arrow handling still
  // work. Escape is deliberately NOT contained: it must bubble to #reader-root
  // to close the panel. The font radiogroup stops its own keys first (deeper
  // target), so this never double-handles them.
  const TYPO_CONTAINED_KEYS = ["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End", " ", "Spacebar", "f", "F"];
  function containReaderKeys(e) {
    if (TYPO_CONTAINED_KEYS.includes(e.key)) e.stopPropagation();
  }

  function wireTypoControls() {
    const btn = $("#typo-btn");
    if (!btn) return;
    btn.addEventListener("keydown", containReaderKeys);
    $("#typo-panel").addEventListener("keydown", containReaderKeys);
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      if (typoPanelOpen) closeTypoPanel(true);
      else openTypoPanel();
    });
    const step = (patch) => { changeTypography(patch); renderTypoPanelState(); };
    $("#typo-fontsize-dec").addEventListener("click", () => step({ fontSize: getTypography().fontSize - TYPO_FS_STEP }));
    $("#typo-fontsize-inc").addEventListener("click", () => step({ fontSize: getTypography().fontSize + TYPO_FS_STEP }));
    $("#typo-linespacing-dec").addEventListener("click", () => step({ lineHeight: getTypography().lineHeight - TYPO_LH_STEP }));
    $("#typo-linespacing-inc").addEventListener("click", () => step({ lineHeight: getTypography().lineHeight + TYPO_LH_STEP }));
    const family = $("#typo-family");
    family.addEventListener("click", (e) => {
      const radio = e.target.closest("[data-family]");
      if (!radio) return;
      step({ fontFamily: radio.getAttribute("data-family") });
      radio.focus();
    });
    family.addEventListener("keydown", onTypoFamilyKeydown);
    $("#typo-width").addEventListener("click", (e) => {
      const seg = e.target.closest("[data-width]");
      if (seg) step({ columnWidth: Number(seg.getAttribute("data-width")) });
    });

    wirePanelRootDismissal({
      isOpen: () => typoPanelOpen,
      close: closeTypoPanel,
      panelSel: "#typo-panel",
      btnSel: "#typo-btn",
    });
  }

  // Shared #reader-root dismissal for a bottom-chrome popover (Aa panel, TOC
  // drawer). Listeners live on #reader-root so they die when a view swaps
  // app().innerHTML (no document-level leak). Escape closes the panel BEFORE
  // the global reader Esc-back handler — this runs in the bubble phase on a
  // descendant of document, so stopPropagation pre-empts it. An outside click
  // dismisses, except clicks on the trigger (btnSel) or any keepOpenSel
  // element (e.g. the reader's Prev/Next buttons, so a chapter turn keeps the
  // panel open). `isOpen` is a getter so the current flag value is read at
  // event time, not captured stale.
  function wirePanelRootDismissal({ isOpen, close, panelSel, btnSel, keepOpenSel }) {
    const root = $("#reader-root");
    if (!root) return;
    root.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && isOpen()) {
        e.preventDefault();
        e.stopPropagation();
        close(true);
      }
    });
    root.addEventListener("click", (e) => {
      if (!isOpen()) return;
      if (e.target.closest(panelSel) || e.target.closest(btnSel)) return;
      if (keepOpenSel && e.target.closest(keepOpenSel)) return;
      close(false);
    });
  }

  // ── Table of contents (flat — the backend never nests children) ─────────
  function tocEntryCount(entries) {
    if (!Array.isArray(entries)) return 0;
    return entries.reduce((n, e) => n + 1 + tocEntryCount(e.children), 0);
  }
  function maxChapterIndex(entries) {
    if (!Array.isArray(entries)) return -1;
    return entries.reduce(
      (m, e) => Math.max(m, e.chapter_index, maxChapterIndex(e.children)),
      -1
    );
  }
  // Pre-order flat projection (children after their parent). children is
  // always [] today, but recurse defensively so a future nested backend
  // still lists every entry.
  function flattenToc(entries, out) {
    out = out || [];
    if (!Array.isArray(entries)) return out;
    for (const e of entries) {
      out.push({ label: e.label, chapter_index: e.chapter_index });
      flattenToc(e.children, out);
    }
    return out;
  }
  // Fetch the TOC once per book open. Navigable only when there's more than
  // one entry AND every index fits the reader's chapter range (count =
  // total_chapters||1, and gotoReaderIndex refuses index>=count) — otherwise
  // taps would dead-clamp to 0, so we degrade to a plain label instead.
  async function loadToc() {
    if (!readerState || readerState.mode === "page") return;
    const bookId = readerState.id;
    readerState.tocStatus = "loading";
    readerState.toc = null;
    try {
      const resp = await api(`/api/books/${bookId}/chapters`);
      if (!readerState || readerState.id !== bookId) return; // superseded
      if (resp && resp.ok) {
        const toc = await resp.json();
        // Re-check after the second await: a book switch during resp.json()
        // must not let book A's TOC overwrite book B's state.
        if (!readerState || readerState.id !== bookId) return;
        readerState.toc = Array.isArray(toc) ? toc : [];
        const navigable =
          tocEntryCount(readerState.toc) > 1 &&
          maxChapterIndex(readerState.toc) < readerState.count;
        readerState.tocStatus = navigable ? "ready" : "none";
      } else {
        readerState.tocStatus = "none";
      }
    } catch (e) {
      if (readerState && readerState.id === bookId) readerState.tocStatus = "none";
    }
    if (readerState && readerState.id === bookId) buildTocTrigger();
    // If the bookmark panel is open, its auto-labels may have rendered with the
    // "Chapter N" fallback before the TOC arrived — re-render now so they pick
    // up the real chapter titles (but not while a rename input is active).
    if (readerState && readerState.id === bookId && bookmarkPanelOpen && !bookmarkRenaming)
      renderBookmarkList();
  }

  // Build the trigger to match tocStatus. Interactive (button) only when the
  // TOC is navigable; otherwise a plain, non-interactive label — never a
  // disabled button (disabled .reader-toolbar buttons still show chrome).
  function buildTocTrigger() {
    const slot = $("#toc-slot");
    if (!slot) return;
    const ready = readerState && readerState.tocStatus === "ready";
    slot.innerHTML = ready
      ? `<button id="toc-btn" class="toc-trigger" aria-haspopup="dialog" aria-expanded="false"><span id="page-label"></span><span class="toc-caret" aria-hidden="true">&#9662;</span></button>`
      : `<span class="toc-plain"><span id="page-label"></span></span>`;
    if (ready) {
      const btn = $("#toc-btn");
      btn.addEventListener("keydown", containReaderKeys);
      btn.addEventListener("click", (e) => {
        e.stopPropagation();
        if (tocPanelOpen) closeTocPanel(true);
        else openTocPanel();
      });
    }
    updateProgressUI(); // repopulate #page-label / prev-next disabled state
  }

  function tocPanelHtml() {
    return `
      <div class="toc-panel" id="toc-panel" role="dialog" aria-label="Table of contents">
        <div class="toc-head">
          <span class="toc-title">Contents</span>
          <button type="button" id="toc-close" class="toc-close" aria-label="Close contents">&times;</button>
        </div>
        <ul class="toc-list" id="toc-list"></ul>
      </div>`;
  }

  function renderTocList() {
    const list = $("#toc-list");
    if (!list || !readerState || !Array.isArray(readerState.toc)) return;
    const cur = readerState.index;
    list.innerHTML = flattenToc(readerState.toc)
      .map((e) => {
        const isCur = e.chapter_index === cur;
        return (
          `<li><button type="button" class="toc-entry${isCur ? " current" : ""}"` +
          `${isCur ? ' aria-current="true"' : ""} data-index="${e.chapter_index}">${esc(e.label)}</button></li>`
        );
      })
      .join("");
  }

  function closeTocPanel(restoreFocus) {
    const panel = $("#toc-panel");
    const btn = $("#toc-btn");
    if (panel) panel.classList.remove("open");
    if (btn) btn.setAttribute("aria-expanded", "false");
    tocPanelOpen = false;
    if (restoreFocus && btn) btn.focus();
  }

  function openTocPanel() {
    const panel = $("#toc-panel");
    const btn = $("#toc-btn");
    if (!panel || !btn || !readerState || readerState.tocStatus !== "ready") return;
    if (typoPanelOpen) closeTypoPanel(false); // only one bottom-chrome panel open
    if (bookmarkPanelOpen) closeBookmarkPanel(false);
    if (hlPanelOpen) closeHlPanel(false);
    if (vocabPanelOpen) closeVocabPanel(false);
    renderTocList();
    panel.classList.add("open");
    btn.setAttribute("aria-expanded", "true");
    tocPanelOpen = true;
    const current = panel.querySelector(".toc-entry.current");
    const target = current || panel.querySelector(".toc-entry");
    if (current) current.scrollIntoView({ block: "nearest" });
    if (target) target.focus();
  }

  // Panel DOM is static (built with the chrome), so its close-button and list
  // listeners bind once here. The trigger button (dynamic) is wired in
  // buildTocTrigger.
  function wireTocControls() {
    const closeBtn = $("#toc-close");
    if (closeBtn) closeBtn.addEventListener("click", () => closeTocPanel(true));
    const list = $("#toc-list");
    if (list)
      list.addEventListener("click", (e) => {
        const entry = e.target.closest(".toc-entry");
        if (!entry) return;
        let idx = parseInt(entry.getAttribute("data-index"), 10);
        if (!Number.isFinite(idx)) return;
        idx = Math.min(Math.max(idx, 0), readerState.count - 1);
        closeTocPanel(false);
        clearHlJump(); // a TOC jump supersedes any pending highlight jump
        clearVocabJump(); // and any pending vocabulary jump
        gotoReaderIndex(idx, { instant: true });
      });
    // Arrow/Home/End/Space/f inside the open panel must not reach the global
    // reader shortcuts (chapter turns / scroll) — same containment as the Aa
    // panel. Escape is deliberately NOT contained: it bubbles to #reader-root
    // (below) to close the panel before the global Esc-back handler.
    const panel = $("#toc-panel");
    if (panel) panel.addEventListener("keydown", containReaderKeys);
    // Esc + outside-click dismissal, shared with the Aa panel. Prev/Next are
    // exempt (keepOpenSel) so a chapter turn keeps the panel open and its
    // current-chapter highlight re-syncs (see updateProgressUI).
    wirePanelRootDismissal({
      isOpen: () => tocPanelOpen,
      close: closeTocPanel,
      panelSel: "#toc-panel",
      btnSel: "#toc-btn",
      keepOpenSel: "#prev-btn, #next-btn",
    });
  }

  // ── Bookmarks (left slide-in drawer, present in both reader modes) ───────
  function bookmarkPanelHtml() {
    return `
      <div class="bookmark-panel" id="bookmark-panel" role="dialog" aria-label="Bookmarks">
        <div class="bookmark-head">
          <span class="bookmark-title">Bookmarks</span>
          <button id="bookmark-close" class="bookmark-close" aria-label="Close">&times;</button>
        </div>
        <button id="bookmark-add-btn" class="bookmark-add">+ Add bookmark here</button>
        <div class="bookmark-list" id="bookmark-list"></div>
      </div>`;
  }

  // Fetch bookmarks once per book, lazily on first panel open. Guarded by an
  // id-recheck after resp.json() so a book switch mid-fetch can't overwrite
  // another book's state (same guard as loadToc).
  async function loadBookmarks() {
    if (!readerState) return;
    const bookId = readerState.id;
    if (bookmarksState && bookmarksState.bookId === bookId && bookmarksState.loaded) return;
    bookmarksState = { bookId, loaded: false, items: [] };
    try {
      const resp = await api(`/api/books/${bookId}/bookmarks`);
      if (!readerState || readerState.id !== bookId) return;
      if (resp && resp.ok) {
        const rows = await resp.json();
        if (!readerState || readerState.id !== bookId) return;
        // Backend returns oldest-first; the UI shows newest-first. LOCAL-
        // preferred merge: an item added OR renamed locally while this GET was
        // in flight (or a rename committed just before it resolved) must win
        // over the older server snapshot — keep every local item, append only
        // server rows we don't already hold. A wholesale replace would drop a
        // concurrent add-here or revert a just-committed rename.
        const serverRows = Array.isArray(rows) ? rows.slice().reverse() : [];
        const localItems =
          bookmarksState && bookmarksState.bookId === bookId ? bookmarksState.items : [];
        const localIds = new Set(localItems.map((b) => b.id));
        bookmarksState = {
          bookId,
          loaded: true,
          items: [...localItems, ...serverRows.filter((s) => !localIds.has(s.id))],
        };
      }
    } catch (e) {
      if (readerState && readerState.id === bookId) bookmarksState = { bookId, loaded: true, items: [] };
    }
    // Skip the re-render while a rename input is active (see bookmarkRenaming).
    if (readerState && readerState.id === bookId && bookmarkPanelOpen && !bookmarkRenaming)
      renderBookmarkList();
  }

  function buildBookmarkTrigger() {
    const btn = $("#bookmark-btn");
    if (!btn) return;
    btn.addEventListener("keydown", containReaderKeys);
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      if (bookmarkPanelOpen) closeBookmarkPanel(true);
      else openBookmarkPanel();
    });
  }

  // Auto-label for an unnamed bookmark: chapter title from the TOC if present
  // (chapter mode only — page-mode books have no TOC), else "Chapter N" /
  // "Page N" per reader mode; plus the "% through" from scroll_position.
  function bookmarkLabel(bm) {
    if (bm.name && bm.name.trim()) return esc(bm.name);
    const isPage = readerState && readerState.mode === "page";
    let label = null;
    if (!isPage && readerState && Array.isArray(readerState.toc)) {
      const entry = readerState.toc.find((e) => e.chapter_index === bm.chapter_index);
      if (entry && entry.label) label = entry.label;
    }
    if (!label) label = `${isPage ? "Page" : "Chapter"} ${bm.chapter_index + 1}`;
    const pct = Math.round(clampScrollRatio(bm.scroll_position) * 100);
    return `${esc(label)} <span class="bookmark-pct">· ${pct}% through</span>`;
  }

  function renderBookmarkList() {
    const list = $("#bookmark-list");
    if (!list) return;
    const items = (bookmarksState && bookmarksState.items) || [];
    if (!items.length) {
      list.innerHTML = `<p class="bookmark-empty">No bookmarks yet</p>`;
      return;
    }
    list.innerHTML = items
      .map((bm) => {
        // Numbers today, but escape+stringify so the attribute-safety invariant
        // holds even if malformed state ever reaches here.
        const idx = Number.isFinite(bm.chapter_index) ? String(bm.chapter_index) : "0";
        const scroll = Number.isFinite(bm.scroll_position) ? String(bm.scroll_position) : "0";
        return `
      <div class="bookmark-entry" data-id="${esc(bm.id)}" data-index="${esc(idx)}" data-scroll="${esc(scroll)}" tabindex="0" role="button">
        <span class="bookmark-entry-label">${bookmarkLabel(bm)}</span>
        <button class="bookmark-rename" data-id="${esc(bm.id)}" aria-label="Rename bookmark">&#9998;</button>
        <button class="bookmark-del" data-id="${esc(bm.id)}" aria-label="Delete bookmark">&times;</button>
      </div>`;
      })
      .join("");
  }

  function closeBookmarkPanel(restoreFocus) {
    const panel = $("#bookmark-panel");
    const btn = $("#bookmark-btn");
    if (panel) panel.classList.remove("open");
    if (btn) btn.setAttribute("aria-expanded", "false");
    bookmarkPanelOpen = false;
    if (restoreFocus && btn) btn.focus();
  }

  function openBookmarkPanel() {
    const panel = $("#bookmark-panel");
    const btn = $("#bookmark-btn");
    if (!panel || !btn || !readerState) return;
    // Mutual exclusion with the other bottom-chrome panels.
    if (typoPanelOpen) closeTypoPanel(false);
    if (tocPanelOpen) closeTocPanel(false);
    if (hlPanelOpen) closeHlPanel(false);
    if (vocabPanelOpen) closeVocabPanel(false);
    renderBookmarkList();
    panel.classList.add("open");
    btn.setAttribute("aria-expanded", "true");
    bookmarkPanelOpen = true;
    loadBookmarks(); // lazy fetch; re-renders on completion if still open
  }

  // Jump to a bookmark. Different chapter: navigate + let the render path
  // consume pendingScrollRestore. SAME chapter: gotoReaderIndex would
  // re-navigate to an unchanged hash (no hashchange -> no render -> restore
  // never runs), so restore the scroll directly.
  function jumpToBookmark(chapterIndex, scrollPosition) {
    if (!readerState) return;
    const idx = Math.min(Math.max(chapterIndex, 0), readerState.count - 1);
    closeBookmarkPanel(false);
    clearHlJump(); // a bookmark jump supersedes any pending highlight jump
    clearVocabJump(); // and any pending vocabulary jump
    if (readerState.mode === "chapter" && idx === readerState.index) {
      restoreChapterScroll(scrollPosition);
    } else {
      const r = readerState.mode === "chapter" ? clampScrollRatio(scrollPosition || 0) : 0;
      // Set BOTH: pendingScrollRestore drives the post-render scroll, and
      // scrollPosition must match it or a later progress save would persist the
      // previous chapter's ratio (the restore's synthetic scroll is suppressed,
      // so the scroll handler never updates scrollPosition itself).
      readerState.pendingScrollRestore = r;
      readerState.scrollPosition = r;
      gotoReaderIndex(idx, { instant: true });
    }
  }

  // Apply an in-chapter scroll fraction to the already-rendered stage (the
  // same-chapter jump case — no re-render happens). The chapter is already on
  // screen with fonts settled, so a single rAF is enough (no fonts.ready
  // re-anchor needed like renderReaderContent's first paint). Do NOT set
  // suppressScrollSave manually — setScrollTop arms it itself, and only when
  // scrollTop actually changes (app.js:151); forcing it would swallow the
  // user's next real scroll. Update readerState.scrollPosition so a later
  // progress save (which reads it) isn't stale.
  function restoreChapterScroll(ratio) {
    const stage = $("#reader-stage");
    if (!stage) return;
    const r = clampScrollRatio(ratio || 0);
    readerState.scrollPosition = r;
    requestAnimationFrame(() => {
      const max = stage.scrollHeight - stage.clientHeight;
      if (r > 0 && max > 0) setScrollTop(stage, r * max);
      else stage.scrollTop = 0;
    });
  }

  async function addBookmarkHere() {
    if (!readerState) return;
    const bookId = readerState.id;
    const chapterIndex = readerState.index;
    const scrollPosition =
      readerState.mode === "chapter"
        ? clampScrollRatio(readerState.scrollPosition || 0)
        : (readerState.count > 1 ? readerState.index / readerState.count : 0);
    try {
      const resp = await apiWrite(`/api/books/${bookId}/bookmarks`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ chapter_index: chapterIndex, scroll_position: scrollPosition }),
        credentials: "same-origin",
      });
      if (resp.status === 401) { authenticated = false; showLogin(); return; }
      if (!resp.ok) return;
      const created = await resp.json();
      // Race guard: a book switch during the POST must not mutate another book.
      if (!readerState || readerState.id !== bookId || !bookmarksState || bookmarksState.bookId !== bookId) return;
      // Upsert: a concurrent initial GET may have already merged this id — drop
      // any existing copy before prepending so the list can't show a duplicate.
      bookmarksState.items = bookmarksState.items.filter((b) => b.id !== created.id);
      bookmarksState.items.unshift(created); // newest-first
      renderBookmarkList();
      const firstEntry = $("#bookmark-list .bookmark-entry");
      if (firstEntry) beginRename(firstEntry, created.id);
    } catch (e) { /* offline: bookmarks are live-only; ignore */ }
  }

  async function deleteBookmark(id) {
    if (!readerState) return;
    const bookId = readerState.id;
    try {
      const resp = await apiWrite(`/api/books/${bookId}/bookmarks/${id}`, {
        method: "DELETE",
        credentials: "same-origin",
      });
      if (resp.status === 401) { authenticated = false; showLogin(); return; }
      if (!(resp.ok || resp.status === 204)) return;
      if (!readerState || readerState.id !== bookId || !bookmarksState || bookmarksState.bookId !== bookId) return;
      bookmarksState.items = bookmarksState.items.filter((b) => b.id !== id);
      renderBookmarkList();
    } catch (e) { /* ignore */ }
  }

  function beginRename(entry, id) {
    const labelEl = entry.querySelector(".bookmark-entry-label");
    if (!labelEl) return;
    const bm = bookmarksState && bookmarksState.items.find((b) => b.id === id);
    const current = (bm && bm.name) || "";
    const input = document.createElement("input");
    input.type = "text";
    input.className = "bookmark-name-input";
    input.maxLength = 100; // server truncates too; cap the input to match
    input.value = current;
    bookmarkRenaming = true;
    labelEl.replaceWith(input);
    input.focus();
    input.select();
    // A click/tap inside the field (to place the cursor) would otherwise bubble
    // to the #bookmark-list handler, match .bookmark-entry, and fire
    // jumpToBookmark — closing the panel and discarding the edit. Stop it here.
    input.addEventListener("mousedown", (e) => e.stopPropagation());
    input.addEventListener("click", (e) => e.stopPropagation());
    let done = false;
    const commit = async () => {
      if (done) return;
      done = true;
      bookmarkRenaming = false;
      // No change → no needless PUT (covers the common auto-rename-then-click-
      // away with an untouched empty field); just drop the input.
      if (input.value === current) {
        renderBookmarkList();
        return;
      }
      await renameBookmark(id, input.value);
    };
    input.addEventListener("keydown", (e) => {
      e.stopPropagation(); // don't let typing reach reader shortcuts
      if (e.key === "Enter") {
        e.preventDefault();
        commit();
      } else if (e.key === "Escape") {
        e.preventDefault();
        done = true;
        bookmarkRenaming = false;
        renderBookmarkList();
      }
    });
    input.addEventListener("blur", commit);
  }

  async function renameBookmark(id, rawName) {
    if (!readerState) {
      renderBookmarkList();
      return;
    }
    const bookId = readerState.id;
    try {
      const resp = await apiWrite(`/api/books/${bookId}/bookmarks/${id}`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name: rawName }),
        credentials: "same-origin",
      });
      if (resp.status === 401) { authenticated = false; showLogin(); return; }
      if (resp.ok) {
        const updated = await resp.json();
        if (!readerState || readerState.id !== bookId || !bookmarksState || bookmarksState.bookId !== bookId) return;
        const i = bookmarksState.items.findIndex((b) => b.id === id);
        if (i >= 0) bookmarksState.items[i] = updated;
      }
    } catch (e) { /* ignore; fall through to re-render */ }
    // Re-render (a failed PUT reverts the entry to its stored name — an honest
    // "didn't save" signal). Guard against a book switch mid-PUT so we never
    // rebuild another book's list.
    if (readerState && bookmarksState && readerState.id === bookId && bookmarksState.bookId === bookId)
      renderBookmarkList();
  }

  function wireBookmarkControls() {
    const closeBtn = $("#bookmark-close");
    if (closeBtn) closeBtn.addEventListener("click", () => closeBookmarkPanel(true));
    const addBtn = $("#bookmark-add-btn");
    if (addBtn) addBtn.addEventListener("click", (e) => { e.stopPropagation(); addBookmarkHere(); });
    const list = $("#bookmark-list");
    if (list)
      list.addEventListener("click", (e) => {
        // A click inside an active rename input must not be treated as an entry
        // tap (belt-and-suspenders with the input's own stopPropagation).
        if (e.target.closest(".bookmark-name-input")) return;
        const del = e.target.closest(".bookmark-del");
        if (del) { e.stopPropagation(); deleteBookmark(del.getAttribute("data-id")); return; }
        const ren = e.target.closest(".bookmark-rename");
        if (ren) {
          e.stopPropagation();
          const en = ren.closest(".bookmark-entry");
          if (en) beginRename(en, ren.getAttribute("data-id"));
          return;
        }
        const entry = e.target.closest(".bookmark-entry");
        if (!entry) return;
        const idx = parseInt(entry.getAttribute("data-index"), 10);
        const scroll = parseFloat(entry.getAttribute("data-scroll"));
        if (!Number.isFinite(idx)) return;
        jumpToBookmark(idx, Number.isFinite(scroll) ? scroll : 0);
      });
    const panel = $("#bookmark-panel");
    if (panel) panel.addEventListener("keydown", containReaderKeys);
    wirePanelRootDismissal({
      isOpen: () => bookmarkPanelOpen,
      close: closeBookmarkPanel,
      panelSel: "#bookmark-panel",
      btnSel: "#bookmark-btn",
    });
  }

  function renderReaderChrome() {
    // A deferred tap from a previous reader (or a previous book's chrome)
    // must not fire against the freshly built one — the prev/next zones are
    // already epoch-guarded, but a stale chrome toggle would still hide the
    // new reader's chrome.
    cancelPendingTap();
    // The Aa popover and TOC panel are rebuilt with the chrome; never carry
    // open state across.
    typoPanelOpen = false;
    tocPanelOpen = false;
    bookmarkPanelOpen = false;
    hlPanelOpen = false;
    vocabPanelOpen = false;
    pendingReanchor = null;
    // A chapter turn rebuilds the chrome without a scroll event when the
    // stage was already at the top, so the scroll-dismiss path can't be
    // relied on — close a lingering Define card here explicitly.
    closeDefinePopover();
    const { book, mode, count, index, fitMode } = readerState;
    const rootClass = mode === "page" ? `reader-page ${fitMode}` : "reader-chapter";
    // Item 12: `page-img-incoming` is the animated overlay for a committed
    // turn — hidden/inert until renderPageTurn() promotes it; see there.
    const stageInner = mode === "page"
      ? `<img id="page-img" alt=""><img id="page-img-incoming" class="page-img-incoming" alt="" hidden><div class="reader-page-error" id="page-error" hidden></div>`
      : `<div class="content" id="reader-content"></div>`;
    const fitToggleBtn = mode === "page"
      ? `<button id="fit-toggle-btn">${fitMode === "fit-height" ? "Fit: Height" : "Fit: Width"}</button>`
      : "";
    // Aa typography control — reflowable books only (EPUB + MOBI).
    const typoBtn = mode === "page"
      ? ""
      : `<button id="typo-btn" aria-label="Text settings" aria-haspopup="dialog" aria-expanded="false">Aa</button>`;
    const typoPanel = mode === "page" ? "" : typoPanelHtml();
    const tocPanel = mode === "page" ? "" : tocPanelHtml();
    // Bookmark control — present in BOTH reader modes (chapter + page).
    const bookmarkBtn =
      `<button id="bookmark-btn" aria-label="Bookmarks" aria-haspopup="dialog" aria-expanded="false">&#128278;</button>`;
    const bookmarkPanel = bookmarkPanelHtml();
    // Highlights control — chapter mode only (page-image formats have no
    // selectable text, spec §5 format gating).
    const hlBtn = mode === "page"
      ? ""
      : `<button id="hl-btn" aria-label="Highlights" aria-haspopup="dialog" aria-expanded="false">&#128397;</button>`;
    const hlPanel = mode === "page" ? "" : hlPanelHtml();
    // Saved-words (vocabulary) control — chapter mode only, same format
    // gating as highlights (words can only be captured from selectable
    // text). Starts hidden: visibility additionally depends on the
    // session-cached vocabularyAvailable() status, applied once it resolves
    // (applyVocabButtonVisibility, called below and off ensureDictionaryStatus).
    const vocabBtn = mode === "page"
      ? ""
      : `<button id="vocab-btn" aria-label="Saved words" aria-haspopup="dialog" aria-expanded="false" hidden>&#128218;</button>`;
    const vocabPanel = mode === "page" ? "" : vocabPanelHtml();

    app().innerHTML = `
      <div class="${rootClass}" id="reader-root">
        <div class="reader-chrome-top">
          <div class="header">
            <button class="back-btn" id="back-btn" aria-label="Back">&larr;</button>
            <h1 class="nav-book-title">${esc(book.title)}</h1>
            ${navIconsHtml("")}
          </div>
        </div>
        <div class="reader-stage" id="reader-stage" tabindex="-1">${stageInner}</div>
        <div class="reader-chrome-bottom">
          <div class="reader-toolbar">
            <button id="prev-btn">Prev</button>
            ${mode === "page"
              ? `<input type="range" id="page-slider" min="0" max="${count - 1}" value="${index}" aria-label="Page slider"><span id="page-label"></span>`
              : `<span id="toc-slot" class="toc-slot"></span>`}
            <button id="next-btn">Next</button>
            ${typoBtn}
            ${fitToggleBtn}
            ${bookmarkBtn}
            ${hlBtn}
            ${vocabBtn}
          </div>
          ${typoPanel}
          ${tocPanel}
          ${bookmarkPanel}
          ${hlPanel}
          ${vocabPanel}
        </div>
        <button class="chrome-toggle-fab" id="chrome-toggle-btn" title="Toggle toolbar" aria-label="Toggle toolbar">&#8942;</button>
      </div>`;

    $("#back-btn").addEventListener("click", () => readerState.handlers.goBack());
    $("#prev-btn").addEventListener("click", () => readerState.handlers.prev());
    $("#next-btn").addEventListener("click", () => readerState.handlers.next());
    $("#chrome-toggle-btn").addEventListener("click", () => readerState.handlers.toggleChrome());
    const pageSlider = $("#page-slider");
    if (pageSlider) pageSlider.addEventListener("change", (e) => {
      // K2: return focus to the document so shortcuts work immediately
      // after a slider drag, without waiting on isTypingTarget special-casing.
      e.target.blur();
      // Item 12: a slider jump stays instant — no slide-in.
      gotoReaderIndex(parseInt(e.target.value, 10), { instant: true });
    });
    bindNavIcons();
    // M5: the nav cluster's Vocabulary icon is gated the same way in every
    // mode (unlike the chapter-mode-only vocab-btn toolbar button below).
    ensureDictionaryStatus().then(applyVocabNavVisibility);

    if (mode === "page") {
      const img = $("#page-img");
      bindClickZones(img);
      // M2: the gesture controller listens on the stage, not the image —
      // a pinch finger landing in the gutter beside a narrow page still
      // counts, and swipes keep working from the whole stage.
      bindSwipe($("#reader-stage"));
      bindWheelZoom($("#reader-stage"));
      img.addEventListener("error", handlePageImageError);
      img.addEventListener("load", () => {
        img.style.display = "";
        const errEl = $("#page-error");
        if (errEl) errEl.hidden = true;
        // A page that loads (including from a recovery blob) re-arms the
        // one-shot cache-bypass retry for its index.
        if (readerState) readerState.pageRetryIndex = null;
      });
      $("#fit-toggle-btn").addEventListener("click", () => {
        resetZoom();
        readerState.fitMode = readerState.fitMode === "fit-height" ? "fit-width" : "fit-height";
        safeStorageSet("carrel_reader_fit_mode", readerState.fitMode);
        applyFitMode();
      });
    } else {
      // F2b: the chrome (and its #reader-stage element) is built once per
      // book, so this listener stays bound across chapter turns — only the
      // stage's content is swapped by renderReaderContent().
      bindChapterScrollTracking($("#reader-stage"));
      // Highlights: mark taps delegate on the stage (survives every chapter
      // innerHTML rebuild); the stage node itself is rebuilt per book entry,
      // so re-binding here is safe — no duplicate listeners.
      bindHighlightMarkTaps();
      // Define (M2): kick the session-cached status fetch on first entry into
      // a chapter-mode reader. Memoized — later book entries and chapter
      // turns are no-ops once it resolves. The vocab-btn's visibility depends
      // on this same status (vocabularyAvailable()), so re-apply it once the
      // fetch lands — it may still be null on the very first book opened in a
      // fresh session.
      ensureDictionaryStatus().then(applyVocabButtonVisibility);
      // Web typography: apply saved font/spacing/family/width to the chapter
      // column once here (inline styles persist across chapter-turn innerHTML
      // swaps). EPUB + MOBI both reach this branch (mode !== "page").
      applyTypography($("#reader-content"));
      wireTypoControls();
      // TOC: static panel listeners bind once; the trigger starts as a plain
      // label and upgrades to interactive when the fetch lands.
      readerState.toc = null;
      readerState.tocStatus = "loading";
      wireTocControls();
      buildTocTrigger();
      loadToc();
    }

    // Bookmarks: present in BOTH reader modes. Wire once here (the block was
    // identical in both branches). loadBookmarks() is lazy (first panel open),
    // so it is NOT called here.
    bookmarksState = null;
    wireBookmarkControls();
    buildBookmarkTrigger();

    // Highlights: reset per book entry (mirrors the bookmarksState reset
    // above), then — chapter mode only — fetch this book's highlights and
    // rebuild the on-screen chapter when the GET resolves (spec §3: full
    // rebuild from clean HTML, never incremental).
    highlightsState = { bookId: null, items: [], loaded: false, failed: false };
    currentChapterCleanHtml = null;
    currentChapterIndex = -1;
    clearHlJump(); // book entry/switch: a stale pending jump must not survive
    if (readerState.mode !== "page") {
      wireHlControls();
      buildHlTrigger();
      loadHighlightsForBook(readerState.id).then(() => {
        rerenderChapterHighlights();
      });
    }

    // Saved words: reset per book entry (mirrors highlightsState above).
    // Fetched lazily on first panel open (bookmark precedent), not here.
    vocabState = { bookId: null, items: [], loaded: false, failed: false };
    clearVocabJump(); // book entry/switch: a stale pending jump must not survive
    if (readerState.mode !== "page") {
      wireVocabControls();
      buildVocabTrigger();
      applyVocabButtonVisibility();
    }

    applyChromeVisibility();
  }

  // F2b: track the real in-chapter scroll offset as a 0..1 fraction of the
  // scrollable range (same scale as the backend's `validate_scroll_position`
  // clamp), debounced through the same save pipeline as page/chapter turns.
  function clampScrollRatio(ratio) {
    if (!Number.isFinite(ratio)) return 0;
    return Math.min(Math.max(ratio, 0), 1);
  }

  function bindChapterScrollTracking(stage) {
    if (!stage) return;
    stage.addEventListener("scroll", () => {
      if (!readerState || readerState.mode !== "chapter") return;
      // Ignore the synthetic scroll event fired by our own programmatic
      // restore (renderReaderContent) — it reflects data already saved,
      // not a new user action.
      if (readerState.suppressScrollSave) { readerState.suppressScrollSave = false; return; }
      // A genuine user scroll wins over any deferred font-ready re-anchor from
      // a typography change still in flight.
      pendingReanchor = null;
      // Highlight popovers are fixed-position — dismiss rather than drift.
      dismissHlPopoversOnScroll(stage.scrollTop);
      const max = stage.scrollHeight - stage.clientHeight;
      readerState.scrollPosition = max > 0 ? clampScrollRatio(stage.scrollTop / max) : 0;
      scheduleProgressSave();
    }, { passive: true });
  }

  // ── Highlights (spec 2026-07-23) ─────────────────────────────────────────
  // Frozen-snapshot renderer for stored text highlights (chapter mode only).
  // Marks are injected into the live DOM; the prefetch/chapter caches keep
  // storing clean HTML, and every re-render rebuilds from that clean HTML —
  // never incrementally (idempotent by construction, spec §3).

  const HIGHLIGHT_COLORS = ["#f6c445", "#7bc47f", "#6ba3d6", "#e88baf", "#e8a55d"];
  let highlightsState = { bookId: null, items: [], loaded: false, failed: false };
  // Clean (mark-free) HTML of the chapter currently on screen + its index —
  // the rebuild source for rerenderChapterHighlights(). Kept up to date in
  // renderReaderContent right where `innerHTML` is set; reset per book entry.
  let currentChapterCleanHtml = null;
  let currentChapterIndex = -1;

  // Monotonic fetch token: a bookId check alone cannot discriminate a stale
  // fetch from a SAME-book reopen (close → reopen while request 1 is still in
  // flight) — only the newest request may write state.
  let hlFetchSeq = 0;

  async function loadHighlightsForBook(bookId) {
    if (highlightsState.bookId !== bookId) {
      highlightsState = { bookId, items: [], loaded: false, failed: false };
      clearHlJump(); // book switch cancels any pending jump
    }
    const seq = ++hlFetchSeq;
    const stale = () => highlightsState.bookId !== bookId || seq !== hlFetchSeq;
    try {
      const resp = await fetch(`/api/books/${encodeURIComponent(bookId)}/highlights`, {
        credentials: "same-origin",
      });
      if (stale()) return; // book switched or reader reopened mid-flight
      if (!resp.ok) { highlightsState.failed = true; clearHlJump(); return; } // terminal failure cancels a pending jump
      const items = await resp.json();
      // Re-check AFTER the await too: a switch/reopen during json() parsing
      // must not let a stale response overwrite the newer state.
      if (stale()) return;
      // Merge, don't replace: preserve any local item missing from the fetched
      // snapshot — in-flight optimistic creates (tmp- ids) AND creates that
      // already reconciled to their real id while this (older) GET was in
      // flight. At book entry the local list starts empty, so every such
      // missing item is necessarily a just-created one, never a stale row.
      // Dedupes by id: if the snapshot already includes a new row, the local
      // copy is dropped.
      const fetchedIds = new Set(items.map((h) => h.id));
      const pending = highlightsState.items.filter((h) => !fetchedIds.has(h.id));
      highlightsState.items = items.concat(pending);
      highlightsState.loaded = true;
      highlightsState.failed = false;
    } catch {
      if (!stale()) { highlightsState.failed = true; clearHlJump(); }
    }
  }

  // Drift fallback: exact offsets first; else nearest occurrence of the quoted
  // text (strict < keeps the EARLIER occurrence on an exact distance tie).
  // Display-only — never writes corrected offsets back.
  function resolveHighlightRange(hl, chapterText) {
    const s = hl.startOffset, e = hl.endOffset;
    if (chapterText.slice(s, e) === hl.text) return { s, e };
    if (!hl.text) return null;
    let best = -1, bestDist = Infinity;
    let idx = chapterText.indexOf(hl.text);
    while (idx !== -1) {
      const dist = Math.abs(idx - s);
      if (dist < bestDist) { best = idx; bestDist = dist; }
      idx = chapterText.indexOf(hl.text, idx + 1);
    }
    return best === -1 ? null : { s: best, e: best + hl.text.length };
  }

  // Frozen-snapshot two-pass renderer (spec §3): map everything against the
  // pristine DOM first, then split every text node at the union of highlight
  // boundaries (descending), then wrap immutable fragments. Crossing ranges can
  // never invalidate frozen offsets because no wrap happens during mapping.
  // `renderNavId` (optional): the highlight-jump navId this render was
  // invoked with — only renderReaderContent (via its captured token) and the
  // same-chapter startHlJump path pass one; rebuilds pass none.
  function applyChapterHighlights(contentEl, chapterIndex, renderNavId) {
    const items = highlightsState.items.filter((h) => h.chapterIndex === chapterIndex);
    if (items.length) {
      // 1. Snapshot: text nodes + chapter plain text (UTF-16 code-unit basis).
      const walker = document.createTreeWalker(contentEl, NodeFilter.SHOW_TEXT);
      const entries = [];
      let chapterText = "";
      let node;
      while ((node = walker.nextNode())) {
        entries.push({ node, start: chapterText.length });
        chapterText += node.nodeValue;
      }
      // 2. Resolve ranges (drift fallback), deterministic wrap order.
      const resolved = [];
      for (const hl of items) {
        const r = resolveHighlightRange(hl, chapterText);
        if (r && r.e > r.s) resolved.push({ hl, s: r.s, e: r.e });
      }
      resolved.sort((a, b) =>
        a.s - b.s || a.e - b.e || (a.hl.id < b.hl.id ? -1 : a.hl.id > b.hl.id ? 1 : 0));
      // 3. Partition pass: split each text node at every boundary inside it.
      const fragments = [];
      for (const { node: textNode, start } of entries) {
        const end = start + textNode.nodeValue.length;
        const bounds = new Set();
        for (const { s, e } of resolved) {
          if (s > start && s < end) bounds.add(s - start);
          if (e > start && e < end) bounds.add(e - start);
        }
        const local = [...bounds].sort((x, y) => y - x); // descending
        const tails = [];
        for (const off of local) tails.push(textNode.splitText(off));
        tails.reverse(); // ascending document order
        let cursor = start;
        for (const frag of [textNode, ...tails]) {
          fragments.push({ node: frag, start: cursor, end: cursor + frag.nodeValue.length });
          cursor += frag.nodeValue.length;
        }
      }
      // 4. Wrap pass: REVERSE sorted order so the highest-sorted highlight wraps
      // first and ends INNERMOST (deterministic tap target, spec §3).
      const topWrapper = new Map(); // fragment node -> outermost wrapper so far
      for (let i = resolved.length - 1; i >= 0; i--) {
        const { hl, s, e } = resolved[i];
        for (const frag of fragments) {
          if (frag.start >= s && frag.end <= e && frag.node.nodeValue.length) {
            const target = topWrapper.get(frag.node) || frag.node;
            const mark = document.createElement("mark"); // DOM APIs only — no HTML strings
            mark.className = "hl-mark";
            mark.dataset.hlId = hl.id;
            mark.style.backgroundColor = hl.color + "44";
            target.parentNode.insertBefore(mark, target);
            mark.appendChild(target);
            topWrapper.set(frag.node, mark);
          }
        }
      }
    }
    // Returns true if a pending jump was consumed with a scroll-to-mark, so a
    // caller that saves/restores scroll (rerenderChapterHighlights) can skip
    // the restore and let the jump's scroll stand.
    return maybeConsumeHlJump(contentEl, chapterIndex, renderNavId !== undefined ? renderNavId : null);
  }

  // ── Highlight jump (navId token, spec §4) ──
  // A drawer-row tap mints a single pending token; it is consumed only by a
  // render that carries its navId (or a same-chapter tap), and cancelled by
  // ANY superseding event — Prev/Next, TOC jump, bookmark jump, reader
  // close, book switch, ANY other route() navigation (history/back, nav
  // icons, direct hash edits — see the backstop in route()), or a terminal
  // highlights-GET failure.
  // `arrived` flips true once the token's OWN navigation has rendered —
  // consumption is never allowed before that, so a later unrelated render of
  // the target chapter (hash away-and-back) can't trigger a stale scroll.
  let pendingHlJump = null; // {bookId, hlId, navId, arrived}
  let hlNavCounter = 0;
  // navId of a jump navigation just dispatched. renderReaderContent captures
  // (and clears) it synchronously at its start, so it binds to exactly the
  // render the navigation produced — a superseded render takes the navId to
  // its grave instead of leaking it to the next unrelated render.
  let hlJumpNavId = null;

  // Test hook (cheap, no behavior): reflect pending state for e2e assertions.
  function syncHlJumpTestHook() {
    window.__hlJumpPendingForTest = pendingHlJump ? pendingHlJump.hlId : null;
  }

  function startHlJump(hl) {
    pendingHlJump = { bookId: highlightsState.bookId, hlId: hl.id, navId: ++hlNavCounter, arrived: false };
    syncHlJumpTestHook();
    clearVocabJump(); // a highlight jump supersedes any pending vocabulary jump
    closeHlPanel(false);
    if (hl.chapterIndex === currentChapterIndex && readerState && currentChapterIndex === readerState.index) {
      // Same chapter: marks already injected — consume immediately (this
      // call IS the token's navigation, so it carries the navId).
      maybeConsumeHlJump(document.getElementById("reader-content"), currentChapterIndex, pendingHlJump.navId);
    } else {
      // Copy the navId into the navigation so the resulting render carries it.
      hlJumpNavId = pendingHlJump.navId;
      gotoReaderIndex(hl.chapterIndex, { instant: true });
    }
  }

  function clearHlJump() {
    pendingHlJump = null;
    // Hygiene: a dispatched-but-never-rendered navId (navigation that fired
    // no hashchange) must not linger for a later render to capture.
    hlJumpNavId = null;
    syncHlJumpTestHook();
  }

  // Called at the end of applyChapterHighlights with the navId the render
  // was invoked with (null for rebuilds). Consumes ONLY when: token exists,
  // book matches, the token's own navigation has arrived (this render or an
  // earlier one), we're on the target chapter, AND highlight data is loaded
  // — a fast render before the GET resolves leaves the token alive; the
  // post-GET rerender re-enters here and consumes it.
  function maybeConsumeHlJump(contentEl, chapterIndex, renderNavId) {
    if (!pendingHlJump) return;
    if (pendingHlJump.bookId !== highlightsState.bookId) { clearHlJump(); return; }
    if (renderNavId === pendingHlJump.navId) pendingHlJump.arrived = true;
    const hl = highlightsState.items.find((h) => h.id === pendingHlJump.hlId);
    if (highlightsState.loaded && !hl) { clearHlJump(); return; } // deleted meanwhile
    if (hl && hl.chapterIndex !== chapterIndex) {
      // A render for a DIFFERENT chapter that doesn't carry our navId is a
      // superseding navigation → cancel (spec §4). The render that DOES
      // carry it is still in flight — keep waiting.
      if (renderNavId !== pendingHlJump.navId) clearHlJump();
      return;
    }
    // Target chapter (or target unknown while the GET is pending): a render
    // that is NOT the token's own navigation must never scroll — cancel.
    if (!pendingHlJump.arrived) { clearHlJump(); return; }
    if (!highlightsState.loaded) return false; // wait for the GET; token survives
    const mark = contentEl.querySelector(`mark[data-hl-id="${CSS.escape(pendingHlJump.hlId)}"]`);
    clearHlJump();
    if (mark) { mark.scrollIntoView({ block: "center" }); return true; }
    // Unanchored (drift fallback failed): chapter navigation only — done.
    return false;
  }

  // Full rebuild from clean HTML (spec §3 "never incremental") — used when
  // highlight state changes after the chapter is already painted (e.g. the
  // book-open GET resolving).
  function rerenderChapterHighlights() {
    const contentEl = document.getElementById("reader-content");
    if (!contentEl || currentChapterCleanHtml === null) return;
    // A chapter turn may be in flight (readerState.index already advanced,
    // new HTML not yet painted). Rebuilding now would flash the PREVIOUS
    // chapter over the "Loading…" placeholder — skip; the in-flight
    // renderReaderContent applies highlights itself when it lands.
    if (currentChapterIndex !== readerState.index) return;
    const stage = document.getElementById("reader-stage");
    const scrollTop = stage ? stage.scrollTop : 0; // preserve position across rebuild
    contentEl.innerHTML = currentChapterCleanHtml;
    const jumped = applyChapterHighlights(contentEl, currentChapterIndex);
    // A pending jump consumed here scrolled to its mark (e.g. the post-GET
    // rerender in the fast-render-before-GET path) — keep that scroll; only
    // restore the pre-rebuild position when no jump moved us.
    if (stage && !jumped) stage.scrollTop = scrollTop;
  }

  // ── Saved words (vocabulary drawer, M4) ──────────────────────────────────
  // Book-scoped view of `GET /api/vocabulary?bookId=`. Unlike highlights,
  // words carry no on-screen `<mark>` — a row tap jumps by resolving the
  // word's stored offsets against a fresh TreeWalker snapshot of the target
  // chapter (see maybeConsumeVocabJump below), reusing the same pending-jump
  // token pattern as highlights (startHlJump/maybeConsumeHlJump) rather than
  // a timer.

  function vocabPanelHtml() {
    // M5: "See all" is a plain hash link, not a navigate()-bound button — a
    // hash-only href changes location.hash itself, which fires the same
    // hashchange -> route() as every other navigation (route() already
    // closes this panel on any navigation, so no extra JS is needed here).
    // Reachable only through this drawer, which is itself gated on
    // vocabularyAvailable() (the trigger button), so no separate gating.
    return `
      <div class="vocab-panel" id="vocab-panel" role="dialog" aria-label="Saved words">
        <div class="vocab-head">
          <span class="vocab-title">Saved words</span>
          <a href="#/vocabulary" id="vocab-see-all" class="vocab-see-all">See all</a>
          <button id="vocab-close" class="vocab-close" aria-label="Close">&times;</button>
        </div>
        <div class="vocab-list" id="vocab-list"></div>
      </div>`;
  }

  function buildVocabTrigger() {
    const btn = $("#vocab-btn");
    if (!btn) return;
    btn.addEventListener("keydown", containReaderKeys);
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      if (vocabPanelOpen) closeVocabPanel(true);
      else openVocabPanel();
    });
  }

  // Shows/hides the toolbar button per the session-cached dictionary status
  // (vocabularyAvailable()). Called synchronously at chrome build (status may
  // already be cached from an earlier book this session) and again once
  // ensureDictionaryStatus()'s fetch resolves (it may not be yet, for the
  // first chapter-mode book opened in a fresh session).
  function applyVocabButtonVisibility() {
    const btn = $("#vocab-btn");
    if (btn) btn.hidden = !vocabularyAvailable();
  }

  // Same idea as applyVocabButtonVisibility, for the M5 "Vocabulary" entry in
  // the header nav cluster and bottom tab bar (rendered on every top-level
  // screen, not just the reader). Toggles the `hidden` attribute set by
  // navIconsHtml()/tabBarHtml() at render time, which reflects whatever
  // vocabularyAvailable() already knew synchronously — this applies the
  // result once the async status fetch resolves. `[hidden]` alone would not
  // hide either element: both `.nav-icon` and `.tab` set `display` in CSS,
  // which as an author rule beats the UA stylesheet's `[hidden]{display:none}`
  // regardless of specificity — see the `.nav-icon[hidden]`/`.tab[hidden]`
  // overrides in app.css.
  function applyVocabNavVisibility() {
    const nav = $('[data-nav="vocabulary"]');
    if (nav) nav.hidden = !vocabularyAvailable();
    const tab = $('[data-tab="vocabulary"]');
    if (tab) tab.hidden = !vocabularyAvailable();
  }

  // Shared by the Vocabulary screen's two 403 paths (initial load, and M6's
  // answerCard) — the setting was switched off since this tab cached its
  // status. Drop the cache so nothing here or elsewhere in the tab keeps
  // treating vocabulary as on, and retire the nav affordance. A single
  // function so the two call sites can't quietly drift on which fields get
  // cleared (they already had before this was extracted).
  function dropVocabularyStatusCache() {
    dictStatus = null;
    dictStatusPromise = null;
    window.__dictStatusForTest = null;
    applyVocabNavVisibility();
  }

  // Chapter label for a drawer row: TOC title when known, else "Chapter N"
  // (same resolution as hlChapterLabel/bookmarkLabel) — null when the word
  // carries no chapter at all (chapterIndex is nullable).
  function vocabChapterLabel(word) {
    if (word.chapterIndex === null || word.chapterIndex === undefined) return null;
    if (readerState && Array.isArray(readerState.toc)) {
      const entry = readerState.toc.find((e) => e.chapter_index === word.chapterIndex);
      if (entry && entry.label) return entry.label;
    }
    return `Chapter ${word.chapterIndex + 1}`;
  }

  function renderVocabList() {
    const list = $("#vocab-list");
    if (!list) return;
    // Online-only, like highlights (spec §5 precedent): a terminal GET
    // failure gets its own message, never an empty list that reads as "none
    // saved".
    if (vocabState.failed && !vocabState.loaded) {
      list.innerHTML = `<p class="vocab-empty">Saved words need a connection.</p>`;
      return;
    }
    const items = vocabState.items;
    if (!items.length) {
      list.innerHTML = `<p class="vocab-empty">No saved words in this book yet.</p>`;
      return;
    }
    list.innerHTML = items
      .map((w) => {
        const meta = [vocabChapterLabel(w), formatAddedDate(w.createdAt)].filter(Boolean).join(" · ");
        // A chapter-less row cannot jump (see startVocabJump), so it must not
        // present itself as a button — no role, no tab stop, no pointer.
        const jumpable = Number.isFinite(w.chapterIndex);
        const rowClass = jumpable ? "vocab-entry" : "vocab-entry vocab-entry-static";
        const rowAttrs = jumpable ? ` tabindex="0" role="button"` : "";
        return `
      <div class="${rowClass}" data-id="${esc(w.id)}"${rowAttrs}>
        <span class="vocab-entry-body">
          <span class="vocab-entry-word"><strong>${esc(w.word)}</strong>${w.pos ? ` <span class="vocab-entry-pos">${esc(w.pos)}</span>` : ""}</span>
          <span class="vocab-entry-def">${esc(w.definition)}</span>
          ${meta ? `<span class="vocab-entry-meta">${esc(meta)}</span>` : ""}
        </span>
        <button class="vocab-entry-delete" data-id="${esc(w.id)}" aria-label="Delete saved word">&times;</button>
      </div>`;
      })
      .join("");
  }

  function renderVocabListIfOpen() {
    if (vocabPanelOpen) renderVocabList();
  }

  function closeVocabPanel(restoreFocus) {
    const panel = $("#vocab-panel");
    const btn = $("#vocab-btn");
    if (panel) panel.classList.remove("open");
    if (btn) btn.setAttribute("aria-expanded", "false");
    vocabPanelOpen = false;
    if (restoreFocus && btn) btn.focus();
  }

  function openVocabPanel() {
    const panel = $("#vocab-panel");
    const btn = $("#vocab-btn");
    if (!panel || !btn || !readerState) return;
    // Mutual exclusion with the other bottom-chrome panels.
    if (typoPanelOpen) closeTypoPanel(false);
    if (tocPanelOpen) closeTocPanel(false);
    if (bookmarkPanelOpen) closeBookmarkPanel(false);
    if (hlPanelOpen) closeHlPanel(false);
    renderVocabList();
    panel.classList.add("open");
    btn.setAttribute("aria-expanded", "true");
    vocabPanelOpen = true;
    refreshVocabPanelData();
  }

  // Monotonic fetch token (highlights precedent): a bookId check alone can't
  // discriminate a stale fetch from a same-book reopen.
  let vocabFetchSeq = 0;

  async function loadVocabForBook(bookId) {
    const seq = ++vocabFetchSeq;
    const stale = () => !readerState || readerState.id !== bookId || seq !== vocabFetchSeq;
    try {
      const resp = await api(`/api/vocabulary?bookId=${encodeURIComponent(bookId)}`);
      if (stale()) return;
      // The setting was turned off since this tab cached its status: drop the
      // cache, retire the trigger, and say what actually happened instead of
      // blaming the network. Mirrors the Define popover's Save button.
      if (resp && resp.status === 403) {
        dictStatus = null;
        dictStatusPromise = null;
        vocabState = { bookId, items: [], loaded: false, failed: true, disabled: true };
        closeVocabPanel(false);
        applyVocabButtonVisibility();
        showToast("Vocabulary is turned off");
        return;
      }
      if (!resp || !resp.ok) { vocabState = { bookId, items: [], loaded: false, failed: true }; return; }
      const items = await resp.json();
      if (stale()) return;
      vocabState = { bookId, items: Array.isArray(items) ? items : [], loaded: true, failed: false };
    } catch {
      if (!stale()) vocabState = { bookId, items: [], loaded: false, failed: true };
    }
  }

  // Always re-fetches on open (no "already loaded" short-circuit, unlike
  // loadBookmarks): a word saved from the Define popover elsewhere while the
  // drawer sat unopened must show up the next time it opens.
  async function refreshVocabPanelData() {
    if (!readerState || readerState.mode === "page") return;
    const bookId = readerState.id;
    await loadVocabForBook(bookId);
    if (!readerState || readerState.id !== bookId) return;
    renderVocabListIfOpen();
  }

  async function deleteVocabWord(id) {
    // Retire any in-flight drawer GET. Every open starts one, so a row
    // deleted while it is outstanding would otherwise be resurrected: that
    // response replaces vocabState wholesale, and it may have read the table
    // before this DELETE committed.
    vocabFetchSeq++;
    const stateAtCall = vocabState;
    const i = vocabState.items.findIndex((w) => w.id === id);
    const removed = i !== -1 ? vocabState.items.splice(i, 1)[0] : null;
    try {
      const resp = await apiWrite(`/api/vocabulary/${encodeURIComponent(id)}`, {
        method: "DELETE",
        credentials: "same-origin",
      });
      if (resp.status === 401) {
        if (removed && vocabState === stateAtCall) {
          vocabState.items.splice(i, 0, removed);
          renderVocabListIfOpen();
        }
        showLogin();
        return;
      }
      // Idempotent DELETE: 204 is success whether or not the row still existed.
      if (!resp.ok && resp.status !== 204) throw new Error(`HTTP ${resp.status}`);
    } catch {
      // A moved profile is not a failed delete: the page is being replaced,
      // and the re-read below would pull the *other* library's words into
      // this list on the way out.
      if (reloadPending) return;
      if (removed && vocabState === stateAtCall) {
        vocabState.items.splice(i, 0, removed);
        renderVocabListIfOpen();
      }
      showToast("Couldn't delete saved word");
      // The token bump above retired whatever refresh was in flight, and it
      // may have been carrying words saved elsewhere since this list was
      // built. Reinserting the row locally is not enough — re-read the
      // server's list so a failed delete leaves the drawer complete, not
      // merely un-deleted.
      refreshVocabPanelData();
    }
  }

  function wireVocabControls() {
    const closeBtn = $("#vocab-close");
    if (closeBtn) closeBtn.addEventListener("click", () => closeVocabPanel(true));
    const list = $("#vocab-list");
    if (list)
      list.addEventListener("click", (e) => {
        const del = e.target.closest(".vocab-entry-delete");
        if (del) {
          e.stopPropagation();
          deleteVocabWord(del.getAttribute("data-id"));
          renderVocabList(); // optimistic removal is synchronous
          return;
        }
        const entry = e.target.closest(".vocab-entry");
        if (!entry) return;
        const word = vocabState.items.find((w) => w.id === entry.getAttribute("data-id"));
        if (!word) return;
        startVocabJump(word);
      });
    const panel = $("#vocab-panel");
    if (panel) panel.addEventListener("keydown", containReaderKeys);
    wirePanelRootDismissal({
      isOpen: () => vocabPanelOpen,
      close: closeVocabPanel,
      panelSel: "#vocab-panel",
      btnSel: "#vocab-btn",
    });
  }

  // ── Vocabulary jump (offset-based — no navId machinery on a `<mark>`;
  // pending-jump token pattern only, spec/plan M4) ──
  let pendingVocabJump = null; // {bookId, word, targetChapterIndex, navId, arrived}
  let vocabNavCounter = 0;
  // navId of a jump navigation just dispatched; captured (and cleared) by
  // renderReaderContent synchronously at its start — same lifecycle as
  // hlJumpNavId above.
  let vocabJumpNavId = null;

  function startVocabJump(word) {
    if (!readerState || readerState.mode === "page") return;
    // `chapter_index` is nullable, and such a row renders with no chapter
    // label (vocabChapterLabel returns null). There is nowhere to jump to —
    // treating a missing chapter as 0 would navigate away from what the
    // reader is on to a position the word was never looked up at.
    if (!Number.isFinite(word.chapterIndex)) return;
    const targetChapterIndex = Math.min(Math.max(word.chapterIndex, 0), readerState.count - 1);
    pendingVocabJump = {
      bookId: vocabState.bookId,
      word,
      targetChapterIndex,
      navId: ++vocabNavCounter,
      arrived: false,
    };
    clearHlJump(); // a vocabulary jump supersedes any pending highlight jump
    closeVocabPanel(false);
    if (targetChapterIndex === currentChapterIndex && currentChapterIndex === readerState.index) {
      // Same chapter: this call IS the token's navigation, so it carries the navId.
      maybeConsumeVocabJump(document.getElementById("reader-content"), currentChapterIndex, pendingVocabJump.navId);
    } else {
      vocabJumpNavId = pendingVocabJump.navId;
      gotoReaderIndex(targetChapterIndex, { instant: true });
    }
  }

  function clearVocabJump() {
    pendingVocabJump = null;
    vocabJumpNavId = null;
  }

  // Snapshot #reader-content's plain text via a fresh TreeWalker (same
  // UTF-16 code-unit basis as applyChapterHighlights) and locate the word's
  // range: exact offsets first, else the nearest occurrence of the word text
  // (resolveHighlightRange's drift-fallback logic, generalized to a single
  // word/startOffset rather than a highlight's [start,end)).
  const escapeRegExp = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

  function resolveVocabWordRange(word, chapterText) {
    const w = word.word || "";
    if (!w) return null;
    const s = typeof word.startOffset === "number" ? word.startOffset : null;
    if (s !== null) {
      const e = typeof word.endOffset === "number" && word.endOffset > s ? word.endOffset : s + w.length;
      if (chapterText.slice(s, e) === w) return { s, e };
    }
    const anchor = s !== null ? s : 0;
    let best = -1, bestDist = Infinity, bestLen = w.length;
    let idx = chapterText.indexOf(w);
    while (idx !== -1) {
      const dist = Math.abs(idx - anchor);
      if (dist < bestDist) { best = idx; bestDist = dist; }
      idx = chapterText.indexOf(w, idx + 1);
    }
    // A saved multi-word term stores its selection whitespace-collapsed
    // (defineSelection), while chapterText carries the markup's real runs of
    // whitespace — so a term spanning a line break never matches literally.
    // Retry allowing any whitespace run between its words.
    if (best === -1 && /\s/.test(w)) {
      const pattern = w.split(/\s+/).map(escapeRegExp).join("\\s+");
      const re = new RegExp(pattern, "g");
      let m;
      while ((m = re.exec(chapterText)) !== null) {
        const dist = Math.abs(m.index - anchor);
        if (dist < bestDist) { best = m.index; bestDist = dist; bestLen = m[0].length; }
        re.lastIndex = m.index + 1; // overlapping candidates stay reachable
      }
    }
    return best === -1 ? null : { s: best, e: best + bestLen };
  }

  // Map a [s,e) plain-text offset pair back onto live text nodes and build a
  // Range over them — no DOM mutation (unlike applyChapterHighlights's
  // wrap pass), since a vocabulary jump leaves no visible mark.
  function rangeFromChapterOffsets(contentEl, s, e) {
    const walker = document.createTreeWalker(contentEl, NodeFilter.SHOW_TEXT);
    let node, pos = 0;
    let startNode = null, startOffset = 0, endNode = null, endOffset = 0;
    while ((node = walker.nextNode())) {
      const len = node.nodeValue.length;
      if (startNode === null && s >= pos && s <= pos + len) { startNode = node; startOffset = s - pos; }
      if (endNode === null && e >= pos && e <= pos + len) { endNode = node; endOffset = e - pos; }
      pos += len;
      if (startNode && endNode) break;
    }
    if (!startNode || !endNode) return null;
    const range = document.createRange();
    range.setStart(startNode, startOffset);
    range.setEnd(endNode, endOffset);
    return range;
  }

  // Centers a Range in #reader-stage. Range has no reliably cross-browser
  // scrollIntoView, unlike Element — so this computes the same "block:
  // center" position directly against the actual scroll container via
  // getBoundingClientRect() math (Range.getBoundingClientRect() IS standard).
  // Returns true if it actually moved the stage — renderReaderContent needs
  // that answer to suppress its own post-render scroll restore (see there).
  function scrollRangeIntoView(range) {
    const stage = document.getElementById("reader-stage");
    if (!stage) return false;
    const rect = range.getBoundingClientRect();
    if (!rect || (rect.width === 0 && rect.height === 0)) return false;
    const stageRect = stage.getBoundingClientRect();
    const target = stage.scrollTop + (rect.top - stageRect.top) - stage.clientHeight / 2 + rect.height / 2;
    stage.scrollTop = Math.max(0, target);
    return true;
  }

  // Called from renderReaderContent (with the render's navId, or null for an
  // unrelated render) and directly from startVocabJump for the same-chapter
  // case. Consumes only when: token exists, book matches, the token's own
  // navigation has arrived, and we are on the target chapter — same
  // supersession rules as maybeConsumeHlJump, minus the "wait for the GET"
  // gate (the word is already in hand from the drawer row, no fetch needed).
  // Returns true when it scrolled (same contract as applyChapterHighlights).
  function maybeConsumeVocabJump(contentEl, chapterIndex, renderNavId) {
    if (!pendingVocabJump) return false;
    if (pendingVocabJump.bookId !== vocabState.bookId) { clearVocabJump(); return false; }
    if (renderNavId === pendingVocabJump.navId) pendingVocabJump.arrived = true;
    if (pendingVocabJump.targetChapterIndex !== chapterIndex) {
      if (renderNavId !== pendingVocabJump.navId) clearVocabJump();
      return false;
    }
    if (!pendingVocabJump.arrived) { clearVocabJump(); return false; }
    const { word } = pendingVocabJump;
    clearVocabJump();
    const walker = document.createTreeWalker(contentEl, NodeFilter.SHOW_TEXT);
    let chapterText = "";
    let n;
    while ((n = walker.nextNode())) chapterText += n.nodeValue;
    const resolved = resolveVocabWordRange(word, chapterText);
    if (!resolved) return false; // drift fallback failed: chapter navigation only, no error
    const range = rangeFromChapterOffsets(contentEl, resolved.s, resolved.e);
    return range ? scrollRangeIntoView(range) : false;
  }

  // ── Selection capture → create popover (spec §2) ──
  let hlPopoverEl = null;
  let hlEditPopoverEl = null;
  let hlCaptured = null; // {text, startOffset, endOffset, rect} frozen at open
  let hlPopoverPointerDown = false;
  let hlSelDebounce = 0;
  let hlPopoverScrollTop = null; // stage.scrollTop at popover open

  // ── Define (dictionary lookup, M2) ──────────────────────────────────────
  // Session-cached status: null until the first fetch resolves, then
  // {installed, enabled, vocabulary}. Fetched once per page load (not per
  // book), memoized via dictStatusPromise — every top-level screen that
  // shows the Vocabulary nav icon (library/detail/reader/stats/collections/
  // vocabulary, M5) kicks it on render, and renderReaderChrome's chapter-mode
  // branch kicks it too for the vocab-btn toolbar button; whichever call
  // lands first does the actual fetch. A 503 from the lookup route
  // (disabled/uninstalled mid-session) downgrades this cache so the popover
  // stops offering Define.
  let dictStatus = null;
  let dictStatusPromise = null;
  let dictUnavailableShown = false; // "Dictionary unavailable" toast: once per session
  let dictPopoverEl = null;

  async function ensureDictionaryStatus() {
    if (dictStatus) return dictStatus;
    if (!dictStatusPromise) {
      dictStatusPromise = (async () => {
        try {
          const resp = await api("/api/dictionary/status");
          if (!resp || !resp.ok) return null; // failure — see recovery below
          return await resp.json();
        } catch {
          return null;
        }
      })();
    }
    const result = await dictStatusPromise;
    // Status-cache recovery: only a successful HTTP response gets cached. A
    // failed/exception fetch (transient error, 401, mid-outage 503) must not
    // freeze the Define button hidden for the rest of the session — clear
    // both so the NEXT ensureDictionaryStatus() call (fired per book entry,
    // see renderReaderChrome) retries instead of replaying the failure.
    if (result === null) {
      dictStatusPromise = null;
      dictStatus = null;
      window.__dictStatusForTest = null;
      return null;
    }
    dictStatus = result;
    // Recovery restores future toasts: a status that comes back healthy
    // means a prior "Dictionary unavailable" is no longer the current state.
    if (dictStatus.installed && dictStatus.enabled) dictUnavailableShown = false;
    // Test hook: lets e2e wait for the async status fetch to land before
    // exercising selection, mirroring __hlJumpPendingForTest below.
    window.__dictStatusForTest = dictStatus;
    return dictStatus;
  }

  function dictionaryAvailable() {
    return !!(dictStatus && dictStatus.installed && dictStatus.enabled);
  }

  // M3: whether the per-profile `vocabulary_enabled` setting is on, per the
  // same session-cached status fetch as dictionaryAvailable() above. Gates
  // the Define popover's "Save" button — POST /api/vocabulary re-checks this
  // server-side too (defense in depth), so this is purely to avoid offering
  // a button that would silently no-op.
  function vocabularyAvailable() {
    return !!(dictStatus && dictStatus.vocabulary);
  }

  // Scroll events are dispatched asynchronously: a programmatic
  // scroll-into-view that lands JUST BEFORE the tap that opens a popover
  // delivers its scroll event AFTER the popover opens — dismissing it
  // instantly. Only a scroll that MOVES the stage after open dismisses.
  function recordHlPopoverScrollTop() {
    const stage = document.getElementById("reader-stage");
    hlPopoverScrollTop = stage ? stage.scrollTop : null;
  }

  function dismissHlPopoversOnScroll(scrollTop) {
    // Define opens from a tap, not a scroll, so it has no "just-opened"
    // scroll position to anchor a guard against (unlike hlPopoverScrollTop
    // below) — dismiss it on any scroll at all.
    if (dictPopoverEl) closeDefinePopover();
    if (!hlPopoverEl && !hlEditPopoverEl) return;
    if (hlPopoverScrollTop !== null && scrollTop === hlPopoverScrollTop) return;
    closeHlPopover();
    closeHlEditPopover();
  }

  // Offsets: UTF-16 code units into #reader-content's rendered textContent —
  // same computation as desktop ReaderPane (preRange trick), with the leading-
  // whitespace trim folded into startOffset.
  function computeSelectionOffsets(contentEl) {
    const sel = window.getSelection();
    if (!sel || sel.rangeCount === 0 || sel.isCollapsed) return null;
    const range = sel.getRangeAt(0);
    if (!contentEl.contains(range.startContainer) || !contentEl.contains(range.endContainer)) {
      return null;
    }
    const raw = range.toString();
    const text = raw.trim();
    if (text.length < 3) return null; // desktop parity
    const pre = document.createRange();
    pre.selectNodeContents(contentEl);
    pre.setEnd(range.startContainer, range.startOffset);
    const lead = raw.length - raw.replace(/^\s+/, "").length;
    const startOffset = pre.toString().length + lead;
    return {
      text,
      startOffset,
      endOffset: startOffset + text.length,
      rect: range.getBoundingClientRect(),
    };
  }

  function bindHighlightSelection() {
    document.addEventListener("selectionchange", () => {
      // Suppress collapse-dismissal while a popover-originated pointer is
      // down: on touch, tapping a swatch collapses the selection BEFORE
      // click fires (spec §2 collapse-vs-tap guard).
      if (hlPopoverPointerDown) return;
      clearTimeout(hlSelDebounce);
      hlSelDebounce = setTimeout(() => {
        // Chapter-mode gate: #reader-content only exists in chapter mode.
        const contentEl = document.getElementById("reader-content");
        if (!contentEl || !navigator.onLine) { closeHlPopover(); closeDefinePopover(); return; } // online-only
        const captured = computeSelectionOffsets(contentEl);
        if (captured) openHlPopover(captured);
        else { closeHlPopover(); closeDefinePopover(); }
      }, 250);
    });
    // Dismiss on outside tap — all three popovers. Pointer interactions that
    // START inside a popover must never dismiss it (contains() check).
    document.addEventListener("pointerdown", (e) => {
      if (hlPopoverEl && !hlPopoverEl.contains(e.target)) closeHlPopover();
      if (hlEditPopoverEl && !hlEditPopoverEl.contains(e.target)) closeHlEditPopover();
      if (dictPopoverEl && !dictPopoverEl.contains(e.target)) closeDefinePopover();
    });
  }
  bindHighlightSelection();

  // Shared clamp math for popovers anchored below a selection/tap rect: 8px
  // margins from the viewport edges on both axes. `opts.center` reproduces
  // openHlPopover's horizontally-centered variant; the default matches
  // openHlEditPopover's and openDefinePopover's left-aligned variant.
  function positionPopoverBelow(pop, rect, opts) {
    const top = Math.min(rect.bottom + 8, window.innerHeight - pop.offsetHeight - 8);
    const left = opts && opts.center
      ? Math.max(8, Math.min(rect.left + rect.width / 2 - pop.offsetWidth / 2,
          window.innerWidth - pop.offsetWidth - 8))
      : Math.max(8, Math.min(rect.left, window.innerWidth - pop.offsetWidth - 8));
    pop.style.top = `${top}px`;
    pop.style.left = `${left}px`;
  }

  function openHlPopover(captured) {
    closeHlPopover();
    closeHlEditPopover();
    // A new selection supersedes any Define result still on screen from a
    // previous one — it would otherwise float, stale and misplaced.
    closeDefinePopover();
    hlCaptured = { text: captured.text, startOffset: captured.startOffset,
      endOffset: captured.endOffset, rect: captured.rect };
    const pop = document.createElement("div");
    pop.id = "hl-popover";
    pop.setAttribute("role", "toolbar");
    pop.setAttribute("aria-label", "Highlight");
    for (const color of HIGHLIGHT_COLORS) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "hl-swatch";
      btn.dataset.color = color;
      btn.style.backgroundColor = color;
      btn.setAttribute("aria-label", `Highlight ${color}`);
      btn.addEventListener("click", () => {
        createHighlightFromCapture(hlCaptured, color);
      });
      pop.appendChild(btn);
    }
    // Create-with-note (spec §2): create with the default color, then open
    // the drawer's inline note editor on the saved highlight.
    const createNoteBtn = document.createElement("button");
    createNoteBtn.type = "button";
    createNoteBtn.id = "hl-create-note-btn";
    createNoteBtn.setAttribute("aria-label", "Highlight with note");
    createNoteBtn.textContent = "✎";
    createNoteBtn.addEventListener("click", async () => {
      const captured = hlCaptured; // frozen before closeHlPopover clears it
      await createHighlightFromCapture(captured, HIGHLIGHT_COLORS[0]); // default yellow
      // The saved highlight is the NEWEST (last) item matching the captured
      // range; tmp- means the POST failed to reconcile — nothing to edit.
      let hl = null;
      for (const h of highlightsState.items) {
        if (h.chapterIndex === currentChapterIndex &&
            h.startOffset === captured.startOffset && h.endOffset === captured.endOffset &&
            !String(h.id).startsWith("tmp-")) hl = h;
      }
      if (hl) {
        openHlPanel();
        const row = rowFor(hl.id);
        if (row) openHlNoteEditor(row, hl);
      }
    });
    pop.appendChild(createNoteBtn);
    // Overlap-clear: when the captured range overlaps existing highlights,
    // offer a trash button that deletes all overlapped (desktop parity).
    const overlapped = highlightsState.items.filter((h) =>
      !String(h.id).startsWith("tmp-") && // never mutate un-reconciled optimistic creates
      h.chapterIndex === currentChapterIndex &&
      h.startOffset < captured.endOffset && captured.startOffset < h.endOffset);
    if (overlapped.length) {
      const clearBtn = document.createElement("button");
      clearBtn.type = "button";
      clearBtn.id = "hl-clear-btn";
      clearBtn.setAttribute("aria-label", "Remove overlapping highlights");
      clearBtn.textContent = "\u{1F5D1}";
      clearBtn.addEventListener("click", () => {
        for (const h of overlapped) deleteHighlight(h.id);
        closeHlPopover();
        window.getSelection()?.removeAllRanges();
      });
      pop.appendChild(clearBtn);
    }
    // Define (M2): only offered once the session status fetch confirms the
    // dictionary is installed + enabled — zero UI trace otherwise.
    if (dictionaryAvailable()) {
      const defineBtn = document.createElement("button");
      defineBtn.type = "button";
      defineBtn.id = "hl-define-btn";
      defineBtn.setAttribute("aria-label", "Define");
      defineBtn.textContent = "\u{1F4D6}"; // 📖
      defineBtn.addEventListener("click", () => {
        const captured = hlCaptured; // frozen before closeHlPopover clears it
        const rect = captured.rect;
        closeHlPopover();
        defineSelection(captured, rect);
      });
      pop.appendChild(defineBtn);
    }
    // Captured-range guard: pointer interactions inside the popover must not
    // dismiss it before the click handler runs (spec §2). The clearing
    // listeners are registered INSIDE pointerdown (paired per press) — an
    // open-time once-listener could be consumed by the still-down selection
    // finger lifting, leaving the flag stuck true and selection handling
    // silently dead.
    pop.addEventListener("pointerdown", (e) => {
      hlPopoverPointerDown = true;
      e.preventDefault(); // keeps the selection alive on touch
      const clear = () => {
        hlPopoverPointerDown = false;
        document.removeEventListener("pointerup", clear);
        document.removeEventListener("pointercancel", clear);
      };
      document.addEventListener("pointerup", clear);
      document.addEventListener("pointercancel", clear);
    });
    document.body.appendChild(pop);
    hlPopoverEl = pop;
    recordHlPopoverScrollTop();
    // Position: below the selection (clear of the iOS callout), edge-clamped,
    // centered under the selection rect.
    positionPopoverBelow(pop, captured.rect, { center: true });
  }

  function closeHlPopover() {
    if (hlPopoverEl) { hlPopoverEl.remove(); hlPopoverEl = null; }
    hlCaptured = null;
  }

  // ── Define result popover (M2) ──
  const DICT_MAX_SENSES = 6;
  // In-flight race guard: bumped at the start of every defineSelection call
  // (and here) so a response for a superseded lookup can recognize itself as
  // stale and discard silently — no popover, no toast.
  let defineSeq = 0;

  function closeDefinePopover() {
    if (dictPopoverEl) { dictPopoverEl.remove(); dictPopoverEl = null; }
    defineSeq++; // invalidate any in-flight lookup
  }

  // A lookup started under `seq` is stale once either a newer defineSelection
  // call has begun, or the user has moved on to a new selection (the hl
  // popover reopened) — checked fresh at each point a response might render.
  function defineIsStale(seq) {
    return seq !== defineSeq || hlPopoverEl !== null;
  }

  function renderDefineBody(opts) {
    if (opts.notFound) {
      return `<div class="dict-empty">No definition found for '${esc(opts.word)}'</div>`;
    }
    const entry = opts.entry;
    const senses = entry.senses.slice(0, DICT_MAX_SENSES);
    const more = entry.senses.length - senses.length;
    const items = senses
      .map((s) => `<li><span class="dict-pos">${esc(s.pos)}</span> — ${esc(s.gloss)}</li>`)
      .join("");
    const moreLine = more > 0 ? `<div class="dict-more">+${more} more</div>` : "";
    return `<div class="dict-word">${esc(entry.matchedWord)}</div><ul class="dict-senses">${items}</ul>${moreLine}`;
  }

  // M3: "Save to vocabulary" button for the Define result popover (success
  // path only — never on the not-found card). Builds and POSTs the same
  // shape desktop's auto-log sends (word/lemma/pos/definition/book context),
  // using the entry's first sense per spec (simplest predictable rule,
  // mirroring defineSelection's own first-word-of-a-long-selection choice)
  // rather than desktop's part-of-speech grouping.
  // Desktop expands the selection to its surrounding sentence and caps it at
  // MAX_CONTEXT_CHARS (src/lib/vocabulary.ts); the web sends the raw
  // selection, so it must respect the same cap — the route rejects longer.
  const VOCAB_CONTEXT_MAX_CHARS = 300;

  function buildVocabularySaveButton(term, entry, captured) {
    if (!vocabularyAvailable() || !entry || !entry.senses || entry.senses.length === 0) return null;
    const primary = entry.senses[0];
    // An empty gloss would make every save 400 ("definition is empty") with
    // no path to success — don't offer the button at all.
    if (!primary || !primary.gloss) return null;
    const btn = document.createElement("button");
    btn.type = "button";
    btn.id = "dict-save-btn";
    btn.className = "dict-save-btn";
    btn.setAttribute("aria-label", "Save to vocabulary");
    btn.textContent = "Save";
    btn.addEventListener("click", async () => {
      if (btn.disabled) return;
      btn.disabled = true; // guard double-taps for the duration of the request
      try {
        const resp = await apiWrite("/api/vocabulary", {
          method: "POST",
          credentials: "same-origin",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            word: term,
            lemma: entry.matchedWord,
            pos: primary.pos || null,
            definition: primary.gloss || "",
            bookId: readerState ? readerState.id : null,
            bookTitle: readerState && readerState.book ? readerState.book.title : null,
            chapterIndex: currentChapterIndex,
            contextSentence: captured ? captured.text.slice(0, VOCAB_CONTEXT_MAX_CHARS) : null,
            startOffset: captured ? captured.startOffset : null,
            endOffset: captured ? captured.endOffset : null,
          }),
        });
        if (resp.status === 401) {
          // #dict-popover is body-attached, so it would float over the PIN
          // screen (with this button stuck disabled) unless closed here.
          closeDefinePopover();
          showLogin();
          return;
        }
        if (resp.status === 403) {
          // The setting was turned off since this tab cached its status.
          // Drop the cache so later popovers stop offering Save, and say so
          // instead of claiming a save that did not happen.
          dictStatus = null;
          dictStatusPromise = null;
          btn.textContent = "Unavailable";
          showToast("Vocabulary is turned off");
          return;
        }
        if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
        btn.textContent = "Saved ✓";
      } catch {
        btn.disabled = false;
        showToast("Could not save");
      }
    });
    return btn;
  }

  function openDefinePopover(rect, opts) {
    closeDefinePopover();
    const pop = document.createElement("div");
    pop.id = "dict-popover";
    pop.setAttribute("role", "dialog");
    pop.setAttribute("aria-label", `Definition of "${opts.entry ? opts.entry.matchedWord : opts.word}"`);
    pop.innerHTML = renderDefineBody(opts);
    if (!opts.notFound) {
      const saveBtn = buildVocabularySaveButton(opts.term, opts.entry, opts.captured);
      if (saveBtn) pop.appendChild(saveBtn);
    }
    document.body.appendChild(pop);
    dictPopoverEl = pop;
    if (rect) positionPopoverBelow(pop, rect);
  }

  // Longer selections still show the Define button but define only the
  // first word — simplest predictable rule (spec M2 §2) rather than
  // guessing which word in a long selection the user meant.
  async function defineSelection(captured, rect) {
    if (!captured) return;
    const mySeq = ++defineSeq;
    const collapsed = captured.text.trim().replace(/\s+/g, " ");
    const words = collapsed.split(" ").filter(Boolean);
    let term = words.length <= 3 && collapsed.length <= 100 ? collapsed : words[0];
    // Punctuation/possessive normalization — mirrors desktop's
    // extractLookupWord (src/lib/dictionary.ts): strip surrounding
    // non-letter characters, then a trailing 's/'s possessive.
    term = (term || "").replace(/^[^\p{L}]+/u, "").replace(/[^\p{L}]+$/u, "");
    term = term.replace(/['’]s$/iu, "").replace(/['’]$/u, "");
    if (!term) return;
    // Overlong single word: the server always 400s past 100 chars (see
    // DICTIONARY_WORD_MAX_LEN in web_server/api.rs) — render the not-found
    // card locally rather than spend the once-per-session toast on a
    // guaranteed failure.
    if (term.length > 100) {
      openDefinePopover(rect, { notFound: true, word: `${term.slice(0, 40)}…` });
      return;
    }
    try {
      const resp = await api(`/api/dictionary/lookup?word=${encodeURIComponent(term)}`);
      if (!resp) return; // 401 — api() already showed the login screen
      if (resp.status === 404) {
        if (!defineIsStale(mySeq)) openDefinePopover(rect, { notFound: true, word: term });
        return;
      }
      if (resp.status === 503) {
        // Dictionary went away (disabled/uninstalled) mid-session — clear the
        // cached status so the button stops appearing AND a future
        // ensureDictionaryStatus() call re-fetches instead of replaying this
        // failure forever (see the recovery comment there).
        dictStatus = null;
        dictStatusPromise = null;
        if (!defineIsStale(mySeq) && !dictUnavailableShown) {
          dictUnavailableShown = true;
          showToast("Dictionary unavailable");
        }
        return;
      }
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const entry = await resp.json();
      if (!defineIsStale(mySeq)) openDefinePopover(rect, { entry, term, captured });
    } catch {
      if (!defineIsStale(mySeq) && !dictUnavailableShown) {
        dictUnavailableShown = true;
        showToast("Dictionary unavailable");
      }
    }
  }

  async function createHighlightFromCapture(captured, color) {
    if (!captured) return;
    const bookId = highlightsState.bookId;
    const chapterIndex = currentChapterIndex;
    closeHlPopover();
    window.getSelection()?.removeAllRanges();
    // Optimistic: temp id, full rebuild (never incremental — spec §3).
    const temp = { id: `tmp-${Date.now()}`, bookId, chapterIndex,
      text: captured.text, color, note: null,
      startOffset: captured.startOffset, endOffset: captured.endOffset };
    highlightsState.items.push(temp);
    rerenderChapterHighlights();
    try {
      const resp = await apiWrite(`/api/books/${encodeURIComponent(bookId)}/highlights`, {
        method: "POST",
        credentials: "same-origin",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ chapterIndex, text: captured.text, color,
          startOffset: captured.startOffset, endOffset: captured.endOffset }),
      });
      if (resp.status === 401) { showLogin(); return; }
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const saved = await resp.json();
      if (highlightsState.bookId !== bookId) return; // book switched mid-flight
      const i = highlightsState.items.indexOf(temp);
      if (i !== -1) highlightsState.items[i] = saved;
      rerenderChapterHighlights();
    } catch {
      const i = highlightsState.items.indexOf(temp);
      if (i !== -1) highlightsState.items.splice(i, 1);
      rerenderChapterHighlights();
      showToast("Couldn't save highlight");
    }
  }

  // ── Mark-tap edit popover (recolor / delete / note stub — spec §4) ──
  // Event delegation on the reader stage survives every innerHTML rebuild.
  function bindHighlightMarkTaps() {
    const stage = document.getElementById("reader-stage");
    if (!stage) return;
    stage.addEventListener("click", (e) => {
      const mark = e.target.closest("mark.hl-mark"); // closest() = innermost (spec §3)
      if (!mark) return;
      e.stopPropagation();
      openHlEditPopover(mark.dataset.hlId, mark.getBoundingClientRect());
    });
  }

  function openHlEditPopover(hlId, rect) {
    closeHlPopover();
    closeHlEditPopover();
    // Tapping an existing highlight mark supersedes any stray Define result
    // still on screen from an earlier selection.
    closeDefinePopover();
    const hl = highlightsState.items.find((h) => h.id === hlId);
    // tmp- = optimistic create not yet reconciled with its server id: a PUT
    // 404s and a DELETE resurrects as a ghost on next load. Untouchable until
    // the POST settles (a round-trip); tapping again then works.
    if (!hl || String(hl.id).startsWith("tmp-")) return;
    const pop = document.createElement("div");
    pop.id = "hl-edit-popover";
    pop.setAttribute("role", "toolbar");
    pop.setAttribute("aria-label", "Edit highlight");
    for (const color of HIGHLIGHT_COLORS) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "hl-swatch";
      btn.dataset.color = color;
      btn.style.backgroundColor = color;
      btn.setAttribute("aria-label", `Recolor ${color}`);
      btn.addEventListener("click", () => updateHighlight(hlId, { color }));
      pop.appendChild(btn);
    }
    const noteBtn = document.createElement("button");
    noteBtn.type = "button";
    noteBtn.id = "hl-note-btn";
    noteBtn.setAttribute("aria-label", "Edit note");
    noteBtn.textContent = "✎";
    noteBtn.addEventListener("click", () => {
      // Note editing lives in the drawer: open it on this highlight's row.
      closeHlEditPopover();
      openHlPanel();
      const row = rowFor(hlId);
      if (row) openHlNoteEditor(row, hl);
    });
    pop.appendChild(noteBtn);
    const delBtn = document.createElement("button");
    delBtn.type = "button";
    delBtn.id = "hl-delete-btn";
    delBtn.setAttribute("aria-label", "Delete highlight");
    delBtn.textContent = "\u{1F5D1}";
    delBtn.addEventListener("click", () => { deleteHighlight(hlId); closeHlEditPopover(); });
    pop.appendChild(delBtn);
    document.body.appendChild(pop);
    hlEditPopoverEl = pop;
    recordHlPopoverScrollTop();
    positionPopoverBelow(pop, rect);
  }

  function closeHlEditPopover() {
    if (hlEditPopoverEl) { hlEditPopoverEl.remove(); hlEditPopoverEl = null; }
  }

  async function deleteHighlight(hlId) {
    const bookId = highlightsState.bookId;
    // Identity token: renderReaderChrome/loadHighlightsForBook replace the
    // whole state object on every book entry, so a failed delete resolving
    // after a book switch must NOT splice its rollback into the new book's
    // list — roll back only into the same state object it was removed from.
    const stateAtCall = highlightsState;
    const i = highlightsState.items.findIndex((h) => h.id === hlId);
    const removed = i !== -1 ? highlightsState.items.splice(i, 1)[0] : null;
    rerenderChapterHighlights();
    try {
      const resp = await apiWrite(
        `/api/books/${encodeURIComponent(bookId)}/highlights/${encodeURIComponent(hlId)}`,
        { method: "DELETE", credentials: "same-origin" });
      if (resp.status === 401) {
        // Session expired: the delete did NOT happen — restore the
        // optimistically-removed row (same identity guard as the failure
        // path) before routing to login.
        if (removed && highlightsState === stateAtCall) {
          highlightsState.items.splice(i, 0, removed);
          rerenderChapterHighlights();
          renderHlListIfOpen();
        }
        showLogin();
        return;
      }
      // Idempotent DELETE: 204 is success whether or not the row still existed.
      if (!resp.ok && resp.status !== 204) throw new Error(`HTTP ${resp.status}`);
    } catch {
      if (removed && highlightsState === stateAtCall) {
        highlightsState.items.splice(i, 0, removed);
        rerenderChapterHighlights();
        renderHlListIfOpen(); // restore the drawer row too
      }
      showToast("Couldn't delete highlight");
    }
  }

  // Per-highlight update serialization: rapid recolor taps must be
  // last-ACTION-wins on the server too, not just locally — two in-flight
  // PUTs on separate pooled connections can arrive reordered, letting an
  // older request commit last. Chain requests per highlight so the wire
  // order matches the tap order; the seq map additionally discards any
  // superseded response before it writes local state.
  const hlUpdateSeq = new Map();
  const hlUpdateChain = new Map();

  function updateHighlight(hlId, patch) {
    // Capture the book identity NOW, not at dequeue time: a book switch
    // while this update sits queued behind an in-flight one must not send
    // the PUT under the NEW book's id (the scoped route would 404 — edit
    // silently lost plus a wrong-book "no longer exists" toast).
    const bookId = highlightsState.bookId;
    const stateAtCall = highlightsState;
    const prev = hlUpdateChain.get(hlId) || Promise.resolve();
    const next = prev.then(() => doUpdateHighlight(hlId, patch, bookId, stateAtCall));
    // The chain must survive a failed link or every later update stalls.
    hlUpdateChain.set(hlId, next.catch(() => {}));
    return next;
  }

  async function doUpdateHighlight(hlId, patch, bookId, stateAtCall) {
    // Dequeued after a book switch (state object replaced): nothing to
    // update here — skip silently, no request, no toast.
    if (highlightsState !== stateAtCall) return null;
    const seq = (hlUpdateSeq.get(hlId) || 0) + 1;
    hlUpdateSeq.set(hlId, seq);
    try {
      const resp = await apiWrite(
        `/api/books/${encodeURIComponent(bookId)}/highlights/${encodeURIComponent(hlId)}`,
        { method: "PUT", credentials: "same-origin",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(patch) });
      if (resp.status === 401) { showLogin(); return null; }
      // A book switch during the request: the response is about another
      // book's highlight — never mutate the new book's state or toast.
      if (highlightsState !== stateAtCall) return null;
      if (resp.status === 404) { // deleted elsewhere (e.g. desktop): drop locally
        const i = highlightsState.items.findIndex((h) => h.id === hlId);
        if (i !== -1) highlightsState.items.splice(i, 1);
        rerenderChapterHighlights();
        renderHlListIfOpen();
        showToast("Highlight no longer exists");
        return null;
      }
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const saved = await resp.json();
      // Superseded by a later update to the same highlight — discard.
      if (hlUpdateSeq.get(hlId) !== seq) return null;
      const i = highlightsState.items.findIndex((h) => h.id === hlId);
      if (i !== -1) highlightsState.items[i] = saved;
      rerenderChapterHighlights();
      renderHlListIfOpen();
      return saved;
    } catch {
      showToast("Couldn't update highlight");
      return null;
    }
  }

  // ── Highlights drawer (spec §4 — cloned from the bookmark panel) ──
  function hlPanelHtml() {
    return `
      <div class="hl-panel" id="hl-panel" role="dialog" aria-label="Highlights">
        <div class="hl-head">
          <span class="hl-title">Highlights</span>
          <button id="hl-close" class="hl-close" aria-label="Close">&times;</button>
        </div>
        <div class="hl-list" id="hl-list"></div>
      </div>`;
  }

  function buildHlTrigger() {
    const btn = $("#hl-btn");
    if (!btn) return;
    btn.addEventListener("keydown", containReaderKeys);
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      if (hlPanelOpen) closeHlPanel(true);
      else openHlPanel();
    });
  }

  // Chapter label for a drawer row: TOC title when known (same resolution as
  // bookmarkLabel), else "Chapter N". Highlights are chapter-mode only, so no
  // page-mode branch.
  function hlChapterLabel(hl) {
    if (readerState && Array.isArray(readerState.toc)) {
      const entry = readerState.toc.find((e) => e.chapter_index === hl.chapterIndex);
      if (entry && entry.label) return entry.label;
    }
    return `Chapter ${hl.chapterIndex + 1}`;
  }

  function renderHlList() {
    const list = $("#hl-list");
    if (!list) return;
    hlNoteEditing = false; // a rebuild always destroys any inline editor
    // Offline / terminal GET failure (spec §5): highlights are online-only.
    if (highlightsState.failed && !highlightsState.loaded) {
      list.innerHTML = `<p class="hl-empty">Highlights need a connection.</p>`;
      return;
    }
    const items = highlightsState.items;
    if (!items.length) {
      list.innerHTML = `<p class="hl-empty">Select text while reading to highlight it.</p>`;
      return;
    }
    list.innerHTML = items
      .map((hl) => {
        // Never interpolate a stored color into a style attribute: legacy/
        // desktop rows could hold free-form strings that smuggle extra CSS
        // declarations past esc(). Off-palette values fall back to default.
        const dot = HIGHLIGHT_COLORS.includes(hl.color) ? hl.color : HIGHLIGHT_COLORS[0];
        return `
      <div class="hl-entry" data-id="${esc(hl.id)}" tabindex="0" role="button">
        <span class="hl-dot" style="background-color: ${dot}"></span>
        <span class="hl-entry-body">
          <span class="hl-entry-quote">${esc(hl.text)}</span>
          ${hl.note ? `<span class="hl-entry-note">${esc(hl.note)}</span>` : ""}
          <span class="hl-entry-chapter">${esc(hlChapterLabel(hl))}</span>
        </span>
        <button class="hl-entry-note-btn" data-id="${esc(hl.id)}" aria-label="Edit note">&#9998;</button>
        <button class="hl-entry-delete" data-id="${esc(hl.id)}" aria-label="Delete highlight">&times;</button>
      </div>`;
      })
      .join("");
  }

  // Guarded drawer refresh for async reconciliation paths (create/update/
  // delete responses): re-render only while the panel is actually open and
  // no inline note editor would be destroyed mid-edit.
  function renderHlListIfOpen() {
    if (hlPanelOpen && !hlNoteEditing) renderHlList();
  }

  function closeHlPanel(restoreFocus) {
    const panel = $("#hl-panel");
    const btn = $("#hl-btn");
    if (panel) panel.classList.remove("open");
    if (btn) btn.setAttribute("aria-expanded", "false");
    hlPanelOpen = false;
    if (restoreFocus && btn) btn.focus();
  }

  function openHlPanel() {
    const panel = $("#hl-panel");
    const btn = $("#hl-btn");
    if (!panel || !btn || !readerState) return;
    // Mutual exclusion with the other bottom-chrome panels.
    if (typoPanelOpen) closeTypoPanel(false);
    if (tocPanelOpen) closeTocPanel(false);
    if (bookmarkPanelOpen) closeBookmarkPanel(false);
    if (vocabPanelOpen) closeVocabPanel(false);
    renderHlList();
    panel.classList.add("open");
    btn.setAttribute("aria-expanded", "true");
    hlPanelOpen = true;
    refreshHlPanelData(); // lazy retry; re-renders on completion if still open
  }

  // The book-entry GET normally settles long before the drawer opens; this
  // retries a failed (e.g. offline) fetch on open, with the same id-recheck
  // race guards as loadBookmarks.
  async function refreshHlPanelData() {
    if (!readerState || readerState.mode === "page") return;
    const bookId = readerState.id;
    if (highlightsState.bookId === bookId && highlightsState.loaded) return;
    await loadHighlightsForBook(bookId);
    if (!readerState || readerState.id !== bookId) return;
    renderHlListIfOpen();
    rerenderChapterHighlights(); // marks may be renderable now
  }

  function wireHlControls() {
    const closeBtn = $("#hl-close");
    if (closeBtn) closeBtn.addEventListener("click", () => closeHlPanel(true));
    const list = $("#hl-list");
    if (list)
      list.addEventListener("click", (e) => {
        // A click inside an active note editor must not be treated as a row
        // tap (belt-and-suspenders with the editor's own stopPropagation).
        if (e.target.closest(".hl-note-editor")) return;
        const del = e.target.closest(".hl-entry-delete");
        if (del) {
          e.stopPropagation();
          deleteHighlight(del.getAttribute("data-id"));
          renderHlList(); // optimistic removal is synchronous
          return;
        }
        const note = e.target.closest(".hl-entry-note-btn");
        if (note) {
          e.stopPropagation();
          const en = note.closest(".hl-entry");
          const hl = highlightsState.items.find((h) => h.id === note.getAttribute("data-id"));
          // tmp- = optimistic create not yet reconciled — a PUT would 404.
          if (en && hl && !String(hl.id).startsWith("tmp-")) openHlNoteEditor(en, hl);
          return;
        }
        const entry = e.target.closest(".hl-entry");
        if (!entry) return;
        const hl = highlightsState.items.find((h) => h.id === entry.getAttribute("data-id"));
        if (!hl) return;
        startHlJump(hl);
      });
    const panel = $("#hl-panel");
    if (panel) panel.addEventListener("keydown", containReaderKeys);
    wirePanelRootDismissal({
      isOpen: () => hlPanelOpen,
      close: closeHlPanel,
      panelSel: "#hl-panel",
      btnSel: "#hl-btn",
    });
  }

  // ── Inline note editor (drawer row — mirrors the bookmark inline rename) ──
  function rowFor(hlId) {
    return document.querySelector(`#hl-list .hl-entry[data-id="${CSS.escape(hlId)}"]`);
  }

  function openHlNoteEditor(rowEl, hl) {
    const body = rowEl.querySelector(".hl-entry-body");
    if (!body) return;
    hlNoteEditing = true;
    const wrap = document.createElement("div");
    wrap.className = "hl-note-editor";
    const input = document.createElement("textarea");
    input.className = "hl-note-input";
    input.maxLength = 2000; // server truncates too; cap the input to match
    input.value = hl.note || "";
    // Clicks placing the cursor must not bubble to the row-tap (jump) handler,
    // and typing must not reach the reader shortcuts. Escape cancels the edit.
    input.addEventListener("mousedown", (e) => e.stopPropagation());
    input.addEventListener("click", (e) => e.stopPropagation());
    input.addEventListener("keydown", (e) => {
      e.stopPropagation();
      if (e.key === "Escape") {
        e.preventDefault();
        renderHlList(); // discard the edit (also clears hlNoteEditing)
      }
    });
    const save = document.createElement("button");
    save.type = "button";
    save.className = "hl-note-save";
    save.textContent = "Save";
    save.addEventListener("click", async (e) => {
      e.stopPropagation();
      hlNoteEditing = false;
      const v = input.value.trim();
      await updateHighlight(hl.id, { note: v.length ? v : null }); // empty → clear (null)
      renderHlList();
    });
    wrap.append(input, save);
    body.replaceWith(wrap);
    input.focus();
  }

  async function renderReaderContent() {
    const { id, mode, index, count } = readerState;
    // Highlight jump: bind the dispatched navId to THIS render, synchronously,
    // before any await — if this render is later superseded (renderGen), the
    // navId dies with it instead of leaking to the next unrelated render.
    const renderHlNavId = hlJumpNavId;
    hlJumpNavId = null;
    // Same capture for a pending vocabulary jump.
    const renderVocabNavId = vocabJumpNavId;
    vocabJumpNavId = null;
    // F8: record the navigated-to index immediately, synchronously, before
    // any await below. Previously this only happened at the tail of this
    // function (after the chapter-content fetch resolved) or on a confirmed
    // PUT response — on a slow connection, leaving the reader before either
    // of those completed left `lastKnownProgress` stale, so a showDetail()
    // GET that raced an in-flight save would win with old data (the exact
    // race F8 exists to prevent).
    recordLocalProgress(id, index, readerState.scrollPosition || 0);
    // R1: monotonic per-book render generation. Captured before each await
    // below; if it no longer matches after the await, a newer render (or a
    // fresh book load) has superseded this one — abandon without touching
    // the DOM.
    readerState.renderGen = (readerState.renderGen || 0) + 1;
    const gen = readerState.renderGen;
    // F2: consumed synchronously, right here, rather than at the end of this
    // function — an entry's first render can be abandoned by a rapid second
    // navigation before it reaches its own tail (see the R1 guards below),
    // which would otherwise leave the flag set and wrongly suppress the
    // *next* (real) navigation's save too.
    const isInitialRender = !!readerState.suppressNextSave;
    readerState.suppressNextSave = false;
    updateProgressUI();

    if (mode === "chapter") {
      const contentEl = $("#reader-content");
      // F-4-4: serve synchronously from the prefetch cache when present — a
      // forward turn to an already-prefetched chapter renders with no network
      // round-trip. A miss falls back to the existing on-demand fetch below.
      const cachedHtml = getCachedChapterHtml(index);
      let html;
      if (cachedHtml !== null) {
        html = cachedHtml;
      } else {
        if (contentEl) contentEl.innerHTML = '<div class="loading">Loading...</div>';
        // F-4-4: if a prefetch for this chapter is already in flight (a fast
        // forward turn that beat it), await that request rather than firing a
        // duplicate — the prefetch stores into the cache on success, so this
        // still yields the same HTML without a second round-trip.
        const pending = readerState.chapterPrefetching && readerState.chapterPrefetching.get(index);
        if (pending) {
          let pendingHtml = null;
          try {
            pendingHtml = await pending;
          } catch (e) {
            pendingHtml = null;
          }
          if (!readerState || readerState.renderGen !== gen) return;
          if (typeof pendingHtml === "string") html = pendingHtml;
        }
        if (html === undefined) {
          // Finding 1: a network error here previously propagated as an
          // unhandled rejection, leaving the "Loading..." text stuck forever —
          // catch it and show the same escaped reader-error message a non-2xx
          // response already gets, plus a toast.
          let chResp;
          try {
            chResp = await api(`/api/books/${id}/chapters/${index}`);
          } catch (e) {
            if (!readerState || readerState.renderGen !== gen) return;
            if (contentEl) {
              contentEl.innerHTML = `<div class="reader-error">${esc(apiFailureToastMessage(e))}</div>`;
            }
            showToast(apiFailureToastMessage(e));
            return;
          }
          if (!readerState || readerState.renderGen !== gen) return;
          if (!chResp) return;
          // S1: non-2xx bodies are plain-text error strings that may contain
          // book-derived content (e.g. from a crafted EPUB) — never insert
          // them as HTML. Render a static, escaped message instead.
          if (!chResp.ok) {
            if (contentEl) {
              contentEl.innerHTML = `<div class="reader-error">${esc(`Couldn't load this chapter (HTTP ${chResp.status})`)}</div>`;
            }
            return;
          }
          html = await chResp.text();
          if (!readerState || readerState.renderGen !== gen) return;
          // Cache it so a later return to this chapter is instant too.
          if (readerState.chapterHtmlCache) readerState.chapterHtmlCache[index] = html;
        }
      }
      if (contentEl) contentEl.innerHTML = html;
      // Highlights: remember this chapter's clean HTML (the rebuild source for
      // rerenderChapterHighlights), then wrap stored highlights into the fresh
      // DOM. We are inside the chapter-mode branch — page mode never gets here.
      currentChapterCleanHtml = html;
      currentChapterIndex = index;
      // Both consumers scroll SYNCHRONOUSLY when they consume a jump, so the
      // restore below has to know it happened — otherwise its rAF resets the
      // stage to 0 and the jump silently degrades to plain chapter
      // navigation. This bit was missing for highlights too (the drawer's
      // cross-chapter jump has never actually landed on the mark); the
      // shipped e2e only highlighted a chapter's FIRST text, which is in view
      // at scrollTop 0 either way, so it never caught it.
      // Evaluated into separate consts, never `a() || b()`: both consumers
      // must run on every render (each is also what CLEARS its own stale
      // token), so short-circuiting one would leak a pending jump.
      const hlScrolled = contentEl ? applyChapterHighlights(contentEl, index, renderHlNavId) === true : false;
      const vocabScrolled = contentEl ? maybeConsumeVocabJump(contentEl, index, renderVocabNavId) === true : false;
      const jumpScrolled = hlScrolled || vocabScrolled;
      renderChapterTurnAnimation(contentEl, index, isInitialRender);
      // K5: native Space/PageDown scrolling needs the scroll container focused.
      const stage = $("#reader-stage");
      if (stage) {
        stage.focus();
        // F2b: restore the saved in-chapter offset on this entry only —
        // `pendingScrollRestore` is consumed once and is 0 for a normal
        // chapter turn, which just lands at the top like before.
        const restoreRatio = readerState.pendingScrollRestore || 0;
        readerState.pendingScrollRestore = 0;
        let establishedTop = null;
        requestAnimationFrame(() => {
          if (!readerState || readerState.renderGen !== gen) return;
          const max = stage.scrollHeight - stage.clientHeight;
          // A consumed jump already positioned the stage on its target
          // (synchronously, above) — leave it exactly where it landed.
          if (jumpScrolled) {
            establishedTop = Math.round(stage.scrollTop);
            return;
          }
          if (restoreRatio > 0 && max > 0) {
            setScrollTop(stage, restoreRatio * max);
          } else {
            stage.scrollTop = 0;
          }
          establishedTop = Math.round(stage.scrollTop);
        });
        // The reading font may load after first paint (font-display: swap),
        // changing chapter height so the ratio-restore above lands short. Re-
        // apply the saved ratio once fonts settle — but ONLY if the reader is
        // still exactly where the rAF restore left it. A genuine user scroll or
        // the browser's own scroll-anchoring moving the offset means: leave it
        // alone (this is robust even if a coalesced scroll slipped past
        // suppressScrollSave). Also guarded by the same render generation and a
        // unique token a user scroll cancels.
        if (restoreRatio > 0 && !jumpScrolled) {
          const token = (pendingReanchor = {});
          document.fonts.ready.then(() => {
            if (pendingReanchor !== token) return;
            if (!readerState || readerState.renderGen !== gen || readerState.mode !== "chapter" || !stage.isConnected) { pendingReanchor = null; return; }
            if (establishedTop === null || Math.round(stage.scrollTop) !== establishedTop) { pendingReanchor = null; return; }
            const max = stage.scrollHeight - stage.clientHeight;
            if (max > 0) setScrollTop(stage, restoreRatio * max);
            pendingReanchor = null;
          });
        }
      }
      // F-4-4: now that this chapter is on screen, prefetch its neighbours
      // (next first) into the cache so the next forward turn is instant.
      prefetchAdjacentChapters(id, index, count);
    } else {
      renderPageTurn(id, index, count);
    }

    // F2: a mere open (or resume/restart choice, which is also just an open)
    // must never itself persist a save — only a real subsequent navigation
    // or scroll should.
    if (!isInitialRender) {
      scheduleProgressSave();
    }
  }

  // ── Item 12: page-turn / chapter-turn commit animation ─────────────────
  // Page-turn semantics (index bookkeeping, saves, preload) are unchanged
  // from Item 3/4 above — this section is a presentation layer on top: it
  // decides whether a turn *shows* a slide-in and drives the two-stacked-img
  // swap for page mode, or a single class toggle on `.content` for chapter
  // mode.

  // Keeps a handful of already-fetched neighbor images so a turn can tell
  // "loaded, safe to animate in" from "not ready yet, hard cut" (point 3 of
  // the spec: never animate an unloaded image). Keyed by index (not URL) so
  // it can be pruned to a small window around the current page.
  function preloadPage(id, index, count) {
    if (index < 0 || index >= count || !readerState) return;
    const cache = readerState.preloadCache;
    if (cache[index]) return;
    const img = new Image();
    img.src = pageUrl(id, index);
    cache[index] = img;
  }

  function getPreloadedImage(index) {
    const cache = readerState && readerState.preloadCache;
    const img = cache && cache[index];
    return img && img.complete && img.naturalWidth > 0 ? img : null;
  }

  function prunePreloadCache(centerIndex) {
    const cache = readerState && readerState.preloadCache;
    if (!cache) return;
    Object.keys(cache).forEach((k) => {
      if (Math.abs(Number(k) - centerIndex) > 2) delete cache[k];
    });
  }

  // ── F-4-4: chapter HTML prefetch (reflowable EPUB/MOBI reader) ──────────
  // The chapter-mode analogue of the page-image preloader above: as soon as a
  // chapter renders, fetch the NEXT chapter's sanitized HTML (and warm its
  // inline image URLs) into `readerState.chapterHtmlCache` so a forward turn
  // renders synchronously from cache instead of awaiting the
  // /api/books/:id/chapters/:index round-trip. Bounded to a small window
  // around the current chapter to cap memory; book-scoped because the cache
  // lives on readerState (recreated per book open).
  const CHAPTER_PREFETCH_RADIUS = 2;

  function getCachedChapterHtml(index) {
    const cache = readerState && readerState.chapterHtmlCache;
    const html = cache && cache[index];
    return typeof html === "string" ? html : null;
  }

  // Warm the browser's HTTP cache for the inline images the prefetched
  // chapter references, so they're already in hand when the turn happens.
  // The server rewrites a book's own images to same-origin
  // /api/books/:id/images/... URLs; only those are warmed. External
  // (http(s)) <img> sources — which the sanitizer passes through untouched —
  // are deliberately skipped: proactively fetching a third-party image for a
  // chapter the reader may never open would leak reading activity earlier and
  // more broadly than an on-demand load ever did.
  function warmChapterImages(id, html) {
    chapterImageUrls(id, html).forEach((u) => {
      const img = new Image();
      img.src = u;
    });
  }

  // Shared with the offline save engine: the inline-image URLs a chapter's
  // sanitized HTML references, restricted to THIS book's own rewritten image
  // route (`/api/books/{id}/images/...`) and returned in origin-relative
  // canonical form. Anything else — external origins, or same-origin URLs
  // outside the book's image route — is excluded, so a crafted chapter can't
  // make the save engine proactively fetch and retain unrelated API
  // resources (and the prefetch warmer keeps its same-book-only scope).
  // DOMParser (inert — never executes scripts or loads resources) rather
  // than a regex: attribute values arrive HTML-entity-encoded (ammonia
  // serializes '&' in a filename as '&amp;'), and getAttribute returns the
  // decoded value the browser would actually request — a regex over raw text
  // would yield URLs that 404.
  function chapterImageUrls(id, html) {
    const out = [];
    let doc;
    try {
      doc = new DOMParser().parseFromString(html, "text/html");
    } catch (e) {
      return out;
    }
    const prefix = `/api/books/${id}/images/`;
    doc.querySelectorAll("img[src]").forEach((img) => {
      let url;
      try {
        url = new URL(img.getAttribute("src"), window.location.href);
      } catch (e) {
        return;
      }
      if (url.origin !== window.location.origin) return;
      if (!url.pathname.startsWith(prefix)) return;
      out.push(url.pathname + url.search);
    });
    return out;
  }

  function prefetchChapter(id, index, count) {
    if (index < 0 || index >= count || !readerState) return;
    const cache = readerState.chapterHtmlCache;
    const inflight = readerState.chapterPrefetching;
    if (!cache || !inflight) return;
    if (typeof cache[index] === "string" || inflight.has(index)) return;
    // Best-effort background fetch: use a bare `fetch`, NOT the shared `api()`
    // helper. `api()` tears down the view and shows the login screen on a 401
    // — that must never happen for a request the user didn't initiate (a
    // mid-read session expiry would otherwise eject the reader). A prefetch
    // miss just falls back to the on-demand fetch on the actual turn.
    const p = fetch(`/api/books/${id}/chapters/${index}`, { credentials: "same-origin" })
      .then((resp) => (resp && resp.ok ? resp.text() : null))
      .then((html) => {
        // The book may have changed while this was in flight — readerState
        // (and thus its cache) is recreated per book open, so only store when
        // we're still looking at the same cache object.
        if (html != null && readerState && readerState.chapterHtmlCache === cache) {
          cache[index] = html;
          warmChapterImages(id, html);
        }
        return html;
      })
      .catch(() => null) // best-effort: a miss falls back to on-demand fetch
      .finally(() => {
        if (inflight.get(index) === p) inflight.delete(index);
      });
    inflight.set(index, p);
  }

  function prefetchAdjacentChapters(id, centerIndex, count) {
    if (!readerState || readerState.mode !== "chapter") return;
    const cache = readerState.chapterHtmlCache;
    if (!cache) return;
    // Bound memory: drop chapters outside the window around the current one.
    Object.keys(cache).forEach((k) => {
      if (Math.abs(Number(k) - centerIndex) > CHAPTER_PREFETCH_RADIUS) delete cache[k];
    });
    // Next chapter first — forward reading is the common case; previous too,
    // symmetric with the page-image preloader.
    prefetchChapter(id, centerIndex + 1, count);
    prefetchChapter(id, centerIndex - 1, count);
  }

  // Interrupt-safe: called at the start of every page turn so a rapid
  // second turn never leaves the previous one's overlay/transform stuck
  // mid-flight — it jumps straight to that turn's own end state first.
  function finalizeTurnAnimation(img, incoming) {
    if (readerState.turnAnimCleanup) {
      const cleanup = readerState.turnAnimCleanup;
      readerState.turnAnimCleanup = null;
      cleanup();
    } else if (incoming) {
      incoming.hidden = true;
      incoming.classList.remove("slide-in-left", "slide-in-right");
      incoming.style.willChange = "";
    }
    resetDragStyles(img); // also clears any leftover drag-follow transform
  }

  function renderPageTurn(id, index, count) {
    const img = $("#page-img");
    const incoming = $("#page-img-incoming");
    if (!img) return;
    resetZoom(); // any turn (buttons, keys, slider, swipe) lands unzoomed
    finalizeTurnAnimation(img, incoming);

    const prevIndex = readerState.lastRenderedPageIndex;
    const direction = typeof prevIndex === "number" && index !== prevIndex
      ? (index > prevIndex ? "next" : "prev")
      : null;
    const forceInstant = !!readerState.pendingInstantTurn;
    readerState.pendingInstantTurn = false;
    readerState.lastRenderedPageIndex = index;

    const url = pageUrl(id, index);
    const alt = `Page ${index + 1} of ${count}`;
    const cached = !forceInstant && direction && !reducedMotionEnabled() ? getPreloadedImage(index) : null;

    if (cached && incoming) {
      // F5: the overlay is position:absolute anchored to the stage's
      // *unscrolled* origin. In fit-width mode the stage can be scrolled
      // (overflow:auto on a tall page) — if it is, that anchor point is
      // scrolled out of view, so the slide plays off-screen (a dead delay,
      // then a hard cut once img.src swaps on completion). Resetting to the
      // top before playing keeps the whole animation on-screen; in
      // fit-height (never scrollable) this is always a no-op.
      const stage = $("#reader-stage");
      if (stage && stage.scrollTop > 0) stage.scrollTop = 0;

      incoming.src = url; // already loaded — paints immediately, no flash
      incoming.alt = alt;
      incoming.hidden = false;
      incoming.classList.remove("slide-in-left", "slide-in-right");
      void incoming.offsetWidth; // force reflow so re-adding the class restarts the animation
      incoming.classList.add(direction === "next" ? "slide-in-right" : "slide-in-left");

      // F3: promotion must happen exactly once, driven by whichever of
      // animationend/animationcancel/the fallback timeout fires first — a
      // backgrounded tab can throttle or drop the animation event entirely,
      // which would otherwise leave the incoming overlay stuck visible over
      // a stale #page-img forever.
      let done = false;
      const finish = () => {
        if (done) return;
        done = true;
        incoming.removeEventListener("animationend", onAnimEnd);
        incoming.removeEventListener("animationcancel", onAnimEnd);
        clearTimeout(timer);
        // A ctrl+wheel/pinch zoom landed during the ~280ms slide (on the
        // covered outgoing img, clamped to its dimensions) must not leak
        // onto the incoming page — re-assert "every turn lands unzoomed".
        resetZoom();
        img.src = url;
        img.alt = alt;
        incoming.hidden = true;
        incoming.classList.remove("slide-in-left", "slide-in-right");
        incoming.style.willChange = "";
        // Reader may have been exited during the ~280ms window (showDetail/
        // showLibrary null readerState without running this cleanup).
        if (readerState && readerState.turnAnimCleanup === finish) readerState.turnAnimCleanup = null;
      };
      const onAnimEnd = () => finish();
      incoming.addEventListener("animationend", onAnimEnd);
      incoming.addEventListener("animationcancel", onAnimEnd);
      // Slightly longer than --dur-page-turn (0.22s) so it never races a
      // normal completion.
      const timer = setTimeout(finish, 280);
      readerState.turnAnimCleanup = finish;
    } else {
      img.src = url;
      img.alt = alt;
    }

    // Preload neighbors so turns feel instant; browser HTTP cache does the
    // rest, and a bounded window keeps this from growing unbounded over a
    // long reading session in a large book.
    preloadPage(id, index + 1, count);
    preloadPage(id, index - 1, count);
    prunePreloadCache(index);
  }

  // Chapter mode has no "unloaded" concern (the full HTML is already in
  // hand by the time this runs) — just slide the freshly-rendered content
  // in from the appropriate side. `readerState.chapterAnimCleanup` (same
  // pattern as `turnAnimCleanup` above) guards against a rapid second
  // chapter render's cleanup firing after a third one has already restarted
  // the animation on the same (reused) `.content` element.
  function renderChapterTurnAnimation(contentEl, index, isInitialRender) {
    if (!contentEl || !readerState) return;
    // F4: run any in-flight prior turn's cleanup — or, if none is pending,
    // strip a leftover slide-in-* class directly — BEFORE any early return
    // below. #reader-content's `.content` element is reused across chapter
    // turns; without this, an instant render (slider jump, reduced-motion,
    // or the initial open) landing while a previous animation is still
    // running would keep that stale class and render mid-slide.
    if (readerState.chapterAnimCleanup) {
      const cleanup = readerState.chapterAnimCleanup;
      readerState.chapterAnimCleanup = null;
      cleanup();
    } else {
      contentEl.classList.remove("slide-in-left", "slide-in-right");
    }

    const prevIndex = readerState.lastRenderedChapterIndex;
    readerState.lastRenderedChapterIndex = index;
    const forceInstant = !!readerState.pendingInstantTurn;
    readerState.pendingInstantTurn = false;
    if (isInitialRender || forceInstant || reducedMotionEnabled()) return;
    if (typeof prevIndex !== "number" || index === prevIndex) return;

    const direction = index > prevIndex ? "next" : "prev";
    void contentEl.offsetWidth; // force reflow so re-adding the class restarts the animation
    contentEl.classList.add(direction === "next" ? "slide-in-right" : "slide-in-left");

    // F3-parity fallback: promotion (here, just the class removal) must run
    // exactly once off animationend/animationcancel/timeout, whichever
    // fires first, so a throttled/dropped event in a backgrounded tab
    // doesn't leave the class (and its will-change) applied forever.
    let done = false;
    const finish = () => {
      if (done) return;
      done = true;
      contentEl.removeEventListener("animationend", onEnd);
      contentEl.removeEventListener("animationcancel", onEnd);
      clearTimeout(timer);
      if (readerState.chapterAnimCleanup === finish) readerState.chapterAnimCleanup = null;
      contentEl.classList.remove("slide-in-left", "slide-in-right");
    };
    const onEnd = () => finish();
    contentEl.addEventListener("animationend", onEnd);
    contentEl.addEventListener("animationcancel", onEnd);
    const timer = setTimeout(finish, 280);
    readerState.chapterAnimCleanup = finish;
  }

  // ── Item 4: reading progress sync ──────────────
  // Debounced save while turning pages/chapters or scrolling within a
  // chapter, flushed immediately on tab hide / navigation-away so a closed
  // tab never loses the last position.
  const PROGRESS_SAVE_DEBOUNCE_MS = 2000;
  let progressSaveTimer = null;

  // F8: record this tab's most recent progress immediately (before the
  // network round trip), so showDetail() can prefer it over a GET that may
  // have raced an in-flight save.
  function recordLocalProgress(id, chapterIndex, scrollPosition) {
    lastKnownProgress[id] = { chapterIndex, scrollPosition, ts: Date.now() };
  }

  // F9: the actual network write, run one-at-a-time per book via
  // queueProgressSave's promise chain. Drops an out-of-order save that would
  // regress this session's high-water mark for the book, unless `reset` is
  // set (an explicit "Start Over").
  async function sendProgress(id, chapterIndex, scrollPosition, opts) {
    opts = opts || {};
    if (!opts.reset && lastSentIndex[id] !== undefined && chapterIndex < lastSentIndex[id]) {
      return;
    }
    try {
      // Checked on the reader's own write path, not just on reads (via
      // apiWrite): this is the request most likely to be the first thing a
      // long-open reader tab does after the active profile moved under it.
      const resp = await apiWrite(`/api/books/${id}/progress`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ chapter_index: chapterIndex, scroll_position: scrollPosition }),
        credentials: "same-origin",
        keepalive: true,
      });
      // F3: a debounced/flushed save (unlike the pagehide teardown flush)
      // can and should react to an expired session — route to the same
      // login redirect the rest of the app uses.
      if (resp.status === 401) {
        authenticated = false;
        showLogin();
        return;
      }
      if (resp.ok) {
        lastSentIndex[id] = chapterIndex;
        recordLocalProgress(id, chapterIndex, scrollPosition);
        // Advance the offline sync baseline so a later offline write's replay
        // compares against the position we just committed. Awaited so it
        // commits within this saveChains link, before any later queued write
        // reads the baseline (sendProgress always runs inside queueProgressSave).
        // The body read is bounded: on a flaky connection the 200 headers can
        // arrive while the tiny JSON body stalls, and an unbounded await here
        // would wedge this book's entire save chain (every later page-turn
        // save would queue behind it forever). A 2s cap degrades to "baseline
        // not refreshed this time" — best-effort, self-heals on the next
        // successful progress GET (detail/reader open also call
        // updateOfflineBaseline). Accepted residual: if the body hangs past 2s
        // AND the connection then drops AND a save is queued, all before any
        // such GET, that row's baseline is stale and replay may discard it —
        // a pathological triple-coincidence costing one recoverable position.
        const saved = await Promise.race([
          resp.json().catch(() => null),
          new Promise((r) => setTimeout(() => r(null), 2000)),
        ]);
        if (saved && saved.last_read_at) await updateOfflineBaseline(id, saved.last_read_at);
      }
    } catch (e) {
      // A moved profile is not a network failure: the PUT already committed
      // under the *new* profile, and queueing would replay that position into
      // whichever profile is active after the reload. Drop it.
      if (e instanceof ProfileMovedError) return;
      // Network error: for a saved book, queue the position for
      // compare-then-push replay on reconnect instead of dropping it. Awaited
      // so two failed saves (both inside the saveChains link) can't race on
      // the row's revision and commit out of order.
      await queueOfflineProgress(id, chapterIndex, scrollPosition);
    }
  }

  // F9: serializes saves per book so a debounced save and a later flush can
  // never commit out of order — the next save always waits for the previous
  // one's response before firing.
  function queueProgressSave(id, chapterIndex, scrollPosition, opts) {
    const prev = saveChains[id] || Promise.resolve();
    const next = prev.then(() => sendProgress(id, chapterIndex, scrollPosition, opts));
    saveChains[id] = next.catch(() => {});
    return next;
  }

  // F9: explicit "Start Over" — legitimately writes 0 even though it's lower
  // than anything already sent this session, so it resets the guard first.
  function resetProgress(id) {
    delete lastSentIndex[id];
    recordLocalProgress(id, 0, 0);
    return queueProgressSave(id, 0, 0, { reset: true });
  }

  function scheduleProgressSave() {
    if (!readerState) return;
    recordLocalProgress(readerState.id, readerState.index, readerState.scrollPosition || 0);
    clearTimeout(progressSaveTimer);
    progressSaveTimer = setTimeout(flushProgressSave, PROGRESS_SAVE_DEBOUNCE_MS);
  }

  function flushProgressSave() {
    clearTimeout(progressSaveTimer);
    progressSaveTimer = null;
    if (!readerState) return;
    const { id, index, scrollPosition } = readerState;
    return queueProgressSave(id, index, scrollPosition || 0);
  }

  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") flushProgressSave();
  });
  // F3: the pagehide teardown flush stays fire-and-forget — the page may be
  // gone before a 401 check could do anything useful with it anyway.
  window.addEventListener("pagehide", flushProgressSave);

  // R4: page-mode turns only ever set img.src, so a 401 on session expiry
  // fails silently (broken image, no redirect to login). Probe a cheap
  // authenticated endpoint — api() already redirects to login on 401 — to
  // distinguish "session expired" from a genuine image failure.
  async function handlePageImageError() {
    if (!readerState) return;
    const { id, index } = readerState;

    // A page image can fail because the browser's HTTP cache holds a poisoned
    // entry (a bad/truncated 200), which then re-serves those broken bytes to
    // every load of this URL — the page never recovers on its own. On the
    // first failure for this index, retry once with a cache-busting query
    // param so the browser fetches the page fresh from the server instead of
    // the bad cached copy (the param is ignored by the route's path match).
    // A blob from a `fetch(..., {cache:"reload"})` would be cleaner but the
    // web UI's CSP img-src is `'self' data:` — no `blob:` — so a same-origin
    // URL is the one that can actually paint. Guarded by pageRetryIndex so a
    // genuinely unloadable page falls through to the error box below rather
    // than looping.
    if (readerState.pageRetryIndex !== index) {
      readerState.pageRetryIndex = index;
      const nonce = (readerState.pageReloadNonce = (readerState.pageReloadNonce || 0) + 1);
      const img = $("#page-img");
      if (img) {
        img.src = `${pageUrl(id, index)}?__reload=${nonce}`;
        return;
      }
    }

    // Finding 1: a network error here means the probe itself couldn't run —
    // that's just as much "can't load this page" as any other outcome, so
    // fall through to the same error box instead of the 401 early return.
    let check;
    try {
      check = await api(`/api/books/${readerState.id}`);
    } catch (e) {
      check = undefined;
    }
    if (check === null) return; // 401 — api() already redirected to the login screen
    const img = $("#page-img");
    const errEl = $("#page-error");
    if (img) img.style.display = "none";
    if (errEl) {
      errEl.hidden = false;
      errEl.innerHTML = esc("Couldn't load this page.");
    }
  }

  // K5: fallback for Space/Shift+Space when the stage doesn't have native
  // focus. Scrolls by ~90% of the visible stage height.
  function scrollReaderStage(direction) {
    const stage = $("#reader-stage");
    if (!stage) return;
    stage.scrollBy({ top: stage.clientHeight * 0.9 * direction, behavior: "auto" });
  }

  function updateProgressUI() {
    const { mode, index, count } = readerState;
    const unit = mode === "page" ? "Page" : "Chapter";
    const label = $("#page-label");
    if (label) label.textContent = `${unit} ${index + 1} / ${count}`;
    const slider = $("#page-slider");
    if (slider) {
      slider.value = index;
      // Item 10: screen readers announce this instead of the raw numeric
      // value while dragging/stepping the slider.
      slider.setAttribute("aria-valuetext", `${unit} ${index + 1} of ${count}`);
    }
    const prevBtn = $("#prev-btn");
    const nextBtn = $("#next-btn");
    if (prevBtn) prevBtn.disabled = index <= 0;
    if (nextBtn) nextBtn.disabled = index >= count - 1;
    // Keep the TOC's current-chapter highlight in sync when a Prev/Next turn
    // happens with the panel already open.
    if (tocPanelOpen) renderTocList();
  }

  // ── Stats ──────────────────────────────────────
  async function showStats() {
    currentView = "stats";
    document.title = "Carrel";
    flushProgressSave();
    readerState = null;
    resumePromptActive = false;
    app().innerHTML = `
      <div class="header">
        <button class="back-btn" id="back-btn" aria-label="Back">&larr;</button>
        <h1>Reading Stats</h1>
        <span style="flex:1"></span>
        ${navIconsHtml("stats")}
      </div>
      <div class="stats"><div class="loading">Loading...</div></div>
      ${tabBarHtml("stats")}`;
    $("#back-btn").addEventListener("click", goHome);
    bindNavIcons();
    bindTabBar();
    ensureDictionaryStatus().then(applyVocabNavVisibility);

    // Finding 1: stats previously had no failure handling at all beyond a
    // 401 — a network error, non-2xx response, or bad JSON left "Loading..."
    // on screen forever. `currentView !== "stats"` guards every branch below
    // against rendering over a view the user has since navigated to.
    let resp;
    try {
      resp = await api("/api/stats");
    } catch (e) {
      if (currentView !== "stats") return;
      const container = $(".stats");
      if (container) container.innerHTML = `<div class="empty">${esc(apiFailureToastMessage(e))}</div>`;
      showToast(apiFailureToastMessage(e));
      return;
    }
    if (!resp || currentView !== "stats") return;
    if (!resp.ok) {
      const container = $(".stats");
      if (container) container.innerHTML = `<div class="empty">${esc(`Couldn't load stats (HTTP ${resp.status})`)}</div>`;
      showToast(httpErrorToastMessage(resp.status));
      return;
    }
    let s;
    try {
      s = await resp.json();
    } catch (e) {
      const container = $(".stats");
      if (container) container.innerHTML = `<div class="empty">${esc("Unexpected response")}</div>`;
      showToast("Unexpected response");
      return;
    }
    if (currentView !== "stats") return;

    const container = $(".stats");
    if (!container) return;
    if (!s || (s.totalSessions === 0 && s.totalReadingTimeSecs === 0)) {
      container.innerHTML = '<div class="empty">No reading stats yet. Start reading on the desktop app to see your progress here.</div>';
      return;
    }

    const maxDaily = s.dailyReading.reduce((max, entry) => Math.max(max, entry[1]), 0);

    let chartHtml = "";
    if (s.dailyReading.length > 0 && maxDaily > 0) {
      const bars = s.dailyReading.map(([date, secs]) => {
        const pct = Math.max(4, (secs / maxDaily) * 100);
        return `<div class="stat-bar" style="height:${pct}%" title="${date}: ${formatDuration(secs)}"></div>`;
      }).join("");
      chartHtml = `
        <div class="stat-chart-header">
          <div class="stat-chart-title">Last 30 Days</div>
          <div class="stat-chart-peak">${formatDuration(maxDaily)} peak</div>
        </div>
        <div class="stat-chart">${bars}</div>`;
    }

    const streak = (d) => d === 1 ? "1 day" : d + " days";

    container.innerHTML = `
      <div class="stat-cards">
        <div class="stat-card"><div class="stat-value">${formatDuration(s.totalReadingTimeSecs)}</div><div class="stat-label">Time Reading</div></div>
        <div class="stat-card"><div class="stat-value">${s.totalSessions}</div><div class="stat-label">Sessions</div></div>
        <div class="stat-card"><div class="stat-value">${s.totalPagesRead.toLocaleString()}</div><div class="stat-label">Pages Read</div></div>
        <div class="stat-card"><div class="stat-value">${s.booksFinished}</div><div class="stat-label">Books Finished</div></div>
        <div class="stat-card"><div class="stat-value accent">${streak(s.currentStreakDays)}</div><div class="stat-label">Current Streak</div></div>
        <div class="stat-card"><div class="stat-value">${streak(s.longestStreakDays)}</div><div class="stat-label">Longest Streak</div></div>
      </div>
      ${chartHtml}`;
  }

  // ── Collections ────────────────────────────────
  async function showCollections() {
    currentView = "collections";
    document.title = "Carrel";
    flushProgressSave();
    readerState = null;
    resumePromptActive = false;
    app().innerHTML = `
      <div class="header">
        <button class="back-btn" id="back-btn" aria-label="Back">&larr;</button>
        <h1>Collections</h1>
        <span style="flex:1"></span>
        ${navIconsHtml("collections")}
      </div>
      <div class="collections"><div class="loading">Loading...</div></div>
      ${tabBarHtml("collections")}`;
    $("#back-btn").addEventListener("click", goHome);
    bindNavIcons();
    bindTabBar();
    ensureDictionaryStatus().then(applyVocabNavVisibility);

    // Finding 1: this previously had no failure handling at all — a network
    // error, non-2xx response, or bad JSON left "Loading..." on screen
    // forever (or, on a 401, could throw trying to use `.collections` after
    // showLogin() had already replaced it). `currentView !== "collections"`
    // guards every branch below against rendering over a view the user has
    // since navigated to.
    let collectionsResp, seriesResp;
    try {
      [collectionsResp, seriesResp] = await Promise.all([
        api("/api/collections"),
        api("/api/series"),
      ]);
    } catch (e) {
      if (currentView !== "collections") return;
      const container = $(".collections");
      if (container) container.innerHTML = `<div class="empty">${esc(apiFailureToastMessage(e))}</div>`;
      showToast(apiFailureToastMessage(e));
      return;
    }
    if (currentView !== "collections") return;
    // A 401 on either fetch already redirected to the login screen.
    if (collectionsResp === null || seriesResp === null) return;
    const badResp = !collectionsResp.ok ? collectionsResp : !seriesResp.ok ? seriesResp : null;
    if (badResp) {
      const container = $(".collections");
      if (container) container.innerHTML = `<div class="empty">${esc(`Couldn't load collections (HTTP ${badResp.status})`)}</div>`;
      showToast(httpErrorToastMessage(badResp.status));
      return;
    }
    let collections, series;
    try {
      collections = await collectionsResp.json();
      series = await seriesResp.json();
    } catch (e) {
      const container = $(".collections");
      if (container) container.innerHTML = `<div class="empty">${esc("Unexpected response")}</div>`;
      showToast("Unexpected response");
      return;
    }

    const container = $(".collections");
    if (!container) return;
    if (collections.length === 0 && series.length === 0) {
      container.innerHTML = '<div class="empty">No collections yet. Create collections in the desktop app.</div>';
      return;
    }

    let sortAsc = true;
    let filterText = "";

    function render() {
      const q = filterText.toLowerCase();
      const filteredColls = collections
        .filter(c => !q || c.name.toLowerCase().includes(q))
        .sort((a, b) => sortAsc ? a.name.localeCompare(b.name) : b.name.localeCompare(a.name));
      const filteredSeries = series
        .filter(s => !q || s.name.toLowerCase().includes(q))
        .sort((a, b) => sortAsc ? a.name.localeCompare(b.name) : b.name.localeCompare(a.name));

      let html = `<div class="collections-toolbar">
        <input type="text" id="coll-filter" placeholder="Filter collections..." value="${esc(filterText)}">
        <button class="sort-btn" id="coll-sort">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M11 5h10M11 9h7M11 13h4"/><path d="M3 17l3 3 3-3M6 18V4"/></svg>
          ${sortAsc ? "A→Z" : "Z→A"}
        </button>
      </div>`;

      if (filteredColls.length > 0) {
        html += `<div class="section-header">Collections <span class="count">(${filteredColls.length})</span></div>`;
        html += '<div class="collection-list">';
        for (const c of filteredColls) {
          const icon = c.icon || "📁";
          const colorSwatch = c.color ? `<span class="collection-color" style="background:${esc(c.color)}"></span>` : "";
          const typeBadge = c.type === "automated"
            ? '<span class="auto-dot"></span>Auto-collection'
            : "Manual collection";
          const count = c.bookCount !== undefined ? c.bookCount : "?";
          html += `<div class="collection-row" data-collection-id="${c.id}">
            <span class="collection-icon">${icon}</span>
            <div class="collection-info">
              <div class="collection-name">${colorSwatch}${esc(c.name)}</div>
              <div class="collection-type">${typeBadge}</div>
            </div>
            <span class="collection-count">${count} book${count !== 1 ? "s" : ""}</span>
            <span class="collection-chevron">&rsaquo;</span>
          </div>`;
        }
        html += '</div>';
      }

      if (filteredSeries.length > 0) {
        html += `<div class="section-header">Series <span class="count">(${filteredSeries.length})</span></div>`;
        html += '<div class="collection-list">';
        for (const s of filteredSeries) {
          html += `<div class="collection-row" data-series-name="${esc(s.name)}">
            <span class="collection-icon" style="display:flex;align-items:center;">
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="#888" stroke-width="2"><path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20"/><path d="M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2z"/></svg>
            </span>
            <div class="collection-info">
              <div class="collection-name">${esc(s.name)}</div>
            </div>
            <span class="collection-count">${s.count} book${s.count !== 1 ? "s" : ""}</span>
            <span class="collection-chevron">&rsaquo;</span>
          </div>`;
        }
        html += '</div>';
      }

      if (filteredColls.length === 0 && filteredSeries.length === 0) {
        html += '<div class="empty">No matches</div>';
      }

      container.innerHTML = html;

      const filterInput = $("#coll-filter");
      let filterTimer;
      filterInput.oninput = (e) => {
        clearTimeout(filterTimer);
        filterTimer = setTimeout(() => { filterText = e.target.value; render(); }, 200);
      };
      filterInput.focus();

      $("#coll-sort").onclick = () => { sortAsc = !sortAsc; render(); };

      container.querySelectorAll("[data-collection-id]").forEach(row => {
        row.onclick = () => {
          navigate(libraryHash({ q: "", series: null, collection: row.dataset.collectionId, sort: activeSort }));
        };
      });
      container.querySelectorAll("[data-series-name]").forEach(row => {
        row.onclick = () => {
          navigate(libraryHash({ q: "", series: row.dataset.seriesName, collection: null, sort: activeSort }));
        };
      });
    }

    render();
  }

  // Monotonic visit token for the vocabulary screen — see the `gen`/`stale()`
  // pair in showVocabularyScreen. Module-scoped so a second visit invalidates
  // the first visit's in-flight work (its GET, its debounce, its deletes).
  let vocabScreenGen = 0;

  // Leitner-box review intervals, in days — must match carrel-core's
  // BOX_INTERVAL_DAYS (vocabulary.rs) exactly, since this is only used to
  // PREDICT the outcome of a review for the feedback text (the request body
  // carries no schedule back; desktop's ReviewView predicts the same way,
  // from the pre-review box, rather than round-tripping the server's answer).
  const VOCAB_BOX_INTERVAL_DAYS = [1, 3, 7, 14, 30];
  function vocabBoxIntervalDays(box) {
    const clamped = Math.min(Math.max(Math.round(box), 1), VOCAB_BOX_INTERVAL_DAYS.length);
    return VOCAB_BOX_INTERVAL_DAYS[clamped - 1];
  }
  function vocabDaysLabel(days) {
    return days === 1 ? "1 day" : `${days} days`;
  }

  // Generous safety bound on one review session's queue — the same 200 as
  // desktop's VocabularyPanel.tsx REVIEW_LIMIT (a personal vocabulary list is
  // small; this isn't real pagination). It must not be LOWER than desktop's:
  // the bar labels this number as a count, so a smaller web cap would quietly
  // understate how many words are due and then claim "All caught up" with
  // cards still pending. The server clamps its own maximum independently
  // (VOCABULARY_DUE_MAX_LIMIT, api.rs) — this is just what the web UI asks for.
  const VOCAB_REVIEW_LIMIT = 200;

  // ── Vocabulary screen (M5 "See all", M6 flashcard review) ───────────
  // Cross-book list of every saved word, reachable from the header nav
  // cluster / bottom tab bar and from the book-scoped drawer's "See all"
  // link (M4). Backed by the same `GET /api/vocabulary` the drawer uses,
  // just without `?bookId=`. M6 adds a review mode to this same screen
  // (rather than a new route) so it inherits the per-visit `stale()` guard
  // below instead of needing its own copy. Structured like showCollections()
  // above: fetch once, then a closure-local render() re-renders the current
  // view (list or review) on every filter/sort/delete/answer.
  async function showVocabularyScreen() {
    currentView = "vocabulary";
    document.title = "Vocabulary — Carrel";
    flushProgressSave();
    readerState = null;
    resumePromptActive = false;
    app().innerHTML = `
      <div class="header">
        <button class="back-btn" id="back-btn" aria-label="Back">&larr;</button>
        <h1>Vocabulary</h1>
        <span style="flex:1"></span>
        ${navIconsHtml("vocabulary")}
      </div>
      <div class="vocab-screen"><div class="loading">Loading...</div></div>
      ${tabBarHtml("vocabulary")}`;
    $("#back-btn").addEventListener("click", goHome);
    bindNavIcons();
    bindTabBar();
    ensureDictionaryStatus().then(applyVocabNavVisibility);

    // Per-visit token, not just `currentView`: leaving and re-entering this
    // screen leaves BOTH invocations seeing currentView === "vocabulary", so
    // the older one's slow GET would otherwise select the new visit's
    // container and overwrite fresh rows (and rebind its handlers) with an
    // older snapshot. Every await below re-checks this.
    const gen = ++vocabScreenGen;
    const stale = () => currentView !== "vocabulary" || gen !== vocabScreenGen;

    // Fetched together (Promise.all, same pattern as showCollections' pair of
    // fetches): the due count feeds the "Review N due" bar shown alongside the
    // list, so there is no reason to serialize the two requests.
    let resp, dueResp;
    try {
      [resp, dueResp] = await Promise.all([
        api("/api/vocabulary"),
        api(`/api/vocabulary/due?limit=${VOCAB_REVIEW_LIMIT}`),
      ]);
    } catch (e) {
      if (stale()) return;
      const container = $(".vocab-screen");
      if (container) container.innerHTML = `<div class="empty">${esc(apiFailureToastMessage(e))}</div>`;
      showToast(apiFailureToastMessage(e));
      return;
    }
    if (!resp || !dueResp || stale()) return; // 401 already redirected to login
    // The vocabulary builder was switched off since this tab last cached its
    // status (or was never on) — same defense-in-depth 403 as the book-scoped
    // drawer's loadVocabForBook. Not a connection problem, so it must not go
    // through httpErrorToastMessage's generic "Server error" phrasing.
    const forbiddenResp = resp.status === 403 ? resp : dueResp.status === 403 ? dueResp : null;
    if (forbiddenResp) {
      dropVocabularyStatusCache();
      const container = $(".vocab-screen");
      if (container) container.innerHTML = `<div class="empty">Vocabulary is turned off.</div>`;
      showToast("Vocabulary is turned off");
      return;
    }
    const badResp = !resp.ok ? resp : !dueResp.ok ? dueResp : null;
    if (badResp) {
      const container = $(".vocab-screen");
      if (container) container.innerHTML = `<div class="empty">${esc(`Couldn't load vocabulary (HTTP ${badResp.status})`)}</div>`;
      showToast(httpErrorToastMessage(badResp.status));
      return;
    }
    let words, dueWords;
    try {
      words = await resp.json();
      dueWords = await dueResp.json();
    } catch (e) {
      if (stale()) return;
      const container = $(".vocab-screen");
      if (container) container.innerHTML = `<div class="empty">${esc("Unexpected response")}</div>`;
      showToast("Unexpected response");
      return;
    }
    if (stale()) return;
    const container = $(".vocab-screen");
    if (!container) return;
    words = Array.isArray(words) ? words : [];
    dueWords = Array.isArray(dueWords) ? dueWords : [];

    let filterText = "";
    let sortMode = "newest"; // "newest" | "alphabetical"
    // Hoisted out of render() deliberately (showLibrary's module-scoped
    // searchDebounceTimer has the same rationale): a per-render local cannot
    // be cleared by a render the sort toggle or a delete fired, so a pending
    // search debounce would land after it, re-render with the PREVIOUS query,
    // and drop whatever was typed in between.
    let filterTimer = null;

    // Review mode state. `view` mirrors desktop's VocabularyPanel "list" |
    // "review" — a separate route was deliberately avoided (see the epic
    // plan) so this reuses the per-visit `stale()` guard above rather than
    // needing its own copy of the M5 lifecycle fix.
    let view = "list";
    let reviewQueue = [];
    let reviewTotal = 0;
    let flipped = false;
    let submitting = false;
    // True while a fresh due count is in flight (see backToList /
    // refreshDueCount): the bar renders inert rather than advertising a count
    // that a just-finished session has invalidated.
    let dueCountPending = false;

    // Client-side search predicate — mirrors the desktop app's
    // matchesVocabularyQuery (src/lib/vocabulary.ts): case-insensitive
    // substring match across word, lemma, definition, and bookTitle. An
    // empty query matches everything.
    function matchesQuery(w, query) {
      const needle = query.trim().toLowerCase();
      if (needle === "") return true;
      return (
        w.word.toLowerCase().includes(needle) ||
        w.lemma.toLowerCase().includes(needle) ||
        w.definition.toLowerCase().includes(needle) ||
        (w.bookTitle ? w.bookTitle.toLowerCase().includes(needle) : false)
      );
    }

    // "Chapter N" only, unlike the drawer's vocabChapterLabel — this screen
    // has no live reader for the word's book (it may not even be open, or
    // may span many different books at once), so there is no TOC to resolve
    // a real title against. Still null when the word carries no chapter at
    // all (chapterIndex is nullable).
    function chapterLabel(w) {
      return Number.isFinite(w.chapterIndex) ? `Chapter ${w.chapterIndex + 1}` : null;
    }

    // Dispatches to the current view. Kept as the single entry point every
    // caller below already used pre-M6 (filter debounce, sort click, delete)
    // so none of them need to know which view they're re-rendering into.
    function render() {
      if (stale()) return;
      if (view === "review") { renderReview(); return; }
      renderList();
    }

    function renderList() {
      // Every render replaces the search box along with the list, so a
      // re-render fired by the input's own debounce would drop focus and
      // swallow whatever the user typed next (a pause longer than the
      // debounce is enough). Capture focus and caret BEFORE the innerHTML
      // swap and restore them after. showCollections' unconditional
      // .focus() would also pull focus — and the mobile keyboard — on plain
      // screen entry, so this restores only what was already there.
      // A render can be fired from a debounce or a delete that resolves after
      // the user navigated away (or re-entered, making a NEWER screen the live
      // one). Bail: `container` is detached by then, so the writes below would
      // be invisible while the document-scoped lookups either return null and
      // throw — which on the delete path pre-empts showLogin() — or, worse,
      // find the NEW visit's controls and rebind them to this dead closure.
      if (stale()) return;
      const priorInput = container.querySelector("#vocab-filter");
      const refocus = !!priorInput && document.activeElement === priorInput;
      const caret = refocus ? priorInput.selectionStart : null;
      const filtered = words.filter((w) => matchesQuery(w, filterText));
      const sorted = filtered.slice().sort((a, b) =>
        sortMode === "alphabetical"
          ? a.word.toLowerCase().localeCompare(b.word.toLowerCase())
          : b.createdAt - a.createdAt
      );

      // M6: "Review N due" — hidden entirely when there is nothing saved at
      // all (there's nothing to review either way), disabled when saved but
      // none are due yet.
      let html = "";
      if (words.length > 0) {
        // `dueCountPending` covers the window after a session ends while the
        // fresh count is still in flight: the OLD count is not shown as if it
        // were current, and the button stays inert so the just-reviewed queue
        // cannot be replayed. A capped queue is labelled "N+" because the
        // number is a cap, not a count — claiming an exact figure there was
        // wrong in both directions (understated bar, false "All caught up").
        const capped = dueWords.length >= VOCAB_REVIEW_LIMIT;
        const dueLabel = dueCountPending
          ? "Checking…"
          : `Review ${dueWords.length}${capped ? "+" : ""} due`;
        const dueDisabled = dueCountPending || dueWords.length === 0;
        html += `<div class="vocab-review-bar">
          <button class="vocab-review-btn" id="vocab-review-btn" ${dueDisabled ? "disabled" : ""}>
            ${dueLabel}
          </button>
        </div>`;
      }

      html += `<div class="vocab-screen-toolbar">
        <input type="text" id="vocab-filter" placeholder="Search saved words..." value="${esc(filterText)}">
        <button class="sort-btn" id="vocab-sort">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M11 5h10M11 9h7M11 13h4"/><path d="M3 17l3 3 3-3M6 18V4"/></svg>
          ${sortMode === "alphabetical" ? "A→Z" : "Newest"}
        </button>
      </div>`;

      if (words.length === 0) {
        html += '<div class="empty">No saved words yet. Look up a word in the reader and tap Save to build your vocabulary.</div>';
      } else if (sorted.length === 0) {
        html += '<div class="empty">No matches</div>';
      } else {
        html += '<div class="vocab-screen-list">';
        for (const w of sorted) {
          // A word whose source book was deleted keeps its row (book_id is
          // nullable) — bookTitle is simply absent from the meta line then.
          const meta = [w.bookTitle, chapterLabel(w), formatAddedDate(w.createdAt)].filter(Boolean).join(" · ");
          html += `
          <div class="vocab-screen-entry" data-id="${esc(w.id)}">
            <span class="vocab-entry-body">
              <span class="vocab-entry-word"><strong>${esc(w.word)}</strong>${w.pos ? ` <span class="vocab-entry-pos">${esc(w.pos)}</span>` : ""}</span>
              <span class="vocab-entry-def">${esc(w.definition)}</span>
              ${meta ? `<span class="vocab-entry-meta">${esc(meta)}</span>` : ""}
            </span>
            <button class="vocab-entry-delete" data-id="${esc(w.id)}" aria-label="Delete saved word">&times;</button>
          </div>`;
        }
        html += '</div>';
      }

      container.innerHTML = html;

      const filterInput = container.querySelector("#vocab-filter");
      clearTimeout(filterTimer); // this render supersedes any pending debounce
      filterInput.oninput = (e) => {
        clearTimeout(filterTimer);
        filterTimer = setTimeout(() => { filterText = e.target.value; render(); }, 200);
      };
      if (refocus) {
        filterInput.focus();
        if (caret !== null) filterInput.setSelectionRange(caret, caret);
      }

      container.querySelector("#vocab-sort").onclick = () => {
        sortMode = sortMode === "alphabetical" ? "newest" : "alphabetical";
        render();
      };

      container.querySelectorAll(".vocab-entry-delete").forEach((btn) => {
        btn.onclick = (e) => {
          e.stopPropagation();
          deleteScreenWord(btn.getAttribute("data-id"));
        };
      });

      const reviewBtn = container.querySelector("#vocab-review-btn");
      if (reviewBtn) reviewBtn.onclick = startReview;
    }

    function startReview() {
      if (dueWords.length === 0) return;
      reviewQueue = dueWords.slice();
      reviewTotal = reviewQueue.length;
      flipped = false;
      view = "review";
      render();
    }

    // Leaving review is also the moment the due count can have changed (a
    // reviewed word is rescheduled), so the bar's "Review N due" label must
    // not just go back to showing the STALE count it had on entry.
    function backToList() {
      view = "list";
      // Mark the count unknown BEFORE rendering. Rendering the old count here
      // let the bar advertise the queue that was just reviewed, and clicking
      // it before the refresh landed restarted those same cards.
      dueCountPending = true;
      render();
      refreshDueCount();
    }

    // Re-reads the due queue. On any failure the count stays "unknown" rather
    // than silently reverting to the pre-review number — an unknown count
    // shows as an inert "Checking…" bar, which is honest and, unlike a stale
    // count, cannot be clicked into a replay of an already-reviewed queue.
    async function refreshDueCount() {
      let r;
      try {
        r = await api(`/api/vocabulary/due?limit=${VOCAB_REVIEW_LIMIT}`);
      } catch (e) {
        if (!stale()) { dueWords = []; dueCountPending = false; render(); }
        return;
      }
      if (!r || stale()) return; // a 401 already redirected to login
      if (!r.ok) { dueWords = []; dueCountPending = false; render(); return; }
      let d;
      try {
        d = await r.json();
      } catch (e) {
        if (!stale()) { dueWords = []; dueCountPending = false; render(); }
        return;
      }
      if (stale()) return;
      dueWords = Array.isArray(d) ? d : [];
      dueCountPending = false;
      render();
    }

    function renderReview() {
      if (stale()) return;
      const card = reviewQueue[0] || null;
      let html;
      if (!card) {
        html = `<div class="vocab-review">
          <div class="empty">All caught up — no more cards due right now.</div>
          <button class="vocab-review-back" id="vocab-review-back">Back to list</button>
        </div>`;
      } else {
        const progress = reviewTotal - reviewQueue.length + 1;
        const missedDays = vocabBoxIntervalDays(1);
        const gotItDays = vocabBoxIntervalDays(card.box + 1);
        const revealOrDefinition = flipped
          ? `<div class="vocab-review-definition">${esc(card.definition)}</div>
             ${card.contextSentence ? `<div class="vocab-review-context">“${esc(card.contextSentence)}”</div>` : ""}
             ${card.bookTitle ? `<div class="vocab-review-book">${esc(card.bookTitle)}</div>` : ""}`
          : `<button class="vocab-review-reveal" id="vocab-review-reveal">Reveal</button>`;
        const answers = flipped
          ? `<div class="vocab-review-answers">
              <button class="vocab-review-missed" id="vocab-review-missed" ${submitting ? "disabled" : ""}>
                Missed<span>due in ${vocabDaysLabel(missedDays)}</span>
              </button>
              <button class="vocab-review-gotit" id="vocab-review-gotit" ${submitting ? "disabled" : ""}>
                Got it<span>due in ${vocabDaysLabel(gotItDays)}</span>
              </button>
            </div>`
          : "";
        html = `<div class="vocab-review">
          <div class="vocab-review-progress">${progress} / ${reviewTotal}</div>
          <div class="vocab-review-card">
            <div class="vocab-review-word"><strong>${esc(card.word)}</strong>${card.pos ? ` <span class="vocab-entry-pos">${esc(card.pos)}</span>` : ""}</div>
            ${revealOrDefinition}
          </div>
          ${answers}
          <button class="vocab-review-back" id="vocab-review-back">Back to list</button>
        </div>`;
      }

      container.innerHTML = html;
      const backBtn = container.querySelector("#vocab-review-back");
      if (backBtn) backBtn.onclick = backToList;
      const revealBtn = container.querySelector("#vocab-review-reveal");
      if (revealBtn) revealBtn.onclick = () => { flipped = true; render(); };
      const missedBtn = container.querySelector("#vocab-review-missed");
      if (missedBtn) missedBtn.onclick = () => answerCard(false);
      const gotItBtn = container.querySelector("#vocab-review-gotit");
      if (gotItBtn) gotItBtn.onclick = () => answerCard(true);
    }

    async function answerCard(correct) {
      if (submitting) return;
      const card = reviewQueue[0];
      if (!card) return;
      submitting = true;
      render(); // disables the answer buttons while the request is in flight

      let resp;
      try {
        resp = await apiWrite(`/api/vocabulary/${encodeURIComponent(card.id)}/review`, {
          method: "POST",
          credentials: "same-origin",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ correct }),
        });
      } catch (e) {
        if (stale()) return;
        // Same as above: `stale()` only tracks this screen's own visit token,
        // not a profile move, and refreshDueCount() below would re-read the
        // other library's queue.
        if (reloadPending) return;
        // The outcome is genuinely UNKNOWN here: the request may have
        // committed with only the response lost. Retaining the card for a
        // blind retry would then record the same review twice, advancing the
        // box two boxes for one answer. Re-read the queue from the server
        // instead and say so — if the review did land, the card is simply
        // gone from the refreshed queue.
        submitting = false;
        flipped = false;
        view = "list";
        dueCountPending = true;
        showToast("Couldn't confirm that review — checking");
        render();
        refreshDueCount();
        return;
      }
      // A profile switched on the desktop mid-session invalidates every queued
      // row id; without this the answers just 404 one by one while the page
      // keeps showing the old profile's words. Now covered by every write
      // path via apiWrite, not just this one.
      if (stale()) return;
      if (resp.status === 401) { showLogin(); return; }
      if (resp.status === 403) {
        // Same defense-in-depth as the screen's own load: the setting was
        // switched off mid-session. Not a connection problem, so it must not
        // be reported as one — drop the cache and retire the affordance.
        // Land on the SAME terminal state the screen's own 403 produces.
        // Returning to the list instead rebuilt the full word list and an
        // enabled Review button from data the server will now refuse, so
        // every following action 403'd while the UI looked functional.
        dropVocabularyStatusCache();
        submitting = false;
        showToast("Vocabulary is turned off");
        container.innerHTML = `<div class="empty">Vocabulary is turned off.</div>`;
        return;
      }
      if (resp.status === 404) {
        // The word was deleted (from another tab/device) since the queue was
        // built — nothing to persist a review against. Drop it and move on
        // rather than getting the session stuck on a card that can't advance.
        // Drop it from the list and the due set too, or the row lingers on the
        // list behind this screen until a reload.
        const goneId = card.id;
        words = words.filter((w) => w.id !== goneId);
        dueWords = dueWords.filter((w) => w.id !== goneId);
        reviewQueue = reviewQueue.slice(1);
        flipped = false;
        submitting = false;
        showToast("That word was deleted");
        render();
        return;
      }
      if (!resp.ok) {
        submitting = false;
        showToast(httpErrorToastMessage(resp.status));
        render();
        return;
      }

      reviewQueue = reviewQueue.slice(1);
      flipped = false;
      submitting = false;
      render();
    }

    // Optimistic delete + rollback on failure — same idempotent-204 and 401
    // handling as the drawer's deleteVocabWord, scoped to this screen's own
    // `words` array instead of vocabState.
    async function deleteScreenWord(id) {
      const i = words.findIndex((w) => w.id === id);
      if (i === -1) return;
      const removed = words.splice(i, 1)[0];
      // The due set is a separate array, and leaving it alone meant the bar
      // kept counting a deleted word and startReview() handed the user a
      // flashcard for it — which then 404'd and blamed them for deleting it.
      const dueIndex = dueWords.findIndex((w) => w.id === id);
      const removedDue = dueIndex !== -1 ? dueWords.splice(dueIndex, 1)[0] : null;
      render();
      try {
        const delResp = await apiWrite(`/api/vocabulary/${encodeURIComponent(id)}`, {
          method: "DELETE",
          credentials: "same-origin",
        });
        if (delResp.status === 401) {
          // showLogin() FIRST: the session is gone, and routing to the PIN
          // screen must not be contingent on the local rollback below
          // succeeding — that ordering is what made an expired session during
          // a delete fail silently.
          showLogin();
          words.splice(i, 0, removed);
          if (removedDue) dueWords.splice(dueIndex, 0, removedDue);
          render();
          return;
        }
        // Idempotent DELETE: 204 is success whether or not the row still existed.
        if (!delResp.ok && delResp.status !== 204) throw new Error(`HTTP ${delResp.status}`);
      } catch {
        words.splice(i, 0, removed);
        if (removedDue) dueWords.splice(dueIndex, 0, removedDue);
        render();
        showToast("Couldn't delete saved word");
      }
    }

    render();
  }

  // ── Helpers ───────────────────────────────────
  // Security (finding A): textContent -> innerHTML only escapes text-node
  // metacharacters (&, <, >) — it leaves " and ' untouched, since those are
  // only special in attribute context. Every caller of esc() in this file
  // interpolates its result into either a text node OR a quoted HTML
  // attribute (e.g. `title="${esc(b.title)}"`), so the escaping must be safe
  // for both: without this, a title/author/series/collection name containing
  // `"` could break out of an attribute and inject arbitrary markup/handlers.
  function esc(s) {
    if (!s) return "";
    const d = document.createElement("div");
    d.textContent = s;
    return d.innerHTML.replace(/"/g, "&quot;").replace(/'/g, "&#39;");
  }

  function formatDuration(secs) {
    if (!secs || secs < 60) return "< 1m";
    const h = Math.floor(secs / 3600);
    const m = Math.floor((secs % 3600) / 60);
    if (h === 0) return m + "m";
    return h + "h " + m + "m";
  }

  // Item 6: sun (light) / moon (dark) / half-filled circle (system) — no
  // icon library, inline SVG only, matching the rest of the nav icons.
  function themeIconSvg(mode) {
    if (mode === "light") {
      return '<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M6.34 17.66l-1.41 1.41M19.07 4.93l-1.41 1.41"/></svg>';
    }
    if (mode === "dark") {
      return '<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/></svg>';
    }
    return '<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="M12 3a9 9 0 0 1 0 18z" fill="currentColor" stroke="none"/></svg>';
  }

  // ── Profile switcher (remote profile switching) ────────────────
  // One active profile is shared by the desktop app and every web/OPDS client,
  // so switching here moves all of them. A locked profile is listed but not
  // offered: its password is never accepted over the network, so it is only
  // reachable if it was already unlocked on the desktop this session (the
  // server reports that as `switchable`).
  let cachedProfiles = [];

  /// Fetches the profile list, caching it for the header. Also the single
  /// source for the active profile name (offline scoping reads it too), so a
  /// boot costs one request, not two.
  // Whether the last loadProfiles() actually learned anything. The list alone
  // cannot say: a failure returns the previous cache, so a caller that needs
  // the *current* active profile — offline namespacing does — would otherwise
  // act on stale data believing it fresh.
  //
  // Every non-OK status counts as a failure, 503 included. An earlier version
  // treated 503 as "this server has no profile host, so there is one library
  // and no namespace to get wrong" — but `profile_lock_gate` answers 503 with
  // "Profile locked" too, so that read would mistake a locked multi-profile
  // server for a single-library one. And every production caller builds
  // `WebState` with `profile_host: Some(…)`, so the case it was protecting
  // does not ship.
  let profilesOutcome = "failed"; // "ok" | "failed"

  async function loadProfiles() {
    try {
      const resp = await fetch("/api/profiles", { credentials: "same-origin" });
      noteProfileTag(resp);
      if (!resp.ok) { profilesOutcome = "failed"; return cachedProfiles; }
      const list = await resp.json();
      if (Array.isArray(list)) { cachedProfiles = list; profilesOutcome = "ok"; }
      else profilesOutcome = "failed";
    } catch (e) {
      // Keep whatever we had; the switcher just won't render.
      profilesOutcome = "failed";
    }
    return cachedProfiles;
  }

  function activeProfileName() {
    const active = cachedProfiles.find((p) => p.active);
    return active ? active.name : "";
  }

  const PROFILE_ICON_PATH =
    '<path d="M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2"/><circle cx="12" cy="7" r="4"/>';

  /// Rendered only with more than one profile: with a single profile there is
  /// nothing to switch to, and the control would be pure decoration.
  function profileSwitcherHtml() {
    if (cachedProfiles.length < 2) return "";
    const active = activeProfileName();
    const rows = cachedProfiles
      .map((p) => {
        const unreachable = p.locked && !p.switchable;
        const title = unreachable
          ? "Unlock on the desktop to use over the network"
          : `Switch to ${p.name}`;
        return `<button type="button" class="profile-row" data-profile="${esc(p.name)}"
          ${p.active ? 'aria-current="true"' : ""}${unreachable ? " disabled" : ""}
          title="${esc(title)}">
          <span class="profile-row-name">${esc(p.name)}</span>
          ${p.active ? '<span class="profile-row-flag" aria-hidden="true">&#10003;</span>' : ""}
          ${unreachable ? '<span class="profile-row-flag" aria-hidden="true">&#128274;</span>' : ""}
        </button>`;
      })
      .join("");
    return `<div class="profile-switcher">
      <button class="nav-icon profile-switcher-btn" id="profile-switcher-btn" aria-haspopup="true"
        aria-expanded="false" title="Profile: ${esc(active)}" aria-label="Profile: ${esc(active)}">
        ${navIconSvg(20, PROFILE_ICON_PATH)}<span class="profile-name">${esc(active)}</span>
      </button>
      <div class="profile-panel" id="profile-panel" hidden>${rows}</div>
    </div>`;
  }

  function closeProfilePanel() {
    const panel = $("#profile-panel");
    if (panel) panel.hidden = true;
    const btn = $("#profile-switcher-btn");
    if (btn) btn.setAttribute("aria-expanded", "false");
  }

  function bindProfileSwitcher() {
    const btn = $("#profile-switcher-btn");
    const panel = $("#profile-panel");
    if (!btn || !panel) return;
    btn.onclick = () => {
      const open = panel.hidden;
      closeAllFilterPanels();
      panel.hidden = !open;
      btn.setAttribute("aria-expanded", open ? "true" : "false");
    };
    $$("#profile-panel .profile-row").forEach((row) => {
      row.onclick = () => {
        closeProfilePanel();
        if (!row.disabled) switchProfile(row.dataset.profile);
      };
    });
  }

  async function switchProfile(name) {
    if (!name || name === activeProfileName()) return;
    let resp;
    try {
      // Deliberately NOT apiWrite: this call's whole purpose is to move the
      // active profile, so its response necessarily carries a different
      // x-carrel-profile tag than the page's baseline. Routing it through
      // noteProfileTag would fire location.reload() right here, racing the
      // location.hash reset below and dropping the user back wherever they
      // were instead of on the library.
      resp = await fetch("/api/profile", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "same-origin",
        body: JSON.stringify({ name }),
      });
    } catch (e) {
      showToast("Couldn't reach Carrel server");
      return;
    }
    if (resp.status === 401) { authenticated = false; showLogin(); return; }
    if (resp.status === 423) {
      // The profile was locked (or re-locked) since the list was fetched. The
      // password is never accepted here, so the only route in is the desktop.
      showToast("That profile is locked — unlock it on the desktop to use it over the network");
      await loadProfiles();
      return;
    }
    if (!resp.ok) { showToast(httpErrorToastMessage(resp.status)); return; }
    // Everything on screen belongs to the previous profile — books, ids,
    // collections, series, progress, offline scope. Reload into the new
    // profile's library rather than trying to invalidate each cache: the
    // boot path already re-fetches all of it and re-scopes offline storage.
    // Same meaning as in noteProfileTag: writes still in flight will fail
    // during teardown and must not toast about it.
    reloadPending = true;
    location.hash = "#/";
    location.reload();
  }

  function themeToggleHtml() {
    const label = themeAriaLabel();
    return `<button class="nav-icon" id="theme-toggle-btn" title="${esc(label)}" aria-label="${esc(label)}">${themeIconSvg(themeMode)}</button>`;
  }

  // Item 6: light -> dark -> system -> light. "system" removes data-theme
  // entirely so the CSS prefers-color-scheme block takes back over.
  function cycleTheme() {
    themeMode = themeMode === "light" ? "dark" : themeMode === "dark" ? "system" : "light";
    safeStorageSet(THEME_STORAGE_KEY, themeMode);
    applyTheme();
    const btn = $("#theme-toggle-btn");
    if (btn) btn.innerHTML = themeIconSvg(themeMode);
    updateThemeButtonLabel();
  }

  // Feather-style icon path geometry shared by the header nav cluster
  // (navIconsHtml) and the bottom tab bar (tabBarHtml) so the two glyphs for a
  // destination never drift. Only the `d` data is shared — the wrappers differ
  // in size (20px header vs 22px tab).
  const NAV_ICON_PATH = {
    collections: '<path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/>',
    stats: '<path d="M18 20V10M12 20V4M6 20v-6"/>',
    library: '<path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20"/><path d="M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2z"/>',
    // M5: Feather "book-open", distinct from the closed-book `library` glyph.
    vocabulary: '<path d="M2 3h6a4 4 0 0 1 4 4v14a3 3 0 0 0-3-3H2z"/><path d="M22 3h-6a4 4 0 0 0-4 4v14a3 3 0 0 1 3-3h7z"/>',
  };
  const navIconSvg = (size, inner) =>
    `<svg width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${inner}</svg>`;

  function navIconsHtml(activePage) {
    const folderColor = activePage === "collections" ? "active" : "";
    const chartColor = activePage === "stats" ? "active" : "";
    const vocabColor = activePage === "vocabulary" ? "active" : "";
    return `<div class="nav-icons">
      ${profileSwitcherHtml()}
      ${themeToggleHtml()}
      <button class="nav-icon ${folderColor}" title="Collections" aria-label="Collections" data-nav="collections">
        ${navIconSvg(20, NAV_ICON_PATH.collections)}
      </button>
      <button class="nav-icon ${chartColor}" title="Reading Stats" aria-label="Reading Stats" data-nav="stats">
        ${navIconSvg(20, NAV_ICON_PATH.stats)}
      </button>
      <button class="nav-icon ${vocabColor}" title="Vocabulary" aria-label="Vocabulary" data-nav="vocabulary"${vocabularyAvailable() ? "" : " hidden"}>
        ${navIconSvg(20, NAV_ICON_PATH.vocabulary)}
      </button>
    </div>`;
  }

  function bindNavIcons() {
    $$("[data-nav]").forEach(btn => {
      btn.onclick = () => navigate("#/" + btn.dataset.nav);
    });
    const themeBtn = $("#theme-toggle-btn");
    if (themeBtn) themeBtn.onclick = cycleTheme;
    bindProfileSwitcher();
  }

  // Item A (app-feel Tier 1): fixed bottom tab bar for primary navigation on
  // narrow/touch viewports. Rendered by the top-level views
  // (library/collections/stats/vocabulary, M5) — not the reader (immersive)
  // or login. On desktop it is CSS-hidden and the header icon cluster is used
  // instead (the two are mutually exclusive via a media query in app.css).
  // SVGs mirror the header cluster's (folder = collections, bar-chart =
  // stats, open book = vocabulary); Library gets a closed-book glyph the
  // cluster never had.
  function tabBarHtml(activePage) {
    const tab = (page, label, svg, hidden) => {
      const active = activePage === page;
      return `<button class="tab ${active ? "active" : ""}" data-tab="${page}" aria-label="${label}"${active ? ' aria-current="page"' : ""}${hidden ? " hidden" : ""}>
        ${svg}
        <span class="tab-label">${label}</span>
      </button>`;
    };
    return `<nav class="tab-bar" aria-label="Primary">
      ${tab("library", "Library", navIconSvg(22, NAV_ICON_PATH.library))}
      ${tab("collections", "Collections", navIconSvg(22, NAV_ICON_PATH.collections))}
      ${tab("stats", "Stats", navIconSvg(22, NAV_ICON_PATH.stats))}
      ${tab("vocabulary", "Words", navIconSvg(22, NAV_ICON_PATH.vocabulary), !vocabularyAvailable())}
    </nav>`;
  }

  function bindTabBar() {
    $$(".tab-bar .tab").forEach(t => {
      t.onclick = () => navigate(t.dataset.tab === "library" ? "#/" : "#/" + t.dataset.tab);
    });
  }

  // Item 9 (PWA): feature-detected registration — service workers only
  // register on a secure context (https, or http://localhost). Carrel's main
  // LAN use case (a phone hitting http://192.168.x.x:7788) is plain HTTP and
  // NOT a secure context, so `serviceWorker` won't even exist in `navigator`
  // there and this silently no-ops. The manifest + icons still work for iOS
  // Safari's "Add to Home Screen" over plain HTTP.
  if ("serviceWorker" in navigator) {
    try {
      navigator.serviceWorker.register("/sw.js").catch(() => {});
    } catch (e) { /* best-effort */ }
  }

  // Finding 7: create the toast live region immediately, before anything
  // else has a chance to need it — see ensureToastContainer()'s comment.
  ensureToastContainer();

  // Finding 2: an offline/unreachable-server launch (the whole point of the
  // service worker caching the shell above) with no friendly fallback here
  // — reuses the resume-prompt's centered-panel markup/classes rather than
  // introducing new CSS for a one-off screen.
  function renderOfflineState() {
    app().innerHTML = `
      <div class="resume-prompt">
        <div class="resume-prompt-panel">
          <h2>Carrel</h2>
          <p>Couldn&rsquo;t reach the Carrel server. Check your connection and try again.</p>
          <div class="resume-actions">
            <button class="btn-primary" id="retry-init-btn">Retry</button>
          </div>
        </div>
      </div>`;
    const btn = $("#retry-init-btn");
    if (btn) btn.onclick = init;
  }

  // True while the app is running against offline storage (server
  // unreachable at boot but saved books exist). route() restricts navigation
  // to the offline library + saved-book detail/reader while this holds; a
  // successful init() clears it.
  let offlineMode = false;

  // The offline library: the normal grid, fed from IndexedDB manifest rows
  // (newest save first), with a banner + Retry. Covers and content load
  // through the M2 service-worker cache fallback. No login gate offline.
  function renderOfflineLibrary(rows) {
    offlineMode = true;
    currentView = "library";
    document.title = "Carrel";
    offlineBookIds = new Set(rows.map((r) => r.id));
    const books = rows
      .slice()
      .sort((a, b) => (b.savedAt || 0) - (a.savedAt || 0))
      .map((r) => ({
        id: r.id,
        title: r.title,
        author: r.author,
        format: r.format,
        total_chapters: r.totalChapters,
      }));
    app().innerHTML = `
      <div class="header"><h1>Carrel</h1></div>
      <div class="offline-banner" role="status">
        <span>Offline — showing downloaded books</span>
        <button class="btn-secondary" id="offline-retry-btn">Retry</button>
      </div>
      <div id="library-content">${gridHtml(books)}</div>`;
    bindGridCardHandlers();
    const retry = $("#offline-retry-btn");
    if (retry) retry.onclick = init;
  }

  // The offline-library entry point used by the route guard. It first
  // re-probes the network: if the server is reachable again, it leaves
  // offline mode and routes normally — so ANY navigation (not just the
  // banner's Retry) recovers the full library/search/stats/collections once
  // connectivity returns. Still offline → render from manifests, or the
  // plain offline card if nothing is saved.
  async function showOfflineLibrary(gen) {
    // gen ties this async render to the route() call that triggered it; if a
    // newer navigation (or a completed reconnect) ran during an await below,
    // bail so we never paint a stale offline library over the newer view.
    const superseded = () => typeof gen === "number" && gen !== routeGen;
    let test;
    try {
      test = await fetch("/api/books", { credentials: "same-origin" });
    } catch (e) {
      if (superseded()) return;
      const rows = await getAllOfflineManifests();
      if (superseded()) return;
      if (rows.length) renderOfflineLibrary(rows);
      else { offlineMode = false; renderOfflineState(); }
      return;
    }
    if (superseded()) return;
    // Leaving offline mode is a boot in miniature, and it was the only server
    // probe that skipped these two: without the tag check this page adopts a
    // profile move instead of noticing it, and without the re-scope it then
    // renders the new profile's library while every offline read and write
    // still points at the old profile's namespace — so a "Save offline" from
    // that state files the new profile's book under the old one.
    if (noteProfileTag(test)) return; // the reload will re-run this properly
    if (test.status === 401) { offlineMode = false; return showLogin(); }
    authenticated = true;
    // Re-scope BEFORE leaving offline mode. `hashchange` still fires during
    // these awaits, and route()'s offline branch is where that navigation
    // belongs until the new namespace is in place — clearing the flag first
    // would let an interleaved render read the *old* profile's namespace,
    // which routeGen can't prevent because that render isn't this
    // continuation.
    const scope = await syncOfflineScopeWithServer();
    if (scope === "unknown") {
      // Fail closed. Leaving offline mode here would permit saves under a
      // namespace nothing has confirmed matches the server's active profile —
      // and this is the one path where the tag check above cannot cover for
      // that, because a page that booted offline has no baseline tag yet, so
      // the probe *establishes* the baseline instead of catching a move.
      // (init() and doLogin() tolerate "unknown" for that reason: by the time
      // they run, their probe either set the baseline at a cold boot or would
      // have caught the move.)
      //
      // Staying offline keeps the banner and its Retry — which re-runs this
      // whole probe — on screen, which is the recoverable end of the trade.
      // Rendered rather than just returned, so a navigation that lands here
      // does not change the URL and leave the previous view up with no word
      // of why.
      const rows = await getAllOfflineManifests();
      if (superseded()) return;
      if (rows.length) renderOfflineLibrary(rows);
      else renderOfflineState();
      showToast("Couldn't confirm which library is active — staying offline");
      return;
    }
    if (scope === "changed") await verifyOfflineIntegrity();
    // loadProfiles() inside the sync above is itself a request, so the move
    // can be noticed *there* rather than on the probe — in which case the
    // scope has already been moved to the profile the imminent reload lands
    // in, which is where boot would put it anyway. Nothing below should run.
    if (superseded() || reloadPending) return;
    offlineMode = false;
    route();
  }

  // ── Init ──────────────────────────────────────
  async function init() {
    // Adopt the last published profile scope before ANY offline read — every
    // manifest/cache lookup below is namespaced by it. Read from storage, not
    // the server, so an offline launch still uses the right profile's
    // downloads; the online path re-syncs it against the server below.
    await loadPublishedOfflineScope();
    // Eviction honesty first: make the manifest truthful before either boot
    // path reads it (the offline branch renders straight from it, and the
    // online grid badges read offlineBookIds). Safe on- or offline.
    await verifyOfflineIntegrity();
    // Finding 2: this initial probe is a raw fetch (not api(), which would
    // itself call showLogin() on 401 before `authenticated` is known here)
    // — but it needs the same guard: an unhandled rejection (offline PWA
    // launch, server not up yet) would otherwise abort this whole IIFE and
    // leave #app permanently blank, since nothing else initializes the page.
    let test;
    try {
      test = await fetch("/api/books", { credentials: "same-origin" });
    } catch (e) {
      // Server unreachable. If any books are saved offline, boot into the
      // offline library instead of the dead-end card — no auth gate (locked
      // spec decision: the content is already on this device).
      if (offlineSupported()) {
        try {
          const saved = await getAllOfflineManifests();
          if (saved.length) {
            offlineMode = true;
            authenticated = true;
            offlineBookIds = new Set(saved.map((r) => r.id));
            // Honor the boot hash: a saved book's URL deep-links straight to
            // its (SW-served) detail/reader offline (route()'s guard lets a
            // saved-book hash through); any top-level hash renders the
            // offline library directly from the rows we just read (no second
            // manifest round-trip).
            if (rawHash().startsWith("#/book/")) route();
            else renderOfflineLibrary(saved);
            return;
          }
        } catch (err) { /* fall through to the plain offline card */ }
      }
      renderOfflineState();
      return;
    }
    offlineMode = false;
    // Establishes this page's profile baseline (see `noteProfileTag`) before
    // anything else talks to the server — the boot probe is a raw fetch, so
    // without this the first tag recorded would be a post-switch one and the
    // page would adopt the move instead of noticing it.
    noteProfileTag(test);
    if (test.status === 401) { showLogin(); return; }
    authenticated = true;
    // The active profile may have moved since this browser last looked (from
    // the desktop, or another web client — one active profile is shared by
    // all of them). Re-scope before anything reads offline storage, and
    // re-check integrity in the new namespace if it changed.
    const bootScope = await syncOfflineScopeWithServer();
    if (bootScope === "unknown") {
      // Fail closed for offline only. The namespace every offline read, write
      // and queue replay uses belongs to a profile, and this page could not
      // confirm which profile is active — the profile tag says the server's
      // answers are self-consistent, not which name the stored namespace was
      // built from. `offlineUnavailable` gates all of that through
      // offlineSupported() (replayProgressQueue included), so online browsing
      // carries on unaffected. Cleared as soon as a profile request succeeds.
      offlineProfileUnknown = true;
    } else if (bootScope === "changed") {
      await verifyOfflineIntegrity();
    }
    // Reconnected (or a normal online launch): flush any progress queued
    // while offline. Fire-and-forget — it serializes per book via saveChains
    // and never blocks the first render.
    replayProgressQueue();
    // Reconcile saved books against the live library (deleted → removed,
    // metadata/cover → refreshed). Only off a genuinely OK probe — a non-401
    // error (e.g. 500) is not a trustworthy library list and must not drive
    // deletions. Fire-and-forget off the probe's own body (nothing else
    // consumes `test`); never blocks the first render.
    if (test.ok) {
      test.clone().json().then((books) => reconcileOfflineBooks(books)).catch(() => {});
    }
    route();
  }

  // Reconnect signal: the browser fires `online` when connectivity returns —
  // replay the offline progress queue without waiting for a navigation.
  window.addEventListener("online", async () => {
    // If boot could not establish the active profile, offline is switched off
    // and the queue is not replayable — and a dropped connection at boot is
    // exactly how that happens. Reconnecting is the natural moment to ask
    // again; a successful answer re-scopes and lifts the flag, and until one
    // arrives replayProgressQueue() is a no-op anyway.
    if (offlineProfileUnknown) {
      const scope = await syncOfflineScopeWithServer();
      if (scope === "changed") await verifyOfflineIntegrity();
    }
    replayProgressQueue();
  });

  init();
})();
