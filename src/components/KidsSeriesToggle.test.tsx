// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import "@testing-library/jest-dom/vitest";
import { enT } from "../test/enI18nMock";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a) }));
vi.mock("react-i18next", () => ({ useTranslation: () => ({ t: enT }) }));

import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import KidsSeriesToggle from "./KidsSeriesToggle";

beforeEach(() => { invoke.mockReset(); });
afterEach(() => cleanup());

describe("KidsSeriesToggle", () => {
  it("marks an unmarked series, then offers to unmark it", async () => {
    let allowed: string[] = [];
    invoke.mockImplementation((cmd: string, args?: { series: string; allowed: boolean }) => {
      if (cmd === "get_kids_series") return Promise.resolve(allowed);
      if (cmd === "set_kids_series") { allowed = args!.allowed ? [args!.series] : []; }
      return Promise.resolve(undefined);
    });
    render(<KidsSeriesToggle series="Asterix" />);
    fireEvent.click(await screen.findByRole("button", { name: "Mark series OK for kids" }));
    expect(await screen.findByRole("button", { name: "Unmark series for kids" })).toBeInTheDocument();
    expect(invoke).toHaveBeenCalledWith("set_kids_series", { series: "Asterix", allowed: true });
  });
});
