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

// Per-command IPC responses; each test seeds what it needs. Unlisted commands
// resolve to []. download_opds_book stays pending so the "Adding…" state holds.
const invokeResponses: Record<string, unknown> = {};
const invokeMock = vi.fn((cmd: string) => {
  if (cmd === "download_opds_book") return new Promise(() => {}); // never resolves
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

import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import Library from "../Library";

// The Discover shelf only renders once the library has at least one book
// (an empty library shows the import call-to-action instead).
const libraryBook = {
  id: "owned-1",
  title: "Owned Book",
  author: "Some Author",
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
  want_to_read: false,
};

function discoverEntry(id: string, title: string, author: string) {
  return {
    id,
    title,
    author,
    summary: "",
    coverUrl: null,
    links: [
      { href: `https://example.com/${id}.epub`, mimeType: "application/epub+zip", rel: "http://opds-spec.org/acquisition" },
    ],
    navUrl: null,
  };
}

afterEach(() => {
  cleanup();
  localStorage.clear();
});

beforeEach(() => {
  vi.clearAllMocks();
  for (const k of Object.keys(invokeResponses)) delete invokeResponses[k];
  // Discover section is opt-in via this flag; enable it for every test.
  localStorage.setItem("carrel-show-discover", "true");
});

describe("Library — Discover section", () => {
  it("shows the empty-state message when no picks remain", async () => {
    invokeResponses["get_library_grid"] = [libraryBook];
    invokeResponses["get_discover_books"] = [];
    render(<Library />);
    // Discover starts in a loading skeleton; the empty-state copy replaces it
    // once get_discover_books resolves with no picks. Query the live document
    // (not a node captured mid-load) so the poll sees the post-resolve render.
    expect(await screen.findByText(en.library.discoverEmpty)).toBeInTheDocument();
  });

  it("disables the add button and shows 'Adding…' while a download is in flight", async () => {
    invokeResponses["get_library_grid"] = [libraryBook];
    invokeResponses["get_discover_books"] = [discoverEntry("e1", "Basil", "Wilkie Collins")];
    render(<Library />);
    const addBtn = await screen.findByText(en.library.addToLibrary);
    fireEvent.click(addBtn);
    const adding = await screen.findByText(en.library.adding);
    expect(adding.closest("button")).toBeDisabled();
  });
});
