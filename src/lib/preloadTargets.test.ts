import { describe, it, expect } from "vitest";
import { computePreloadTargets } from "./preloadTargets";

describe("computePreloadTargets", () => {
  it("single page mid-book: forward pages first, then backward", () => {
    const t = computePreloadTargets({ visibleLeft: 5, visibleRight: null, totalPages: 20, radius: 2 });
    expect(t).toEqual([6, 7, 4, 3]);
  });

  it("single page near the start: no negative indices", () => {
    const t = computePreloadTargets({ visibleLeft: 0, visibleRight: null, totalPages: 20, radius: 2 });
    expect(t).toEqual([1, 2]);
  });

  it("single page near the end: no overflow past totalPages", () => {
    const t = computePreloadTargets({ visibleLeft: 19, visibleRight: null, totalPages: 20, radius: 2 });
    expect(t).toEqual([18, 17]);
  });

  it("dual page: preloads the next and previous spreads, forward first", () => {
    const t = computePreloadTargets({ visibleLeft: 4, visibleRight: 5, totalPages: 20, radius: 2 });
    expect(t).toEqual([6, 7, 3, 2]);
  });

  it("dual page never re-preloads a currently-visible page", () => {
    const t = computePreloadTargets({ visibleLeft: 4, visibleRight: 5, totalPages: 20, radius: 2 });
    expect(t).not.toContain(4);
    expect(t).not.toContain(5);
  });

  it("tiny book: clamps to the few valid neighbors", () => {
    const t = computePreloadTargets({ visibleLeft: 0, visibleRight: 1, totalPages: 3, radius: 2 });
    expect(t).toEqual([2]);
  });

  it("radius 1 preloads exactly one page each side", () => {
    const t = computePreloadTargets({ visibleLeft: 5, visibleRight: null, totalPages: 20, radius: 1 });
    expect(t).toEqual([6, 4]);
  });

  it("returns nothing for a single-page book", () => {
    const t = computePreloadTargets({ visibleLeft: 0, visibleRight: null, totalPages: 1, radius: 2 });
    expect(t).toEqual([]);
  });
});
