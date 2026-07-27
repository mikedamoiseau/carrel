import { describe, it, expect } from "vitest";
import { computeRenderWidth, MAX_RENDER_WIDTH, MIN_RENDER_WIDTH } from "./pageRenderWidth";

describe("computeRenderWidth", () => {
  it("caps requests at the maximum render width", () => {
    const w = computeRenderWidth(6000, { dualPage: false, zoom: 1, dpr: 2 });
    expect(w).toBe(MAX_RENDER_WIDTH);
  });

  it("does not cap PDF-sized requests below the maximum (preserves fallback sharpness)", () => {
    // 2000 px × 2 DPR = 4000 raw — a zoomed/high-DPR request that the uncached
    // direct-render path can honor. Must pass through, not be clamped to a
    // canonical width.
    const w = computeRenderWidth(2000, { dualPage: false, zoom: 1, dpr: 2 });
    expect(w).toBe(4000);
  });

  it("quantizes to 400 px steps so small resizes reuse the cache key", () => {
    // 1450 and 1550 both round to 1600 (round(1450/400)=4, round(1550/400)=4).
    expect(computeRenderWidth(1450, { dualPage: false, zoom: 1, dpr: 1 })).toBe(1600);
    expect(computeRenderWidth(1550, { dualPage: false, zoom: 1, dpr: 1 })).toBe(1600);
  });

  it("halves the base width in dual-page mode", () => {
    const single = computeRenderWidth(1600, { dualPage: false, zoom: 1, dpr: 1 });
    const dual = computeRenderWidth(1600, { dualPage: true, zoom: 1, dpr: 1 });
    expect(single).toBe(1600);
    expect(dual).toBe(800);
  });

  it("uses the fallback base before the container is measured", () => {
    const w = computeRenderWidth(0, { dualPage: false, zoom: 1, dpr: 1 });
    expect(w).toBe(1600); // FALLBACK_BASE_WIDTH, quantized
  });

  it("floors at the minimum render width for tiny containers", () => {
    const w = computeRenderWidth(100, { dualPage: false, zoom: 1, dpr: 1 });
    expect(w).toBe(MIN_RENDER_WIDTH);
  });

  it("raises resolution when zoomed in", () => {
    const w = computeRenderWidth(800, { dualPage: false, zoom: 2, dpr: 1 });
    expect(w).toBe(1600);
  });

  it("does not lower resolution for zoom below 1", () => {
    const zoomedOut = computeRenderWidth(800, { dualPage: false, zoom: 0.5, dpr: 1 });
    const normal = computeRenderWidth(800, { dualPage: false, zoom: 1, dpr: 1 });
    expect(zoomedOut).toBe(normal);
  });
});
