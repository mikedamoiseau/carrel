// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import "@testing-library/jest-dom/vitest";
import { MemoryRouter } from "react-router-dom";
import { enT } from "../test/enI18nMock";

const kids = { enabled: true, codeSet: true, checked: true, failed: false, refresh: vi.fn() };
vi.mock("../hooks/useKidsMode", () => ({ useKidsMode: () => kids }));

let gridItems: unknown[] = [];
let collectionItems: unknown[] = [];
let fullBook: unknown = null;
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn((cmd: string) => {
    if (cmd === "get_library_grid") return Promise.resolve(gridItems);
    if (cmd === "get_collections") return Promise.resolve(collectionItems);
    if (cmd === "get_book") return Promise.resolve(fullBook);
    if (cmd === "check_file_exists") return Promise.reject(new Error("Book file not found"));
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

import { render, screen, cleanup, waitFor, fireEvent } from "@testing-library/react";
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

const shelf = {
  id: "c1", name: "Shelf", type: "manual", icon: null, color: null,
  created_at: 1, updated_at: 1, rules: [],
};

beforeEach(() => {
  kids.enabled = true;
  kids.checked = true;
  gridItems = [];
  collectionItems = [];
  fullBook = null;
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

  it("has no metadata scan or highlight search", async () => {
    gridItems = [book];
    mount();
    await screen.findByText("Tintin");
    expect(screen.queryByLabelText("Scan library for metadata")).toBeNull();
    expect(screen.queryByLabelText("Search highlights")).toBeNull();
  });

  it("collections can't be edited or deleted", async () => {
    gridItems = [book];
    collectionItems = [shelf];
    mount();
    await screen.findByText("Tintin");
    fireEvent.click(screen.getByLabelText("Open collections"));
    await screen.findByText("Shelf");
    expect(screen.queryByLabelText("Edit Shelf")).toBeNull();
    expect(screen.queryByLabelText("Delete Shelf")).toBeNull();
  });

  it("a linked book whose file is missing can't be removed from the library", async () => {
    gridItems = [{ ...book, is_imported: false }];
    fullBook = { ...book, is_imported: false, file_path: "/Volumes/nas/a.epub" };
    mount();
    fireEvent.click(await screen.findByText("Tintin"));
    await screen.findByText("File not available. Reconnect the drive or remove this book.");
    expect(screen.queryByRole("button", { name: "Remove" })).toBeNull();
  });

  it("while Kids mode is still unknown, shows none of the admin controls", async () => {
    kids.enabled = false;
    kids.checked = false;
    gridItems = [book];
    mount();
    await screen.findByText("Tintin");
    expect(screen.queryByTitle("Select books")).toBeNull();
    expect(screen.queryByText("Add books")).toBeNull();
    expect(screen.queryByLabelText("Scan library for metadata")).toBeNull();
    expect(screen.queryByLabelText("Search highlights")).toBeNull();
    expect(screen.queryByLabelText("Remove Tintin")).toBeNull();
    expect(screen.queryByTestId("onboarding")).toBeNull();
  });

  it("control: with Kids mode off the admin controls are there", async () => {
    kids.enabled = false;
    gridItems = [{ ...book, is_imported: false }];
    collectionItems = [shelf];
    fullBook = { ...book, is_imported: false, file_path: "/Volumes/nas/a.epub" };
    mount();
    fireEvent.click(await screen.findByText("Tintin"));
    await screen.findByText("File not available. Reconnect the drive or remove this book.");
    expect(screen.getByRole("button", { name: "Remove" })).toBeInTheDocument();
    expect(screen.getByLabelText("Scan library for metadata")).toBeInTheDocument();
    expect(screen.getByLabelText("Search highlights")).toBeInTheDocument();
    fireEvent.click(screen.getByLabelText("Open collections"));
    await screen.findByText("Shelf");
    expect(screen.getByLabelText("Edit Shelf")).toBeInTheDocument();
    expect(screen.getByLabelText("Delete Shelf")).toBeInTheDocument();
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
