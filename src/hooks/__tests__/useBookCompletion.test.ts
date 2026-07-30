// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { useBookCompletion } from "../useBookCompletion";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
}));

import { invoke } from "@tauri-apps/api/core";

const bookInfo = {
  title: "Test Book",
  author: "Test Author",
  cover_path: "/covers/book-1.jpg",
};

describe("useBookCompletion", () => {
  beforeEach(() => {
    localStorage.clear();
    vi.mocked(invoke).mockImplementation((cmd: string) => {
      if (cmd === "get_book_reading_time") return Promise.resolve(3600);
      if (cmd === "get_book") return Promise.resolve(bookInfo);
      return Promise.resolve(undefined);
    });
  });

  it("does not show celebration on earlier chapters", () => {
    const { result } = renderHook(() => useBookCompletion("book-1", 3, 10));
    expect(result.current.showCelebration).toBe(false);
  });

  it("shows celebration when transitioning to last chapter", async () => {
    const { result, rerender } = renderHook(
      ({ ch }) => useBookCompletion("book-1", ch, 10),
      { initialProps: { ch: 8 } },
    );

    rerender({ ch: 9 });
    await vi.waitFor(() => expect(result.current.showCelebration).toBe(true));
    expect(result.current.completionData?.title).toBe("Test Book");
    await vi.waitFor(() => expect(result.current.readingTimeSecs).toBe(3600));
  });

  it("does not show after dismiss", async () => {
    const { result, rerender } = renderHook(
      ({ ch }) => useBookCompletion("book-1", ch, 10),
      { initialProps: { ch: 8 } },
    );
    rerender({ ch: 9 });
    await vi.waitFor(() => expect(result.current.showCelebration).toBe(true));
    act(() => result.current.dismiss());
    expect(result.current.showCelebration).toBe(false);
    expect(JSON.parse(localStorage.getItem("carrel-celebration-dismissed") || "[]")).toContain("book-1");
  });

  it("does not show if already dismissed in localStorage", async () => {
    localStorage.setItem("carrel-celebration-dismissed", JSON.stringify(["book-1"]));
    const { result, rerender } = renderHook(
      ({ ch }) => useBookCompletion("book-1", ch, 10),
      { initialProps: { ch: 8 } },
    );
    rerender({ ch: 9 });
    await vi.waitFor(() => expect(result.current.showCelebration).toBe(false));
  });

  it("does not trigger on initial mount at last chapter", () => {
    const { result } = renderHook(() => useBookCompletion("book-1", 9, 10));
    expect(result.current.showCelebration).toBe(false);
  });
});
