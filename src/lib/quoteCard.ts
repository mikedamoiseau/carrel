// Pure layout logic + thin canvas draw for shareable highlight "quote cards"
// (F-1-6). Split so the hard part — text fitting/wrapping — is pure and
// unit-tested; only `drawCard`/`renderCardToCanvas` touch the DOM/canvas.
//
// See docs/superpowers/specs/2026-07-12-highlight-quote-cards-design.md.

// ── Card geometry ────────────────────────────────────────────

/**
 * Card width in CSS px is fixed; height hugs its content (see
 * `computeCardLayout`), floored at `MIN_CARD_H` so a short quote still gets a
 * square-ish card instead of a sliver. The backing store renders at
 * `CARD_W*SCALE x cardHeight*SCALE`.
 */
export const CARD_W = 1080;
export const MIN_CARD_H = 1080;
/** Backing-store scale factor for retina-crisp exports. */
export const SCALE = 2;

/** Horizontal margin either side of the quote/footer content. */
const CONTENT_PADDING_X = 96;
/** Width available to the wrapped quote text. */
export const QUOTE_BOX_WIDTH_PX = CARD_W - CONTENT_PADDING_X * 2;

export const MIN_QUOTE_PX = 28;
export const MAX_QUOTE_PX = 64;
const FONT_STEP_PX = 2;
/**
 * Generous line budget — real highlights routinely run 10-12 lines wrapped
 * at a readable font size, and the content-hugging card height (see
 * `computeCardLayout`) means a longer quote just makes a taller card instead
 * of leaving whitespace. `MAX_QUOTE_CHARS` bounds the true worst case.
 */
export const MAX_LINES = 24;
/** Hard cap on quote length before layout — a pathological selection can't blow up the fit loop. */
export const MAX_QUOTE_CHARS = 600;

// ── Content-hugging layout constants ─────────────────────────

/** Space above the quote block. */
const TOP_MARGIN = 110;
/** Space between the end of the quote block and the footer row. */
const QUOTE_FOOTER_GAP = 72;
/** Space below the footer row (the wordmark, if shown, sits within this margin). */
const BOTTOM_MARGIN = 84;
/**
 * Vertical space the footer row occupies: the cover thumb plus title/author
 * text beside it, with room for the author line's descenders.
 */
const FOOTER_HEIGHT_PX = 132;

// ── Style presets ────────────────────────────────────────────

export type CardStyle = "light" | "sepia" | "dark";

export interface CardPalette {
  bg: string;
  ink: string;
  inkMuted: string;
  accent: string;
}

/**
 * Three fixed, tuned card palettes — deliberately NOT the reader's live
 * theme tokens (the reader also has a Custom mode with arbitrary CSS we
 * don't want to rasterize into a shared image).
 */
export const CARD_STYLES: Record<CardStyle, CardPalette> = {
  light: { bg: "#faf8f3", ink: "#2c2218", inkMuted: "#8c7b6e", accent: "#c2714e" },
  sepia: { bg: "#f0e4ce", ink: "#3b2510", inkMuted: "#7a5c3e", accent: "#9c5a2e" },
  dark: { bg: "#1a1614", ink: "#e8e2d9", inkMuted: "#9c8e83", accent: "#d4886a" },
};

/** Reader color mode -> default card style. Custom (arbitrary CSS) maps to light. */
export function defaultStyleForMode(mode: string): CardStyle {
  if (mode === "dark") return "dark";
  if (mode === "sepia") return "sepia";
  return "light"; // light, system, custom, and anything unrecognized
}

// ── Card input ───────────────────────────────────────────────

export interface CardInput {
  quote: string;
  title: string;
  author: string;
  style: CardStyle;
  includeCover: boolean;
  includeWordmark: boolean;
}

// ── Pure layout functions (unit-tested, no canvas/DOM) ───────

/**
 * Collapse all whitespace/newlines in a raw selection/highlight to single
 * spaces, trim, and hard-cap to `MAX_QUOTE_CHARS` so a pathological
 * selection can't blow up the fit loop below.
 */
export function sanitizeQuoteForCard(raw: string): string {
  const collapsed = raw.replace(/\s+/g, " ").trim();
  return collapsed.length > MAX_QUOTE_CHARS ? collapsed.slice(0, MAX_QUOTE_CHARS) : collapsed;
}

/** Measures the pixel width of a string at whatever font is currently set. */
export type Measure = (text: string) => number;

/**
 * Split `word` into chunks that each fit within `maxWidthPx`, per `measure`
 * (no hyphenation — layout approximation, not typography). Always makes
 * progress (each chunk is at least one character) even if a single character
 * alone exceeds `maxWidthPx`.
 */
function hardSplitByWidth(word: string, maxWidthPx: number, measure: Measure): string[] {
  const parts: string[] = [];
  let start = 0;
  while (start < word.length) {
    let end = start + 1;
    while (end < word.length && measure(word.slice(start, end + 1)) <= maxWidthPx) {
      end++;
    }
    parts.push(word.slice(start, end));
    start = end;
  }
  return parts;
}

/**
 * Greedy word-wrap: pack as many whitespace-separated words per line as fit
 * within `maxWidthPx`, using real measured widths via `measure` (e.g.
 * `ctx.measureText`). A single word wider than `maxWidthPx` is hard-split
 * across multiple lines.
 */
export function wrapTextByWidth(text: string, maxWidthPx: number, measure: Measure): string[] {
  const words = text.trim().split(/\s+/).filter((w) => w.length > 0);
  if (words.length === 0) return [];

  const lines: string[] = [];
  let current = "";

  for (const word of words) {
    if (measure(word) > maxWidthPx) {
      if (current) {
        lines.push(current);
        current = "";
      }
      const parts = hardSplitByWidth(word, maxWidthPx, measure);
      for (let i = 0; i < parts.length - 1; i++) lines.push(parts[i]);
      current = parts[parts.length - 1] ?? "";
      continue;
    }

    const candidate = current ? `${current} ${word}` : word;
    if (measure(candidate) <= maxWidthPx) {
      current = candidate;
    } else {
      lines.push(current);
      current = word;
    }
  }
  if (current) lines.push(current);

  return lines;
}

/**
 * Return `text` unchanged if it already fits within `maxWidthPx` (per
 * `measure`); otherwise trim it and append "…" so the result fits.
 */
export function truncateToWidth(text: string, maxWidthPx: number, measure: Measure): string {
  if (text.length === 0 || measure(text) <= maxWidthPx) return text;

  const ellipsis = "…";
  if (measure(ellipsis) > maxWidthPx) return ellipsis;

  // Binary search for the longest prefix such that `prefix + "…"` fits.
  let lo = 0;
  let hi = text.length;
  while (lo < hi) {
    const mid = Math.ceil((lo + hi) / 2);
    const candidate = `${text.slice(0, mid).trimEnd()}${ellipsis}`;
    if (measure(candidate) <= maxWidthPx) {
      lo = mid;
    } else {
      hi = mid - 1;
    }
  }
  return `${text.slice(0, lo).trimEnd()}${ellipsis}`;
}

export interface FitQuoteResult {
  fontSize: number;
  lines: string[];
  truncated: boolean;
}

export interface FitQuoteOptions {
  /** Width available to the wrapped text, in px. Defaults to `QUOTE_BOX_WIDTH_PX`. */
  boxWidthPx?: number;
  /** Max number of lines before truncation kicks in. Defaults to `MAX_LINES`. */
  maxLines?: number;
}

/** Builds a `Measure` for a given font size (e.g. sets `ctx.font` then measures). */
export type MeasureAt = (fontSize: number) => Measure;

/**
 * Choose the largest font size in `[MIN_QUOTE_PX, MAX_QUOTE_PX]` (stepping
 * down) whose wrapped quote — wrapped via `wrapTextByWidth` using real
 * measured widths from `measureAt(fontSize)` — fits within `maxLines`.
 * Measuring per font size (rather than a single measurer) is necessary
 * because a string's pixel width depends on the font size it's drawn at.
 *
 * If even `MIN_QUOTE_PX` overflows `maxLines`, keep the first `maxLines`
 * lines, append "…" to the last one, and set `truncated: true`.
 *
 * Deterministic: same inputs (and same `measureAt`) always produce the same
 * output.
 */
export function fitQuote(quote: string, measureAt: MeasureAt, opts: FitQuoteOptions = {}): FitQuoteResult {
  const boxWidthPx = opts.boxWidthPx ?? QUOTE_BOX_WIDTH_PX;
  const maxLines = opts.maxLines ?? MAX_LINES;

  for (let fontSize = MAX_QUOTE_PX; fontSize >= MIN_QUOTE_PX; fontSize -= FONT_STEP_PX) {
    const lines = wrapTextByWidth(quote, boxWidthPx, measureAt(fontSize));
    if (lines.length <= maxLines) {
      return { fontSize, lines, truncated: false };
    }
  }

  // Still overflows at MIN_QUOTE_PX: hard-truncate with a trailing ellipsis.
  const lines = wrapTextByWidth(quote, boxWidthPx, measureAt(MIN_QUOTE_PX)).slice(0, maxLines);
  const lastIndex = lines.length - 1;
  if (lastIndex >= 0) {
    lines[lastIndex] = `${lines[lastIndex].trimEnd()}…`;
  }
  return { fontSize: MIN_QUOTE_PX, lines, truncated: true };
}

export interface CardLayout {
  /** Final card height in CSS px — `MIN_CARD_H` or taller, hugging the content. */
  cardHeight: number;
  fontSize: number;
  lines: string[];
  truncated: boolean;
  lineHeight: number;
  /** Top y of the quote block (top-aligned text flow start). */
  quoteTop: number;
  /** Top y of the footer row (cover thumb top), directly below the quote block. */
  footerTop: number;
}

/**
 * Pure layout pass shared by measuring and drawing: fits the quote (via
 * `fitQuote`), then lays out top-to-bottom with fixed margins — quote block,
 * then a gap, then the footer row — and derives the card height from that
 * flow (floored at `MIN_CARD_H`). When the floor kicks in (a short quote),
 * the whole content group is centered in the extra vertical space rather
 * than left pinned to the top, so short quotes don't leave a lopsided void.
 */
export function computeCardLayout(quote: string, measureAt: MeasureAt, opts: FitQuoteOptions = {}): CardLayout {
  const { fontSize, lines, truncated } = fitQuote(quote, measureAt, opts);
  const lineHeight = fontSize * 1.35;
  const quoteBlockHeight = lines.length * lineHeight;
  const contentHeight = TOP_MARGIN + quoteBlockHeight + QUOTE_FOOTER_GAP + FOOTER_HEIGHT_PX + BOTTOM_MARGIN;
  const cardHeight = Math.max(MIN_CARD_H, contentHeight);
  const centerOffset = Math.max(0, (cardHeight - contentHeight) / 2);
  const quoteTop = TOP_MARGIN + centerOffset;
  const footerTop = quoteTop + quoteBlockHeight + QUOTE_FOOTER_GAP;
  return { cardHeight, fontSize, lines, truncated, lineHeight, quoteTop, footerTop };
}

// ── Canvas draw (impure — not unit-tested; canvas isn't available in jsdom) ──

const QUOTE_FONT_STACK = "'Playfair Display Variable', Georgia, serif";
const FOOTER_FONT_STACK = "'DM Sans Variable', system-ui, -apple-system, sans-serif";
const COVER_BOX_SIZE = 108;
const COVER_BOX_RADIUS = 10;
const WORDMARK_TEXT = "Carrel";

function drawCoverThumb(ctx: CanvasRenderingContext2D, img: HTMLImageElement, x: number, y: number) {
  const size = COVER_BOX_SIZE;
  ctx.save();
  ctx.beginPath();
  ctx.moveTo(x + COVER_BOX_RADIUS, y);
  ctx.arcTo(x + size, y, x + size, y + size, COVER_BOX_RADIUS);
  ctx.arcTo(x + size, y + size, x, y + size, COVER_BOX_RADIUS);
  ctx.arcTo(x, y + size, x, y, COVER_BOX_RADIUS);
  ctx.arcTo(x, y, x + size, y, COVER_BOX_RADIUS);
  ctx.closePath();
  ctx.clip();

  // object-fit: cover — crop to the box's aspect ratio, never stretch.
  const imgW = img.naturalWidth || img.width;
  const imgH = img.naturalHeight || img.height;
  const scale = Math.max(size / imgW, size / imgH);
  const drawW = imgW * scale;
  const drawH = imgH * scale;
  const drawX = x + (size - drawW) / 2;
  const drawY = y + (size - drawH) / 2;
  ctx.drawImage(img, drawX, drawY, drawW, drawH);
  ctx.restore();
}

function measureQuoteFont(ctx: CanvasRenderingContext2D): MeasureAt {
  return (fontSize) => (s) => {
    ctx.font = `italic 600 ${fontSize}px ${QUOTE_FONT_STACK}`;
    return ctx.measureText(s).width;
  };
}

/**
 * Fill background, draw the wrapped quote (sized via `fitQuote`), and the
 * footer: optional cover thumbnail, title, author (omitted entirely when
 * empty — no dangling "— , Title"), and an optional low-contrast wordmark.
 * The footer flows directly below the quote block rather than being pinned
 * to the card bottom — see `computeCardLayout` for the height/flow math.
 * Assumes fonts are already loaded (caller awaits `document.fonts.ready`).
 * Returns the card height actually used (the canvas must already be sized to
 * match — see `renderCardToCanvas`).
 */
export function drawCard(ctx: CanvasRenderingContext2D, input: CardInput, coverImg: HTMLImageElement | null): number {
  const palette = CARD_STYLES[input.style];

  // Measure against the live ctx at each candidate font size so the lines
  // fitQuote returns are the exact ones drawn below (real glyph widths, not
  // a fixed-ratio estimate — matters for CJK, long uppercase runs, etc).
  const { cardHeight, fontSize, lines, lineHeight, quoteTop, footerTop } = computeCardLayout(
    input.quote,
    measureQuoteFont(ctx),
  );

  ctx.fillStyle = palette.bg;
  ctx.fillRect(0, 0, CARD_W, cardHeight);

  ctx.fillStyle = palette.ink;
  ctx.font = `italic 600 ${fontSize}px ${QUOTE_FONT_STACK}`;
  ctx.textAlign = "left";
  ctx.textBaseline = "alphabetic";
  lines.forEach((line, i) => {
    ctx.fillText(line, CONTENT_PADDING_X, quoteTop + (i + 1) * lineHeight - lineHeight * 0.25);
  });

  // Footer — cover thumb top-aligned to footerTop; title/author baselines
  // anchored to a reference point 100px into the footer row (same relative
  // offsets the card always used, just re-anchored to the flowed position
  // instead of a fixed distance from the card bottom).
  const showCover = input.includeCover && coverImg !== null;
  const footerTextX = showCover ? CONTENT_PADDING_X + COVER_BOX_SIZE + 24 : CONTENT_PADDING_X;
  const footerTextMaxWidth = CARD_W - CONTENT_PADDING_X - footerTextX;
  const footerAnchorY = footerTop + 100;

  if (showCover && coverImg) {
    drawCoverThumb(ctx, coverImg, CONTENT_PADDING_X, footerTop);
  }

  const hasAuthor = input.author.trim().length > 0;
  ctx.textAlign = "left";
  ctx.fillStyle = palette.ink;
  ctx.font = `700 30px ${FOOTER_FONT_STACK}`;
  const title = truncateToWidth(input.title, footerTextMaxWidth, (s) => ctx.measureText(s).width);
  ctx.fillText(title, footerTextX, hasAuthor ? footerAnchorY - 14 : footerAnchorY);

  if (hasAuthor) {
    ctx.fillStyle = palette.inkMuted;
    ctx.font = `400 24px ${FOOTER_FONT_STACK}`;
    const author = truncateToWidth(input.author, footerTextMaxWidth, (s) => ctx.measureText(s).width);
    ctx.fillText(author, footerTextX, footerAnchorY + 24);
  }

  if (input.includeWordmark) {
    ctx.textAlign = "right";
    ctx.fillStyle = palette.accent;
    ctx.globalAlpha = 0.55;
    ctx.font = `600 22px ${FOOTER_FONT_STACK}`;
    ctx.fillText(WORDMARK_TEXT, CARD_W - CONTENT_PADDING_X, cardHeight - 40);
    ctx.globalAlpha = 1;
  }

  return cardHeight;
}

/**
 * Render a full quote card into a fresh offscreen canvas at `CARD_W*SCALE x
 * cardHeight*SCALE` (2x backing store for retina-crisp exports; `cardHeight`
 * hugs the content, see `computeCardLayout`). Used for both the dialog's
 * live preview and the final export (`toBlob`/`getImageData`).
 */
export function renderCardToCanvas(input: CardInput, coverImg: HTMLImageElement | null): HTMLCanvasElement {
  const canvas = document.createElement("canvas");
  // Canvas pixel dimensions don't affect ctx.measureText, so it's safe to
  // measure at a placeholder height before we know the final one.
  canvas.width = CARD_W * SCALE;
  canvas.height = MIN_CARD_H * SCALE;
  let ctx = canvas.getContext("2d");
  if (!ctx) return canvas;
  ctx.scale(SCALE, SCALE);
  const { cardHeight } = computeCardLayout(input.quote, measureQuoteFont(ctx));

  // Resizing the canvas resets its 2D state (transform included), so
  // re-fetch the context and re-apply the scale before drawing.
  canvas.width = CARD_W * SCALE;
  canvas.height = cardHeight * SCALE;
  ctx = canvas.getContext("2d");
  if (!ctx) return canvas;
  ctx.scale(SCALE, SCALE);
  drawCard(ctx, input, coverImg);
  return canvas;
}
