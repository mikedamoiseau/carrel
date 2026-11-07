// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import "@testing-library/jest-dom/vitest";
import { MemoryRouter } from "react-router-dom";
import { enT } from "../test/enI18nMock";

const kids = { enabled: true, codeSet: true, checked: true, failed: false, refresh: vi.fn() };
vi.mock("../hooks/useKidsMode", () => ({ useKidsMode: () => kids }));

// get_book answers null for a book Kids mode hides; the TOC read fails the
// way a missing file does.
let book: unknown = null;
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn((cmd: string) => {
    if (cmd === "get_book") return Promise.resolve(book);
    if (cmd === "get_toc") return Promise.reject(new Error("Book file not found: /Volumes/nas/a.epub"));
    if (cmd === "get_private_mode") return Promise.resolve(false);
    // An array, as the backend answers: the reader keeps it in state.
    if (cmd === "get_chapter_word_counts") return Promise.resolve([]);
    return Promise.resolve(null);
  }),
  convertFileSrc: (s: string) => s,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(() => Promise.resolve(() => {})) }));
vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn() }));
vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: enT, i18n: { language: "en", changeLanguage: vi.fn() } }),
  initReactI18next: { type: "3rdParty", init: () => {} },
}));
vi.mock("./Toast", () => ({ useToast: () => ({ addToast: vi.fn() }) }));

import { render, screen, cleanup } from "@testing-library/react";
import { ThemeProvider } from "../context/ThemeContext";
import ReaderPane from "./ReaderPane";

const epub = {
  id: "b1", title: "Tintin", author: "Herge", file_path: "/Volumes/nas/a.epub", cover_path: null,
  total_chapters: 3, added_at: 1, format: "epub", file_hash: null, description: null, genres: null,
  rating: null, isbn: null, openlibrary_key: null, enrichment_status: null, series: null, volume: null,
  language: null, publisher: null, publish_year: null, is_imported: false, want_to_read: false,
};

function mount() {
  return render(
    <ThemeProvider>
      <MemoryRouter>
        <ReaderPane bookId="b1" />
      </MemoryRouter>
    </ThemeProvider>,
  );
}

beforeEach(() => {
  kids.enabled = true;
  book = null;
  window.matchMedia = ((q: string) => ({
    matches: false, media: q, onchange: null,
    addEventListener: () => {}, removeEventListener: () => {},
    addListener: () => {}, removeListener: () => {}, dispatchEvent: () => false,
  })) as unknown as typeof window.matchMedia;
});
afterEach(() => cleanup());

describe("ReaderPane and Kids mode", () => {
  it("a book get_book doesn't return shows Book not found", async () => {
    mount();
    expect(await screen.findByText("Book not found")).toBeInTheDocument();
  });

  it("a missing file can't be removed from the library in Kids mode", async () => {
    book = epub;
    mount();
    expect(await screen.findByRole("dialog", { name: "File Not Found" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Remove from library" })).toBeNull();
  });

  it("control: outside Kids mode the missing-file dialog offers removal", async () => {
    kids.enabled = false;
    book = epub;
    mount();
    expect(await screen.findByRole("button", { name: "Remove from library" })).toBeInTheDocument();
  });
});
