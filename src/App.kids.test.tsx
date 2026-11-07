// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import "@testing-library/jest-dom/vitest";
import { enT } from "./test/enI18nMock";

const kids = { enabled: true, codeSet: true, refresh: vi.fn() };
vi.mock("./hooks/useKidsMode", () => ({ useKidsMode: () => kids }));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn((cmd: string) => {
    if (cmd === "get_profiles") return Promise.resolve([{ name: "default", is_active: true }]);
    if (cmd === "profile_lock_status") return Promise.resolve({ locked: false, unlockedThisSession: false });
    if (cmd === "get_library" || cmd === "list_vocabulary") return Promise.resolve([]);
    return Promise.resolve(null);
  }),
  convertFileSrc: (s: string) => s,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(() => Promise.resolve(() => {})) }));
vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: enT, i18n: { language: "en", changeLanguage: vi.fn() } }),
  initReactI18next: { type: "3rdParty", init: () => {} },
}));
// The real Library/Settings/Switcher pull in the whole app; the contract here
// is only which controls the shell mounts in Kids mode.
vi.mock("./screens/Library", () => ({ default: () => <div data-testid="library" /> }));
vi.mock("./components/SettingsPanel", () => ({ default: () => <div data-testid="settings-panel" /> }));
vi.mock("./components/ProfileSwitcher", () => ({ default: () => <div data-testid="profile-switcher" /> }));
vi.mock("./components/PrivateModeToggle", () => ({ default: () => <div data-testid="private-toggle" /> }));
vi.mock("./components/PrivateModeBar", () => ({ default: () => null }));
vi.mock("./components/ImportStatusBar", () => ({ default: () => null }));
vi.mock("./components/AnalyticsConsentDialog", () => ({ default: () => null }));
vi.mock("./components/UpdateCheckHost", () => ({ default: () => null }));
vi.mock("./components/LanguageSwitcher", () => ({ default: () => null }));

import { render, screen, cleanup, waitFor } from "@testing-library/react";
import App from "./App";

beforeEach(() => {
  kids.enabled = true;
  window.matchMedia = ((q: string) => ({
    matches: false, media: q, onchange: null,
    addEventListener: () => {}, removeEventListener: () => {},
    addListener: () => {}, removeListener: () => {}, dispatchEvent: () => false,
  })) as unknown as typeof window.matchMedia;
});
afterEach(() => cleanup());

describe("App shell in Kids mode", () => {
  it("shows the banner and none of the admin controls", async () => {
    render(<App />);
    await screen.findByTestId("library");
    expect(screen.getByText("Kids mode")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Leave Kids mode" })).toBeInTheDocument();
    expect(screen.queryByLabelText("Open settings")).toBeNull();
    expect(screen.queryByLabelText("Reading stats")).toBeNull();
    expect(screen.queryByLabelText("Browse catalogs")).toBeNull();
    expect(screen.queryByTestId("profile-switcher")).toBeNull();
    expect(screen.queryByTestId("private-toggle")).toBeNull();
  });

  it("keeps the controls when Kids mode is off", async () => {
    kids.enabled = false;
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("Open settings")).toBeInTheDocument());
    expect(screen.getByLabelText("Reading stats")).toBeInTheDocument();
    expect(screen.getByLabelText("Browse catalogs")).toBeInTheDocument();
    expect(screen.getByTestId("profile-switcher")).toBeInTheDocument();
    expect(screen.queryByText("Kids mode")).toBeNull();
  });
});
