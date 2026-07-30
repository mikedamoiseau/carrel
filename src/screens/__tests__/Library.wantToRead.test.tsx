// @vitest-environment jsdom
import { describe, it, expect, vi, afterEach, beforeEach } from "vitest";
import "@testing-library/jest-dom/vitest";
import en from "../../locales/en.json";

// jsdom lacks ResizeObserver, which VirtualizedBookGrid instantiates on mount.
class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}
globalThis.ResizeObserver = ResizeObserverStub as unknown as typeof ResizeObserver;

// Resolve real en.json keys and interpolate {{...}} placeholders.
vi.mock("react-i18next", () => ({
  initReactI18next: { type: "3rdParty", init: () => {} },
  useTranslation: () => ({
    t: (key: string, opts?: Record<string, unknown>) => {
      const parts = key.split(".");
      let val: unknown = en;
      for (const p of parts) val = (val as Record<string, unknown>)?.[p];
      let str = typeof val === "string" ? val : key;
      if (opts)
        for (const [k, v] of Object.entries(opts))
          str = str.replace(new RegExp(`\\{\\{${k}\\}\\}`, "g"), String(v));
      return str;
    },
  }),
}));

const invokeResponses: Record<string, unknown> = {};
const invokeMock = vi.fn((cmd: string) => {
  if (cmd in invokeResponses) return Promise.resolve(invokeResponses[cmd]);
  return Promise.resolve([]);
});
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string) => invokeMock(cmd),
  convertFileSrc: (p: string) => p,
}));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({ onDragDropEvent: () => Promise.resolve(() => {}) }),
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: () => Promise.resolve(() => {}) }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));
vi.mock("react-router-dom", () => ({ useNavigate: () => vi.fn() }));
vi.mock("../../context/ImportContext", () => ({
  useImport: () => ({
    running: false,
    progress: null,
    lastCompletedAt: null,
    startFolder: vi.fn(),
    startFiles: vi.fn(),
    cancel: vi.fn(),
    retry: vi.fn(),
    canRetry: false,
    dismiss: vi.fn(),
  }),
}));
vi.mock("../../context/OnboardingContext", () => ({
  useOnboardingContext: () => ({
    isActive: false,
    currentStep: 0,
    advance: vi.fn(),
    skip: vi.fn(),
    complete: vi.fn(),
  }),
}));
vi.mock("../../hooks/useWhatsNew", () => ({
  useWhatsNew: () => ({
    showBanner: false,
    showModal: false,
    openModal: vi.fn(),
    closeModal: vi.fn(),
    dismissBanner: vi.fn(),
    currentRelease: null,
    flagLoaded: true,
  }),
}));
vi.mock("../../components/Toast", () => ({ useToast: () => ({ addToast: vi.fn() }) }));
vi.mock("../../lib/supportedFormats", () => ({
  useSupportedFormats: () => new Set(["epub"]),
  FALLBACK_FORMATS: new Set(["epub"]),
  getSupportedFormats: () => Promise.resolve(new Set(["epub"])),
}));

import { render, screen, fireEvent, cleanup, within } from "@testing-library/react";
import Library from "../Library";

function gridBook(id: string, title: string, wantToRead: boolean) {
  return {
    id,
    title,
    author: "Author",
    cover_path: null,
    total_chapters: 10,
    added_at: 0,
    format: "epub" as const,
    series: null,
    volume: null,
    rating: null,
    language: null,
    publish_year: null,
    is_imported: true,
    want_to_read: wantToRead,
  };
}

afterEach(() => {
  cleanup();
  localStorage.clear();
});

beforeEach(() => {
  vi.clearAllMocks();
  for (const k of Object.keys(invokeResponses)) delete invokeResponses[k];
});

describe("Library — want-to-read filter", () => {
  // Series sort renders cards in a plain CSS grid (not the Virtuoso window),
  // so the cards are reliably in the DOM under jsdom.
  beforeEach(() => localStorage.setItem("carrel-library-sort-by", "series"));

  it("hides unflagged books when the filter is active", async () => {
    invokeResponses["get_library_grid"] = [
      gridBook("flagged", "Flagged Book", true),
      gridBook("plain", "Plain Book", false),
    ];
    render(<Library />);
    // Both cards visible before filtering.
    expect(await screen.findByText("Flagged Book")).toBeInTheDocument();
    expect(screen.getByText("Plain Book")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: en.library.wantToRead }));

    expect(screen.getByText("Flagged Book")).toBeInTheDocument();
    expect(screen.queryByText("Plain Book")).not.toBeInTheDocument();
  });

  it("shows the no-match empty state when nothing is flagged", async () => {
    invokeResponses["get_library_grid"] = [
      gridBook("a", "Book A", false),
      gridBook("b", "Book B", false),
    ];
    render(<Library />);
    expect(await screen.findByText("Book A")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: en.library.wantToRead }));

    expect(await screen.findByText(en.library.noMatchFilters)).toBeInTheDocument();
    expect(screen.queryByText("Book A")).not.toBeInTheDocument();
  });
});

describe("Library — want-to-read home shelf", () => {
  beforeEach(() => {
    localStorage.setItem("carrel-show-want-to-read", "true");
    // Series sort renders cards in a plain grid, a reliable "loaded" signal.
    localStorage.setItem("carrel-library-sort-by", "series");
  });

  it("renders flagged books in the shelf when enabled and unfiltered", async () => {
    invokeResponses["get_library_grid"] = [
      gridBook("flagged", "Flagged Book", true),
      gridBook("plain", "Plain Book", false),
    ];
    render(<Library />);
    const shelf = await screen.findByTestId("want-to-read-section");
    expect(within(shelf).getByText("Flagged Book")).toBeInTheDocument();
    expect(within(shelf).queryByText("Plain Book")).not.toBeInTheDocument();
  });

  it("hides the shelf when no books are flagged", async () => {
    invokeResponses["get_library_grid"] = [gridBook("plain", "Plain Book", false)];
    render(<Library />);
    // Wait for the library to load (the card renders), then assert the shelf is absent.
    await screen.findByText("Plain Book");
    expect(screen.queryByTestId("want-to-read-section")).not.toBeInTheDocument();
  });

  it("hides the shelf when the visibility flag is off", async () => {
    localStorage.setItem("carrel-show-want-to-read", "false");
    invokeResponses["get_library_grid"] = [gridBook("flagged", "Flagged Book", true)];
    render(<Library />);
    await screen.findByText("Flagged Book");
    expect(screen.queryByTestId("want-to-read-section")).not.toBeInTheDocument();
  });
});
