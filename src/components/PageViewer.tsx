import { useState, useEffect, useLayoutEffect, useCallback, useRef, type RefObject } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { invoke } from "@tauri-apps/api/core";
import { getSpreadPages } from "../lib/utils";
import { friendlyError } from "../lib/errors";
import { blobUrlFromBytes } from "../lib/pageWire";
import { computeRenderWidth } from "../lib/pageRenderWidth";
import { computePreloadTargets } from "../lib/preloadTargets";
import { glyphToPx, highlightBands, selectionOffsets, selectedOffsets, imageLayerActive, type Glyph, type ImageId } from "../lib/pdfText";
import { HIGHLIGHT_COLORS } from "./HighlightsPanel";
import { useToast } from "./Toast";

/** A resolved PDF text selection, in the page's char-offset space. */
export interface PdfSelection {
  text: string;
  startOffset: number;
  endOffset: number;
  pageIndex: number;
}

const MIN_ZOOM = 0.5;
const MAX_ZOOM = 4;
const ZOOM_STEP = 0.25;
// How long the zoom level must hold still before the backend page is
// re-rendered at the new width. A wheel/keyboard zoom burst fires many steps in
// quick succession; debouncing collapses them into ONE render at the settled
// width. 200ms is below the ~250ms that reads as sluggish, yet long enough to
// swallow a fast burst. The CSS transform (applyTransform) uses the raw zoom, so
// visual feedback stays instant regardless of this delay.
const ZOOM_SETTLE_MS = 200;
// Max blob URLs kept alive at once. ~±12 pages around the current one, so
// backtracking through a chapter stays a cache hit. Coarse render-width
// quantization (see pageRenderWidth) keeps a window resize from churning
// these keys and blowing the window on every drag.
const BLOB_CACHE_MAX = 24;
// Preload window: pages on each side of the visible one warmed once the reader
// settles. Radius 2 covers a forward turn plus the one after it.
const PRELOAD_RADIUS = 2;
// Never render more than this many preloads at once — a network-mounted
// library (NAS) can't absorb a burst of parallel page renders without the
// foreground turn getting starved.
const MAX_PRELOAD_INFLIGHT = 2;
// Debounce before a settled page triggers preloading. Short enough to be
// invisible between reading-paced turns, long enough to coalesce a fast flip
// burst so mid-burst pages are never preloaded.
const PRELOAD_DEBOUNCE_MS = 250;
// Upper bound on how long the browser may defer the idle callback.
const PRELOAD_IDLE_TIMEOUT_MS = 600;

/**
 * Schedule `cb` to run when the browser is idle, falling back to a microtask
 * timer where requestIdleCallback is unavailable (not verified present in
 * every WebView this ships to). Returns a canceller.
 */
function scheduleWhenIdle(cb: () => void): () => void {
  if (typeof requestIdleCallback === "function") {
    const id = requestIdleCallback(cb, { timeout: PRELOAD_IDLE_TIMEOUT_MS });
    return () => cancelIdleCallback(id);
  }
  const id = setTimeout(cb, 0);
  return () => clearTimeout(id);
}

// Enable with: localStorage.setItem("folio-debug-pages", "1")
// Disable with: localStorage.removeItem("folio-debug-pages")
// Takes effect immediately — no reload needed.
function dbg(...args: unknown[]) {
  if (localStorage.getItem("folio-debug-pages") === "1") {
    console.warn("[page-load]", ...args);
  }
}

interface PageViewerProps {
  bookId: string;
  format: "cbz" | "cbr" | "pdf";
  totalPages: number;
  initialPage?: number;
  onPageChange?: (pageIndex: number) => void;
  /**
   * Fires when the user jumps to a non-adjacent page via the "Go to page"
   * input — distinct from prev/next sequential navigation. The receiver is
   * expected to record the jump in navigation history so back/forward can
   * return to the source page.
   */
  onPageJump?: (targetIndex: number) => void;
  dualPage?: boolean;
  mangaMode?: boolean;
  pageAnimation?: boolean;
  /** When false, skip the window-level keydown listener (split-mode companion pane). */
  keyboardEnabled?: boolean;
  /**
   * PDF-only (F-1-4). Create a highlight from a text-layer selection.
   * Provided by ReaderPane so PDF highlights run the same IPC + toast/undo
   * path as the EPUB reader. Absent for comics.
   */
  onCreateHighlight?: (color: string, sel: PdfSelection) => Promise<unknown> | void;
  /** PDF-only. Remove the given highlight ids (selection-overlap clear). */
  onRemoveHighlights?: (ids: string[]) => Promise<void> | void;
  /** PDF-only. Bumps whenever a highlight is created/removed elsewhere so the
   *  visible page's saved-highlight bands re-fetch. */
  highlightRefreshKey?: number;
}

export default function PageViewer({
  bookId,
  format,
  totalPages,
  initialPage = 0,
  onPageChange,
  onPageJump,
  dualPage = false,
  mangaMode = false,
  pageAnimation = true,
  keyboardEnabled = true,
  onCreateHighlight,
  onRemoveHighlights,
  highlightRefreshKey = 0,
}: PageViewerProps) {
  const [pageIndex, setPageIndex] = useState(initialPage);

  // Sync with external page changes (e.g. search result navigation)
  const prevInitialPage = useRef(initialPage);
  useEffect(() => {
    if (initialPage !== prevInitialPage.current) {
      prevInitialPage.current = initialPage;
      setPageIndex(initialPage);
      onPageChange?.(initialPage);
    }
  }, [initialPage, onPageChange]);

  const [leftImageData, setLeftImageData] = useState<string | null>(null);
  const [rightImageData, setRightImageData] = useState<string | null>(null);
  // Identity of the bitmap each <img>'s current src REPRESENTS ({bookId, page}),
  // stored in lockstep with the blob URL (same update in the page-load effect).
  // The <img>'s onLoad records this identity (via a ref, to avoid the
  // stale-closure that binding to the nav target `spread.left` would cause)
  // into leftDisplayedId/rightDisplayedId. The PDF text layer only activates
  // when the on-screen bitmap is exactly its book+page, so a late load for a
  // superseded page (rapid A→B→C nav) or a same-index book switch can't pair
  // current-page text with a stale bitmap (F-1-4 page-turn race).
  const [leftImageId, setLeftImageId] = useState<ImageId | null>(null);
  const [rightImageId, setRightImageId] = useState<ImageId | null>(null);
  const [leftDisplayedId, setLeftDisplayedId] = useState<ImageId | null>(null);
  const [rightDisplayedId, setRightDisplayedId] = useState<ImageId | null>(null);
  const leftImageIdRef = useRef<ImageId | null>(null);
  const rightImageIdRef = useRef<ImageId | null>(null);
  leftImageIdRef.current = leftImageId;
  rightImageIdRef.current = rightImageId;
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [retryCount, setRetryCount] = useState(0);

  // Zoom & pan state — restore persisted zoom level per book
  const zoomStorageKey = `folio-zoom-${bookId}`;
  const zoomPersistTimer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const [zoom, setZoomState] = useState(() => {
    const stored = localStorage.getItem(zoomStorageKey);
    if (stored) {
      const parsed = parseFloat(stored);
      if (!isNaN(parsed) && parsed >= MIN_ZOOM && parsed <= MAX_ZOOM) return parsed;
    }
    return 1;
  });
  const [zoomRestored, setZoomRestored] = useState(() => {
    const stored = localStorage.getItem(zoomStorageKey);
    if (stored) {
      const parsed = parseFloat(stored);
      return !isNaN(parsed) && parsed >= MIN_ZOOM && parsed <= MAX_ZOOM && parsed !== 1;
    }
    return false;
  });
  // Auto-hide zoom-restored indicator after 2s
  useEffect(() => {
    if (!zoomRestored) return;
    const t = setTimeout(() => setZoomRestored(false), 2000);
    return () => clearTimeout(t);
  }, [zoomRestored]);
  const setZoom = useCallback((value: number | ((prev: number) => number)) => {
    setZoomState((prev) => {
      const next = typeof value === "function" ? value(prev) : value;
      clearTimeout(zoomPersistTimer.current);
      zoomPersistTimer.current = setTimeout(() => {
        localStorage.setItem(zoomStorageKey, String(next));
      }, 500);
      return next;
    });
    setZoomRestored(false);
  }, [zoomStorageKey]);
  const panRef = useRef({ x: 0, y: 0 });
  const isPanning = useRef(false);
  const panStart = useRef({ x: 0, y: 0 });
  const panOffset = useRef({ x: 0, y: 0 });
  const containerRef = useRef<HTMLDivElement>(null);
  const spreadRef = useRef<HTMLDivElement>(null);
  // Refs to the rendered page images — the PDF text layer measures each
  // image's object-contain rendered box to position glyph spans/bands.
  const leftImgRef = useRef<HTMLImageElement>(null);
  const rightImgRef = useRef<HTMLImageElement>(null);
  const directionRef = useRef<"left" | "right">("right");
  const isInitialLoad = useRef(true);
  const animationRef = useRef<Animation | null>(null);
  const isAnimating = useRef(false);
  // Tracks the last page index that successfully animated in. Used to
  // suppress redundant slide-in animations that fire when the
  // load-spread effect re-runs for reasons that are not a real page
  // turn — for example, a `renderWidth` quantization flip caused by a
  // sibling component (the thumbnail strip) mounting and reflowing
  // the layout, which changes the cache key and forces a re-fetch.
  const lastAnimatedPageRef = useRef<number | null>(null);

  const { t } = useTranslation();
  const isPdf = format === "pdf";
  const spread = dualPage ? getSpreadPages(pageIndex, totalPages) : { left: pageIndex, right: null };

  // Apply transform directly to the DOM (no React re-render).
  // Use physical resize (width/height %) so the browser resamples images at full resolution
  // instead of CSS scale() which blurs by upscaling the rasterized paint buffer.
  const applyTransform = useCallback((z: number, p: { x: number; y: number }) => {
    if (!spreadRef.current) return;
    spreadRef.current.style.width = `${z * 100}%`;
    spreadRef.current.style.height = `${z * 100}%`;
    spreadRef.current.style.transform = `translate(calc(-50% + ${p.x}px), calc(-50% + ${p.y}px))`;
  }, []);

  // Animate new page sliding into view using Web Animations API.
  // Runs on spreadRef directly — the API applies the animation in a separate layer
  // that overrides inline styles during playback, then reverts when done.
  // No conflict with applyTransform's inline styles.
  const slideIn = useCallback(() => {
    if (isInitialLoad.current) {
      isInitialLoad.current = false;
      return;
    }
    if (!pageAnimation || !spreadRef.current) return;
    // Cancel any in-progress animation
    animationRef.current?.cancel();
    // Offset is half the container width — enough to start fully off-page
    const containerW = containerRef.current?.clientWidth ?? 800;
    const offset = directionRef.current === "right" ? containerW / 2 : -containerW / 2;
    isAnimating.current = true;
    animationRef.current = spreadRef.current.animate([
      { transform: `translate(calc(-50% + ${offset}px), calc(-50%))`, opacity: 0.3 },
      { transform: `translate(calc(-50%), calc(-50%))`, opacity: 1 },
    ], { duration: 300, easing: "ease-out" });
    animationRef.current.onfinish = () => { isAnimating.current = false; };
    animationRef.current.oncancel = () => { isAnimating.current = false; };
  }, [pageAnimation]);

  const slideInRef = useRef(slideIn);
  useEffect(() => { slideInRef.current = slideIn; }, [slideIn]);

  // Quantize zoom to nearest 0.25 so we don't re-render on every tiny change
  const renderZoom = Math.ceil(zoom * 4) / 4;
  // Debounce the zoom that drives the BACKEND re-fetch (via renderWidth below):
  // a zoom burst collapses to a single render at the settled width, while
  // applyTransform keeps using the raw `zoom` for instant visual feedback. The
  // state seeds to the current renderZoom on the first render — whatever a
  // restored zoom is — so the cold/initial load is never delayed.
  //
  // A page turn resets zoom to 1 (goTo) and must re-fetch at the new zoom
  // IMMEDIATELY, not lag 200ms behind the pre-turn (larger) width. So when the
  // page changes we snap to the current renderZoom synchronously — during
  // render, not in an effect, so the load effect (which re-runs on the same
  // page change) already sees the correct width and fetches once. Debouncing
  // then applies only to genuine same-page zoom changes.
  //
  // Implemented with React's sanctioned "adjust state during render" pattern:
  // the previous page is held in STATE (zoomPageKey), and when the page changes
  // we call setState during render (only setState — no ref writes or other side
  // effects, so it stays render-pure and StrictMode/concurrent-safe). React
  // discards the in-progress render and retries with the snapped state before
  // committing, so the load effect only ever commits at the correct width.
  const [debouncedRenderZoom, setDebouncedRenderZoom] = useState(renderZoom);
  const [zoomPageKey, setZoomPageKey] = useState(pageIndex);
  const zoomPageChanged = zoomPageKey !== pageIndex;
  if (zoomPageChanged) {
    setZoomPageKey(pageIndex);
    if (debouncedRenderZoom !== renderZoom) setDebouncedRenderZoom(renderZoom);
  }
  // Debounce genuine same-page zoom changes. The timer is local to the effect
  // (no ref), and the cleanup cancels a superseded pending settle when
  // renderZoom changes again or on unmount. On a page turn renderZoom drops to
  // the reset value, so this effect re-runs and cancels any pending settle for
  // the old zoom; the fired callback then sets debouncedRenderZoom to a value
  // the render-phase snap already applied (an Object.is no-op).
  useEffect(() => {
    const t = setTimeout(() => setDebouncedRenderZoom(renderZoom), ZOOM_SETTLE_MS);
    return () => clearTimeout(t);
  }, [renderZoom]);
  // On the page-change render the snapped state hasn't flushed into
  // debouncedRenderZoom yet, so use the current renderZoom directly to keep that
  // (discarded) render internally consistent; the committed retry reads the
  // snapped debouncedRenderZoom, which equals renderZoom anyway.
  const effectiveRenderZoom = zoomPageChanged ? renderZoom : debouncedRenderZoom;

  // Fetches a single page from the backend over the binary wire format
  // and returns a blob URL ready to assign to `<img src>`. The caller
  // owns the URL and must `URL.revokeObjectURL` it when no longer in use
  // (handled here by the cache eviction + unmount paths below).
  const loadPage = useCallback(
    // `cachedOnly` asks the backend to serve the page only if it's already
    // cached; a miss comes back as an empty payload and resolves to `null`
    // (the caller skips it) rather than triggering a cold render.
    async (index: number, renderWidth: number, cachedOnly = false): Promise<string | null> => {
      const command = isPdf ? "get_pdf_page_bytes" : "get_comic_page_bytes";
      // Tauri auto-converts snake_case Rust params to camelCase JS keys.
      // `get_pdf_page_bytes` exposes `width`; `get_comic_page_bytes`
      // exposes `targetWidth`. Both accept zero/undefined as "default".
      const params: Record<string, unknown> = { bookId, pageIndex: index };
      if (renderWidth > 0) {
        if (isPdf) params.width = renderWidth;
        else params.targetWidth = renderWidth;
      }
      if (cachedOnly) params.cachedOnly = true;
      dbg(`invoke ${command} page=${index} width=${renderWidth}${cachedOnly ? " cachedOnly" : ""}`);
      const t0 = performance.now();
      const payload = await invoke<ArrayBuffer>(command, params);
      if (payload.byteLength === 0) {
        // cachedOnly miss: page isn't on disk, nothing to promote.
        dbg(`invoke ${command} page=${index} not cached (skip)`);
        return null;
      }
      const { url, mime } = blobUrlFromBytes(payload);
      dbg(
        `invoke ${command} page=${index} done in ${(performance.now() - t0).toFixed(0)}ms ` +
          `size=${(payload.byteLength / 1024).toFixed(0)}KB mime=${mime}`
      );
      return url;
    },
    [bookId, isPdf]
  );

  // Render-target width. Measured container width × DPR (Retina sharpness) ×
  // zoom, then quantized/clamped by `computeRenderWidth` — see that module for
  // the coarse-quantization (cache-key stability) rationale.
  const dpr = typeof window !== "undefined" ? window.devicePixelRatio || 1 : 1;
  const [containerWidth, setContainerWidth] = useState(0);
  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    setContainerWidth(el.clientWidth);
    const observer = new ResizeObserver((entries) => {
      const next = entries[0]?.contentRect.width ?? 0;
      if (next > 0) setContainerWidth(next);
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, []);
  const renderWidth = computeRenderWidth(containerWidth, {
    dualPage,
    zoom: effectiveRenderZoom,
    dpr,
  });

  // Blob URL cache keyed by `{pageIndex}:{renderWidth}`. URLs evicted
  // here MUST be revoked or the renderer keeps the blob alive
  // indefinitely — a 4 MB page leaks once per page turn otherwise.
  const pageCacheRef = useRef<Map<string, string>>(new Map());
  const inflightRef = useRef<Map<string, Promise<string>>>(new Map());
  // In-flight cached-only preloads, kept separate from `inflightRef` because a
  // preload can resolve to null (page not cached) and must never be handed back
  // as a foreground result. A foreground load coalesces with one of these
  // (awaits it) and only starts a real render if it resolves null.
  const preloadInflightRef = useRef<Map<string, Promise<string | null>>>(new Map());
  // Count of preload renders currently in flight, to cap concurrency
  // (MAX_PRELOAD_INFLIGHT). Foreground page loads are never gated by this.
  const preloadActiveRef = useRef(0);
  // The current preload effect's pump. Freeing a slot must resume the LATEST
  // effect's queue, not the (now-cancelled) closure that started the finishing
  // preload — otherwise a page turn that interrupts saturated preloads leaves
  // the counter pinned and the new settle's preloads never start.
  const preloadPumpRef = useRef<() => void>(() => {});
  // Generation counter — bumps on bookId change/unmount cleanup. Each
  // in-flight `loadPage` snapshots the generation at start; when the
  // promise resolves we compare against the live counter to reject and
  // revoke stale blobs that would otherwise either leak (URL never put
  // into the cache, never revoked) or cross-contaminate the next book
  // by being inserted under a key like `0:1600` that the new book also
  // requests.
  const generationRef = useRef(0);

  const evictUrl = useCallback((key: string) => {
    const url = pageCacheRef.current.get(key);
    if (url) {
      URL.revokeObjectURL(url);
      pageCacheRef.current.delete(key);
    }
  }, []);

  const loadPageCached = useCallback(
    async (index: number, width: number): Promise<string> => {
      const key = `${index}:${width}`;
      const cached = pageCacheRef.current.get(key);
      if (cached) {
        dbg(`frontend cache HIT page=${index}`);
        return cached;
      }
      const inflight = inflightRef.current.get(key);
      if (inflight) {
        dbg(`frontend cache PENDING page=${index}, reusing in-flight request`);
        return inflight;
      }
      // Coalesce with an in-flight cached-only preload of the same page instead
      // of starting a duplicate cache read + resize. If the preload resolves a
      // URL, use it; if it resolves null (not cached) or fails, fall through to
      // a real render below.
      const preloadInflight = preloadInflightRef.current.get(key);
      if (preloadInflight) {
        let preloaded: string | null = null;
        try {
          preloaded = await preloadInflight;
        } catch {
          preloaded = null;
        }
        if (preloaded) {
          dbg(`frontend preload adopted page=${index}`);
          return preloaded;
        }
        // The preload populated the cache or a real fetch started while we
        // awaited — re-check both before starting our own.
        const nowCached = pageCacheRef.current.get(key);
        if (nowCached) return nowCached;
        const nowInflight = inflightRef.current.get(key);
        if (nowInflight) return nowInflight;
      }
      dbg(`frontend cache MISS page=${index}, fetching...`);
      const myGen = generationRef.current;
      const promise = loadPage(index, width)
        .then((url) => {
          // Foreground loads never pass cachedOnly, so a null (not-cached)
          // result is impossible here; guard defensively for the type.
          if (url === null) {
            inflightRef.current.delete(key);
            throw new Error("page load returned no data");
          }
          // Stale: bookId changed (or component unmounted) while the
          // backend was rendering. Revoke immediately so the blob does
          // NOT enter the cache (where the new book might pick it up
          // under the same key) and does NOT leak.
          if (myGen !== generationRef.current) {
            URL.revokeObjectURL(url);
            inflightRef.current.delete(key);
            throw new Error("page load aborted: book changed");
          }
          // If another request for the same key beat us to it (rare —
          // we de-dupe via inflightRef above — but possible across
          // re-renders), drop the duplicate blob to avoid a leak.
          const existing = pageCacheRef.current.get(key);
          if (existing && existing !== url) {
            URL.revokeObjectURL(url);
            inflightRef.current.delete(key);
            return existing;
          }
          pageCacheRef.current.set(key, url);
          inflightRef.current.delete(key);
          while (pageCacheRef.current.size > BLOB_CACHE_MAX) {
            const oldest = pageCacheRef.current.keys().next().value;
            if (oldest === undefined) break;
            evictUrl(oldest);
          }
          return url;
        })
        .catch((err) => {
          inflightRef.current.delete(key);
          throw err;
        });
      inflightRef.current.set(key, promise);
      return promise;
    },
    [loadPage, evictUrl]
  );

  // Preload variant used only for warming neighbor pages: it asks the backend
  // for the page ONLY if it's already cached (cachedOnly), so preloading never
  // triggers a cold render that would contend with the foreground page over a
  // network-mounted library. A not-cached page resolves to null and is skipped.
  // Registered in `preloadInflightRef` (not `inflightRef`) so a foreground load
  // for the same page can coalesce with it, retrying for real only if it
  // resolves null.
  const preloadPageIfCached = useCallback(
    async (index: number, width: number): Promise<void> => {
      const key = `${index}:${width}`;
      if (pageCacheRef.current.has(key)) return; // already warm
      if (inflightRef.current.has(key)) return; // a foreground load is already fetching it
      if (preloadInflightRef.current.has(key)) return; // another preload is already trying
      const myGen = generationRef.current;
      const promise = loadPage(index, width, true).then((url) => {
        if (url === null) return null; // not on disk — leave it to on-demand load
        // Stale (book changed / unmounted) or raced into the cache meanwhile:
        // revoke rather than leak or cross-contaminate the next book.
        if (myGen !== generationRef.current || pageCacheRef.current.has(key)) {
          URL.revokeObjectURL(url);
          return null;
        }
        pageCacheRef.current.set(key, url);
        while (pageCacheRef.current.size > BLOB_CACHE_MAX) {
          const oldest = pageCacheRef.current.keys().next().value;
          if (oldest === undefined) break;
          evictUrl(oldest);
        }
        return url;
      });
      preloadInflightRef.current.set(key, promise);
      try {
        await promise;
      } finally {
        preloadInflightRef.current.delete(key);
      }
    },
    [loadPage, evictUrl]
  );

  // Revoke every cached blob URL when the viewer unmounts or the book
  // switches — otherwise the renderer keeps every page we ever loaded
  // alive in memory.
  useEffect(() => {
    const cacheRef = pageCacheRef;
    return () => {
      // Bump first so any in-flight promises that resolve AFTER this
      // cleanup runs see a stale generation and revoke their blobs
      // instead of leaking or contaminating the next book.
      generationRef.current += 1;
      for (const url of cacheRef.current.values()) {
        URL.revokeObjectURL(url);
      }
      cacheRef.current.clear();
      inflightRef.current.clear();
    };
  }, [bookId]);

  // Identity of the last load the effect ran, so a re-run triggered purely by
  // a width change (zoom settle) can be told apart from a genuine page change,
  // book switch, cold/mount load, or retry. `book` matters because the reader
  // pane is not remounted on navigation (the id changes in place), so switching
  // to a different book at the same page index must still show the spinner.
  // Impossible initial values make the first run count as a real load.
  const prevLoadKeyRef = useRef<{
    book: string;
    left: number;
    right: number | null;
    retry: number;
  }>({
    book: "",
    left: -1,
    right: -1,
    retry: -1,
  });

  // Load spread (one or two pages in parallel) with timeout
  useEffect(() => {
    let cancelled = false;
    let rafId: number | undefined;
    let timeoutId: ReturnType<typeof setTimeout> | undefined;
    // A same-page width-only reload (zoom settle) keeps the current
    // CSS-transformed bitmap visible instead of flashing the spinner over it;
    // the sharper render swaps in when it resolves. Genuine page changes, the
    // cold/mount load, and retries still flip loading true.
    const samePageReload =
      prevLoadKeyRef.current.book === bookId &&
      prevLoadKeyRef.current.left === spread.left &&
      prevLoadKeyRef.current.right === spread.right &&
      prevLoadKeyRef.current.retry === retryCount;
    prevLoadKeyRef.current = {
      book: bookId,
      left: spread.left,
      right: spread.right,
      retry: retryCount,
    };
    if (!samePageReload) setLoading(true);
    setError(null);
    // NOTE: we deliberately do NOT clear leftDisplayedId/rightDisplayedId here.
    // The `imageLayerActive` identity predicate already deactivates the text
    // layer whenever the displayed bitmap's identity doesn't match the target
    // page/book, so an explicit reset is redundant — and harmful: navigating
    // A -> uncached B -> back to A resolves A from the URL cache to the SAME
    // src, which fires no new <img> load event, so a reset would strand
    // displayedId at null and leave A's text layer permanently inert. Keeping
    // the last-loaded identity means the still-visible A bitmap stays
    // selectable.
    //
    // Known limitation (accepted): because a reused <img> element's load
    // handler reads the current identity, an already-decoded page's queued
    // load event could, in a sub-frame window during very rapid multi-page
    // navigation, record the newer page's identity over a not-yet-repainted
    // bitmap. Mis-attribution would require the user to also complete a text
    // selection within that same ~millisecond window, so it is left as a
    // documented edge rather than re-keying the <img> (which would ripple into
    // the shared measure/ResizeObserver/animation logic).

    // Clear stale in-flight entries for pages we no longer need.
    // This prevents abandoned renders from blocking the pdfium queue.
    const keep = new Set<string>();
    keep.add(`${spread.left}:${renderWidth}`);
    if (spread.right !== null) keep.add(`${spread.right}:${renderWidth}`);
    for (const key of inflightRef.current.keys()) {
      if (!keep.has(key)) {
        dbg(`clearing stale inflight: ${key}`);
        inflightRef.current.delete(key);
      }
    }

    // Show "taking longer than expected" after 8s while still waiting
    let slowTimerId: ReturnType<typeof setTimeout> | undefined;

    const loadSpread = async () => {
      dbg(`loadSpread: left=${spread.left} right=${spread.right} retry=${retryCount}`);
      const t0 = performance.now();
      slowTimerId = setTimeout(() => {
        if (!cancelled) setError(t("reader.pageLoadSlow"));
      }, 8000);
      try {
        const timeout = new Promise<never>((_, reject) => {
          timeoutId = setTimeout(() => reject(new Error("timeout")), 30000);
        });
        const promises: Promise<string>[] = [loadPageCached(spread.left, renderWidth)];
        if (spread.right !== null) {
          promises.push(loadPageCached(spread.right, renderWidth));
        }
        const results = await Promise.race([Promise.all(promises), timeout]);
        clearTimeout(timeoutId);
        clearTimeout(slowTimerId);
        dbg(`loadSpread complete in ${(performance.now() - t0).toFixed(0)}ms`);
        if (cancelled) return;
        setError(null);
        // Store each image's identity in lockstep with its blob URL so the
        // text layer can tell which book+page the on-screen bitmap represents.
        setLeftImageData(results[0]);
        setLeftImageId({ bookId, page: spread.left });
        if (results.length > 1 && spread.right !== null) {
          setRightImageData(results[1]);
          setRightImageId({ bookId, page: spread.right });
        } else {
          setRightImageData(null);
          setRightImageId(null);
        }
        // Slide in after new images are set — but only when the
        // page actually changed. A second loadSpread fire for the
        // same page (e.g. cache key churn after a sibling reflow)
        // would otherwise replay the slide animation on a page that
        // is already on screen.
        if (lastAnimatedPageRef.current !== spread.left) {
          lastAnimatedPageRef.current = spread.left;
          rafId = requestAnimationFrame(() => {
            if (!cancelled) slideInRef.current();
          });
        }
      } catch (err) {
        clearTimeout(timeoutId);
        clearTimeout(slowTimerId);
        dbg(`loadSpread FAILED after ${(performance.now() - t0).toFixed(0)}ms:`, err);
        if (!cancelled) {
          const msg = err instanceof Error && err.message === "timeout"
            ? t("reader.pageLoadTimeout")
            : friendlyError(err, t);
          setError(msg);
        }
      } finally {
        if (!cancelled) setLoading(false);
      }
    };

    loadSpread();
    return () => {
      cancelled = true;
      clearTimeout(timeoutId);
      clearTimeout(slowTimerId);
      if (rafId !== undefined) cancelAnimationFrame(rafId);
    };
  }, [spread.left, spread.right, loadPageCached, renderWidth, retryCount]);

  // Preload neighbor pages once the reader settles on the current one.
  //
  // Idle-triggered so it never competes with the foreground turn: a short
  // debounce coalesces a fast flip burst (mid-burst pages are never preloaded),
  // then requestIdleCallback waits for the browser to be free before rendering
  // begins. Targets are ordered forward-first (see computePreloadTargets) and
  // pulled with a concurrency cap so a network-mounted library can't be flooded
  // with parallel renders that starve the page the user is waiting on.
  useEffect(() => {
    if (loading) return;
    let cancelIdle = () => {};
    let cancelled = false;

    const debounceId = setTimeout(() => {
      cancelIdle = scheduleWhenIdle(() => {
        const targets = computePreloadTargets({
          visibleLeft: spread.left,
          visibleRight: dualPage ? spread.right : null,
          totalPages,
          radius: PRELOAD_RADIUS,
        });
        let next = 0;
        const pump = () => {
          while (
            !cancelled &&
            preloadActiveRef.current < MAX_PRELOAD_INFLIGHT &&
            next < targets.length
          ) {
            const idx = targets[next++];
            preloadActiveRef.current++;
            dbg(`preload page=${idx}`);
            preloadPageIfCached(idx, renderWidth)
              .catch(() => {}) // preloads are best-effort
              .finally(() => {
                preloadActiveRef.current--;
                // Resume whichever effect run is current now, not this one.
                preloadPumpRef.current();
              });
          }
        };
        preloadPumpRef.current = pump;
        pump();
      });
    }, PRELOAD_DEBOUNCE_MS);

    return () => {
      cancelled = true;
      clearTimeout(debounceId);
      cancelIdle();
    };
  }, [loading, spread.left, spread.right, dualPage, totalPages, preloadPageIfCached, renderWidth]);

  const goTo = useCallback(
    (index: number) => {
      if (index < 0 || index >= totalPages) return;
      setPageIndex(index);
      onPageChange?.(index);
      // Reset zoom/pan on page change
      setZoom(1);
      panRef.current = { x: 0, y: 0 };
      applyTransform(1, panRef.current);
    },
    [totalPages, onPageChange, applyTransform, setZoom]
  );

  // Navigate by spread: advance to next/prev spread's left page
  const prevSpread = useCallback(() => {
    if (isAnimating.current) return;
    directionRef.current = "left";
    if (dualPage) {
      if (spread.left <= 0) return;
      const prevLeft = spread.left <= 2 ? 0 : spread.left - 2;
      goTo(prevLeft);
    } else {
      goTo(pageIndex - 1);
    }
  }, [dualPage, spread.left, pageIndex, goTo]);

  const nextSpread = useCallback(() => {
    if (isAnimating.current) return;
    directionRef.current = "right";
    if (dualPage) {
      const nextLeft = spread.right !== null ? spread.right + 1 : spread.left + 1;
      if (nextLeft >= totalPages) return;
      goTo(nextLeft);
    } else {
      goTo(pageIndex + 1);
    }
  }, [dualPage, spread, pageIndex, totalPages, goTo]);

  // Keep DOM transform in sync with zoom state (React doesn't manage this inline)
  useEffect(() => {
    applyTransform(zoom, panRef.current);
  }, [zoom, applyTransform]);

  const isAtStart = dualPage ? spread.left <= 0 : pageIndex <= 0;
  const isAtEnd = dualPage
    ? (spread.right !== null ? spread.right >= totalPages - 1 : spread.left >= totalPages - 1)
    : pageIndex >= totalPages - 1;

  const zoomIn = useCallback(() => {
    setZoom((z) => Math.min(MAX_ZOOM, Math.round((z + ZOOM_STEP) * 100) / 100));
  }, []);
  const zoomOut = useCallback(() => {
    setZoom((z) => {
      const next = Math.max(MIN_ZOOM, Math.round((z - ZOOM_STEP) * 100) / 100);
      if (next <= 1) {
        panRef.current = { x: 0, y: 0 };
        applyTransform(next, panRef.current);
      }
      return next;
    });
  }, [applyTransform]);
  const zoomReset = useCallback(() => {
    panRef.current = { x: 0, y: 0 };
    setZoom(1);
    applyTransform(1, panRef.current);
  }, [applyTransform]);

  useEffect(() => {
    function handleKeyDown(e: KeyboardEvent) {
      const tag = (e.target as HTMLElement)?.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA") return;

      // Alt+Arrow is reserved for the Reader's navigation history; don't
      // also consume it as page-prev/next.
      if (e.altKey && (e.key === "ArrowLeft" || e.key === "ArrowRight")) return;

      if (e.key === "ArrowLeft") mangaMode ? nextSpread() : prevSpread();
      else if (e.key === "ArrowRight") mangaMode ? prevSpread() : nextSpread();
      else if ((e.key === "=" || e.key === "+") && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        zoomIn();
      } else if (e.key === "-" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        zoomOut();
      } else if (e.key === "0" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        zoomReset();
      }
    }
    if (!keyboardEnabled) return;
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [prevSpread, nextSpread, zoomIn, zoomOut, zoomReset, mangaMode, keyboardEnabled]);

  const wheelCooldown = useRef(false);
  const handleWheel = useCallback(
    (e: React.WheelEvent) => {
      if (e.ctrlKey || e.metaKey) {
        e.preventDefault();
        if (e.deltaY < 0) zoomIn();
        else zoomOut();
        return;
      }
      if (canPan()) return;
      if (wheelCooldown.current || loading) return;
      if (Math.abs(e.deltaY) < 10) return;
      wheelCooldown.current = true;
      if (e.deltaY > 0) nextSpread();
      else prevSpread();
      setTimeout(() => { wheelCooldown.current = false; }, 300);
    },
    [nextSpread, prevSpread, loading, zoomIn, zoomOut, zoom]
  );

  // Measure content vs container to determine pan overflow
  const getOverflow = useCallback(() => {
    if (!containerRef.current) return { x: 0, y: 0 };
    const containerW = containerRef.current.clientWidth;
    const containerH = containerRef.current.clientHeight;
    // Spread is physically zoom-sized (width/height %), no CSS scale
    const contentW = containerW * zoom;
    const contentH = containerH * zoom;
    return {
      x: Math.max(0, contentW - containerW),
      y: Math.max(0, contentH - containerH),
    };
  }, [zoom]);

  const canPan = useCallback(() => {
    const overflow = getOverflow();
    return overflow.x > 1 || overflow.y > 1;
  }, [getOverflow]);

  // Clamp pan so content can't be dragged beyond its edges
  const clampPan = useCallback((p: { x: number; y: number }): { x: number; y: number } => {
    const overflow = getOverflow();
    const maxPanX = overflow.x / 2;
    const maxPanY = overflow.y / 2;
    return {
      x: Math.max(-maxPanX, Math.min(maxPanX, p.x)),
      y: Math.max(-maxPanY, Math.min(maxPanY, p.y)),
    };
  }, [getOverflow]);

  const handleMouseDown = useCallback(
    (e: React.MouseEvent) => {
      // Don't start a pan (or preventDefault) when the press lands on a
      // selectable glyph span or the selection popup — that would cancel
      // native text selection at zoom > 1. Presses on the text layer's blank
      // background (between glyphs) still fall through to panning, so a
      // zoomed page can be panned from any empty area.
      if ((e.target as HTMLElement).closest("[data-off], [data-pdf-selection-popup]")) return;
      if (!canPan()) return;
      // Collapse any active text selection before the pan's preventDefault
      // freezes it — otherwise the native selection survives the pan and the
      // document mouseup re-runs handleMouseUp, resurrecting the old popup.
      window.getSelection()?.removeAllRanges();
      e.preventDefault();
      isPanning.current = true;
      panStart.current = { x: e.clientX, y: e.clientY };
      panOffset.current = { ...panRef.current };
    },
    [canPan]
  );

  const handleMouseMove = useCallback(
    (e: React.MouseEvent) => {
      if (!isPanning.current) return;
      const dx = e.clientX - panStart.current.x;
      const dy = e.clientY - panStart.current.y;
      const raw = {
        x: panOffset.current.x + dx,
        y: panOffset.current.y + dy,
      };
      panRef.current = clampPan(raw);
      applyTransform(zoom, panRef.current);
    },
    [zoom, applyTransform, clampPan]
  );

  const handleMouseUp = useCallback(() => {
    isPanning.current = false;
  }, []);

  const [editingPage, setEditingPage] = useState(false);
  const [pageInput, setPageInput] = useState("");
  const pageInputRef = useRef<HTMLInputElement>(null);

  const handlePageLabelClick = useCallback(() => {
    setPageInput(String(spread.left + 1));
    setEditingPage(true);
  }, [spread.left]);

  const handlePageInputSubmit = useCallback(() => {
    const num = parseInt(pageInput, 10);
    if (!isNaN(num) && num >= 1 && num <= totalPages) {
      directionRef.current = "right";
      const target = num - 1;
      // Fire the jump notification *before* goTo so the listener can read the
      // pre-jump page index for history's source entry.
      if (target !== pageIndex) onPageJump?.(target);
      goTo(target);
    }
    setEditingPage(false);
  }, [pageInput, totalPages, pageIndex, goTo, onPageJump]);

  useEffect(() => {
    if (editingPage && pageInputRef.current) {
      pageInputRef.current.select();
    }
  }, [editingPage]);

  const pageLabel = dualPage && spread.right !== null
    ? `${t("reader.pages")} ${spread.left + 1}–${spread.right + 1} / ${totalPages}`
    : `${t("reader.page")} ${spread.left + 1} / ${totalPages}`;

  return (
    <div className="flex flex-col flex-1 min-h-0 bg-paper">
      {/* Page image area */}
      <div
        ref={containerRef}
        className={`flex-1 relative overflow-hidden ${canPan() ? "cursor-grab active:cursor-grabbing" : ""}`}
        onWheel={handleWheel}
        onMouseDown={handleMouseDown}
        onMouseMove={handleMouseMove}
        onMouseUp={handleMouseUp}
        onMouseLeave={handleMouseUp}
      >
        {/* Always render spread so spreadRef stays mounted for animations */}
        <div
          ref={spreadRef}
          className={`absolute top-1/2 left-1/2 flex items-center justify-center gap-1 will-change-transform ${mangaMode && dualPage ? "flex-row-reverse" : "flex-row"}`}
          style={{ width: `${zoom * 100}%`, height: `${zoom * 100}%`, transform: `translate(calc(-50% + ${panRef.current.x}px), calc(-50% + ${panRef.current.y}px))` }}
        >
          {/* PDF pages wrap the image so the selectable text layer (F-1-4)
              can overlay the object-contain rendered box. Comics keep the
              bare-image markup unchanged — no text layer, no layout shift. */}
          {leftImageData && (
            isPdf ? (
              <div className="relative flex-1 min-w-0 h-full flex items-center justify-center">
                <img
                  ref={leftImgRef}
                  src={leftImageData}
                  alt={`Page ${spread.left + 1} of ${totalPages}`}
                  className="max-h-full max-w-full object-contain rounded-sm shadow-[0_4px_24px_-4px_rgba(44,34,24,0.18)]"
                  draggable={false}
                  onLoad={() => setLeftDisplayedId(leftImageIdRef.current)}
                  onError={() =>
                    setError(t("reader.failedToLoadPage", { error: t("reader.imageDecodeError") }))
                  }
                />
                <PdfTextLayer
                  bookId={bookId}
                  pageIndex={spread.left}
                  displayedId={leftDisplayedId}
                  imgRef={leftImgRef}
                  containerRef={containerRef}
                  refreshKey={highlightRefreshKey}
                  onCreateHighlight={onCreateHighlight}
                  onRemoveHighlights={onRemoveHighlights}
                />
              </div>
            ) : (
              <img
                src={leftImageData}
                alt={`Page ${spread.left + 1} of ${totalPages}`}
                className="max-h-full max-w-full object-contain rounded-sm shadow-[0_4px_24px_-4px_rgba(44,34,24,0.18)]"
                style={dualPage && rightImageData ? { maxWidth: "50%" } : undefined}
                draggable={false}
                onError={() =>
                  setError(t("reader.failedToLoadPage", { error: t("reader.imageDecodeError") }))
                }
              />
            )
          )}
          {rightImageData && spread.right !== null && (
            isPdf ? (
              <div className="relative flex-1 min-w-0 h-full flex items-center justify-center">
                <img
                  ref={rightImgRef}
                  src={rightImageData}
                  alt={`Page ${(spread.right ?? 0) + 1} of ${totalPages}`}
                  className="max-h-full max-w-full object-contain rounded-sm shadow-[0_4px_24px_-4px_rgba(44,34,24,0.18)]"
                  draggable={false}
                  onLoad={() => setRightDisplayedId(rightImageIdRef.current)}
                  onError={() =>
                    setError(t("reader.failedToLoadPage", { error: t("reader.imageDecodeError") }))
                  }
                />
                <PdfTextLayer
                  bookId={bookId}
                  pageIndex={spread.right}
                  displayedId={rightDisplayedId}
                  imgRef={rightImgRef}
                  containerRef={containerRef}
                  refreshKey={highlightRefreshKey}
                  onCreateHighlight={onCreateHighlight}
                  onRemoveHighlights={onRemoveHighlights}
                />
              </div>
            ) : (
              <img
                src={rightImageData}
                alt={`Page ${(spread.right ?? 0) + 1} of ${totalPages}`}
                className="max-h-full object-contain rounded-sm shadow-[0_4px_24px_-4px_rgba(44,34,24,0.18)]"
                style={{ maxWidth: "50%" }}
                draggable={false}
                onError={() =>
                  setError(t("reader.failedToLoadPage", { error: t("reader.imageDecodeError") }))
                }
              />
            )
          )}
        </div>
        {/* Overlay: loading spinner or error on top of previous/current page */}
        {loading && (
          <div className="absolute inset-0 flex flex-col items-center justify-center gap-2 bg-paper/80">
            <div className="flex items-center gap-2">
              <div className="w-5 h-5 border-2 border-accent/30 border-t-accent rounded-full animate-spin" />
              <span className="text-sm text-ink-muted">{t("reader.loadingPage")}</span>
            </div>
            {error && (
              <span className="text-xs text-ink-muted/70 animate-fade-in">{error}</span>
            )}
          </div>
        )}
        {!loading && error && (
          <div className="absolute inset-0 flex flex-col items-center justify-center gap-3 bg-paper/80">
            <span className="text-sm text-red-500 text-center max-w-sm">{error}</span>
            <button
              onClick={() => {
                // Evict from cache and in-flight so retry fetches fresh.
                // Must go through evictUrl so the blob URL is revoked —
                // a raw cache delete would leak the successfully-loaded
                // page (e.g. left page in a dual-page spread when right
                // failed).
                const key = `${spread.left}:${renderWidth}`;
                evictUrl(key);
                inflightRef.current.delete(key);
                if (spread.right !== null) {
                  const rkey = `${spread.right}:${renderWidth}`;
                  evictUrl(rkey);
                  inflightRef.current.delete(rkey);
                }
                setRetryCount((c) => c + 1);
              }}
              className="px-4 py-1.5 text-sm bg-accent text-white rounded-lg hover:bg-accent/90 transition-colors"
            >
              {t("reader.retryLoadPage")}
            </button>
          </div>
        )}
      </div>

      {/* Screen reader announcement for page changes */}
      <div aria-live="polite" aria-atomic="true" className="sr-only">
        {!loading && t("reader.pageOf", { current: pageIndex + 1, total: totalPages })}
      </div>

      {/* Navigation bar */}
      <div className="shrink-0 border-t border-warm-border bg-surface px-5 py-3 flex items-center gap-3">
        <button
          onClick={prevSpread}
          disabled={isAtStart}
          className="flex items-center gap-1.5 px-4 py-1.5 text-sm text-ink-muted bg-warm-subtle hover:bg-warm-border rounded-xl transition-colors disabled:opacity-30 disabled:cursor-not-allowed"
          aria-label="Previous page"
        >
          <svg width="14" height="14" viewBox="0 0 20 20" fill="none">
            <path d="M12 4l-6 6 6 6" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
          {t("common.previous")}
        </button>

        {editingPage ? (
          <form
            className="flex-1 flex items-center justify-center gap-1.5"
            onSubmit={(e) => { e.preventDefault(); handlePageInputSubmit(); }}
          >
            <input
              ref={pageInputRef}
              type="number"
              min={1}
              max={totalPages}
              value={pageInput}
              onChange={(e) => setPageInput(e.target.value)}
              onBlur={() => setEditingPage(false)}
              onKeyDown={(e) => { if (e.key === "Escape") setEditingPage(false); }}
              className="w-14 text-center text-xs tabular-nums bg-warm-subtle text-ink border border-warm-border rounded-lg px-2 py-1 focus:outline-none focus:ring-1 focus:ring-accent"
              aria-label={t("reader.goToPage")}
            />
            <span className="text-xs text-ink-muted tabular-nums">/ {totalPages}</span>
          </form>
        ) : (
          <button
            type="button"
            onClick={handlePageLabelClick}
            className="flex-1 text-center text-xs text-ink-muted tabular-nums hover:text-ink transition-colors cursor-pointer"
            title={t("reader.goToPage")}
          >
            {pageLabel}
          </button>
        )}

        {/* Zoom controls */}
        <div className="flex items-center gap-1">
          <button
            onClick={zoomOut}
            disabled={zoom <= MIN_ZOOM}
            className="w-7 h-7 flex items-center justify-center text-xs text-ink-muted hover:text-ink bg-warm-subtle hover:bg-warm-border rounded-lg transition-colors disabled:opacity-30"
            aria-label="Zoom out"
          >
            −
          </button>
          <button
            onClick={zoomReset}
            className={`px-2 h-7 text-[11px] tabular-nums rounded-lg transition-colors ${zoom !== 1 ? "text-accent bg-accent-light hover:bg-accent-light/80 font-medium" : "text-ink-muted bg-warm-subtle"} ${zoomRestored ? "animate-fade-in ring-1 ring-accent" : ""}`}
            title="Reset zoom"
          >
            {Math.round(zoom * 100)}%
          </button>
          <button
            onClick={zoomIn}
            disabled={zoom >= MAX_ZOOM}
            className="w-7 h-7 flex items-center justify-center text-xs text-ink-muted hover:text-ink bg-warm-subtle hover:bg-warm-border rounded-lg transition-colors disabled:opacity-30"
            aria-label="Zoom in"
          >
            +
          </button>
        </div>

        <button
          onClick={nextSpread}
          disabled={isAtEnd}
          className="flex items-center gap-1.5 px-4 py-1.5 text-sm text-ink-muted bg-warm-subtle hover:bg-warm-border rounded-xl transition-colors disabled:opacity-30 disabled:cursor-not-allowed"
          aria-label="Next page"
        >
          {t("common.next")}
          <svg width="14" height="14" viewBox="0 0 20 20" fill="none">
            <path d="M8 4l6 6-6 6" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        </button>
      </div>
    </div>
  );
}

interface SavedHighlight {
  id: string;
  startOffset: number;
  endOffset: number;
  color: string;
}

interface PdfTextLayerProps {
  bookId: string;
  pageIndex: number;
  /** Identity of the bitmap the paired <img> currently displays (set on its
   *  onLoad from the src it represents). The layer is inert until this matches
   *  this layer's book+page, so a late load for a superseded page or a
   *  same-index book switch can't make stale text selectable. */
  displayedId: ImageId | null;
  imgRef: RefObject<HTMLImageElement | null>;
  /** The reader's visible (overflow-hidden) viewport box, for popup clamping. */
  containerRef: RefObject<HTMLDivElement | null>;
  refreshKey: number;
  onCreateHighlight?: (color: string, sel: PdfSelection) => Promise<unknown> | void;
  onRemoveHighlights?: (ids: string[]) => Promise<void> | void;
}

/**
 * Transparent, selectable text layer over one rendered PDF page image
 * (F-1-4, desktop reader). Fetches the page's glyph rects + full text +
 * saved highlights, then overlays:
 *   - colored highlight bands (below, non-interactive), and
 *   - one transparent `<span>` per glyph carrying its real character so the
 *     browser's native selection + Cmd/Ctrl+C copies real text.
 * On selection it surfaces a copy/highlight popup wired to ReaderPane's
 * handlers. Glyph `off` values are Unicode-scalar (Rust `char`) offsets into
 * the page text, so the text is indexed via `Array.from` (code points), never
 * `pageText[off]` — see `get_pdf_page_text`'s doc comment on the backend.
 */
function PdfTextLayer({
  bookId,
  pageIndex,
  displayedId,
  imgRef,
  containerRef,
  refreshKey,
  onCreateHighlight,
  onRemoveHighlights,
}: PdfTextLayerProps) {
  const { t } = useTranslation();
  const { addToast } = useToast();
  const layerRef = useRef<HTMLDivElement>(null);
  const popupRef = useRef<HTMLDivElement>(null);
  const [glyphs, setGlyphs] = useState<Glyph[]>([]);
  const [chars, setChars] = useState<string[]>([]);
  const [saved, setSaved] = useState<SavedHighlight[]>([]);
  const [box, setBox] = useState<{ left: number; top: number; width: number; height: number } | null>(null);
  // Popup anchor is stored in VIEWPORT coords (from the selection's
  // getBoundingClientRect); final on-screen position is clamped to the visible
  // container in a layout effect below, so it can never be clipped by the
  // reader's overflow-hidden box or displaced by the panned/zoomed spread's
  // transform.
  const [popup, setPopup] = useState<
    { anchorX: number; anchorTop: number; anchorBottom: number; sel: PdfSelection; overlapIds: string[] } | null
  >(null);
  const [popupPos, setPopupPos] = useState<{ left: number; top: number } | null>(null);

  // The image ON SCREEN must be exactly this book+page and measured before the
  // layer becomes interactive (F-1-4 page-turn race): otherwise a late load for
  // a superseded page, or a same-index book switch, could pair current-page
  // text with a stale bitmap.
  const active = imageLayerActive(displayedId, bookId, pageIndex, box !== null);

  // Drop the previous page's glyphs/text/highlights/popup/box SYNCHRONOUSLY the
  // instant the page (or book) changes — before paint — so the freshly
  // rendered image never becomes interactive with stale data or a stale
  // measurement still live. `box` is reset here and only re-measured after the
  // new image's load fires, which also records the on-screen bitmap's identity.
  useLayoutEffect(() => {
    setGlyphs([]);
    setChars([]);
    setSaved([]);
    setPopup(null);
    setBox(null);
  }, [bookId, pageIndex]);

  // Glyph rects + page text — refetched per page (backend memory-cached).
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const [g, text] = await Promise.all([
          invoke<Glyph[]>("get_pdf_page_glyphs", { bookId, pageIndex }),
          invoke<string>("get_pdf_page_text", { bookId, pageIndex }),
        ]);
        if (cancelled) return;
        setGlyphs(g);
        setChars(Array.from(text));
      } catch {
        if (!cancelled) {
          setGlyphs([]);
          setChars([]);
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [bookId, pageIndex]);

  // Saved highlights — also refetched when a create/remove elsewhere bumps
  // `refreshKey` so bands stay in sync after mutations (including undo).
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const hls = await invoke<SavedHighlight[]>("get_chapter_highlights", {
          bookId,
          chapterIndex: pageIndex,
        });
        if (!cancelled) setSaved(hls);
      } catch {
        if (!cancelled) setSaved([]);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [bookId, pageIndex, refreshKey]);

  // Measure the image's object-contain rendered box (the letterboxed content
  // rect, not the element box) so glyph rects map onto the visible pixels.
  const measure = useCallback(() => {
    const img = imgRef.current;
    if (!img) return;
    const nw = img.naturalWidth;
    const nh = img.naturalHeight;
    const cw = img.clientWidth;
    const ch = img.clientHeight;
    if (!nw || !nh || !cw || !ch) {
      setBox(null);
      return;
    }
    const scale = Math.min(cw / nw, ch / nh);
    const width = nw * scale;
    const height = nh * scale;
    setBox({
      left: img.offsetLeft + (cw - width) / 2,
      top: img.offsetTop + (ch - height) / 2,
      width,
      height,
    });
  }, [imgRef]);

  useEffect(() => {
    const img = imgRef.current;
    if (!img) return;
    measure();
    const ro = new ResizeObserver(() => measure());
    ro.observe(img);
    img.addEventListener("load", measure);
    window.addEventListener("resize", measure);
    return () => {
      ro.disconnect();
      img.removeEventListener("load", measure);
      window.removeEventListener("resize", measure);
    };
  }, [imgRef, measure]);

  // Re-measure once new-page glyphs arrive (the image src has changed and its
  // natural dimensions may only now be available).
  useEffect(() => {
    measure();
  }, [glyphs, measure]);

  // Selection → popup. A document-level mouseup lets a drag that ends outside
  // the layer still resolve. A selection is only claimed when BOTH endpoints
  // are inside this layer, so a two-page spread never produces two popups and
  // native copy can never disagree with the popup's sliced text.
  useEffect(() => {
    const layer = layerRef.current;
    if (!layer) return;
    const glyphByOff = new Map(glyphs.map((g) => [g.off, g]));

    function handleMouseUp() {
      if (!active) return;
      const sel = window.getSelection();
      if (!sel || sel.isCollapsed || !sel.anchorNode) return;
      if (!layer || !layer.contains(sel.anchorNode)) return;
      // Anchor is in this layer but the focus ended in another page's layer:
      // a cross-page drag. Native Cmd/Ctrl+C would copy both pages while the
      // popup slices only this page — so clear the selection and show nothing.
      if (!sel.focusNode || !layer.contains(sel.focusNode)) {
        sel.removeAllRanges();
        return;
      }
      const picked: Glyph[] = [];
      for (const off of selectedOffsets(sel.getRangeAt(0), layer.querySelectorAll<HTMLElement>("[data-off]"))) {
        const g = glyphByOff.get(off);
        if (g) picked.push(g);
      }
      const offs = selectionOffsets(picked);
      if (!offs) return;
      const text = chars.slice(offs.startOffset, offs.endOffset).join("");
      if (!text) return;
      const rangeRect = sel.getRangeAt(0).getBoundingClientRect();
      const overlapIds = saved
        .filter((h) => h.startOffset < offs.endOffset && h.endOffset > offs.startOffset)
        .map((h) => h.id);
      setPopup({
        anchorX: rangeRect.left + rangeRect.width / 2,
        anchorTop: rangeRect.top,
        anchorBottom: rangeRect.bottom,
        sel: { text, startOffset: offs.startOffset, endOffset: offs.endOffset, pageIndex },
        overlapIds,
      });
    }

    function handleMouseDown(e: MouseEvent) {
      const target = e.target as HTMLElement;
      if (!target.closest("[data-pdf-selection-popup]")) setPopup(null);
    }

    document.addEventListener("mouseup", handleMouseUp);
    document.addEventListener("mousedown", handleMouseDown);
    return () => {
      document.removeEventListener("mouseup", handleMouseUp);
      document.removeEventListener("mousedown", handleMouseDown);
    };
  }, [glyphs, chars, saved, pageIndex, active]);

  const dismiss = useCallback(() => {
    setPopup(null);
    window.getSelection()?.removeAllRanges();
  }, []);

  const handleCopy = useCallback(
    async (text: string) => {
      try {
        await navigator.clipboard.writeText(text);
        addToast(t("reader.copied"), "success");
      } catch {
        addToast(t("reader.copyFailed"), "error");
      }
      dismiss();
    },
    [addToast, t, dismiss],
  );

  const handleHighlight = useCallback(
    async (color: string) => {
      if (!popup) return;
      // Dismiss only on success — if the create IPC failed (its toast already
      // told the user), keep the popup + selection so they can retry.
      try {
        await onCreateHighlight?.(color, popup.sel);
        dismiss();
      } catch {
        /* keep popup for retry */
      }
    },
    [popup, onCreateHighlight, dismiss],
  );

  const handleClear = useCallback(async () => {
    if (!popup || popup.overlapIds.length === 0) return;
    try {
      await onRemoveHighlights?.(popup.overlapIds);
      dismiss();
    } catch {
      /* keep popup for retry */
    }
  }, [popup, onRemoveHighlights, dismiss]);

  // Clamp the popup into the visible container, in viewport coords. Runs after
  // the popup mounts (so its measured width/height are known) and re-runs on
  // scroll/resize. Rendered via a portal to <body> at position:fixed so it
  // escapes the reader's overflow-hidden box AND the spread's transform.
  useLayoutEffect(() => {
    if (!popup) {
      setPopupPos(null);
      return;
    }
    function place() {
      const el = popupRef.current;
      const container = containerRef.current;
      if (!el || !container || !popup) return;
      const c = container.getBoundingClientRect();
      const w = el.offsetWidth;
      const h = el.offsetHeight;
      const gap = 8;
      const left = Math.max(c.left + gap, Math.min(popup.anchorX - w / 2, c.right - w - gap));
      // Prefer above the selection; drop below when there isn't room above.
      const roomAbove = popup.anchorTop - c.top;
      const rawTop = roomAbove >= h + gap ? popup.anchorTop - gap - h : popup.anchorBottom + gap;
      const top = Math.max(c.top + gap, Math.min(rawTop, c.bottom - h - gap));
      setPopupPos({ left, top });
    }
    place();
    window.addEventListener("resize", place);
    window.addEventListener("scroll", place, true);
    return () => {
      window.removeEventListener("resize", place);
      window.removeEventListener("scroll", place, true);
    };
  }, [popup, containerRef]);

  if (!active || !box) return null;

  return (
    <>
      <div
        ref={layerRef}
        data-pdf-text-layer
        className="absolute select-text"
        style={{ left: box.left, top: box.top, width: box.width, height: box.height }}
      >
        {/* Saved-highlight bands — below the spans, non-interactive so
            selection still hits the text layer. */}
        {saved.flatMap((h) =>
          highlightBands(glyphs, h.startOffset, h.endOffset, box.width, box.height).map((b, i) => (
            <div
              key={`${h.id}-${i}`}
              className="absolute pointer-events-none rounded-[1px]"
              style={{
                left: b.left,
                top: b.top,
                width: b.width,
                height: b.height,
                backgroundColor: `${h.color}66`,
              }}
            />
          )),
        )}
        {/* Transparent selectable glyph spans (every glyph, so native copy
            keeps spaces/punctuation in reading order). */}
        {glyphs.map((g) => {
          const r = glyphToPx(g, box.width, box.height);
          return (
            <span
              key={g.off}
              data-off={g.off}
              className="absolute overflow-hidden"
              style={{
                left: r.left,
                top: r.top,
                width: r.width,
                height: r.height,
                color: "transparent",
                cursor: "text",
                lineHeight: 1,
                whiteSpace: "pre",
                fontSize: r.height || 1,
              }}
            >
              {chars[g.off] ?? ""}
            </span>
          );
        })}
      </div>

      {popup && createPortal(
        <div
          ref={popupRef}
          data-pdf-selection-popup
          className="fixed z-50 flex items-center gap-1 px-2 py-1.5 bg-ink/90 backdrop-blur-sm rounded-lg shadow-lg"
          style={{
            left: `${popupPos?.left ?? 0}px`,
            top: `${popupPos?.top ?? 0}px`,
            // Hidden for the first layout pass (before its size is measured),
            // then clamped into the visible container and shown.
            visibility: popupPos ? "visible" : "hidden",
          }}
        >
          {popup.sel.text.length >= 3 &&
            HIGHLIGHT_COLORS.map((c) => (
              <button
                key={c.value}
                onClick={() => handleHighlight(c.value)}
                className="w-5 h-5 rounded-full hover:scale-125 transition-transform"
                style={{ backgroundColor: c.value }}
                aria-label={t("reader.highlightColor", { color: c.name })}
              />
            ))}
          {popup.overlapIds.length > 0 && (
            <button
              onClick={handleClear}
              className="w-5 h-5 rounded-full hover:scale-125 transition-transform border border-white/40 flex items-center justify-center"
              style={{ background: "repeating-conic-gradient(#ccc 0% 25%, transparent 0% 50%) 50% / 6px 6px" }}
              aria-label={t("reader.clearHighlight")}
              title={t("reader.clearHighlight")}
            />
          )}
          <button
            onClick={() => handleCopy(popup.sel.text)}
            className="px-2 h-5 rounded text-xs font-medium text-white/90 hover:text-white hover:bg-white/10 transition-colors"
          >
            {t("reader.copySelection")}
          </button>
          <div className="w-px h-4 bg-white/20 mx-0.5" />
          <button
            onClick={dismiss}
            className="w-5 h-5 rounded-full hover:scale-125 transition-transform flex items-center justify-center text-white/60 hover:text-white"
            aria-label={t("reader.dismiss")}
          >
            <svg width="12" height="12" viewBox="0 0 16 16" fill="none">
              <path d="M12 4L4 12M4 4l8 8" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
            </svg>
          </button>
        </div>,
        document.body,
      )}
    </>
  );
}
