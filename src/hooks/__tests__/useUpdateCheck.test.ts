// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { renderHook, act, cleanup } from "@testing-library/react";
import { useUpdateCheck, startupCheckEnabled } from "../useUpdateCheck";

// Capture the event listener callback so tests can invoke it.
const evt = vi.hoisted(() => ({ cb: undefined as ((e: unknown) => void) | undefined, unlisten: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn((_e: string, cb: (e: unknown) => void) => {
    evt.cb = cb;
    return Promise.resolve(evt.unlisten);
  }),
}));

import { invoke } from "@tauri-apps/api/core";

const AVAILABLE = {
  update_available: true,
  current_version: "2.7.0",
  latest_version: "2.8.0",
  release_url: "https://github.com/mikedamoiseau/carrel/releases/tag/v2.8.0",
  changelog_url: "https://github.com/mikedamoiseau/carrel/releases",
  release_notes: "notes",
};

// Route commands to scripted values. A value may be a function (dynamic) or a
// value/Error (static). Errors reject.
// Wrap a value to make the command REJECT with it (raw string or Error), which
// is how Tauri surfaces `Err(String)` vs a thrown Error.
const reject = (value: unknown) => ({ __reject: value });

function route(map: Record<string, unknown>) {
  vi.mocked(invoke).mockImplementation(async (cmd: string) => {
    const v = typeof map[cmd] === "function" ? (map[cmd] as () => unknown)() : map[cmd];
    if (v && typeof v === "object" && "__reject" in v) throw (v as { __reject: unknown }).__reject;
    if (v instanceof Error) throw v;
    if (v instanceof Promise) return v as never; // allow deferred control
    return (v ?? null) as never;
  });
}

function deferred<T>() {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((res) => { resolve = res; });
  return { promise, resolve };
}

const countCalls = (cmd: string) =>
  vi.mocked(invoke).mock.calls.filter((c) => c[0] === cmd).length;

beforeEach(() => {
  vi.clearAllMocks();
  evt.cb = undefined;
});

afterEach(() => cleanup());

describe("useUpdateCheck", () => {
  it("auto: presents when token granted, toggle enabled (null default), update available", async () => {
    route({
      take_pending_manual_update_check: false,
      take_startup_update_check: true,
      get_setting_value: null,
      check_for_update: AVAILABLE,
    });
    const { result } = renderHook(() => useUpdateCheck(false));
    await vi.waitFor(() => expect(result.current.modal?.status).toBe("available"));
  });

  it("auto: silent when up to date", async () => {
    route({
      take_pending_manual_update_check: false,
      take_startup_update_check: true,
      get_setting_value: "true",
      check_for_update: { ...AVAILABLE, update_available: false },
    });
    const { result } = renderHook(() => useUpdateCheck(false));
    await new Promise((r) => setTimeout(r, 20));
    expect(result.current.modal).toBeNull();
  });

  it("auto: skipped and no fetch when toggle disabled", async () => {
    route({
      take_pending_manual_update_check: false,
      take_startup_update_check: true,
      get_setting_value: "false",
      check_for_update: AVAILABLE,
    });
    renderHook(() => useUpdateCheck(false));
    await new Promise((r) => setTimeout(r, 20));
    expect(invoke).not.toHaveBeenCalledWith("check_for_update");
  });

  it("auto: setting-read rejection → no automatic check", async () => {
    route({
      take_pending_manual_update_check: false,
      take_startup_update_check: true,
      get_setting_value: new Error("boom"),
      check_for_update: AVAILABLE,
    });
    renderHook(() => useUpdateCheck(false));
    await new Promise((r) => setTimeout(r, 20));
    expect(invoke).not.toHaveBeenCalledWith("check_for_update");
  });

  it("does nothing automatic while onboarding is active", async () => {
    route({
      take_pending_manual_update_check: true,
      take_startup_update_check: true,
      check_for_update: AVAILABLE,
    });
    renderHook(() => useUpdateCheck(true));
    await new Promise((r) => setTimeout(r, 20));
    expect(invoke).not.toHaveBeenCalledWith("take_pending_manual_update_check");
    expect(invoke).not.toHaveBeenCalledWith("take_startup_update_check");
  });

  it("runs the queued manual check when onboarding goes active → inactive", async () => {
    route({
      take_pending_manual_update_check: true,
      take_startup_update_check: false,
      get_setting_value: null,
      check_for_update: { ...AVAILABLE, update_available: false },
    });
    const { result, rerender } = renderHook(({ a }) => useUpdateCheck(a), {
      initialProps: { a: true },
    });
    await new Promise((r) => setTimeout(r, 10));
    expect(result.current.modal).toBeNull();
    rerender({ a: false });
    await vi.waitFor(() => expect(result.current.modal?.status).toBe("uptodate"));
  });

  it("manual: pending flag on mount presents result even when up to date", async () => {
    route({
      take_pending_manual_update_check: true,
      take_startup_update_check: false,
      get_setting_value: null,
      check_for_update: { ...AVAILABLE, update_available: false },
    });
    const { result } = renderHook(() => useUpdateCheck(false));
    await vi.waitFor(() => expect(result.current.modal?.status).toBe("uptodate"));
  });

  it("manual: event fires a check when the flag is set", async () => {
    let pending = false;
    route({
      take_pending_manual_update_check: () => pending,
      take_startup_update_check: false,
      get_setting_value: null,
      check_for_update: AVAILABLE,
    });
    const { result } = renderHook(() => useUpdateCheck(false));
    await new Promise((r) => setTimeout(r, 10)); // mount consume: pending=false, nothing
    pending = true; // tray set the flag
    await act(async () => {
      evt.cb?.({}); // simulate the tray event
      await new Promise((r) => setTimeout(r, 10));
    });
    await vi.waitFor(() => expect(result.current.modal?.status).toBe("available"));
  });

  it("manual: rate-limit error surfaces (raw Tauri string AND Error instance)", async () => {
    for (const err of ["rate_limited", new Error("rate_limited")]) {
      route({
        take_pending_manual_update_check: true,
        take_startup_update_check: false,
        get_setting_value: null,
        check_for_update: reject(err), // reject, not resolve
      });
      const { result, unmount } = renderHook(() => useUpdateCheck(false));
      await vi.waitFor(() => expect(result.current.modal).toMatchObject({ status: "error", rateLimited: true }));
      unmount();
    }
  });

  it("cleans up the event listener on unmount", async () => {
    route({ take_pending_manual_update_check: false, take_startup_update_check: false });
    const { unmount } = renderHook(() => useUpdateCheck(false));
    await new Promise((r) => setTimeout(r, 10));
    unmount();
    await new Promise((r) => setTimeout(r, 10));
    expect(evt.unlisten).toHaveBeenCalled();
  });

  it("close() hides the modal", async () => {
    route({
      take_pending_manual_update_check: false,
      take_startup_update_check: true,
      get_setting_value: null,
      check_for_update: AVAILABLE,
    });
    const { result } = renderHook(() => useUpdateCheck(false));
    await vi.waitFor(() => expect(result.current.modal?.status).toBe("available"));
    act(() => result.current.close());
    expect(result.current.modal).toBeNull();
  });

  it("upgrades an in-flight auto request to manual presentation", async () => {
    const fetchD = deferred<unknown>();
    let pending = false;
    route({
      take_pending_manual_update_check: () => pending,
      take_startup_update_check: true,
      get_setting_value: null,
      check_for_update: () => fetchD.promise,
    });
    const { result } = renderHook(() => useUpdateCheck(false));
    await new Promise((r) => setTimeout(r, 10)); // auto in flight
    pending = true; // tray set the flag
    await act(async () => { evt.cb?.({}); await new Promise((r) => setTimeout(r, 5)); });
    expect(result.current.modal?.status).toBe("loading"); // upgraded
    await act(async () => { fetchD.resolve({ ...AVAILABLE, update_available: false }); await new Promise((r) => setTimeout(r, 10)); });
    expect(result.current.modal?.status).toBe("uptodate"); // manual semantics, not silent
    expect(countCalls("check_for_update")).toBe(1); // single request
  });

  it("still presents when the upgrade lands during the final toggle reread (no stuck loading)", async () => {
    const reread = deferred<string | null>();
    let pending = false, reads = 0;
    route({
      take_pending_manual_update_check: () => pending,
      take_startup_update_check: true,
      get_setting_value: () => (++reads === 1 ? null : reread.promise),
      check_for_update: { ...AVAILABLE, update_available: false },
    });
    const { result } = renderHook(() => useUpdateCheck(false));
    await new Promise((r) => setTimeout(r, 15)); // auto fetched, now awaiting toggle reread (#2)
    pending = true;
    await act(async () => { evt.cb?.({}); await new Promise((r) => setTimeout(r, 5)); });
    expect(result.current.modal?.status).toBe("loading"); // upgraded during reread
    await act(async () => { reread.resolve("true"); await new Promise((r) => setTimeout(r, 10)); });
    expect(result.current.modal?.status).toBe("uptodate"); // presented, not stuck on loading
  });

  it("suppresses auto presentation when the toggle is disabled mid-flight", async () => {
    const fetchD = deferred<unknown>();
    let reads = 0;
    route({
      take_pending_manual_update_check: false,
      take_startup_update_check: true,
      get_setting_value: () => (++reads === 1 ? null : "false"), // enabled to start, disabled on reread
      check_for_update: () => fetchD.promise,
    });
    const { result } = renderHook(() => useUpdateCheck(false));
    await new Promise((r) => setTimeout(r, 10));
    await act(async () => { fetchD.resolve(AVAILABLE); await new Promise((r) => setTimeout(r, 10)); });
    expect(result.current.modal).toBeNull();
  });

  it("dedups repeated manual events into a single check", async () => {
    const fetchD = deferred<unknown>();
    let pending = false;
    route({
      take_pending_manual_update_check: () => pending,
      take_startup_update_check: false,
      get_setting_value: null,
      check_for_update: () => fetchD.promise,
    });
    renderHook(() => useUpdateCheck(false));
    await new Promise((r) => setTimeout(r, 10)); // mount consume: pending false
    pending = true;
    await act(async () => { evt.cb?.({}); await new Promise((r) => setTimeout(r, 3)); });
    await act(async () => { evt.cb?.({}); await new Promise((r) => setTimeout(r, 3)); }); // second event while in flight
    expect(countCalls("check_for_update")).toBe(1); // deduped
    await act(async () => { fetchD.resolve(AVAILABLE); });
  });

  it("denies the automatic check on webview recreation (startup token already taken)", async () => {
    route({
      take_pending_manual_update_check: false,
      take_startup_update_check: false, // token already consumed this process
      get_setting_value: null,
      check_for_update: AVAILABLE,
    });
    const { result } = renderHook(() => useUpdateCheck(false));
    await new Promise((r) => setTimeout(r, 15));
    expect(result.current.modal).toBeNull();
    expect(countCalls("check_for_update")).toBe(0);
  });

  it("dismissing the loading modal suppresses the in-flight manual result", async () => {
    const fetchD = deferred<unknown>();
    route({
      take_pending_manual_update_check: true,
      take_startup_update_check: false,
      get_setting_value: null,
      check_for_update: () => fetchD.promise,
    });
    const { result } = renderHook(() => useUpdateCheck(false));
    await vi.waitFor(() => expect(result.current.modal?.status).toBe("loading"));
    act(() => result.current.close());
    expect(result.current.modal).toBeNull();
    await act(async () => {
      fetchD.resolve(AVAILABLE);
      await new Promise((r) => setTimeout(r, 10));
    });
    expect(result.current.modal).toBeNull(); // superseded run's result is not presented
  });

  it("runs queued manual check after onboarding is re-run", async () => {
    let pending = false;
    route({
      take_pending_manual_update_check: () => pending,
      take_startup_update_check: true,
      get_setting_value: "false", // auto disabled: initial mount stays silent
      check_for_update: AVAILABLE,
    });
    const { result, rerender } = renderHook(({ a }) => useUpdateCheck(a), {
      initialProps: { a: false },
    });
    await new Promise((r) => setTimeout(r, 10)); // initial startup consumes tokens
    expect(result.current.modal).toBeNull();

    rerender({ a: true }); // onboarding re-run starts
    await new Promise((r) => setTimeout(r, 10));

    pending = true; // tray sets the flag while onboarding is active
    await act(async () => {
      evt.cb?.({});
      await new Promise((r) => setTimeout(r, 10));
    });
    expect(result.current.modal).toBeNull(); // left untouched while active
    expect(countCalls("take_pending_manual_update_check")).toBe(1); // only the initial mount consume

    rerender({ a: false }); // onboarding re-run completes
    await vi.waitFor(() => expect(result.current.modal?.status).toBe("available"));
  });
});

describe("startupCheckEnabled", () => {
  it("treats null as enabled and parses true/false", () => {
    expect(startupCheckEnabled(null)).toBe(true);
    expect(startupCheckEnabled("true")).toBe(true);
    expect(startupCheckEnabled("false")).toBe(false);
    expect(startupCheckEnabled("whatever")).toBe(false);
  });
});
