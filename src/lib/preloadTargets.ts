// Pure selection of which page indices to preload around the current view,
// extracted from PageViewer so the ordering/bounds logic can be unit-tested
// without React or the Tauri bridge.
//
// Preloading is idle-triggered (see PageViewer): a short debounce plus
// requestIdleCallback means it only runs once the reader has settled on a
// page, never while flipping. The list this returns is ordered forward-first
// so a forward turn — the common case — is warm before a backward one, and the
// caller pulls from it with a small concurrency cap so a network-mounted
// library isn't flooded with parallel renders.

export interface PreloadTargetsInput {
  /** Left (or only) visible page index. */
  visibleLeft: number;
  /** Right visible page index in dual-page mode, or null (single page / last page alone). */
  visibleRight: number | null;
  /** Total pages in the book. */
  totalPages: number;
  /** How many pages to preload on each side of the visible window. */
  radius: number;
}

/**
 * Ordered, deduped, in-range list of page indices to preload — all `radius`
 * forward pages first (nearest first), then the backward pages. Visible and
 * out-of-range indices are excluded.
 */
export function computePreloadTargets({
  visibleLeft,
  visibleRight,
  totalPages,
  radius,
}: PreloadTargetsInput): number[] {
  const minVisible = Math.min(visibleLeft, visibleRight ?? visibleLeft);
  const maxVisible = Math.max(visibleLeft, visibleRight ?? visibleLeft);

  const targets: number[] = [];
  const seen = new Set<number>();
  const push = (idx: number) => {
    if (idx < 0 || idx >= totalPages) return;
    if (idx >= minVisible && idx <= maxVisible) return; // already visible
    if (seen.has(idx)) return;
    seen.add(idx);
    targets.push(idx);
  };

  // Forward first (nearest neighbor first), then backward.
  for (let i = 1; i <= radius; i++) push(maxVisible + i);
  for (let i = 1; i <= radius; i++) push(minVisible - i);

  return targets;
}
