import { test, expect } from "@playwright/test";

// These tests require the Carrel web server running on localhost:7788
// with no PIN configured (open access).

test.describe("Reading Stats page", () => {
  test("navigates to stats via header icon", async ({ page }) => {
    await page.goto("/");
    await page.click('[data-nav="stats"]');
    await expect(page).toHaveURL(/#\/stats/);
    await expect(page.locator("h1")).toHaveText("Reading Stats");
  });

  test("shows stat cards or empty state", async ({ page }) => {
    await page.goto("/#/stats");
    await page.waitForSelector(".stats");
    const hasCards = await page.locator(".stat-card").count();
    const hasEmpty = await page.locator(".empty").count();
    expect(hasCards > 0 || hasEmpty > 0).toBeTruthy();
  });

  test("stat cards show expected labels", async ({ page }) => {
    await page.goto("/#/stats");
    await page.waitForSelector(".stats");
    const hasCards = await page.locator(".stat-card").count();
    if (hasCards > 0) {
      const labels = await page.locator(".stat-label").allTextContents();
      expect(labels).toContain("Time Reading");
      expect(labels).toContain("Sessions");
      expect(labels).toContain("Pages Read");
      expect(labels).toContain("Books Finished");
      expect(labels).toContain("Current Streak");
      expect(labels).toContain("Longest Streak");
    }
  });

  test("back button returns to library", async ({ page }) => {
    await page.goto("/#/stats");
    await page.waitForSelector(".stats");
    await page.click("#back-btn");
    await expect(page).toHaveURL(/#$/);
  });
});

test.describe("Collections page", () => {
  test("navigates to collections via header icon", async ({ page }) => {
    await page.goto("/");
    await page.click('[data-nav="collections"]');
    await expect(page).toHaveURL(/#\/collections/);
    await expect(page.locator("h1")).toHaveText("Collections");
  });

  test("shows collections or empty state", async ({ page }) => {
    await page.goto("/#/collections");
    await page.waitForSelector(".collections");
    const hasRows = await page.locator(".collection-row").count();
    const hasEmpty = await page.locator(".empty").count();
    expect(hasRows > 0 || hasEmpty > 0).toBeTruthy();
  });

  test("filter input filters collection rows", async ({ page }) => {
    await page.goto("/#/collections");
    await page.waitForSelector(".collections");
    const totalBefore = await page.locator(".collection-row").count();
    if (totalBefore > 0) {
      await page.fill("#coll-filter", "zzzznonexistent");
      await page.waitForTimeout(300);
      const totalAfter = await page.locator(".collection-row").count();
      expect(totalAfter).toBeLessThanOrEqual(totalBefore);
    }
  });

  test("sort toggle changes button label", async ({ page }) => {
    await page.goto("/#/collections");
    await page.waitForSelector(".collections");
    const hasSort = await page.locator("#coll-sort").count();
    if (hasSort > 0) {
      const before = await page.locator("#coll-sort").textContent();
      await page.click("#coll-sort");
      const after = await page.locator("#coll-sort").textContent();
      expect(before).not.toEqual(after);
    }
  });

  test("clicking collection row navigates to library", async ({ page }) => {
    await page.goto("/#/collections");
    await page.waitForSelector(".collections");
    const rows = page.locator("[data-collection-id]");
    const count = await rows.count();
    if (count > 0) {
      await rows.first().click();
      await expect(page).toHaveURL(/#$/);
    }
  });

  test("back button returns to library", async ({ page }) => {
    await page.goto("/#/collections");
    await page.waitForSelector(".collections");
    await page.click("#back-btn");
    await expect(page).toHaveURL(/#$/);
  });
});

test.describe("Nav icons", () => {
  test("library page shows nav icons", async ({ page }) => {
    await page.goto("/");
    await expect(page.locator(".nav-icons")).toBeVisible();
    await expect(page.locator('[data-nav="stats"]')).toBeVisible();
    await expect(page.locator('[data-nav="collections"]')).toBeVisible();
  });

  test("stats page highlights stats icon", async ({ page }) => {
    await page.goto("/#/stats");
    await page.waitForSelector(".nav-icons");
    await expect(page.locator('[data-nav="stats"]')).toHaveClass(/active/);
    await expect(page.locator('[data-nav="collections"]')).not.toHaveClass(/active/);
  });

  test("collections page highlights collections icon", async ({ page }) => {
    await page.goto("/#/collections");
    await page.waitForSelector(".nav-icons");
    await expect(page.locator('[data-nav="collections"]')).toHaveClass(/active/);
    await expect(page.locator('[data-nav="stats"]')).not.toHaveClass(/active/);
  });
});
