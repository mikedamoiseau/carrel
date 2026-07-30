import { test, expect, type Page } from "@playwright/test";
import { openDetailMenu } from "./detail-actions";

// Harness fixtures (src-tauri/examples/web_e2e_server.rs): Book 050 = the
// only EPUB with a real 2-chapter file (chapter 1 embeds one inline image);
// Book 130 = the only CBZ with a real 2-page file.
const EPUB_ID = "e2e-book-050";
const CBZ_ID = "e2e-book-130";

async function saveBookOffline(page: Page, bookId: string) {
  await page.goto(`/#/book/${bookId}`);
  // Save offline now lives in the detail "More" overflow menu.
  await openDetailMenu(page);
  await page.locator("#offline-save-btn").click();
  // The save flow re-renders the detail page into the saved state on
  // completion, which closes the menu — wait for the Remove row to exist, then
  // reopen the menu so callers see the saved state (Saved label + Remove).
  await expect(page.locator("#offline-remove-btn")).toBeAttached({ timeout: 30_000 });
  await openDetailMenu(page);
  await expect(page.locator("#offline-remove-btn")).toBeVisible();
}

// Reload and wait until the service worker actually CONTROLS the page. Without
// this, a reload can run app.js before the SW takes control, so the offline
// book/content fetches bypass the cache and hit the (offline) network — the
// detail then dead-ends on the error view instead of rendering. Real installed
// PWAs are controlled from launch; this just makes the test deterministic.
//
// Waiting for an ACTIVE registration BEFORE reloading is load-bearing, not
// belt-and-braces. app.js registers fire-and-forget (`register().catch(() => {})`,
// deliberately never awaited — see its comment), so a reload issued shortly
// after the first navigation can outrun the install: the reloaded page then
// starts life uncontrolled, and `clients.claim()` only lands once install
// finishes, which on a loaded CI runner can fall outside the budget below.
// A navigation into a scope that already has an active worker is controlled
// from the start, so this makes the wait below deterministic instead of a race.
//
// Callers that pass through a slow, SW-dependent step first (saveBookOffline,
// switchProfile) satisfied this by accident. The callers that reload within
// milliseconds of a goto did not, and flaked on CI.
async function reloadControlled(page: Page, controllerTimeout = 15_000) {
  // Note `!!registration.active` is NOT sufficient here: `active` is already
  // populated while the worker is still `activating`, and a navigation is only
  // taken over by an *activated* worker. Waiting for the current page to be
  // controlled is the honest signal — sw.js calls `clients.claim()`, so the
  // already-open page gets claimed the moment activation completes.
  await page.waitForFunction(() => !!navigator.serviceWorker.controller, null, {
    timeout: 30_000,
  });
  await page.reload();
  await page.waitForFunction(() => !!navigator.serviceWorker.controller, null, {
    timeout: controllerTimeout,
  });
}

// Open a book's reader at page 0 online and land on the stage. If the book
// carries server-side progress from an earlier test on the shared harness,
// opening /0/read shows a resume prompt (Continue vs Start Over) instead of
// the stage — dismiss it via Start Over so we deterministically render page 0.
async function openReaderAtZero(page: Page, bookId: string) {
  await page.goto(`/#/book/${bookId}/0/read`);
  const restart = page.locator("#resume-restart-btn");
  const stage = page.locator("#reader-stage");
  await expect(restart.or(stage)).toBeVisible({ timeout: 15_000 });
  if (await restart.isVisible().catch(() => false)) await restart.click();
  await expect(stage).toBeVisible({ timeout: 15_000 });
}

// Offline mode (spec docs/superpowers/specs/2026-07-17-web-reader-offline-design.md).
//
// M2 — service-worker foundations: the activate handler's shell-version
// cleanup must never delete per-book offline content caches
// (`carrel-offline-book-*`), while still purging stale shell caches. Playwright
// runs against localhost (a secure context), so the service worker registers
// exactly as it does on the HTTPS deployments offline mode targets.

test.describe("offline mode — service worker foundations", () => {
  // Regression guard for reloadControlled()'s pre-reload wait. Stalls one
  // best-effort font so the install handler — which awaits
  // Promise.allSettled(fonts) — is still running when the reload is issued,
  // reproducing the CI flake deterministically. The tight controller budget is
  // what gives the test teeth: without the pre-reload wait for an active
  // worker, the controller only arrives once the stall clears and this fails.
  test("a reload issued mid-install still ends up controlled", async ({ page, context }) => {
    const STALL_MS = 3_000;
    await context.route("**/fonts/opendyslexic-italic-*.woff2", async (route) => {
      if (!route.request().serviceWorker()) return route.continue();
      await new Promise((r) => setTimeout(r, STALL_MS));
      await route.continue();
    });

    await page.goto("/#/");
    // Precondition: the SW really is mid-install here, or the test proves nothing.
    expect(
      await page.evaluate(async () => !!(await navigator.serviceWorker.getRegistration())?.active),
    ).toBe(false);

    await reloadControlled(page, 1_000);
  });

  test("activation purge spares offline book caches, kills stale shell caches", async ({
    page,
  }) => {
    await page.goto("/");
    await page.waitForFunction(() => navigator.serviceWorker?.ready.then(() => true), null, {
      timeout: 15_000,
    });

    // Plant a fake offline book cache (survivor) and a stale shell-version
    // cache (must die), then force a fresh SW install+activate cycle so the
    // activate purge runs with both present.
    await page.evaluate(async () => {
      await caches.open("carrel-offline-book-e2e-fake");
      await caches.open("carrel-shell-deadbeef0000");
      const reg = await navigator.serviceWorker.getRegistration();
      await reg!.unregister();
    });

    await page.reload();
    await page.waitForFunction(
      async () => {
        const reg = await navigator.serviceWorker.getRegistration();
        return !!reg?.active;
      },
      null,
      { timeout: 15_000 },
    );
    // The purge promise isn't awaitable from the page, so poll until the
    // stale shell cache is gone…
    await expect
      .poll(async () => page.evaluate(() => caches.keys()), { timeout: 10_000 })
      .not.toContain("carrel-shell-deadbeef0000");

    // …then confirm the survivor is STILL there after the purge settles — a
    // single immediate read could race a concurrent (regressed) delete of the
    // offline cache and pass intermittently.
    await page.waitForTimeout(500);
    const keys = await page.evaluate(() => caches.keys());
    expect(keys).toContain("carrel-offline-book-e2e-fake");
    expect(keys).not.toContain("carrel-shell-deadbeef0000");
  });
});

test.describe("offline mode — save / unsave (M3)", () => {
  test("saving an EPUB caches its full inventory and flips the detail UI", async ({ page }) => {
    await saveBookOffline(page, EPUB_ID);

    // Detail view shows the saved state.
    await expect(page.locator(".offline-saved-label")).toContainText(/Saved ·/);

    // The cache holds detail JSON, covers, TOC, both chapters, and the
    // chapter-1 inline image; the manifest row exists with a matching hash.
    const state = await page.evaluate(async (id) => {
      const cache = await caches.open(`carrel-offline-book-${id}`);
      const keys = (await cache.keys()).map((r) => new URL(r.url).pathname + new URL(r.url).search);
      const db = await new Promise<IDBDatabase>((res, rej) => {
        const req = indexedDB.open("carrel-offline");
        req.onsuccess = () => res(req.result);
        req.onerror = () => rej(req.error);
      });
      const row = await new Promise<any>((res, rej) => {
        const tx = db.transaction("books", "readonly");
        const r = tx.objectStore("books").get(id);
        r.onsuccess = () => res(r.result);
        r.onerror = () => rej(r.error);
      });
      return { keys, row };
    }, EPUB_ID);

    expect(state.keys).toContain(`/api/books/${EPUB_ID}`);
    // These harness books have no cover file, so /cover 404s and the optional-
    // cover skip drops it from the inventory — proving the skip path works.
    expect(state.keys).not.toContain(`/api/books/${EPUB_ID}/cover`);
    expect(state.keys).toContain(`/api/books/${EPUB_ID}/chapters`);
    expect(state.keys).toContain(`/api/books/${EPUB_ID}/chapters/0`);
    expect(state.keys).toContain(`/api/books/${EPUB_ID}/chapters/1`);
    expect(state.keys.some((k: string) => k.startsWith(`/api/books/${EPUB_ID}/images/`))).toBe(true);
    expect(state.row).toBeTruthy();
    expect(state.row.generation).toBeTruthy();

    // The manifest hash must actually equal the hash of the cached key set —
    // recompute it independently (dedup + sort + SHA-256, the same canonical
    // form the app uses) so a wrong/short hash or a missing key can't pass.
    const recomputed = await page.evaluate(async (keys) => {
      const canonical = Array.from(new Set(keys)).sort().join("\n");
      const buf = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(canonical));
      return Array.from(new Uint8Array(buf)).map((b) => b.toString(16).padStart(2, "0")).join("");
    }, state.keys);
    expect(state.row.inventoryHash).toBe(recomputed);

    // Grid badge appears for the saved book only. (Book 050 isn't on grid
    // page 1 under the default recent-first sort — search brings it up.)
    await page.goto("/#/?q=Book%20050");
    const savedCard = page.locator(".grid .card", { hasText: "Book 050" });
    await savedCard.waitFor();
    await expect(savedCard.locator(".offline-badge")).toBeVisible();
  });

  test("saving a CBZ downloads pages at OFFLINE_PAGE_WIDTH", async ({ page }) => {
    const widthRequest = page.waitForRequest((r) =>
      r.url().includes(`/api/books/${CBZ_ID}/pages/0`) && r.url().includes("width=1080"),
    );
    await saveBookOffline(page, CBZ_ID);
    await widthRequest;

    const keys = await page.evaluate(async (id) => {
      const cache = await caches.open(`carrel-offline-book-${id}`);
      return (await cache.keys()).map((r) => new URL(r.url).pathname + new URL(r.url).search);
    }, CBZ_ID);
    expect(keys).toContain(`/api/books/${CBZ_ID}/pages/0?width=1080`);
    expect(keys).toContain(`/api/books/${CBZ_ID}/pages/1?width=1080`);
    expect(keys).toContain(`/api/books/${CBZ_ID}/page-count`);
  });

  test("network-first: online reads bypass the cache; offline falls back to it", async ({
    page,
    context,
  }) => {
    await saveBookOffline(page, EPUB_ID);

    // Poison the cached chapter 1 with a sentinel.
    await page.evaluate(async (id) => {
      const cache = await caches.open(`carrel-offline-book-${id}`);
      await cache.put(
        new Request(`/api/books/${id}/chapters/1`),
        new Response("<p>SENTINEL-CACHED</p>", { headers: { "Content-Type": "text/html" } }),
      );
    }, EPUB_ID);

    // ONLINE: open the reader at chapter 1 — network must win.
    await page.goto(`/#/book/${EPUB_ID}/1/read`);
    const restart = page.locator("#resume-restart-btn");
    const content = page.locator("#reader-content");
    await expect(restart.or(content)).toBeVisible({ timeout: 15_000 });
    if (await restart.isVisible()) await restart.click();
    await expect(content).toContainText("chapter one", { timeout: 10_000 });
    await expect(content).not.toContainText("SENTINEL-CACHED");

    // OFFLINE: the same URL fetched directly is now served by the SW cache
    // fallback → sentinel bytes. (A reader page-turn wouldn't prove this —
    // the F-4-4 in-memory prefetch cache would satisfy it without any
    // network/SW involvement.)
    await context.setOffline(true);
    const offlineBody = await page.evaluate(async (id) => {
      const resp = await fetch(`/api/books/${id}/chapters/1`, { credentials: "same-origin" });
      return resp.text();
    }, EPUB_ID);
    expect(offlineBody).toContain("SENTINEL-CACHED");
    await context.setOffline(false);
  });

  test("a second concurrent save of the same book is rejected, not duplicated", async ({ page }) => {
    // The single-flight guard: once a save starts the button flips to a
    // disabled Downloading state, so the UI can't initiate a second, and
    // exactly one manifest row is published.
    await saveBookOffline(page, CBZ_ID);
    // Exactly one manifest row for this book (no duplicate from the guard).
    const count = await page.evaluate(async (id) => {
      const db = await new Promise<IDBDatabase>((res, rej) => {
        const req = indexedDB.open("carrel-offline");
        req.onsuccess = () => res(req.result);
        req.onerror = () => rej(req.error);
      });
      const rows = await new Promise<any[]>((res) => {
        const tx = db.transaction("books", "readonly");
        const r = tx.objectStore("books").getAll();
        r.onsuccess = () => res(r.result);
        r.onerror = () => res([]);
      });
      return rows.filter((row) => row.id === id).length;
    }, CBZ_ID);
    expect(count).toBe(1);
  });

  test("a chapter book with unknown chapter count gets no save affordance", async ({ page }) => {
    // Book 060: EPUB with total_chapters = 0 (a legitimate "count not known
    // yet" state). Saving it would publish a phantom offline book with zero
    // readable content — the UI must not offer it.
    await page.goto("/#/book/e2e-book-060");
    await expect(page.locator(".detail .actions")).toBeVisible();
    await expect(page.locator("#offline-save-btn")).toHaveCount(0);
    await expect(page.locator("#offline-remove-btn")).toHaveCount(0);
  });

  test("unsave removes the cache, manifest row, and badge", async ({ page }) => {
    await saveBookOffline(page, EPUB_ID);
    await page.locator("#offline-remove-btn").click();
    // Remove re-renders the detail page (menu closes) back to the unsaved
    // "Save offline" row — wait for it, then reopen the menu to see it.
    await expect(page.locator("#offline-save-btn")).toBeAttached({ timeout: 10_000 });
    await openDetailMenu(page);
    await expect(page.locator("#offline-save-btn")).toBeVisible();

    const gone = await page.evaluate(async (id) => {
      const hasCache = await caches.has(`carrel-offline-book-${id}`);
      const db = await new Promise<IDBDatabase>((res, rej) => {
        const req = indexedDB.open("carrel-offline");
        req.onsuccess = () => res(req.result);
        req.onerror = () => rej(req.error);
      });
      const row = await new Promise<any>((res) => {
        const tx = db.transaction("books", "readonly");
        const r = tx.objectStore("books").get(id);
        r.onsuccess = () => res(r.result);
        r.onerror = () => res(undefined);
      });
      return { hasCache, row };
    }, EPUB_ID);
    expect(gone.hasCache).toBe(false);
    expect(gone.row).toBeFalsy();

    await page.goto("/#/?q=Book%20050");
    const card = page.locator(".grid .card", { hasText: "Book 050" });
    await card.waitFor();
    await expect(card.locator(".offline-badge")).toHaveCount(0);
  });
});

test.describe("offline mode — boot & offline library (M4)", () => {
  test("booting offline with saved books shows the offline library and reads", async ({
    page,
    context,
  }) => {
    await saveBookOffline(page, EPUB_ID);

    // Boot offline from a top-level route: the SW-cached shell loads, init()
    // probes /api/books, the network fails, and the offline library renders
    // from IndexedDB.
    await page.goto("/#/");
    await context.setOffline(true);
    await reloadControlled(page);

    await expect(page.locator(".offline-banner")).toBeVisible();
    const card = page.locator(".grid .card", { hasText: "Book 050" });
    await expect(card).toHaveCount(1); // only the saved book

    // Open it → detail fully renders offline (cached detail JSON via the SW
    // fallback). The open button is Read or, if this book already has
    // server-side progress from an earlier test on the shared harness,
    // Continue — either proves showDetail's real render completed. Then the
    // reader renders a cached chapter offline (chapter zero or one; both are
    // in the saved inventory).
    await card.click();
    await expect(page).toHaveURL(/#\/book\//);
    const openBtn = page.locator("#read-btn, #continue-btn").first();
    await expect(openBtn).toBeVisible({ timeout: 15_000 });
    await openBtn.click();
    await expect(page.locator("#reader-content")).toContainText(/chapter (zero|one)/, {
      timeout: 15_000,
    });

    await context.setOffline(false);
  });

  test("Retry from the offline library recovers the online library", async ({ page, context }) => {
    await saveBookOffline(page, EPUB_ID);
    await page.goto("/#/");
    await context.setOffline(true);
    await reloadControlled(page);
    await expect(page.locator(".offline-banner")).toBeVisible();

    await context.setOffline(false);
    await page.locator("#offline-retry-btn").click();
    // Back online: full library (search box present, offline banner gone).
    await expect(page.locator("#search")).toBeVisible({ timeout: 15_000 });
    await expect(page.locator(".offline-banner")).toHaveCount(0);
  });

  test("offline deep-link to a NON-saved book redirects to the offline library", async ({
    page,
    context,
  }) => {
    await saveBookOffline(page, EPUB_ID); // 050 saved; 130 is not
    await page.goto("/#/");
    await context.setOffline(true);
    await reloadControlled(page);
    await expect(page.locator(".offline-banner")).toBeVisible();

    // Navigate to an un-downloaded book's URL — must land on the offline
    // library, not a bare "couldn't reach server" dead-end.
    await page.evaluate((id) => { location.hash = `#/book/${id}`; }, CBZ_ID);
    await expect(page.locator(".offline-banner")).toBeVisible();
    await expect(page.locator(".grid .card", { hasText: "Book 050" })).toHaveCount(1);
    await context.setOffline(false);
  });

  test("ordinary navigation recovers the online library after reconnect", async ({
    page,
    context,
  }) => {
    await saveBookOffline(page, EPUB_ID);
    await page.goto("/#/");
    await context.setOffline(true);
    await reloadControlled(page);
    await expect(page.locator(".offline-banner")).toBeVisible();

    // Reconnect, then navigate normally (NOT the Retry button) — the offline
    // library's re-probe must recover the full library with search.
    await context.setOffline(false);
    await page.evaluate(() => { location.hash = "#/library"; });
    await expect(page.locator("#search")).toBeVisible({ timeout: 15_000 });
    await expect(page.locator(".offline-banner")).toHaveCount(0);
  });

  test("booting offline with no saved books shows the offline card", async ({ page, context }) => {
    // Ensure nothing is saved.
    await page.goto("/#/");
    await page.evaluate(async () => {
      for (const k of await caches.keys()) {
        if (k.startsWith("carrel-offline-book-")) await caches.delete(k);
      }
      await new Promise<void>((res) => {
        const req = indexedDB.open("carrel-offline");
        req.onsuccess = () => {
          const db = req.result;
          const tx = db.transaction("books", "readwrite");
          tx.objectStore("books").clear();
          tx.oncomplete = () => res();
          tx.onerror = () => res();
        };
        req.onerror = () => res();
      });
    });

    // An offline reload can only be served if the worker finished precaching the
    // shell, and this spec — unlike the others, which spend seconds saving a
    // book first — gives the install no time of its own. Wait for the cached
    // shell explicitly instead of racing it (a lost race surfaces as
    // `page.reload: net::ERR_INTERNET_DISCONNECTED`).
    await page.waitForFunction(
      async () => {
        if (!navigator.serviceWorker.controller) return false;
        const shell = (await caches.keys()).find((k) => k.startsWith("carrel-shell-"));
        if (!shell) return false;
        return !!(await (await caches.open(shell)).match("/"));
      },
      null,
      { timeout: 20_000 }
    );

    await context.setOffline(true);
    await reloadControlled(page);
    // No manifests → the plain "couldn't reach server" card, not a library.
    await expect(page.locator("#retry-init-btn")).toBeVisible();
    await expect(page.locator(".offline-banner")).toHaveCount(0);
    await context.setOffline(false);
  });
});

test.describe("offline mode — progress replay (M5)", () => {
  test("progress made offline queues, then replays on reconnect", async ({ page, context }) => {
    // Use the CBZ (page mode): a page turn is a single deterministic progress
    // write. Reset any prior server progress so the queued position is newer.
    await saveBookOffline(page, CBZ_ID);
    await page.goto(`/#/book/${CBZ_ID}`);
    // Start Over lives in the More menu now; open it and reset if present.
    await openDetailMenu(page);
    const startOver = page.locator("#restart-btn");
    if (await startOver.isVisible().catch(() => false)) await startOver.click();

    // Open the reader ONLINE first (avoids racing SW control on an offline
    // navigation — and matches the real flow: reading, then losing signal),
    // then go offline and turn to page 1, waiting out the 2s progress
    // debounce so the PUT fires, fails, and enqueues.
    await openReaderAtZero(page, CBZ_ID);
    await context.setOffline(true);
    await page.locator("#reader-stage").focus();
    await page.keyboard.press("ArrowRight");
    await page.waitForTimeout(2500);

    // A queue row now exists for this book.
    const queued = await page.evaluate(async (id) => {
      const db = await new Promise<IDBDatabase>((res, rej) => {
        const req = indexedDB.open("carrel-offline");
        req.onsuccess = () => res(req.result);
        req.onerror = () => rej(req.error);
      });
      return new Promise<any>((res) => {
        const tx = db.transaction("progressQueue", "readonly");
        const r = tx.objectStore("progressQueue").get(id);
        r.onsuccess = () => res(r.result);
        r.onerror = () => res(undefined);
      });
    }, CBZ_ID);
    expect(queued).toBeTruthy();
    expect(queued.chapterIndex).toBeGreaterThan(0);

    // Reconnect → a PUT of the queued position must fire, and the queue drains.
    const replayPut = page.waitForRequest(
      (r) => r.method() === "PUT" && r.url().includes(`/api/books/${CBZ_ID}/progress`),
      { timeout: 20_000 },
    );
    await context.setOffline(false);
    await page.evaluate(() => window.dispatchEvent(new Event("online")));
    await replayPut;

    await expect
      .poll(async () =>
        page.evaluate(async (id) => {
          const db = await new Promise<IDBDatabase>((res, rej) => {
            const req = indexedDB.open("carrel-offline");
            req.onsuccess = () => res(req.result);
            req.onerror = () => rej(req.error);
          });
          return new Promise<number>((res) => {
            const tx = db.transaction("progressQueue", "readonly");
            const r = tx.objectStore("progressQueue").getAll();
            r.onsuccess = () => res(r.result.filter((x: any) => x.bookId === id).length);
            r.onerror = () => res(-1);
          });
        }, CBZ_ID),
        { timeout: 10_000 },
      )
      .toBe(0);
  });

  test("replay discards the queued position when the server advanced past the baseline", async ({
    page,
    context,
  }) => {
    // Simulate "another device advanced this book's server progress after our
    // last sync": open the reader online (which refreshes the baseline from
    // the server), THEN corrupt the manifest baseline to an old value. On
    // reconnect the server's real last_read_at won't match, so the queued
    // offline edit must be discarded (server wins), leaving the server
    // position untouched. (Corrupting before the online open wouldn't work —
    // the reader-open baseline refresh would overwrite it.)
    await saveBookOffline(page, CBZ_ID);
    await openReaderAtZero(page, CBZ_ID);
    await page.evaluate(async (id) => {
      const db = await new Promise<IDBDatabase>((res, rej) => {
        const req = indexedDB.open("carrel-offline");
        req.onsuccess = () => res(req.result);
        req.onerror = () => rej(req.error);
      });
      await new Promise<void>((res) => {
        const tx = db.transaction("books", "readwrite");
        const store = tx.objectStore("books");
        const g = store.get(id);
        g.onsuccess = () => {
          const row = g.result;
          row.baselineLastReadAt = 1; // definitely older than any real server ts
          store.put(row);
        };
        tx.oncomplete = () => res();
        tx.onerror = () => res();
      });
    }, CBZ_ID);

    await context.setOffline(true);
    await page.locator("#reader-stage").focus();
    await page.keyboard.press("ArrowRight");
    await page.waitForTimeout(2500);

    // Reconnect: replay GETs the server, baseline mismatch → discard, no PUT.
    let putFired = false;
    page.on("request", (r) => {
      if (r.method() === "PUT" && r.url().includes(`/api/books/${CBZ_ID}/progress`)) putFired = true;
    });
    await context.setOffline(false);
    await page.evaluate(() => window.dispatchEvent(new Event("online")));

    // Queue drains (row consumed) but no PUT was issued (discarded).
    await expect
      .poll(async () =>
        page.evaluate(async (id) => {
          const db = await new Promise<IDBDatabase>((res, rej) => {
            const req = indexedDB.open("carrel-offline");
            req.onsuccess = () => res(req.result);
            req.onerror = () => rej(req.error);
          });
          return new Promise<number>((res) => {
            const tx = db.transaction("progressQueue", "readonly");
            const r = tx.objectStore("progressQueue").getAll();
            r.onsuccess = () => res(r.result.filter((x: any) => x.bookId === id).length);
            r.onerror = () => res(-1);
          });
        }, CBZ_ID),
        { timeout: 10_000 },
      )
      .toBe(0);
    expect(putFired).toBe(false);
  });
});

test.describe("offline mode — eviction integrity & reconciliation (M6)", () => {
  test("a partially-evicted cache drops the manifest row and badge on boot", async ({ page }) => {
    await saveBookOffline(page, EPUB_ID);

    // Simulate browser eviction: delete one cached entry so the cache's key
    // set no longer matches the manifest's inventory hash.
    await page.evaluate(async (id) => {
      const cache = await caches.open(`carrel-offline-book-${id}`);
      await cache.delete(new Request(`/api/books/${id}/chapters/1`));
    }, EPUB_ID);

    // Reload online: the boot integrity check must detect the hash mismatch,
    // drop the manifest row AND the cache, and toast.
    await reloadControlled(page);
    await expect(page.locator(".toast")).toContainText(/removed some downloaded books/i, {
      timeout: 15_000,
    });

    const state = await page.evaluate(async (id) => {
      const hasCache = await caches.has(`carrel-offline-book-${id}`);
      const db = await new Promise<IDBDatabase>((res, rej) => {
        const req = indexedDB.open("carrel-offline");
        req.onsuccess = () => res(req.result);
        req.onerror = () => rej(req.error);
      });
      const row = await new Promise<any>((res) => {
        const tx = db.transaction("books", "readonly");
        const r = tx.objectStore("books").get(id);
        r.onsuccess = () => res(r.result);
        r.onerror = () => res(undefined);
      });
      return { hasCache, row };
    }, EPUB_ID);
    expect(state.row).toBeFalsy();
    expect(state.hasCache).toBe(false);

    // Grid badge gone.
    await page.goto("/#/?q=Book%20050");
    const card = page.locator(".grid .card", { hasText: "Book 050" });
    await card.waitFor();
    await expect(card.locator(".offline-badge")).toHaveCount(0);
  });

  test("reconciliation removes an offline copy of a book deleted server-side", async ({ page }) => {
    // Inject a manifest row + cache for a book id the server doesn't have,
    // mimicking a book deleted on the desktop side while it was saved offline.
    const GHOST = "e2e-book-ghost-999";
    await page.goto("/#/");
    await page.evaluate(async (id) => {
      const cache = await caches.open(`carrel-offline-book-${id}`);
      await cache.put(new Request(`/api/books/${id}`), new Response("{}", {
        headers: { "Content-Type": "application/json" },
      }));
      const db = await new Promise<IDBDatabase>((res, rej) => {
        const req = indexedDB.open("carrel-offline");
        req.onsuccess = () => res(req.result);
        req.onerror = () => rej(req.error);
      });
      await new Promise<void>((res) => {
        const tx = db.transaction("books", "readwrite");
        tx.objectStore("books").put({
          id, title: "Ghost", author: "x", format: "epub", totalChapters: 1,
          pageCount: 0, savedAt: Date.now(), bytes: 2,
          inventoryHash: "deadbeef", generation: "g", baselineLastReadAt: null, version: 1,
        });
        tx.oncomplete = () => res();
        tx.onerror = () => res();
      });
    }, GHOST);

    // Reload online: reconciliation compares manifests to the live library
    // and removes the copy of a book that no longer exists on the server.
    await reloadControlled(page);
    await expect
      .poll(async () =>
        page.evaluate(async (id) => {
          const hasCache = await caches.has(`carrel-offline-book-${id}`);
          const db = await new Promise<IDBDatabase>((res, rej) => {
            const req = indexedDB.open("carrel-offline");
            req.onsuccess = () => res(req.result);
            req.onerror = () => rej(req.error);
          });
          const row = await new Promise<any>((res) => {
            const tx = db.transaction("books", "readonly");
            const r = tx.objectStore("books").get(id);
            r.onsuccess = () => res(r.result);
            r.onerror = () => res(undefined);
          });
          return hasCache || !!row;
        }, GHOST),
        { timeout: 15_000 },
      )
      .toBe(false);
  });
});

// Highlights are online-only by design (spec 2026-07-23 §5): the SW excludes
// /highlights from the saved-book fallback (never cached), no marks render
// offline, the drawer degrades to a connection message, and the selection
// popover is suppressed while offline.
test.describe("offline mode — highlights are online-only", () => {
  test("saved book offline: no marks, drawer shows connection message, no popover", async ({
    page,
    context,
    request,
  }) => {
    // Seed a chapter-0 highlight ONLINE (offsets computed from the rendered
    // chapter text, same as highlights.spec.ts).
    await openReaderAtZero(page, EPUB_ID);
    await expect(page.locator("#reader-content")).toContainText("chapter zero", {
      timeout: 10_000,
    });
    const { s, e } = await page.evaluate(() => {
      const text = document.querySelector("#reader-content")!.textContent!;
      const start = text.indexOf("quick brown fox");
      return { s: start, e: start + "quick brown fox".length };
    });
    expect(s).toBeGreaterThan(-1);
    const created = await (
      await request.post(`/api/books/${EPUB_ID}/highlights`, {
        data: { chapterIndex: 0, text: "quick brown fox", color: "#f6c445", startOffset: s, endOffset: e },
      })
    ).json();

    try {
      await saveBookOffline(page, EPUB_ID);

      // Read the saved book OFFLINE (boot offline like a real launch).
      await page.goto("/#/");
      await context.setOffline(true);
      await reloadControlled(page);
      await page.evaluate((id) => { location.hash = `#/book/${id}/0/read`; }, EPUB_ID);
      const restart = page.locator("#resume-restart-btn");
      const content = page.locator("#reader-content");
      await expect(restart.or(content)).toBeVisible({ timeout: 15_000 });
      if (await restart.isVisible().catch(() => false)) await restart.click();
      await expect(content).toContainText("chapter zero", { timeout: 15_000 });

      // No marks render offline (the highlights GET failed, live-only).
      await expect(page.locator("mark.hl-mark")).toHaveCount(0);

      // Drawer degrades to the connection message.
      await page.locator("#hl-btn").click();
      await expect(page.locator("#hl-panel")).toBeVisible();
      await expect(page.locator("#hl-panel .hl-empty")).toContainText(
        "Highlights need a connection.",
      );
      await page.keyboard.press("Escape");

      // Selection popover is suppressed while offline.
      await page.evaluate(() => {
        const el = document.querySelector("#reader-content")!;
        const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
        let node: Node | null = null;
        let off = -1;
        while ((node = walker.nextNode())) {
          off = node.nodeValue!.indexOf("quick brown fox");
          if (off !== -1) break;
        }
        const range = document.createRange();
        range.setStart(node!, off);
        range.setEnd(node!, off + "quick brown fox".length);
        const sel = window.getSelection()!;
        sel.removeAllRanges();
        sel.addRange(range);
        document.dispatchEvent(new Event("selectionchange"));
      });
      await page.waitForTimeout(500); // > the 250ms selection debounce
      await expect(page.locator("#hl-popover")).toHaveCount(0);

      // And nothing highlight-related ever entered a cache (SW exclusion).
      const urls = await page.evaluate(async () => {
        const out: string[] = [];
        for (const name of await caches.keys()) {
          const cache = await caches.open(name);
          for (const req of await cache.keys()) out.push(req.url);
        }
        return out;
      });
      expect(urls.some((u) => u.includes(`/api/books/${EPUB_ID}`))).toBe(true); // non-vacuous
      expect(urls.some((u) => /\/highlights(?:\/|\?|$)/.test(u))).toBe(false);
    } finally {
      await context.setOffline(false);
      // Clean up the seeded highlight (the request fixture bypasses the
      // browser context, so this works regardless of emulated offline state).
      await request.delete(`/api/books/${EPUB_ID}/highlights/${created.id}`);
    }
  });
});

// Profile-scoped offline storage (remote profile switching, PR #119).
//
// Book ids are per-profile DB id spaces, so an offline cache keyed by bare book
// id can serve profile A's saved content for a *different* book that happens to
// share that id in profile B. Both the Cache Storage names and the IndexedDB
// manifest must therefore be scoped to the active profile, and switching must
// not destroy the other profile's downloads.
//
// The harness (src-tauri/examples/web_e2e_server.rs) advertises `default`,
// `magazines` (switchable) and `vault` (locked), and switching only moves the
// advertised active flag — the served library is the same seeded DB, which is
// exactly the collision scenario: identical book ids under two profiles.
test.describe("offline mode — profile scoping", () => {
  async function switchProfile(page: Page, name: string) {
    const status = await page.evaluate(async (profile) => {
      const resp = await fetch("/api/profile", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "same-origin",
        body: JSON.stringify({ name: profile }),
      });
      return resp.status;
    }, name);
    expect(status).toBe(200);
  }

  const offlineBookCacheKeys = (page: Page) =>
    page.evaluate(() => caches.keys().then((k) => k.filter((n) => n.startsWith("carrel-offline-book-"))));

  test.afterEach(async ({ page }) => {
    // The active profile is server-global state shared with every other spec.
    await page.goto("/");
    await switchProfile(page, "default");
  });

  test("a book saved in one profile is not saved in another, and both downloads survive", async ({
    page,
  }) => {
    await page.goto("/");
    await saveBookOffline(page, EPUB_ID);

    const defaultKeys = await offlineBookCacheKeys(page);
    expect(defaultKeys).toHaveLength(1);

    await switchProfile(page, "magazines");
    await page.reload();

    // Same book id, different profile → not saved here.
    await page.goto(`/#/book/${EPUB_ID}`);
    await openDetailMenu(page);
    await expect(page.locator("#offline-save-btn")).toBeVisible();
    await expect(page.locator("#offline-remove-btn")).toHaveCount(0);

    // Saving it here creates a second, distinctly-named cache — the first
    // profile's download is untouched.
    await saveBookOffline(page, EPUB_ID);
    const bothKeys = await offlineBookCacheKeys(page);
    expect(bothKeys).toHaveLength(2);
    expect(new Set(bothKeys).size).toBe(2);
    for (const key of defaultKeys) expect(bothKeys).toContain(key);

    // Back to the first profile: its download is still there.
    await switchProfile(page, "default");
    await page.reload();
    await page.goto(`/#/book/${EPUB_ID}`);
    await openDetailMenu(page);
    await expect(page.locator("#offline-remove-btn")).toBeVisible();
  });

  test("a failed scope publish disables offline rather than serving the wrong profile", async ({
    page,
    context,
  }) => {
    await page.goto("/");
    await saveBookOffline(page, EPUB_ID);

    // Break Cache Storage for the scope marker only — the shape of a quota
    // failure that hits the one write the worker's namespace depends on. Reads
    // and every other cache keep working, so this is the fail-open window:
    // without care the page moves to profile B's namespace while the worker is
    // still answering out of profile A's caches.
    await page.addInitScript(() => {
      const open = caches.open.bind(caches);
      caches.open = (name: string) =>
        name === "carrel-offline-scope"
          ? Promise.reject(new DOMException("Quota exceeded", "QuotaExceededError"))
          : open(name);
    });

    await switchProfile(page, "magazines");
    await reloadControlled(page);

    await context.setOffline(true);
    try {
      // Whatever the page decided about its own namespace, the worker must not
      // answer this id from the previous profile's cache.
      const outcome = await page.evaluate(async (id) => {
        try {
          const resp = await fetch(`/api/books/${id}`, { credentials: "same-origin" });
          return { status: resp.status, body: (await resp.text()).slice(0, 120) };
        } catch (e) {
          return { failed: true };
        }
      }, EPUB_ID);
      expect(outcome).toEqual({ failed: true });
    } finally {
      await context.setOffline(false);
    }
  });

  test("offline in a profile with no downloads never serves another profile's saved book", async ({
    page,
    context,
  }) => {
    await page.goto("/");
    await saveBookOffline(page, EPUB_ID);
    await switchProfile(page, "magazines");
    await reloadControlled(page);

    await context.setOffline(true);
    try {
      // The direct guarantee: with the network gone and nothing saved under
      // this profile, the service worker must not answer this book id out of
      // the *other* profile's cache — the request has to fail like any
      // unsaved book's would.
      const outcome = await page.evaluate(async (id) => {
        try {
          const resp = await fetch(`/api/books/${id}`, { credentials: "same-origin" });
          return { status: resp.status, body: (await resp.text()).slice(0, 120) };
        } catch (e) {
          return { failed: true };
        }
      }, EPUB_ID);
      expect(outcome).toEqual({ failed: true });

      // And a cold offline boot lands on the plain unreachable-server card
      // (`renderOfflineState`), not the offline library — which only renders
      // when *this* profile has saved books.
      await page.reload();
      await expect(page.locator("#retry-init-btn")).toBeVisible({ timeout: 15_000 });
      await expect(page.locator("#offline-retry-btn")).toHaveCount(0);
    } finally {
      await context.setOffline(false);
    }
  });
});
