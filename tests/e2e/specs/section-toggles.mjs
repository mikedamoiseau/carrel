import { expect } from "@wdio/globals";

// Shared helpers for both toggle test suites
async function openSettings() {
  const settingsBtn = await browser.$(
    'button[aria-label="Open settings"], button[aria-label*="settings" i]'
  );
  await settingsBtn.waitForExist({ timeout: 10000 });
  await settingsBtn.click();
  await browser.pause(500);
}

async function closeSettings() {
  const closeBtn = await browser.$(
    'button[aria-label*="close" i], button[aria-label*="fermer" i]'
  );
  if (await closeBtn.isExisting()) {
    await closeBtn.click();
    await browser.pause(300);
  }
}

async function openGeneralSection() {
  const accordions = await browser.$$("button[aria-expanded]");
  for (const acc of accordions) {
    const text = await acc.getText();
    if (
      text.toLowerCase().includes("general") ||
      text.toLowerCase().includes("général")
    ) {
      const expanded = await acc.getAttribute("aria-expanded");
      if (expanded === "false") {
        await acc.click();
        await browser.pause(300);
      }
      break;
    }
  }
}

describe("Continue Reading Section Toggle", () => {
  it("should show continue reading toggle in settings General tab, enabled by default", async () => {
    await openSettings();
    await openGeneralSection();

    const toggle = await browser.$('[data-testid="show-continue-reading-toggle"]');
    await expect(toggle).toBeExisting();

    // Should be ON by default
    const checked = await toggle.getAttribute("aria-checked");
    expect(checked).toBe("true");
  });

  it("should hide continue reading section when toggle is turned off", async () => {
    const toggle = await browser.$('[data-testid="show-continue-reading-toggle"]');
    await toggle.click();
    await browser.pause(300);

    const checked = await toggle.getAttribute("aria-checked");
    expect(checked).toBe("false");

    const stored = await browser.execute(
      () => localStorage.getItem("carrel-show-continue-reading")
    );
    expect(stored).toBe("false");

    await closeSettings();
    await browser.pause(500);

    // Verify localStorage persisted the off state
    const storedVal = await browser.execute(
      () => localStorage.getItem("carrel-show-continue-reading")
    );
    expect(storedVal).toBe("false");
  });

  it("should re-enable continue reading section when toggle is turned back on", async () => {
    await openSettings();
    await openGeneralSection();

    const toggle = await browser.$('[data-testid="show-continue-reading-toggle"]');
    await toggle.click();
    await browser.pause(300);

    const checked = await toggle.getAttribute("aria-checked");
    expect(checked).toBe("true");

    const stored = await browser.execute(
      () => localStorage.getItem("carrel-show-continue-reading")
    );
    expect(stored).toBe("true");

    await closeSettings();
  });
});

describe("Discover Section Toggle", () => {
  it("should default to discover section hidden", async () => {
    await browser.pause(2000);
    const discoverSection = await browser.$('[data-testid="discover-section"]');
    const exists = await discoverSection.isExisting();
    expect(exists).toBe(false);
  });

  it("should show discover toggle in settings General tab, off by default", async () => {
    await openSettings();
    await openGeneralSection();

    const toggle = await browser.$('[data-testid="show-discover-toggle"]');
    await expect(toggle).toBeExisting();

    const checked = await toggle.getAttribute("aria-checked");
    expect(checked).toBe("false");
  });

  it("should enable discover section when toggle is turned on", async () => {
    const toggle = await browser.$('[data-testid="show-discover-toggle"]');
    await toggle.click();
    await browser.pause(300);

    const checked = await toggle.getAttribute("aria-checked");
    expect(checked).toBe("true");

    const stored = await browser.execute(
      () => localStorage.getItem("carrel-show-discover")
    );
    expect(stored).toBe("true");
  });

  it("should show discover section on library after enabling", async () => {
    await closeSettings();
    await browser.pause(3000);

    const discoverSection = await browser.$('[data-testid="discover-section"]');
    const exists = await discoverSection.isExisting();
    expect(exists).toBe(true);
  });

  it("should hide discover section when toggle is turned off", async () => {
    await openSettings();
    await openGeneralSection();

    const toggle = await browser.$('[data-testid="show-discover-toggle"]');
    await toggle.click();
    await browser.pause(300);

    const checked = await toggle.getAttribute("aria-checked");
    expect(checked).toBe("false");

    await closeSettings();
    await browser.pause(500);

    const discoverSection = await browser.$('[data-testid="discover-section"]');
    const exists = await discoverSection.isExisting();
    expect(exists).toBe(false);
  });

  it("should persist the setting in localStorage", async () => {
    await openSettings();
    await openGeneralSection();

    const toggle = await browser.$('[data-testid="show-discover-toggle"]');
    await toggle.click();
    await browser.pause(300);

    const stored = await browser.execute(
      () => localStorage.getItem("carrel-show-discover")
    );
    expect(stored).toBe("true");

    // Disable to leave clean state
    await toggle.click();
    await browser.pause(300);

    const storedAfter = await browser.execute(
      () => localStorage.getItem("carrel-show-discover")
    );
    expect(storedAfter).toBe("false");

    await closeSettings();
  });
});
