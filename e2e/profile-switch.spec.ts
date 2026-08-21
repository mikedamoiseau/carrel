import { test, expect, type Page } from "@playwright/test";

// Remote profile switching from the web UI (PR #119).
//
// The harness (src-tauri/examples/web_e2e_server.rs) advertises three profiles
// covering every state the switcher renders: `default` (active), `magazines`
// (switchable) and `vault` (locked and never unlocked on the desktop, so it is
// unreachable over HTTP by design). Switching only moves the advertised active
// flag there — the served library is the same seeded DB — which is all the UI
// needs to exercise.

const activeProfile = (page: Page) =>
  page.evaluate(async () => {
    const profiles = await (await fetch("/api/profiles", { credentials: "same-origin" })).json();
    return profiles.find((p: { active: boolean }) => p.active).name;
  });

async function forceProfile(page: Page, name: string) {
  await page.evaluate(async (profile) => {
    await fetch("/api/profile", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      credentials: "same-origin",
      body: JSON.stringify({ name: profile }),
    });
  }, name);
}

async function openProfileMenu(page: Page) {
  const btn = page.locator("#profile-switcher-btn");
  await expect(btn).toBeVisible();
  await btn.click();
  await expect(page.locator("#profile-panel")).toBeVisible();
}

test.describe("web UI profile switcher", () => {
  test.afterEach(async ({ page }) => {
    // The active profile is server-global state shared with every other spec.
    await page.goto("/");
    await forceProfile(page, "default");
  });

  test("lists every profile, marks the active one, and disables an unreachable locked one", async ({
    page,
  }) => {
    await page.goto("/");
    await openProfileMenu(page);

    const rows = page.locator("#profile-panel .profile-row");
    await expect(rows).toHaveCount(3);
    await expect(page.locator('.profile-row[data-profile="default"]')).toHaveAttribute(
      "aria-current",
      "true"
    );

    const locked = page.locator('.profile-row[data-profile="vault"]');
    await expect(locked).toBeDisabled();
    await expect(locked).toHaveAttribute("title", /Unlock on the desktop/);

    // A switchable profile is offered, not disabled.
    await expect(page.locator('.profile-row[data-profile="magazines"]')).toBeEnabled();
  });

  test("switching moves the server's active profile and the button shows it", async ({ page }) => {
    await page.goto("/");
    expect(await activeProfile(page)).toBe("default");

    await openProfileMenu(page);
    await page.locator('.profile-row[data-profile="magazines"]').click();

    // The switch reloads into the new profile's library, so the button label
    // reflects the server's active profile without a manual refresh.
    await expect(page.locator("#profile-switcher-btn")).toContainText("magazines", {
      timeout: 15_000,
    });
    expect(await activeProfile(page)).toBe("magazines");

    // And the switcher still works from the new profile — back to default.
    await openProfileMenu(page);
    await page.locator('.profile-row[data-profile="default"]').click();
    await expect(page.locator("#profile-switcher-btn")).toContainText("default", {
      timeout: 15_000,
    });
    expect(await activeProfile(page)).toBe("default");
  });

  test("a locked profile cannot be entered from the web UI", async ({ page }) => {
    await page.goto("/");
    await openProfileMenu(page);

    // Disabled, so the click is a no-op — the password is never accepted over
    // the network, and nothing about the session changes.
    await page.locator('.profile-row[data-profile="vault"]').click({ force: true });
    await page.waitForTimeout(500);
    expect(await activeProfile(page)).toBe("default");
  });

  test("a tab left on the old profile reloads itself on its next request", async ({
    context,
  }) => {
    // Two tabs, as if one were left open on a phone while the profile moved —
    // from another device, another tab, or the desktop app. There is one shared
    // active profile, so the stale tab is showing another profile's library and
    // its book ids no longer mean what it thinks.
    const stale = await context.newPage();
    await stale.goto("/");
    await expect(stale.locator("#profile-switcher-btn")).toContainText("default");

    const other = await context.newPage();
    await other.goto("/");
    await openProfileMenu(other);
    await other.locator('.profile-row[data-profile="magazines"]').click();
    await expect(other.locator("#profile-switcher-btn")).toContainText("magazines", {
      timeout: 15_000,
    });

    // The stale tab makes any ordinary request (a navigation that refetches the
    // library) and must notice from the response that it is on the wrong
    // profile, then reload into the new one on its own.
    await stale.locator("#search").fill("Book");
    await expect(stale.locator("#profile-switcher-btn")).toContainText("magazines", {
      timeout: 15_000,
    });

    await stale.close();
    await other.close();
  });

  test("a write with a stale profile tag triggers a reload, not just a status check", async ({
    page,
  }) => {
    // Writes (unlike GETs) used to bypass noteProfileTag entirely — a write
    // issued after the profile moved would commit under the new profile with
    // the old profile's book id and the tab would never notice. Model the
    // move on a single write's response, without touching the server's real
    // (shared) active profile: rewrite just this request's x-carrel-profile
    // header to something other than the page's baseline.
    const bookId = "e2e-book-001";
    await page.goto(`/#/book/${bookId}`);
    await expect(page.locator("#want-btn")).toBeVisible();

    await page.route(`**/api/books/${bookId}/want-to-read`, async (route) => {
      const response = await route.fetch();
      const headers = { ...response.headers(), "x-carrel-profile": "e2e-spoofed-profile" };
      await route.fulfill({ response, headers });
    });

    // A window global only a fresh document load clears — proves an actual
    // reload happened, not merely that the write "succeeded" or that some
    // DOM element changed.
    await page.evaluate(() => {
      (window as unknown as { __stalenessProbe?: string }).__stalenessProbe = "present";
    });

    try {
      await page.locator("#want-btn").click();

      await expect
        .poll(
          async () => page.evaluate(() => (window as unknown as { __stalenessProbe?: string }).__stalenessProbe),
          { timeout: 10_000 }
        )
        .toBeUndefined();
    } finally {
      // The click's PUT really did commit server-side (only its response
      // header was rewritten). want-to-read.spec.ts's flagged-book count
      // assumes this book stays unflagged, and the DB persists across the
      // whole (workers=1) run — reset it via page.request, which is a direct
      // API client and isn't caught by the page.route above.
      await page.request
        .put(`/api/books/${bookId}/want-to-read`, { data: { want_to_read: false } })
        .catch(() => {});
    }
  });

  test("switching profile from a non-library view still lands on the library", async ({
    page,
  }) => {
    // switchProfile's own write deliberately does NOT go through the shared
    // reload guard (its response necessarily carries a different tag than
    // the page's baseline, by design). If it ever did, that reload would
    // race switchProfile's own location.hash="#/" reset and could leave the
    // tab stuck on the book-detail view instead of the library it just
    // switched into.
    const bookId = "e2e-book-001";
    await page.goto(`/#/book/${bookId}`);
    await expect(page.locator("#want-btn")).toBeVisible();

    await openProfileMenu(page);
    await page.locator('.profile-row[data-profile="magazines"]').click();

    await expect(page).toHaveURL(/#\/$/, { timeout: 15_000 });
    await expect(page.locator("#profile-switcher-btn")).toContainText("magazines", {
      timeout: 15_000,
    });
  });

  test("the menu closes on an outside click", async ({ page }) => {
    await page.goto("/");
    await openProfileMenu(page);
    await page.locator("h1").first().click();
    await expect(page.locator("#profile-panel")).toBeHidden();
  });
});
