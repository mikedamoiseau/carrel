import { test, expect, Page } from "@playwright/test";

// M4: saved-words (vocabulary) drawer in the web reader — book-scoped list +
// jump + delete. Mirrors highlights.spec.ts/bookmarks.spec.ts's drawer
// patterns; mirrors dictionary.spec.ts's reader-open helper and status-ready
// wait, since the drawer trigger's visibility depends on the same
// session-cached `GET /api/dictionary/status` fetch.
const EPUB_ID = "e2e-book-050";
test.use({ serviceWorkers: "block" });

declare global {
  interface Window {
    __dictStatusForTest?: { installed: boolean; enabled: boolean; vocabulary: boolean } | null;
  }
}

async function openEpubReader(page: Page) {
  await page.goto(`/#/book/${EPUB_ID}/0/read`);
  const restart = page.locator("#resume-restart-btn");
  const content = page.locator("#reader-content");
  await expect(restart.or(content)).toBeVisible({ timeout: 15_000 });
  if (await restart.isVisible()) { await restart.click(); await content.waitFor(); }
  await expect(content).toContainText("chapter zero", { timeout: 10_000 });
  // The vocab-btn's visibility depends on the same async status fetch as
  // Define (dictionary.spec.ts) — wait for it before asserting on the button.
  await expect
    .poll(() => page.evaluate(() => window.__dictStatusForTest ?? null))
    .not.toBeNull();
}

// Chapter-0 rendered text contains "A cat rested quietly on the windowsill."
// (see web_e2e_server.rs) — compute real offsets against the live DOM rather
// than hardcoding them, same technique as highlights.spec.ts's
// chapterOffsetsOf.
async function chapterOffsetsOf(page: Page, needle: string) {
  return page.evaluate((needle) => {
    const el = document.querySelector("#reader-content")!;
    const text = el.textContent!;
    const s = text.indexOf(needle);
    return { s, e: s + needle.length };
  }, needle);
}

async function seedVocabWord(
  page: Page,
  opts: { chapterIndex: number; startOffset: number; endOffset: number }
) {
  const resp = await page.request.post("/api/vocabulary", {
    data: {
      word: "cat",
      lemma: "cat",
      pos: "n",
      definition: "feline mammal",
      bookId: EPUB_ID,
      bookTitle: "Book 050",
      chapterIndex: opts.chapterIndex,
      startOffset: opts.startOffset,
      endOffset: opts.endOffset,
    },
  });
  expect(resp.status()).toBe(204);
}

// Vocabulary rows persist to the harness's shared on-disk DB (seeded once at
// server start, not reset per test) — clear this book's saved words before
// each test so counts start from a clean slate (highlights.spec.ts precedent;
// `lemma` is globally UNIQUE, so a stray "cat" row from dictionary.spec.ts's
// Save-button test would otherwise leak into these assertions too).
test.beforeEach(async ({ request }) => {
  const resp = await request.get(`/api/vocabulary?bookId=${EPUB_ID}`);
  if (!resp.ok()) return;
  for (const w of await resp.json()) {
    await request.delete(`/api/vocabulary/${w.id}`);
  }
});

test.describe("saved words drawer", () => {
  test("toolbar shows a saved-words trigger", async ({ page }) => {
    await openEpubReader(page);
    await expect(page.locator("#vocab-btn")).toBeVisible();
  });

  test("empty state before any word is saved", async ({ page }) => {
    await openEpubReader(page);
    await page.locator("#vocab-btn").click();
    await expect(page.locator("#vocab-panel")).toBeVisible();
    await expect(page.locator(".vocab-empty")).toContainText("No saved words in this book yet");
  });

  test("a saved word appears in the drawer with its definition and chapter", async ({ page }) => {
    await openEpubReader(page);
    const { s, e } = await chapterOffsetsOf(page, "cat");
    expect(s).toBeGreaterThan(-1);
    await seedVocabWord(page, { chapterIndex: 0, startOffset: s, endOffset: e });

    await page.locator("#vocab-btn").click();
    const entry = page.locator(".vocab-entry");
    await expect(entry).toHaveCount(1);
    await expect(entry).toContainText("cat");
    await expect(entry).toContainText("feline mammal");
    await expect(entry).toContainText("Chapter Zero"); // real EPUB3 nav label
  });

  test("tapping a different-chapter row jumps back and closes the panel", async ({ page }) => {
    await openEpubReader(page);
    const { s, e } = await chapterOffsetsOf(page, "cat");
    expect(s).toBeGreaterThan(-1);
    await seedVocabWord(page, { chapterIndex: 0, startOffset: s, endOffset: e });

    await page.locator("#next-btn").click();
    await expect(page.locator("#reader-content")).toContainText("chapter one");

    await page.locator("#vocab-btn").click();
    await page.locator(".vocab-entry").first().click();
    await expect(page.locator("#reader-content")).toContainText("chapter zero");
    await expect(page.locator("#vocab-panel")).toBeHidden();
    // The jump must land ON the word, not merely on its chapter — that offset
    // resolve (resolveVocabWordRange -> rangeFromChapterOffsets ->
    // scrollRangeIntoView) is M4's only genuinely new machinery, and chapter
    // navigation alone would satisfy every assertion above it. "A cat rested
    // quietly on the windowsill." sits after 60 lorem paragraphs in the
    // harness EPUB, so a working resolve scrolls well past the top and leaves
    // the word inside the stage's visible box; a broken one parks at 0.
    await expect
      .poll(() => page.evaluate(() => document.getElementById("reader-stage")!.scrollTop))
      .toBeGreaterThan(0);
    const visible = await page.evaluate(() => {
      const stage = document.getElementById("reader-stage")!;
      const walker = document.createTreeWalker(
        document.querySelector("#reader-content")!,
        NodeFilter.SHOW_TEXT
      );
      let node: Node | null = null;
      let off = -1;
      while ((node = walker.nextNode())) {
        off = node.nodeValue!.indexOf("cat");
        if (off !== -1) break;
      }
      if (!node) return null;
      const range = document.createRange();
      range.setStart(node, off);
      range.setEnd(node, off + "cat".length);
      const r = range.getBoundingClientRect();
      const s = stage.getBoundingClientRect();
      return { inside: r.top >= s.top && r.bottom <= s.bottom };
    });
    expect(visible?.inside).toBe(true);
  });

  // `chapter_index` is nullable, so a word can exist with no chapter to jump
  // to. Such a row must not advertise a jump it cannot perform.
  test("a word saved without a chapter renders as an inert row", async ({ page }) => {
    await openEpubReader(page);
    const resp = await page.request.post("/api/vocabulary", {
      data: {
        word: "dog",
        lemma: "dog",
        definition: "canine mammal",
        bookId: EPUB_ID,
        bookTitle: "Book 050",
      },
    });
    expect(resp.status()).toBe(204);

    await page.locator("#vocab-btn").click();
    const entry = page.locator(".vocab-entry");
    await expect(entry).toHaveCount(1);
    await expect(entry).toHaveClass(/vocab-entry-static/);
    expect(await entry.getAttribute("role")).toBeNull();
    expect(await entry.getAttribute("tabindex")).toBeNull();
    // Clicking it is a no-op: the reader stays on the chapter it was on and
    // the drawer stays open (a jump would close it).
    await entry.click();
    await expect(page.locator("#vocab-panel")).toBeVisible();
    await expect(page.locator("#reader-content")).toContainText("chapter zero");
  });

  test("delete removes the row and shows the empty state again", async ({ page }) => {
    await openEpubReader(page);
    const { s, e } = await chapterOffsetsOf(page, "cat");
    expect(s).toBeGreaterThan(-1);
    await seedVocabWord(page, { chapterIndex: 0, startOffset: s, endOffset: e });

    await page.locator("#vocab-btn").click();
    await expect(page.locator(".vocab-entry")).toHaveCount(1);
    await page.locator(".vocab-entry-delete").first().click();
    await expect(page.locator(".vocab-entry")).toHaveCount(0);
    await expect(page.locator(".vocab-empty")).toContainText("No saved words in this book yet");
  });

  test("saving a word from the Define popover shows up in the drawer", async ({ page }) => {
    await openEpubReader(page);
    // Programmatic selection of "cat", same technique as dictionary.spec.ts.
    await page.evaluate(() => {
      const el = document.querySelector("#reader-content")!;
      const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
      let node: Node | null = null;
      let off = -1;
      while ((node = walker.nextNode())) {
        off = node.nodeValue!.indexOf("cat");
        if (off !== -1) break;
      }
      const range = document.createRange();
      range.setStart(node!, off);
      range.setEnd(node!, off + "cat".length);
      const sel = window.getSelection()!;
      sel.removeAllRanges();
      sel.addRange(range);
      document.dispatchEvent(new Event("selectionchange"));
    });
    await page.locator("#hl-popover #hl-define-btn").click();
    await page.locator("#dict-popover #dict-save-btn").click();
    await expect(page.locator("#dict-popover #dict-save-btn")).toHaveText("Saved ✓");
    await page.keyboard.press("Escape"); // dismiss the Define result card

    await page.locator("#vocab-btn").click();
    const entry = page.locator(".vocab-entry");
    await expect(entry).toHaveCount(1);
    await expect(entry).toContainText("cat");
    await expect(entry).toContainText("feline mammal");
  });
});
