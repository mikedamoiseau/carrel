import { describe, it, expect } from "vitest";
import {
  formatDuration,
  formatDate,
  formatBytes,
  sortBooks,
  groupBy,
  clamp,
  isSupportedFile,
  isExternalUrl,
  formatMetadataPills,
  getSpreadPages,
  sanitizeCss,
  pickSupportedOpdsLink,
  resolveBookmarkScrollTop,
  getReadingStatus,
  PAUSED_AFTER_DAYS,
  providerDisplayName,
  computeTagBookCounts,
  validateWebServerPort,
  WEB_SERVER_PORT_MIN,
  WEB_SERVER_PORT_MAX,
  getHeatmapBucket,
  toDateKey,
  buildHeatmapWeeks,
  getHeatmapMonthLabels,
  HEATMAP_DAYS,
  getDayOfYear,
  computeReadingPace,
  type BookLike,
} from "./utils";

// ---------------------------------------------------------------------------
// formatDuration
// ---------------------------------------------------------------------------
describe("formatDuration", () => {
  it("formats seconds under a minute", () => {
    expect(formatDuration(0)).toBe("0s");
    expect(formatDuration(30)).toBe("30s");
    expect(formatDuration(59)).toBe("59s");
  });

  it("formats minutes under an hour", () => {
    expect(formatDuration(60)).toBe("1m");
    expect(formatDuration(90)).toBe("1m");
    expect(formatDuration(3599)).toBe("59m");
  });

  it("formats hours", () => {
    expect(formatDuration(3600)).toBe("1h");
    expect(formatDuration(5400)).toBe("1h 30m");
    expect(formatDuration(7200)).toBe("2h");
  });

  it("formats hours with remaining minutes", () => {
    expect(formatDuration(3660)).toBe("1h 1m");
    expect(formatDuration(7320)).toBe("2h 2m");
  });
});

// ---------------------------------------------------------------------------
// formatDate
// ---------------------------------------------------------------------------
describe("formatDate", () => {
  it("formats a unix timestamp with the default options", () => {
    // 2026-07-06T00:00:00Z
    expect(formatDate(1783382400)).toBe(
      new Date(1783382400 * 1000).toLocaleDateString(undefined, {
        year: "numeric",
        month: "short",
        day: "numeric",
      }),
    );
  });

  it("respects custom Intl.DateTimeFormatOptions", () => {
    const secs = 1783382400;
    expect(formatDate(secs, { month: "short", day: "numeric" })).toBe(
      new Date(secs * 1000).toLocaleDateString(undefined, { month: "short", day: "numeric" }),
    );
  });
});

// ---------------------------------------------------------------------------
// Shared book fixtures (used by the sortBooks tests)
// ---------------------------------------------------------------------------
const sampleBooks: BookLike[] = [
  { id: "1", title: "Dune", author: "Frank Herbert", format: "epub", added_at: 100 },
  { id: "2", title: "Neuromancer", author: "William Gibson", format: "pdf", added_at: 200 },
  { id: "3", title: "Foundation", author: "Isaac Asimov", format: "cbz", added_at: 300 },
  { id: "4", title: "Snow Crash", author: "Neal Stephenson", format: "epub", added_at: 400 },
];

const progressMap: Record<string, number> = {
  "1": 0,    // unread
  "2": 50,   // in progress
  "3": 100,  // finished
  "4": 100,  // finished
};

// ---------------------------------------------------------------------------
// sortBooks
// ---------------------------------------------------------------------------
describe("sortBooks", () => {
  it("sorts by title ascending", () => {
    const result = sortBooks(sampleBooks, "title", true, {}, {});
    expect(result.map((b) => b.title)).toEqual([
      "Dune", "Foundation", "Neuromancer", "Snow Crash",
    ]);
  });

  it("sorts by title descending", () => {
    const result = sortBooks(sampleBooks, "title", false, {}, {});
    expect(result.map((b) => b.title)).toEqual([
      "Snow Crash", "Neuromancer", "Foundation", "Dune",
    ]);
  });

  it("sorts by date_added ascending", () => {
    const result = sortBooks(sampleBooks, "date_added", true, {}, {});
    expect(result.map((b) => b.id)).toEqual(["1", "2", "3", "4"]);
  });

  it("sorts by progress descending", () => {
    const result = sortBooks(sampleBooks, "progress", false, progressMap, {});
    // 100, 100, 50, 0
    expect(result[0].id).toBe("3"); // or "4", both are 100
    expect(result[result.length - 1].id).toBe("1"); // 0
  });

  it("sorts by last_read", () => {
    const lastRead = { "1": 500, "2": 100, "3": 300, "4": 200 };
    const result = sortBooks(sampleBooks, "last_read", true, {}, lastRead);
    expect(result.map((b) => b.id)).toEqual(["2", "4", "3", "1"]);
  });

  it("does not mutate original array", () => {
    const original = [...sampleBooks];
    sortBooks(sampleBooks, "title", false, {}, {});
    expect(sampleBooks).toEqual(original);
  });
});

// ---------------------------------------------------------------------------
// groupBy
// ---------------------------------------------------------------------------
describe("groupBy", () => {
  it("groups items by key", () => {
    const items = [
      { chapter: 1, text: "a" },
      { chapter: 1, text: "b" },
      { chapter: 2, text: "c" },
    ];
    const result = groupBy(items, (i) => i.chapter);
    expect(Object.keys(result)).toHaveLength(2);
    expect(result[1]).toHaveLength(2);
    expect(result[2]).toHaveLength(1);
  });

  it("returns empty object for empty input", () => {
    expect(groupBy([], () => "key")).toEqual({});
  });
});

// ---------------------------------------------------------------------------
// clamp
// ---------------------------------------------------------------------------
describe("clamp", () => {
  it("returns value when in range", () => {
    expect(clamp(18, 14, 24)).toBe(18);
  });

  it("clamps to min", () => {
    expect(clamp(10, 14, 24)).toBe(14);
  });

  it("clamps to max", () => {
    expect(clamp(30, 14, 24)).toBe(24);
  });

  it("handles edge values", () => {
    expect(clamp(14, 14, 24)).toBe(14);
    expect(clamp(24, 14, 24)).toBe(24);
  });
});

// ---------------------------------------------------------------------------
// isSupportedFile
// ---------------------------------------------------------------------------
describe("isSupportedFile", () => {
  it("accepts supported formats", () => {
    expect(isSupportedFile("book.epub")).toBe(true);
    expect(isSupportedFile("comic.cbz")).toBe(true);
    expect(isSupportedFile("comic.cbr")).toBe(true);
    expect(isSupportedFile("doc.pdf")).toBe(true);
  });

  it("is case-insensitive", () => {
    expect(isSupportedFile("Book.EPUB")).toBe(true);
    expect(isSupportedFile("Doc.PDF")).toBe(true);
  });

  it("rejects unsupported formats", () => {
    expect(isSupportedFile("readme.txt")).toBe(false);
    expect(isSupportedFile("image.png")).toBe(false);
    expect(isSupportedFile("book.mobi")).toBe(false);
    expect(isSupportedFile("")).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// formatMetadataPills
// ---------------------------------------------------------------------------
describe("formatMetadataPills", () => {
  it("returns empty array when all fields are null", () => {
    expect(formatMetadataPills({})).toEqual([]);
  });

  it("includes language pill when language is set", () => {
    const pills = formatMetadataPills({ language: "fr" });
    expect(pills).toEqual([{ label: "fr" }]);
  });

  it("includes year pill when publishYear is set", () => {
    const pills = formatMetadataPills({ publishYear: 2024 });
    expect(pills).toEqual([{ label: "2024" }]);
  });

  it("formats series with volume", () => {
    const pills = formatMetadataPills({ series: "Aria", volume: 30 });
    expect(pills).toEqual([{ label: "Aria #30" }]);
  });

  it("formats series without volume", () => {
    const pills = formatMetadataPills({ series: "Aria" });
    expect(pills).toEqual([{ label: "Aria" }]);
  });

  it("returns all pills in order: language, year, series", () => {
    const pills = formatMetadataPills({
      language: "en",
      publishYear: 2023,
      series: "Dune",
      volume: 1,
    });
    expect(pills).toEqual([
      { label: "en" },
      { label: "2023" },
      { label: "Dune #1" },
    ]);
  });

  it("skips null and undefined fields", () => {
    const pills = formatMetadataPills({
      language: null,
      publishYear: undefined,
      series: "Saga",
      volume: null,
    });
    expect(pills).toEqual([{ label: "Saga" }]);
  });
});

// ---------------------------------------------------------------------------
// getSpreadPages
// ---------------------------------------------------------------------------
describe("getSpreadPages", () => {
  it("returns cover page solo (index 0)", () => {
    expect(getSpreadPages(0, 10)).toEqual({ left: 0, right: null });
  });

  it("pairs pages after cover: 1-2, 3-4, etc.", () => {
    expect(getSpreadPages(1, 10)).toEqual({ left: 1, right: 2 });
    expect(getSpreadPages(2, 10)).toEqual({ left: 1, right: 2 });
    expect(getSpreadPages(3, 10)).toEqual({ left: 3, right: 4 });
    expect(getSpreadPages(4, 10)).toEqual({ left: 3, right: 4 });
  });

  it("returns last page solo when odd total", () => {
    expect(getSpreadPages(6, 7)).toEqual({ left: 5, right: 6 });
    expect(getSpreadPages(5, 6)).toEqual({ left: 5, right: null });
  });

  it("handles single-page book", () => {
    expect(getSpreadPages(0, 1)).toEqual({ left: 0, right: null });
  });

  it("handles two-page book", () => {
    expect(getSpreadPages(0, 2)).toEqual({ left: 0, right: null });
    expect(getSpreadPages(1, 2)).toEqual({ left: 1, right: null });
  });

  it("handles three-page book", () => {
    expect(getSpreadPages(0, 3)).toEqual({ left: 0, right: null });
    expect(getSpreadPages(1, 3)).toEqual({ left: 1, right: 2 });
    expect(getSpreadPages(2, 3)).toEqual({ left: 1, right: 2 });
  });
});

// ---------------------------------------------------------------------------
// sanitizeCss
// ---------------------------------------------------------------------------
describe("sanitizeCss", () => {
  it("allows safe CSS through", () => {
    const css = ".reader-content { color: red; font-size: 16px; }";
    expect(sanitizeCss(css)).toBe(css);
  });

  it("blocks url() to prevent data exfiltration", () => {
    const css = "body { background: url('http://evil.com/steal'); }";
    expect(sanitizeCss(css)).not.toContain("url(");
    expect(sanitizeCss(css)).toContain("/* blocked */");
  });

  it("blocks @import to prevent external resource loading", () => {
    const css = "@import 'http://evil.com/payload.css';";
    expect(sanitizeCss(css)).not.toContain("@import");
  });

  it("blocks expression() for IE script execution", () => {
    const css = "div { width: expression(alert(1)); }";
    expect(sanitizeCss(css)).not.toContain("expression(");
  });

  it("blocks javascript: protocol", () => {
    const css = "div { background: javascript:alert(1); }";
    expect(sanitizeCss(css)).not.toContain("javascript:");
  });

  it("blocks -moz-binding", () => {
    const css = "div { -moz-binding: url(evil.xml#xbl); }";
    expect(sanitizeCss(css)).not.toContain("-moz-binding");
  });

  it("blocks @font-face to prevent external font loading", () => {
    const css = "@font-face { src: url(evil.woff); }";
    expect(sanitizeCss(css)).not.toContain("@font-face");
  });

  it("returns empty string unchanged", () => {
    expect(sanitizeCss("")).toBe("");
  });

  it("allows normal selectors and properties", () => {
    const css = ".reader-content p { line-height: 1.8; margin-bottom: 1em; text-align: justify; }";
    expect(sanitizeCss(css)).toBe(css);
  });
});

// ---------------------------------------------------------------------------
// pickSupportedOpdsLink
// ---------------------------------------------------------------------------
describe("pickSupportedOpdsLink", () => {
  it("prefers EPUB over PDF when both are available", () => {
    const picked = pickSupportedOpdsLink([
      { href: "http://host/book.pdf", mimeType: "application/pdf" },
      { href: "http://host/book.epub", mimeType: "application/epub+zip" },
    ]);
    expect(picked?.label).toBe("EPUB");
    expect(picked?.link.href).toContain(".epub");
  });

  it("picks MOBI when only MOBI is offered", () => {
    const picked = pickSupportedOpdsLink([
      { href: "http://host/book.mobi", mimeType: "application/x-mobipocket-ebook" },
    ]);
    expect(picked?.label).toBe("MOBI");
  });

  it("picks AZW3 via amazon vendor MIME even with opaque URL", () => {
    const picked = pickSupportedOpdsLink([
      { href: "http://host/download/123", mimeType: "application/vnd.amazon.ebook" },
    ]);
    expect(picked?.label).toBe("AZW3");
  });

  it("falls back to URL extension when MIME is generic", () => {
    // Feeds that serve everything as octet-stream still put the extension in
    // the URL. We can't learn the format from the MIME, but the URL does.
    const picked = pickSupportedOpdsLink([
      { href: "http://host/book.cbz?token=abc", mimeType: "application/octet-stream" },
    ]);
    expect(picked?.label).toBe("CBZ");
  });

  it("returns null when nothing is importable", () => {
    const picked = pickSupportedOpdsLink([
      { href: "http://host/cover.jpg", mimeType: "image/jpeg" },
      { href: "http://host/info.html", mimeType: "text/html" },
    ]);
    expect(picked).toBeNull();
  });

  it("picks CBR correctly (not shadowed by CBZ)", () => {
    const picked = pickSupportedOpdsLink([
      { href: "http://host/book.cbr", mimeType: "application/x-cbr" },
    ]);
    expect(picked?.label).toBe("CBR");
  });

  it("skips formats outside the allowlist (feature-gating)", () => {
    // A MOBI-only entry on a build that didn't compile with --features mobi.
    const picked = pickSupportedOpdsLink(
      [{ href: "http://host/book.mobi", mimeType: "application/x-mobipocket-ebook" }],
      new Set(["epub", "pdf", "cbz", "cbr"]),
    );
    expect(picked).toBeNull();
  });

  it("falls back to EPUB when the allowlist forbids higher-priority matches", () => {
    // A feed offering both AZW3 and EPUB, on a build without the mobi
    // feature. Without the allowlist, AZW3 would win the lookup order;
    // with it, we should fall through to EPUB.
    const picked = pickSupportedOpdsLink(
      [
        { href: "http://host/book.azw3", mimeType: "application/vnd.amazon.ebook" },
        { href: "http://host/book.epub", mimeType: "application/epub+zip" },
      ],
      new Set(["epub", "pdf", "cbz", "cbr"]),
    );
    expect(picked?.label).toBe("EPUB");
  });

  it("treats an undefined allowlist as 'allow everything'", () => {
    // Existing behavior — allowlist is optional.
    const picked = pickSupportedOpdsLink([
      { href: "http://host/book.mobi", mimeType: "application/x-mobipocket-ebook" },
    ]);
    expect(picked?.label).toBe("MOBI");
  });

  it("labels a .azw URL as AZW even when MIME is the ambiguous vendor one", () => {
    // The vendor MIME `application/vnd.amazon.ebook` is shared by .azw and
    // .azw3 in the wild. The previous iteration order made AZW3 always win,
    // silently renaming AZW downloads. URL extension must take precedence
    // over the ambiguous MIME so round-tripping preserves the container.
    const picked = pickSupportedOpdsLink([
      { href: "http://host/book.azw", mimeType: "application/vnd.amazon.ebook" },
    ]);
    expect(picked?.label).toBe("AZW");
    expect(picked?.link.href).toContain(".azw");
    expect(picked?.link.href).not.toContain(".azw3");
  });

  it("labels a .azw3 URL as AZW3 with the vendor MIME", () => {
    const picked = pickSupportedOpdsLink([
      { href: "http://host/book.azw3", mimeType: "application/vnd.amazon.ebook" },
    ]);
    expect(picked?.label).toBe("AZW3");
  });

  it("prefers EPUB link over AZW link when both are offered", () => {
    // Confirms the AZW URL-first fix doesn't override the global format
    // preference order (EPUB is still the best reflowable option).
    const picked = pickSupportedOpdsLink([
      { href: "http://host/book.azw", mimeType: "application/vnd.amazon.ebook" },
      { href: "http://host/book.epub", mimeType: "application/epub+zip" },
    ]);
    expect(picked?.label).toBe("EPUB");
  });
});

// ---------------------------------------------------------------------------
// resolveBookmarkScrollTop
// ---------------------------------------------------------------------------
describe("resolveBookmarkScrollTop", () => {
  // HTML-reflowable books store bookmark positions as chapter-local fractions
  // (0–1 of the current chapter's height). `resolveBookmarkScrollTop` turns
  // that back into an absolute container.scrollTop value the reader assigns
  // when the bookmark is reopened.

  it("continuous mode: midpoint of 2000 px chapter at offset 5000", () => {
    const top = resolveBookmarkScrollTop(true, 0.5, {
      chapterOffsetTop: 5000,
      chapterHeight: 2000,
      containerScrollHeight: 12000,
    });
    expect(top).toBe(6000);
  });

  it("continuous mode: top of chapter lands exactly at chapter offset", () => {
    const top = resolveBookmarkScrollTop(true, 0, {
      chapterOffsetTop: 3500,
      chapterHeight: 1000,
      containerScrollHeight: 8000,
    });
    expect(top).toBe(3500);
  });

  it("continuous mode: end of chapter lands at chapter bottom", () => {
    const top = resolveBookmarkScrollTop(true, 1, {
      chapterOffsetTop: 3500,
      chapterHeight: 1000,
      containerScrollHeight: 8000,
    });
    expect(top).toBe(4500);
  });

  it("paginated mode: with clientHeight, denominator is scrollHeight - clientHeight", () => {
    // scrollProgress save side = scrollTop / (scrollHeight - clientHeight),
    // so the restore multiplier must match: 0.5 of (2000 - 1000) = 500.
    const top = resolveBookmarkScrollTop(false, 0.5, {
      chapterOffsetTop: 0,
      chapterHeight: 0,
      containerScrollHeight: 2000,
      containerClientHeight: 1000,
    });
    expect(top).toBe(500);
  });

  it("paginated mode: round-trip — save fraction restores to source pixel", () => {
    // Reproduces the round-trip: scrollTop=500 in a 2000/1000 container.
    const scrollTop = 500;
    const scrollHeight = 2000;
    const clientHeight = 1000;
    const stored = scrollTop / (scrollHeight - clientHeight); // 0.5
    const restored = resolveBookmarkScrollTop(false, stored, {
      chapterOffsetTop: 0,
      chapterHeight: 0,
      containerScrollHeight: scrollHeight,
      containerClientHeight: clientHeight,
    });
    expect(restored).toBe(scrollTop);
  });

  it("paginated mode: clientHeight undefined → falls back to scrollHeight (legacy)", () => {
    const top = resolveBookmarkScrollTop(false, 0.25, {
      chapterOffsetTop: 5000,
      chapterHeight: 2000,
      containerScrollHeight: 8000,
    });
    expect(top).toBe(2000);
  });

  it("paginated mode: ignores chapter geometry entirely", () => {
    const a = resolveBookmarkScrollTop(false, 0.5, {
      chapterOffsetTop: 5000,
      chapterHeight: 2000,
      containerScrollHeight: 10000,
      containerClientHeight: 800,
    });
    const b = resolveBookmarkScrollTop(false, 0.5, {
      chapterOffsetTop: 99,
      chapterHeight: 123,
      containerScrollHeight: 10000,
      containerClientHeight: 800,
    });
    expect(a).toBe(b);
  });

  it("continuous mode: out-of-range fraction is clamped by caller's geometry", () => {
    // The helper doesn't clamp — it's a pure arithmetic function. Callers
    // that already clamp on save (getChapterScrollPosition does) won't emit
    // out-of-range values, but document the contract here so a future
    // change doesn't silently clamp and mask save-side bugs.
    const top = resolveBookmarkScrollTop(true, 1.25, {
      chapterOffsetTop: 1000,
      chapterHeight: 800,
      containerScrollHeight: 5000,
    });
    expect(top).toBe(2000); // 1000 + 1.25 * 800 = 2000
  });

  it("continuous mode: zero-height chapter returns the chapter offset", () => {
    // Guard against divide-by-zero in the reader: an empty chapter shouldn't
    // scroll to NaN. Any fraction × 0 = 0, so scrollTop == chapterOffsetTop.
    const top = resolveBookmarkScrollTop(true, 0.5, {
      chapterOffsetTop: 4200,
      chapterHeight: 0,
      containerScrollHeight: 8000,
    });
    expect(top).toBe(4200);
  });
});

describe("isExternalUrl", () => {
  it("recognises http(s) URLs as external", () => {
    expect(isExternalUrl("http://example.com")).toBe(true);
    expect(isExternalUrl("https://example.com/path?q=1#x")).toBe(true);
  });

  it("recognises mailto and tel as external", () => {
    expect(isExternalUrl("mailto:author@example.com")).toBe(true);
    expect(isExternalUrl("tel:+15551234567")).toBe(true);
  });

  it("treats relative paths and fragments as internal", () => {
    expect(isExternalUrl("chapter02.html")).toBe(false);
    expect(isExternalUrl("../images/cover.jpg")).toBe(false);
    expect(isExternalUrl("/absolute/path")).toBe(false);
    expect(isExternalUrl("#section-3")).toBe(false);
  });

  it("treats unknown / dangerous schemes as internal (no escape to OS)", () => {
    // javascript: and data: URIs must NOT be openUrl-ed — keep them in-app
    // where DOMPurify already neutralised them via sanitisation.
    expect(isExternalUrl("javascript:alert(1)")).toBe(false);
    expect(isExternalUrl("data:text/html,<script>alert(1)</script>")).toBe(false);
    expect(isExternalUrl("file:///etc/passwd")).toBe(false);
    expect(isExternalUrl("asset://localhost/foo")).toBe(false);
  });

  it("returns false for empty or malformed input", () => {
    expect(isExternalUrl("")).toBe(false);
    expect(isExternalUrl("   ")).toBe(false);
    expect(isExternalUrl("not a url")).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// getReadingStatus
// ---------------------------------------------------------------------------
describe("getReadingStatus", () => {
  const DAY = 86400;
  const now = 1_700_000_000; // fixed reference (unix seconds)

  it("returns unread when progress is 0", () => {
    expect(getReadingStatus(0, now, now)).toBe("unread");
    expect(getReadingStatus(0, undefined, now)).toBe("unread");
  });

  it("returns finished when progress is 100 or more", () => {
    expect(getReadingStatus(100, now, now)).toBe("finished");
    expect(getReadingStatus(150, now - 999 * DAY, now)).toBe("finished");
  });

  it("returns active for in-progress read within the window", () => {
    expect(getReadingStatus(34, now, now)).toBe("active");
    expect(getReadingStatus(34, now - 13 * DAY, now)).toBe("active");
  });

  it("treats exactly 14 days as still active (inclusive boundary)", () => {
    expect(getReadingStatus(34, now - PAUSED_AFTER_DAYS * DAY, now)).toBe("active");
  });

  it("returns paused for in-progress read older than the window", () => {
    expect(getReadingStatus(34, now - 15 * DAY, now)).toBe("paused");
  });

  it("returns paused for in-progress book with no/zero last-read timestamp", () => {
    expect(getReadingStatus(34, undefined, now)).toBe("paused");
    expect(getReadingStatus(34, 0, now)).toBe("paused");
  });
});

// ---------------------------------------------------------------------------
// providerDisplayName
// ---------------------------------------------------------------------------
describe("providerDisplayName", () => {
  it("maps known provider ids", () => {
    expect(providerDisplayName("google_books")).toBe("Google Books");
    expect(providerDisplayName("openlibrary")).toBe("OpenLibrary");
    expect(providerDisplayName("comic_vine")).toBe("Comic Vine");
    expect(providerDisplayName("bnf")).toBe("BnF");
  });

  it("falls back to the raw id for unknown providers", () => {
    expect(providerDisplayName("somenew_api")).toBe("somenew_api");
  });
});

import { isValidHttpUrl } from "./utils";

describe("isValidHttpUrl", () => {
  it("accepts http and https absolute URLs", () => {
    expect(isValidHttpUrl("https://example.com/opds")).toBe(true);
    expect(isValidHttpUrl("http://192.168.0.5:8080/feed")).toBe(true);
    expect(isValidHttpUrl("  https://trimmed.example/ ")).toBe(true);
  });
  it("rejects empty, malformed, and non-http schemes", () => {
    expect(isValidHttpUrl("")).toBe(false);
    expect(isValidHttpUrl("not a url")).toBe(false);
    expect(isValidHttpUrl("ftp://example.com")).toBe(false);
    expect(isValidHttpUrl("javascript:alert(1)")).toBe(false);
    expect(isValidHttpUrl("example.com")).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// computeTagBookCounts (F2g — counts reflect the currently-filtered set)
// ---------------------------------------------------------------------------

describe("computeTagBookCounts", () => {
  const bookTagMap = new Map<string, Set<string>>([
    ["b1", new Set(["t1", "t2"])],
    ["b2", new Set(["t1"])],
    ["b3", new Set(["t3"])],
    ["b4", new Set(["t2"])],
  ]);

  it("counts every book in the library when nothing is filtered out", () => {
    const books = [{ id: "b1" }, { id: "b2" }, { id: "b3" }, { id: "b4" }];
    const counts = computeTagBookCounts(books, bookTagMap);
    expect(counts.get("t1")).toBe(2);
    expect(counts.get("t2")).toBe(2);
    expect(counts.get("t3")).toBe(1);
  });

  it("reflects the currently-filtered book set, not the whole library", () => {
    // Simulate another active filter that left only b2 and b3 visible.
    const filtered = [{ id: "b2" }, { id: "b3" }];
    const counts = computeTagBookCounts(filtered, bookTagMap);
    expect(counts.get("t1")).toBe(1); // only b2 carries t1 in the filtered set
    expect(counts.get("t3")).toBe(1); // only b3 carries t3
    expect(counts.get("t2")).toBeUndefined(); // b1/b4 filtered out -> no t2
  });

  it("ignores books with no tags and yields an empty map for an empty set", () => {
    expect(computeTagBookCounts([], bookTagMap).size).toBe(0);
    const counts = computeTagBookCounts([{ id: "unknown" }], bookTagMap);
    expect(counts.size).toBe(0);
  });
});

// ---------------------------------------------------------------------------
// validateWebServerPort
// ---------------------------------------------------------------------------

describe("validateWebServerPort", () => {
  it("accepts an in-range port and returns the parsed number", () => {
    expect(validateWebServerPort("7788")).toEqual({ valid: true, port: 7788 });
  });

  it("accepts the boundary values", () => {
    expect(validateWebServerPort(String(WEB_SERVER_PORT_MIN))).toEqual({
      valid: true,
      port: WEB_SERVER_PORT_MIN,
    });
    expect(validateWebServerPort(String(WEB_SERVER_PORT_MAX))).toEqual({
      valid: true,
      port: WEB_SERVER_PORT_MAX,
    });
  });

  it("rejects values above the range (no silent clamp to 65535)", () => {
    expect(validateWebServerPort("99999")).toEqual({ valid: false });
  });

  it("rejects values below the range", () => {
    expect(validateWebServerPort("80")).toEqual({ valid: false });
    expect(validateWebServerPort("0")).toEqual({ valid: false });
  });

  it("rejects non-numeric, empty, and negative input", () => {
    expect(validateWebServerPort("")).toEqual({ valid: false });
    expect(validateWebServerPort("abc")).toEqual({ valid: false });
    expect(validateWebServerPort("-1")).toEqual({ valid: false });
    expect(validateWebServerPort("80.5")).toEqual({ valid: false });
  });

  it("tolerates surrounding whitespace", () => {
    expect(validateWebServerPort("  7788  ")).toEqual({ valid: true, port: 7788 });
  });
});

describe("formatBytes", () => {
  it("formats zero", () => {
    expect(formatBytes(0)).toBe("0 B");
  });

  it("formats bytes with no decimals", () => {
    expect(formatBytes(512)).toBe("512 B");
  });

  it("formats kilobytes", () => {
    expect(formatBytes(2048)).toBe("2.0 KB");
  });

  it("formats megabytes", () => {
    expect(formatBytes(2.4 * 1024 * 1024)).toBe("2.4 MB");
  });

  it("formats gigabytes", () => {
    expect(formatBytes(3 * 1024 * 1024 * 1024)).toBe("3.0 GB");
  });

  it("returns empty string for null/undefined", () => {
    expect(formatBytes(null)).toBe("");
    expect(formatBytes(undefined)).toBe("");
  });
});

// ---------------------------------------------------------------------------
// Reading heatmap (F-5-4): bucketing + week-grid construction
// ---------------------------------------------------------------------------

describe("getHeatmapBucket", () => {
  it("returns 0 for no reading", () => {
    expect(getHeatmapBucket(0)).toBe(0);
    expect(getHeatmapBucket(-5)).toBe(0);
  });

  it("returns bucket 1 for any positive reading below 15 minutes, however brief", () => {
    // Regression: Math.round(seconds/60) used to map 1-29s to 0 minutes,
    // making a day with real reading indistinguishable from an empty one.
    expect(getHeatmapBucket(25)).toBe(1);
    expect(getHeatmapBucket(899)).toBe(1); // 14m59s
  });

  it("does not round minutes up into the next bucket near a threshold", () => {
    // Regression: 14m31s used to round to 15 minutes and jump to bucket 2.
    expect(getHeatmapBucket(871)).toBe(1); // 14m31s
  });

  it("treats threshold boundaries as inclusive on the higher bucket", () => {
    expect(getHeatmapBucket(900)).toBe(2); // 15m
    expect(getHeatmapBucket(1799)).toBe(2); // 29m59s
    expect(getHeatmapBucket(1800)).toBe(3); // 30m
    expect(getHeatmapBucket(3599)).toBe(3); // 59m59s
    expect(getHeatmapBucket(3600)).toBe(4); // 60m
  });

  it("caps at the top bucket for very long reading days", () => {
    expect(getHeatmapBucket(600 * 60)).toBe(4);
  });
});

describe("toDateKey", () => {
  it("formats a local date as YYYY-MM-DD with zero-padding", () => {
    expect(toDateKey(new Date(2026, 0, 5))).toBe("2026-01-05");
    expect(toDateKey(new Date(2026, 11, 31))).toBe("2026-12-31");
  });
});

describe("buildHeatmapWeeks", () => {
  const today = new Date(2026, 6, 6); // 2026-07-06, a Monday

  it("covers the full 365-day window plus week-alignment padding", () => {
    const weeks = buildHeatmapWeeks([], today);
    const totalDays = weeks.reduce((sum, w) => sum + w.length, 0);
    expect(totalDays).toBeGreaterThanOrEqual(HEATMAP_DAYS);
    // Every week is a complete 7-day column.
    for (const week of weeks) expect(week).toHaveLength(7);
  });

  it("orders weeks oldest to newest, with today in the last week", () => {
    const weeks = buildHeatmapWeeks([], today);
    const lastWeek = weeks[weeks.length - 1];
    expect(lastWeek.some((d) => d.date === toDateKey(today))).toBe(true);
    const firstDate = weeks[0][0].date;
    const lastDate = lastWeek[lastWeek.length - 1].date;
    expect(firstDate < toDateKey(today)).toBe(true);
    expect(lastDate >= toDateKey(today)).toBe(true);
  });

  it("starts each week on Sunday by default", () => {
    const weeks = buildHeatmapWeeks([], today);
    for (const week of weeks) {
      expect(new Date(`${week[0].date}T00:00:00`).getDay()).toBe(0);
    }
  });

  it("marks padding days outside the 365-day window as out of range", () => {
    const weeks = buildHeatmapWeeks([], today);
    const allDays = weeks.flat();
    expect(allDays.filter((d) => d.inRange)).toHaveLength(HEATMAP_DAYS);

    const todayKey = toDateKey(today);
    const rangeStart = new Date(today);
    rangeStart.setDate(rangeStart.getDate() - (HEATMAP_DAYS - 1));
    const rangeStartKey = toDateKey(rangeStart);

    for (const d of allDays) {
      const expectedInRange = d.date >= rangeStartKey && d.date <= todayKey;
      expect(d.inRange).toBe(expectedInRange);
    }
  });

  it("maps seconds to the correct bucket for a given day", () => {
    const todayKey = toDateKey(today);
    const weeks = buildHeatmapWeeks([[todayKey, 45 * 60]], today);
    const day = weeks.flat().find((d) => d.date === todayKey);
    expect(day?.seconds).toBe(45 * 60);
    expect(day?.bucket).toBe(3);
  });

  it("treats days with no session as bucket 0 (rendered as the lowest intensity)", () => {
    const weeks = buildHeatmapWeeks([], today);
    const day = weeks.flat().find((d) => d.date === toDateKey(today));
    expect(day?.bucket).toBe(0);
  });
});

describe("getHeatmapMonthLabels", () => {
  it("labels the week column containing the 1st of a month", () => {
    const today = new Date(2026, 6, 6);
    const weeks = buildHeatmapWeeks([], today);
    const labels = getHeatmapMonthLabels(weeks);
    expect(labels).toHaveLength(weeks.length);

    // Find the week that actually contains 2026-07-01 and confirm it's
    // labeled July (month index 6); no other week should also claim it.
    const julyWeekIndex = weeks.findIndex((w) => w.some((d) => d.date === "2026-07-01"));
    expect(labels[julyWeekIndex]).toBe(6);
  });

  it("returns null for weeks that don't start a month", () => {
    const today = new Date(2026, 6, 6);
    const weeks = buildHeatmapWeeks([], today);
    const midMonthWeekIndex = weeks.findIndex((w) => w.some((d) => d.date === "2026-07-06"));
    expect(getHeatmapMonthLabels(weeks)[midMonthWeekIndex]).toBeNull();
  });

  it("does not label a future month whose 1st falls only in end-of-grid padding", () => {
    // 2026-07-29 is a Wednesday (mid-week). The grid pads forward to the
    // Saturday of the current week, so 2026-08-01 appears as an
    // out-of-range padding cell in the last week column. Without the fix,
    // that column would claim the August label — a duplicate of the label
    // already correctly placed on last year's real August (2025-08-01,
    // which does fall inside the 365-day window).
    const today = new Date(2026, 6, 29);
    const weeks = buildHeatmapWeeks([], today);
    const lastWeekIndex = weeks.length - 1;
    expect(weeks[lastWeekIndex].some((d) => d.date === "2026-08-01" && !d.inRange)).toBe(true);

    const labels = getHeatmapMonthLabels(weeks);
    expect(labels[lastWeekIndex]).toBeNull();

    // The legitimate August label (from 2025-08-01, in range) still appears
    // exactly once elsewhere in the grid.
    const augustLabels = labels.filter((label) => label === 7);
    expect(augustLabels).toHaveLength(1);
  });
});

// ---------------------------------------------------------------------------
// Yearly reading goal (F-1-3): day-of-year + pace comparison
// ---------------------------------------------------------------------------

describe("getDayOfYear", () => {
  it("returns 1 for January 1st", () => {
    expect(getDayOfYear(new Date(2026, 0, 1))).toBe(1);
  });

  it("returns 365 for December 31st of a non-leap year", () => {
    expect(getDayOfYear(new Date(2026, 11, 31))).toBe(365);
  });

  it("returns 366 for December 31st of a leap year", () => {
    expect(getDayOfYear(new Date(2028, 11, 31))).toBe(366);
  });

  it("returns 166 for June 15th of a non-leap year", () => {
    expect(getDayOfYear(new Date(2026, 5, 15))).toBe(166);
  });
});

describe("computeReadingPace", () => {
  it("day 1: expects 0 books yet, any finished book is ahead", () => {
    expect(computeReadingPace(0, 12, 1)).toEqual({ status: "onTrack", count: 0, expected: 0 });
    expect(computeReadingPace(1, 12, 1)).toEqual({ status: "ahead", count: 1, expected: 0 });
  });

  it("day 365: expects the full goal", () => {
    expect(computeReadingPace(365, 365, 365)).toEqual({ status: "onTrack", count: 0, expected: 365 });
    expect(computeReadingPace(364, 365, 365)).toEqual({ status: "behind", count: 1, expected: 365 });
    expect(computeReadingPace(366, 365, 365)).toEqual({ status: "ahead", count: 1, expected: 365 });
  });

  it("reports ahead of schedule", () => {
    // goal 12, day 182 -> expected floor(12*182/365) = 5
    expect(computeReadingPace(8, 12, 182)).toEqual({ status: "ahead", count: 3, expected: 5 });
  });

  it("reports behind schedule", () => {
    expect(computeReadingPace(2, 12, 182)).toEqual({ status: "behind", count: 3, expected: 5 });
  });

  it("reports on track when finished matches the expected pace exactly", () => {
    expect(computeReadingPace(5, 12, 182)).toEqual({ status: "onTrack", count: 0, expected: 5 });
  });
});
