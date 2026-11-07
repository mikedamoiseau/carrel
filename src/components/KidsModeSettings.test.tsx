// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import "@testing-library/jest-dom/vitest";
import { enT } from "../test/enI18nMock";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(() => Promise.resolve(() => {})) }));
vi.mock("react-i18next", () => ({ useTranslation: () => ({ t: enT }) }));

import { render, screen, cleanup, fireEvent, waitFor } from "@testing-library/react";
import KidsModeSettings from "./KidsModeSettings";

beforeEach(() => { invoke.mockReset(); });
afterEach(() => cleanup());

describe("KidsModeSettings", () => {
  it("rejects a non-numeric code client-side without calling the backend", async () => {
    invoke.mockResolvedValue({ enabled: false, codeSet: false });
    render(<KidsModeSettings />);
    fireEvent.change(await screen.findByLabelText("Parents' code"), { target: { value: "12a" } });
    fireEvent.click(screen.getByRole("button", { name: "Set parents' code" }));
    expect(await screen.findByText("Parents' code must be 4 to 12 digits")).toBeInTheDocument();
    expect(invoke).not.toHaveBeenCalledWith("set_kids_code", expect.anything());
  });

  it("saves a valid code", async () => {
    invoke.mockResolvedValue({ enabled: false, codeSet: false });
    render(<KidsModeSettings />);
    fireEvent.change(await screen.findByLabelText("Parents' code"), { target: { value: "1357" } });
    fireEvent.click(screen.getByRole("button", { name: "Set parents' code" }));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("set_kids_code", { code: "1357" }));
  });

  it("offers 'Change parents' code' once a code is set, and turns Kids mode on", async () => {
    invoke.mockResolvedValue({ enabled: false, codeSet: true });
    render(<KidsModeSettings />);
    expect(await screen.findByRole("button", { name: "Change parents' code" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Turn on Kids mode" }));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("enter_kids_mode"));
  });

  it("shows 'Set a parents' code first' from the backend when turning on without a code", async () => {
    invoke.mockImplementation((cmd: string) =>
      cmd === "enter_kids_mode"
        ? Promise.reject("Set a parents' code first")
        : Promise.resolve({ enabled: false, codeSet: false }),
    );
    render(<KidsModeSettings />);
    fireEvent.click(await screen.findByRole("button", { name: "Turn on Kids mode" }));
    expect(await screen.findByText("Set a parents' code first")).toBeInTheDocument();
  });
});
