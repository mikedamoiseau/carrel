import { TOKEN_NAMES, type ColorTokens, type ColorMode, isValidColorMode } from "./themes";
import type { TypographySettings } from "../context/ThemeContext";

export interface SavedTheme {
  id: string;
  name: string;
  mode: ColorMode;
  colors: ColorTokens;
  fontFamily: string;
  fontSize: number;
  typography: TypographySettings;
  createdAt: number;
}

const STORAGE_KEY = "carrel-saved-themes";

const HEX_COLOR_RE = /^#[0-9a-fA-F]{6}$/;
const VALID_FONT_FAMILIES = new Set(["serif", "literata", "sans-serif", "dyslexic"]);
const CUSTOM_FONT_RE = /^custom:[0-9a-f-]+$/i;
const MAX_SAVED_THEMES = 50;

function isValidTheme(obj: unknown): obj is SavedTheme {
  if (!obj || typeof obj !== "object") return false;
  const t = obj as Record<string, unknown>;
  return (
    typeof t.id === "string" &&
    typeof t.name === "string" &&
    typeof t.mode === "string" && isValidColorMode(t.mode) &&
    typeof t.colors === "object" && t.colors !== null &&
    TOKEN_NAMES.every((name) => {
      const v = (t.colors as Record<string, unknown>)[name];
      return typeof v === "string" && HEX_COLOR_RE.test(v);
    }) &&
    typeof t.fontFamily === "string" && (VALID_FONT_FAMILIES.has(t.fontFamily) || CUSTOM_FONT_RE.test(t.fontFamily)) &&
    typeof t.fontSize === "number" && Number.isFinite(t.fontSize) &&
    typeof t.typography === "object" && t.typography !== null &&
    typeof (t.typography as Record<string, unknown>).lineHeight === "number" && Number.isFinite((t.typography as Record<string, unknown>).lineHeight) &&
    typeof (t.typography as Record<string, unknown>).pageMargins === "number" && Number.isFinite((t.typography as Record<string, unknown>).pageMargins) &&
    ((t.typography as Record<string, unknown>).textAlign === "left" || (t.typography as Record<string, unknown>).textAlign === "justify") &&
    typeof (t.typography as Record<string, unknown>).paragraphSpacing === "number" && Number.isFinite((t.typography as Record<string, unknown>).paragraphSpacing) &&
    typeof (t.typography as Record<string, unknown>).hyphenation === "boolean" &&
    typeof t.createdAt === "number" && Number.isFinite(t.createdAt) && t.createdAt > 0
  );
}

/** Hydrate themes saved before `mode` was added (pre-v0.x) as "custom". */
function migrateTheme(obj: unknown): unknown {
  if (!obj || typeof obj !== "object") return obj;
  const t = obj as Record<string, unknown>;
  if (t.mode === undefined) {
    return { ...t, mode: "custom" };
  }
  return obj;
}

export function loadSavedThemes(): SavedTheme[] {
  const stored = localStorage.getItem(STORAGE_KEY);
  if (!stored) return [];
  try {
    const parsed = JSON.parse(stored);
    if (!Array.isArray(parsed)) return [];
    return parsed.map(migrateTheme).filter(isValidTheme);
  } catch {
    return [];
  }
}

export function saveSavedThemes(themes: SavedTheme[]): boolean {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(themes));
    return true;
  } catch {
    return false;
  }
}

export function addTheme(themes: SavedTheme[], theme: SavedTheme): SavedTheme[] {
  const idIdx = themes.findIndex((t) => t.id === theme.id);
  if (idIdx !== -1) {
    return themes.map((t, i) => (i === idIdx ? theme : t));
  }
  if (themes.some((t) => t.name.toLowerCase() === theme.name.toLowerCase())) return themes;
  if (themes.length >= MAX_SAVED_THEMES) return themes;
  return [...themes, theme];
}

export function deleteTheme(themes: SavedTheme[], id: string): SavedTheme[] {
  return themes.filter((t) => t.id !== id);
}

export function renameTheme(themes: SavedTheme[], id: string, newName: string): SavedTheme[] {
  if (themes.some((t) => t.name.toLowerCase() === newName.toLowerCase() && t.id !== id)) return themes;
  return themes.map((t) => (t.id === id ? { ...t, name: newName } : t));
}
