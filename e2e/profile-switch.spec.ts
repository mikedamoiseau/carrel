import { test, expect, type Page } from "@playwright/test";

// Remote profile switching from the web UI
// (PRD docs/backlog/2026-07-26-remote-profile-switch.md).
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

  test("the menu closes on an outside click", async ({ page }) => {
    await page.goto("/");
    await openProfileMenu(page);
    await page.locator("h1").first().click();
    await expect(page.locator("#profile-panel")).toBeHidden();
  });
});
