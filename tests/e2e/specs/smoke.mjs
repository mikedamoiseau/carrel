import { expect } from "@wdio/globals";

describe("Carrel App - Smoke Test", () => {
  it("should launch and show the correct window title", async () => {
    const title = await browser.getTitle();
    expect(title).toBe("Carrel");
  });

  it("should render the app container", async () => {
    const app = await browser.$("#root");
    await expect(app).toBeExisting();
  });

  it("should display the Carrel logo/link in navigation", async () => {
    const logo = await browser.$('a[href="/"]');
    await logo.waitForExist({ timeout: 10000 });
    const text = await logo.getText();
    expect(text).toContain("Carrel");
  });

  it("should show the settings button in the header", async () => {
    const settingsBtn = await browser.$('button[aria-label="Open settings"]');
    await expect(settingsBtn).toBeExisting();
  });

  it("should take a launch screenshot", async () => {
    await browser.saveScreenshot("./screenshots/launch.png");
  });
});
