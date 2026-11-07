// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import "@testing-library/jest-dom/vitest";
import { MemoryRouter } from "react-router-dom";
import { enT } from "../test/enI18nMock";

const kids = { enabled: true, codeSet: true, refresh: vi.fn() };
vi.mock("../hooks/useKidsMode", () => ({ useKidsMode: () => kids }));

let gridItems: unknown[] = [];
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn((cmd: string) => {
    if (cmd === "get_library_grid") return Promise.resolve(gridItems);
    if (cmd === "get_supported_formats") return Promise.resolve(["epub"]);
    if (cmd === "get_setting_value") return Promise.resolve(null);
    if (cmd === "get_discover_books") return Promise.resolve([]);
    return Promise.resolve([]);
  }),
  convertFileSrc: (s: string) => s,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(() => Promise.resolve(() => {})) }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));

let dragHandler: ((e: { payload: { type: string; paths?: string[] } }) => Promise<void>) | null = null;
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({
    onDragDropEvent: (cb: typeof dragHandler) => {
      dragHandler = cb;
      return Promise.resolve(() => {});
    },
  }),
}));

const startFiles = vi.fn(() => Promise.resolve());
vi.mock("../context/ImportContext", () => ({
  useImport: () => ({
    running: false, progress: null, lastCompletedAt: null,
    startFiles, startFolder: vi.fn(),
  }),
}));
vi.mock("../components/OnboardingWizard", () => ({ default: () => <div data-testid="onboarding" /> }));
vi.mock("../components/VirtualizedBookGrid", () => ({
  default: ({ items, renderItem, header }: { items: unknown[]; renderItem: (i: number, x: unknown) => React.ReactNode; header?: React.ReactNode }) => (
    <div>{header}{items.map((it, i) => renderItem(i, it))}</div>
  ),
}));
vi.mock("../components/Toast", () => ({ useToast: () => ({ addToast: vi.fn() }) }));
vi.mock("../hooks/useWhatsNew", () => ({
  useWhatsNew: () => ({
    showBanner: false, showModal: false, openModal: vi.fn(), closeModal: vi.fn(),
    dismissBanner: vi.fn(), currentRelease: null, flagLoaded: true,
  }),
}));
vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: enT, i18n: { language: "en" } }),
  initReactI18next: { type: "3rdParty", init: () => {} },
}));

import { render, screen, cleanup, waitFor } from "@testing-library/react";
import Library from "./Library";

const book = {
  id: "b1", title: "Tintin", author: "Herge", cover_path: null, total_chapters: 1,
  added_at: 1, format: "epub", series: null, volume: null, rating: null,
  language: "en", publish_year: null, is_imported: true, want_to_read: false,
};

function mount() {
  return render(<MemoryRouter><Library /></MemoryRouter>);
}

async function drop() {
  await waitFor(() => expect(dragHandler).not.toBeNull());
  await dragHandler!({ payload: { type: "drop", paths: ["/tmp/a.epub"] } });
  // let getSupportedFormats resolve
  await new Promise((r) => setTimeout(r, 0));
}

beforeEach(() => {
  kids.enabled = true;
  gridItems = [];
  dragHandler = null;
  startFiles.mockClear();
  localStorage.clear();
});
afterEach(() => cleanup());

describe("Library in Kids mode", () => {
  it("empty library shows no import actions and ignores a file drop", async () => {
    mount();
    await screen.findByText("Nothing here yet.");
    expect(screen.queryByText("Add books")).toBeNull();
    expect(screen.queryByText("Import folder")).toBeNull();
    expect(screen.queryByTestId("onboarding")).toBeNull();
    await drop();
    expect(startFiles).not.toHaveBeenCalled();
  });

  it("a book card has no delete or edit actions, and there is no bulk-select or import", async () => {
    gridItems = [book];
    mount();
    await screen.findByText("Tintin");
    expect(screen.queryByLabelText("Remove Tintin")).toBeNull();
    expect(screen.queryByLabelText("Edit Tintin")).toBeNull();
    expect(screen.queryByTitle("Select books")).toBeNull();
    expect(screen.queryByText("Add books")).toBeNull();
  });

  it("control: with Kids mode off the same harness does import on drop", async () => {
    kids.enabled = false;
    gridItems = [book];
    mount();
    await screen.findByText("Tintin");
    expect(screen.getByTitle("Select books")).toBeInTheDocument();
    expect(screen.getByTestId("onboarding")).toBeInTheDocument();
    await drop();
    await waitFor(() => expect(startFiles).toHaveBeenCalledWith(["/tmp/a.epub"]));
  });
});
