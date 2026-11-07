import { test, expect, type Page } from "@playwright/test";

// Kids mode lock in the web UI, against the second harness (port 7811,
// `CARREL_E2E_KIDS=1` — see playwright.config.ts and
// src-tauri/examples/web_e2e_server.rs): web PIN 2468, books 001–003 in series
// "Kids Series", books 004–006 in "Adult Series", every other book in none.
//
// The parents' code and the kids marks are server-global, so each test sets
// them through the API and afterEach undoes them, the way
// e2e/profile-switch.spec.ts resets the active profile.
test.use({ baseURL: "http://127.0.0.1:7811" });

const PIN = "2468";
const CODE = "1357";
// Default sort is newest first, so the three Kids Series books list 003..001.
const KIDS_TITLES = ["Book 003", "Book 002", "Book 001"];
const ADULT_TITLES = ["Book 006", "Book 005", "Book 004"];
const ADULT_ID = "e2e-book-004";
// The EPUB with a real file on disk (in no series), for the reader chrome.
const READER_BOOK_ID = "e2e-book-050";

// Calls the server from inside the page, so it carries the page's session
// cookie, and returns the HTTP status.
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

const badge = (page: Page) => page.locator(".header .kids-badge");

// What a device that remembers Kids mode shows when it can't be put back in.
const lockScreen = (page: Page) =>
  page.locator(".resume-prompt", { hasText: "Kids mode is on for this device." });

async function rememberKidsDevice(page: Page) {
  await page.evaluate(() => localStorage.setItem("carrel-kids-device", "1"));
}

async function expectNoBookTitles(page: Page) {
  await expect(page.locator(".grid .card")).toHaveCount(0);
  for (const title of [...KIDS_TITLES, ...ADULT_TITLES]) {
    await expect(page.locator("body")).not.toContainText(title);
  }
}

async function turnOnKidsMode(page: Page) {
  await page.getByRole("button", { name: "Kids mode", exact: true }).click();
  await page.getByRole("button", { name: "Turn on Kids mode" }).click();
  await expect(badge(page)).toBeVisible();
}

async function leaveWith(page: Page, code: string) {
  await page.getByRole("button", { name: "Leave Kids mode" }).click();
  await page.getByLabel("Parents' code").fill(code);
  await page.getByRole("button", { name: "Leave", exact: true }).click();
}

// As in e2e/vocabulary.spec.ts: the saved-words button's visibility waits on
// the dictionary status, so wait for that before asserting on it.
async function openEpubReader(page: Page) {
  await page.goto(`/#/book/${READER_BOOK_ID}/0/read`);
  const restart = page.locator("#resume-restart-btn");
  const content = page.locator("#reader-content");
  await expect(restart.or(content)).toBeVisible({ timeout: 15_000 });
  if (await restart.isVisible()) await restart.click();
  await expect(content).toContainText("chapter zero", { timeout: 10_000 });
  await expect
    .poll(() =>
      page.evaluate(
        () => (window as unknown as { __dictStatusForTest?: unknown }).__dictStatusForTest ?? null,
      ),
    )
    .not.toBeNull();
}

async function expectKidsGrid(page: Page) {
  await expect(page.locator(".grid .card .title")).toHaveText(KIDS_TITLES);
  for (const title of ADULT_TITLES) {
    await expect(page.locator("body")).not.toContainText(title);
  }
}

test.describe("web UI Kids mode", () => {
  test.beforeEach(async ({ page }) => {
    await page.goto("/");
    await logIn(page);
    expect(await apiStatus(page, "PUT", "/api/kids/code", { code: CODE })).toBe(200);
    expect(
      await apiStatus(page, "PUT", "/api/kids/series", { series: "Kids Series", allowed: true }),
    ).toBe(200);
  });

  test.afterEach(async ({ page }) => {
    // Leave first: a Kids session is refused the marking routes below. A
    // session that isn't in Kids mode gets a no-op 200. (A page a failed test
    // left logged out only gets 401s here; the next beforeEach sets the same
    // fixtures again.)
    await apiStatus(page, "POST", "/api/kids/exit", { code: CODE });
    await apiStatus(page, "PUT", "/api/kids/series", { series: "Kids Series", allowed: false });
    await apiStatus(page, "PUT", `/api/kids/books/${READER_BOOK_ID}`, { override: "follow" });
  });

  test("Kids mode shows only marked books and hides household icons", async ({ page }) => {
    // The parent's header carries the household entries…
    await expect(page.locator("#profile-switcher-btn")).toBeVisible();
    await expect(page.locator('.header [data-nav="stats"]')).toBeVisible();

    await turnOnKidsMode(page);

    await expectKidsGrid(page);
    // …and Kids mode omits them from the header and the bottom tab bar alike.
    for (const sel of [
      "#profile-switcher-btn",
      '[data-nav="stats"]',
      '[data-nav="vocabulary"]',
      '[data-tab="stats"]',
      '[data-tab="vocabulary"]',
    ]) {
      await expect(page.locator(sel)).toHaveCount(0);
    }
    await expect(badge(page)).toHaveText(/Kids mode/);
    await expect(page.locator(".tab-bar .kids-badge")).toHaveCount(1);
    // The server agrees this session is in Kids mode.
    const kids = await page.evaluate(async () => (await (await fetch("/api/kids/status")).json()).kids);
    expect(kids).toBe(true);
  });

  test("stats and vocabulary URLs land on the library in Kids mode", async ({ page }) => {
    await turnOnKidsMode(page);
    for (const hash of ["#/stats", "#/vocabulary"]) {
      await page.evaluate((h) => { location.hash = h; }, hash);
      await expect(page).toHaveURL(/#\/library$/);
      await expectKidsGrid(page);
      await expect(page.locator(".stats, .vocab-screen")).toHaveCount(0);
    }
  });

  test("deep link to a hidden book shows an error, not the book", async ({ page }) => {
    // Control: the parent can open it.
    await page.goto(`/#/book/${ADULT_ID}`);
    await expect(page.locator(".header .nav-book-title")).toHaveText("Book 004");

    await page.goto("/");
    await turnOnKidsMode(page);
    await page.goto(`/#/book/${ADULT_ID}`);
    await expect(page.locator(".view-error")).toBeVisible();
    await expect(page.locator("body")).not.toContainText("Book 004");
    // The same URL as a fresh page load, not a hash change.
    await page.reload();
    await expect(page.locator(".view-error")).toBeVisible();
    await expect(page.locator("body")).not.toContainText("Book 004");
  });

  test("leaving needs the code", async ({ page }) => {
    await turnOnKidsMode(page);

    await leaveWith(page, "0000");
    await expect(page.getByText("Incorrect code")).toBeVisible();
    await expect(badge(page)).toBeVisible();

    await page.getByLabel("Parents' code").fill(CODE);
    await page.getByRole("button", { name: "Leave", exact: true }).click();
    await expect(page.locator(".kids-badge")).toHaveCount(0);
    await expect(page.locator("#profile-switcher-btn")).toBeVisible();

    await page.evaluate(() => { location.hash = "#/library?series=Adult%20Series"; });
    await expect(page.locator(".grid .card .title")).toHaveText(ADULT_TITLES);

    // The device forgot Kids mode too: a reload doesn't put it back.
    await page.reload();
    await expect(page.locator(".grid .card .title")).toHaveText(ADULT_TITLES);
    await expect(page.locator(".kids-badge")).toHaveCount(0);
  });

  test("device re-enters Kids mode after a parent logs in again", async ({ page, context }) => {
    await turnOnKidsMode(page);

    // The session is gone (expired, or the app restarted); the device's
    // remembered flag is not.
    await context.clearCookies();
    await page.reload();
    await logIn(page);

    // No click on "Turn on Kids mode": the login alone put it back.
    await expect(badge(page)).toBeVisible();
    await expectKidsGrid(page);
    const kids = await page.evaluate(async () => (await (await fetch("/api/kids/status")).json()).kids);
    expect(kids).toBe(true);
  });

  test("a failed re-entry after a re-login locks the device until a parent's code", async ({
    page,
    context,
  }) => {
    await turnOnKidsMode(page);
    // The session ends under a page that is still drawing Kids mode, so its
    // next request (no reload) gets a 401 and the PIN screen.
    await context.clearCookies();
    await page.evaluate(() => { location.hash = "#/library?sort=title"; });
    await expect(page.locator("#pin")).toBeVisible();

    let refused = 0;
    await page.route("**/api/kids/enter", (route) => {
      refused++;
      return route.fulfill({ status: 500 });
    });
    await page.locator("#pin").fill(PIN);
    await page.locator("#login-btn").click();

    // The new session is a parent's, but the device is the kid's: neither the
    // whole library nor Kids chrome claiming that session is a kid's.
    await expect(lockScreen(page)).toBeVisible();
    expect(refused).toBe(1);
    await expectNoBookTitles(page);
    await expect(page.locator(".kids-badge")).toHaveCount(0);
    await expect(page.locator("#profile-switcher-btn")).toHaveCount(0);

    // The server can check the parents' code, so that is the way out.
    await expect(page.getByRole("button", { name: "Forget Kids mode on this device" })).toHaveCount(0);
    await page.unroute("**/api/kids/enter");
    await leaveWith(page, "0000");
    await expect(page.getByText("Incorrect code")).toBeVisible();
    await expect(lockScreen(page)).toBeVisible();
    await page.getByLabel("Parents' code").fill(CODE);
    await page.getByRole("button", { name: "Leave", exact: true }).click();
    await expect(page.locator(".grid .card .title", { hasText: "Book 004" })).toBeVisible();
    await expect(lockScreen(page)).toHaveCount(0);
    await expect(page.locator("#profile-switcher-btn")).toBeVisible();
    expect(await page.evaluate(() => localStorage.getItem("carrel-kids-device"))).toBeNull();
  });

  test("a device that remembers Kids mode stays locked while the server has no web PIN", async ({
    page,
  }) => {
    await rememberKidsDevice(page);
    // After a restart that couldn't read the PIN, the server is open to all.
    await page.route("**/api/kids/status", (route) =>
      route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ kids: false, code_set: true, pin_configured: false }),
      }),
    );
    await page.reload();
    await expect(lockScreen(page)).toBeVisible();
    await expect(
      page.getByText("A parent can set a web PIN and a parents' code in the Carrel desktop app."),
    ).toBeVisible();
    await expectNoBookTitles(page);
    // With no PIN nothing can check a parent, so there is no way out here.
    await expect(page.getByRole("button", { name: "Forget Kids mode on this device" })).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Leave Kids mode" })).toHaveCount(0);
    // A URL doesn't get past it either.
    await page.evaluate(() => { location.hash = "#/library?series=Adult%20Series"; });
    await expect(lockScreen(page)).toBeVisible();
    await expectNoBookTitles(page);

    // Once the server can hold Kids mode again, Try again puts the device back in.
    await page.unroute("**/api/kids/status");
    await page.getByRole("button", { name: "Try again" }).click();
    await expect(badge(page)).toBeVisible();
    await expectKidsGrid(page);
  });

  test("a device that remembers Kids mode is locked when its status can't be read; forgetting needs the web PIN", async ({
    page,
  }) => {
    await rememberKidsDevice(page);
    await page.route("**/api/kids/status", (route) => route.fulfill({ status: 500 }));
    await page.reload();
    await expect(lockScreen(page)).toBeVisible();
    await expectNoBookTitles(page);
    await expect(page.getByRole("button", { name: "Leave Kids mode" })).toHaveCount(0);

    await page.getByRole("button", { name: "Forget Kids mode on this device" }).click();
    await page.getByLabel("Web PIN").fill("0000");
    await page.getByRole("button", { name: "Forget", exact: true }).click();
    await expect(page.getByText("Invalid PIN")).toBeVisible();
    await expect(lockScreen(page)).toBeVisible();

    await page.getByLabel("Web PIN").fill(PIN);
    await page.getByRole("button", { name: "Forget", exact: true }).click();
    await expect(page.locator("#profile-switcher-btn")).toBeVisible();
    await expect(lockScreen(page)).toHaveCount(0);
    await expect(page.locator(".kids-badge")).toHaveCount(0);
    expect(await page.evaluate(() => localStorage.getItem("carrel-kids-device"))).toBeNull();
    await page.evaluate(() => { location.hash = "#/library?series=Adult%20Series"; });
    await expect(page.locator(".grid .card .title")).toHaveText(ADULT_TITLES);
  });

  test("the reader chrome shows the badge, not the household controls", async ({ page }) => {
    expect(
      await apiStatus(page, "PUT", `/api/kids/books/${READER_BOOK_ID}`, { override: "allowed" }),
    ).toBe(200);
    // Control: the parent's reader offers saved words.
    await openEpubReader(page);
    await expect(page.locator("#vocab-btn")).toBeVisible();

    await page.goto("/");
    await turnOnKidsMode(page);
    await openEpubReader(page);
    await expect(page.locator(".reader-chrome-top .kids-badge")).toBeVisible();
    await expect(page.locator("#vocab-btn")).toBeHidden();
    await expect(page.locator("#profile-switcher-btn")).toHaveCount(0);
    await expect(page.locator('[data-nav="stats"], [data-nav="vocabulary"]')).toHaveCount(0);
  });

  test("at phone width the tab bar carries the Kids mode controls", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    // Only the tab bar's copy is shown here, so each role lookup finds one.
    await page.getByRole("button", { name: "Kids mode", exact: true }).click();
    await page.getByRole("button", { name: "Turn on Kids mode" }).click();
    await expect(page.locator(".tab-bar .kids-badge")).toBeVisible();
    await expect(badge(page)).toBeHidden();
    await expectKidsGrid(page);

    await leaveWith(page, CODE);
    await expect(page.locator(".kids-badge")).toHaveCount(0);
    await expect(page.locator('.tab-bar [data-tab="stats"]')).toBeVisible();
  });

  test("the parents' code can be changed from the Kids mode panel", async ({ page }) => {
    await page.getByRole("button", { name: "Kids mode", exact: true }).click();
    await page.getByLabel("Parents' code").fill("12");
    await page.getByRole("button", { name: "Change parents' code" }).click();
    await expect(page.getByText("Parents' code must be 4 to 12 digits")).toBeVisible();

    await page.getByLabel("Parents' code").fill("9753");
    await page.getByRole("button", { name: "Change parents' code" }).click();
    await expect(page.getByText("Parents' code saved")).toBeVisible();
    await page.getByRole("button", { name: "Turn on Kids mode" }).click();
    await expect(badge(page)).toBeVisible();

    // The old code no longer opens it; the new one does.
    await leaveWith(page, CODE);
    await expect(page.getByText("Incorrect code")).toBeVisible();
    await page.getByLabel("Parents' code").fill("9753");
    await page.getByRole("button", { name: "Leave", exact: true }).click();
    await expect(page.locator(".kids-badge")).toHaveCount(0);
  });
});

test.describe("web UI Kids mode without a web PIN", () => {
  // The default harness has no PIN, so the web UI is open to anyone and a
  // lock there would mean nothing.
  test.use({ baseURL: "http://127.0.0.1:7810" });

  test("the panel explains why Kids mode is unavailable", async ({ page }) => {
    await page.goto("/");
    await page.getByRole("button", { name: "Kids mode", exact: true }).click();
    await expect(
      page.getByText("Kids mode needs a web PIN. Set one in the desktop app first."),
    ).toBeVisible();
    await expect(page.getByRole("button", { name: "Turn on Kids mode" })).toHaveCount(0);
  });
});
