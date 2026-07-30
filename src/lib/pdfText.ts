// Pure mapping helpers for the PDF text-selection layer (F-1-4, M3).
// No Tauri/DOM dependencies — see pdfText.test.ts for the Vitest coverage.
// `Glyph` mirrors carrel-core's `Glyph` struct byte-for-byte (see
// `carrel-core/src/pdf.rs`): `off` is the char ordinal into the page's
// `chars()`-built text; `x/y/w/h` are normalized (0..1) fractions of the
// page's rendered box, `y` already converted to top-down orientation.

export interface Glyph {
  off: number;
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface PxRect {
  left: number;
  top: number;
  width: number;
  height: number;
}

/** Identity of the bitmap a rendered <img> actually displays: which book and
 *  which page its current src represents (set atomically with the blob URL). */
export interface ImageId {
  bookId: string;
  page: number;
}

/**
 * Whether a PDF text layer may become interactive: the image currently ON
 * SCREEN must be exactly this book+page AND the layer's box must be measured.
 * `displayedId` reflects the loaded bitmap's identity (set on the <img>'s
 * onLoad from the src it represents), NOT the navigation target — so a late
 * load event for a superseded page, or a same-index book switch, can't pair
 * current-page text with a stale bitmap. See PdfTextLayer / F-1-4 page-turn
 * race.
 */
export function imageLayerActive(
  displayedId: ImageId | null,
  bookId: string,
  pageIndex: number,
  hasBox: boolean,
): boolean {
  return (
    displayedId !== null &&
    displayedId.bookId === bookId &&
    displayedId.page === pageIndex &&
    hasBox
  );
}

/** Scale a normalized glyph rect to pixels within a `boxW`x`boxH` box (the rendered page image). */
export function glyphToPx(g: Glyph, boxW: number, boxH: number): PxRect {
  return {
    left: g.x * boxW,
    top: g.y * boxH,
    width: g.w * boxW,
    height: g.h * boxH,
  };
}

/**
 * Reduce a set of selected glyphs to the `[startOffset, endOffset)` char
 * range they cover — the same offset space `add_highlight`'s
 * `startOffset`/`endOffset` params expect. `endOffset` is exclusive (one
 * past the max `off` seen). Returns `null` for an empty selection.
 */
export function selectionOffsets(
  selected: Glyph[],
): { startOffset: number; endOffset: number } | null {
  if (selected.length === 0) return null;
  let min = Infinity;
  let max = -Infinity;
  for (const g of selected) {
    if (g.off < min) min = g.off;
    if (g.off > max) max = g.off;
  }
  return { startOffset: min, endOffset: max + 1 };
}

/**
 * Which glyph `off` values a selection `range` actually covers with POSITIVE
 * width — i.e. spans the range genuinely intersects, not ones it merely
 * touches at an edge. `containsNode(span, true)` (partial containment) counts
 * a zero-width edge touch as selected, so a range from the end of A to the
 * start of D over single-char spans A B C D would wrongly include A and D.
 *
 * Positive-width overlap requires `range.start < span.end` AND
 * `range.end > span.start`:
 *   - a range ending exactly at a span's start → `END_TO_START == 0` (not < 0)
 *     → span excluded;
 *   - a range starting exactly at a span's end → `START_TO_END == 0` (not > 0)
 *     → span excluded.
 */
export function selectedOffsets(range: Range, spans: Iterable<HTMLElement>): number[] {
  const offs: number[] = [];
  for (const span of spans) {
    const sr = span.ownerDocument.createRange();
    sr.selectNodeContents(span);
    const overlaps =
      range.compareBoundaryPoints(Range.END_TO_START, sr) < 0 &&
      range.compareBoundaryPoints(Range.START_TO_END, sr) > 0;
    if (overlaps) offs.push(Number(span.dataset.off));
  }
  return offs;
}

// Two rects are treated as the same text row if their tops are within this
// fraction of the (shorter) row's glyph height — loose_bounds() rects on the
// same line can differ slightly (ascenders/descenders), so an exact-equality
// check would needlessly split one visual line into several bands.
const ROW_TOLERANCE_FACTOR = 0.5;

/**
 * Resolve the glyphs covering `[startOffset, endOffset)` into merged
 * pixel rect "bands" for rendering a highlight — one band per text row,
 * spanning that row's min-left..max-right and its top/height.
 */
export function highlightBands(
  glyphs: Glyph[],
  startOffset: number,
  endOffset: number,
  boxW: number,
  boxH: number,
): PxRect[] {
  const rects = glyphs
    .filter((g) => g.off >= startOffset && g.off < endOffset && g.w > 0 && g.h > 0)
    .map((g) => glyphToPx(g, boxW, boxH))
    .sort((a, b) => a.top - b.top || a.left - b.left);

  const rows: PxRect[][] = [];
  for (const rect of rects) {
    const row = rows[rows.length - 1];
    const rowAnchor = row?.[0];
    if (row && rowAnchor && Math.abs(rect.top - rowAnchor.top) <= rowAnchor.height * ROW_TOLERANCE_FACTOR) {
      row.push(rect);
    } else {
      rows.push([rect]);
    }
  }

  return rows.map((row) => {
    const left = Math.min(...row.map((r) => r.left));
    const right = Math.max(...row.map((r) => r.left + r.width));
    const top = Math.min(...row.map((r) => r.top));
    // Span to the lowest bottom edge in the row, not max(height) from the
    // topmost glyph — otherwise a glyph that sits lower (e.g. a descender)
    // has its bottom clipped by the band.
    const bottom = Math.max(...row.map((r) => r.top + r.height));
    return { left, top, width: right - left, height: bottom - top };
  });
}
