import { test, expect, Page } from "@playwright/test";

// F-1-1/M2: "Define" in the web reader — selection popover gains a Define
// button when the dictionary is installed+enabled, and tapping it shows a
// small result popover with the matched word's senses (or a not-found
// message). Mirrors highlights.spec.ts's selection/popover patterns.
const EPUB_ID = "e2e-book-050";
test.use({ serviceWorkers: "block" });

declare global {
  interface Window {
    // Test hook set by app.js once the session dictionary-status fetch
    // resolves: {installed, enabled} or null before it lands.
    __dictStatusForTest?: { installed: boolean; enabled: boolean } | null;
  }
}

async function openEpubReader(page: Page) {
  await page.goto(`/#/book/${EPUB_ID}/0/read`);
  const restart = page.locator("#resume-restart-btn");
  const content = page.locator("#reader-content");
  await expect(restart.or(content)).toBeVisible({ timeout: 15_000 });
  if (await restart.isVisible()) { await restart.click(); await content.waitFor(); }
  await expect(content).toContainText("chapter zero", { timeout: 10_000 });
  // The Define button's availability depends on an async status fetch kicked
  // off at chrome-render time; wait for it to land before selecting text, or
  // the very first popover could be built before the cache is warm.
  await expect
    .poll(() => page.evaluate(() => window.__dictStatusForTest ?? null))
    .not.toBeNull();
}

// Selects `needle` (must appear verbatim in #reader-content's text) via a
// programmatic Range + selectionchange dispatch, same technique as
// highlights.spec.ts.
async function selectText(page: Page, needle: string) {
  await page.evaluate((needle) => {
    const el = document.querySelector("#reader-content")!;
    const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
    let node: Node | null = null;
    let off = -1;
    while ((node = walker.nextNode())) {
      off = node.nodeValue!.indexOf(needle);
      if (off !== -1) break;
    }
    const range = document.createRange();
    range.setStart(node!, off);
    range.setEnd(node!, off + needle.length);
    const sel = window.getSelection()!;
    sel.removeAllRanges();
    sel.addRange(range);
    document.dispatchEvent(new Event("selectionchange"));
  }, needle);
}

test.describe("Define (dictionary lookup)", () => {
  test("selecting text shows a Define button in the highlight popover", async ({ page }) => {
    await openEpubReader(page);
    await selectText(page, "cat");
    const popover = page.locator("#hl-popover");
    await expect(popover).toBeVisible();
    const defineBtn = popover.locator("#hl-define-btn");
    await expect(defineBtn).toBeVisible();
    await expect(defineBtn).toHaveAttribute("aria-label", "Define");
  });

  test("Define on a seeded word shows its gloss", async ({ page }) => {
    await openEpubReader(page);
    await selectText(page, "cat");
    await page.locator("#hl-popover #hl-define-btn").click();
    const result = page.locator("#dict-popover");
    await expect(result).toBeVisible();
    await expect(result).toHaveAttribute("role", "dialog");
    await expect(result).toHaveAttribute("aria-label", 'Definition of "cat"');
    await expect(result).toContainText("cat");
    await expect(result).toContainText("feline mammal");
    // Selection popover is gone — Define replaces it, doesn't stack with it.
    await expect(page.locator("#hl-popover")).toBeHidden();
  });

  test("Define on a word absent from the dictionary shows the not-found message", async ({ page }) => {
    await openEpubReader(page);
    // "lorem" appears in the seeded filler paragraphs but isn't one of the
    // handful of words write_test_artifact seeds (cat/run/mouse/light).
    await selectText(page, "lorem");
    await page.locator("#hl-popover #hl-define-btn").click();
    const result = page.locator("#dict-popover");
    await expect(result).toBeVisible();
    await expect(result).toContainText("No definition found for 'lorem'");
  });

  test("Esc dismisses the Define result popover", async ({ page }) => {
    await openEpubReader(page);
    await selectText(page, "cat");
    await page.locator("#hl-popover #hl-define-btn").click();
    await expect(page.locator("#dict-popover")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.locator("#dict-popover")).toBeHidden();
  });

  test("tapping outside dismisses the Define result popover", async ({ page }) => {
    await openEpubReader(page);
    await selectText(page, "cat");
    await page.locator("#hl-popover #hl-define-btn").click();
    await expect(page.locator("#dict-popover")).toBeVisible();
    await page.locator("#reader-content").click({ position: { x: 5, y: 5 } });
    await expect(page.locator("#dict-popover")).toBeHidden();
  });

  // Toggling `dictionary_enabled` per test would need a settings-mutation
  // route; the web API only exposes GET /api/dictionary/status and
  // GET /api/dictionary/lookup (M1), and the e2e harness is one shared,
  // serial (workers: 1) server process for the whole suite, seeded once at
  // startup with the setting on. There is no way to flip it off for a single
  // test without racing every other spec file that also reads dictionary
  // status. Covered instead by the Rust unit tests in
  // src-tauri/src/web_server/api.rs (dictionary_lookup_disabled_setting_is_
  // service_unavailable) and by inspection of app.js's dictionaryAvailable()
  // gate, which the tests above already exercise in the enabled case.
  test.skip("no Define button offered when the dictionary is disabled", () => {});
});
