import { test, expect, Page } from "@playwright/test";

const EPUB_ID = "e2e-book-050";
test.use({ serviceWorkers: "block" });

declare global {
  interface Window {
    // Test hook set by app.js: the pending highlight-jump's id, or null.
    __hlJumpPendingForTest?: string | null;
  }
}

async function openEpubReader(page: Page) {
  await page.goto(`/#/book/${EPUB_ID}/0/read`);
  const restart = page.locator("#resume-restart-btn");
  const content = page.locator("#reader-content");
  await expect(restart.or(content)).toBeVisible({ timeout: 15_000 });
  if (await restart.isVisible()) { await restart.click(); await content.waitFor(); }
  await expect(content).toContainText("chapter zero", { timeout: 10_000 });
}

// Chapter-0 rendered text contains: "Fish & chips 🦀 the quick brown fox …"
// Compute offsets of a substring within the rendered chapter text at runtime
// so the seed matches reality (UTF-16 code units, textContent basis).
async function chapterOffsetsOf(page: Page, needle: string) {
  return page.evaluate((needle) => {
    const el = document.querySelector("#reader-content")!;
    const text = el.textContent!;
    const s = text.indexOf(needle);
    return { s, e: s + needle.length };
  }, needle);
}

async function seedHighlight(page: Page, needle: string, color = "#f6c445") {
  const { s, e } = await chapterOffsetsOf(page, needle);
  expect(s).toBeGreaterThan(-1);
  const resp = await page.request.post(`/api/books/${EPUB_ID}/highlights`, {
    data: { chapterIndex: 0, text: needle, color, startOffset: s, endOffset: e },
  });
  expect(resp.status()).toBe(201);
  return resp.json();
}

test.beforeEach(async ({ request }) => {
  const resp = await request.get(`/api/books/${EPUB_ID}/highlights`);
  if (!resp.ok()) return;
  for (const hl of await resp.json()) {
    await request.delete(`/api/books/${EPUB_ID}/highlights/${hl.id}`);
  }
});

test.describe("highlight create (selection popover)", () => {
  test("select text → popover → swatch creates a persisted highlight", async ({ page }) => {
    await openEpubReader(page);
    // Programmatic selection of "brown fox" inside #reader-content.
    await page.evaluate(() => {
      const el = document.querySelector("#reader-content")!;
      const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
      let node: Node | null = null;
      let off = -1;
      while ((node = walker.nextNode())) {
        off = node.nodeValue!.indexOf("brown fox");
        if (off !== -1) break;
      }
      const range = document.createRange();
      range.setStart(node!, off);
      range.setEnd(node!, off + "brown fox".length);
      const sel = window.getSelection()!;
      sel.removeAllRanges();
      sel.addRange(range);
      document.dispatchEvent(new Event("selectionchange"));
    });
    const popover = page.locator("#hl-popover");
    await expect(popover).toBeVisible();
    await popover.locator('[data-color="#7bc47f"]').click();
    const mark = page.locator("#reader-content mark.hl-mark").first();
    await expect(mark).toHaveText("brown fox");
    // persisted server-side
    await expect
      .poll(async () => (await (await page.request.get(`/api/books/${EPUB_ID}/highlights`)).json()).length)
      .toBe(1);
    const rows = await (await page.request.get(`/api/books/${EPUB_ID}/highlights`)).json();
    expect(rows[0].color).toBe("#7bc47f");
    // popover gone, selection cleared
    await expect(popover).toBeHidden();
  });
});

test.describe("highlight edit (mark-tap popover)", () => {
  test("tapping a mark opens edit popover; delete unwraps", async ({ page }) => {
    await openEpubReader(page);
    await seedHighlight(page, "quick brown fox");
    await page.reload();
    await openEpubReader(page);
    await page.locator("mark.hl-mark").first().click();
    const edit = page.locator("#hl-edit-popover");
    await expect(edit).toBeVisible();
    await edit.locator("#hl-delete-btn").click();
    await expect(page.locator("mark.hl-mark")).toHaveCount(0);
    await expect
      .poll(async () => (await (await page.request.get(`/api/books/${EPUB_ID}/highlights`)).json()).length)
      .toBe(0);
  });

  test("recolor via mark-tap popover persists", async ({ page }) => {
    await openEpubReader(page);
    await seedHighlight(page, "quick brown fox", "#f6c445");
    await page.reload();
    await openEpubReader(page);
    await page.locator("mark.hl-mark").first().click();
    await page.locator('#hl-edit-popover [data-color="#e8a55d"]').click();
    await expect
      .poll(async () => (await (await page.request.get(`/api/books/${EPUB_ID}/highlights`)).json())[0].color)
      .toBe("#e8a55d");
    // mark re-tinted live
    const bg = await page.locator("mark.hl-mark").first()
      .evaluate((el) => (el as HTMLElement).style.backgroundColor !== "");
    expect(bg).toBe(true);
  });
});

test.describe("highlight rendering", () => {
  test("stored highlight renders as a mark after entity and emoji", async ({ page }) => {
    await openEpubReader(page);
    // Region AFTER "&" (1 rendered char vs 5 raw chars) and AFTER 🦀 (2 code
    // units) — proves textContent/UTF-16 basis, the desktop injector's bug.
    await seedHighlight(page, "quick brown fox");
    await page.reload();
    await openEpubReader(page);
    const mark = page.locator("#reader-content mark.hl-mark");
    await expect(mark.first()).toBeVisible();
    await expect(mark.first()).toHaveText("quick brown fox");
  });

  test("drift fallback re-anchors by quoted text", async ({ page }) => {
    await openEpubReader(page);
    const { s, e } = await chapterOffsetsOf(page, "lazy dog");
    // Deliberately wrong offsets (shifted +7), correct quote text.
    const resp = await page.request.post(`/api/books/${EPUB_ID}/highlights`, {
      data: { chapterIndex: 0, text: "lazy dog", color: "#f6c445",
              startOffset: s + 7, endOffset: e + 7 },
    });
    expect(resp.status()).toBe(201);
    await page.reload();
    await openEpubReader(page);
    await expect(page.locator("#reader-content mark.hl-mark").first())
      .toHaveText("lazy dog");
  });

  test("crossing overlaps nest and innermost is deterministic", async ({ page }) => {
    await openEpubReader(page);
    // A = "brown fox jumps", B = "fox jumps over" — B starts inside A and
    // ends after it (crossing, not containment).
    const a = await seedHighlight(page, "brown fox jumps", "#f6c445");
    const b = await seedHighlight(page, "fox jumps over", "#6ba3d6");
    await page.reload();
    await openEpubReader(page);
    // Shared region "fox jumps" sits inside marks of BOTH ids.
    const shared = page.locator(
      `#reader-content mark[data-hl-id="${b.id}"] >> text=fox jumps`).first();
    await expect(shared).toBeVisible();
    const nestedIds = await page.evaluate(() => {
      const inner = [...document.querySelectorAll("#reader-content mark.hl-mark mark.hl-mark")];
      return inner.map((m) => (m as HTMLElement).dataset.hlId);
    });
    expect(nestedIds.length).toBeGreaterThan(0);
    // Deterministic wrap order (spec §3): B sorts higher than A on
    // (startOffset, endOffset, id) — B starts later — so B must be the
    // INNERMOST mark on every shared fragment. If the wrap order ever
    // reversed, these inner marks would carry A's id instead.
    for (const id of nestedIds) expect(id).toBe(b.id);
  });

  test("marks survive chapter turn away and back", async ({ page }) => {
    await openEpubReader(page);
    await seedHighlight(page, "quick brown fox");
    await page.reload();
    await openEpubReader(page);
    await expect(page.locator("mark.hl-mark").first()).toBeVisible();
    await page.locator("#next-btn").click();
    await expect(page.locator("#reader-content")).toContainText("chapter one");
    await expect(page.locator("mark.hl-mark")).toHaveCount(0); // chapter 1 has none
    await page.locator("#prev-btn").click();
    await expect(page.locator("#reader-content")).toContainText("chapter zero");
    await expect(page.locator("mark.hl-mark").first()).toBeVisible();
  });
});

test.describe("highlights drawer", () => {
  test("trigger + empty state", async ({ page }) => {
    await openEpubReader(page);
    await page.locator("#hl-btn").click();
    await expect(page.locator("#hl-panel")).toBeVisible();
    await expect(page.locator(".hl-empty")).toContainText("Select text while reading");
  });

  test("lists rows with color dot, quote, chapter; delete removes", async ({ page }) => {
    await openEpubReader(page);
    await seedHighlight(page, "quick brown fox", "#6ba3d6");
    await page.reload();
    await openEpubReader(page);
    await page.locator("#hl-btn").click();
    const row = page.locator(".hl-entry");
    await expect(row).toHaveCount(1);
    await expect(row.first()).toContainText("quick brown fox");
    await expect(row.first().locator(".hl-dot")).toBeVisible();
    await expect(row.first().locator(".hl-entry-chapter")).toBeVisible();
    await row.first().locator(".hl-entry-delete").click();
    await expect(page.locator(".hl-entry")).toHaveCount(0);
    await expect(page.locator("mark.hl-mark")).toHaveCount(0);
  });

  test("mutual exclusion with the bookmark panel, both directions", async ({ page }) => {
    await openEpubReader(page);
    await page.locator("#hl-btn").click();
    await expect(page.locator("#hl-panel")).toBeVisible();
    await page.locator("#bookmark-btn").click();
    await expect(page.locator("#bookmark-panel")).toBeVisible();
    await expect(page.locator("#hl-panel")).toBeHidden();
    await page.locator("#hl-btn").click();
    await expect(page.locator("#hl-panel")).toBeVisible();
    await expect(page.locator("#bookmark-panel")).toBeHidden();
  });

  test("Esc closes the drawer", async ({ page }) => {
    await openEpubReader(page);
    await page.locator("#hl-btn").click();
    await expect(page.locator("#hl-panel")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.locator("#hl-panel")).toBeHidden();
    // Esc was consumed by the panel — the reader itself stays open.
    await expect(page.locator("#reader-content")).toBeVisible();
  });
});

// Seed a highlight in chapter 1 ("chapter one" text) while chapter 1 is
// rendered, then return to chapter 0 — the starting state for jump tests.
async function seedChapterOneHighlight(page: Page) {
  await page.locator("#next-btn").click();
  await expect(page.locator("#reader-content")).toContainText("chapter one");
  const { s, e } = await chapterOffsetsOf(page, "chapter one");
  const resp = await page.request.post(`/api/books/${EPUB_ID}/highlights`, {
    data: { chapterIndex: 1, text: "chapter one", color: "#f6c445", startOffset: s, endOffset: e },
  });
  expect(resp.status()).toBe(201);
  await page.locator("#prev-btn").click();
  await expect(page.locator("#reader-content")).toContainText("chapter zero");
}

test.describe("highlight jump (navId token)", () => {
  test("drawer jump navigates cross-chapter and scrolls to the mark", async ({ page }) => {
    await openEpubReader(page);
    await seedChapterOneHighlight(page);
    await page.reload();
    await openEpubReader(page);
    await page.locator("#hl-btn").click();
    await page.locator(".hl-entry").first().click();
    await expect(page.locator("#reader-content")).toContainText("chapter one");
    await expect(page.locator("mark.hl-mark").first()).toBeVisible();
    // The jump must actually scroll the mark into view — not merely render it
    // (guards against a scroll-restore undoing the jump's scrollIntoView).
    await expect(page.locator("mark.hl-mark").first()).toBeInViewport();
    await expect(page.locator("#hl-panel")).toBeHidden();
    // Token consumed by the render that carried its navId. The hook must
    // EXIST (set by startHlJump) — `in` distinguishes "consumed" from
    // "never minted".
    const pending = await page.evaluate(() =>
      "__hlJumpPendingForTest" in window ? window.__hlJumpPendingForTest : "hook-missing");
    expect(pending).toBeNull();
  });

  // The test above seeds its highlight on "chapter one" — the chapter's FIRST
  // text, which is on screen at scrollTop 0 whether or not the jump scrolled,
  // so toBeInViewport there cannot fail. This one seeds deep in the chapter
  // (after 60 lorem paragraphs), where only a jump that survives the
  // post-render scroll restore can bring the mark into view.
  test("jump to a mark deep in the chapter survives the post-render scroll restore", async ({ page }) => {
    await openEpubReader(page);
    await page.locator("#next-btn").click();
    await expect(page.locator("#reader-content")).toContainText("chapter one");
    const needle = "Paragraph 59";
    const { s, e } = await chapterOffsetsOf(page, needle);
    expect(s).toBeGreaterThan(-1);
    const resp = await page.request.post(`/api/books/${EPUB_ID}/highlights`, {
      data: { chapterIndex: 1, text: needle, color: "#f6c445", startOffset: s, endOffset: e },
    });
    expect(resp.status()).toBe(201);
    await page.reload();
    await openEpubReader(page);

    await page.locator("#hl-btn").click();
    await page.locator(".hl-entry").first().click();
    await expect(page.locator("#reader-content")).toContainText("chapter one");
    const mark = page.locator(`mark.hl-mark`).filter({ hasText: needle }).first();
    await expect(mark).toBeInViewport();
    await expect
      .poll(() => page.evaluate(() => document.getElementById("reader-stage")!.scrollTop))
      .toBeGreaterThan(0);
  });

  test("immediate Prev/Next after a jump cancels the pending jump", async ({ page }) => {
    await openEpubReader(page);
    await seedChapterOneHighlight(page);
    await page.reload();
    await openEpubReader(page);
    await page.locator("#hl-btn").click();
    await page.locator(".hl-entry").first().click();
    // Immediately navigate back — the pending jump must NOT hijack this render.
    await page.locator("#prev-btn").click();
    await expect(page.locator("#reader-content")).toContainText("chapter zero");
    const pending = await page.evaluate(() =>
      "__hlJumpPendingForTest" in window ? window.__hlJumpPendingForTest : "hook-missing");
    expect(pending).toBeNull();
  });

  test("raw hash navigation cancels the jump; returning later doesn't scroll-hijack", async ({ page }) => {
    await openEpubReader(page);
    await seedChapterOneHighlight(page);
    await page.reload();
    await openEpubReader(page);
    await page.locator("#hl-btn").click();
    await page.locator(".hl-entry").first().click();
    // Navigate away via a raw hash edit — NO reader handler fires, so only
    // the router backstop can cancel the pending jump.
    await page.evaluate((id) => { location.hash = `#/book/${id}/0/read`; }, EPUB_ID);
    await expect(page.locator("#reader-content")).toContainText("chapter zero");
    const pending = await page.evaluate(() =>
      "__hlJumpPendingForTest" in window ? window.__hlJumpPendingForTest : "hook-missing");
    expect(pending).toBeNull();
    // Returning to the target chapter later renders normally — the dead
    // token must not scroll-hijack this unrelated render.
    await page.evaluate((id) => { location.hash = `#/book/${id}/1/read`; }, EPUB_ID);
    await expect(page.locator("#reader-content")).toContainText("chapter one");
    await expect(page.locator("mark.hl-mark").first()).toBeVisible();
    expect(
      await page.evaluate(() =>
        "__hlJumpPendingForTest" in window ? window.__hlJumpPendingForTest : "hook-missing"),
    ).toBeNull();
  });
});

test.describe("highlight note editor", () => {
  test("note round-trip via drawer", async ({ page }) => {
    await openEpubReader(page);
    await seedHighlight(page, "quick brown fox");
    await page.reload();
    await openEpubReader(page);
    await page.locator("#hl-btn").click();
    await page.locator(".hl-entry-note-btn").first().click();
    await page.locator(".hl-note-input").fill("remember this");
    await page.locator(".hl-note-save").click();
    await expect
      .poll(async () => (await (await page.request.get(`/api/books/${EPUB_ID}/highlights`)).json())[0].note)
      .toBe("remember this");
    await expect(page.locator(".hl-entry").first()).toContainText("remember this");
  });

  test("mark-tap popover note button opens the drawer row editor", async ({ page }) => {
    await openEpubReader(page);
    await seedHighlight(page, "quick brown fox");
    await page.reload();
    await openEpubReader(page);
    await page.locator("mark.hl-mark").first().click();
    await expect(page.locator("#hl-edit-popover")).toBeVisible();
    await page.locator("#hl-note-btn").click();
    await expect(page.locator("#hl-panel")).toBeVisible();
    await expect(page.locator(".hl-note-input")).toBeVisible();
    await page.locator(".hl-note-input").fill("from the popover");
    await page.locator(".hl-note-save").click();
    await expect
      .poll(async () => (await (await page.request.get(`/api/books/${EPUB_ID}/highlights`)).json())[0].note)
      .toBe("from the popover");
  });

  test("selection popover create-note button creates then opens the editor", async ({ page }) => {
    await openEpubReader(page);
    await page.evaluate(() => {
      const el = document.querySelector("#reader-content")!;
      const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
      let node: Node | null = null;
      let off = -1;
      while ((node = walker.nextNode())) {
        off = node.nodeValue!.indexOf("lazy dog");
        if (off !== -1) break;
      }
      const range = document.createRange();
      range.setStart(node!, off);
      range.setEnd(node!, off + "lazy dog".length);
      const sel = window.getSelection()!;
      sel.removeAllRanges();
      sel.addRange(range);
      document.dispatchEvent(new Event("selectionchange"));
    });
    await expect(page.locator("#hl-popover")).toBeVisible();
    await page.locator("#hl-create-note-btn").click();
    // Highlight created with the default color, drawer opens on its editor.
    await expect(page.locator("#hl-panel")).toBeVisible();
    await expect(page.locator(".hl-note-input")).toBeVisible();
    await page.locator(".hl-note-input").fill("noted at creation");
    await page.locator(".hl-note-save").click();
    await expect
      .poll(async () => {
        const rows = await (await page.request.get(`/api/books/${EPUB_ID}/highlights`)).json();
        return rows.length === 1 ? rows[0].note : null;
      })
      .toBe("noted at creation");
    await expect(page.locator("mark.hl-mark").first()).toHaveText("lazy dog");
  });

  test("clearing the note saves null", async ({ page }) => {
    await openEpubReader(page);
    const hl = await seedHighlight(page, "quick brown fox");
    const put = await page.request.put(`/api/books/${EPUB_ID}/highlights/${hl.id}`, {
      data: { note: "to be cleared" },
    });
    expect(put.ok()).toBe(true);
    await page.reload();
    await openEpubReader(page);
    await page.locator("#hl-btn").click();
    await expect(page.locator(".hl-entry").first()).toContainText("to be cleared");
    await page.locator(".hl-entry-note-btn").first().click();
    await page.locator(".hl-note-input").fill("");
    await page.locator(".hl-note-save").click();
    await expect
      .poll(async () => (await (await page.request.get(`/api/books/${EPUB_ID}/highlights`)).json())[0].note)
      .toBeNull();
  });
});

