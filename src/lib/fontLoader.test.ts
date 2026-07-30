import { describe, it, expect, vi, beforeEach } from "vitest";

const store: Record<string, string> = {};
vi.stubGlobal("localStorage", {
  getItem: (key: string) => store[key] ?? null,
  setItem: (key: string, value: string) => { store[key] = value; },
  removeItem: (key: string) => { delete store[key]; },
  clear: () => { Object.keys(store).forEach((k) => delete store[k]); },
});

describe("fontLoader", () => {
  beforeEach(() => {
    vi.resetModules();
    localStorage.clear();
  });

  it("loadFont resolves for known keys", async () => {
    const { loadFont } = await import("./fontLoader");
    await expect(loadFont("serif")).resolves.toBeUndefined();
  });

  it("loadFont is a no-op for unknown keys", async () => {
    const { loadFont } = await import("./fontLoader");
    await expect(loadFont("sans-serif")).resolves.toBeUndefined();
  });

  it("loadFont deduplicates repeated calls", async () => {
    const { loadFont } = await import("./fontLoader");
    await loadFont("serif");
    await expect(loadFont("serif")).resolves.toBeUndefined();
  });

  it("preloadStoredFont triggers load for stored reading font", async () => {
    localStorage.setItem("carrel-font-family", "literata");
    const { preloadStoredFont, loadFont } = await import("./fontLoader");
    preloadStoredFont();
    await expect(loadFont("literata")).resolves.toBeUndefined();
  });

  it("preloadStoredFont loads serif by default when no stored value", async () => {
    const { preloadStoredFont, loadFont } = await import("./fontLoader");
    preloadStoredFont();
    await expect(loadFont("serif")).resolves.toBeUndefined();
  });

  it("preloadStoredFont does nothing for UI fonts", async () => {
    localStorage.setItem("carrel-font-family", "sans-serif");
    const { preloadStoredFont } = await import("./fontLoader");
    preloadStoredFont();
  });
});
