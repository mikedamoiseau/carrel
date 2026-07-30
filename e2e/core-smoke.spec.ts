import { test, expect } from "@playwright/test";
import { enterReaderAtStart } from "./detail-actions";

// Core smoke flows against the real client (app.js / index.html), run
// against the deterministic harness (src-tauri/examples/web_e2e_server.rs).
//
//   - Hash router: bare "#"/"#/" = library, "#/book/:id" = detail,
//     "#/book/:id/:index/read" = reader.
//   - Detail page: header `<h1>` + `.header` back button (`#back-btn`), cover
//     at `.detail .cover img` (falls back to `.cover-placeholder` since none
//     of the seeded books have a real cover file), and either `#read-btn`
//     (no progress) or `#continue-btn`/`#restart-btn` (has progress).
//   - Reader: chrome built once (`#reader-stage`); CBZ pages render into
//     `#page-img` (EPUB/MOBI would use `#reader-content` instead, but the
//     harness's only book with a real file on disk is the CBZ, `Book 130`).
//     `#prev-btn`/`#next-btn` plus ArrowLeft/ArrowRight turn pages. Escape
//     (no fullscreen active) or `#back-btn` calls goBack(), which returns to
//     the book's DETAIL page (`#/book/:id`), not straight to the library grid.
//   - Theme: cycles light -> dark -> system -> light via `#theme-toggle-btn`;
//     persisted as localStorage key `carrel_theme`; applied as `data-theme`
//     on <html> (removed entirely for "system").
//   - Keyboard shortcut `/` focuses `#search`, only while currentView is the
//     library and focus isn't already inside a typing target.
//
// `Book 130` (id `e2e-book-130`) is the harness's CBZ book: a real 2-page
// CBZ (2 tiny valid PNGs) on disk, no reading progress, so the detail page
// shows `#read-btn` and the reader always opens at page 0.
const READER_BOOK_ID = "e2e-book-130";

test.describe("Core smoke", () => {
  test("library -> click a card -> detail page renders", async ({ page }) => {
    await page.goto("/");
    const card = page.locator(".grid .card").first();
    await card.waitFor();
    const title = await card.getAttribute("aria-label"); // "Open <title>"

    await card.click();
    await expect(page).toHaveURL(/#\/book\//);
    await page.locator("h1").waitFor();
    const h1Text = await page.locator("h1").textContent();
    expect(title).toContain(h1Text?.trim() ?? " ");

    // None of the seeded books have a real cover file, so the fallback
    // placeholder is the only path exercised here — still assert something
    // renders in the cover slot.
    await expect(page.locator(".detail .cover .cover-placeholder")).toBeVisible();

    // A Read/Continue action must be present.
    const actionCount = (await page.locator("#read-btn").count()) + (await page.locator("#continue-btn").count());
    expect(actionCount).toBeGreaterThan(0);
  });

  test("reader opens a CBZ, renders a page image, advances on ArrowRight, and Esc returns to detail", async ({
    page,
  }) => {
    await page.goto(`/#/book/${READER_BOOK_ID}`);
    // The reader records progress as soon as it's entered, so re-running
    // this spec against an already-seeded harness turns "Read" into
    // "Continue"/"Start Over" on the second run — use whichever chapter-0
    // entry point is present so the test is idempotent across repeated runs.
    await enterReaderAtStart(page);

    await expect(page).toHaveURL(new RegExp(`#/book/${READER_BOOK_ID}/0/read`));
    await page.locator("#reader-stage").waitFor();
    const pageImg = page.locator("#page-img");
    await expect(pageImg).toBeVisible();
    await expect
      .poll(async () => pageImg.evaluate((img) => (img as HTMLImageElement).naturalWidth), { timeout: 15_000 })
      .toBeGreaterThan(0);

    await page.keyboard.press("ArrowRight");
    await expect(page).toHaveURL(new RegExp(`#/book/${READER_BOOK_ID}/1/read`), { timeout: 10_000 });
    await expect
      .poll(async () => pageImg.evaluate((img) => (img as HTMLImageElement).naturalWidth), { timeout: 15_000 })
      .toBeGreaterThan(0);

    await page.keyboard.press("Escape");
    await expect(page).toHaveURL(new RegExp(`#/book/${READER_BOOK_ID}$`));
    await page.locator("#back-btn").waitFor();
  });

  test("theme toggle switches data-theme on <html> and persists across reload", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("carrel_theme", "light"));
    await page.reload();
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");

    await page.click("#theme-toggle-btn");
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
    const stored = await page.evaluate(() => localStorage.getItem("carrel_theme"));
    expect(stored).toBe("dark");

    await page.reload();
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  });

  test("'/' focuses the search input", async ({ page }) => {
    await page.goto("/");
    await page.locator(".grid .card").first().waitFor();
    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());

    await page.keyboard.press("/");
    await expect(page.locator("#search")).toBeFocused();
  });
});
