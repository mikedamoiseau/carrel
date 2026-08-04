/**
 * Pure utility functions extracted for testability.
 */

/** Schemes that should escape the WebView and open in the user's default
 *  handler (browser, mail client, dialer). All other schemes — including
 *  Tauri's own asset/IPC schemes and relative paths — stay in-app. */
const EXTERNAL_URL_SCHEMES = new Set([
  "http:",
  "https:",
  "mailto:",
  "tel:",
  "ftp:",
  "ftps:",
  "sftp:",
]);

/**
 * True when `href` parses as an absolute URL with a scheme that should be
 * delegated to the OS (web browser, mail client, …). Relative URLs and
 * fragment-only hrefs return false and stay in-app.
 */
export function isExternalUrl(href: string): boolean {
  try {
    const url = new URL(href);
    return EXTERNAL_URL_SCHEMES.has(url.protocol);
  } catch {
    return false;
  }
}

/** Format seconds into human-readable duration (e.g. "5s", "12m", "2h 30m"). */
export function formatDuration(secs: number): string {
  if (secs < 60) return `${secs}s`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m`;
  const hrs = Math.floor(mins / 60);
  const remMins = mins % 60;
  return remMins > 0 ? `${hrs}h ${remMins}m` : `${hrs}h`;
}

/** Format a unix timestamp (seconds) as a locale-aware date string, e.g.
 *  "Jul 6, 2026". Passes `undefined` as the locale (rather than hardcoding
 *  one) so it follows the user's system/browser locale. */
export function formatDate(
  secs: number,
  opts: Intl.DateTimeFormatOptions = { year: "numeric", month: "short", day: "numeric" },
): string {
  return new Date(secs * 1000).toLocaleDateString(undefined, opts);
}

export interface BookLike {
  id: string;
  title: string;
  author: string;
  format: string;
  added_at: number;
}

/** Filter books by search query, format, and reading status. */
export function filterBooks<T extends BookLike>(
  books: T[],
  search: string,
  filterFormat: string,
  filterStatus: string,
  progressMap: Record<string, number>,
): T[] {
  return books.filter((book) => {
    if (search) {
      const q = search.toLowerCase();
      if (
        !book.title.toLowerCase().includes(q) &&
        !book.author.toLowerCase().includes(q)
      )
        return false;
    }
    if (filterFormat !== "all" && book.format !== filterFormat) return false;
    if (filterStatus !== "all") {
      const pct = progressMap[book.id] ?? 0;
      if (filterStatus === "unread" && pct !== 0) return false;
      if (filterStatus === "in_progress" && (pct === 0 || pct >= 100))
        return false;
      if (filterStatus === "finished" && pct < 100) return false;
    }
    return true;
  });
}

/**
 * Count how many of the given books carry each tag.
 *
 * Pass the set of books that already pass every *other* active filter
 * (search, format, status, rating, source, series) but NOT the tag filter
 * itself — that way each tag's count reflects how many results selecting it
 * would yield given the current filters, while staying useful for multi-select.
 */
export function computeTagBookCounts<T extends { id: string }>(
  books: T[],
  bookTagMap: Map<string, Set<string>>,
): Map<string, number> {
  const counts = new Map<string, number>();
  for (const book of books) {
    const tagIds = bookTagMap.get(book.id);
    if (!tagIds) continue;
    for (const tagId of tagIds) {
      counts.set(tagId, (counts.get(tagId) ?? 0) + 1);
    }
  }
  return counts;
}

/**
 * True when any library filter or search is narrowing the visible set.
 * Used to distinguish "filters hide everything" from "the view is genuinely
 * empty" in the empty-state copy.
 */
export function hasActiveLibraryFilters(f: {
  search: string;
  filterFormat: string;
  filterStatus: string;
  filterRating: string;
  filterSource: string;
  filterWantToRead: boolean;
  filterTagIds: string[];
}): boolean {
  return (
    f.search.length > 0 ||
    f.filterFormat !== "all" ||
    f.filterStatus !== "all" ||
    f.filterRating !== "all" ||
    f.filterSource !== "all" ||
    f.filterWantToRead ||
    f.filterTagIds.length > 0
  );
}

export type SortField =
  | "title"
  | "author"
  | "last_read"
  | "progress"
  | "date_added";

/** Sort books by the given field and direction. */
export function sortBooks<T extends BookLike>(
  books: T[],
  sortBy: SortField,
  sortAsc: boolean,
  progressMap: Record<string, number>,
  lastReadMap: Record<string, number>,
): T[] {
  const dir = sortAsc ? 1 : -1;
  return [...books].sort((a, b) => {
    switch (sortBy) {
      case "title":
        return dir * a.title.localeCompare(b.title);
      case "author":
        return dir * a.author.localeCompare(b.author);
      case "last_read":
        return (
          dir * ((lastReadMap[a.id] ?? 0) - (lastReadMap[b.id] ?? 0))
        );
      case "progress":
        return (
          dir * ((progressMap[a.id] ?? 0) - (progressMap[b.id] ?? 0))
        );
      case "date_added":
      default:
        return dir * (a.added_at - b.added_at);
    }
  });
}

/** Group items by a key extracted from each item. */
export function groupBy<T>(
  items: T[],
  keyFn: (item: T) => string | number,
): Record<string | number, T[]> {
  return items.reduce<Record<string | number, T[]>>((acc, item) => {
    const key = keyFn(item);
    (acc[key] ??= []).push(item);
    return acc;
  }, {});
}

/** Clamp a number between min and max (inclusive). */
export function clamp(value: number, min: number, max: number): number {
  return Math.min(Math.max(value, min), max);
}

/** Geometry inputs for {@link resolveBookmarkScrollTop}. All values are in
 *  CSS pixels and come from `HTMLElement.offsetTop`, `offsetHeight`, the
 *  container's `scrollHeight`, and (for paginated mode) `clientHeight`. */
export interface ChapterGeometry {
  chapterOffsetTop: number;
  chapterHeight: number;
  containerScrollHeight: number;
  /** Container's `clientHeight` — used as the divisor for the paginated
   *  scroll fraction so save/restore use the same denominator
   *  (`scrollTop / (scrollHeight - clientHeight)` on the save side). */
  containerClientHeight?: number;
}

/**
 * Convert a stored bookmark/history `scroll_position` (fraction 0–1) back
 * into an absolute `container.scrollTop` value.
 *
 * Continuous mode: the fraction is **chapter-local** — same coordinate
 * system `getChapterScrollPosition()` produces on save. Resolving it
 * requires the chapter's geometry because the container holds every
 * chapter end to end.
 *
 * Paginated mode: the fraction is **container-global** and matches the
 * `scrollProgress` formula `scrollTop / (scrollHeight - clientHeight)`,
 * so the inverse multiplies by that same denominator. When `clientHeight`
 * isn't supplied (older callers / synthetic geometry), this falls back to
 * the full `scrollHeight` for backward compatibility.
 *
 * Pure: does not clamp out-of-range fractions (saved values should
 * already be in [0, 1]) and returns `chapterOffsetTop` when the chapter
 * has zero height instead of producing NaN.
 */
export function resolveBookmarkScrollTop(
  isContinuous: boolean,
  storedPosition: number,
  geometry: ChapterGeometry,
): number {
  if (isContinuous) {
    return geometry.chapterOffsetTop + storedPosition * geometry.chapterHeight;
  }
  const denom =
    typeof geometry.containerClientHeight === "number"
      ? Math.max(0, geometry.containerScrollHeight - geometry.containerClientHeight)
      : geometry.containerScrollHeight;
  return storedPosition * denom;
}

const SUPPORTED_EXTENSIONS = [".epub", ".cbz", ".cbr", ".pdf"];

/** Check if a filename has a supported ebook extension. */
export function isSupportedFile(filename: string): boolean {
  const lower = filename.toLowerCase();
  return SUPPORTED_EXTENSIONS.some((ext) => lower.endsWith(ext));
}

/**
 * Format a byte count human-readably (e.g. "0 B", "2.4 MB"). Returns an empty
 * string for null/undefined so callers can omit the size when a feed omits it.
 */
export function formatBytes(bytes: number | null | undefined): string {
  if (bytes == null) return "";
  if (bytes === 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.floor(Math.log(bytes) / Math.log(1024));
  return `${(bytes / Math.pow(1024, i)).toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

export interface MetadataPill {
  label: string;
}

/** Given a page index and total pages, return the left and right pages for a dual-page spread.
 *  Cover (index 0) is always solo. After that, pages pair as 1-2, 3-4, 5-6, etc.
 *  If the last page has no partner (odd total), it displays solo (right: null). */
export function getSpreadPages(
  pageIndex: number,
  totalPages: number,
): { left: number; right: number | null } {
  // Cover is always solo
  if (pageIndex === 0) return { left: 0, right: null };

  // Find the left page of the pair containing pageIndex
  // After cover: pairs are (1,2), (3,4), (5,6), ...
  // Left page of a pair is always odd-indexed
  const left = pageIndex % 2 === 1 ? pageIndex : pageIndex - 1;
  const right = left + 1;

  // If the right page is beyond total, it's solo
  if (right >= totalPages) return { left, right: null };

  return { left, right };
}

export function formatMetadataPills(meta: {
  language?: string | null;
  publishYear?: number | null;
  series?: string | null;
  volume?: number | null;
}): MetadataPill[] {
  const pills: MetadataPill[] = [];
  if (meta.language) pills.push({ label: meta.language });
  if (meta.publishYear != null) pills.push({ label: String(meta.publishYear) });
  if (meta.series) {
    const seriesLabel = meta.volume != null ? `${meta.series} #${meta.volume}` : meta.series;
    pills.push({ label: seriesLabel });
  }
  return pills;
}

export interface OpdsLinkLike {
  href: string;
  mimeType: string;
}

// Format preference for OPDS acquisition: EPUB first (best reflowable
// rendering), then PDF/CBZ/CBR for page-based books, then MOBI/AZW3/AZW.
// `ext` is the canonical extension used to filter against the backend's
// get_supported_formats() list; `mimeNeedles` / `extNeedles` are loose
// substring matches tolerating the MIME + URL variations seen in the wild.
const OPDS_FORMATS: Array<{
  label: string;
  ext: string;
  mimeNeedles: string[];
  extNeedles: string[];
}> = [
  { label: "EPUB", ext: "epub", mimeNeedles: ["epub"], extNeedles: ["epub"] },
  { label: "PDF", ext: "pdf", mimeNeedles: ["pdf"], extNeedles: ["pdf"] },
  { label: "CBZ", ext: "cbz", mimeNeedles: ["cbz", "comicbook+zip"], extNeedles: ["cbz"] },
  { label: "CBR", ext: "cbr", mimeNeedles: ["cbr", "comicbook-rar"], extNeedles: ["cbr"] },
  { label: "AZW3", ext: "azw3", mimeNeedles: ["vnd.amazon.ebook", "azw3"], extNeedles: ["azw3"] },
  { label: "MOBI", ext: "mobi", mimeNeedles: ["mobipocket", "mobi"], extNeedles: ["mobi"] },
  { label: "AZW", ext: "azw", mimeNeedles: ["azw"], extNeedles: ["azw"] },
];

function matchesMime(link: OpdsLinkLike, mimeNeedles: string[]): boolean {
  const mime = link.mimeType.toLowerCase();
  return mimeNeedles.some((n) => mime.includes(n));
}

function matchesExt(link: OpdsLinkLike, extNeedles: string[]): boolean {
  const href = link.href.toLowerCase();
  return extNeedles.some((n) => href.includes(`.${n}`));
}

/**
 * Pick the best OPDS acquisition link for import. Walks the Carrel preference
 * order and returns the first matching link along with a human-readable
 * label. When `allowedExtensions` is supplied (e.g. the set returned by the
 * backend's get_supported_formats command), formats not in the allowlist are
 * skipped — this prevents the UI from offering e.g. `+ MOBI` on builds that
 * weren't compiled with the `mobi` feature.
 *
 * For each candidate format we look at URL extension matches before MIME
 * matches. This matters for the MOBI family: `application/vnd.amazon.ebook`
 * is shared by `.azw` and `.azw3`, so a MIME-first rule silently renames
 * AZW downloads to AZW3. The URL path is the only signal that disambiguates.
 *
 * Returns null when no supported + allowed link is found; callers should
 * hide the download action rather than pulling an arbitrary link.
 */
export function pickSupportedOpdsLink<T extends OpdsLinkLike>(
  links: T[],
  allowedExtensions?: ReadonlySet<string>,
): { link: T; label: string } | null {
  // URL-extension pass: if any link has a definite URL extension matching a
  // preferred format, use it. This runs through formats in preference order
  // and checks URL suffixes only.
  for (const { label, ext, extNeedles } of OPDS_FORMATS) {
    if (allowedExtensions && !allowedExtensions.has(ext)) continue;
    const match = links.find((l) => matchesExt(l, extNeedles));
    if (match) return { link: match, label };
  }
  // MIME-type fallback pass: when nothing in the URL path matched, trust
  // the advertised MIME.
  for (const { label, ext, mimeNeedles } of OPDS_FORMATS) {
    if (allowedExtensions && !allowedExtensions.has(ext)) continue;
    const match = links.find((l) => matchesMime(l, mimeNeedles));
    if (match) return { link: match, label };
  }
  return null;
}

/**
 * Patterns that are dangerous in user-supplied CSS.
 * Blocks data exfiltration (url, @import), script execution (expression,
 * javascript:, -moz-binding), and external resource loading.
 */
const DANGEROUS_CSS_PATTERNS = [
  /url\s*\(/gi,
  /@import/gi,
  /expression\s*\(/gi,
  /javascript\s*:/gi,
  /-moz-binding/gi,
  /behavior\s*:/gi,
  /@font-face/gi,
  /@namespace/gi,
  /\\[0-9a-fA-F]/g, // CSS escape sequences used to bypass filters
];

/** Sanitize user-supplied custom CSS by removing dangerous constructs. */
export function sanitizeCss(css: string): string {
  let sanitized = css;
  for (const pattern of DANGEROUS_CSS_PATTERNS) {
    sanitized = sanitized.replace(pattern, "/* blocked */");
  }
  return sanitized;
}

export type ReadingStatus = "unread" | "active" | "paused" | "finished";

/** Days of inactivity after which an in-progress book is considered paused. */
export const PAUSED_AFTER_DAYS = 14;

/**
 * Derive a book's reading status from its progress and last-read time.
 * Pure: callers pass `nowSecs` (unix seconds) so it is deterministic/testable.
 * - progress >= 100        → finished
 * - progress <= 0          → unread
 * - in progress, read <=14d ago → active
 * - in progress, older or no timestamp → paused
 */
/** Display names for enrichment provider ids used in retry feedback (F-2-7). */
const PROVIDER_DISPLAY_NAMES: Record<string, string> = {
  google_books: "Google Books",
  openlibrary: "OpenLibrary",
  comic_vine: "Comic Vine",
  bnf: "BnF",
};

export function providerDisplayName(id: string): string {
  return PROVIDER_DISPLAY_NAMES[id] ?? id;
}

export function getReadingStatus(
  progress: number,
  lastReadAt: number | undefined,
  nowSecs: number,
): ReadingStatus {
  if (progress >= 100) return "finished";
  if (progress <= 0) return "unread";
  if (!lastReadAt) return "paused";
  const ageDays = (nowSecs - lastReadAt) / 86400;
  return ageDays <= PAUSED_AFTER_DAYS ? "active" : "paused";
}

/**
 * True when `value` parses as an absolute http(s) URL. Used to pre-validate
 * OPDS catalog URLs before attempting a connection test.
 */
export function isValidHttpUrl(value: string): boolean {
  let url: URL;
  try {
    url = new URL(value.trim());
  } catch {
    return false;
  }
  return url.protocol === "http:" || url.protocol === "https:";
}

/**
 * Frontend mirror of carrel-core's `is_loopback_host` (`opds.rs`) — true for
 * `localhost`, `*.localhost`, and loopback IPs. Used only to decide whether
 * the add-catalog form should warn before sending a credential over
 * cleartext HTTP; the backend is the actual enforcement point.
 */
export function isLoopbackHost(hostname: string): boolean {
  const bare = hostname.replace(/^\[/, "").replace(/\]$/, "");
  const lower = bare.toLowerCase();
  if (lower === "localhost" || lower.endsWith(".localhost")) return true;
  if (bare === "::1") return true;
  return /^127(?:\.\d{1,3}){3}$/.test(bare);
}

/** Number of days shown by the reading heatmap (F-5-4): a rolling year. */
export const HEATMAP_DAYS = 365;

/**
 * Minute thresholds separating the heatmap's intensity buckets 0 (no reading)
 * through 4 (heaviest). Fixed/absolute rather than relative to the data's max
 * so a cell's color has a stable meaning day to day, instead of shifting
 * whenever the reader's best day changes (unlike the existing 30-day bar
 * chart, which normalizes bar height against the period's max on purpose).
 * Compared against exact fractional minutes — see {@link getHeatmapBucket}.
 */
export const HEATMAP_BUCKET_THRESHOLDS_MIN = [15, 30, 60] as const;

/**
 * Bucket a day's reading duration (in seconds) into an intensity level (0-4)
 * for the heatmap. Takes seconds rather than pre-rounded minutes so that:
 * - a day with a few seconds of reading (bucket 1) never collapses into the
 *   same bucket as an untouched day (bucket 0), and
 * - minutes just under a threshold (e.g. 14m31s) aren't rounded up into the
 *   next bucket.
 */
export function getHeatmapBucket(seconds: number): number {
  if (seconds <= 0) return 0;
  const minutes = seconds / 60;
  if (minutes < HEATMAP_BUCKET_THRESHOLDS_MIN[0]) return 1;
  if (minutes < HEATMAP_BUCKET_THRESHOLDS_MIN[1]) return 2;
  if (minutes < HEATMAP_BUCKET_THRESHOLDS_MIN[2]) return 3;
  return 4;
}

/** Format a Date as a local "YYYY-MM-DD" key, matching the backend's
 *  `date(started_at, 'unixepoch', 'localtime')` day strings. Avoids
 *  `toISOString()`, which is UTC-based and can shift the date by a day. */
export function toDateKey(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

export interface HeatmapDay {
  date: string; // "YYYY-MM-DD", local
  seconds: number;
  bucket: number;
  /** False for padding cells added to complete the first/last week — the
   *  days before the 365-day window starts, or after `today`. */
  inRange: boolean;
}

/**
 * Build a GitHub-style contribution grid: one column per week, weeks ordered
 * oldest (left) to current (right), each column holding exactly 7 days.
 * Weeks start on Sunday, matching GitHub's default contribution graph.
 *
 * The 365-day window ends on `today` and is padded at both ends to complete
 * whole weeks — those padding days carry `inRange: false` so callers can
 * render them as blank instead of the lowest reading-intensity color.
 */
export function buildHeatmapWeeks(
  dailyReadingSecs: [string, number][],
  today: Date,
): HeatmapDay[][] {
  const secondsByDate = new Map(dailyReadingSecs);

  const rangeStart = new Date(today);
  rangeStart.setDate(rangeStart.getDate() - (HEATMAP_DAYS - 1));
  const rangeStartKey = toDateKey(rangeStart);
  const todayKey = toDateKey(today);

  // Pad the start back to the beginning of its week (Sunday).
  const gridStart = new Date(rangeStart);
  gridStart.setDate(gridStart.getDate() - gridStart.getDay());

  // Pad the end forward to the end of today's week (Saturday).
  const gridEnd = new Date(today);
  gridEnd.setDate(gridEnd.getDate() + (6 - gridEnd.getDay()));

  const days: HeatmapDay[] = [];
  for (
    const cursor = new Date(gridStart);
    cursor.getTime() <= gridEnd.getTime();
    cursor.setDate(cursor.getDate() + 1)
  ) {
    const dateKey = toDateKey(cursor);
    const seconds = secondsByDate.get(dateKey) ?? 0;
    days.push({
      date: dateKey,
      seconds,
      bucket: getHeatmapBucket(seconds),
      inRange: dateKey >= rangeStartKey && dateKey <= todayKey,
    });
  }

  const weeks: HeatmapDay[][] = [];
  for (let i = 0; i < days.length; i += 7) {
    weeks.push(days.slice(i, i + 7));
  }
  return weeks;
}

/**
 * Month label (0-11) for each week column, placed on the week that contains
 * the 1st of that month — mirrors GitHub's contribution graph header, which
 * naturally spaces labels ~4-5 columns apart. `null` means no label for that
 * column. Only considers in-range days: a padding cell landing on a 1st (the
 * current week's future days, or days before the window starts) is invisible
 * in the grid, so it must not claim a label.
 */
export function getHeatmapMonthLabels(weeks: HeatmapDay[][]): (number | null)[] {
  return weeks.map((week) => {
    for (const day of week) {
      if (day.inRange && day.date.endsWith("-01")) {
        return Number(day.date.slice(5, 7)) - 1;
      }
    }
    return null;
  });
}

/** The valid TCP port range Carrel's web server accepts (non-privileged). */
export const WEB_SERVER_PORT_MIN = 1024;
export const WEB_SERVER_PORT_MAX = 65535;

/**
 * Validates a web-server port string. Returns the parsed port when it is an
 * integer within [WEB_SERVER_PORT_MIN, WEB_SERVER_PORT_MAX], otherwise
 * `{ valid: false }`. Used to surface an inline range error instead of
 * silently clamping out-of-range input.
 */
export function validateWebServerPort(
  value: string
): { valid: true; port: number } | { valid: false } {
  const trimmed = value.trim();
  if (!/^\d+$/.test(trimmed)) return { valid: false };
  const port = parseInt(trimmed, 10);
  if (port < WEB_SERVER_PORT_MIN || port > WEB_SERVER_PORT_MAX) {
    return { valid: false };
  }
  return { valid: true, port };
}

/**
 * 1-based day-of-year for a local date (Jan 1 = 1). Used to gauge progress
 * toward the yearly reading goal against elapsed time (F-1-3).
 *
 * Computed via UTC calendar arithmetic on the date's local year/month/day
 * components rather than flooring a wall-clock millisecond difference —
 * the latter is one day short during 00:00-00:59 on the day after a DST
 * spring-forward, since that local day is only 23 hours long. `Date.UTC`
 * has no DST, so this is correct by construction regardless of timezone.
 */
export function getDayOfYear(d: Date): number {
  const y = d.getFullYear();
  const start = Date.UTC(y, 0, 1);
  const current = Date.UTC(y, d.getMonth(), d.getDate());
  return (current - start) / 86_400_000 + 1;
}

export type ReadingPaceStatus = "ahead" | "behind" | "onTrack";

export interface ReadingPace {
  status: ReadingPaceStatus;
  /** Books ahead of / behind schedule; 0 when `onTrack`. */
  count: number;
  /** Books expected to be finished by today at a steady pace toward `goal`. */
  expected: number;
}

/**
 * Compares books actually finished this year against the number expected by
 * today at a steady pace (`goal * dayOfYear / 365`, floored) for the yearly
 * reading goal's pace indicator (F-1-3). `daysInYear` defaults to 365 per
 * spec rather than accounting for leap years.
 */
export function computeReadingPace(
  finished: number,
  goal: number,
  dayOfYear: number,
  daysInYear = 365
): ReadingPace {
  const expected = Math.floor((goal * dayOfYear) / daysInYear);
  const diff = finished - expected;
  if (diff > 0) return { status: "ahead", count: diff, expected };
  if (diff < 0) return { status: "behind", count: -diff, expected };
  return { status: "onTrack", count: 0, expected };
}
