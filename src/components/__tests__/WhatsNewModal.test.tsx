// @vitest-environment jsdom
import { describe, it, expect, vi, afterEach } from "vitest";
import "@testing-library/jest-dom/vitest";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, opts?: Record<string, string>) => {
      const map: Record<string, string> = {
        "whatsNew.modalTitle": `What's New in Carrel ${opts?.version ?? ""}`,
        "whatsNew.modalFullChangelog": "See full changelog",
      };
      return map[key] ?? key;
    },
  }),
}));

vi.mock("@tauri-apps/plugin-opener", () => ({
  openUrl: vi.fn(),
}));

import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import WhatsNewModal from "../WhatsNewModal";

const release = {
  version: "2.0.3",
  date: "2026-05-18",
  categories: {
    Added: [
      { title: "OPDS feed primitives", description: "Public primitives for rendering OPDS Atom feeds." },
    ],
    Fixed: [
      { title: "Web server deadlock", description: "The auto-start path held the mutex." },
    ],
  },
};

afterEach(() => cleanup());

describe("WhatsNewModal", () => {
  it("renders title with version", () => {
    render(<WhatsNewModal release={release} onClose={() => {}} />);
    expect(screen.getByText("What's New in Carrel 2.0.3")).toBeInTheDocument();
  });

  it("renders category headings", () => {
    render(<WhatsNewModal release={release} onClose={() => {}} />);
    expect(screen.getByText("Added")).toBeInTheDocument();
    expect(screen.getByText("Fixed")).toBeInTheDocument();
  });

  it("renders entry titles", () => {
    render(<WhatsNewModal release={release} onClose={() => {}} />);
    expect(screen.getByText("OPDS feed primitives")).toBeInTheDocument();
    expect(screen.getByText("Web server deadlock")).toBeInTheDocument();
  });

  it("calls onClose when backdrop clicked", () => {
    const onClose = vi.fn();
    render(<WhatsNewModal release={release} onClose={onClose} />);
    fireEvent.click(screen.getByRole("dialog").parentElement!);
    expect(onClose).toHaveBeenCalled();
  });

  it("calls onClose on Escape key", () => {
    const onClose = vi.fn();
    render(<WhatsNewModal release={release} onClose={onClose} />);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalled();
  });

  it("renders full changelog link", () => {
    render(<WhatsNewModal release={release} onClose={() => {}} />);
    expect(screen.getByText("See full changelog", { exact: false })).toBeInTheDocument();
  });
});
