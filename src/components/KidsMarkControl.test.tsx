// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import "@testing-library/jest-dom/vitest";
import { enT } from "../test/enI18nMock";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a) }));
vi.mock("react-i18next", () => ({ useTranslation: () => ({ t: enT }) }));

import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import KidsMarkControl from "./KidsMarkControl";

beforeEach(() => { invoke.mockReset(); });
afterEach(() => cleanup());

describe("KidsMarkControl", () => {
  it("sets the override and then shows the resulting visibility", async () => {
    let mark = { override: "follow", series_allowed: true, visible: true };
    invoke.mockImplementation((cmd: string, args?: { mode?: string }) => {
      if (cmd === "get_kids_book_mark") return Promise.resolve(mark);
      if (cmd === "set_kids_book_override") {
        mark = { override: args!.mode!, series_allowed: true, visible: args!.mode !== "blocked" };
        return Promise.resolve(undefined);
      }
      return Promise.resolve(undefined);
    });
    render(<KidsMarkControl bookId="b1" />);
    expect(await screen.findByText("Visible in Kids mode")).toBeInTheDocument();

    fireEvent.change(screen.getByRole("combobox"), { target: { value: "blocked" } });

    expect(await screen.findByText("Hidden in Kids mode")).toBeInTheDocument();
    expect(invoke).toHaveBeenCalledWith("set_kids_book_override", { bookId: "b1", mode: "blocked" });
  });

  it("offers the three options", async () => {
    invoke.mockResolvedValue({ override: "follow", series_allowed: false, visible: false });
    render(<KidsMarkControl bookId="b1" />);
    expect(await screen.findByText("Hidden in Kids mode")).toBeInTheDocument();
    const opts = screen.getAllByRole("option").map((o) => o.textContent);
    expect(opts).toEqual(["Follow series", "OK for kids", "Not for kids"]);
  });
});
