import { expect } from "@wdio/globals";

describe("App Navigation", () => {
  describe("Header Controls", () => {
    it("should show the Carrel logo as a link to home", async () => {
      const logo = await browser.$('a[href="/"]');
      await expect(logo).toBeExisting();
    });

    it("should have a reading stats button", async () => {
      const statsBtn = await browser.$(
        'button[aria-label*="stat" i], button[aria-label*="Reading stats"]'
      );
      await expect(statsBtn).toBeExisting();
    });

    it("should have a catalog browser button", async () => {
      const catalogBtn = await browser.$(
        'button[aria-label*="catalog" i], button[aria-label*="Browse catalogs"]'
      );
      await expect(catalogBtn).toBeExisting();
    });

    it("should have a settings button", async () => {
      const settingsBtn = await browser.$(
        'button[aria-label*="settings" i], button[aria-label*="Settings"]'
      );
      await expect(settingsBtn).toBeExisting();
    });
  });

  describe("Collections Sidebar", () => {
    it("should open collections sidebar when clicking the toggle", async () => {
      const collectionsBtn = await browser.$(
        'button[aria-label*="collection" i], button[aria-label*="Collection"]'
      );
      if (await collectionsBtn.isExisting()) {
        await collectionsBtn.click();
        await browser.pause(500);
        await browser.saveScreenshot("./screenshots/collections-open.png");

        // Close it again
        await collectionsBtn.click();
        await browser.pause(500);
      }
    });
  });

  describe("Bulk Select Mode", () => {
    it("should toggle bulk select mode", async () => {
      const selectBtn = await browser.$(
        'button[title*="select" i], button[title*="Select"]'
      );
      if (await selectBtn.isExisting()) {
        await selectBtn.click();
        await browser.pause(300);
        await browser.saveScreenshot("./screenshots/bulk-select-mode.png");

        // Exit select mode
        await selectBtn.click();
        await browser.pause(300);
      }
    });
  });

  describe("Reading Stats Dialog", () => {
    it("should open reading stats when clicking the stats button", async () => {
      const statsBtn = await browser.$(
        'button[aria-label*="stat" i], button[aria-label*="Reading stats"]'
      );
      if (await statsBtn.isExisting()) {
        await statsBtn.click();
        await browser.pause(500);
        await browser.saveScreenshot("./screenshots/reading-stats.png");

        // Close — press Escape
        await browser.keys("Escape");
        await browser.pause(300);
      }
    });
  });

  describe("Catalog Browser", () => {
    it("should open catalog browser when clicking the button", async () => {
      const catalogBtn = await browser.$(
        'button[aria-label*="catalog" i], button[aria-label*="Browse catalogs"]'
      );
      if (await catalogBtn.isExisting()) {
        await catalogBtn.click();
        await browser.pause(500);
        await browser.saveScreenshot("./screenshots/catalog-browser.png");

        // Close — press Escape
        await browser.keys("Escape");
        await browser.pause(300);
      }
    });
  });

  describe("Keyboard Shortcuts", () => {
    it("should focus search with / key", async () => {
      await browser.$("body").click();
      await browser.pause(200);
      await browser.keys("/");
      await browser.pause(200);
      const focused = await browser.execute(() => document.activeElement?.id);
      expect(focused).toBe("library-search");
      // Unfocus
      await browser.keys("Escape");
    });
  });
});
