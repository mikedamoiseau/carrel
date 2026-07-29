import {
  createContext,
  useContext,
  useState,
  useEffect,
  useCallback,
  useMemo,
  useRef,
  type ReactNode,
} from "react";
import {
  type ColorMode,
  type ColorTokens,
  TOKEN_NAMES,
  isValidColorMode,
  SEPIA_TOKENS,
  DEFAULT_CUSTOM_TOKENS,
  applyTokensToRoot,
  clearRootTokens,
} from "../lib/themes";
import { sanitizeCss } from "../lib/utils";

export type { ColorMode, ColorTokens };

type ResolvedTheme = "light" | "dark";
type FontFamily = string;
type ScrollMode = "paginated" | "continuous";
type TextAlign = "left" | "justify";

export interface TypographySettings {
  lineHeight: number;     // 1.2 – 2.4, default 1.8
  pageMargins: number;    // 0 – 80 (px), default 32 (px-8)
  textAlign: TextAlign;   // default "justify"
  paragraphSpacing: number; // 0 – 2 (em), default 1.1
  hyphenation: boolean;   // default true
}

interface ThemeContextValue {
  mode: ColorMode;
  resolved: ResolvedTheme;
  setMode: (mode: ColorMode) => void;
  customColors: ColorTokens;
  setCustomColors: (colors: ColorTokens) => void;
  fontSize: number;
  setFontSize: (size: number) => void;
  fontFamily: FontFamily;
  setFontFamily: (family: FontFamily) => void;
  scrollMode: ScrollMode;
  setScrollMode: (mode: ScrollMode) => void;
  typography: TypographySettings;
  setTypography: (t: TypographySettings) => void;
  customCss: string;
  setCustomCss: (css: string) => void;
  dualPage: boolean;
  setDualPage: (enabled: boolean) => void;
  mangaMode: boolean;
  setMangaMode: (enabled: boolean) => void;
  pageAnimation: boolean;
  setPageAnimation: (enabled: boolean) => void;
  loadTheme: (theme: { colors: ColorTokens; fontFamily: string; fontSize: number; typography: TypographySettings }) => void;
}

// These keys keep their `folio-` prefix after the Carrel rename — they name
// values already in every user's localStorage, and renaming them silently
// resets theme, typography, and reader preferences. Same for the library
// filters in `src/screens/Library.tsx` and the web UI's keys in
// `static/app.js`. See CLAUDE.md, "Legacy `folio` identifiers".
const STORAGE_KEYS = {
  theme: "folio-theme",
  customColors: "folio-custom-colors",
  fontSize: "folio-font-size",
  fontFamily: "folio-font-family",
  scrollMode: "folio-scroll-mode",
  typography: "folio-typography",
  customCss: "folio-custom-css",
  dualPage: "folio-dual-page",
  mangaMode: "folio-manga-mode",
  pageAnimation: "folio-page-animation",
} as const;

export const MIN_FONT_SIZE = 14;
export const MAX_FONT_SIZE = 24;
const DEFAULT_FONT_SIZE = 18;

const DEFAULT_TYPOGRAPHY: TypographySettings = {
  lineHeight: 1.8,
  pageMargins: 32,
  textAlign: "justify",
  paragraphSpacing: 1.1,
  hyphenation: true,
};

const ThemeContext = createContext<ThemeContextValue | null>(null);

function getSystemTheme(): ResolvedTheme {
  return window.matchMedia("(prefers-color-scheme: dark)").matches
    ? "dark"
    : "light";
}

function loadStoredMode(): ColorMode {
  const stored = localStorage.getItem(STORAGE_KEYS.theme);
  if (stored && isValidColorMode(stored)) return stored;
  return "system";
}

function loadStoredCustomColors(): ColorTokens {
  const stored = localStorage.getItem(STORAGE_KEYS.customColors);
  if (stored) {
    try {
      const parsed = JSON.parse(stored);
      if (parsed && typeof parsed === "object") {
        // Merge against defaults so partial saves don't produce undefined tokens
        const merged = { ...DEFAULT_CUSTOM_TOKENS };
        for (const name of TOKEN_NAMES) {
          if (typeof parsed[name] === "string") merged[name] = parsed[name];
        }
        return merged;
      }
    } catch {
      localStorage.removeItem(STORAGE_KEYS.customColors);
    }
  }
  return DEFAULT_CUSTOM_TOKENS;
}

function loadStoredFontSize(): number {
  const stored = localStorage.getItem(STORAGE_KEYS.fontSize);
  if (stored) {
    const parsed = parseInt(stored, 10);
    if (!isNaN(parsed) && parsed >= MIN_FONT_SIZE && parsed <= MAX_FONT_SIZE)
      return parsed;
  }
  return DEFAULT_FONT_SIZE;
}

function loadStoredFontFamily(): FontFamily {
  const stored = localStorage.getItem(STORAGE_KEYS.fontFamily);
  if (stored) return stored;
  return "serif";
}

function loadStoredTypography(): TypographySettings {
  const stored = localStorage.getItem(STORAGE_KEYS.typography);
  if (stored) {
    try {
      const parsed = JSON.parse(stored);
      if (parsed && typeof parsed === "object") {
        const clampNum = (v: unknown, min: number, max: number, fallback: number) =>
          typeof v === "number" && isFinite(v) ? Math.min(max, Math.max(min, v)) : fallback;
        return {
          lineHeight: clampNum(parsed.lineHeight, 1.2, 2.4, DEFAULT_TYPOGRAPHY.lineHeight),
          pageMargins: clampNum(parsed.pageMargins, 0, 80, DEFAULT_TYPOGRAPHY.pageMargins),
          paragraphSpacing: clampNum(parsed.paragraphSpacing, 0, 2, DEFAULT_TYPOGRAPHY.paragraphSpacing),
          textAlign: parsed.textAlign === "left" || parsed.textAlign === "justify" ? parsed.textAlign : DEFAULT_TYPOGRAPHY.textAlign,
          hyphenation: typeof parsed.hyphenation === "boolean" ? parsed.hyphenation : DEFAULT_TYPOGRAPHY.hyphenation,
        };
      }
    } catch {
      localStorage.removeItem(STORAGE_KEYS.typography);
    }
  }
  return DEFAULT_TYPOGRAPHY;
}

function loadStoredScrollMode(): ScrollMode {
  const stored = localStorage.getItem(STORAGE_KEYS.scrollMode);
  if (stored === "paginated" || stored === "continuous") return stored;
  return "paginated";
}

export function ThemeProvider({ children }: { children: ReactNode }) {
  const [mode, setModeState] = useState<ColorMode>(loadStoredMode);
  const [systemTheme, setSystemTheme] = useState<ResolvedTheme>(getSystemTheme);
  const [customColors, setCustomColorsState] = useState<ColorTokens>(loadStoredCustomColors);
  const [fontSize, setFontSizeState] = useState(loadStoredFontSize);
  const [fontFamily, setFontFamilyState] = useState<FontFamily>(loadStoredFontFamily);
  const [scrollMode, setScrollModeState] = useState<ScrollMode>(loadStoredScrollMode);
  const [typography, setTypographyState] = useState<TypographySettings>(loadStoredTypography);
  const [customCss, setCustomCssState] = useState(() => sanitizeCss(localStorage.getItem(STORAGE_KEYS.customCss) ?? ""));
  const [dualPage, setDualPageState] = useState(() => localStorage.getItem(STORAGE_KEYS.dualPage) === "true");
  const [mangaMode, setMangaModeState] = useState(() => localStorage.getItem(STORAGE_KEYS.mangaMode) === "true");
  const [pageAnimation, setPageAnimationState] = useState(() => {
    const stored = localStorage.getItem(STORAGE_KEYS.pageAnimation);
    return stored === null ? true : stored === "true";
  });

  // For dark: variant purposes, sepia and custom resolve to "light"
  const resolved: ResolvedTheme =
    mode === "dark" ? "dark"
    : mode === "system" ? systemTheme
    : "light";

  // Listen for system theme changes
  useEffect(() => {
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const handler = (e: MediaQueryListEvent) =>
      setSystemTheme(e.matches ? "dark" : "light");
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  }, []);

  // Apply theme to <html>: dark class + inline CSS custom properties
  useEffect(() => {
    const root = document.documentElement;
    const effectivelyDark =
      mode === "dark" || (mode === "system" && systemTheme === "dark");

    if (effectivelyDark) {
      root.classList.add("dark");
      clearRootTokens();
    } else {
      root.classList.remove("dark");
      if (mode === "sepia") {
        applyTokensToRoot(SEPIA_TOKENS);
      } else if (mode === "custom") {
        applyTokensToRoot(customColors);
      } else {
        // light or system-light: clear overrides, let :root CSS handle it
        clearRootTokens();
      }
    }
  }, [mode, systemTheme, customColors]);

  const setMode = useCallback((m: ColorMode) => {
    setModeState(m);
    localStorage.setItem(STORAGE_KEYS.theme, m);
  }, []);

  const setCustomColors = useCallback((colors: ColorTokens) => {
    setCustomColorsState(colors);
    localStorage.setItem(STORAGE_KEYS.customColors, JSON.stringify(colors));
  }, []);

  const setFontSize = useCallback((size: number) => {
    const clamped = Math.min(MAX_FONT_SIZE, Math.max(MIN_FONT_SIZE, size));
    setFontSizeState(clamped);
    localStorage.setItem(STORAGE_KEYS.fontSize, String(clamped));
  }, []);

  const setFontFamily = useCallback((family: FontFamily) => {
    import("../lib/fontLoader").then((m) => m.loadFont(family));
    setFontFamilyState(family);
    localStorage.setItem(STORAGE_KEYS.fontFamily, family);
  }, []);

  const setScrollMode = useCallback((sm: ScrollMode) => {
    setScrollModeState(sm);
    localStorage.setItem(STORAGE_KEYS.scrollMode, sm);
  }, []);

  const setTypography = useCallback((t: TypographySettings) => {
    setTypographyState(t);
    localStorage.setItem(STORAGE_KEYS.typography, JSON.stringify(t));
  }, []);

  const setDualPage = useCallback((enabled: boolean) => {
    setDualPageState(enabled);
    localStorage.setItem(STORAGE_KEYS.dualPage, String(enabled));
  }, []);

  const setMangaMode = useCallback((enabled: boolean) => {
    setMangaModeState(enabled);
    localStorage.setItem(STORAGE_KEYS.mangaMode, String(enabled));
  }, []);

  const setPageAnimation = useCallback((enabled: boolean) => {
    setPageAnimationState(enabled);
    localStorage.setItem(STORAGE_KEYS.pageAnimation, String(enabled));
  }, []);

  const MAX_CUSTOM_CSS_LENGTH = 10000;
  const cssPersistTimer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const setCustomCss = useCallback((css: string) => {
    const trimmed = css.length > MAX_CUSTOM_CSS_LENGTH ? css.slice(0, MAX_CUSTOM_CSS_LENGTH) : css;
    const safe = sanitizeCss(trimmed);
    setCustomCssState(safe);
    clearTimeout(cssPersistTimer.current);
    cssPersistTimer.current = setTimeout(() => {
      localStorage.setItem(STORAGE_KEYS.customCss, safe);
    }, 500);
  }, []);

  const loadTheme = useCallback((theme: { colors: ColorTokens; fontFamily: string; fontSize: number; typography: TypographySettings }) => {
    setCustomColors(theme.colors);
    setFontFamily(theme.fontFamily);
    setFontSize(theme.fontSize);
    setTypography(theme.typography);
    setMode("custom");
  }, [setCustomColors, setFontFamily, setFontSize, setTypography, setMode]);

  const value = useMemo<ThemeContextValue>(() => ({
    mode, resolved, setMode,
    customColors, setCustomColors,
    fontSize, setFontSize,
    fontFamily, setFontFamily,
    scrollMode, setScrollMode,
    typography, setTypography,
    customCss, setCustomCss,
    dualPage, setDualPage,
    mangaMode, setMangaMode,
    pageAnimation, setPageAnimation,
    loadTheme,
  }), [mode, resolved, setMode, customColors, setCustomColors, fontSize, setFontSize, fontFamily, setFontFamily, scrollMode, setScrollMode, typography, setTypography, customCss, setCustomCss, dualPage, setDualPage, mangaMode, setMangaMode, pageAnimation, setPageAnimation, loadTheme]);

  return (
    <ThemeContext.Provider value={value}>
      {children}
    </ThemeContext.Provider>
  );
}

export function useTheme() {
  const ctx = useContext(ThemeContext);
  if (!ctx) throw new Error("useTheme must be used within ThemeProvider");
  return ctx;
}
