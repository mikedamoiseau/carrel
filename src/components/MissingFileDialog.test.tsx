// @vitest-environment jsdom
import { describe, it, expect, vi, afterEach } from "vitest";
import "@testing-library/jest-dom/vitest";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string) => {
      const map: Record<string, string> = {
        "common.cancel": "Cancel",
        "reader.missingFileTitle": "File Not Found",
        "reader.missingFileMessage": "This book’s file could not be found.",
        "reader.removeFromLibrary": "Remove from library",
      };
      return map[key] ?? key;
    },
  }),
}));

vi.mock("../lib/useFocusTrap", () => ({
  useFocusTrap: () => ({ current: null }),
}));

import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import MissingFileDialog from "./MissingFileDialog";

afterEach(() => cleanup());

describe("MissingFileDialog", () => {
  it("renders a single dialog with title, message and recovery actions", () => {
    render(<MissingFileDialog onCancel={() => {}} onRemove={() => {}} />);
    const dialogs = screen.getAllByRole("dialog");
    expect(dialogs).toHaveLength(1);
    expect(dialogs[0]).toHaveAttribute("aria-modal", "true");
    expect(screen.getByText("File Not Found")).toBeInTheDocument();
    expect(
      screen.getByText("This book’s file could not be found.")
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Cancel" })
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Remove from library" })
    ).toBeInTheDocument();
  });

  it("fires onCancel when the cancel button is clicked", () => {
    const onCancel = vi.fn();
    const onRemove = vi.fn();
    render(<MissingFileDialog onCancel={onCancel} onRemove={onRemove} />);
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onCancel).toHaveBeenCalledTimes(1);
    expect(onRemove).not.toHaveBeenCalled();
  });

  it("offers only Cancel when no onRemove is given (Kids mode)", () => {
    render(<MissingFileDialog onCancel={() => {}} />);
    expect(screen.getByRole("button", { name: "Cancel" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Remove from library" })).toBeNull();
  });

  it("fires onRemove when the remove button is clicked", () => {
    const onCancel = vi.fn();
    const onRemove = vi.fn();
    render(<MissingFileDialog onCancel={onCancel} onRemove={onRemove} />);
    fireEvent.click(
      screen.getByRole("button", { name: "Remove from library" })
    );
    expect(onRemove).toHaveBeenCalledTimes(1);
    expect(onCancel).not.toHaveBeenCalled();
  });
});
