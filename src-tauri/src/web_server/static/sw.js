// Item 9 (PWA): minimal service worker — cache-first for the static app
// shell only. Reading content and API data are never cached (freshness/auth
// complexity isn't worth it for v1; see docs/web-ui-improvements.md Item 9).
//
// Finding 9: CACHE_VERSION must embed a short content hash of the shell
// assets (index.html + app.js + app.css + manifest.json) — enforced by
// `cache_version_embeds_shell_asset_content_hash` in mod.rs, which fails CI
// with the expected hash whenever one of those files changes without this
// being regenerated to match. Don't hand-edit the version without re-running
// that test.
//
// SHELL_ASSETS mirrors `web_ui::PUBLIC_SHELL_ASSETS` in web_ui.rs (Finding
// 11) — that's the source of truth for which paths are public/unauthed on
// the Rust side; keep this array in sync with it by hand (a JS file can't
// import a Rust const), plus `/favicon.ico` which isn't a distinct shell
// asset worth precaching separately from `/favicon.png`.
//
// Secure-context caveat: service workers only register on secure contexts
// (https, or http://localhost). Carrel's primary LAN use case — a phone
// hitting http://192.168.x.x:7788 — is plain HTTP and NOT a secure context,
// so the service worker will not register there and this cache never
// activates; app.js's registration call is feature-detected/try-catched so
// this is silent, not an error. The manifest + icons still work for iOS
// Safari "Add to Home Screen" over plain HTTP.
//
// The `carrel-` prefix on this and the two cache names below names caches held
// in the user's browser: renaming it orphans every offline-saved book rather
// than migrating it. Only the trailing content hash is meant to change.
const CACHE_VERSION = "carrel-shell-01caba844b16";

// Offline mode (spec 2026-07-17-web-reader-offline): per-book content caches,
// written ONLY by app.js's save flow — the SW never writes to them. The SW
// reads them as a fallback when the network is unreachable. Network-first, so
// online behavior (auth, session expiry, profile lock, full-size pages) is
// exactly what the server says, always.
const OFFLINE_CACHE_PREFIX = "carrel-offline-book-";

// Profile scoping (remote profile switching, PR #119): book
// ids are per-profile id spaces, so offline caches are namespaced by a short
// hash of the active profile name — "" for the default profile, which keeps the
// names offline mode shipped with. app.js owns the token and publishes it to a
// one-entry cache; this worker only reads it. Both constants must mirror app.js.
//
// Read per request rather than memoized: a worker outlives page loads and
// profile switches, so a cached value could serve one profile's saved book
// under another. The read is a local Cache Storage lookup on a one-entry cache,
// and only on book routes — cheap next to the cache match it precedes.
const OFFLINE_SCOPE_CACHE = "carrel-offline-scope";
const OFFLINE_SCOPE_URL = "/__offline_scope";

// Returns the active profile's namespace, or `null` when it can't be
// established. `null` means "don't serve offline content": an absent or
// unreadable marker is NOT the default profile — treating it as "" would answer
// another profile's book ids out of the default profile's caches, which is the
// exact confusion the namespace exists to prevent. app.js drops the marker when
// it cannot publish one, so this is also how a fail-closed page switches the
// worker off.
async function currentOfflineScope() {
  try {
    const resp = await (await caches.open(OFFLINE_SCOPE_CACHE)).match(OFFLINE_SCOPE_URL);
    return resp ? await resp.text() : null;
  } catch (e) {
    return null;
  }
}

const SHELL_ASSETS = [
  "/",
  "/app.js",
  "/app.css",
  "/favicon.png",
  "/manifest.json",
  "/icon-192.png",
  "/icon-512.png",
  // Reader typography fonts — kept in lockstep with web_ui.rs FONT_ASSETS by
  // mod::tests::font_assets_are_public_and_precached. cache.addAll() is atomic,
  // so a missing/renamed entry here breaks the whole SW install.
  "/fonts/lora-latin-normal-ddb8c6603510.woff2",
  "/fonts/lora-latin-italic-d824d807d4d8.woff2",
  "/fonts/lora-latinext-normal-2a2d9c22c986.woff2",
  "/fonts/lora-latinext-italic-45f989df83f3.woff2",
  "/fonts/literata-latin-normal-9adbeac5b167.woff2",
  "/fonts/literata-latin-italic-ab198d6616c7.woff2",
  "/fonts/literata-latinext-normal-46792f7cd10b.woff2",
  "/fonts/literata-latinext-italic-ba0e6a12e2f0.woff2",
  "/fonts/dmsans-latin-normal-9fea608a947e.woff2",
  "/fonts/dmsans-latin-italic-f1a235db5bcb.woff2",
  "/fonts/dmsans-latinext-normal-a5d38fe99f93.woff2",
  "/fonts/dmsans-latinext-italic-6e646d202280.woff2",
  "/fonts/opendyslexic-regular-f007004af3cd.woff2",
  "/fonts/opendyslexic-bold-dd9fa9c79911.woff2",
  "/fonts/opendyslexic-italic-eb6a1bacf7e7.woff2",
  "/fonts/opendyslexic-bolditalic-a20d82c2a1a0.woff2",
];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches.open(CACHE_VERSION).then(async (cache) => {
      // Finding 8: web_ui.rs serves these with `Cache-Control: public,
      // max-age=3600` — a plain `cache.addAll(...)` lets the browser's own
      // HTTP cache satisfy the fetch, which can populate a freshly-versioned
      // SW cache with an hour-old copy of app.js/app.css right after a deploy.
      // `cache: "reload"` forces each request past the HTTP cache.
      const reload = (url) => new Request(url, { cache: "reload" });
      // Core shell (the app itself) is precached ATOMICALLY — install fails and
      // retries if any of these can't be fetched. Reader fonts are precached
      // BEST-EFFORT: cache.addAll is all-or-nothing, so folding 15+ font files
      // into it would let one flaky font fetch abort the whole SW update and
      // strand the user on the stale core shell. Fonts are still in
      // SHELL_ASSETS, so a best-effort miss is repopulated cache-first by the
      // fetch handler on the first request for that face.
      const fonts = SHELL_ASSETS.filter((url) => url.startsWith("/fonts/"));
      const core = SHELL_ASSETS.filter((url) => !url.startsWith("/fonts/"));
      await cache.addAll(core.map(reload));
      await Promise.allSettled(fonts.map((url) => cache.add(reload(url))));
    })
  );
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches.keys().then((keys) =>
      Promise.all(
        keys
          // Shell-version cleanup only — offline book caches are owned by
          // app.js (saved/deleted there) and MUST survive every SW update,
          // or each shell deploy would silently wipe the user's downloads.
          .filter(
            (key) =>
              key !== CACHE_VERSION &&
              key !== OFFLINE_SCOPE_CACHE &&
              !key.startsWith(OFFLINE_CACHE_PREFIX)
          )
          .map((key) => caches.delete(key))
      )
    )
  );
  self.clients.claim();
});

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);

  // Saved-book offline fallback: network-first. Every HTTP response —
  // including 401/503 — is returned as-is, so online auth semantics
  // (session expiry, profile lock, api()'s 401→login) are untouched. The
  // cache answers only when fetch() itself rejects (server unreachable);
  // a fallback miss lets the rejection propagate so app.js handles it
  // exactly as it does today. The book-list route (/api/books, no id
  // segment) never matches — the offline library renders from IndexedDB.
  // The bare /api/books/{id} detail route MUST match (no trailing slash):
  // the reader and detail view fetch it, and the saved inventory caches it.
  // /download is deliberately excluded: whole-file downloads are never in
  // the offline inventory, and piping a multi-hundred-MB stream through
  // respondWith would subject it to SW-lifetime termination mid-download.
  const bookMatch =
    event.request.method === "GET" &&
    url.origin === self.location.origin &&
    !url.pathname.includes("/download") &&
    // Anchor to the endpoint segment after the book id, so a book whose id
    // merely contains "bookmarks" (e.g. /api/books/bookmarks-x/chapters/0)
    // still gets the offline fallback — only the real bookmarks/highlights
    // endpoints are excluded (live-only, never cached — annotations must
    // never be served stale).
    !/^\/api\/books\/[^/]+\/(?:bookmarks|highlights)(?:\/|$)/.test(url.pathname) &&
    url.pathname.match(/^\/api\/books\/([^/]+)(?:\/|$)/);
  if (bookMatch) {
    // Page images are cached with ?width=... but requested plain (or with a
    // ?__reload= retry nonce) — match ignoring the query for those URLs
    // ONLY. Everything else must match exactly (/cover vs /cover?size=thumb
    // are distinct entries). caches.match with a cacheName returns undefined
    // for a nonexistent cache without creating it, so unsaved books just
    // propagate the network outcome.
    const isPage = /^\/api\/books\/[^/]+\/pages\/\d+$/.test(url.pathname);
    event.respondWith((async () => {
      // Is this exact request in the book's offline cache, under the active
      // profile's namespace? A book id saved under another profile lives in a
      // differently-named cache and must never answer here.
      const scope = await currentOfflineScope();
      // Namespace unknown → behave as if nothing were saved: plain network.
      if (scope === null) return fetch(event.request);
      const matchOpts = {
        cacheName: OFFLINE_CACHE_PREFIX + scope + bookMatch[1],
        ignoreSearch: isPage,
      };
      const cached = await caches.match(event.request, matchOpts);

      // No cache to fall back to — the ONLY correct behavior is a plain
      // network fetch that runs to its natural success or failure. This is
      // the common online case (reading a book that isn't saved offline);
      // fabricating any timeout/error here would turn a merely-slow request
      // (routine over a phone's Tailscale/VPN link) into a bogus "couldn't
      // reach server". No cache also means nothing to serve offline, so a
      // genuine failure propagates normally.
      if (!cached) return fetch(event.request);

      // A cached copy EXISTS (a saved book). If the browser reports offline,
      // don't even try the network — serve the cache instantly.
      if (!self.navigator.onLine) return cached;

      // Online with a cached copy: network-FIRST so auth/session/freshness
      // come from the server when it answers, but never let saved content be
      // held hostage to a hung/dead-but-"online" network. Race the network
      // against a short timeout; on timeout OR outright failure, fall back to
      // the cache we already have. (Book content is immutable per id, so
      // serving the cached copy on a slow network is safe.) The timeout only
      // ever costs a fresh re-validation of an already-downloaded book — it
      // can never manufacture an error, because we only reach here holding a
      // cache to return.
      const netPromise = fetch(event.request);
      netPromise.catch(() => {}); // may be abandoned — don't leak an unhandled rejection
      const raced = await Promise.race([
        netPromise.then((response) => ({ response })).catch(() => ({ failed: true })),
        new Promise((resolve) => setTimeout(() => resolve({ timedOut: true }), 2500)),
      ]);
      return raced.response || cached;
    })());
    return;
  }

  // Network-only passthrough for all other API and OPDS traffic — do not
  // intercept. Leaving these un-handled lets the browser handle them
  // natively so auth (cookies/session) and streaming semantics are
  // untouched.
  if (url.pathname.startsWith("/api/") || url.pathname.startsWith("/opds/")) {
    return;
  }

  // Only handle GETs to our own origin's shell assets; ignore everything else.
  if (event.request.method !== "GET" || url.origin !== self.location.origin) {
    return;
  }

  if (!SHELL_ASSETS.includes(url.pathname)) {
    return;
  }

  // Cache-first, falling back to network (and re-populating the cache).
  event.respondWith(
    caches.match(event.request).then((cached) => {
      if (cached) return cached;
      // Finding 8: same HTTP-cache concern as the install handler above —
      // this fallback only runs on a cache miss (a shell asset not covered
      // by SHELL_ASSETS/install, or a request racing an in-progress
      // install), but it must still bypass a possibly stale HTTP cache entry
      // rather than trust it into the SW cache.
      return fetch(event.request, { cache: "no-cache" }).then((resp) => {
        if (resp && resp.ok) {
          const clone = resp.clone();
          caches.open(CACHE_VERSION).then((cache) => cache.put(event.request, clone));
        }
        return resp;
      });
    })
  );
});
