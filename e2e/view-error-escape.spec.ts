import { test, expect } from "@playwright/test";

// A whole-view load failure (detail or reader) must leave the user a way out.
// The web UI is installed as a home-screen app on iPad, where there is no
// browser back button: before this fix the error view was a bare line of text
// with no header and no button, and on a notched/status-bar device the
// headerless view's fixed accent strip (body:not(:has(.header))::before) sat
// on top of that line — the screen looked empty and the only escape was
// killing the app. The typical trigger is a linked book whose file lives on a
// network drive that is not mounted on the host.
//
// Failures are modelled with route interception, so the SW must not own these
// fetches (an SW-controlled page bypasses page.route()).
test.use({ serviceWorkers: "block" });

const COMIC_ID = "e2e-book-130";
const EPUB_ID = "e2e-book-001";

test.describe("Whole-view load errors can be escaped", () => {
  test("reader: a comic whose page count fails offers a way back to the book", async ({ page }) => {
    await page.route(`**/api/books/${COMIC_ID}/page-count`, (route) =>
      route.fulfill({ status: 404, contentType: "text/plain", body: "Page count not available for this book" }),
    );

    await page.goto(`/#/book/${COMIC_ID}/0/read`);
    const message = page.getByText("Couldn't load page count (HTTP 404)");
    await expect(message).toBeVisible();

    // The error renders under a real .header, which carries the status-bar
    // inset padding — the message is laid out below it, not underneath it.
    const header = page.locator("#app .header");
    await expect(header).toBeVisible();
    const headerBox = await header.boundingBox();
    const messageBox = await message.boundingBox();
    expect(headerBox && messageBox && messageBox.y >= headerBox.y + headerBox.height).toBe(true);

    await page.locator("#back-btn").click();
    await expect(page).toHaveURL(new RegExp(`#/book/${COMIC_ID}$`));
    await expect(page.locator(".nav-book-title")).toHaveText("Book 130");
  });

  test("detail: a book that fails to load offers a way back to the library", async ({ page }) => {
    await page.route(new RegExp(`/api/books/${EPUB_ID}$`), (route) =>
      route.fulfill({ status: 500, contentType: "text/plain", body: "boom" }),
    );

    await page.goto(`/#/book/${EPUB_ID}`);
    await expect(page.getByText("Couldn't load this book (HTTP 500)")).toBeVisible();
    await expect(page.locator("#app .header")).toBeVisible();

    await page.locator("#back-btn").click();
    await expect(page).not.toHaveURL(/#\/book\//);
    await expect(page.locator(".grid .card").first()).toBeVisible();
  });
});
