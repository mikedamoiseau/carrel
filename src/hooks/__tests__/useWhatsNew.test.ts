// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { useWhatsNew } from "../useWhatsNew";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(() => Promise.resolve(() => {})),
}));

vi.mock("virtual:release-notes", () => ({
  releaseNotes: [
    {
      version: "2.0.3",
      date: "2026-05-18",
      categories: {
        Added: [{ title: "OPDS feed", description: "New primitives." }],
      },
    },
  ],
  appVersion: "2.0.3",
}));

import { invoke } from "@tauri-apps/api/core";

describe("useWhatsNew", () => {
  beforeEach(() => {
    localStorage.clear();
    vi.mocked(invoke).mockResolvedValue(true);
  });

  it("shows banner when flag enabled, not dismissed, and onboarding complete", async () => {
    localStorage.setItem("carrel-onboarding-complete", "true");
    const { result } = renderHook(() => useWhatsNew());
    await vi.waitFor(() => expect(result.current.showBanner).toBe(true));
  });

  it("hides banner when already dismissed for current version", async () => {
    localStorage.setItem("carrel-onboarding-complete", "true");
    localStorage.setItem("carrel-whats-new-dismissed", "2.0.3");
    const { result } = renderHook(() => useWhatsNew());
    await vi.waitFor(() => expect(result.current.flagLoaded).toBe(true));
    expect(result.current.showBanner).toBe(false);
  });

  it("hides banner when onboarding not completed (fresh install)", async () => {
    const { result } = renderHook(() => useWhatsNew());
    await vi.waitFor(() => expect(result.current.flagLoaded).toBe(true));
    expect(result.current.showBanner).toBe(false);
  });

  it("hides banner when feature flag disabled", async () => {
    localStorage.setItem("carrel-onboarding-complete", "true");
    vi.mocked(invoke).mockResolvedValue(false);
    const { result } = renderHook(() => useWhatsNew());
    await vi.waitFor(() => expect(result.current.flagLoaded).toBe(true));
    expect(result.current.showBanner).toBe(false);
  });

  it("dismissBanner sets localStorage and hides banner", async () => {
    localStorage.setItem("carrel-onboarding-complete", "true");
    const { result } = renderHook(() => useWhatsNew());
    await vi.waitFor(() => expect(result.current.showBanner).toBe(true));
    act(() => result.current.dismissBanner());
    expect(result.current.showBanner).toBe(false);
    expect(localStorage.getItem("carrel-whats-new-dismissed")).toBe("2.0.3");
  });

  it("openModal and closeModal toggle showModal", async () => {
    localStorage.setItem("carrel-onboarding-complete", "true");
    const { result } = renderHook(() => useWhatsNew());
    await vi.waitFor(() => expect(result.current.flagLoaded).toBe(true));
    expect(result.current.showModal).toBe(false);
    act(() => result.current.openModal());
    expect(result.current.showModal).toBe(true);
    act(() => result.current.closeModal());
    expect(result.current.showModal).toBe(false);
  });

  it("currentRelease matches appVersion", async () => {
    localStorage.setItem("carrel-onboarding-complete", "true");
    const { result } = renderHook(() => useWhatsNew());
    await vi.waitFor(() => expect(result.current.currentRelease).not.toBeNull());
    expect(result.current.currentRelease!.version).toBe("2.0.3");
  });
});
