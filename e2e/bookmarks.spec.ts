import { test, expect, Page } from "@playwright/test";
import { openDetailMenu } from "./detail-actions";

const EPUB_ID = "e2e-book-050";
test.use({ serviceWorkers: "block" });

async function openEpubReader(page: Page) {
  await page.goto(`/#/book/${EPUB_ID}/0/read`);
  const restart = page.locator("#resume-restart-btn");
  const content = page.locator("#reader-content");
  await expect(restart.or(content)).toBeVisible({ timeout: 15_000 });
  if (await restart.isVisible()) { await restart.click(); await content.waitFor(); }
  await expect(content).toContainText("chapter zero", { timeout: 10_000 });
}

// Bookmarks persist to the harness's shared on-disk DB (seeded once at server
// start, not reset per test). Clear this book's bookmarks before each test so
// counts start from a clean slate regardless of what earlier tests created.
test.beforeEach(async ({ request }) => {
  const resp = await request.get(`/api/books/${EPUB_ID}/bookmarks`);
  if (!resp.ok()) return;
  const rows = await resp.json();
  for (const bm of rows) {
    await request.delete(`/api/books/${EPUB_ID}/bookmarks/${bm.id}`);
  }
});

test.describe("web reader bookmarks — core", () => {
  test("toolbar shows a bookmark trigger", async ({ page }) => {
    await openEpubReader(page);
    await expect(page.locator("#bookmark-btn")).toBeVisible();
  });

  test("empty state before any bookmark", async ({ page }) => {
    await openEpubReader(page);
    await page.locator("#bookmark-btn").click();
    await expect(page.locator("#bookmark-panel")).toBeVisible();
    await expect(page.locator(".bookmark-empty")).toContainText("No bookmarks yet");
  });

  test("add-here creates an entry the list shows", async ({ page }) => {
    await openEpubReader(page);
    await page.locator("#bookmark-btn").click();
    await page.locator("#bookmark-add-btn").click();
    await expect(page.locator(".bookmark-entry")).toHaveCount(1);
  });

  test("tap a different-chapter bookmark jumps and closes the panel", async ({ page }) => {
    await openEpubReader(page);
    // Bookmark chapter 0, turn to chapter 1, then jump back via the bookmark.
    await page.locator("#bookmark-btn").click();
    await page.locator("#bookmark-add-btn").click();
    await expect(page.locator(".bookmark-entry")).toHaveCount(1);
    await page.locator("#bookmark-close").click();
    await page.locator("#next-btn").click();
    await expect(page.locator("#reader-content")).toContainText("chapter one");
    await page.locator("#bookmark-btn").click();
    await page.locator(".bookmark-entry").first().click();
    await expect(page.locator("#reader-content")).toContainText("chapter zero");
    await expect(page.locator("#bookmark-panel")).toBeHidden();
  });

  test("delete removes the entry", async ({ page }) => {
    await openEpubReader(page);
    await page.locator("#bookmark-btn").click();
    await page.locator("#bookmark-add-btn").click();
    await expect(page.locator(".bookmark-entry")).toHaveCount(1);
    // add-here drops into inline rename; exit it (Escape) so the row settles
    // before we click delete — matches the real flow where the field isn't
    // mid-edit when you reach for the ✕.
    await page.locator(".bookmark-entry input.bookmark-name-input").press("Escape");
    await page.locator(".bookmark-del").first().click();
    await expect(page.locator(".bookmark-entry")).toHaveCount(0);
    await expect(page.locator(".bookmark-empty")).toBeVisible();
  });

  test("rename persists across a reload (fresh GET)", async ({ page }) => {
    await openEpubReader(page);
    await page.locator("#bookmark-btn").click();
    await page.locator("#bookmark-add-btn").click();
    // add-here drops straight into the inline rename input.
    const input = page.locator(".bookmark-entry input.bookmark-name-input");
    await expect(input).toBeVisible();
    await input.fill("My favourite line");
    await input.press("Enter");
    await expect(page.locator(".bookmark-entry").first()).toContainText("My favourite line");
    // Full reload wipes module state (bookmarksState), so reopening the drawer
    // MUST refetch from the server — proving the rename persisted, not just
    // that local state held it (a reopen alone short-circuits on `loaded`).
    await page.reload();
    await openEpubReader(page);
    await page.locator("#bookmark-btn").click();
    await expect(page.locator(".bookmark-entry").first()).toContainText("My favourite line");
  });

  test("clicking inside the rename input keeps the panel open (no jump)", async ({ page }) => {
    await openEpubReader(page);
    await page.locator("#bookmark-btn").click();
    await page.locator("#bookmark-add-btn").click();
    const input = page.locator(".bookmark-entry input.bookmark-name-input");
    await expect(input).toBeVisible();
    // A real pointer click (to place the cursor) must NOT bubble to the list
    // handler and fire jumpToBookmark — which would close the panel and discard
    // the edit. fill() alone focuses without a bubbling click, so this test is
    // what actually exercises the click path.
    await input.click();
    await expect(page.locator("#bookmark-panel")).toBeVisible();
    await expect(input).toBeVisible();
    await input.fill("Typed after click");
    await input.press("Enter");
    await expect(page.locator(".bookmark-entry").first()).toContainText("Typed after click");
  });

  test("same-chapter bookmark restores scroll without a chapter turn", async ({ page }) => {
    await openEpubReader(page);
    const stage = page.locator("#reader-stage");
    // Guard against a silently-passing test: chapter zero MUST be scrollable at
    // the test viewport (fixture ch0 is padded in web_e2e_server.rs for this).
    const scrollable = await stage.evaluate((el) => el.scrollHeight > el.clientHeight);
    expect(scrollable).toBe(true);
    // Scroll down within chapter zero, bookmark, scroll back to top, jump.
    await stage.evaluate((el) => { el.scrollTop = el.scrollHeight; });
    await expect.poll(() => stage.evaluate((el) => el.scrollTop)).toBeGreaterThan(0);
    await page.locator("#bookmark-btn").click();
    await page.locator("#bookmark-add-btn").click();
    await expect(page.locator(".bookmark-entry")).toHaveCount(1);
    // add-here drops into inline rename; cancel it so the panel owns Escape/close.
    const input = page.locator(".bookmark-entry input.bookmark-name-input");
    await expect(input).toBeVisible();
    await input.press("Escape");
    await page.locator("#bookmark-close").click();
    await expect(page.locator("#bookmark-panel")).toBeHidden();
    await stage.evaluate((el) => { el.scrollTop = 0; });
    await page.locator("#bookmark-btn").click();
    await page.locator(".bookmark-entry").first().click();
    // Still chapter zero (no turn), but scrolled back down.
    await expect(page.locator("#reader-content")).toContainText("chapter zero");
    await expect(page.locator("#bookmark-panel")).toBeHidden();
    await expect.poll(() => stage.evaluate((el) => el.scrollTop)).toBeGreaterThan(0);
  });
});

// A GET .../bookmarks response must never land in a per-book offline cache:
// bookmarks are live-only. Task 3 excludes /bookmarks from the SW's saved-book
// branch; the offline-save inventory (app.js) never enumerates it. This proves
// the end-to-end guarantee with a real saved book + a drawer open. Needs the
// service worker enabled (the rest of the file blocks it).
test.describe("web reader bookmarks — not cached offline", () => {
  test.use({ serviceWorkers: "allow" });

  test("bookmarks never enter a saved book's offline cache", async ({ page }) => {
    // Save book 050 offline (populates carrel-offline-book-<id>).
    await page.goto(`/#/book/${EPUB_ID}`);
    await openDetailMenu(page);
    await page.locator("#offline-save-btn").click();
    await expect(page.locator("#offline-remove-btn")).toBeAttached({ timeout: 30_000 });

    // Open the reader + bookmark drawer, which fires GET /bookmarks, and add
    // one (POST) — neither must be persisted to any cache.
    await openEpubReader(page);
    await page.locator("#bookmark-btn").click();
    await expect(page.locator("#bookmark-panel")).toBeVisible();
    await page.locator("#bookmark-add-btn").click();
    await expect(page.locator(".bookmark-entry")).toHaveCount(1);

    const urls = await page.evaluate(async () => {
      const out: string[] = [];
      for (const name of await caches.keys()) {
        const cache = await caches.open(name);
        for (const req of await cache.keys()) out.push(req.url);
      }
      return out;
    });
    // Non-vacuous: the offline cache really was populated with book URLs.
    expect(urls.some((u) => u.includes(`/api/books/${EPUB_ID}`))).toBe(true);
    // The guarantee: nothing bookmark-related is cached.
    expect(urls.some((u) => /\/bookmarks(?:\/|\?|$)/.test(u))).toBe(false);
  });
});

test.describe("web reader bookmarks — dismissal", () => {
  test("Escape closes without navigating back", async ({ page }) => {
    await openEpubReader(page);
    await page.locator("#bookmark-btn").click();
    await expect(page.locator("#bookmark-panel")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.locator("#bookmark-panel")).toBeHidden();
    await expect(page.locator("#reader-content")).toBeVisible();
  });

  test("click outside closes, click inside keeps open", async ({ page }) => {
    await openEpubReader(page);
    await page.locator("#bookmark-btn").click();
    await page.locator("#bookmark-panel").click({ position: { x: 10, y: 10 } });
    await expect(page.locator("#bookmark-panel")).toBeVisible();
    await page.locator("#reader-stage").click({ position: { x: 600, y: 300 } });
    await expect(page.locator("#bookmark-panel")).toBeHidden();
  });

  test("opening Contents/Aa closes Bookmarks and vice-versa", async ({ page }) => {
    await openEpubReader(page);
    await page.locator("#bookmark-btn").click();
    await expect(page.locator("#bookmark-panel")).toBeVisible();
    await page.locator("#typo-btn").click();
    await expect(page.locator("#typo-panel")).toBeVisible();
    await expect(page.locator("#bookmark-panel")).toBeHidden();
    await page.locator("#bookmark-btn").click();
    await expect(page.locator("#bookmark-panel")).toBeVisible();
    await expect(page.locator("#typo-panel")).toBeHidden();
  });

  test("chrome-hide closes it and it stays closed on chrome return", async ({ page }) => {
    await openEpubReader(page);
    await page.locator("#bookmark-btn").click();
    await expect(page.locator("#bookmark-panel")).toBeVisible();
    await page.locator("#chrome-toggle-btn").click();
    await expect(page.locator("#bookmark-panel")).toBeHidden();
    await page.locator("#chrome-toggle-btn").click();
    await expect(page.locator("#bookmark-panel")).toBeHidden();
    await expect(page.locator("#bookmark-btn")).toHaveAttribute("aria-expanded", "false");
  });

  test("arrow keys don't turn the chapter while the panel is open", async ({ page }) => {
    await openEpubReader(page);
    await page.locator("#bookmark-btn").click();
    await page.locator("#bookmark-panel").press("ArrowRight");
    await expect(page.locator("#reader-content")).toContainText("chapter zero");
    await expect(page.locator("#bookmark-panel")).toBeVisible();
  });
});
