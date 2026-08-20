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

// M5: cross-book "See all" vocabulary screen (#/vocabulary) — global search,
// sort, and delete over every saved word, reachable from the header nav
// cluster and from the M4 drawer's "See all" link. Backed by the same
// `GET /api/vocabulary` (no `bookId`) as M4 — nothing new on the backend.
const OTHER_BOOK_ID = "e2e-book-001";
const OTHER_BOOK_TITLE = "Book 001";

async function waitForDictStatus(page: Page) {
  // The Vocabulary nav icon's visibility depends on the same session-cached
  // `GET /api/dictionary/status` fetch as the reader's vocab-btn — see
  // openEpubReader above.
  await expect
    .poll(() => page.evaluate(() => window.__dictStatusForTest ?? null))
    .not.toBeNull();
}

async function seedWord(
  page: Page,
  opts: {
    word: string;
    lemma: string;
    definition: string;
    bookId?: string;
    bookTitle?: string;
  }
) {
  const resp = await page.request.post("/api/vocabulary", {
    data: {
      word: opts.word,
      lemma: opts.lemma,
      definition: opts.definition,
      bookId: opts.bookId,
      bookTitle: opts.bookTitle,
    },
  });
  expect(resp.status()).toBe(204);
}

test.describe("vocabulary screen (#/vocabulary)", () => {
  // This screen lists EVERY saved word, not just one book's — clear the
  // whole table before each test, not just EPUB_ID's (the outer beforeEach
  // above only clears that one book, but these tests also seed
  // OTHER_BOOK_ID and bookId-less rows).
  test.beforeEach(async ({ request }) => {
    const resp = await request.get("/api/vocabulary");
    if (!resp.ok()) return;
    for (const w of await resp.json()) {
      await request.delete(`/api/vocabulary/${w.id}`);
    }
  });

  test("header nav cluster shows a Vocabulary icon that opens the screen", async ({ page }) => {
    await page.goto("/#/");
    await waitForDictStatus(page);
    const navIcon = page.locator('[data-nav="vocabulary"]');
    await expect(navIcon).toBeVisible();
    await navIcon.click();
    await expect(page).toHaveURL(/#\/vocabulary$/);
    await expect(page.locator(".vocab-screen")).toBeVisible();
  });

  test("empty state before any word is saved", async ({ page }) => {
    await page.goto("/#/vocabulary");
    await expect(page.locator(".vocab-screen .empty")).toContainText("No saved words yet");
  });

  test("lists words saved from different books, each with its own book label", async ({ page }) => {
    await seedWord(page, { word: "cat", lemma: "cat-m5-a", definition: "feline mammal", bookId: EPUB_ID, bookTitle: "Book 050" });
    await seedWord(page, { word: "dog", lemma: "dog-m5-a", definition: "canine mammal", bookId: OTHER_BOOK_ID, bookTitle: OTHER_BOOK_TITLE });

    await page.goto("/#/vocabulary");
    const entries = page.locator(".vocab-screen-entry");
    await expect(entries).toHaveCount(2);
    await expect(entries.filter({ hasText: "cat" })).toContainText("Book 050");
    await expect(entries.filter({ hasText: "dog" })).toContainText(OTHER_BOOK_TITLE);
  });

  // A word's source book can be deleted while the word survives
  // (`vocabulary.book_id` is nullable) — it must still render, with no book
  // label, and still be deletable.
  test("a word with no book still renders and can be deleted", async ({ page }) => {
    await seedWord(page, { word: "orphan", lemma: "orphan-m5", definition: "left behind" });

    await page.goto("/#/vocabulary");
    const entry = page.locator(".vocab-screen-entry");
    await expect(entry).toHaveCount(1);
    await expect(entry).toContainText("orphan");

    await entry.locator(".vocab-entry-delete").click();
    await expect(page.locator(".vocab-screen-entry")).toHaveCount(0);
    const resp = await page.request.get("/api/vocabulary");
    expect(await resp.json()).toHaveLength(0);
  });

  // A row that should be excluded must actually be ABSENT, not merely
  // outnumbered by the matching row — an assertion that only checks the
  // match's presence would still pass with filtering deleted entirely.
  test("search filters the list", async ({ page }) => {
    await seedWord(page, { word: "ephemeral", lemma: "ephemeral-m5", definition: "lasting a short time", bookId: EPUB_ID, bookTitle: "Book 050" });
    await seedWord(page, { word: "gregarious", lemma: "gregarious-m5", definition: "fond of company", bookId: OTHER_BOOK_ID, bookTitle: OTHER_BOOK_TITLE });

    await page.goto("/#/vocabulary");
    await expect(page.locator(".vocab-screen-entry")).toHaveCount(2);

    await page.locator("#vocab-filter").fill("ephemeral");
    await expect(page.locator(".vocab-screen-entry")).toHaveCount(1);
    await expect(page.locator(".vocab-screen-entry")).toContainText("ephemeral");
    await expect(page.locator(".vocab-screen-entry", { hasText: "gregarious" })).toHaveCount(0);
    // The debounced re-render replaces the search box itself, so keep typing
    // after it: focus must survive, or the next keystrokes go nowhere.
    expect(await page.evaluate(() => document.activeElement?.id)).toBe("vocab-filter");
    await page.keyboard.type("XYZ");
    await expect(page.locator("#vocab-filter")).toHaveValue("ephemeralXYZ");
    await expect(page.locator(".vocab-screen-entry")).toHaveCount(0);
    await expect(page.locator(".vocab-screen .empty")).toContainText("No matches");
  });

  // The sort toggle must actually reorder the rows — comparing the first
  // row's text before and after, not merely that both words are present.
  test("sort toggle reorders the list", async ({ page }) => {
    // "apple" seeded first (older), "zebra" second (newer): newest-first
    // puts zebra on top, alphabetical puts apple on top — the two orders
    // disagree, so whichever one the toggle produces is unambiguous. A full
    // second apart so created_at (whole-second resolution) can't tie.
    await seedWord(page, { word: "apple", lemma: "apple-m5", definition: "a fruit", bookId: EPUB_ID, bookTitle: "Book 050" });
    await new Promise((r) => setTimeout(r, 1100));
    await seedWord(page, { word: "zebra", lemma: "zebra-m5", definition: "an animal", bookId: OTHER_BOOK_ID, bookTitle: OTHER_BOOK_TITLE });

    await page.goto("/#/vocabulary");
    await expect(page.locator(".vocab-screen-entry")).toHaveCount(2);
    const firstWord = () => page.locator(".vocab-screen-entry .vocab-entry-word").first().innerText();
    await expect.poll(firstWord).toContain("zebra"); // newest first, the default

    await page.locator("#vocab-sort").click();
    await expect.poll(firstWord).toContain("apple"); // alphabetical
  });

  // Verified against the server, not the DOM: a stale client-side removal
  // would pass a DOM-only assertion but leave the row on reload.
  test("delete removes the row from the server", async ({ page }) => {
    await seedWord(page, { word: "cat", lemma: "cat-m5-b", definition: "feline mammal", bookId: EPUB_ID, bookTitle: "Book 050" });

    await page.goto("/#/vocabulary");
    await expect(page.locator(".vocab-screen-entry")).toHaveCount(1);
    await page.locator(".vocab-entry-delete").click();
    await expect(page.locator(".vocab-screen-entry")).toHaveCount(0);

    await page.reload();
    await expect(page.locator(".vocab-screen-entry")).toHaveCount(0);
    const resp = await page.request.get("/api/vocabulary");
    expect(await resp.json()).toHaveLength(0);
  });

  test("the drawer's See all link opens the same screen", async ({ page }) => {
    await openEpubReader(page);
    await seedWord(page, { word: "cat", lemma: "cat-m5-c", definition: "feline mammal", bookId: EPUB_ID, bookTitle: "Book 050" });
    await page.locator("#vocab-btn").click();
    await expect(page.locator("#vocab-panel")).toBeVisible();
    await page.locator("#vocab-see-all").click();
    await expect(page).toHaveURL(/#\/vocabulary$/);
    await expect(page.locator(".vocab-screen-entry")).toHaveCount(1);
  });
});

// M6: flashcard review, added to the same #/vocabulary screen (no new route)
// so it inherits the M5 lifecycle fix's per-visit token/container scoping.
async function markReviewed(page: Page, id: string, correct: boolean) {
  const resp = await page.request.post(`/api/vocabulary/${id}/review`, { data: { correct } });
  expect(resp.status()).toBe(204);
}

test.describe("vocabulary screen — flashcard review (M6)", () => {
  test.beforeEach(async ({ request }) => {
    const resp = await request.get("/api/vocabulary");
    if (!resp.ok()) return;
    for (const w of await resp.json()) {
      await request.delete(`/api/vocabulary/${w.id}`);
    }
  });

  test("the review bar reflects the due count and is disabled at zero", async ({ page }) => {
    await page.goto("/#/vocabulary");
    // No saved words at all yet — nothing to review, bar isn't shown.
    await expect(page.locator("#vocab-review-btn")).toHaveCount(0);

    await seedWord(page, { word: "cat", lemma: "cat-m6-a", definition: "feline mammal" });
    // reload(), not a second goto() to the identical URL: the screen only
    // re-reads the due queue on a real load, and goto to a URL the page is
    // already on is not a dependable reload. That made this test flaky —
    // usually passing, occasionally rendering the pre-seed (empty) state.
    await page.reload();
    const reviewBtn = page.locator("#vocab-review-btn");
    await expect(reviewBtn).toContainText("Review 1 due");
    await expect(reviewBtn).toBeEnabled();
  });

  // The definition must be genuinely ABSENT from the DOM pre-reveal, not just
  // hidden by CSS — a test that only checked visibility would still pass with
  // the definition rendered behind a `display:none`.
  test("the card's definition is hidden until revealed", async ({ page }) => {
    await seedWord(page, { word: "cat", lemma: "cat-m6-b", definition: "a very particular feline gloss" });

    await page.goto("/#/vocabulary");
    await page.locator("#vocab-review-btn").click();
    await expect(page.locator(".vocab-review-card")).toContainText("cat");
    await expect(page.locator(".vocab-review-card")).not.toContainText("a very particular feline gloss");
    await expect(page.locator("#vocab-review-reveal")).toBeVisible();

    await page.locator("#vocab-review-reveal").click();
    await expect(page.locator(".vocab-review-card")).toContainText("a very particular feline gloss");
    await expect(page.locator("#vocab-review-reveal")).toHaveCount(0);
  });

  // A row already scheduled for the future must be excluded from the review
  // QUEUE itself, not merely from the count — seeded here via a real prior
  // review through the API (the same route the UI uses), so this proves the
  // queue-building fetch honors `next_due_at`, not just that one word shows up.
  test("a word that isn't due yet never appears in the review queue", async ({ page }) => {
    await seedWord(page, { word: "cat", lemma: "cat-m6-c", definition: "feline mammal" });
    await seedWord(page, { word: "dog", lemma: "dog-m6-c", definition: "canine mammal" });
    const dogId = (await (await page.request.get("/api/vocabulary")).json())
      .find((w: { lemma: string; id: string }) => w.lemma === "dog-m6-c").id;
    await markReviewed(page, dogId, true); // pushes dog's next_due_at days out

    await page.goto("/#/vocabulary");
    await expect(page.locator("#vocab-review-btn")).toContainText("Review 1 due");
    await page.locator("#vocab-review-btn").click();
    await expect(page.locator(".vocab-review-card")).toContainText("cat");
    await expect(page.locator(".vocab-review-progress")).toContainText("1 / 1");
  });

  // Persistence verified against the server (not just the UI's own optimism),
  // and the due count is asserted to have actually CHANGED after the session
  // — not merely that the review screen returned to the list.
  test("marking a card correct persists the review and the due count drops", async ({ page }) => {
    await seedWord(page, { word: "cat", lemma: "cat-m6-d", definition: "feline mammal" });
    const id = (await (await page.request.get("/api/vocabulary")).json())[0].id;

    await page.goto("/#/vocabulary");
    await page.locator("#vocab-review-btn").click();
    await page.locator("#vocab-review-reveal").click();
    await page.locator("#vocab-review-gotit").click();
    await expect(page.locator(".vocab-review")).toContainText("All caught up");

    await page.locator("#vocab-review-back").click();
    await expect(page.locator("#vocab-review-btn")).toContainText("Review 0 due");

    const word = (await (await page.request.get("/api/vocabulary")).json())
      .find((w: { id: string }) => w.id === id);
    expect(word.box).toBe(2);
    expect(word.nextDueAt).not.toBeNull();
  });

  // Missing a card must persist a real review (box stays at 1, its floor, and
  // `nextDueAt` moves from null to a real timestamp) — distinguishes "the
  // Missed button actually called the API" from "the UI just advanced the
  // queue locally". (Box>1 -> reset-to-1 on a miss is covered at the Rust
  // handler level, `review_vocabulary_word_route_wrong_resets_box_to_one`:
  // advancing a box here first would reschedule the word days out and make it
  // fall out of THIS due queue before the miss could be exercised through the
  // UI against a real server clock.)
  test("marking a card wrong persists the review", async ({ page }) => {
    await seedWord(page, { word: "cat", lemma: "cat-m6-e", definition: "feline mammal" });
    const id = (await (await page.request.get("/api/vocabulary")).json())[0].id;

    await page.goto("/#/vocabulary");
    await page.locator("#vocab-review-btn").click();
    await page.locator("#vocab-review-reveal").click();
    await page.locator("#vocab-review-missed").click();
    await expect(page.locator(".vocab-review")).toContainText("All caught up");

    const word = (await (await page.request.get("/api/vocabulary")).json())
      .find((w: { id: string }) => w.id === id);
    expect(word.box).toBe(1);
    expect(word.nextDueAt).not.toBeNull();
  });
});

// Both review passes flagged these two paths as reachable-but-untested: a
// render fired after the user left (or re-entered) the screen reached through
// document-global selectors, which either threw — swallowing the delete path's
// showLogin() — or rebound a NEWER screen's controls to a dead closure.
//
// The debounce is driven with page.clock, not real time: racing a 200ms timer
// against two navigations is exactly the kind of test that passes for timing
// reasons rather than behavioural ones (an earlier real-time version of these
// two passed against the unfixed code). With the clock installed, the timer
// fires precisely when fastForward says, so a regression cannot hide in a
// lucky interleaving.
test.describe("vocabulary screen lifecycle", () => {
  test.beforeEach(async ({ request }) => {
    const resp = await request.get("/api/vocabulary");
    if (!resp.ok()) return;
    for (const w of await resp.json()) {
      await request.delete(`/api/vocabulary/${w.id}`);
    }
  });

  // NOTE on what this one is worth: it does NOT fail against the unfixed code
  // (an exception thrown inside a faked-clock timer callback does not surface
  // as a pageerror), so it is a guard against a future render() writing into
  // the live view — not evidence for the fix. The test below IS the one that
  // fails without it.
  test("a debounce that fires after navigating away leaves the new view alone", async ({ page }) => {
    const errors: string[] = [];
    page.on("pageerror", (e) => errors.push(String(e)));
    await seedWord(page, { word: "cat", lemma: "cat-life-a", definition: "feline mammal", bookId: EPUB_ID, bookTitle: "Book 050" });

    await page.clock.install();
    await page.goto("/#/vocabulary");
    await expect(page.locator(".vocab-screen-entry")).toHaveCount(1);

    // Queue the 200ms debounce, then leave in-page (a full goto would reload
    // the document and take the pending timer with it — nothing under test).
    await page.locator("#vocab-filter").fill("ca");
    await page.evaluate(() => { location.hash = "#/"; });
    await expect(page.locator("#library-content")).toBeVisible();

    // Now let the orphaned timer fire into the dead view.
    await page.clock.fastForward(1000);
    expect(errors).toEqual([]);
    await expect(page.locator(".vocab-screen")).toHaveCount(0);
    await expect(page.locator("#library-content")).toBeVisible();
  });

  test("a stale visit's debounce cannot hijack a re-entered screen", async ({ page }) => {
    const errors: string[] = [];
    page.on("pageerror", (e) => errors.push(String(e)));
    await seedWord(page, { word: "apple", lemma: "apple-life", definition: "a fruit", bookId: EPUB_ID, bookTitle: "Book 050" });
    await seedWord(page, { word: "zebra", lemma: "zebra-life", definition: "an animal", bookId: EPUB_ID, bookTitle: "Book 050" });

    await page.clock.install();
    await page.goto("/#/vocabulary");
    await expect(page.locator(".vocab-screen-entry")).toHaveCount(2);

    // Queue visit A's debounce, then leave and re-enter so visit B is live.
    await page.locator("#vocab-filter").fill("apple");
    await page.evaluate(() => { location.hash = "#/"; });
    await expect(page.locator("#library-content")).toBeVisible();
    await page.evaluate(() => { location.hash = "#/vocabulary"; });
    await expect(page.locator(".vocab-screen-entry")).toHaveCount(2);
    await expect(page.locator("#vocab-filter")).toHaveValue("");

    // A's timer lands now, against B's DOM. It must not filter B's list, and
    // must not rebind B's controls to A's dead closure.
    await page.clock.fastForward(1000);
    await expect(page.locator(".vocab-screen-entry")).toHaveCount(2);
    await expect(page.locator("#vocab-filter")).toHaveValue("");

    // B's own controls still drive B's list — proof its handlers survived.
    await page.locator("#vocab-filter").fill("zebra");
    await page.clock.fastForward(1000);
    await expect(page.locator(".vocab-screen-entry")).toHaveCount(1);
    await expect(page.locator(".vocab-screen-entry")).toContainText("zebra");
    expect(errors).toEqual([]);
  });
});
