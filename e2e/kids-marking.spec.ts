import { test, expect, type Page } from "@playwright/test";

// Parent-side Kids marking in the web UI, against the Kids harness (port 7811,
// see e2e/kids-mode.spec.ts): web PIN 2468, books 001–003 in "Kids Series",
// books 004–006 in "Adult Series". Marks are server-global, so afterEach
// resets them through the API.
test.use({ baseURL: "http://127.0.0.1:7811" });

const PIN = "2468";
const CODE = "1357";
const KIDS_BOOK_ID = "e2e-book-001";

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

async function turnOnKidsMode(page: Page) {
  await page.getByRole("button", { name: "Kids mode", exact: true }).click();
  await page.getByRole("button", { name: "Turn on Kids mode" }).click();
  await expect(page.locator(".header .kids-badge")).toBeVisible();
}

test.describe("web UI Kids marking", () => {
  test.beforeEach(async ({ page }) => {
    await page.goto("/");
    await logIn(page);
    expect(await apiStatus(page, "PUT", "/api/kids/code", { code: CODE })).toBe(200);
  });

  test.afterEach(async ({ page }) => {
    await apiStatus(page, "POST", "/api/kids/exit", { code: CODE });
    await apiStatus(page, "PUT", "/api/kids/series", { series: "Kids Series", allowed: false });
    await apiStatus(page, "PUT", `/api/kids/books/${KIDS_BOOK_ID}`, { override: "follow" });
  });

  test("marking a series from its chip shows its books in Kids mode", async ({ page }) => {
    await page.evaluate(() => { location.hash = "#/library?series=Kids%20Series"; });
    await page.getByRole("button", { name: "Mark series OK for kids" }).click();
    await expect(page.getByRole("button", { name: "Unmark series for kids" })).toBeVisible();

    await turnOnKidsMode(page);
    await expect(page.locator(".grid .card .title")).toHaveText(["Book 003", "Book 002", "Book 001"]);
    // Kids mode offers no marking controls.
    await expect(page.getByRole("button", { name: /series.*for kids/ })).toHaveCount(0);
  });

  test("Not for kids hides one book of a marked series; Follow series restores it", async ({ page }) => {
    expect(
      await apiStatus(page, "PUT", "/api/kids/series", { series: "Kids Series", allowed: true }),
    ).toBe(200);
    await page.goto(`/#/book/${KIDS_BOOK_ID}`);
    await expect(page.getByText("Visible in Kids mode")).toBeVisible();

    await page.getByRole("button", { name: "Not for kids" }).click();
    await expect(page.getByText("Hidden in Kids mode")).toBeVisible();

    await page.goto("/#/library");
    await turnOnKidsMode(page);
    await expect(page.locator(".grid .card .title")).toHaveText(["Book 003", "Book 002"]);
    await page.getByRole("button", { name: "Leave Kids mode" }).click();
    await page.getByLabel("Parents' code").fill(CODE);
    await page.getByRole("button", { name: "Leave", exact: true }).click();
    await expect(page.locator(".kids-badge")).toHaveCount(0);

    await page.goto(`/#/book/${KIDS_BOOK_ID}`);
    await page.getByRole("button", { name: "Follow series" }).click();
    await expect(page.getByText("Visible in Kids mode")).toBeVisible();
    await page.goto("/#/library");
    await turnOnKidsMode(page);
    await expect(page.locator(".grid .card .title")).toHaveText(["Book 003", "Book 002", "Book 001"]);
  });
});
