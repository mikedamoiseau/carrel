// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { renderHook, act, waitFor } from "@testing-library/react";

const invoke = vi.fn();
let emitted: ((event: { payload: boolean }) => void) | null = null;

vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a) }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn((_n: string, cb: (event: { payload: boolean }) => void) => {
    emitted = cb;
    return Promise.resolve(() => {});
  }),
}));

import { useKidsMode } from "../useKidsMode";

beforeEach(() => {
  invoke.mockReset();
  emitted = null;
});

describe("useKidsMode", () => {
  it("reads its initial state from get_kids_mode", async () => {
    invoke.mockResolvedValue({ enabled: true, codeSet: true });
    const { result } = renderHook(() => useKidsMode());
    await waitFor(() => expect(result.current.enabled).toBe(true));
    expect(invoke).toHaveBeenCalledWith("get_kids_mode");
    expect(result.current.codeSet).toBe(true);
  });

  it("defaults to off when the read fails", async () => {
    invoke.mockRejectedValue(new Error("no backend"));
    const { result } = renderHook(() => useKidsMode());
    await waitFor(() => expect(invoke).toHaveBeenCalled());
    expect(result.current.enabled).toBe(false);
    expect(result.current.codeSet).toBe(false);
  });

  it("flips on the kids-mode-changed event", async () => {
    invoke.mockResolvedValue({ enabled: false, codeSet: true });
    const { result } = renderHook(() => useKidsMode());
    await waitFor(() => expect(emitted).not.toBeNull());
    await waitFor(() => expect(result.current.codeSet).toBe(true));
    act(() => emitted!({ payload: true }));
    expect(result.current.enabled).toBe(true);
    act(() => emitted!({ payload: false }));
    expect(result.current.enabled).toBe(false);
  });

  it("refresh() re-reads the backend", async () => {
    invoke.mockResolvedValue({ enabled: false, codeSet: false });
    const { result } = renderHook(() => useKidsMode());
    await waitFor(() => expect(invoke).toHaveBeenCalledTimes(1));
    invoke.mockResolvedValue({ enabled: false, codeSet: true });
    await act(async () => { await result.current.refresh(); });
    expect(result.current.codeSet).toBe(true);
  });
});
