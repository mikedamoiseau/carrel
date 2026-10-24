import { test, expect, type Page } from "@playwright/test";

// Magazine titles are long ("Tout Comprendre Junior - Hors-série N°24 -
// Novembre 2026") and the distinguishing part is at the end. On a phone:
//   - library cards cut the title to one ellipsized line, so issues of the
//     same magazine were indistinguishable;
//   - the detail header gave the title whatever width the back button, the
//     profile switcher and the nav icons left over — a few pixels — and
//     `overflow-wrap: anywhere` then broke it one letter per line.
//
// Titles are lengthened by patching the real API responses, so the SW must not
// own these fetches (an SW-controlled page bypasses page.route()).
test.use({ serviceWorkers: "block", viewport: { width: 390, height: 844 } });

const longTitle = (n: number) => `Tout Comprendre Junior - Hors-série N°${n} - Novembre 2026`;

async function lengthenTitles(page: Page) {
  // Library grid, shelves (want-to-read / recently added) and the
  // continue-reading shelf all return a flat array of books.
  await page.route(/\/api\/books(\/continue-reading)?\?/, async (route) => {
    const resp = await route.fetch();
    const books = await resp.json();
    books.forEach((b: { title: string }, i: number) => (b.title = longTitle(i + 1)));
    await route.fulfill({ response: resp, json: books });
  });
  await page.route(/\/api\/books\/e2e-book-(001|130)$/, async (route) => {
    const resp = await route.fetch();
    const book = await resp.json();
    book.title = longTitle(24);
    await route.fulfill({ response: resp, json: book });
  });
}

// Opens a view and returns its header title. Earlier specs leave reading
// progress on the harness's shared DB, so the reader may open on the resume
// prompt first; dismiss it the way the other reader specs do.
async function openHeaderTitle(page: Page, hash: string) {
  await page.goto("/" + hash);
  const title = page.locator(".header .nav-book-title");
  const restart = page.locator("#resume-restart-btn");
  await expect(restart.or(title)).toBeVisible({ timeout: 15_000 });
  if (await restart.isVisible()) await restart.click();
  await expect(title).toHaveText(longTitle(24));
  return title;
}

test.describe("Long titles at phone width", () => {
  test("library cards show the whole title", async ({ page }) => {
    await lengthenTitles(page);
    await page.goto("/");
    const title = page.locator(".grid .card .title").first();
    await expect(title).toHaveText(longTitle(1));

    // Nothing clipped: the box holds all of its text in both directions.
    const clipped = await title.evaluate(
      (el) => el.scrollWidth > el.clientWidth || el.scrollHeight > el.clientHeight,
    );
    expect(clipped).toBe(false);
  });

  test("library shelves show the whole title", async ({ page }) => {
    await lengthenTitles(page);
    await page.goto("/");
    const title = page.locator(".shelf-card .shelf-title").first();
    await expect(title).toContainText("Tout Comprendre Junior - Hors-série N°");

    const clipped = await title.evaluate(
      (el) => el.scrollWidth > el.clientWidth || el.scrollHeight > el.clientHeight,
    );
    expect(clipped).toBe(false);
  });

  // 390 wraps the icon cluster below the title; at 430 (the widest iPhones)
  // back button + nav icons fit on one row with a zero-width title, which is
  // exactly the case that left the title a sliver.
  // The reader renders the same header; e2e-book-130 is the one book whose
  // file exists, so its reader opens instead of erroring.
  const pages = [
    { name: "detail", hash: "#/book/e2e-book-001" },
    { name: "reader", hash: "#/book/e2e-book-130/0/read" },
  ];
  for (const { name, hash } of pages) for (const width of [390, 430]) {
    test(`${name} header title is not squeezed into a sliver at ${width}px`, async ({ page }) => {
      await page.setViewportSize({ width, height: 844 });
      await lengthenTitles(page);
      const title = await openHeaderTitle(page, hash);

      // The title gets a usable share of the header, so it wraps by word,
      // not by letter: no more than three lines.
      const header = await page.locator("#app .header").boundingBox();
      const box = await title.boundingBox();
      expect(box!.width).toBeGreaterThan(header!.width / 2);
      const lineHeight = await title.evaluate((el) => parseFloat(getComputedStyle(el).lineHeight));
      expect(box!.height).toBeLessThanOrEqual(lineHeight * 3 + 1);
    });
  }

  // Phone landscape: there is room for the title beside the icons, so the
  // header must stay one row — a second row would come out of the reader's
  // fixed-height stage.
  test("phone landscape keeps a one-row header when the title fits", async ({ page }) => {
    await page.setViewportSize({ width: 568, height: 320 });
    await lengthenTitles(page);
    const title = await openHeaderTitle(page, "#/book/e2e-book-130/0/read");

    const box = await title.boundingBox();
    const icons = await page.locator(".header .nav-icons").boundingBox();
    expect(icons!.y).toBeLessThan(box!.y + box!.height);
    const fontSize = await title.evaluate((el) => parseFloat(getComputedStyle(el).fontSize));
    expect(box!.width).toBeGreaterThanOrEqual(fontSize * 8 - 1);
  });
});
