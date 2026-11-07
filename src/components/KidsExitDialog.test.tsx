// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import "@testing-library/jest-dom/vitest";
import { enT } from "../test/enI18nMock";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a) }));
vi.mock("react-i18next", () => ({ useTranslation: () => ({ t: enT }) }));
vi.mock("../lib/useFocusTrap", () => ({ useFocusTrap: () => ({ current: null }) }));

import { render, screen, cleanup, fireEvent, waitFor, act } from "@testing-library/react";
import KidsExitDialog from "./KidsExitDialog";

beforeEach(() => { invoke.mockReset(); });
afterEach(() => cleanup());

async function submit(code: string) {
  fireEvent.change(screen.getByLabelText("Parents' code"), { target: { value: code } });
  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: "Leave" }));
  });
}

describe("KidsExitDialog", () => {
  it("calls exit_kids_mode with the typed code and reports success", async () => {
    invoke.mockResolvedValue({ enabled: false, codeSet: true });
    const onExited = vi.fn();
    render(<KidsExitDialog onClose={() => {}} onExited={onExited} />);
    await submit("1357");
    await waitFor(() => expect(onExited).toHaveBeenCalled());
    expect(invoke).toHaveBeenCalledWith("exit_kids_mode", { code: "1357" });
  });

  it("shows 'Incorrect code' when the backend rejects", async () => {
    invoke.mockImplementation(async () => { throw "Incorrect code"; });
    const onExited = vi.fn();
    render(<KidsExitDialog onClose={() => {}} onExited={onExited} />);
    await submit("0000");
    expect(await screen.findByText("Incorrect code")).toBeInTheDocument();
    expect(onExited).not.toHaveBeenCalled();
  });

  it("shows the backend message verbatim even for a structured error", async () => {
    invoke.mockImplementation(() => Promise.reject({ kind: "InvalidInput", message: "Too many attempts. Try again later." }));
    render(<KidsExitDialog onClose={() => {}} onExited={() => {}} />);
    await submit("0000");
    expect(await screen.findByText("Too many attempts. Try again later.")).toBeInTheDocument();
  });
});
