import { useState, useEffect, useCallback, useRef, useMemo, useReducer } from "react";
import { useNavigate } from "react-router-dom";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useTranslation } from "react-i18next";
import { useTheme, MIN_FONT_SIZE, MAX_FONT_SIZE } from "../context/ThemeContext";
import PageViewer from "./PageViewer";
import ChapterErrorCard from "./ChapterErrorCard";
import ChapterLoadProgress from "./ChapterLoadProgress";
import ComicExtractProgress from "./ComicExtractProgress";
import PageThumbnailStrip from "./PageThumbnailStrip";
import KeyboardShortcutsHelp from "./KeyboardShortcutsHelp";
import HighlightsPanel, { HIGHLIGHT_COLORS } from "./HighlightsPanel";
import ShareCardDialog from "./ShareCardDialog";
import BookmarksPanel from "./BookmarksPanel";
import BookmarkToast from "./BookmarkToast";
import BookDetailModal from "./BookDetailModal";
import type { Book } from "../types";
import BookCompletionModal from "./BookCompletionModal";
import MissingFileDialog from "./MissingFileDialog";
import { useBookCompletion } from "../hooks/useBookCompletion";
import LanguageSwitcher from "./LanguageSwitcher";
import PrivateModeToggle from "./PrivateModeToggle";
import OverflowMenu from "./OverflowMenu";
import ReaderSkeleton from "./ReaderSkeleton";
import { usePrivateMode } from "../hooks/usePrivateMode";
import { getVolatilePosition, setVolatilePosition } from "../lib/volatileResume";
import { sanitizeChapterHtml } from "../lib/sanitizeHtml";
import { openUrl } from "@tauri-apps/plugin-opener";
import { friendlyError, isBookFileMissing, toFolioError } from "../lib/errors";
import { extractLookupWord, groupSensesByPos, POS_LABEL_KEYS, type DictionaryEntry, type DictionaryStatus } from "../lib/dictionary";
import { extractContextSentence, formatDefinitionSnapshot, primarySense } from "../lib/vocabulary";
import { useToast } from "./Toast";
import { resolveBookmarkScrollTop, isExternalUrl } from "../lib/utils";
import {
  createChapterCache,
  getCachedChapter,
  setCachedChapter,
  evictOutsideWindow,
  adjacentChapterIndices,
  type ChapterCache,
} from "../lib/chapterCache";
import {
  emptyHistory,
  pushEntry,
  replaceCurrent,
  goBack as historyGoBack,
  goForward as historyGoForward,
  canGoBack,
  canGoForward,
  type NavigationEntry,
  type NavigationHistory,
} from "../lib/navigationHistory";
import {
  initialComicExtractProgress,
  comicExtractProgressReducer,
  isComicExtractProgressVisible,
} from "../lib/comicExtractProgress";
import { resolveJumpTokenGate } from "../lib/jumpTokenGate";

// ---- Types matching Rust backend ----

interface TocEntry {
  label: string;
  chapter_index: number;
  play_order: string;
  children: TocEntry[];
}

interface ReadingProgress {
  book_id: string;
  chapter_index: number;
  scroll_position: number;
  last_read_at: number;
}

// ---- Component ----
//
// `ReaderPane` owns the per-book reading state (chapter index, scroll
// progress, navigation history, bookmarks, search, etc.) and the
// content-area JSX. It receives `bookId` as a prop rather than reading
// the route so the surrounding Reader screen can mount more than one
// pane (for the upcoming split-view feature — see ROADMAP #40).
//
// Global concerns that are not yet per-pane (settings panel, focus
// mode, keyboard shortcuts help) still live inside this component for
// now and are scheduled to move up to the Reader shell in a follow-up
// milestone. Today the Reader screen mounts exactly one ReaderPane, so
// behavior is identical to before the extraction.

// Idle window after the last `pdf-prerender-progress` event before the shared
// bar is `settle`d hidden (F-4-5). Comfortably larger than the gap between the
// backend's ~1%-throttled events so it fires only after the pass truly ends
// (incl. an honest partial-coverage terminal that settles below 100%); a rare
// false mid-pass settle is benign (see the PDF prerender listener effect).
const PDF_PRERENDER_SETTLE_MS = 4000;

interface ReaderPaneProps {
  bookId: string;
  onOpenSettings: () => void;
  settingsOpen?: boolean;
  /** True when two panes are mounted side-by-side. Defaults to false. */
  splitMode?: boolean;
  /**
   * True for the left pane (or the single pane when split is off).
   * Only the primary pane owns the global keyboard listeners so split
   * mode does not double-fire navigation on every arrow key. The
   * companion pane reuses props but skips window-level event wiring.
   */
  isPrimary?: boolean;
  /**
   * Whether keyboard navigation + the split-toggle hotkey should fire
   * on this pane. Defaults to `isPrimary` so the single-pane case
   * works unchanged. In split mode the Reader shell tracks which pane
   * is "active" and routes keyboard handling to it via this prop.
   */
  isActive?: boolean;
  /**
   * Whether this pane is allowed to write reading progress + sync to
   * the backend on its own. Defaults to `isPrimary`. In split mode
   * the Reader shell sets this to `true` for both panes as long as
   * their bookIds differ (so each pane writes only its own book's
   * progress); when both panes show the same book it stays
   * primary-only to avoid two panes racing on the same DB row.
   */
  canPersist?: boolean;
  /** Fires on mousedown anywhere inside the pane root. Reader shell
   *  uses it to mark the pane "active" for keyboard routing. */
  onActivate?: () => void;
  /** Toggle split mode on/off — supplied by the Reader shell. */
  onToggleSplit?: () => void;
  /** Swap primary / companion bookIds. Wired on the primary pane in
   *  split mode when a companion has been picked. */
  onSwapPanes?: () => void;
  /**
   * When set, the pane renders a "Choose another book" header button
   * that fires this callback. Only the Reader shell wires this — it
   * passes the callback to the companion pane in split mode so the
   * user can pick a different book for that pane.
   */
  onChangeBook?: () => void;
  /**
   * When set, the pane renders an X close button in its header that
   * fires this callback. The Reader shell wires it on the companion
   * pane in split mode so the user can collapse back to single-pane
   * view without having to find the split-toggle button.
   */
  onCloseThisPane?: () => void;
  initialChapterIndex?: number;
  /**
   * Character offset into the target chapter's plain text to precisely
   * scroll to on first open (e.g. a vocabulary-word "jump to source").
   * Consumed once alongside `initialChapterIndex` — `null`/`undefined`
   * degrades to the current chapter-only behavior (no scroll-to-offset,
   * no flash).
   */
  initialScrollOffset?: number | null;
  /**
   * Identity of the current jump request (F-1-5 vocabulary "jump to
   * source"). Reader.tsx keeps this pane mounted across same-book
   * navigations, so `bookId` alone can't signal "a new jump arrived while
   * already reading this book" — `jumpToken` changes on every distinct
   * navigation (Reader.tsx derives it from `location.key`) even when
   * `initialChapterIndex`/`initialScrollOffset` repeat a previous value.
   */
  jumpToken?: string;
  autoFocus?: boolean;
}

export default function ReaderPane({
  bookId,
  onOpenSettings,
  settingsOpen = false,
  splitMode = false,
  isPrimary = true,
  isActive,
  canPersist,
  onActivate,
  onToggleSplit,
  onSwapPanes,
  onChangeBook,
  onCloseThisPane,
  initialChapterIndex,
  initialScrollOffset,
  jumpToken,
  autoFocus = false,
}: ReaderPaneProps) {
  const effectiveActive = isActive ?? isPrimary;
  const effectivePersist = canPersist ?? isPrimary;
  const navigate = useNavigate();
  const { t } = useTranslation();
  const { addToast } = useToast();
  const { mode, fontSize, setFontSize, fontFamily, scrollMode, typography, customCss, dualPage, setDualPage, mangaMode, setMangaMode, pageAnimation } = useTheme();
  // Read-only here regardless of pane role (both primary and companion
  // panes need it for the volatile-resume gate below); only the primary
  // pane renders the actual toggle control (see the header, further down).
  const { enabled: isPrivate } = usePrivateMode();

  const [bookTitle, setBookTitle] = useState("");
  const [bookFormat, setBookFormat] = useState<"epub" | "cbz" | "cbr" | "pdf" | "mobi">("epub");
  const [toc, setToc] = useState<TocEntry[]>([]);
  const [chapterIndex, setChapterIndex] = useState(0);
  const [totalChapters, setTotalChapters] = useState(0);
  const [pageCount, setPageCount] = useState(0);
  const [chapterHtml, setChapterHtml] = useState("");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [tocOpen, setTocOpen] = useState(false);
  const [scrollProgress, setScrollProgress] = useState(0);
  const [showShortcuts, setShowShortcuts] = useState(false);
  const [highlightsOpen, setHighlightsOpen] = useState(false);
  const [bookmarksOpen, setBookmarksOpen] = useState(false);
  const [infoOpen, setInfoOpen] = useState(false);
  const [bookDetail, setBookDetail] = useState<Book | null>(null);
  const [toastBookmarkId, setToastBookmarkId] = useState<string | null>(null);
  const [bookmarkRefreshKey, setBookmarkRefreshKey] = useState(0);
  const [saveIndicator, setSaveIndicator] = useState(false);
  const [saveError, setSaveError] = useState(false);
  const [selectionPopup, setSelectionPopup] = useState<{ x: number; y: number; text: string; startOffset: number; endOffset: number } | null>(null);
  // Share as image (F-1-6): holds the ShareCardDialog's props while open, null otherwise.
  const [shareCard, setShareCard] = useState<{ quote: string; title: string; author: string; coverPath: string | null; initialMode: string } | null>(null);

  // Offline dictionary (F-1-1): whether the artifact is ready gates the
  // "Define" action; the card holds the anchored lookup result.
  const [dictionaryReady, setDictionaryReady] = useState(false);
  // Vocabulary builder (F-1-5): whether auto-logging looked-up words is
  // enabled. Gated the same way as `dictionaryReady` above.
  const [vocabularyEnabled, setVocabularyEnabled] = useState(false);
  const [definitionCard, setDefinitionCard] = useState<
    { x: number; y: number; word: string; entry: DictionaryEntry | null; loading: boolean; error: string | null } | null
  >(null);

  // Book search
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");
  const [searchResults, setSearchResults] = useState<{ chapterIndex: number; snippet: string; matchOffset: number }[]>([]);
  const [searching, setSearching] = useState(false);
  const [activeMatchIndex, setActiveMatchIndex] = useState(-1);
  const searchInputRef = useRef<HTMLInputElement>(null);

  // Thumbnail strip (CBZ/CBR/PDF only) — persisted per book so reopening
  // the same book restores the user's preference. Reader stays mounted
  // across `/reader/:bookId` changes, so we must reload the preference on
  // bookId change AND write storage at toggle time (not via an effect) to
  // avoid persisting book A's state to book B's key during the swap.
  const [thumbStripOpen, setThumbStripOpen] = useState(() =>
    bookId ? localStorage.getItem(`folio-thumbstrip-${bookId}`) === "1" : false,
  );
  useEffect(() => {
    if (!bookId) {
      setThumbStripOpen(false);
      return;
    }
    setThumbStripOpen(localStorage.getItem(`folio-thumbstrip-${bookId}`) === "1");
  }, [bookId]);
  const toggleThumbStrip = useCallback(() => {
    if (!bookId) return;
    setThumbStripOpen((prev) => {
      const next = !prev;
      const key = `folio-thumbstrip-${bookId}`;
      if (next) localStorage.setItem(key, "1");
      else localStorage.removeItem(key);
      return next;
    });
  }, [bookId]);

  // Do Not Disturb mode
  const [dndMode, setDndMode] = useState(autoFocus);
  const [focusHint, setFocusHint] = useState(autoFocus);
  const [dndShowControls, setDndShowControls] = useState(false);
  const [dndCursorHidden, setDndCursorHidden] = useState(false);
  const dndTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Auto-dismiss focus hint after 5 seconds, or immediately when DND exits
  useEffect(() => {
    if (!focusHint) return;
    if (!dndMode) { setFocusHint(false); return; }
    const timer = setTimeout(() => setFocusHint(false), 5000);
    return () => clearTimeout(timer);
  }, [focusHint, dndMode]);

  const [chapterError, setChapterError] = useState<string | null>(null);
  const [chapterRetryCount, setChapterRetryCount] = useState(0);
  const [missingFileDialog, setMissingFileDialog] = useState(false);

  const completion = useBookCompletion(bookId, chapterIndex, totalChapters);

  // ---- Navigation history (back/forward) ----
  // Tracks jump-style navigations (TOC clicks, search-result jumps, bookmark
  // navigation). Sequential prev/next chapter changes are *not* recorded —
  // they're reading flow, not jumps.
  type ChapterHistoryMeta = { scroll: number };
  const [history, setHistory] = useState<NavigationHistory<ChapterHistoryMeta>>(
    () => emptyHistory<ChapterHistoryMeta>(),
  );
  const historyRef = useRef(history);
  useEffect(() => {
    historyRef.current = history;
  }, [history]);
  const historyInitialized = useRef(false);

  // Continuous scroll mode state
  const [allChaptersHtml, setAllChaptersHtml] = useState<string[]>([]);
  const [allChaptersLoaded, setAllChaptersLoaded] = useState(false);
  // Per-chapter load progress for the determinate ChapterLoadProgress bar.
  const [chaptersLoaded, setChaptersLoaded] = useState(0);
  const chapterDivRefs = useRef<(HTMLDivElement | null)[]>([]);
  // EPUB and MOBI are both chapter-HTML formats and share the Reader's
  // text-rendering path; PDF/CBZ/CBR go through PageViewer instead.
  const isHtmlBook = bookFormat === "epub" || bookFormat === "mobi";
  const isContinuous = scrollMode === "continuous" && isHtmlBook;
  // Comic formats extract their pages in the background after prepare_comic
  // returns (F-4-1); PDF runs a background prerender pass after prepare_pdf
  // returns (F-4-5). Both stream throttled progress and share the bar below.
  const isComic = bookFormat === "cbz" || bookFormat === "cbr";
  const isPdf = bookFormat === "pdf";

  // Background page-prerender progress bar. Driven by `comic-extract-progress`
  // (F-4-1) for comics and `pdf-prerender-progress` (F-4-5) for PDFs — a book
  // is exactly one format, so a single shared reducer/component instance backs
  // both. Pure state lives in ../lib.
  const [comicProgress, dispatchComicProgress] = useReducer(
    comicExtractProgressReducer,
    undefined,
    initialComicExtractProgress,
  );

  // Time-to-finish state
  const [chapterWordCounts, setChapterWordCounts] = useState<number[]>([]);
  const READING_WPM = 250;

  const contentRef = useRef<HTMLDivElement>(null);
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  // In-memory adjacent-chapter prefetch cache (F-4-3). Serves prev/next
  // chapter turns synchronously; reset on book change (below) so one book's
  // HTML is never served for another.
  const chapterCacheRef = useRef<ChapterCache>(createChapterCache());
  const chapterNavRef = useRef<HTMLDivElement>(null);
  const tocSidebarRef = useRef<HTMLElement>(null);
  const [bottomNavVisible, setBottomNavVisible] = useState(true);
  const sessionStartRef = useRef<number>(Math.floor(Date.now() / 1000));
  const startChapterRef = useRef<number>(0);
  // Latest private-mode flag, read from cleanup/callbacks that must not
  // re-fire when the flag changes (a private→off transition must never
  // masquerade as a reader close — see the unmount effect below).
  const isPrivateRef = useRef(isPrivate);
  useEffect(() => { isPrivateRef.current = isPrivate; }, [isPrivate]);
  const restoringScroll = useRef<number | null>(null);
  const savedScrollPosition = useRef<number | null>(null);
  const targetMatchOffset = useRef<number | null>(null);
  // Pending character-offset jump (F-1-5 vocabulary "jump to source"),
  // set once alongside `restoringScroll`/`resumePage` in the init effect
  // and consumed by the same scroll-restore effects that handle
  // `savedScrollPosition`, below.
  const pendingScrollOffset = useRef<number | null>(null);
  // Bumped by `applyJumpTarget` below so the scroll-restore effects re-run
  // even when neither `chapterIndex` nor `chapterHtml` actually changes
  // (e.g. a same-book jump that targets the chapter already being viewed —
  // those effects otherwise only fire off of state/prop changes, not ref
  // writes).
  const [jumpNonce, setJumpNonce] = useState(0);
  // Shared by the mount-time init effect and the same-book jump effect
  // further down (F-1-5 vocabulary "jump to source"): switches to
  // `targetChapter` and arranges for the scroll-restore effects to land at
  // chapter-local plain-text `offset` once that chapter's content is
  // rendered. `offset === null` degrades to chapter-only (no scroll, no
  // flash) — same as an ordinary chapter switch.
  const applyJumpTarget = useCallback((targetChapter: number, offset: number | null) => {
    setChapterIndex(targetChapter);
    restoringScroll.current = targetChapter;
    savedScrollPosition.current = null;
    pendingScrollOffset.current = offset;
    setJumpNonce((n) => n + 1);
  }, []);
  // Source capture for search jumps — committed to history once the
  // destination scroll is resolved (either synchronously for same-chapter or
  // after the chapter renders for cross-chapter paginated search).
  const pendingSearchHistory = useRef<{
    source: { position: number; scroll: number };
    destChapter: number;
  } | null>(null);
  const userHasInteracted = useRef(false);
  // Last `jumpToken` applied by the same-book jump effect further down
  // (F-1-5 vocabulary "jump to source"). Declared here (rather than next to
  // that effect) so the `[bookId]` reset effect below can reset it — see
  // that effect's comment for why.
  const appliedJumpToken = useRef(jumpToken);

  const isFileNotFound = isBookFileMissing;

  // ---- Reset in-session state when switching books ----
  // Reader stays mounted across `/reader/:bookId` changes (no route key), so
  // anything keyed to the old book — navigation history, pending search
  // refs, the search match offset — must be cleared on bookId change before
  // the init effect seeds the new book's history.
  useEffect(() => {
    setHistory(emptyHistory<ChapterHistoryMeta>());
    historyInitialized.current = false;
    pendingSearchHistory.current = null;
    targetMatchOffset.current = null;
    pendingScrollOffset.current = null;
    // F-4-3: drop the previous book's prefetched chapters so a stale entry
    // can never be served for the newly-switched book.
    chapterCacheRef.current = createChapterCache();
    // Close the details popup and drop the previous book's record so it can't
    // flash stale content over the newly-switched book before get_book resolves.
    setInfoOpen(false);
    setBookDetail(null);
    // F-1-5: re-baseline the same-book jump token to whatever arrived on
    // this commit. A vocabulary "jump to source" into a book that isn't
    // already open changes `bookId` and `jumpToken` together — without this
    // reset, the jumpToken effect below would see a jumpToken it hasn't
    // applied yet and re-apply the (already-handled-by-mount) target while
    // the previous book's `chapterHtml` is still on screen, causing a
    // transient wrong-scroll flash. Resetting here means the jumpToken
    // effect always sees `jumpToken === appliedJumpToken.current` on a
    // cross-book jump and skips — the mount effect below owns that case
    // exclusively. A same-book jump (bookId unchanged) never runs this
    // effect, so the jumpToken effect's own gating is unaffected.
    //
    // `jumpToken` is deliberately NOT a dependency below: adding it would
    // re-run this whole reset (history, search state, chapter cache) on
    // every same-book jump too, which is wrong — this effect must fire on
    // `bookId` change only. The closure still reads the current render's
    // `jumpToken` because React re-creates it every render; only *which*
    // deps trigger invocation is affected by the deps array, not which
    // values a firing invocation closes over.
    appliedJumpToken.current = jumpToken;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [bookId]);

  // ---- Load book info, TOC, and saved progress on mount ----

  useEffect(() => {
    if (!bookId) return;

    let cancelled = false;
    // Re-enter the loading state on book change so downstream effects (incl.
    // the nav-history seed) wait for the new book's progress fetch instead
    // of firing with the previous book's chapterIndex.
    setLoading(true);

    async function init() {
      try {
        const bookInfo = await invoke<Book>("get_book", { bookId });

        if (cancelled) return;

        setBookTitle(bookInfo.title);
        setBookFormat(bookInfo.format);
        setTotalChapters(bookInfo.total_chapters);
        // Full record kept for the read-only "book details" popup (i button).
        setBookDetail(bookInfo);

        // `isHtmlBook` is derived from React state (`bookFormat`), which still
        // holds the previous render's value here — `setBookFormat` above is
        // asynchronous. Gate on the freshly-fetched format so we don't ask
        // the backend for a TOC on PDF/CBZ/CBR (which returns an error and
        // prevents the book from loading).
        if (bookInfo.format === "epub" || bookInfo.format === "mobi") {
          const tocEntries = await invoke<TocEntry[]>("get_toc", { bookId });
          if (!cancelled) setToc(tocEntries);
        } else if (!cancelled) {
          setToc([]);
        }

        // Page count is only meaningful for fixed-layout (PDF) and image
        // (CBZ/CBR) formats. HTML-reflowable books (EPUB + MOBI) use scroll
        // progress instead, so skip the fetch and leave pageCount at 0.
        // PDF's page count comes from prepare_pdf below, which is deferred
        // until the resume page is known (so it can reserve the right page).
        if (bookInfo.format === "cbz" || bookInfo.format === "cbr") {
          try {
            const count = await invoke<number>("get_comic_page_count", { bookId });
            if (!cancelled) setPageCount(count);
          } catch {
            // page count unavailable
          }
        }

        // Resolved resume page — passed to prepare_comic as `startPage` so a
        // mid-book open extracts that page eagerly (alongside page 0) and
        // paints instantly instead of waiting for the background pass.
        let resumePage = 0;
        if (initialChapterIndex !== undefined) {
          applyJumpTarget(initialChapterIndex, typeof initialScrollOffset === "number" ? initialScrollOffset : null);
          resumePage = initialChapterIndex;
        } else {
          // Volatile in-session resume (D-5): while private, the backend
          // never wrote the DB row below for anything read after private
          // mode turned on, so it can be stale. A prior in-memory position
          // for this book (from earlier in the same private session) takes
          // priority over it. First open of a book in a private session
          // has no volatile entry yet, so it falls through to the normal
          // DB read below — never a hard requirement to have one.
          //
          // Read the flag straight from the backend here rather than the
          // `usePrivateMode` hook state: on a fresh reader mount that hook
          // starts `false` and only resolves `get_private_mode`
          // asynchronously, so this init effect (which depends on `bookId`
          // only) could otherwise observe a stale `false` and skip the
          // volatile position entirely, resurrecting the pre-private DB row.
          let privateNow = isPrivate;
          try {
            privateNow = await invoke<boolean>("get_private_mode");
          } catch {
            // Backend unavailable — fall back to the hook's current value.
          }
          if (cancelled) return;
          const volatile = privateNow ? getVolatilePosition(bookId) : undefined;
          if (volatile) {
            setChapterIndex(volatile.chapterIndex);
            savedScrollPosition.current = volatile.scrollPosition;
            restoringScroll.current = volatile.chapterIndex;
            resumePage = volatile.chapterIndex;
          } else {
            try {
              const progress = await invoke<ReadingProgress | null>(
                "get_reading_progress",
                { bookId }
              );
              if (!cancelled && progress) {
                setChapterIndex(progress.chapter_index);
                savedScrollPosition.current = progress.scroll_position;
                restoringScroll.current = progress.chapter_index;
                resumePage = progress.chapter_index;
              }
            } catch {
              // No saved progress — start at chapter 0
            }
          }
        }

        // Warm the page cache now that the resume page is known, so the
        // format-specific "reserve the page the reader opens on" logic targets
        // the actual resume page rather than page 0. Navigation to any
        // not-yet-cached page still works — the backend serves it on demand —
        // so this is purely an open-latency optimization.
        if (bookInfo.format === "pdf") {
          // Establishes the page-cache manifest and renders only the resume
          // page synchronously (the rest go to the background pass), so the
          // first request is a cache hit and doesn't race the background pass.
          // page_count comes back in the manifest; fall back to a direct count
          // only if prepare_pdf fails (e.g. missing file hash).
          try {
            const manifest = await invoke<{ page_count: number }>("prepare_pdf", {
              bookId,
              startPage: resumePage,
            });
            if (!cancelled) setPageCount(manifest.page_count);
          } catch (e) {
            console.warn("PDF cache preparation failed, falling back to direct read:", e);
            try {
              const count = await invoke<number>("get_pdf_page_count", { bookId });
              if (!cancelled) setPageCount(count);
            } catch {
              // page count unavailable
            }
          }
        } else if (bookInfo.format === "cbz" || bookInfo.format === "cbr") {
          // Page 0 + `startPage` are extracted eagerly; the rest stream in the
          // background (surfaced via the comic-extract-progress bar).
          try {
            await invoke("prepare_comic", { bookId, startPage: resumePage });
          } catch (e) {
            console.warn("Cache preparation failed, falling back to direct read:", e);
          }
        }
      } catch (err) {
        if (!cancelled) {
          if (isFileNotFound(err)) {
            setMissingFileDialog(true);
          }
          setError(friendlyError(err, t));
        }
      } finally {
        if (!cancelled) {
          setLoading(false);
        }
      }
    }

    init();
    return () => {
      cancelled = true;
    };
  }, [bookId]);

  // ---- Apply a same-book jump while already mounted (F-1-5) ----
  // The mount-time init effect above only consumes `initialChapterIndex`/
  // `initialScrollOffset` once, on `bookId` change. Reader.tsx keeps this
  // pane mounted across a jump to a book that's already open (no `bookId`
  // change, no remount), so a second jump needs its own trigger: `jumpToken`
  // changes on every distinct navigation (see its prop doc), even when the
  // target chapter/offset repeats a previous value. Skips on the initial
  // mount and on a cross-book jump — in both cases the `[bookId]` reset
  // effect above has already re-baselined `appliedJumpToken.current` to the
  // current `jumpToken`, so `resolveJumpTokenGate` (`bookIdChanged: false`
  // here — this effect only ever observes the ref's already-settled value)
  // sees them equal and reports `shouldApply: false`; the mount effect owns
  // applying the target in that case.
  useEffect(() => {
    const { shouldApply, nextApplied } = resolveJumpTokenGate(
      jumpToken,
      appliedJumpToken.current,
      false,
    );
    appliedJumpToken.current = nextApplied;
    if (!shouldApply) return;
    if (initialChapterIndex === undefined) return; // navigation carried no jump target
    applyJumpTarget(initialChapterIndex, typeof initialScrollOffset === "number" ? initialScrollOffset : null);
  }, [jumpToken, initialChapterIndex, initialScrollOffset, applyJumpTarget]);

  // ---- Seed navigation history with the initial chapter ----
  // Fires once after the initial load so the user's starting position is the
  // first entry on the back stack.
  useEffect(() => {
    if (loading || historyInitialized.current) return;
    historyInitialized.current = true;
    setHistory(
      pushEntry(emptyHistory<ChapterHistoryMeta>(), {
        position: chapterIndex,
        meta: { scroll: savedScrollPosition.current ?? 0 },
      }),
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [loading]);

  // ---- Fetch word counts for time-to-finish (HTML chapter formats) ----

  useEffect(() => {
    if (!bookId || loading || !isHtmlBook) return;
    invoke<number[]>("get_chapter_word_counts", { bookId })
      .then(setChapterWordCounts)
      .catch(() => {}); // word counts are best-effort
  }, [bookId, loading, isHtmlBook]);

  // ---- Load all chapters for continuous scroll mode ----

  useEffect(() => {
    if (!bookId || loading || !isContinuous) {
      setAllChaptersLoaded(false);
      setAllChaptersHtml([]);
      setChaptersLoaded(0);
      return;
    }

    let cancelled = false;
    let unlisten: (() => void) | null = null;
    // Reset the per-chapter counter at the start of each continuous load so
    // the determinate bar restarts from 0 on book change / retry.
    setChaptersLoaded(0);

    async function loadAll() {
      try {
        // Await listener registration BEFORE invoking `get_all_chapters` so no
        // `chapter-load-progress` event can be emitted (e.g. for cached/small
        // books) before the listener is attached and slip past the counter.
        // Match payload.bookId so a stale event from a previous book can't
        // drive this pane's counter.
        const fn = await listen<{ bookId: string; loaded: number; total: number }>(
          "chapter-load-progress",
          (event) => {
            if (!cancelled && event.payload.bookId === bookId) {
              setChaptersLoaded(event.payload.loaded);
            }
          },
        );
        // If the effect was torn down while listen() was registering, the
        // teardown ran before `unlisten` was set — clean up here instead so
        // the late-resolved listener doesn't leak.
        if (cancelled) {
          fn();
          return;
        }
        unlisten = fn;

        const chapters = await invoke<string[]>("get_all_chapters", { bookId });
        if (!cancelled) {
          setAllChaptersHtml(chapters);
          setAllChaptersLoaded(true);
        }
      } catch (err) {
        if (!cancelled) setChapterError(friendlyError(err, t));
      }
    }

    loadAll();
    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }, [bookId, loading, isContinuous, chapterRetryCount]);

  // ---- Listen for background comic page-extraction progress (F-4-1) ----
  // Mirrors the `chapter-load-progress` lifecycle above: register the
  // listener before any extraction can matter, reset the bar on book
  // change, and unlisten on unmount/book change. The reducer scopes events
  // to the bound book, so a stale event from a previous book is ignored.
  useEffect(() => {
    if (!bookId || !isComic) return;
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    dispatchComicProgress({ type: "reset", bookId });

    async function subscribe() {
      const fn = await listen<{ bookId: string; loaded: number; total: number }>(
        "comic-extract-progress",
        (event) => {
          if (!cancelled) {
            dispatchComicProgress({ type: "progress", ...event.payload });
          }
        },
      );
      // Torn down while listen() was registering — clean up the late
      // listener instead of leaking it (same guard as chapter-load-progress).
      if (cancelled) {
        fn();
        return;
      }
      unlisten = fn;
    }

    subscribe();
    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }, [bookId, isComic]);

  // ---- Listen for background PDF prerender progress (F-4-5) ----
  // Reuses the shared comic-progress reducer/bar (approach b): `prepare_pdf`
  // (invoked in the init effect) kicks off a background pass that emits
  // throttled `pdf-prerender-progress` events, then a guaranteed terminal
  // event at pass end. That terminal event carries only {loaded,total} — no
  // "done" flag — and on honest PARTIAL coverage settles below 100% (the size
  // bound stopped the pass early). Since the bar's `loaded < total` visibility
  // rule can't tell a partial terminal from a mid-pass step, an idle timer
  // dispatches `settle` once events stop, hiding the bar. A false mid-pass
  // settle self-heals: the next progress event clears `settled` and re-shows
  // the bar (the genuine terminal event is the last, so its settle stays).
  // Private mode emits no events, so the timer is never armed and the bar
  // simply never shows (no hang).
  useEffect(() => {
    if (!bookId || !isPdf) return;
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    let settleTimer: ReturnType<typeof setTimeout> | null = null;
    dispatchComicProgress({ type: "reset", bookId });

    const armSettle = () => {
      if (settleTimer) clearTimeout(settleTimer);
      settleTimer = setTimeout(() => {
        if (!cancelled) dispatchComicProgress({ type: "settle" });
      }, PDF_PRERENDER_SETTLE_MS);
    };

    async function subscribe() {
      const fn = await listen<{ bookId: string; loaded: number; total: number }>(
        "pdf-prerender-progress",
        (event) => {
          // Scope to THIS pane's book before arming the settle timer: a stale
          // pass from a just-closed book (or the other pane's book in split
          // mode) still emits globally, and while the reducer ignores its
          // counts, an unguarded `armSettle()` could settle/hide this book's
          // live bar. Mirrors the `chapter-load-progress` bookId filter.
          if (!cancelled && event.payload.bookId === bookId) {
            dispatchComicProgress({ type: "progress", ...event.payload });
            armSettle();
          }
        },
      );
      // Torn down while listen() was registering — clean up the late listener
      // instead of leaking it (same guard as comic-extract-progress).
      if (cancelled) {
        fn();
        return;
      }
      unlisten = fn;
    }

    subscribe();
    return () => {
      cancelled = true;
      if (settleTimer) clearTimeout(settleTimer);
      if (unlisten) unlisten();
    };
  }, [bookId, isPdf]);

  // ---- Scroll to saved chapter after all chapters load (continuous mode) ----

  useEffect(() => {
    if (!isContinuous || !allChaptersLoaded || restoringScroll.current === null) return;

    const targetChapter = restoringScroll.current;
    const scrollPos = savedScrollPosition.current;
    const offset = pendingScrollOffset.current;

    // Wait for DOM to render all chapter divs
    requestAnimationFrame(() => {
      const chapterDiv = chapterDivRefs.current[targetChapter];
      const container = scrollContainerRef.current;
      if (chapterDiv && container) {
        if (offset !== null) {
          // F-1-5 vocabulary jump: chapter-local char-offset fraction through
          // the same resolveBookmarkScrollTop math the bookmark restore path
          // uses for continuous mode (chapterOffsetTop + fraction * chapterHeight).
          const textLen = chapterDiv.textContent?.length || 1;
          container.scrollTop = resolveBookmarkScrollTop(true, offset / textLen, {
            chapterOffsetTop: chapterDiv.offsetTop,
            chapterHeight: chapterDiv.offsetHeight,
            containerScrollHeight: container.scrollHeight,
          });
        } else {
          // Scroll to the target chapter div, then offset within it
          const chapterTop = chapterDiv.offsetTop;
          const chapterHeight = chapterDiv.offsetHeight;
          container.scrollTop = chapterTop + (scrollPos ?? 0) * chapterHeight;
        }
      }
      savedScrollPosition.current = null;
      pendingScrollOffset.current = null;
      // Defer clearing restoringScroll until after the scroll event from the
      // programmatic scrollTop has been dispatched, so the continuous-scroll
      // listener sees restoringScroll !== null and skips setting userHasInteracted.
      requestAnimationFrame(() => {
        restoringScroll.current = null;
      });
    });
  }, [allChaptersLoaded, isContinuous, jumpNonce]); // eslint-disable-line react-hooks/exhaustive-deps
  // Note: chapterIndex intentionally excluded — including it would re-fire
  // the restore effect when chapterIndex changes from scroll tracking.
  // jumpNonce included so a same-book jump (F-1-5) that targets the chapter
  // already being viewed still re-applies the new pendingScrollOffset.

  // ---- Track visible chapter in continuous scroll mode ----

  useEffect(() => {
    if (!isContinuous || !allChaptersLoaded) return;
    const container = scrollContainerRef.current;
    if (!container) return;

    function updateVisibleChapter() {
      if (!container) return;
      // Don't mark as user-interacted during programmatic scroll restore —
      // that would suppress applying valid remote progress updates.
      if (restoringScroll.current === null) {
        userHasInteracted.current = true;
      }
      const containerTop = container.scrollTop;
      const containerMid = containerTop + container.clientHeight / 3;

      for (let i = chapterDivRefs.current.length - 1; i >= 0; i--) {
        const div = chapterDivRefs.current[i];
        if (div && div.offsetTop <= containerMid) {
          setChapterIndex(i);
          break;
        }
      }

      // Update scroll progress as book-global 0-1
      const maxScroll = container.scrollHeight - container.clientHeight;
      setScrollProgress(maxScroll > 0 ? container.scrollTop / maxScroll : 0);
    }

    container.addEventListener("scroll", updateVisibleChapter, { passive: true });
    return () => container.removeEventListener("scroll", updateVisibleChapter);
  }, [isContinuous, allChaptersLoaded]);

  // ---- Load chapter content when chapterIndex changes ----

  useEffect(() => {
    if (!bookId || loading || isContinuous) return;

    // Reset scroll to the top of the new chapter unless we're restoring a
    // saved position or scrolling to a search match. Shared by the cache-hit
    // and IPC-load paths so both behave identically.
    const resetScroll = () => {
      if (restoringScroll.current !== chapterIndex && targetMatchOffset.current === null && scrollContainerRef.current) {
        scrollContainerRef.current.scrollTop = 0;
      }
    };

    // F-4-3: serve synchronously from the prefetch cache when present — this
    // is what removes the blank/spinner flash at chapter boundaries. On a
    // miss, fall through to the async IPC path (unchanged behavior).
    const cached = getCachedChapter(chapterCacheRef.current, bookId, chapterIndex);
    if (cached !== undefined) {
      setChapterHtml(cached);
      setChapterError(null);
      resetScroll();
      return;
    }

    let cancelled = false;

    async function loadChapter() {
      try {
        const html = await invoke<string>("get_chapter_content", {
          bookId,
          chapterIndex,
        });
        if (!cancelled) {
          setCachedChapter(chapterCacheRef.current, bookId, chapterIndex, html);
          setChapterHtml(html);
          setChapterError(null);
          resetScroll();
        }
      } catch (err) {
        if (!cancelled) {
          if (isFileNotFound(err)) {
            setMissingFileDialog(true);
          }
          setChapterError(friendlyError(err, t));
        }
      }
    }

    loadChapter();
    return () => {
      cancelled = true;
    };
  }, [bookId, chapterIndex, loading, chapterRetryCount]);

  // ---- Prefetch adjacent chapters after the current one renders (F-4-3) ----
  // Gated on `chapterHtml` so this fires after the current chapter is on
  // screen — it never blocks the current render. Prefetches are best-effort
  // and idempotent (cache hits are skipped); eviction bounds memory to a
  // small window around the current position.
  useEffect(() => {
    if (!bookId || loading || isContinuous || !isHtmlBook || !chapterHtml) return;

    const cache = chapterCacheRef.current;
    evictOutsideWindow(cache, chapterIndex);

    let cancelled = false;
    for (const idx of adjacentChapterIndices(chapterIndex, totalChapters)) {
      if (getCachedChapter(cache, bookId, idx) !== undefined) continue;
      invoke<string>("get_chapter_content", { bookId, chapterIndex: idx })
        .then((html) => {
          if (!cancelled) setCachedChapter(cache, bookId, idx, html);
        })
        .catch(() => {}); // prefetch is best-effort; misses fall back to IPC
    }
    return () => {
      cancelled = true;
    };
  }, [bookId, chapterHtml, chapterIndex, totalChapters, loading, isContinuous, isHtmlBook]);

  // ---- Flash the jumped-to word (F-1-5 vocabulary "jump to source") ----
  // Best-effort, paginated mode only: the highlight-injection pipeline below
  // (`highlightedHtml`) only runs for the single-chapter paginated render;
  // continuous mode renders each chapter's raw HTML directly (`allChaptersHtml`),
  // so it isn't wired into that pipeline and doesn't get a flash — precise
  // scroll still lands correctly there, per the effect above.
  const [flashRange, setFlashRange] = useState<{ chapterIndex: number; start: number; end: number } | null>(null);
  const flashTimeoutRef = useRef<number | null>(null);

  useEffect(() => {
    return () => {
      if (flashTimeoutRef.current !== null) window.clearTimeout(flashTimeoutRef.current);
    };
  }, []);

  // The nav-state contract only carries a single start offset (no end), so
  // the flashed range is approximated as the contiguous non-whitespace run
  // containing it — "the word" at that position in the same contentRef-ish
  // text the offset was computed against.
  const triggerFlash = useCallback(
    (offset: number) => {
      const text = scrollContainerRef.current?.textContent ?? "";
      if (offset < 0 || offset >= text.length) return;
      let start = offset;
      let end = offset;
      while (start > 0 && !/\s/.test(text[start - 1])) start--;
      while (end < text.length && !/\s/.test(text[end])) end++;
      if (end <= start) return;
      if (flashTimeoutRef.current !== null) window.clearTimeout(flashTimeoutRef.current);
      setFlashRange({ chapterIndex, start, end });
      flashTimeoutRef.current = window.setTimeout(() => setFlashRange(null), 1800);
    },
    [chapterIndex]
  );

  // ---- Restore scroll position after chapter HTML renders ----

  useEffect(() => {
    if (isContinuous) return; // continuous mode has its own restore
    if (restoringScroll.current !== chapterIndex || !chapterHtml || !bookId) return;

    const scrollPos = savedScrollPosition.current;
    const offset = pendingScrollOffset.current;
    if ((scrollPos !== null || offset !== null) && scrollContainerRef.current) {
      const container = scrollContainerRef.current;
      requestAnimationFrame(() => {
        if (offset !== null) {
          // F-1-5 vocabulary jump: same offset→scroll approach the search
          // "scroll to match" effect uses below.
          const textLen = container.textContent?.length || 1;
          container.scrollTop = (offset / textLen) * container.scrollHeight;
          triggerFlash(offset);
        } else if (scrollPos !== null) {
          // Match the save-side denominator: scrollProgress is computed as
          // scrollTop / (scrollHeight - clientHeight). Restore must invert
          // that, otherwise long chapters drift towards the bottom.
          const max = Math.max(0, container.scrollHeight - container.clientHeight);
          container.scrollTop = scrollPos * max;
        }
        restoringScroll.current = null;
        savedScrollPosition.current = null;
        pendingScrollOffset.current = null;
      });
    } else {
      restoringScroll.current = null;
      savedScrollPosition.current = null;
      pendingScrollOffset.current = null;
    }
  }, [chapterHtml, bookId, chapterIndex, jumpNonce]); // eslint-disable-line react-hooks/exhaustive-deps
  // jumpNonce included so a same-book jump (F-1-5) that targets the chapter
  // already being viewed still re-applies the new pendingScrollOffset.

  // ---- Scroll to search match after chapter loads ----
  useEffect(() => {
    if (targetMatchOffset.current === null || !chapterHtml || !scrollContainerRef.current) return;
    const container = scrollContainerRef.current;
    const offset = targetMatchOffset.current;
    targetMatchOffset.current = null;
    requestAnimationFrame(() => {
      const textLen = container.textContent?.length || 1;
      container.scrollTop = (offset / textLen) * container.scrollHeight;

      // Commit pending search history with the achieved destination scroll.
      // Use the scrollProgress denominator (scrollHeight - clientHeight) so
      // a later restore via resolveBookmarkScrollTop lands on the same pixel.
      const pending = pendingSearchHistory.current;
      pendingSearchHistory.current = null;
      if (pending) {
        const max = Math.max(0, container.scrollHeight - container.clientHeight);
        const destScroll = max > 0 ? container.scrollTop / max : 0;
        recordJumpFromRef.current?.(pending.source, {
          position: pending.destChapter,
          scroll: destScroll,
        });
      }
    });
  }, [chapterHtml]);

  // ---- Save reading progress ----

  const getChapterScrollPosition = useCallback(() => {
    if (!isContinuous) return scrollProgress;
    const container = scrollContainerRef.current;
    const chapterDiv = chapterDivRefs.current[chapterIndex];
    if (!container || !chapterDiv) return 0;
    const posInChapter = container.scrollTop - chapterDiv.offsetTop;
    const chapterHeight = chapterDiv.offsetHeight;
    return chapterHeight > 0 ? Math.max(0, Math.min(1, posInChapter / chapterHeight)) : 0;
  }, [isContinuous, scrollProgress, chapterIndex]);

  const saveProgress = useCallback(
    async (scrollPos?: number) => {
      if (!bookId) return;
      const resolvedScroll = scrollPos ?? getChapterScrollPosition();
      // Volatile in-session resume (D-5): mirror into memory only, never a
      // persistent store. The DB write just below is a no-op on the
      // backend while private (B-M1), so this is the only place the
      // reader's true current position survives a book-close/reopen.
      if (isPrivateRef.current) {
        setVolatilePosition(bookId, { chapterIndex, scrollPosition: resolvedScroll });
      }
      try {
        await invoke("save_reading_progress", {
          bookId,
          chapterIndex,
          scrollPosition: resolvedScroll,
        });
        setSaveIndicator(true);
        setTimeout(() => setSaveIndicator(false), 1500);
      } catch {
        // Show brief error indicator — don't interrupt reading
        setSaveError(true);
        setTimeout(() => setSaveError(false), 2000);
      }
    },
    [bookId, chapterIndex, getChapterScrollPosition]
  );

  // Save progress on chapter change (paginated mode only — continuous tracks via scroll).
  // Companion panes in split mode only persist progress when they're
  // showing a different book than the primary pane (so each pane
  // writes its own book's row). When both panes share a bookId the
  // shell sets `canPersist` to primary-only.
  useEffect(() => {
    if (!effectivePersist || !bookId || loading || isContinuous) return;
    saveProgress(0);
  }, [chapterIndex, effectivePersist]); // eslint-disable-line react-hooks/exhaustive-deps

  // ---- Scroll tracking (paginated mode) ----

  useEffect(() => {
    if (isContinuous) return; // continuous mode has its own tracking
    const container = scrollContainerRef.current;
    if (!container) return;

    function handleScroll() {
      if (!container || restoringScroll.current === chapterIndex) return;
      userHasInteracted.current = true;
      const { scrollTop, scrollHeight, clientHeight } = container;
      const maxScroll = scrollHeight - clientHeight;
      const progress = maxScroll > 0 ? scrollTop / maxScroll : 0;
      setScrollProgress(progress);
    }

    container.addEventListener("scroll", handleScroll, { passive: true });
    return () => container.removeEventListener("scroll", handleScroll);
  }, [chapterHtml, chapterIndex]);

  const bookIdRef = useRef(bookId);
  const chapterIndexRef = useRef(chapterIndex);
  useEffect(() => { bookIdRef.current = bookId; }, [bookId]);
  useEffect(() => { chapterIndexRef.current = chapterIndex; }, [chapterIndex]);

  useEffect(() => {
    startChapterRef.current = chapterIndex;
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // Keep the latest saveProgress reachable from the unmount cleanup below
  // without listing it (or chapterIndex) as a dependency — otherwise the
  // cleanup re-fires on every chapter change and, critically, on a private
  // mode toggle, recording a session as if the reader had closed.
  const saveProgressRef = useRef(saveProgress);
  useEffect(() => { saveProgressRef.current = saveProgress; }, [saveProgress]);

  // When tracking resumes (private → off), restart the session baseline so
  // the time and pages accumulated while private aren't folded into the
  // next non-private session record.
  const prevPrivateRef = useRef(isPrivate);
  useEffect(() => {
    if (prevPrivateRef.current && !isPrivate) {
      sessionStartRef.current = Math.floor(Date.now() / 1000);
      startChapterRef.current = chapterIndexRef.current;
    }
    prevPrivateRef.current = isPrivate;
  }, [isPrivate]);

  useEffect(() => {
    if (!effectivePersist) return;
    return () => {
      saveProgressRef.current();
      const id = bookIdRef.current;
      const now = Math.floor(Date.now() / 1000);
      const duration = now - sessionStartRef.current;
      if (id && duration >= 10) {
        invoke("record_reading_session", {
          bookId: id,
          startedAt: sessionStartRef.current,
          durationSecs: duration,
          pagesRead: Math.abs(chapterIndexRef.current - startChapterRef.current) + 1,
        }).catch(() => {});
      }
    };
  }, [effectivePersist]);

  // Push local changes to sync remote only on reader unmount (not on chapter change).
  // Chains after an explicit save_reading_progress to guarantee the final position
  // is in the DB before the sync payload is built.
  const getScrollPosRef = useRef(getChapterScrollPosition);
  useEffect(() => { getScrollPosRef.current = getChapterScrollPosition; }, [getChapterScrollPosition]);

  useEffect(() => {
    if (!effectivePersist) return;
    return () => {
      const id = bookIdRef.current;
      if (!id) return;
      // Mirror the final position into volatile memory too (see
      // `saveProgress`) — this is the true "closing" position, captured
      // via refs since this cleanup can run after `isPrivate` changed.
      if (isPrivateRef.current) {
        setVolatilePosition(id, {
          chapterIndex: chapterIndexRef.current,
          scrollPosition: getScrollPosRef.current(),
        });
      }
      invoke("save_reading_progress", {
        bookId: id,
        chapterIndex: chapterIndexRef.current,
        scrollPosition: getScrollPosRef.current(),
      })
        .catch(() => {})
        .finally(() => {
          invoke("sync_push_book", { bookId: id }).catch(() => {});
        });
    };
  }, [effectivePersist]); // eslint-disable-line react-hooks/exhaustive-deps

  // Text selection handler for highlights
  useEffect(() => {
    function handleMouseUp() {
      const selection = window.getSelection();
      if (!selection || selection.isCollapsed || !contentRef.current) {
        return;
      }
      const text = selection.toString().trim();
      // The popup drives two actions with different minimums: highlight creation
      // keeps its 3-char floor, while "Define" follows extractLookupWord's 2-char
      // rule. Show the popup when either is possible so 2-letter words (go/be/ax)
      // can be defined without loosening highlight creation.
      if (!text) return;
      const canHighlight = text.length >= 3;
      const canDefine = dictionaryReady && extractLookupWord(text) !== null;
      if (!canHighlight && !canDefine) return;

      const range = selection.getRangeAt(0);
      if (!contentRef.current.contains(range.commonAncestorContainer)) return;

      const rect = range.getBoundingClientRect();
      const containerRect = scrollContainerRef.current?.getBoundingClientRect();
      if (!containerRect) return;

      // Compute text offset relative to the reader-content div
      const preRange = document.createRange();
      preRange.selectNodeContents(contentRef.current);
      preRange.setEnd(range.startContainer, range.startOffset);
      const startOffset = preRange.toString().length;
      const endOffset = startOffset + text.length;

      const scrollTop = scrollContainerRef.current?.scrollTop ?? 0;
      setSelectionPopup({
        x: rect.left + rect.width / 2 - containerRect.left,
        y: rect.top - containerRect.top + scrollTop - 8,
        text,
        startOffset,
        endOffset,
      });
    }

    function handleMouseDown(e: MouseEvent) {
      // Dismiss the selection popup and the definition card when clicking
      // outside both.
      const target = e.target as HTMLElement;
      if (!target.closest('[data-highlight-popup]') && !target.closest('[data-definition-card]')) {
        setSelectionPopup(null);
        setDefinitionCard(null);
      }
    }

    document.addEventListener("mouseup", handleMouseUp);
    document.addEventListener("mousedown", handleMouseDown);
    return () => {
      document.removeEventListener("mouseup", handleMouseUp);
      document.removeEventListener("mousedown", handleMouseDown);
    };
  }, [chapterHtml, dictionaryReady]);

  // Offline dictionary readiness (F-1-1): only when the feature is enabled and
  // the artifact is installed does the "Define" action appear. Re-checked per
  // book open and whenever `settingsOpen` toggles — the settings panel is an
  // overlay that keeps the reader mounted, so enabling/downloading/deleting the
  // dictionary there must refresh the Define gate when the panel closes.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const enabled = await invoke<string | null>("get_setting_value", { key: "dictionary_enabled" });
        if (enabled !== "true") {
          if (!cancelled) setDictionaryReady(false);
          return;
        }
        const status = await invoke<DictionaryStatus>("get_dictionary_status");
        if (!cancelled) setDictionaryReady(status.state === "ready");
      } catch {
        if (!cancelled) setDictionaryReady(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [bookId, settingsOpen]);

  // Vocabulary builder auto-log readiness (F-1-5): re-checked per book open
  // and whenever `settingsOpen` toggles, same rationale as `dictionaryReady`
  // above — the Settings overlay is what flips this setting while the
  // reader stays mounted underneath it.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const enabled = await invoke<string | null>("get_setting_value", { key: "vocabulary_enabled" });
        if (!cancelled) setVocabularyEnabled(enabled === "true");
      } catch {
        if (!cancelled) setVocabularyEnabled(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [bookId, settingsOpen]);

  const handleDefine = useCallback(async () => {
    if (!selectionPopup) return;
    const word = extractLookupWord(selectionPopup.text);
    if (!word) return;
    const anchor = { x: selectionPopup.x, y: selectionPopup.y };
    // Capture the offsets before the popup is cleared below, so a vocabulary
    // log call after the lookup resolves still has the real selection range.
    const { startOffset, endOffset } = selectionPopup;
    // Opening the card supersedes the selection popup (mirrors dismiss).
    setSelectionPopup(null);
    window.getSelection()?.removeAllRanges();
    setDefinitionCard({ x: anchor.x, y: anchor.y, word, entry: null, loading: true, error: null });
    try {
      const entry = await invoke<DictionaryEntry | null>("lookup_word", { word });
      setDefinitionCard((c) => (c && c.word === word ? { ...c, loading: false, entry } : c));
      // Auto-log (F-1-5): fire-and-forget, only when the user opted in.
      // Never blocks or throws into the Define UX — failures are swallowed by
      // this dedicated try/catch, kept separate from the lookup's own so a
      // logging failure can never paint over an already-successful lookup.
      try {
        if (entry && vocabularyEnabled && contentRef.current) {
          const full = document.createRange();
          full.selectNodeContents(contentRef.current);
          const chapterText = full.toString();
          const contextSentence = extractContextSentence(chapterText, startOffset, endOffset);
          invoke("log_vocabulary_word", {
            word,
            lemma: entry.matchedWord,
            pos: primarySense(entry)?.pos ?? null,
            definition: formatDefinitionSnapshot(entry),
            bookId,
            bookTitle: bookTitle || null,
            chapterIndex,
            contextSentence,
            startOffset,
            endOffset,
          }).catch((e) => console.error("vocab log failed", e));
        }
      } catch (e) {
        console.error("vocab log failed", e);
      }
    } catch (err) {
      const { kind } = toFolioError(err);
      const notReady = kind === "NotFound";
      if (notReady) setDictionaryReady(false); // artifact vanished — hide Define
      setDefinitionCard((c) =>
        c && c.word === word
          ? { ...c, loading: false, error: notReady ? t("reader.dictionary.notReady") : t("reader.dictionary.lookupFailed") }
          : c,
      );
    }
  }, [selectionPopup, t, vocabularyEnabled, bookId, bookTitle, chapterIndex]);

  // Share as image (F-1-6): opens ShareCardDialog with the current
  // selection as the quote. Dismisses the selection popup, mirroring
  // handleDefine above.
  const handleShareAsImage = useCallback(() => {
    if (!selectionPopup) return;
    setShareCard({
      quote: selectionPopup.text,
      title: bookTitle,
      author: bookDetail?.author ?? "",
      coverPath: bookDetail?.cover_path ?? null,
      initialMode: mode,
    });
    setSelectionPopup(null);
    window.getSelection()?.removeAllRanges();
  }, [selectionPopup, bookTitle, bookDetail, mode]);

  // ---- Highlights ----
  interface ChapterHighlight { id: string; startOffset: number; endOffset: number; color: string }
  const [highlights, setHighlights] = useState<ChapterHighlight[]>([]);
  // Monotonic signal PageViewer re-fetches its PDF highlight bands on after any
  // mutation (popup create/remove + undo, Highlights-panel delete/edit, sync).
  // Declared here (before the sync effect that bumps it) and invalidated in the
  // same places the EPUB highlight list refreshes, so bands and marks stay in
  // step. See handleCreatePdfHighlight / handleRemovePdfHighlights below.
  const [pdfHighlightRefresh, setPdfHighlightRefresh] = useState(0);

  const loadHighlights = useCallback(async () => {
    if (!bookId) return;
    try {
      const all = await invoke<Array<{ id: string; chapterIndex: number; startOffset: number; endOffset: number; color: string }>>("get_highlights", { bookId });
      setHighlights(all.filter((h) => h.chapterIndex === chapterIndex));
    } catch {
      // ignore
    }
  }, [bookId, chapterIndex]);

  const loadHighlightsRef = useRef(loadHighlights);
  useEffect(() => { loadHighlightsRef.current = loadHighlights; }, [loadHighlights]);

  useEffect(() => {
    loadHighlights();
  }, [loadHighlights]);

  // ---- Sync: pull on mount, listen for remote updates ----
  // Listeners are registered before the pull to avoid a race where the backend
  // emits events before the frontend is listening. Dependencies are only [bookId]
  // so this effect runs once per book, not on every chapter change.

  useEffect(() => {
    if (!bookId) return;

    // Listen for sync events targeting this book
    const unlistenApplied = listen<string>("sync-applied", (event) => {
      if (event.payload === bookId) {
        // Remote bookmarks/highlights were merged — refresh from DB
        loadHighlightsRef.current();
        setBookmarkRefreshKey((k) => k + 1);
        setPdfHighlightRefresh((k) => k + 1);
      }
    });

    const unlistenProgress = listen<string>("sync-progress-updated", (event) => {
      if (event.payload === bookId && !userHasInteracted.current) {
        // Remote progress arrived and user hasn't navigated — apply it
        invoke<ReadingProgress | null>("get_reading_progress", { bookId })
          .then((progress) => {
            if (progress && progress.chapter_index !== chapterIndexRef.current) {
              console.info(`[sync] Applying remote reading position: chapter ${progress.chapter_index}`);
              setChapterIndex(progress.chapter_index);
              savedScrollPosition.current = progress.scroll_position;
              restoringScroll.current = progress.chapter_index;
            } else if (progress) {
              savedScrollPosition.current = progress.scroll_position;
              restoringScroll.current = progress.chapter_index;
            }
          })
          .catch(() => {});
      }
    });

    // Pull latest remote state on reader open (non-blocking)
    // Invoked AFTER listeners are registered so events are never missed.
    invoke("sync_pull_book", { bookId }).catch(() => {});

    return () => {
      unlistenApplied.then((fn) => fn());
      unlistenProgress.then((fn) => fn());
    };
  }, [bookId]); // eslint-disable-line react-hooks/exhaustive-deps

  // Inject highlight <mark> tags into the sanitized HTML string.
  // This survives React re-renders (unlike DOM manipulation).
  const activeFlash = flashRange && flashRange.chapterIndex === chapterIndex ? flashRange : null;

  const highlightedHtml = useMemo(() => {
    const html = chapterHtml;
    if (highlights.length === 0 && !activeFlash) return html;

    // Walk the HTML string, tracking text offset (skip inside tags).
    // Build a map: textOffset -> htmlIndex for insertion points.
    const textToHtml: number[] = []; // textToHtml[textOffset] = htmlIndex
    let inTag = false;
    let textOffset = 0;
    for (let i = 0; i < html.length; i++) {
      if (html[i] === "<") { inTag = true; continue; }
      if (html[i] === ">") { inTag = false; continue; }
      if (!inTag) {
        textToHtml[textOffset] = i;
        textOffset++;
      }
    }
    textToHtml[textOffset] = html.length; // sentinel

    // Sort highlights by startOffset (stable for insertion)
    const sorted = [...highlights].sort((a, b) => a.startOffset - b.startOffset);

    // Build result by inserting <mark> and </mark> at the right HTML positions
    type Insertion = { htmlIdx: number; tag: string; priority: number };
    const insertions: Insertion[] = [];
    for (const hl of sorted) {
      const startIdx = textToHtml[hl.startOffset];
      const endIdx = textToHtml[hl.endOffset];
      if (startIdx == null || endIdx == null) continue;
      insertions.push({
        htmlIdx: startIdx,
        tag: `<mark style="background-color:${hl.color}44;border-radius:2px;padding:1px 0">`,
        priority: 0,
      });
      insertions.push({
        htmlIdx: endIdx,
        tag: "</mark>",
        priority: 1,
      });
    }

    // F-1-5 vocabulary jump: a transient pulse over the jumped-to word,
    // reusing the same textOffset -> htmlIndex map as the persisted
    // highlights above.
    if (activeFlash) {
      const startIdx = textToHtml[activeFlash.start];
      const endIdx = textToHtml[activeFlash.end];
      if (startIdx != null && endIdx != null) {
        insertions.push({ htmlIdx: startIdx, tag: `<mark class="vj-flash-target">`, priority: 0 });
        insertions.push({ htmlIdx: endIdx, tag: "</mark>", priority: 1 });
      }
    }

    // Sort by position (descending) so we insert from end to start without shifting indices
    insertions.sort((a, b) => b.htmlIdx - a.htmlIdx || b.priority - a.priority);

    let result = html;
    for (const ins of insertions) {
      result = result.slice(0, ins.htmlIdx) + ins.tag + result.slice(ins.htmlIdx);
    }
    return result;
  }, [chapterHtml, highlights, activeFlash]);

  // ---- Intercept clicks on links inside chapter HTML ----
  // The chapter content is sanitized but anchors survive — left to the
  // WebView, an http(s) link would replace the in-app page with the remote
  // site (Project Gutenberg, etc.). Delegate the click: external schemes
  // open in the user's default app via the opener plugin; relative paths
  // are blocked (internal cross-chapter resolution is a separate feature);
  // fragment-only hashes fall through so the browser scrolls naturally.
  useEffect(() => {
    const container = contentRef.current;
    if (!container) return;

    function handleClick(e: MouseEvent) {
      if (e.defaultPrevented || e.button !== 0) return;
      // Let the user open links in a new window/tab via modifier keys
      // (Cmd/Ctrl+click) — those go through the WebView's default handling.
      if (e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;

      const root = contentRef.current;
      const anchor = (e.target as HTMLElement | null)?.closest("a");
      if (!root || !anchor || !root.contains(anchor)) return;
      const href = anchor.getAttribute("href");
      if (!href) return;

      if (isExternalUrl(href)) {
        e.preventDefault();
        openUrl(href).catch((err) => addToast(friendlyError(err, t), "error"));
        return;
      }

      // Fragment-only links: let the browser scroll into view.
      if (href.startsWith("#")) return;

      // Relative paths (e.g. cross-chapter EPUB references): suppress so
      // the WebView doesn't navigate to a non-existent route. Resolving
      // these to TOC chapters is a future enhancement.
      e.preventDefault();
    }

    container.addEventListener("click", handleClick);
    return () => container.removeEventListener("click", handleClick);
  }, [chapterHtml, allChaptersLoaded, isContinuous, addToast, t]);

  // Apply search term highlighting on top of existing highlights
  const searchHighlightedHtml = useMemo(() => {
    if (!searchOpen || !searchQuery.trim() || !highlightedHtml) return highlightedHtml;
    const q = searchQuery.trim();
    // Escape regex special characters
    const escaped = q.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    const regex = new RegExp(`(${escaped})`, "gi");
    // Only replace text outside of HTML tags
    let result = "";
    let inTag = false;
    let textRun = "";
    for (const ch of highlightedHtml) {
      if (ch === "<") {
        // Flush text run with replacements
        result += textRun.replace(regex, '<mark style="background-color:#93c5fd88;border-radius:2px;padding:1px 0">$1</mark>');
        textRun = "";
        inTag = true;
        result += ch;
      } else if (ch === ">") {
        inTag = false;
        result += ch;
      } else if (inTag) {
        result += ch;
      } else {
        textRun += ch;
      }
    }
    // Flush remaining text
    result += textRun.replace(regex, '<mark style="background-color:#93c5fd88;border-radius:2px;padding:1px 0">$1</mark>');
    return result;
  }, [highlightedHtml, searchOpen, searchQuery]);

  // Client-side sanitization (defense-in-depth behind server-side ammonia)
  // applied just before injection via dangerouslySetInnerHTML. Memoized so the
  // DOMPurify pass runs only when the underlying HTML changes.
  const sanitizedChapterHtml = useMemo(
    () => sanitizeChapterHtml(searchHighlightedHtml),
    [searchHighlightedHtml],
  );
  const sanitizedChaptersHtml = useMemo(
    () => allChaptersHtml.map(sanitizeChapterHtml),
    [allChaptersHtml],
  );

  const handleCreateHighlight = useCallback(async (color: string) => {
    if (!bookId || !selectionPopup) return;
    try {
      const created = await invoke<ChapterHighlight>("add_highlight", {
        bookId,
        chapterIndex,
        text: selectionPopup.text,
        color,
        note: null,
        startOffset: selectionPopup.startOffset,
        endOffset: selectionPopup.endOffset,
      });
      setSelectionPopup(null);
      window.getSelection()?.removeAllRanges();
      await loadHighlights();
      // Offer an immediate remove affordance for the just-created highlight.
      addToast(t("reader.highlightCreated"), "success", {
        action: {
          label: t("common.remove"),
          onClick: async () => {
            try {
              await invoke("remove_highlight", { highlightId: created.id });
              // Refresh via the ref so we always load highlights for the
              // CURRENT chapter — the user may have navigated away while the
              // toast was visible. Using the captured loadHighlights closure
              // would overwrite the current chapter's state with the old one.
              await loadHighlightsRef.current();
            } catch (err) {
              addToast(friendlyError(err, t), "error");
            }
          },
        },
      });
    } catch (err) {
      addToast(friendlyError(err, t), "error");
    }
  }, [bookId, chapterIndex, selectionPopup, loadHighlights, addToast, t]);

  const handleClearHighlight = useCallback(async () => {
    if (!bookId || !selectionPopup) return;
    // Find highlights that overlap the selected range
    const overlapping = highlights.filter(
      (h) => h.startOffset < selectionPopup.endOffset && h.endOffset > selectionPopup.startOffset
    );
    try {
      for (const h of overlapping) {
        await invoke("remove_highlight", { highlightId: h.id });
      }
      setSelectionPopup(null);
      window.getSelection()?.removeAllRanges();
      await loadHighlights();
    } catch (err) {
      addToast(friendlyError(err, t), "error");
    }
  }, [bookId, selectionPopup, highlights, loadHighlights, addToast, t]);

  // ---- PDF highlight wiring (F-1-4) ----
  // PageViewer owns PDF selection geometry + its own highlight-band rendering
  // (it fetches per-page glyphs/text/highlights), so these handlers only run
  // the same IPC + toast/undo path as the EPUB flow above, then bump
  // `pdfHighlightRefresh` (declared with the other highlight state) so the
  // visible page's bands re-fetch.
  const handleCreatePdfHighlight = useCallback(
    async (color: string, sel: { text: string; startOffset: number; endOffset: number; pageIndex: number }) => {
      if (!bookId) return;
      try {
        const created = await invoke<{ id: string }>("add_highlight", {
          bookId,
          chapterIndex: sel.pageIndex,
          text: sel.text,
          color,
          note: null,
          startOffset: sel.startOffset,
          endOffset: sel.endOffset,
        });
        setPdfHighlightRefresh((k) => k + 1);
        addToast(t("reader.highlightCreated"), "success", {
          action: {
            label: t("common.remove"),
            onClick: async () => {
              try {
                await invoke("remove_highlight", { highlightId: created.id });
                setPdfHighlightRefresh((k) => k + 1);
              } catch (err) {
                addToast(friendlyError(err, t), "error");
              }
            },
          },
        });
      } catch (err) {
        addToast(friendlyError(err, t), "error");
        // Rethrow so PageViewer keeps the popup + selection for a retry
        // instead of dismissing on a failed create.
        throw err;
      }
    },
    [bookId, addToast, t],
  );
  const handleRemovePdfHighlights = useCallback(
    async (ids: string[]) => {
      try {
        for (const id of ids) {
          await invoke("remove_highlight", { highlightId: id });
        }
      } catch (err) {
        addToast(friendlyError(err, t), "error");
        throw err;
      } finally {
        // Always refresh: a partial multi-delete (some removed before one
        // threw) must still repaint bands so no deleted band lingers.
        setPdfHighlightRefresh((k) => k + 1);
      }
    },
    [addToast, t],
  );

  // ---- Navigation ----

  // Pure scroll/chapter restore — no history side effects. Shared by jump
  // navigations and history back/forward.
  const applyChapterPosition = useCallback(
    (targetChapter: number, targetScrollPosition: number) => {
      const container = scrollContainerRef.current;

      if (isContinuous && container) {
        if (targetChapter !== chapterIndex) setChapterIndex(targetChapter);
        const chapterDiv = chapterDivRefs.current[targetChapter];
        container.scrollTop = resolveBookmarkScrollTop(true, targetScrollPosition, {
          chapterOffsetTop: chapterDiv?.offsetTop ?? 0,
          chapterHeight: chapterDiv?.offsetHeight ?? 0,
          containerScrollHeight: container.scrollHeight,
        });
        return;
      }

      if (targetChapter !== chapterIndex) {
        setChapterIndex(targetChapter);
        savedScrollPosition.current = targetScrollPosition;
        restoringScroll.current = targetChapter;
      } else if (container) {
        const chapterDiv = chapterDivRefs.current[targetChapter];
        container.scrollTop = resolveBookmarkScrollTop(false, targetScrollPosition, {
          chapterOffsetTop: chapterDiv?.offsetTop ?? 0,
          chapterHeight: chapterDiv?.offsetHeight ?? 0,
          containerScrollHeight: container.scrollHeight,
          containerClientHeight: container.clientHeight,
        });
      }
    },
    [chapterIndex, isContinuous]
  );

  // Stamp the *live* reader position onto the current history entry, then
  // push the destination. This is the "browser pushState"-equivalent for
  // user-initiated jumps where the destination scroll is known up front
  // (TOC clicks, bookmark clicks).
  const recordJump = useCallback(
    (target: number, targetScroll: number = 0) => {
      const liveScroll = getChapterScrollPosition();
      const liveChapter = chapterIndex;
      setHistory((prev) =>
        pushEntry(
          replaceCurrent(prev, { position: liveChapter, meta: { scroll: liveScroll } }),
          { position: target, meta: { scroll: targetScroll } },
        ),
      );
    },
    [getChapterScrollPosition, chapterIndex]
  );

  // Variant that takes an explicit source — for paths where the destination
  // scroll is computed *after* a chapter change (e.g. cross-chapter search).
  // Capture `source` before triggering the chapter render, then call this
  // once `dest` is resolved so history stamps the correct origin.
  const recordJumpFrom = useCallback(
    (
      source: { position: number; scroll: number },
      dest: { position: number; scroll: number },
    ) => {
      setHistory((prev) =>
        pushEntry(
          replaceCurrent(prev, { position: source.position, meta: { scroll: source.scroll } }),
          { position: dest.position, meta: { scroll: dest.scroll } },
        ),
      );
    },
    []
  );
  // Ref so the search-match scroll effect can call recordJumpFrom without
  // adding a non-stable dep that would re-bind the effect on every render.
  const recordJumpFromRef = useRef(recordJumpFrom);
  useEffect(() => { recordJumpFromRef.current = recordJumpFrom; }, [recordJumpFrom]);

  const applyHistoryEntry = useCallback(
    (entry: NavigationEntry<ChapterHistoryMeta>) => {
      applyChapterPosition(entry.position, entry.meta?.scroll ?? 0);
    },
    [applyChapterPosition]
  );

  const goToChapter = useCallback(
    (index: number, opts?: { recordHistory?: boolean }) => {
      if (index >= 0 && index < totalChapters) {
        userHasInteracted.current = true;
        if (opts?.recordHistory ?? true) {
          recordJump(index, 0);
        }
        if (isContinuous) {
          const chapterDiv = chapterDivRefs.current[index];
          const container = scrollContainerRef.current;
          if (chapterDiv && container) {
            container.scrollTop = chapterDiv.offsetTop;
          }
        } else {
          setChapterIndex(index);
        }
        setTocOpen(false);
      }
    },
    [totalChapters, isContinuous, recordJump]
  );

  const navigateToBookmark = useCallback(
    (targetChapter: number, targetScrollPosition: number) => {
      setBookmarksOpen(false);
      recordJump(targetChapter, targetScrollPosition);
      applyChapterPosition(targetChapter, targetScrollPosition);
    },
    [recordJump, applyChapterPosition]
  );

  // Stamp the *live* reader position onto the cursor entry without changing
  // length, cursor, or the forward branch. Used before back/forward so the
  // entry the user is leaving reflects their actual chapter and scroll.
  const stampLiveScroll = useCallback(
    (h: NavigationHistory<ChapterHistoryMeta>): NavigationHistory<ChapterHistoryMeta> =>
      replaceCurrent(h, { position: chapterIndex, meta: { scroll: getChapterScrollPosition() } }),
    [chapterIndex, getChapterScrollPosition]
  );

  const goHistoryBack = useCallback(() => {
    const stamped = stampLiveScroll(historyRef.current);
    const { history: next, entry } = historyGoBack(stamped);
    if (!entry) return;
    setHistory(next);
    applyHistoryEntry(entry);
  }, [stampLiveScroll, applyHistoryEntry]);

  const goHistoryForward = useCallback(() => {
    const stamped = stampLiveScroll(historyRef.current);
    const { history: next, entry } = historyGoForward(stamped);
    if (!entry) return;
    setHistory(next);
    applyHistoryEntry(entry);
  }, [stampLiveScroll, applyHistoryEntry]);

  const prevChapter = useCallback(() => {
    goToChapter(chapterIndex - 1, { recordHistory: false });
  }, [chapterIndex, goToChapter]);

  const nextChapter = useCallback(() => {
    goToChapter(chapterIndex + 1, { recordHistory: false });
  }, [chapterIndex, goToChapter]);

  // ---- Focus search input when search panel opens ----

  useEffect(() => {
    if (searchOpen) {
      // Wait for the DOM to render the input before focusing
      requestAnimationFrame(() => searchInputRef.current?.focus());
    }
  }, [searchOpen]);

  // ---- Book search ----

  const executeSearch = useCallback(async (query: string) => {
    if (!bookId || !query.trim()) {
      setSearchResults([]);
      setActiveMatchIndex(-1);
      return;
    }
    setSearching(true);
    try {
      const results = await invoke<{ chapterIndex: number; snippet: string; matchOffset: number }[]>(
        "search_book_content", { bookId, query: query.trim() }
      );
      setSearchResults(results);
      setActiveMatchIndex(results.length > 0 ? 0 : -1);
    } catch {
      setSearchResults([]);
      setActiveMatchIndex(-1);
    } finally {
      setSearching(false);
    }
  }, [bookId]);

  const navigateToMatch = useCallback((index: number) => {
    if (index < 0 || index >= searchResults.length) return;
    const result = searchResults[index];
    setActiveMatchIndex(index);

    if (!isHtmlBook) {
      // PDF/CBZ/CBR: page-level navigation only (M3 wires history).
      goToChapter(result.chapterIndex);
      return;
    }

    // Capture the live source position *before* any state change so history
    // doesn't stamp the destination as the source after the render lands.
    const source = { position: chapterIndex, scroll: getChapterScrollPosition() };
    pendingSearchHistory.current = { source, destChapter: result.chapterIndex };
    targetMatchOffset.current = result.matchOffset;

    if (result.chapterIndex === chapterIndex && scrollContainerRef.current) {
      // Same chapter — scroll immediately, then commit history with the
      // achieved scroll fraction.
      requestAnimationFrame(() => {
        const container = scrollContainerRef.current;
        if (!container || targetMatchOffset.current === null) return;
        const textLen = container.textContent?.length || 1;
        container.scrollTop = (targetMatchOffset.current / textLen) * container.scrollHeight;
        targetMatchOffset.current = null;
        const pending = pendingSearchHistory.current;
        pendingSearchHistory.current = null;
        if (pending) {
          // Match the scrollProgress denominator (scrollHeight - clientHeight)
          // so back/forward restore lands at the same pixel.
          const max = Math.max(0, container.scrollHeight - container.clientHeight);
          const destScroll = max > 0 ? container.scrollTop / max : 0;
          recordJumpFrom(pending.source, {
            position: pending.destChapter,
            scroll: destScroll,
          });
        }
      });
    } else if (isContinuous) {
      // Continuous cross-chapter — match-scroll across chapters in continuous
      // mode is a pre-existing limitation (the search math is global, not
      // chapter-local). Land at the chapter top and commit immediately.
      const chapterDiv = chapterDivRefs.current[result.chapterIndex];
      const container = scrollContainerRef.current;
      if (chapterDiv && container) container.scrollTop = chapterDiv.offsetTop;
      targetMatchOffset.current = null;
      const pending = pendingSearchHistory.current;
      pendingSearchHistory.current = null;
      if (pending) {
        recordJumpFrom(pending.source, { position: pending.destChapter, scroll: 0 });
      }
    } else {
      // Paginated cross-chapter — let the targetMatchOffset useEffect scroll
      // to the match after the new chapter HTML mounts; it will commit
      // pendingSearchHistory then. setChapterIndex directly so we don't
      // re-push history via goToChapter.
      setChapterIndex(result.chapterIndex);
    }
  }, [searchResults, goToChapter, isHtmlBook, isContinuous, chapterIndex, getChapterScrollPosition, recordJumpFrom]);

  const prevMatch = useCallback(() => {
    if (searchResults.length === 0) return;
    const next = activeMatchIndex <= 0 ? searchResults.length - 1 : activeMatchIndex - 1;
    navigateToMatch(next);
  }, [searchResults, activeMatchIndex, navigateToMatch]);

  const nextMatch = useCallback(() => {
    if (searchResults.length === 0) return;
    const next = activeMatchIndex >= searchResults.length - 1 ? 0 : activeMatchIndex + 1;
    navigateToMatch(next);
  }, [searchResults, activeMatchIndex, navigateToMatch]);

  // ---- Keyboard shortcuts ----

  const addBookmarkAtCurrentPosition = useCallback(async () => {
    if (!bookId) return;
    try {
      // HTML-reflowable books (EPUB + MOBI) store a chapter-local scroll
      // fraction — `getChapterScrollPosition()` produces the same coordinate
      // system that `saveProgress` uses, so bookmarks and reading progress
      // round-trip consistently. In continuous mode `scrollProgress` would
      // be book-global, which mismatched the restore path and landed the
      // user far from the saved passage. Page-based books (PDF + CBZ + CBR)
      // store page-fraction.
      const bookmark = await invoke<{ id: string }>("add_bookmark", {
        bookId,
        chapterIndex,
        scrollPosition: isHtmlBook
          ? getChapterScrollPosition()
          : pageCount > 0 ? chapterIndex / pageCount : 0,
      });
      setToastBookmarkId(bookmark.id);
    } catch {
      // silently fail
    }
  }, [bookId, chapterIndex, getChapterScrollPosition, isHtmlBook, pageCount]);

  useEffect(() => {
    function handleKeyDown(e: KeyboardEvent) {
      // Cmd/Ctrl+F — open book search (HTML-reflowable books + PDF)
      if ((e.metaKey || e.ctrlKey) && e.key === "f" && (isHtmlBook || bookFormat === "pdf")) {
        e.preventDefault();
        setSearchOpen(true);
        setSearchQuery("");
        setSearchResults([]);
        return;
      }

      // Close search panel from any context (including when input is focused)
      if (e.key === "Escape" && searchOpen) {
        setSearchOpen(false); setSearchQuery(""); setSearchResults([]); setActiveMatchIndex(-1);
        return;
      }

      const tag = (e.target as HTMLElement)?.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA") return;

      // The read-only book-details popup (BookDetailModal) is modal: while it
      // is open no reader shortcut or navigation key should fire. The modal
      // owns its own Escape/Tab handling; swallow everything else here so a
      // stray 'b'/'d'/'t' can't mutate bookmarks / toggle chrome behind it.
      // (PageViewer's own key listener is separately gated via keyboardEnabled.)
      if (infoOpen) return;

      // Let SettingsPanel handle Escape and Tab when it is open
      if (settingsOpen && (e.key === "Escape" || e.key === "Tab")) return;

      // Alt+Left / Alt+Right → navigation history (jump back / forward).
      // Browser-style binding; takes precedence over chapter/page prev/next.
      if (e.altKey && (e.key === "ArrowLeft" || e.key === "ArrowRight")) {
        e.preventDefault();
        if (e.key === "ArrowLeft") goHistoryBack();
        else goHistoryForward();
        return;
      }

      // Don't navigate chapters when any panel is open
      if ((settingsOpen || tocOpen || bookmarksOpen) && (e.key === "ArrowLeft" || e.key === "ArrowRight")) return;

      // For image-based formats (CBZ/CBR/PDF), PageViewer handles arrow keys.
      // HTML books (EPUB + MOBI) use chapter navigation here.
      if (!isHtmlBook && (e.key === "ArrowLeft" || e.key === "ArrowRight")) return;

      if (e.key === "ArrowLeft") {
        prevChapter();
      } else if (e.key === "ArrowRight") {
        nextChapter();
      } else if (e.key === "t" && !e.metaKey && !e.ctrlKey) {
        setTocOpen((prev) => !prev);
      } else if (e.key === "b" && !e.metaKey && !e.ctrlKey) {
        addBookmarkAtCurrentPosition();
      } else if (e.key === "d" && !e.metaKey && !e.ctrlKey) {
        setDndMode((prev) => !prev);
      } else if (e.key === "m" && !e.metaKey && !e.ctrlKey && !isHtmlBook) {
        toggleThumbStrip();
      } else if (e.key === "\\" && !e.metaKey && !e.ctrlKey && onToggleSplit) {
        onToggleSplit();
      } else if (e.key === "?" || (e.key === "/" && e.shiftKey)) {
        setShowShortcuts((prev) => !prev);
      } else if (e.key === "Escape") {
        if (dndMode) { setDndMode(false); return; }
        if (showShortcuts) setShowShortcuts(false);
        else if (bookmarksOpen) setBookmarksOpen(false);
        else if (tocOpen) setTocOpen(false);
        else navigate("/");
      }
    }

    // Only the active pane binds the window-level listener. In split
    // mode the Reader shell tracks which pane was clicked last and
    // routes keys to it via the `isActive` prop. Without this guard
    // every arrow key (and every shortcut) would fire on both panes
    // at once.
    if (!effectiveActive) return;
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [prevChapter, nextChapter, goHistoryBack, goHistoryForward, addBookmarkAtCurrentPosition, showShortcuts, tocOpen, bookmarksOpen, infoOpen, dndMode, settingsOpen, navigate, bookFormat, isHtmlBook, searchOpen, toggleThumbStrip, onToggleSplit, effectiveActive]);

  // ---- TOC focus trap ----

  useEffect(() => {
    if (!tocOpen) return;
    // Use the ref instead of `getElementById` so each ReaderPane traps
    // focus on its own sidebar — in split mode both panes can render a
    // `<aside>` with the same role, so a global id lookup would always
    // resolve to the primary pane's sidebar.
    const sidebar = tocSidebarRef.current;
    if (!sidebar) return;

    const focusable = sidebar.querySelectorAll<HTMLElement>(
      'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])'
    );
    const first = focusable[0];
    const last = focusable[focusable.length - 1];

    function trapFocus(e: KeyboardEvent) {
      if (e.key !== "Tab") return;
      if (e.shiftKey) {
        if (document.activeElement === first) {
          e.preventDefault();
          last?.focus();
        }
      } else {
        if (document.activeElement === last) {
          e.preventDefault();
          first?.focus();
        }
      }
    }

    first?.focus();
    document.addEventListener("keydown", trapFocus);
    return () => document.removeEventListener("keydown", trapFocus);
  }, [tocOpen]);

  // ---- Track bottom nav visibility for floating arrows ----

  useEffect(() => {
    const el = chapterNavRef.current;
    const container = scrollContainerRef.current;
    if (!el || !container) return;

    const observer = new IntersectionObserver(
      ([entry]) => setBottomNavVisible(entry.isIntersecting),
      { root: container, threshold: 0.1 }
    );
    observer.observe(el);
    return () => observer.disconnect();
  }, [chapterHtml]);

  // ---- DND mode: auto-hide cursor & reveal controls on edge hover ----

  useEffect(() => {
    if (!dndMode) {
      setDndCursorHidden(false);
      setDndShowControls(false);
      if (dndTimerRef.current) clearTimeout(dndTimerRef.current);
      return;
    }

    function handleMouseMove(e: MouseEvent) {
      setDndCursorHidden(false);
      // Show controls when mouse is near top or bottom edge (48px)
      const nearEdge = e.clientY < 48 || e.clientY > window.innerHeight - 48;
      setDndShowControls(nearEdge);

      if (dndTimerRef.current) clearTimeout(dndTimerRef.current);
      dndTimerRef.current = setTimeout(() => {
        setDndCursorHidden(true);
        setDndShowControls(false);
      }, 2000);
    }

    document.addEventListener("mousemove", handleMouseMove);
    // Start the hide timer immediately
    dndTimerRef.current = setTimeout(() => {
      setDndCursorHidden(true);
    }, 2000);

    return () => {
      document.removeEventListener("mousemove", handleMouseMove);
      if (dndTimerRef.current) clearTimeout(dndTimerRef.current);
    };
  }, [dndMode]);

  // ---- Current chapter title ----

  const currentChapterTitle =
    toc.find((entry) => entry.chapter_index === chapterIndex)?.label ??
    t("reader.chapterDefault", { number: chapterIndex + 1 });

  // ---- Font family CSS value ----

  const fontFamilyCss =
    fontFamily === "serif"
      ? '"Lora Variable", Georgia, serif'
      : fontFamily === "literata"
        ? '"Literata Variable", Georgia, serif'
        : fontFamily === "dyslexic"
          ? '"OpenDyslexic", sans-serif'
          : fontFamily.startsWith("custom:")
            ? `"CustomFont-${fontFamily.slice(7)}", serif`
            : '"DM Sans Variable", system-ui, sans-serif';

  const readerContentStyle: React.CSSProperties = {
    fontSize: `${fontSize}px`,
    lineHeight: typography.lineHeight,
    fontFamily: fontFamilyCss,
  };

  // ---- Time-to-finish estimates ----

  const timeEstimate = useMemo(() => {
    if (chapterWordCounts.length === 0 || !isHtmlBook) return null;
    const currentChapterWords = chapterWordCounts[chapterIndex] ?? 0;
    // In continuous mode, scrollProgress is book-global; use chapter-local fraction instead
    const chapterProgress = isContinuous ? getChapterScrollPosition() : scrollProgress;
    const wordsLeftInChapter = Math.round(currentChapterWords * (1 - chapterProgress));
    const wordsLeftInBook = chapterWordCounts
      .slice(chapterIndex + 1)
      .reduce((sum, w) => sum + w, 0) + wordsLeftInChapter;

    if (wordsLeftInBook === 0) return null;

    const minsLeftChapter = Math.max(1, Math.round(wordsLeftInChapter / READING_WPM));
    const minsLeftBook = Math.max(1, Math.round(wordsLeftInBook / READING_WPM));

    const formatTime = (mins: number) => {
      if (mins < 60) return `${mins} min`;
      const h = Math.floor(mins / 60);
      const m = mins % 60;
      return m > 0 ? `${h}h ${m}m` : `${h}h`;
    };

    return {
      chapter: formatTime(minsLeftChapter),
      book: formatTime(minsLeftBook),
    };
  }, [chapterWordCounts, chapterIndex, scrollProgress, isHtmlBook, isContinuous, getChapterScrollPosition]);

  // ---- Render ----

  // A missing underlying file is a terminal condition reachable from either the
  // initial load or a chapter fetch. Render the recovery dialog from a single
  // place so the user sees one clear dialog rather than two stages.
  if (missingFileDialog) {
    return (
      <MissingFileDialog
        onCancel={() => {
          setMissingFileDialog(false);
          navigate("/");
        }}
        onRemove={async () => {
          try {
            await invoke("remove_book", { bookId });
          } catch {
            // Already gone or other error — navigate away regardless
          }
          navigate("/");
        }}
      />
    );
  }

  if (loading) {
    return (
      <div className="flex items-center justify-center h-full bg-paper">
        <div className="text-sm text-ink-muted">{t("reader.loading")}</div>
      </div>
    );
  }

  if (error) {
    return (
      <div
        className="relative flex flex-col items-center justify-center h-full gap-4 p-8 bg-paper"
        role="alert"
        aria-live="assertive"
      >
        <h1 className="text-ink font-medium" id="reader-error-title">
          {t("reader.failedToLoad")}
        </h1>
        <p
          className="text-ink-muted text-sm max-w-md text-center"
          aria-describedby="reader-error-title"
        >
          {error}
        </p>
        <button
          onClick={() => navigate("/")}
          className="px-4 py-2 bg-accent text-white rounded-xl hover:bg-accent-hover transition-colors text-sm font-medium"
        >
          {t("reader.backToLibrary")}
        </button>
      </div>
    );
  }

  const showHeader = !dndMode || dndShowControls;
  const showFooter = !dndMode || dndShowControls;

  // Split-mode active-pane indicator: when the user is split-viewing,
  // a subtle accent ring on the active pane shows which one will
  // receive arrow keys / shortcuts. Outside split mode the pane is
  // implicitly active and the ring is hidden.
  const paneActivationClass = splitMode
    ? effectiveActive
      ? "ring-1 ring-inset ring-accent/40 transition-shadow"
      : "ring-1 ring-inset ring-transparent transition-shadow"
    : "";

  return (
    <div
      onMouseDown={onActivate}
      className={`flex h-full relative bg-paper ${dndCursorHidden ? "cursor-none" : ""} ${paneActivationClass}`}
    >
      {/* Auto-focus hint overlay */}
      {focusHint && (
        <div className="absolute inset-x-0 top-4 z-50 flex justify-center pointer-events-none animate-fade-in">
          <div className="bg-ink/80 text-paper text-sm px-4 py-2 rounded-lg shadow-lg backdrop-blur-sm">
            {t("reader.focusHint")}
          </div>
        </div>
      )}

      {/* Keyboard shortcuts help */}
      {showShortcuts && (
        <KeyboardShortcutsHelp context="reader" onClose={() => setShowShortcuts(false)} />
      )}

      {/* Highlights Panel */}
      {highlightsOpen && (
        <HighlightsPanel
          bookId={bookId!}
          onClose={() => setHighlightsOpen(false)}
          onGoToChapter={(index) => { goToChapter(index); setHighlightsOpen(false); }}
          onMutate={() => { loadHighlightsRef.current(); setPdfHighlightRefresh((k) => k + 1); }}
        />
      )}

      {/* Bookmarks Panel */}
      {bookmarksOpen && (
        <BookmarksPanel
          bookId={bookId!}
          currentChapterIndex={chapterIndex}
          toc={toc}
          onClose={() => setBookmarksOpen(false)}
          onNavigate={navigateToBookmark}
          refreshKey={bookmarkRefreshKey}
        />
      )}

      {/* Book details (read-only) popup */}
      {infoOpen && bookDetail && (
        <BookDetailModal book={bookDetail} onClose={() => setInfoOpen(false)} />
      )}

      {/* Share as image (F-1-6) */}
      {shareCard && (
        <ShareCardDialog
          quote={shareCard.quote}
          title={shareCard.title}
          author={shareCard.author}
          coverPath={shareCard.coverPath}
          initialMode={shareCard.initialMode}
          onClose={() => setShareCard(null)}
        />
      )}

      {/* Bookmark toast */}
      {toastBookmarkId && (
        <BookmarkToast
          bookmarkId={toastBookmarkId}
          onDismiss={() => {
            setToastBookmarkId(null);
            setBookmarkRefreshKey((k) => k + 1);
          }}
        />
      )}

      {/* Progress saved indicator */}
      {saveIndicator && (
        <div className="fixed bottom-16 right-4 z-50 text-xs text-gray-400 dark:text-gray-500 transition-opacity">
          {t("reader.progressSaved")}
        </div>
      )}

      {/* Progress save error indicator */}
      {saveError && (
        <div className="fixed bottom-16 right-4 z-50 text-xs text-amber-500 dark:text-amber-400 transition-opacity">
          {t("reader.progressNotSaved")}
        </div>
      )}

      {/* TOC Sidebar — slide-in animation */}
      {tocOpen && (
        <>
          {/* Backdrop */}
          <div
            className="absolute inset-0 bg-ink/20 backdrop-blur-sm z-10 animate-fade-in"
            onClick={() => setTocOpen(false)}
          />
          {/* Sidebar */}
          <aside ref={tocSidebarRef} role="dialog" aria-modal="true" aria-label={t("reader.contents")} className="absolute left-0 top-0 bottom-0 w-72 bg-surface border-r border-warm-border z-20 flex flex-col shadow-[4px_0_24px_-4px_rgba(44,34,24,0.12)] animate-slide-in-left">
            <div className="px-5 py-4 border-b border-warm-border flex items-center justify-between">
              <h2 className="font-serif text-base font-semibold text-ink">
                {t("reader.contents")}
              </h2>
              <button
                onClick={() => setTocOpen(false)}
                className="p-1 text-ink-muted hover:text-ink transition-colors rounded focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
                aria-label={t("reader.closeToc")}
              >
                <svg width="18" height="18" viewBox="0 0 20 20" fill="none">
                  <path d="M15 5L5 15M5 5l10 10" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
                </svg>
              </button>
            </div>

            {/* Book title */}
            {bookTitle && (
              <div className="px-5 py-3 border-b border-warm-border">
                <p className="font-serif text-sm font-medium text-ink leading-snug truncate" title={bookTitle}>{bookTitle}</p>
              </div>
            )}

            <nav
              className="flex-1 overflow-y-auto py-2"
              aria-label="Table of contents"
            >
              {toc.length > 0 ? toc.map((entry) => (
                <TocItem
                  key={`${entry.chapter_index}-${entry.label}`}
                  entry={entry}
                  currentIndex={chapterIndex}
                  onSelect={goToChapter}
                  depth={0}
                />
              )) : (
                <div className="px-5 py-2 space-y-3">
                  {Array.from({ length: 8 }, (_, i) => (
                    <div key={i} className="flex flex-col gap-1.5">
                      <div className="h-3 rounded bg-warm-subtle animate-shimmer" style={{ width: `${60 + (i * 7) % 30}%` }} />
                    </div>
                  ))}
                </div>
              )}
            </nav>
          </aside>
        </>
      )}

      {/* Main reading area */}
      <div className="flex flex-col flex-1 min-w-0">
        {/* Header */}
        <header className={`flex items-center gap-2 px-4 py-2.5 border-b border-warm-border bg-surface shrink-0 transition-all duration-300 ${showHeader ? "opacity-100 max-h-20" : "opacity-0 max-h-0 overflow-hidden py-0 border-b-0"}`}>
          <button
            onClick={() => navigate("/")}
            className="p-1.5 text-ink-muted hover:text-ink transition-colors rounded-lg hover:bg-warm-subtle focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
            aria-label={t("reader.backToLibrary")}
          >
            <svg width="18" height="18" viewBox="0 0 20 20" fill="none">
              <path d="M12 4l-6 6 6 6" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
          </button>

          <button
            onClick={() => setTocOpen(true)}
            className="p-1.5 text-ink-muted hover:text-ink transition-colors rounded-lg hover:bg-warm-subtle focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
            aria-label={t("reader.openToc")}
          >
            <svg width="18" height="18" viewBox="0 0 20 20" fill="none">
              <path d="M3 5h14M3 10h14M3 15h14" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
            </svg>
          </button>

          {/* Navigation history — available for all formats */}
          <button
            onClick={goHistoryBack}
            disabled={!canGoBack(history)}
            className="p-1.5 text-ink-muted hover:text-ink transition-colors rounded-lg hover:bg-warm-subtle focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 disabled:opacity-30 disabled:cursor-not-allowed disabled:hover:bg-transparent disabled:hover:text-ink-muted"
            aria-label={t("reader.historyBack")}
            title={t("reader.historyBackShortcut")}
          >
            <svg width="18" height="18" viewBox="0 0 20 20" fill="none">
              <path d="M11 4l-5 6 5 6M6 10h10" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
          </button>
          <button
            onClick={goHistoryForward}
            disabled={!canGoForward(history)}
            className="p-1.5 text-ink-muted hover:text-ink transition-colors rounded-lg hover:bg-warm-subtle focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 disabled:opacity-30 disabled:cursor-not-allowed disabled:hover:bg-transparent disabled:hover:text-ink-muted"
            aria-label={t("reader.historyForward")}
            title={t("reader.historyForwardShortcut")}
          >
            <svg width="18" height="18" viewBox="0 0 20 20" fill="none">
              <path d="M9 4l5 6-5 6M14 10H4" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
          </button>

          <h1 className="flex-1 text-sm text-ink-muted truncate font-medium px-1">
            {currentChapterTitle}
          </h1>

          {/* Search button (HTML-reflowable books + PDF) */}
          {(isHtmlBook || bookFormat === "pdf") && (
            <button
              onClick={() => { setSearchOpen(true); setSearchQuery(""); setSearchResults([]); }}
              className={`p-1.5 transition-colors rounded-lg focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 ${searchOpen ? "text-accent bg-accent-light" : "text-ink-muted hover:text-ink hover:bg-warm-subtle"}`}
              aria-label={t("reader.searchLabel")}
              title={t("reader.searchShortcut")}
            >
              <svg width="18" height="18" viewBox="0 0 20 20" fill="none">
                <circle cx="9" cy="9" r="5.5" stroke="currentColor" strokeWidth="1.5" />
                <path d="M13 13l4 4" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
              </svg>
            </button>
          )}

          {/* Highlights button */}
          <button
            onClick={() => setHighlightsOpen((prev) => !prev)}
            className={`p-1.5 transition-colors rounded-lg focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 ${highlightsOpen ? "text-accent bg-accent-light" : "text-ink-muted hover:text-ink hover:bg-warm-subtle"}`}
            aria-label={t("highlights.title")}
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none">
              <path d="M15.232 5.232l3.536 3.536m-2.036-5.036a2.5 2.5 0 113.536 3.536L6.5 21.036H3v-3.572L16.732 3.732z" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
          </button>

          {/* Bookmarks button */}
          <button
            onClick={() => setBookmarksOpen((prev) => !prev)}
            className={`p-1.5 transition-colors rounded-lg focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 ${bookmarksOpen ? "text-accent bg-accent-light" : "text-ink-muted hover:text-ink hover:bg-warm-subtle"}`}
            aria-label={t("bookmarks.title")}
            title={t("bookmarks.title")}
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
              <path d="M19 21l-7-3.5L5 21V5a2 2 0 012-2h10a2 2 0 012 2z" />
            </svg>
          </button>

          {/* Book details (info) button */}
          <button
            onClick={() => setInfoOpen(true)}
            disabled={!bookDetail}
            className={`p-1.5 transition-colors rounded-lg focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 disabled:opacity-40 ${infoOpen ? "text-accent bg-accent-light" : "text-ink-muted hover:text-ink hover:bg-warm-subtle"}`}
            aria-label={t("reader.bookInfo")}
            title={t("reader.bookInfo")}
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
              <circle cx="12" cy="12" r="9" />
              <path d="M12 16v-4M12 8h.01" />
            </svg>
          </button>

          <div className="w-px h-5 bg-warm-border/60 mx-0.5 shrink-0" aria-hidden="true" />

          {/* Font size controls */}
          <div className="flex items-center gap-0.5 mr-1">
            <button
              onClick={() => setFontSize(fontSize - 2)}
              disabled={fontSize <= MIN_FONT_SIZE}
              className="px-2 py-1 text-xs text-ink-muted hover:text-ink hover:bg-warm-subtle rounded transition-colors disabled:opacity-50 disabled:cursor-not-allowed focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
              aria-label={t("reader.decreaseFontSize")}
            >
              A−
            </button>
            <span className="text-xs text-ink-muted w-7 text-center tabular-nums">
              {fontSize}
            </span>
            <button
              onClick={() => setFontSize(fontSize + 2)}
              disabled={fontSize >= MAX_FONT_SIZE}
              className="px-2 py-1 text-xs text-ink-muted hover:text-ink hover:bg-warm-subtle rounded transition-colors disabled:opacity-50 disabled:cursor-not-allowed focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
              aria-label={t("reader.increaseFontSize")}
            >
              A+
            </button>
          </div>

          {/* Dual-page toggle — hidden in continuous scroll mode */}
          {!isContinuous && (
            <div className="flex items-center">
              <button
                onClick={() => setDualPage(!dualPage)}
                className={`p-1.5 transition-colors rounded-lg focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 ${dualPage ? "text-accent bg-accent-light" : "text-ink-muted hover:text-ink hover:bg-warm-subtle"}`}
                aria-label={t("reader.toggleDualPage")}
                title={t("reader.dualPageSpread")}
              >
                <svg width="18" height="18" viewBox="0 0 24 24" fill="none">
                  <rect x="2" y="4" width="8" height="16" rx="1" stroke="currentColor" strokeWidth="1.5" />
                  <rect x="14" y="4" width="8" height="16" rx="1" stroke="currentColor" strokeWidth="1.5" />
                </svg>
              </button>
              {dualPage && (
                <button
                  onClick={() => setMangaMode(!mangaMode)}
                  className={`p-1.5 transition-colors rounded-lg focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 ${mangaMode ? "text-accent bg-accent-light" : "text-ink-muted hover:text-ink hover:bg-warm-subtle"}`}
                  aria-label={t("reader.toggleMangaMode")}
                  title={t("reader.mangaMode")}
                >
                  <svg width="18" height="18" viewBox="0 0 24 24" fill="none">
                    <path d="M19 12H5" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
                    <path d="M10 7l-5 5 5 5" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
                  </svg>
                </button>
              )}
              {!isHtmlBook && (
                <button
                  onClick={toggleThumbStrip}
                  className={`p-1.5 transition-colors rounded-lg focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 ${thumbStripOpen ? "text-accent bg-accent-light" : "text-ink-muted hover:text-ink hover:bg-warm-subtle"}`}
                  aria-label={t("reader.toggleThumbStrip")}
                  title={t("reader.thumbStripTitle")}
                  aria-pressed={thumbStripOpen}
                >
                  <svg width="18" height="18" viewBox="0 0 24 24" fill="none">
                    <rect x="3" y="6" width="4" height="12" rx="1" stroke="currentColor" strokeWidth="1.5" />
                    <rect x="10" y="6" width="4" height="12" rx="1" stroke="currentColor" strokeWidth="1.5" />
                    <rect x="17" y="6" width="4" height="12" rx="1" stroke="currentColor" strokeWidth="1.5" />
                  </svg>
                </button>
              )}
            </div>
          )}

          {/* Close-this-pane (X) — only rendered when the shell
              wires the callback (companion pane in split mode). Lets
              the user collapse back to single-pane view without
              hunting for the split-toggle button. */}
          {onCloseThisPane && (
            <button
              onClick={onCloseThisPane}
              className="p-1.5 text-ink-muted hover:text-ink hover:bg-warm-subtle transition-colors rounded-lg focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
              aria-label={t("reader.closePane")}
              title={t("reader.closePane")}
            >
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none">
                <path d="M6 6l12 12M18 6L6 18" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
              </svg>
            </button>
          )}

          {/* "Choose another book" — only rendered on the companion
              pane in split mode. Lets the user pair two different
              books in split view. */}
          {!isPrimary && onChangeBook && (
            <button
              onClick={onChangeBook}
              className="p-1.5 text-ink-muted hover:text-ink hover:bg-warm-subtle transition-colors rounded-lg focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
              aria-label={t("reader.chooseAnotherBook")}
              title={t("reader.chooseAnotherBook")}
            >
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none">
                <path d="M4 5h12v14H4z" stroke="currentColor" strokeWidth="1.5" strokeLinejoin="round" />
                <path d="M8 9h4M8 12h4M8 15h2" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
                <circle cx="18" cy="6" r="3" stroke="currentColor" strokeWidth="1.5" fill="none" />
                <path d="M18 4.5v3M16.5 6h3" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
              </svg>
            </button>
          )}

          {/* Swap panes — only rendered on the primary pane in split
              mode once a companion has been chosen. */}
          {isPrimary && onSwapPanes && (
            <button
              onClick={onSwapPanes}
              className="p-1.5 text-ink-muted hover:text-ink hover:bg-warm-subtle transition-colors rounded-lg focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
              aria-label={t("reader.swapPanes")}
              title={t("reader.swapPanes")}
            >
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none">
                <path d="M7 8h13l-3-3M17 16H4l3 3" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
              </svg>
            </button>
          )}

          {/* Split-view toggle — only rendered on the primary pane. */}
          {isPrimary && onToggleSplit && (
            <button
              onClick={onToggleSplit}
              className={`p-1.5 transition-colors rounded-lg focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 ${splitMode ? "text-accent bg-accent-light" : "text-ink-muted hover:text-ink hover:bg-warm-subtle"}`}
              aria-label={t("reader.toggleSplitView")}
              title={t("reader.splitViewTitle")}
              aria-pressed={splitMode}
            >
              {/* One viewport split by a vertical divider — visually
                  distinct from the dual-page-spread icon (two book
                  pages side by side) and the thumbnail-strip icon
                  (three vertical bars). */}
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none">
                <rect x="3" y="5" width="18" height="14" rx="2" stroke="currentColor" strokeWidth="1.5" />
                <path d="M12 5v14" stroke="currentColor" strokeWidth="1.5" />
              </svg>
            </button>
          )}

          <div className="w-px h-5 bg-warm-border/60 mx-0.5 shrink-0" aria-hidden="true" />

          {/* App group: low-frequency actions tucked into an overflow menu so
              the header stays grouped rather than a flat row of icons. */}
          <OverflowMenu label={t("reader.moreActions")}>
            <button
              onClick={() => setDndMode((prev) => !prev)}
              role="menuitem"
              className={`flex items-center gap-2 px-3 py-2 text-sm transition-colors text-left w-full hover:bg-warm-subtle ${dndMode ? "text-accent" : "text-ink-muted hover:text-ink"}`}
            >
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" className="shrink-0">
                <circle cx="12" cy="12" r="9" stroke="currentColor" strokeWidth="1.5" />
                <path d="M12 8v4l2.5 2.5" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
              </svg>
              {t("reader.focusMode")}
            </button>
            <button
              onClick={() => setShowShortcuts(true)}
              role="menuitem"
              className="flex items-center gap-2 px-3 py-2 text-sm text-ink-muted hover:text-ink hover:bg-warm-subtle transition-colors text-left w-full"
            >
              <span className="w-[18px] text-center font-medium shrink-0">?</span>
              {t("shortcuts.title")}
            </button>
          </OverflowMenu>

          {/* "Don't track this session" — app-global (one shared backend
              flag), so only the primary pane renders it; the companion
              pane would otherwise show a second, redundant control for
              the same state. */}
          {isPrimary && <PrivateModeToggle />}

          {/* Language switcher */}
          <LanguageSwitcher />

          {/* Settings button */}
          <button
            onClick={onOpenSettings}
            className={`p-1.5 transition-colors rounded-lg focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 ${settingsOpen ? "text-accent bg-accent-light" : "text-ink-muted hover:text-ink hover:bg-warm-subtle"}`}
            aria-label={t("reader.openSettings")}
          >
            <svg width="18" height="18" viewBox="0 0 20 20" fill="none">
              <path
                d="M10 12.5a2.5 2.5 0 100-5 2.5 2.5 0 000 5z"
                stroke="currentColor"
                strokeWidth="1.5"
              />
              <path
                d="M16.2 12.3a1.3 1.3 0 00.26 1.43l.05.05a1.58 1.58 0 11-2.23 2.23l-.05-.05a1.3 1.3 0 00-1.43-.26 1.3 1.3 0 00-.79 1.19v.14a1.58 1.58 0 01-3.16 0v-.07a1.3 1.3 0 00-.85-1.19 1.3 1.3 0 00-1.43.26l-.05.05a1.58 1.58 0 11-2.23-2.23l.05-.05a1.3 1.3 0 00.26-1.43 1.3 1.3 0 00-1.19-.79h-.14a1.58 1.58 0 010-3.16h.07a1.3 1.3 0 001.19-.85 1.3 1.3 0 00-.26-1.43l-.05-.05a1.58 1.58 0 112.23-2.23l.05.05a1.3 1.3 0 001.43.26h.06a1.3 1.3 0 00.79-1.19v-.14a1.58 1.58 0 013.16 0v.07a1.3 1.3 0 00.79 1.19 1.3 1.3 0 001.43-.26l.05-.05a1.58 1.58 0 112.23 2.23l-.05.05a1.3 1.3 0 00-.26 1.43v.06a1.3 1.3 0 001.19.79h.14a1.58 1.58 0 010 3.16h-.07a1.3 1.3 0 00-1.19.79z"
                stroke="currentColor"
                strokeWidth="1.5"
              />
            </svg>
          </button>
        </header>

        {/* Book search panel */}
        {searchOpen && (isHtmlBook || bookFormat === "pdf") && (
          <div className="shrink-0 border-b border-warm-border bg-surface px-4 py-2 flex items-center gap-2">
            <svg width="14" height="14" viewBox="0 0 20 20" fill="none" className="text-ink-muted shrink-0">
              <circle cx="9" cy="9" r="6" stroke="currentColor" strokeWidth="2" />
              <path d="M13.5 13.5L17 17" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
            </svg>
            <input
              ref={searchInputRef}
              type="text"
              value={searchQuery}
              onChange={(e) => {
                setSearchQuery(e.target.value);
                if (e.target.value.trim().length >= 2) {
                  executeSearch(e.target.value);
                } else {
                  setSearchResults([]);
                }
              }}
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  setSearchOpen(false);
                  setSearchQuery("");
                  setSearchResults([]);
                  setActiveMatchIndex(-1);
                } else if (e.key === "Enter" && searchResults.length > 0) {
                  e.shiftKey ? prevMatch() : nextMatch();
                } else if (e.key === "Enter") {
                  executeSearch(searchQuery);
                }
              }}
              placeholder={t("reader.searchInBook")}
              className="flex-1 text-sm bg-transparent text-ink placeholder-ink-muted/50 focus:outline-none"
              autoFocus
            />
            {searching && <span className="text-xs text-ink-muted">{t("reader.searchingText")}</span>}
            {!searching && searchQuery.length > 0 && searchQuery.trim().length < 2 && (
              <span className="text-xs text-ink-muted/60">{t("reader.searchMinChars")}</span>
            )}
            {!searching && searchResults.length > 0 && (
              <div className="flex items-center gap-1">
                <span className="text-xs text-ink-muted tabular-nums">
                  {t("reader.matchNav", { current: activeMatchIndex + 1, total: searchResults.length })}
                </span>
                <button type="button" onClick={prevMatch} className="p-0.5 text-ink-muted hover:text-ink transition-colors" aria-label={t("reader.prevMatch")} title={t("reader.searchNavHint")}>
                  <svg width="14" height="14" viewBox="0 0 20 20" fill="none"><path d="M12 15l-5-5 5-5" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" /></svg>
                </button>
                <button type="button" onClick={nextMatch} className="p-0.5 text-ink-muted hover:text-ink transition-colors" aria-label={t("reader.nextMatch")} title={t("reader.searchNavHint")}>
                  <svg width="14" height="14" viewBox="0 0 20 20" fill="none"><path d="M8 5l5 5-5 5" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" /></svg>
                </button>
              </div>
            )}
            <button
              type="button"
              onClick={() => { setSearchOpen(false); setSearchQuery(""); setSearchResults([]); setActiveMatchIndex(-1); }}
              className="p-1 text-ink-muted hover:text-ink transition-colors"
              aria-label={t("reader.closeSearch")}
            >
              <svg width="14" height="14" viewBox="0 0 20 20" fill="none">
                <path d="M15 5L5 15M5 5l10 10" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
              </svg>
            </button>
          </div>
        )}

        {/* Search results dropdown */}
        {searchOpen && !searching && searchQuery.trim().length >= 2 && searchResults.length === 0 && (
          <div className="shrink-0 border-b border-warm-border bg-surface/95 px-4 py-3">
            <p className="text-xs text-ink-muted text-center">{t("reader.noMatchesFor", { query: searchQuery.trim() })}</p>
          </div>
        )}
        {searchOpen && searchResults.length > 0 && (
          <div className="shrink-0 max-h-48 overflow-y-auto border-b border-warm-border bg-surface/95">
            {searchResults.map((result, i) => {
              const chapterTitle = bookFormat === "pdf"
                ? t("reader.pageNumber", { number: result.chapterIndex + 1 })
                : toc.find((e) => e.chapter_index === result.chapterIndex)?.label ?? t("reader.chapterDefault", { number: result.chapterIndex + 1 });
              return (
                <button
                  key={`${result.chapterIndex}-${result.matchOffset}-${i}`}
                  type="button"
                  onClick={() => {
                    navigateToMatch(i);
                    setSearchOpen(false);
                  }}
                  className={`w-full text-left px-4 py-2 hover:bg-warm-subtle transition-colors border-b border-warm-border/50 last:border-b-0 ${i === activeMatchIndex ? "bg-accent-light" : ""}`}
                >
                  <span className="text-[11px] text-accent font-medium">{chapterTitle}</span>
                  <p className="text-xs text-ink-muted mt-0.5 line-clamp-2">{result.snippet}</p>
                </button>
              );
            })}
            {searchResults.length >= 200 && (
              <div className="px-4 py-2 text-[11px] text-ink-muted/70 text-center bg-warm-subtle/50">
                {t("reader.resultsCapped")}
              </div>
            )}
          </div>
        )}

        {/* Screen reader announcement for chapter changes (#56) */}
        {isHtmlBook && (
          <div aria-live="polite" aria-atomic="true" className="sr-only">
            {t("reader.chapterOf", { current: chapterIndex + 1, total: totalChapters || 1 })}
          </div>
        )}

        {/* Screen reader announcement for search state */}
        {searchOpen && (
          <div aria-live="polite" aria-atomic="true" className="sr-only">
            {searching && t("reader.searchingText")}
            {!searching && searchResults.length > 0 && t("reader.matchNav", { current: activeMatchIndex + 1, total: searchResults.length })}
            {!searching && searchQuery.trim().length >= 2 && searchResults.length === 0 && t("reader.noMatchesFor", { query: searchQuery.trim() })}
          </div>
        )}

        {/* Content area — chapter HTML (EPUB/MOBI) or page-based viewer */}
        {!isHtmlBook ? (
          pageCount > 0 ? (
            <div className="relative flex flex-col flex-1 min-h-0">
              <PageViewer
                bookId={bookId!}
                format={bookFormat}
                totalPages={pageCount}
                initialPage={chapterIndex}
                onPageChange={(index) => setChapterIndex(index)}
                onPageJump={(target) => recordJump(target, 0)}
                dualPage={dualPage}
                mangaMode={mangaMode}
                pageAnimation={pageAnimation}
                keyboardEnabled={effectiveActive && !infoOpen}
                onCreateHighlight={handleCreatePdfHighlight}
                onRemoveHighlights={handleRemovePdfHighlights}
                highlightRefreshKey={pdfHighlightRefresh}
              />
              {/* Non-blocking, dismissible background page-prerender bar.
                  Floats over the page viewer; never gates reading. Shared by
                  comic extraction (F-4-1) and PDF prerender (F-4-5) — same bar,
                  format-appropriate label. */}
              {(isComic || isPdf) && isComicExtractProgressVisible(comicProgress) && (
                <ComicExtractProgress
                  loaded={comicProgress.loaded}
                  total={comicProgress.total}
                  labelKey={isPdf ? "reader.cachingPagesProgress" : undefined}
                  onDismiss={() => dispatchComicProgress({ type: "dismiss" })}
                />
              )}
              {thumbStripOpen && (bookFormat === "cbz" || bookFormat === "cbr" || bookFormat === "pdf") && (
                <PageThumbnailStrip
                  bookId={bookId!}
                  format={bookFormat}
                  totalPages={pageCount}
                  currentPage={chapterIndex}
                  onSelect={(target) => {
                    if (target !== chapterIndex) recordJump(target, 0);
                    setChapterIndex(target);
                  }}
                />
              )}
            </div>
          ) : (
            <div className="flex-1 flex items-center justify-center">
              <p className="text-sm text-ink-muted">{t("reader.loadingPages")}</p>
            </div>
          )
        ) : (
          <>
            {/* Dynamic typography overrides — must target .reader-content p to beat index.css specificity */}
            <style>{`
              .reader-content p {
                margin-bottom: ${typography.paragraphSpacing}em;
                text-align: ${typography.textAlign};
                hyphens: ${typography.hyphenation ? "auto" : "manual"};
                -webkit-hyphens: ${typography.hyphenation ? "auto" : "manual"};
              }
            `}</style>
            {dualPage && !isContinuous && (
              <style>{`
                .reader-content {
                  columns: 2;
                  column-gap: 48px;
                  column-rule: 1px solid var(--warm-border, #e5e0d8);
                  ${mangaMode ? "direction: rtl;" : ""}
                }
                .reader-content > * {
                  ${mangaMode ? "direction: ltr;" : ""}
                }
              `}</style>
            )}
            {customCss && <style>{customCss}</style>}

            <div
              ref={scrollContainerRef}
              className="flex-1 overflow-y-auto relative"
            >
              {/* Floating prev/next arrows — visible when bottom nav is scrolled out of view (paginated only) */}
              {!isContinuous && !bottomNavVisible && isHtmlBook && !dndMode && (
                <>
                  {chapterIndex > 0 && (
                    <button
                      onClick={prevChapter}
                      className="fixed left-3 top-1/2 -translate-y-1/2 z-20 w-9 h-9 flex items-center justify-center rounded-full bg-surface/90 border border-warm-border shadow-md text-ink-muted hover:text-ink hover:bg-surface transition-all opacity-60 hover:opacity-100 focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
                      aria-label={t("reader.previousChapter")}
                    >
                      <svg width="16" height="16" viewBox="0 0 20 20" fill="none">
                        <path d="M12 4l-6 6 6 6" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
                      </svg>
                    </button>
                  )}
                  {chapterIndex < totalChapters - 1 && (
                    <button
                      onClick={nextChapter}
                      className="fixed right-3 top-1/2 -translate-y-1/2 z-20 w-9 h-9 flex items-center justify-center rounded-full bg-surface/90 border border-warm-border shadow-md text-ink-muted hover:text-ink hover:bg-surface transition-all opacity-60 hover:opacity-100 focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
                      aria-label={t("reader.nextChapter")}
                    >
                      <svg width="16" height="16" viewBox="0 0 20 20" fill="none">
                        <path d="M8 4l6 6-6 6" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
                      </svg>
                    </button>
                  )}
                </>
              )}
              {/* Highlight color popup */}
              {selectionPopup && (() => {
                const containerW = contentRef.current?.clientWidth ?? 600;
                const popupW = Math.min(200, containerW - 16);
                const popupH = 36;
                // Clamp X so popup doesn't overflow left/right edges (#61)
                const clampedX = Math.max(popupW / 2 + 8, Math.min(selectionPopup.x, containerW - popupW / 2 - 8));
                // Smart Y positioning (#61): check both top and bottom viewport edges
                const scrollTop = scrollContainerRef.current?.scrollTop ?? 0;
                const viewportH = scrollContainerRef.current?.clientHeight ?? window.innerHeight;
                const wouldClipTop = selectionPopup.y - popupH < scrollTop;
                const wouldClipBottom = selectionPopup.y + 32 + popupH > scrollTop + viewportH;
                // Prefer above; fall back to below if clipped; if both clip, show above
                const showBelow = wouldClipTop && !wouldClipBottom;
                const yOffset = showBelow ? 32 : -8;
                const transformY = showBelow ? "0%" : "-100%";
                return (
                <div
                  data-highlight-popup
                  className="absolute z-30 flex items-center gap-1 px-2 py-1.5 bg-ink/90 backdrop-blur-sm rounded-lg shadow-lg"
                  style={{
                    left: `${clampedX}px`,
                    top: `${selectionPopup.y + yOffset}px`,
                    transform: `translate(-50%, ${transformY})`,
                    maxWidth: `${containerW - 16}px`,
                  }}
                >
                  {/* Highlight colors keep the 3-char floor; a 2-char selection
                      only reaches the popup for a dictionary lookup. */}
                  {selectionPopup.text.length >= 3 && HIGHLIGHT_COLORS.map((c) => (
                    <button
                      key={c.value}
                      onClick={() => handleCreateHighlight(c.value)}
                      className="w-5 h-5 rounded-full hover:scale-125 transition-transform"
                      style={{ backgroundColor: c.value }}
                      aria-label={t("reader.highlightColor", { color: c.name })}
                    />
                  ))}
                  {/* Clear highlight — only show if selection overlaps existing highlights */}
                  {selectionPopup && highlights.some((h) => h.startOffset < selectionPopup.endOffset && h.endOffset > selectionPopup.startOffset) && (
                    <button
                      onClick={handleClearHighlight}
                      className="w-5 h-5 rounded-full hover:scale-125 transition-transform border border-white/40 flex items-center justify-center"
                      style={{ background: "repeating-conic-gradient(#ccc 0% 25%, transparent 0% 50%) 50% / 6px 6px" }}
                      aria-label={t("reader.clearHighlight")}
                      title={t("reader.clearHighlight")}
                    />
                  )}
                  {/* Define — only when the dictionary is ready and the
                      selection reduces to a single lookable word. */}
                  {dictionaryReady && extractLookupWord(selectionPopup.text) !== null && (
                    <button
                      onClick={handleDefine}
                      className="px-2 h-5 rounded text-xs font-medium text-white/90 hover:text-white hover:bg-white/10 transition-colors"
                    >
                      {t("reader.dictionary.define")}
                    </button>
                  )}
                  {/* Share as image (F-1-6) — same non-trivial-selection
                      floor as the highlight colors above. */}
                  {selectionPopup.text.length >= 3 && (
                    <button
                      onClick={handleShareAsImage}
                      className="px-2 h-5 rounded text-xs font-medium text-white/90 hover:text-white hover:bg-white/10 transition-colors"
                    >
                      {t("shareCard.entryLabel")}
                    </button>
                  )}
                  <div className="w-px h-4 bg-white/20 mx-0.5" />
                  <button
                    onClick={() => { setSelectionPopup(null); window.getSelection()?.removeAllRanges(); }}
                    className="w-5 h-5 rounded-full hover:scale-125 transition-transform flex items-center justify-center text-white/60 hover:text-white"
                    aria-label={t("reader.dismiss")}
                  >
                    <svg width="12" height="12" viewBox="0 0 16 16" fill="none">
                      <path d="M12 4L4 12M4 4l8 8" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
                    </svg>
                  </button>
                </div>
                );
              })()}

              {/* Definition card (F-1-1): anchored popover near the selection. */}
              {definitionCard && (() => {
                const containerW = contentRef.current?.clientWidth ?? 600;
                const cardW = Math.min(320, containerW - 16);
                const clampedX = Math.max(cardW / 2 + 8, Math.min(definitionCard.x, containerW - cardW / 2 - 8));
                const scrollTop = scrollContainerRef.current?.scrollTop ?? 0;
                const viewportH = scrollContainerRef.current?.clientHeight ?? window.innerHeight;
                const cardH = 288; // ≈ max-h-72
                const wouldClipBottom = definitionCard.y + 24 + cardH > scrollTop + viewportH;
                const wouldClipTop = definitionCard.y - cardH < scrollTop;
                const showBelow = !wouldClipBottom || wouldClipTop;
                const yOffset = showBelow ? 24 : -8;
                const transformY = showBelow ? "0%" : "-100%";
                const entry = definitionCard.entry;
                const groups = entry ? groupSensesByPos(entry.senses) : [];
                return (
                  <div
                    data-definition-card
                    className="absolute z-30 max-h-72 overflow-y-auto bg-surface text-ink rounded-xl shadow-xl border border-warm-border p-3.5"
                    style={{
                      left: `${clampedX}px`,
                      top: `${definitionCard.y + yOffset}px`,
                      transform: `translate(-50%, ${transformY})`,
                      width: `${cardW}px`,
                    }}
                  >
                    <div className="flex items-start justify-between gap-2">
                      <div className="min-w-0">
                        <span className="text-base font-semibold break-words">
                          {entry ? entry.matchedWord : definitionCard.word}
                        </span>
                        {entry && entry.matchedWord !== entry.word && (
                          <span className="block text-[11px] text-ink-muted">
                            {t("reader.dictionary.lemmaNote", { query: entry.word, lemma: entry.matchedWord })}
                          </span>
                        )}
                      </div>
                      <button
                        onClick={() => setDefinitionCard(null)}
                        className="shrink-0 w-5 h-5 rounded-full text-ink-muted hover:text-ink hover:bg-warm-subtle flex items-center justify-center"
                        aria-label={t("reader.dismiss")}
                      >
                        <svg width="12" height="12" viewBox="0 0 16 16" fill="none">
                          <path d="M12 4L4 12M4 4l8 8" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
                        </svg>
                      </button>
                    </div>

                    {definitionCard.loading && (
                      <p className="text-xs text-ink-muted mt-2">{t("reader.loading")}</p>
                    )}
                    {definitionCard.error && (
                      <p className="text-xs text-ink-muted mt-2">{definitionCard.error}</p>
                    )}
                    {!definitionCard.loading && !definitionCard.error && !entry && (
                      <p className="text-xs text-ink-muted mt-2">
                        {t("reader.dictionary.noMatch", { word: definitionCard.word })}
                      </p>
                    )}

                    {entry && groups.length > 0 && (
                      <div className="mt-2 space-y-3">
                        {groups.map((group) => (
                          <div key={group.pos}>
                            <span className="text-[11px] italic text-ink-muted">
                              {t(POS_LABEL_KEYS[group.pos] ?? "reader.dictionary.posNoun")}
                            </span>
                            <ol className="mt-1 space-y-1.5 list-decimal list-inside">
                              {group.senses.map((sense) => (
                                <li key={`${sense.pos}-${sense.senseNum}`} className="text-sm leading-snug">
                                  <span>{sense.gloss}</span>
                                  {sense.examples[0] && (
                                    <span className="block text-xs italic text-ink-muted mt-0.5">
                                      “{sense.examples[0]}”
                                    </span>
                                  )}
                                  {sense.synonyms.length > 0 && (
                                    <span className="block text-xs text-ink-muted mt-0.5">
                                      {sense.synonyms.slice(0, 5).join(", ")}
                                    </span>
                                  )}
                                </li>
                              ))}
                            </ol>
                          </div>
                        ))}
                      </div>
                    )}
                  </div>
                );
              })()}

              {chapterError ? (
                <ChapterErrorCard
                  error={chapterError}
                  onRetry={() => {
                    setChapterError(null);
                    setChapterRetryCount((c) => c + 1);
                  }}
                />
              ) : isContinuous ? (
                /* ── Continuous scroll: all chapters stacked ── */
                allChaptersLoaded ? (
                  <div ref={contentRef} className="max-w-[680px] mx-auto py-10" style={{ paddingLeft: `${typography.pageMargins}px`, paddingRight: `${typography.pageMargins}px` }}>
                    {sanitizedChaptersHtml.map((html, i) => {
                      const chapterTitle = toc.find((e) => e.chapter_index === i)?.label;
                      return (
                        <div
                          key={i}
                          ref={(el) => { chapterDivRefs.current[i] = el; }}
                          data-chapter-index={i}
                        >
                          {i > 0 && (
                            <div className="my-10 flex items-center gap-4">
                              <div className="flex-1 h-px bg-warm-border" />
                              <span className="text-xs text-ink-muted font-medium shrink-0">
                                {chapterTitle ?? t("reader.chapterDefault", { number: i + 1 })}
                              </span>
                              <div className="flex-1 h-px bg-warm-border" />
                            </div>
                          )}
                          <div
                            className="reader-content"
                            style={readerContentStyle}
                            dangerouslySetInnerHTML={{ __html: html }}
                          />
                        </div>
                      );
                    })}
                  </div>
                ) : (
                  <ChapterLoadProgress total={totalChapters} loaded={chaptersLoaded} />
                )
              ) : !chapterHtml ? (
                /* ── Paginated: loading chapter ── */
                <ReaderSkeleton variant="content" />
              ) : (
                /* ── Paginated: single chapter ── */
                <div
                  ref={contentRef}
                  className="reader-content max-w-[680px] mx-auto py-10"
                  style={{ ...readerContentStyle, paddingLeft: `${typography.pageMargins}px`, paddingRight: `${typography.pageMargins}px` }}
                  dangerouslySetInnerHTML={{ __html: sanitizedChapterHtml }}
                />
              )}

              {/* Chapter navigation (paginated only) */}
              {!isContinuous && (
                <div ref={chapterNavRef} className={`max-w-[680px] mx-auto px-8 pb-12 flex items-center justify-between gap-4 transition-opacity duration-300 ${dndMode && !dndShowControls ? "opacity-0 pointer-events-none" : "opacity-100"}`}>
                  <button
                    onClick={prevChapter}
                    disabled={chapterIndex <= 0}
                    className="flex items-center gap-1.5 px-4 py-2 text-sm text-ink-muted bg-warm-subtle hover:bg-warm-border rounded-xl transition-colors disabled:opacity-50 disabled:cursor-not-allowed focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
                  >
                    <svg width="14" height="14" viewBox="0 0 20 20" fill="none">
                      <path d="M12 4l-6 6 6 6" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
                    </svg>
                    {t("common.previous")}
                  </button>
                  <span className="text-xs text-ink-muted tabular-nums">
                    {t("reader.chapterOf", { current: chapterIndex + 1, total: totalChapters })}
                  </span>
                  <button
                    onClick={nextChapter}
                    disabled={chapterIndex >= totalChapters - 1}
                    className="flex items-center gap-1.5 px-4 py-2 text-sm text-ink-muted bg-warm-subtle hover:bg-warm-border rounded-xl transition-colors disabled:opacity-50 disabled:cursor-not-allowed focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
                  >
                    {t("common.next")}
                    <svg width="14" height="14" viewBox="0 0 20 20" fill="none">
                      <path d="M8 4l6 6-6 6" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
                    </svg>
                  </button>
                </div>
              )}
            </div>

            {/* Progress bar */}
            <footer className={`shrink-0 border-t border-warm-border bg-surface px-5 py-2 flex items-center gap-3 transition-all duration-300 ${showFooter ? "opacity-100 max-h-20" : "opacity-0 max-h-0 overflow-hidden py-0 border-t-0"}`}>
              <span className="text-[11px] text-ink-muted tabular-nums whitespace-nowrap">
                {t("reader.chapterLabel", { current: chapterIndex + 1, total: totalChapters })}
              </span>
              <div className="flex-1 h-[3px] bg-warm-subtle rounded-full overflow-hidden">
                <div
                  className="h-full bg-accent rounded-full transition-all duration-200"
                  style={{ width: `${scrollProgress * 100}%` }}
                />
              </div>
              <span className="text-[11px] text-ink-muted tabular-nums w-8 text-right">
                {Math.round(scrollProgress * 100)}%
              </span>
              {timeEstimate && (
                <span className="text-[11px] text-ink-muted/70 whitespace-nowrap ml-1" title={t("reader.timeLeftTitle", { chapter: timeEstimate.chapter, book: timeEstimate.book })}>
                  {t("reader.timeLeft", { time: timeEstimate.book })}
                </span>
              )}
            </footer>
          </>
        )}
      {completion.showCelebration && completion.completionData && (
        <BookCompletionModal
          bookId={bookId}
          title={completion.completionData.title}
          author={completion.completionData.author}
          coverPath={completion.completionData.coverPath}
          readingTimeSecs={completion.readingTimeSecs}
          onClose={completion.dismiss}
        />
      )}
      </div>
    </div>
  );
}

// ---- TOC Item (recursive) ----

function TocItem({
  entry,
  currentIndex,
  onSelect,
  depth,
}: {
  entry: TocEntry;
  currentIndex: number;
  onSelect: (index: number) => void;
  depth: number;
}) {
  const isActive = entry.chapter_index === currentIndex;

  return (
    <>
      <button
        onClick={() => onSelect(entry.chapter_index)}
        className={`w-full text-left py-2 text-sm transition-colors focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-inset ${
          isActive
            ? "text-accent font-medium bg-accent-light"
            : "text-ink-muted hover:text-ink hover:bg-warm-subtle"
        }`}
        style={{ paddingLeft: `${20 + depth * 14}px`, paddingRight: "16px" }}
      >
        {isActive && (
          <span className="inline-block w-1 h-1 rounded-full bg-accent mr-2 align-middle" />
        )}
        {entry.label}
      </button>
      {entry.children.map((child) => (
        <TocItem
          key={`${child.chapter_index}-${child.label}`}
          entry={child}
          currentIndex={currentIndex}
          onSelect={onSelect}
          depth={depth + 1}
        />
      ))}
    </>
  );
}
