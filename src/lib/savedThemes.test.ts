import { describe, it, expect, beforeEach, vi } from "vitest";
import type { SavedTheme } from "./savedThemes";
import {
  loadSavedThemes,
  saveSavedThemes,
  addTheme,
  deleteTheme,
  renameTheme,
} from "./savedThemes";

// ---------------------------------------------------------------------------
// localStorage mock (vitest runs in node; no browser globals)
// ---------------------------------------------------------------------------
const localStorageStore: Record<string, string> = {};
const localStorageMock = {
  getItem: (key: string) => localStorageStore[key] ?? null,
  setItem: (key: string, value: string) => { localStorageStore[key] = value; },
  removeItem: (key: string) => { delete localStorageStore[key]; },
  clear: () => { Object.keys(localStorageStore).forEach((k) => delete localStorageStore[k]); },
};
vi.stubGlobal("localStorage", localStorageMock);

function makeTheme(overrides: Partial<SavedTheme> = {}): SavedTheme {
  return {
    id: "id-1",
    name: "Theme 1",
    mode: "custom",
    colors: {
      paper: "#ffffff",
      surface: "#ffffff",
      ink: "#000000",
      "ink-muted": "#888888",
      "warm-border": "#dddddd",
      "warm-subtle": "#eeeeee",
      accent: "#cc0000",
      "accent-hover": "#aa0000",
      "accent-light": "#ffeeee",
    },
    fontFamily: "serif",
    fontSize: 18,
    typography: {
      lineHeight: 1.8,
      pageMargins: 32,
      textAlign: "justify",
      paragraphSpacing: 1.1,
      hyphenation: true,
    },
    createdAt: Date.now(),
    ...overrides,
  };
}

// ---------------------------------------------------------------------------
// loadSavedThemes
// ---------------------------------------------------------------------------
describe("loadSavedThemes", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("returns empty array when localStorage is empty", () => {
    expect(loadSavedThemes()).toEqual([]);
  });

  it("returns valid themes from localStorage", () => {
    const theme = makeTheme();
    localStorage.setItem("carrel-saved-themes", JSON.stringify([theme]));
    const result = loadSavedThemes();
    expect(result).toHaveLength(1);
    expect(result[0].id).toBe("id-1");
    expect(result[0].name).toBe("Theme 1");
  });

  it("returns empty array for corrupted JSON", () => {
    localStorage.setItem("carrel-saved-themes", "not-valid-json{{{");
    expect(loadSavedThemes()).toEqual([]);
  });

  it("returns empty array when stored value is not an array", () => {
    localStorage.setItem("carrel-saved-themes", JSON.stringify({ id: "id-1" }));
    expect(loadSavedThemes()).toEqual([]);
  });

  it("migrates pre-upgrade themes without mode field as custom", () => {
    // Simulate a theme saved by the old schema (no `mode` field)
    const { mode: _mode, ...legacyTheme } = makeTheme({ id: "id-legacy", name: "Legacy" });
    localStorage.setItem("carrel-saved-themes", JSON.stringify([legacyTheme]));
    const result = loadSavedThemes();
    expect(result).toHaveLength(1);
    expect(result[0].id).toBe("id-legacy");
    expect(result[0].mode).toBe("custom");
  });

  it("does not overwrite an existing mode during migration", () => {
    const theme = makeTheme({ id: "id-dark", name: "Dark Theme", mode: "dark" });
    localStorage.setItem("carrel-saved-themes", JSON.stringify([theme]));
    const result = loadSavedThemes();
    expect(result).toHaveLength(1);
    expect(result[0].mode).toBe("dark");
  });

  it("filters out malformed entries, keeps valid ones", () => {
    const valid = makeTheme({ id: "id-valid" });
    const malformed = { id: 123, name: null }; // missing required fields
    localStorage.setItem(
      "carrel-saved-themes",
      JSON.stringify([malformed, valid])
    );
    const result = loadSavedThemes();
    expect(result).toHaveLength(1);
    expect(result[0].id).toBe("id-valid");
  });

  it("filters out entries missing colors object", () => {
    const bad = { ...makeTheme(), colors: "not-an-object" };
    localStorage.setItem("carrel-saved-themes", JSON.stringify([bad]));
    expect(loadSavedThemes()).toEqual([]);
  });

  it("filters out entries missing typography object", () => {
    const bad = { ...makeTheme(), typography: null };
    localStorage.setItem("carrel-saved-themes", JSON.stringify([bad]));
    expect(loadSavedThemes()).toEqual([]);
  });

  it("filters out entries with empty typography object", () => {
    const bad = { ...makeTheme(), typography: {} };
    localStorage.setItem("carrel-saved-themes", JSON.stringify([bad]));
    expect(loadSavedThemes()).toEqual([]);
  });

  it("filters out entries with wrong-typed typography fields", () => {
    const bad = {
      ...makeTheme(),
      typography: {
        lineHeight: "not-a-number",
        pageMargins: 32,
        textAlign: "justify",
        paragraphSpacing: 1.1,
        hyphenation: true,
      },
    };
    localStorage.setItem("carrel-saved-themes", JSON.stringify([bad]));
    expect(loadSavedThemes()).toEqual([]);
  });

  it("filters out entries with invalid textAlign value", () => {
    const bad = {
      ...makeTheme(),
      typography: {
        lineHeight: 1.8,
        pageMargins: 32,
        textAlign: "center",
        paragraphSpacing: 1.1,
        hyphenation: true,
      },
    };
    localStorage.setItem("carrel-saved-themes", JSON.stringify([bad]));
    expect(loadSavedThemes()).toEqual([]);
  });

  it("filters out entries with non-boolean hyphenation", () => {
    const bad = {
      ...makeTheme(),
      typography: {
        lineHeight: 1.8,
        pageMargins: 32,
        textAlign: "justify",
        paragraphSpacing: 1.1,
        hyphenation: "yes",
      },
    };
    localStorage.setItem("carrel-saved-themes", JSON.stringify([bad]));
    expect(loadSavedThemes()).toEqual([]);
  });

  it("filters out entries with NaN numeric fields", () => {
    const bad = { ...makeTheme(), fontSize: NaN };
    localStorage.setItem("carrel-saved-themes", JSON.stringify([bad]));
    expect(loadSavedThemes()).toEqual([]);
  });

  it("filters out entries with Infinity in typography", () => {
    const bad = {
      ...makeTheme(),
      typography: {
        lineHeight: Infinity,
        pageMargins: 32,
        textAlign: "justify",
        paragraphSpacing: 1.1,
        hyphenation: true,
      },
    };
    localStorage.setItem("carrel-saved-themes", JSON.stringify([bad]));
    expect(loadSavedThemes()).toEqual([]);
  });

  it("filters out entries with non-positive createdAt", () => {
    const bad = { ...makeTheme(), createdAt: -1 };
    localStorage.setItem("carrel-saved-themes", JSON.stringify([bad]));
    expect(loadSavedThemes()).toEqual([]);
  });

  it("filters out entries with incomplete color tokens", () => {
    const bad = { ...makeTheme(), colors: { paper: "#ffffff" } }; // missing 8 tokens
    localStorage.setItem("carrel-saved-themes", JSON.stringify([bad]));
    expect(loadSavedThemes()).toEqual([]);
  });

  it("filters out entries with empty colors object", () => {
    const bad = { ...makeTheme(), colors: {} };
    localStorage.setItem("carrel-saved-themes", JSON.stringify([bad]));
    expect(loadSavedThemes()).toEqual([]);
  });

  it("returns multiple valid themes", () => {
    const themes = [
      makeTheme({ id: "id-1", name: "Theme 1" }),
      makeTheme({ id: "id-2", name: "Theme 2" }),
    ];
    localStorage.setItem("carrel-saved-themes", JSON.stringify(themes));
    expect(loadSavedThemes()).toHaveLength(2);
  });
});

// ---------------------------------------------------------------------------
// saveSavedThemes
// ---------------------------------------------------------------------------
describe("saveSavedThemes", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("persists themes to localStorage", () => {
    const themes = [makeTheme()];
    saveSavedThemes(themes);
    expect(localStorage.getItem("carrel-saved-themes")).not.toBeNull();
  });

  it("persisted themes can be reloaded", () => {
    const theme = makeTheme({ id: "id-persist", name: "Persisted" });
    saveSavedThemes([theme]);
    const reloaded = loadSavedThemes();
    expect(reloaded).toHaveLength(1);
    expect(reloaded[0].id).toBe("id-persist");
    expect(reloaded[0].name).toBe("Persisted");
  });

  it("overwrites previous value on subsequent saves", () => {
    saveSavedThemes([makeTheme({ id: "id-1", name: "First" })]);
    saveSavedThemes([makeTheme({ id: "id-2", name: "Second" })]);
    const reloaded = loadSavedThemes();
    expect(reloaded).toHaveLength(1);
    expect(reloaded[0].id).toBe("id-2");
  });

  it("persists empty array", () => {
    saveSavedThemes([]);
    expect(loadSavedThemes()).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// addTheme
// ---------------------------------------------------------------------------
describe("addTheme", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("adds a new theme to an empty list", () => {
    const theme = makeTheme();
    const result = addTheme([], theme);
    expect(result).toHaveLength(1);
    expect(result[0].id).toBe("id-1");
  });

  it("appends a new theme when name is unique", () => {
    const existing = makeTheme({ id: "id-1", name: "Theme 1" });
    const newTheme = makeTheme({ id: "id-2", name: "Theme 2" });
    const result = addTheme([existing], newTheme);
    expect(result).toHaveLength(2);
  });

  it("updates existing theme by id, preserving position", () => {
    const original = makeTheme({ id: "id-1", name: "My Theme", fontSize: 16 });
    const updated = makeTheme({ id: "id-1", name: "My Theme", fontSize: 20 });
    const result = addTheme([original], updated);
    expect(result).toHaveLength(1);
    expect(result[0].id).toBe("id-1");
    expect(result[0].fontSize).toBe(20);
  });

  it("rejects new theme when name already exists with different id", () => {
    const existing = makeTheme({ id: "id-1", name: "My Theme" });
    const duplicate = makeTheme({ id: "id-2", name: "My Theme" });
    const result = addTheme([existing], duplicate);
    expect(result).toHaveLength(1);
    expect(result[0].id).toBe("id-1");
  });

  it("does not mutate the original array", () => {
    const original: SavedTheme[] = [];
    addTheme(original, makeTheme());
    expect(original).toHaveLength(0);
  });

  it("new theme appears at end of list", () => {
    const themes = [
      makeTheme({ id: "id-1", name: "Alpha" }),
      makeTheme({ id: "id-2", name: "Beta" }),
    ];
    const newTheme = makeTheme({ id: "id-3", name: "Gamma" });
    const result = addTheme(themes, newTheme);
    expect(result[result.length - 1].name).toBe("Gamma");
  });
});

// ---------------------------------------------------------------------------
// deleteTheme
// ---------------------------------------------------------------------------
describe("deleteTheme", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("removes theme by id", () => {
    const themes = [
      makeTheme({ id: "id-1", name: "Theme 1" }),
      makeTheme({ id: "id-2", name: "Theme 2" }),
    ];
    const result = deleteTheme(themes, "id-1");
    expect(result).toHaveLength(1);
    expect(result[0].id).toBe("id-2");
  });

  it("is a no-op for unknown id", () => {
    const themes = [makeTheme({ id: "id-1", name: "Theme 1" })];
    const result = deleteTheme(themes, "id-nonexistent");
    expect(result).toHaveLength(1);
  });

  it("returns empty array when last theme is deleted", () => {
    const themes = [makeTheme({ id: "id-1" })];
    const result = deleteTheme(themes, "id-1");
    expect(result).toHaveLength(0);
  });

  it("does not mutate the original array", () => {
    const themes = [makeTheme({ id: "id-1" })];
    deleteTheme(themes, "id-1");
    expect(themes).toHaveLength(1);
  });

  it("only removes the matched theme, not others with similar names", () => {
    const themes = [
      makeTheme({ id: "id-1", name: "Theme A" }),
      makeTheme({ id: "id-2", name: "Theme B" }),
      makeTheme({ id: "id-3", name: "Theme C" }),
    ];
    const result = deleteTheme(themes, "id-2");
    expect(result).toHaveLength(2);
    expect(result.map((t) => t.id)).toEqual(["id-1", "id-3"]);
  });
});

// ---------------------------------------------------------------------------
// renameTheme
// ---------------------------------------------------------------------------
describe("renameTheme", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("renames a theme by id", () => {
    const themes = [makeTheme({ id: "id-1", name: "Old Name" })];
    const result = renameTheme(themes, "id-1", "New Name");
    expect(result[0].name).toBe("New Name");
  });

  it("is a no-op for unknown id", () => {
    const themes = [makeTheme({ id: "id-1", name: "Theme 1" })];
    const result = renameTheme(themes, "id-nonexistent", "New Name");
    expect(result[0].name).toBe("Theme 1");
  });

  it("only renames the matched theme", () => {
    const themes = [
      makeTheme({ id: "id-1", name: "Theme 1" }),
      makeTheme({ id: "id-2", name: "Theme 2" }),
    ];
    const result = renameTheme(themes, "id-1", "Renamed");
    expect(result[0].name).toBe("Renamed");
    expect(result[1].name).toBe("Theme 2");
  });

  it("does not mutate the original array", () => {
    const themes = [makeTheme({ id: "id-1", name: "Original" })];
    renameTheme(themes, "id-1", "Changed");
    expect(themes[0].name).toBe("Original");
  });

  it("preserves all other fields when renaming", () => {
    const theme = makeTheme({ id: "id-1", name: "Old" });
    const result = renameTheme([theme], "id-1", "New");
    const renamed = result[0];
    expect(renamed.id).toBe("id-1");
    expect(renamed.colors).toEqual(theme.colors);
    expect(renamed.fontFamily).toBe(theme.fontFamily);
    expect(renamed.fontSize).toBe(theme.fontSize);
    expect(renamed.typography).toEqual(theme.typography);
    expect(renamed.createdAt).toBe(theme.createdAt);
  });

  it("returns same length array after rename", () => {
    const themes = [
      makeTheme({ id: "id-1", name: "A" }),
      makeTheme({ id: "id-2", name: "B" }),
    ];
    const result = renameTheme(themes, "id-2", "Beta");
    expect(result).toHaveLength(2);
  });

  it("rejects rename when newName already exists on another theme", () => {
    const themes = [
      makeTheme({ id: "id-1", name: "Alpha" }),
      makeTheme({ id: "id-2", name: "Beta" }),
    ];
    const result = renameTheme(themes, "id-2", "Alpha");
    expect(result[0].name).toBe("Alpha");
    expect(result[1].name).toBe("Beta");
  });
});
