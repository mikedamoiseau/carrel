import { test, expect, type BrowserContext, type Page } from "@playwright/test";
import { openDetailMenu } from "./detail-actions";

// Offline copies in web Kids mode (spec Decision 9): a book a parent saved
// offline that isn't marked OK for kids is hidden on the device while it is in
// Kids mode, online and offline, and never deleted — it comes back when the
// parent leaves Kids mode.
//
// Against the Kids harness (port 7811, see e2e/kids-mode.spec.ts). Only two of
// its books have real files and can be saved offline, Book 050 (EPUB) and
// Book 130 (CBZ), and neither is in a series. So a book mark makes 050 the
// kid's book, and 130, with no mark, stays hidden.
test.use({ baseURL: "http://127.0.0.1:7811" });

const PIN = "2468";
const CODE = "1357";
const KIDS_ID = "e2e-book-050";
const KIDS_TITLE = "Book 050";
const HIDDEN_ID = "e2e-book-130";
const HIDDEN_TITLE = "Book 130";

// The helpers below are kids-mode.spec.ts's and offline-mode.spec.ts's.

function apiStatus(page: Page, method: string, path: string, body?: unknown) {
  return page.evaluate(
    async ({ method, path, body }) =>
      (
        await fetch(path, {
          method,
          credentials: "same-origin",
          headers: { "Content-Type": "application/json" },
          body: body === undefined ? undefined : JSON.stringify(body),
        })
      ).status,
    { method, path, body },
  );
}

async function logIn(page: Page) {
  await expect(page.locator("#pin")).toBeVisible();
  await page.locator("#pin").fill(PIN);
  await page.locator("#login-btn").click();
  await expect(page.locator(".grid .card").first()).toBeVisible();
}

const badge = (page: Page) => page.locator(".header .kids-badge");

async function turnOnKidsMode(page: Page) {
  await page.getByRole("button", { name: "Kids mode", exact: true }).click();
  await page.getByRole("button", { name: "Turn on Kids mode" }).click();
  await expect(badge(page)).toBeVisible();
}

async function leaveWith(page: Page, code: string) {
  await page.getByRole("button", { name: "Leave Kids mode" }).click();
  await page.getByLabel("Parents' code").fill(code);
  await page.getByRole("button", { name: "Leave", exact: true }).click();
}

async function saveBookOffline(page: Page, bookId: string) {
  await page.goto(`/#/book/${bookId}`);
  await openDetailMenu(page);
  await page.locator("#offline-save-btn").click();
  await expect(page.locator("#offline-remove-btn")).toBeAttached({ timeout: 30_000 });
}

// Reload once the service worker controls the page, and wait until it
// controls the reloaded one too (see offline-mode.spec.ts for why).
async function reloadControlled(page: Page) {
  await page.waitForFunction(() => !!navigator.serviceWorker.controller, null, {
    timeout: 30_000,
  });
  await page.reload();
  await page.waitForFunction(() => !!navigator.serviceWorker.controller, null, {
    timeout: 15_000,
  });
}

// What is still on the device: the manifest rows and per-book caches of the
// default profile (the unscoped `carrel-offline` database), and the stored
// kid-visible list.
function deviceState(page: Page) {
  return page.evaluate(async (ids) => {
    const db = await new Promise<IDBDatabase>((res, rej) => {
      const req = indexedDB.open("carrel-offline");
      req.onsuccess = () => res(req.result);
      req.onerror = () => rej(req.error);
    });
    const read = <T,>(store: string, op: (s: IDBObjectStore) => IDBRequest) =>
      new Promise<T>((res, rej) => {
        const r = op(db.transaction(store, "readonly").objectStore(store));
        r.onsuccess = () => res(r.result as T);
        r.onerror = () => rej(r.error);
      });
    const rows = await read<string[]>("books", (s) => s.getAllKeys());
    const visible = await read<{ ids: string[] } | undefined>("meta", (s) => s.get("kids-visible-ids"));
    db.close();
    const cached: string[] = [];
    for (const id of ids) {
      if (await caches.has(`carrel-offline-book-${id}`)) cached.push(id);
    }
    return { rows: rows.slice().sort(), cached, visibleIds: visible ? visible.ids : null };
  }, [KIDS_ID, HIDDEN_ID]);
}

async function deleteStoredVisibleIds(page: Page) {
  await page.evaluate(async () => {
    const db = await new Promise<IDBDatabase>((res, rej) => {
      const req = indexedDB.open("carrel-offline");
      req.onsuccess = () => res(req.result);
      req.onerror = () => rej(req.error);
    });
    await new Promise<void>((res, rej) => {
      const tx = db.transaction("meta", "readwrite");
      tx.objectStore("meta").delete("kids-visible-ids");
      tx.oncomplete = () => res();
      tx.onerror = () => rej(tx.error);
    });
    db.close();
  });
}

const BOTH_KEPT = { rows: [KIDS_ID, HIDDEN_ID], cached: [KIDS_ID, HIDDEN_ID] };

async function expectOfflineLibrary(page: Page, titles: string[]) {
  await expect(page.locator(".offline-banner")).toBeVisible();
  await expect(page.locator(".grid .card .title")).toHaveText(titles);
}

// Records whether the hidden book's title ever reaches the document, even for
// a moment, from every page load after this call. Returns a reader for the
// current document's record.
async function watchForHiddenTitle(page: Page) {
  await page.addInitScript((title) => {
    const w = window as unknown as { __sawHidden: boolean };
    w.__sawHidden = false;
    new MutationObserver(() => {
      if (document.documentElement.innerHTML.includes(title)) w.__sawHidden = true;
    }).observe(document, { childList: true, subtree: true, characterData: true, attributes: true });
  }, HIDDEN_TITLE);
  return () => page.evaluate(() => (window as unknown as { __sawHidden: boolean }).__sawHidden);
}

// Both books saved by the parent, then the device put in Kids mode and
// reloaded online, which is the boot that used to reconcile the hidden copy
// away. Ends on the online Kids library once the network has gone quiet, so
// that reconcile has had its chance to run.
async function saveBothThenKidsBoot(page: Page) {
  await saveBookOffline(page, KIDS_ID);
  await saveBookOffline(page, HIDDEN_ID);
  await page.goto("/#/");
  await turnOnKidsMode(page);
  await reloadControlled(page);
  await expect(badge(page)).toBeVisible();
  await expect(page.locator(".grid .card .title")).toHaveText([KIDS_TITLE]);
  await page.waitForLoadState("networkidle");
}

async function bootOffline(page: Page, context: BrowserContext) {
  await context.setOffline(true);
  await reloadControlled(page);
}

test.describe("web UI Kids mode with offline copies", () => {
  test.beforeEach(async ({ page }) => {
    await page.goto("/");
    await logIn(page);
    expect(await apiStatus(page, "PUT", "/api/kids/code", { code: CODE })).toBe(200);
    expect(
      await apiStatus(page, "PUT", `/api/kids/books/${KIDS_ID}`, { override: "allowed" }),
    ).toBe(200);
  });

  test.afterEach(async ({ page, context }) => {
    await context.setOffline(false);
    await apiStatus(page, "POST", "/api/kids/exit", { code: CODE });
    await apiStatus(page, "PUT", `/api/kids/books/${KIDS_ID}`, { override: "follow" });
  });

  test("Kids mode keeps unmarked offline copies but hides them", async ({ page, context }) => {
    await saveBothThenKidsBoot(page);
    // The online Kids boot deleted nothing, and stored the kid's list.
    expect(await deviceState(page)).toEqual({ ...BOTH_KEPT, visibleIds: [KIDS_ID] });

    await bootOffline(page, context);
    await expectOfflineLibrary(page, [KIDS_TITLE]);
    await expect(page.locator("body")).not.toContainText(HIDDEN_TITLE);
    await expect(badge(page)).toBeVisible();
    // Hidden, not deleted.
    expect(await deviceState(page)).toMatchObject(BOTH_KEPT);
  });

  test("a Kids boot that can't read its Kids status still keeps the hidden copy", async ({
    page,
  }) => {
    await saveBookOffline(page, KIDS_ID);
    await saveBookOffline(page, HIDDEN_ID);
    await page.goto("/#/");
    await turnOnKidsMode(page);
    // The session is still a kid's, so the boot gets the Kids list, but the
    // page can't learn that it's in Kids mode.
    await page.route("**/api/kids/status", (route) => route.fulfill({ status: 500 }));
    await reloadControlled(page);
    await expect(page.locator(".grid .card .title")).toHaveText([KIDS_TITLE]);
    await page.waitForLoadState("networkidle");
    expect(await deviceState(page)).toMatchObject(BOTH_KEPT);
  });

  test("offline deep link to a hidden book is refused", async ({ page, context }) => {
    const sawHidden = await watchForHiddenTitle(page);
    await saveBothThenKidsBoot(page);
    await bootOffline(page, context);
    await expectOfflineLibrary(page, [KIDS_TITLE]);

    // A kid in airplane mode types the hidden book's URL…
    await page.evaluate((id) => { location.hash = `#/book/${id}`; }, HIDDEN_ID);
    await expect(page.locator(".toast", { hasText: "That book isn't available offline" })).toBeVisible();
    await expectOfflineLibrary(page, [KIDS_TITLE]);
    expect(await sawHidden()).toBe(false);

    // …or opens the app straight onto it, reader and all.
    await page.evaluate((id) => { location.hash = `#/book/${id}/0/read`; }, HIDDEN_ID);
    await reloadControlled(page);
    await expectOfflineLibrary(page, [KIDS_TITLE]);
    expect(await sawHidden()).toBe(false);
    await expect(page.locator("#reader-stage, #reader-content")).toHaveCount(0);

    // Control: the kid's own book still opens offline.
    await page.locator(".grid .card", { hasText: KIDS_TITLE }).click();
    await expect(page.locator(".header .nav-book-title")).toHaveText(KIDS_TITLE);
  });

  test("a hidden copy stays hidden when the network drops after an online boot", async ({
    page,
    context,
  }) => {
    const sawHidden = await watchForHiddenTitle(page);
    await saveBothThenKidsBoot(page);
    // No reload: this page booted online, so it isn't in offline mode, and the
    // service worker now answers a saved book from its cache.
    await context.setOffline(true);
    // Control: the kid's own copy opens from that cache.
    await page.evaluate((id) => { location.hash = `#/book/${id}`; }, KIDS_ID);
    await expect(page.locator(".header .nav-book-title")).toHaveText(KIDS_TITLE);

    for (const hash of [`#/book/${HIDDEN_ID}`, `#/book/${HIDDEN_ID}/0/read`]) {
      await page.evaluate((h) => { location.hash = h; }, hash);
      // What the server answers for a hidden book: 404.
      await expect(page.locator(".view-error")).toHaveText("Couldn't load this book (HTTP 404)");
    }
    expect(await sawHidden()).toBe(false);
  });

  test("leaving Kids mode restores the offline copies", async ({ page, context }) => {
    await saveBothThenKidsBoot(page);
    await bootOffline(page, context);
    await expectOfflineLibrary(page, [KIDS_TITLE]);

    await context.setOffline(false);
    await reloadControlled(page);
    await expect(badge(page)).toBeVisible();
    await leaveWith(page, CODE);
    await expect(page.locator(".kids-badge")).toHaveCount(0);

    await bootOffline(page, context);
    // Newest save first.
    await expectOfflineLibrary(page, [HIDDEN_TITLE, KIDS_TITLE]);
    await expect(page.locator(".kids-badge")).toHaveCount(0);
  });

  test("turning Kids mode on stores the kid's list before any reload", async ({
    page,
    context,
  }) => {
    await saveBookOffline(page, KIDS_ID);
    await saveBookOffline(page, HIDDEN_ID);
    await page.goto("/#/");
    await turnOnKidsMode(page);
    // The network drops right away: no online boot has run in Kids mode.
    await bootOffline(page, context);
    await expectOfflineLibrary(page, [KIDS_TITLE]);
  });

  test("with no stored list, a Kids device shows none of its offline copies", async ({
    page,
    context,
  }) => {
    await saveBothThenKidsBoot(page);
    await deleteStoredVisibleIds(page);
    await bootOffline(page, context);
    // Nothing it may show, so the plain offline card, as with nothing saved.
    await expect(page.locator("#retry-init-btn")).toBeVisible();
    await expect(page.locator(".grid .card")).toHaveCount(0);
    expect(await deviceState(page)).toEqual({ ...BOTH_KEPT, visibleIds: null });
  });

  test("a Kids device that booted offline settles into Kids mode on reconnect", async ({
    page,
    context,
  }) => {
    await saveBothThenKidsBoot(page);
    await bootOffline(page, context);
    await expectOfflineLibrary(page, [KIDS_TITLE]);
    // Forget the kid's list, so the reconnect has to fetch it again.
    await deleteStoredVisibleIds(page);

    // Reconnect, then any navigation re-probes the server.
    await context.setOffline(false);
    await page.evaluate(() => { location.hash = "#/library"; });
    await expect(page.locator("#search")).toBeVisible({ timeout: 15_000 });
    await expect(page.locator(".offline-banner")).toHaveCount(0);
    await expect(badge(page)).toBeVisible();
    await expect(page.locator(".grid .card .title")).toHaveText([KIDS_TITLE]);
    await expect
      .poll(() => deviceState(page))
      .toEqual({ ...BOTH_KEPT, visibleIds: [KIDS_ID] });
  });
});
