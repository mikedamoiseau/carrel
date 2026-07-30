// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import "@testing-library/jest-dom/vitest";
import { render, screen, cleanup, fireEvent, act } from "@testing-library/react";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("@tauri-apps/api/core", () => {
  // List-returning commands must yield []; object/scalar commands yield null so
  // the mount effects hit their `if (x)` guards instead of touching undefined.
  const arrayCmds = new Set([
    "get_backup_providers",
    "get_enrichment_providers",
    "get_profiles",
    "get_custom_fonts",
  ]);
  const responses: Record<string, unknown> = {
    // Dispatched into the dictionary reducer, which reads status.state.
    get_dictionary_status: { state: "missing" },
  };
  return {
    invoke: vi.fn((cmd: string) =>
      Promise.resolve(cmd in responses ? responses[cmd] : arrayCmds.has(cmd) ? [] : null),
    ),
  };
});
vi.mock("@tauri-apps/api/app", () => ({ getVersion: () => Promise.resolve("2.8.0") }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: () => Promise.resolve(() => {}),
  emit: () => Promise.resolve(),
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));
vi.mock("virtual:release-notes", () => ({ releaseNotes: [], appVersion: "2.8.0" }));
vi.mock("../../hooks/useUpdateCheck", () => ({ startupCheckEnabled: () => false }));
vi.mock("../Toast", () => ({ useToast: () => ({ addToast: vi.fn() }) }));
vi.mock("../../context/OnboardingContext", () => ({
  useOnboardingContext: () => ({ restart: vi.fn() }),
}));
// Child subtrees are irrelevant to the toggle and pull in their own deps.
vi.mock("../SavedThemesList", () => ({ default: () => null }));
vi.mock("../PluginsPanel", () => ({ default: () => null }));
vi.mock("../ActivityLog", () => ({ default: () => null }));
vi.mock("../../context/ThemeContext", () => ({
  MIN_FONT_SIZE: 14,
  MAX_FONT_SIZE: 24,
  useTheme: () => ({
    mode: "sepia",
    setMode: vi.fn(),
    customColors: {},
    setCustomColors: vi.fn(),
    fontSize: 18,
    setFontSize: vi.fn(),
    fontFamily: "serif",
    setFontFamily: vi.fn(),
    scrollMode: "paginated",
    setScrollMode: vi.fn(),
    typography: { lineHeight: 1.8, pageMargins: 32, textAlign: "justify", paragraphSpacing: 1.1, hyphenation: true },
    setTypography: vi.fn(),
    customCss: "",
    setCustomCss: vi.fn(),
    dualPage: false,
    setDualPage: vi.fn(),
    mangaMode: false,
    setMangaMode: vi.fn(),
    pageAnimation: true,
    setPageAnimation: vi.fn(),
    loadTheme: vi.fn(),
  }),
}));

import SettingsPanel from "../SettingsPanel";

afterEach(() => {
  cleanup();
  localStorage.clear();
});
beforeEach(() => localStorage.clear());

describe("SettingsPanel — Want to Read visibility toggle", () => {
  it("persists the flag and dispatches the sync event when toggled on", async () => {
    render(<SettingsPanel open onClose={vi.fn()} />);
    const toggle = await screen.findByTestId("show-want-to-read-toggle");
    expect(toggle).toHaveAttribute("aria-checked", "false");

    const events: Event[] = [];
    const listener = (e: Event) => events.push(e);
    window.addEventListener("carrel-show-want-to-read-changed", listener);

    await act(async () => {
      fireEvent.click(toggle);
    });

    expect(localStorage.getItem("carrel-show-want-to-read")).toBe("true");
    expect(events).toHaveLength(1);
    expect(toggle).toHaveAttribute("aria-checked", "true");

    window.removeEventListener("carrel-show-want-to-read-changed", listener);
  });
});
