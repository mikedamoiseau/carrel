import { expect } from "@wdio/globals";

describe("Window Management", () => {
  it("should report the correct window title", async () => {
    const title = await browser.getTitle();
    expect(title).toBe("Carrel");
  });

  it("should have a valid window size", async () => {
    const { width, height } = await browser.getWindowRect();
    expect(width).toBeGreaterThan(400);
    expect(height).toBeGreaterThan(300);
  });

  it("should be able to resize the window", async () => {
    await browser.setWindowRect(null, null, 1024, 768);
    const { width, height } = await browser.getWindowRect();
    expect(width).toBe(1024);
    expect(height).toBe(768);
  });

  it("should be able to maximize the window", async () => {
    await browser.maximizeWindow();
    const { width, height } = await browser.getWindowRect();
    expect(width).toBeGreaterThan(1000);
    expect(height).toBeGreaterThan(600);
  });

  it("should restore to a standard size", async () => {
    await browser.setWindowRect(null, null, 800, 600);
    const { width, height } = await browser.getWindowRect();
    expect(width).toBe(800);
    expect(height).toBe(600);
  });
});
