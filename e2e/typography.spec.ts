import { test, expect, type Page } from "@playwright/test";
import { enterReaderAtStart } from "./detail-actions";

// Web reader typography controls (M2/M3). The settings model, live apply, and
// reflow-preservation are exercised against the real client (app.js) on the
// deterministic harness. Book 050 (`e2e-book-050`) is the only on-disk EPUB —
// 2 chapters, ch1 lengthened to ~60 paragraphs so `#reader-stage` scrolls.
const EPUB_ID = "e2e-book-050";

// app.js is an IIFE; the typography surface is exposed on `window.__carrelTypo`
// for these tests: { validate, get, set, change }.
type TypoHook = {
  validate: (raw: unknown) => Record<string, unknown>;
  get: () => Record<string, unknown>;
  set: (patch: Record<string, unknown>) => Record<string, unknown>;
  change: (patch: Record<string, unknown>) => void;
};

async function gotoLibrary(page: Page) {
  await page.goto("/");
  await page.locator(".grid .card").first().waitFor({ timeout: 15_000 });
}

// Open Book 050's chapter reader at chapter 0, dismissing a resume prompt so we
// deterministically land on chapter 0 with `#reader-content` populated.
async function openEpubChapter(page: Page) {
  await page.goto(`/#/book/${EPUB_ID}/0/read`);
  const restart = page.locator("#resume-restart-btn");
  const content = page.locator("#reader-content");
  await expect(restart.or(content)).toBeVisible({ timeout: 15_000 });
  if (await restart.isVisible()) {
    await restart.click();
    await content.waitFor();
  }
  await expect(content).toContainText("chapter zero", { timeout: 10_000 });
}

// Inline style of #reader-content (typography is applied as inline styles, not
// a stylesheet rule — read el.style directly rather than getComputedStyle).
async function readerContentStyle(page: Page) {
  return page.evaluate(() => {
    const el = document.querySelector("#reader-content") as HTMLElement;
    return {
      fontSize: el.style.fontSize,
      lineHeight: el.style.lineHeight,
      maxWidth: el.style.maxWidth,
      fontFamily: el.style.fontFamily,
    };
  });
}

// The typography test hook (window.__carrelTypo) is gated in app.js behind this
// flag, set before load, so the internal API never ships to the production
// reader. Registered before every test's navigation.
test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    (window as unknown as { __carrelExposeTypoHook: boolean }).__carrelExposeTypoHook = true;
  });
});

test.describe("Web reader typography — settings model", () => {
  test("validateTypography clamps then snaps", async ({ page }) => {
    await gotoLibrary(page);
    const out = await page.evaluate(() =>
      (window as unknown as { __carrelTypo: TypoHook }).__carrelTypo.validate({
        fontSize: "x",
        lineHeight: 2.5,
        fontFamily: "bad",
        columnWidth: 5,
      })
    );
    expect(out).toEqual({
      fontSize: 18,
      lineHeight: 2.4,
      fontFamily: "lora",
      columnWidth: 700,
    });
  });

  test("the __carrelTypo hook is NOT exposed without the opt-in flag (production)", async ({
    browser,
  }) => {
    // Fresh context that never ran the beforeEach opt-in (it applied to the
    // default `page`, unused here) — proves the gate keeps the hook out of the
    // shipped reader.
    const ctx = await browser.newContext();
    const p = await ctx.newPage();
    await p.goto("/");
    await p.locator(".grid .card").first().waitFor({ timeout: 15_000 });
    const hook = await p.evaluate(() => (window as unknown as { __carrelTypo?: unknown }).__carrelTypo);
    expect(hook).toBeUndefined();
    await ctx.close();
  });

  test("applies default typography to #reader-content", async ({ page }) => {
    await openEpubChapter(page);
    const cs = await readerContentStyle(page);
    expect(cs.fontSize).toBe("18px");
    expect(cs.lineHeight).toBe("1.8");
    expect(cs.maxWidth).toBe("700px");
    expect(cs.fontFamily).toContain("Lora Variable");
  });

  test("malformed stored typography falls back to defaults", async ({ page }) => {
    await page.addInitScript(() => localStorage.setItem("carrel-web-typography", "{not json"));
    await openEpubChapter(page);
    const cs = await readerContentStyle(page);
    expect(cs.fontSize).toBe("18px");
  });

  test("failed persist still applies for the session", async ({ page }) => {
    await page.addInitScript(() => {
      const orig = Storage.prototype.setItem;
      Storage.prototype.setItem = function (k: string, v: string) {
        if (k === "carrel-web-typography") throw new Error("quota");
        return orig.call(this, k, v);
      };
    });
    await openEpubChapter(page);
    await page.evaluate(() =>
      (window as unknown as { __carrelTypo: TypoHook }).__carrelTypo.set({ fontSize: 22 })
    );
    const fs = await page.evaluate(
      () => (window as unknown as { __carrelTypo: TypoHook }).__carrelTypo.get().fontSize
    );
    expect(fs).toBe(22); // in-memory survived the denied persist
  });
});

// Navigate to the long chapter 1 (ch1 was lengthened to ~60 paragraphs so the
// stage scrolls) and scroll to the middle.
async function openLongChapterMidScroll(page: Page) {
  await openEpubChapter(page);
  await page.locator("#reader-stage").focus();
  await page.keyboard.press("ArrowRight");
  await expect(page.locator("#reader-content")).toContainText("chapter one", { timeout: 10_000 });
  await page.locator("#reader-stage").evaluate((s) => {
    s.scrollTop = Math.round((s.scrollHeight - s.clientHeight) / 2);
  });
  await page.waitForTimeout(50);
}

// The text + top-edge offset of the paragraph currently at the stage's top.
async function topAnchor(page: Page) {
  return page.evaluate(() => {
    const stg = document.querySelector("#reader-stage") as HTMLElement;
    const content = document.querySelector("#reader-content") as HTMLElement;
    const stageTop = stg.getBoundingClientRect().top;
    for (const kid of Array.from(content.children)) {
      const r = kid.getBoundingClientRect();
      if (r.bottom > stageTop) return { text: kid.textContent, offset: r.top - stageTop };
    }
    return null;
  });
}

async function offsetOf(page: Page, text: string) {
  return page.evaluate((t) => {
    const stg = document.querySelector("#reader-stage") as HTMLElement;
    const content = document.querySelector("#reader-content") as HTMLElement;
    const stageTop = stg.getBoundingClientRect().top;
    const el = Array.from(content.children).find((k) => k.textContent === t);
    return el ? el.getBoundingClientRect().top - stageTop : null;
  }, text);
}

test.describe("Web reader typography — reflow preservation", () => {
  test("keeps the top paragraph in place when font size changes", async ({ page }) => {
    await openLongChapterMidScroll(page);
    const before = await topAnchor(page);
    expect(before).not.toBeNull();

    await page.evaluate(() =>
      (window as unknown as { __carrelTypo: TypoHook }).__carrelTypo.change({ fontSize: 24 })
    );
    await page.waitForTimeout(100);

    const after = await offsetOf(page, before!.text as string);
    expect(after).not.toBeNull();
    expect(Math.abs((after as number) - before!.offset)).toBeLessThan(12);
  });

  test("rapid changes settle with the top paragraph stable", async ({ page }) => {
    await openLongChapterMidScroll(page);
    const before = await topAnchor(page);
    expect(before).not.toBeNull();

    await page.evaluate(() => {
      const t = (window as unknown as { __carrelTypo: TypoHook }).__carrelTypo;
      t.change({ fontSize: 20 });
      t.change({ fontSize: 22 });
      t.change({ fontSize: 24 });
    });
    await page.waitForTimeout(150);

    const after = await offsetOf(page, before!.text as string);
    expect(after).not.toBeNull();
    expect(Math.abs((after as number) - before!.offset)).toBeLessThan(14);
  });

  test("a typography change does not jump reading position to the top", async ({ page }) => {
    await openLongChapterMidScroll(page);
    const stageRatio = () =>
      page.locator("#reader-stage").evaluate((s) => {
        const max = s.scrollHeight - s.clientHeight;
        return max > 0 ? s.scrollTop / max : 0;
      });
    const before = await stageRatio();
    await page.evaluate(() =>
      (window as unknown as { __carrelTypo: TypoHook }).__carrelTypo.change({ fontSize: 22 })
    );
    await page.waitForTimeout(100);
    const after = await stageRatio();
    // The reflow re-anchors rather than resetting to the top or NaN: the scroll
    // ratio stays a finite fraction close to where we were.
    expect(Number.isFinite(after)).toBe(true);
    expect(after).toBeGreaterThan(0.1);
    expect(Math.abs(after - before)).toBeLessThan(0.2);
  });
});

const CBZ_ID = "e2e-book-130"; // page-image reader — no typography controls

test.describe("Web reader typography — Aa control", () => {
  test("Aa button present in chapter reader, absent in a page reader", async ({ page }) => {
    await openEpubChapter(page);
    await expect(page.locator("#typo-btn")).toBeVisible();

    // Page-image (CBZ) reader must NOT offer typography.
    await page.goto(`/#/book/${CBZ_ID}`);
    await enterReaderAtStart(page); // idempotent across shared-harness re-runs
    await page.locator("#page-img").waitFor({ timeout: 15_000 });
    await expect(page.locator("#typo-btn")).toHaveCount(0);
  });

  test("Aa toggles the panel and aria-expanded", async ({ page }) => {
    await openEpubChapter(page);
    const btn = page.locator("#typo-btn");
    const panel = page.locator("#typo-panel");
    await expect(btn).toHaveAttribute("aria-expanded", "false");
    await expect(panel).toBeHidden();

    await btn.click();
    await expect(btn).toHaveAttribute("aria-expanded", "true");
    await expect(panel).toBeVisible();

    await btn.click();
    await expect(btn).toHaveAttribute("aria-expanded", "false");
    await expect(panel).toBeHidden();
  });

  test("each control changes #reader-content live and persists across reload", async ({ page }) => {
    await openEpubChapter(page);
    await page.locator("#typo-btn").click();

    // Font size +2 (18 -> 20)
    await page.locator("#typo-fontsize-inc").click();
    expect((await readerContentStyle(page)).fontSize).toBe("20px");

    // Line spacing +0.2 (1.8 -> 2)
    await page.locator("#typo-linespacing-inc").click();
    expect((await readerContentStyle(page)).lineHeight).toBe("2");

    // Font family -> Literata
    await page.locator('#typo-family [data-family="literata"]').click();
    expect((await readerContentStyle(page)).fontFamily).toContain("Literata Variable");

    // Column width -> Wide (860)
    await page.locator('#typo-width [data-width="860"]').click();
    expect((await readerContentStyle(page)).maxWidth).toBe("860px");

    // Persisted across reload.
    await page.reload();
    await openEpubChapter(page);
    const cs = await readerContentStyle(page);
    expect(cs.fontSize).toBe("20px");
    expect(cs.lineHeight).toBe("2");
    expect(cs.fontFamily).toContain("Literata Variable");
    expect(cs.maxWidth).toBe("860px");
  });

  test("steppers disable at the maximum", async ({ page }) => {
    await page.addInitScript(() =>
      localStorage.setItem(
        "carrel-web-typography",
        JSON.stringify({ fontSize: 24, lineHeight: 2.4, fontFamily: "lora", columnWidth: 700 })
      )
    );
    await openEpubChapter(page);
    await page.locator("#typo-btn").click();
    await expect(page.locator("#typo-fontsize-inc")).toBeDisabled();
    await expect(page.locator("#typo-linespacing-inc")).toBeDisabled();
    await expect(page.locator("#typo-fontsize-dec")).toBeEnabled();
    await expect(page.locator("#typo-linespacing-dec")).toBeEnabled();
  });

  test("steppers disable at the minimum", async ({ page }) => {
    await page.addInitScript(() =>
      localStorage.setItem(
        "carrel-web-typography",
        JSON.stringify({ fontSize: 14, lineHeight: 1.2, fontFamily: "lora", columnWidth: 700 })
      )
    );
    await openEpubChapter(page);
    await page.locator("#typo-btn").click();
    await expect(page.locator("#typo-fontsize-dec")).toBeDisabled();
    await expect(page.locator("#typo-linespacing-dec")).toBeDisabled();
    await expect(page.locator("#typo-fontsize-inc")).toBeEnabled();
    await expect(page.locator("#typo-linespacing-inc")).toBeEnabled();
  });

  test("font radiogroup: arrow keys move selection, Space activates", async ({ page }) => {
    await openEpubChapter(page);
    await page.locator("#typo-btn").click();
    const group = page.locator("#typo-family");
    await expect(group).toHaveAttribute("role", "radiogroup");

    // Focus the selected radio (lora), arrow down to the next family, activate.
    await page.locator('#typo-family [data-family="lora"]').focus();
    await page.keyboard.press("ArrowDown");
    await page.keyboard.press(" ");
    // Whatever is now checked must be reflected on #reader-content and the URL
    // must NOT have changed (arrow keys were contained, not chapter nav).
    await expect(page).toHaveURL(new RegExp(`#/book/${EPUB_ID}/0/read`));
    const checked = await page.locator('#typo-family [aria-checked="true"]').getAttribute("data-family");
    expect(["literata", "dm-sans", "opendyslexic"]).toContain(checked);
  });

  test("Esc closes the panel, keeps the reader open, returns focus to Aa", async ({ page }) => {
    await openEpubChapter(page);
    const btn = page.locator("#typo-btn");
    await btn.click();
    await expect(page.locator("#typo-panel")).toBeVisible();

    await page.keyboard.press("Escape");
    await expect(page.locator("#typo-panel")).toBeHidden();
    // Reader still open (Esc did NOT navigate back to detail).
    await expect(page).toHaveURL(new RegExp(`#/book/${EPUB_ID}/0/read`));
    await expect(btn).toBeFocused();
  });

  test("each font radio previews in its own typeface", async ({ page }) => {
    await openEpubChapter(page);
    await page.locator("#typo-btn").click();
    // The style attribute must survive parsing (single-quoted so the stack's
    // inner double-quotes don't truncate it) and resolve to the face.
    for (const [fam, face] of [
      ["lora", "Lora Variable"],
      ["literata", "Literata Variable"],
      ["dm-sans", "DM Sans Variable"],
      ["opendyslexic", "OpenDyslexic"],
    ] as const) {
      const ff = await page
        .locator(`#typo-family [data-family="${fam}"]`)
        .evaluate((el) => getComputedStyle(el as HTMLElement).fontFamily);
      expect(ff).toContain(face);
    }
  });

  test("arrow keys on a stepper do not turn the chapter", async ({ page }) => {
    await openEpubChapter(page);
    await page.locator("#typo-btn").click();
    await page.locator("#typo-fontsize-inc").focus();
    await page.keyboard.press("ArrowRight");
    await page.keyboard.press("ArrowLeft");
    await page.waitForTimeout(100);
    // Contained — still on chapter 0, panel still open.
    await expect(page).toHaveURL(new RegExp(`#/book/${EPUB_ID}/0/read`));
    await expect(page.locator("#typo-panel")).toBeVisible();
  });

  test("keyboard focus stays in the panel when a stepper disables at its bound", async ({ page }) => {
    await page.addInitScript(() =>
      localStorage.setItem(
        "carrel-web-typography",
        JSON.stringify({ fontSize: 22, lineHeight: 1.8, fontFamily: "lora", columnWidth: 700 })
      )
    );
    await openEpubChapter(page);
    await page.locator("#typo-btn").click();
    await page.locator("#typo-fontsize-inc").focus();
    // 22 -> 24: inc becomes disabled; focus must move to the sibling (dec), not <body>.
    await page.keyboard.press("Enter");
    await expect(page.locator("#typo-fontsize-inc")).toBeDisabled();
    const focusInPanel = await page.evaluate(
      () => !!document.activeElement?.closest("#typo-panel")
    );
    expect(focusInPanel).toBe(true);
  });

  test("all popover controls meet the 44px minimum tap target", async ({ page }) => {
    await openEpubChapter(page);
    await page.locator("#typo-btn").click();
    const controls = page.locator("#typo-panel button, #typo-panel [role=radio]");
    const n = await controls.count();
    expect(n).toBeGreaterThan(0);
    for (let i = 0; i < n; i++) {
      const box = await controls.nth(i).boundingBox();
      expect(box, `control ${i} has a box`).not.toBeNull();
      expect(box!.height).toBeGreaterThanOrEqual(43.5);
      expect(box!.width).toBeGreaterThanOrEqual(43.5);
    }
  });
});

test.describe("Web reader typography — font-load race & layout", () => {
  test("a user scroll during a pending font-ready re-anchor wins", async ({ page, context }) => {
    // The e2e host is loopback = a secure context, so sw.js precaches the fonts
    // and a Cache-Storage hit can bypass a page.route network delay. Disable the
    // SW and clear caches so the font fetch really goes through the delayed
    // route, keeping document.fonts.ready pending long enough to interleave a
    // user scroll. Register the route BEFORE navigation.
    await context.addInitScript(() => {
      navigator.serviceWorker?.getRegistrations?.().then((rs) => rs.forEach((r) => r.unregister()));
      caches?.keys?.().then((ks) => ks.forEach((k) => caches.delete(k)));
    });
    await page.route("**/fonts/*.woff2", async (route) => {
      await new Promise((r) => setTimeout(r, 600));
      await route.continue();
    });

    await openEpubChapter(page);
    await page.locator("#reader-stage").focus();
    await page.keyboard.press("ArrowRight");
    await expect(page.locator("#reader-content")).toContainText("chapter one", { timeout: 10_000 });

    // Trigger a typography change AT THE TOP (schedules a fonts.ready re-anchor
    // whose captured anchor is the first paragraph), then — while the font is
    // still loading — the user scrolls down to a mid paragraph.
    await page.evaluate(() =>
      (window as unknown as { __carrelTypo: TypoHook }).__carrelTypo.change({ fontSize: 22 })
    );
    await page.locator("#reader-stage").evaluate((s) => {
      s.scrollTop = Math.round((s.scrollHeight - s.clientHeight) / 2);
    });
    // The paragraph the user parked at the top of the viewport.
    const userAnchor = await topAnchor(page);
    expect(userAnchor).not.toBeNull();

    // Wait past the 600ms font delay so the deferred re-anchor fires if not
    // cancelled. Assert SEMANTICALLY (paragraph identity + offset), not by
    // absolute scrollTop: the late font swap reflows the chapter and the
    // browser's own scroll-anchoring shifts absolute offsets, so an exact
    // scrollTop check is environment-brittle. What must hold is that the view
    // stayed with the USER's paragraph and did NOT snap back to the top
    // (the change-time anchor).
    await page.waitForTimeout(900);
    const after = await offsetOf(page, userAnchor!.text as string);
    expect(after).not.toBeNull();
    // Still near the top edge where the user left it (tolerant of reflow), not
    // pushed a full viewport down by a re-anchor to the first paragraph.
    expect(Math.abs((after as number) - userAnchor!.offset)).toBeLessThan(150);
  });

  test("no horizontal overflow at a narrow viewport with the panel open", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await openEpubChapter(page);
    await page.locator("#typo-btn").click();
    await expect(page.locator("#typo-panel")).toBeVisible();
    const overflow = await page.evaluate(
      () => document.scrollingElement!.scrollWidth - window.innerWidth
    );
    expect(overflow).toBeLessThanOrEqual(1); // ≤1px rounding tolerance
    // The panel itself stays within the viewport.
    const box = await page.locator("#typo-panel").boundingBox();
    expect(box!.x).toBeGreaterThanOrEqual(-1);
    expect(box!.x + box!.width).toBeLessThanOrEqual(391);
  });
});
