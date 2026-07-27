// Pure computation of the render-target width for a reader page request,
// extracted from PageViewer so it can be unit-tested without the DOM.
//
// The requested width drives both the backend raster cost and the frontend
// blob-cache key. The performance-relevant rule here is coarse quantization:
// snapping the width to QUANTIZE_STEP means small window-size jitter (a resize
// drag, a scrollbar appearing/disappearing) doesn't produce a new cache key
// and invalidate every already-fetched page blob.
//
// Note we deliberately do NOT cap PDF requests below the comic maximum. A
// cache hit already returns the canonical page untouched whenever the request
// is at least the canonical width (the backend's resize helper passes bytes
// through when `src_w <= target`), so a large request costs nothing on a hit —
// while the uncached direct-render fallback (`prepare_pdf` failed / no file
// hash) genuinely uses the larger width for a sharper zoomed page. Capping
// would only degrade that fallback with no gain on the hot path.

/** Upper bound for any page request, to bound worst-case raster cost. */
export const MAX_RENDER_WIDTH = 9600;
/** Quantization granularity in px. Coarser than the render itself needs, to stabilize the cache key. */
export const QUANTIZE_STEP = 400;
/** Floor so a not-yet-measured or tiny container still requests a legible page. */
export const MIN_RENDER_WIDTH = 400;
/** Fallback base width before the container has been measured. */
export const FALLBACK_BASE_WIDTH = 1600;

export interface RenderWidthInputs {
  /** True when two pages are shown side by side (each gets half the width). */
  dualPage: boolean;
  /** Current zoom factor; values below 1 do not lower render resolution. */
  zoom: number;
  /** Device pixel ratio for Retina sharpness. */
  dpr: number;
}

/**
 * Compute the quantized, clamped render width for one page.
 *
 * @param containerWidth measured CSS px of the page container, or 0/undefined
 *        before the ResizeObserver has reported.
 */
export function computeRenderWidth(
  containerWidth: number,
  { dualPage, zoom, dpr }: RenderWidthInputs,
): number {
  const base = containerWidth > 0 ? containerWidth : FALLBACK_BASE_WIDTH;
  const perPage = dualPage ? base / 2 : base;
  const raw = perPage * Math.max(zoom, 1) * dpr;
  const quantized = Math.round(raw / QUANTIZE_STEP) * QUANTIZE_STEP;
  return Math.min(MAX_RENDER_WIDTH, Math.max(MIN_RENDER_WIDTH, quantized));
}
