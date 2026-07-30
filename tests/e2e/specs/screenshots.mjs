// Regenerates the numbered README screenshots in the repo-root /screenshots/
// folder. Run on its own:
//   npx wdio run wdio.conf.mjs --spec ./specs/screenshots.mjs
//
// Pre-reqs:
//   1. Library must contain books (BDs for round 1, classics for round 2).
//      Reader shots (10–13) work best when at least one EPUB exists.
//   2. `npm run dev` + `tauri-wd --port 4444` running.
//
// Capture strategy: WebDriver's takeScreenshot on macOS Tauri returns an
// offscreen buffer that's out-of-sync with the on-screen render. We use macOS
// `screencapture -R` against the window bounds (via AppleScript) to grab the
// real visible pixels. Requires Terminal to have:
//   - Accessibility permission (to read/set window bounds)
//   - Screen Recording permission (to run screencapture)

import { expect } from "@wdio/globals";
import { execFileSync } from "node:child_process";
import { resolve } from "node:path";

const OUT_DIR = resolve(process.cwd(), "../../screenshots");

function getScreenSize() {
  const out = execFileSync(
    "osascript",
    [
      "-e",
      'tell application "Finder" to get bounds of window of desktop',
    ],
    { encoding: "utf8" }
  ).trim();
  // Returns "0, 0, width, height"
  const parts = out.split(",").map((n) => parseInt(n.trim(), 10));
  return { w: parts[2], h: parts[3] };
}

function pickWindowSize() {
  try {
    const screen = getScreenSize();
    // Leave ~80px margin for menu bar + dock. Clamp to a sane upper bound.
    const w = Math.min(2400, Math.max(1440, screen.w - 120));
    const h = Math.min(1500, Math.max(900, screen.h - 160));
    return { w, h };
  } catch {
    return { w: 1800, h: 1150 };
  }
}

const WINDOW_SIZE = pickWindowSize();

// ─── OS / window helpers ───────────────────────────────────────────────────

function resizeCarrelWindow({ w, h }) {
  const script = `
    tell application "System Events"
      set procs to (every process whose name is "carrel" or name is "Carrel")
      if (count of procs) is 0 then error "no carrel process"
      tell (item 1 of procs)
        set size of window 1 to {${w}, ${h}}
      end tell
    end tell
  `;
  execFileSync("osascript", ["-e", script]);
}

function getCarrelWindowBounds() {
  const script = `
    tell application "System Events"
      set procs to (every process whose name is "carrel" or name is "Carrel")
      if (count of procs) is 0 then error "no carrel process"
      tell (item 1 of procs)
        set {x, y} to position of window 1
        set {w, h} to size of window 1
      end tell
    end tell
    return (x as string) & "," & (y as string) & "," & (w as string) & "," & (h as string)
  `;
  const out = execFileSync("osascript", ["-e", script], {
    encoding: "utf8",
  }).trim();
  const [x, y, w, h] = out.split(",").map((n) => parseInt(n.trim(), 10));
  if ([x, y, w, h].some((n) => Number.isNaN(n))) {
    throw new Error(`Could not parse window bounds from: ${out}`);
  }
  return { x, y, w, h };
}

async function capture(filename) {
  // Small settle pause so animations / hover states finish.
  await browser.pause(400);
  const bounds = getCarrelWindowBounds();
  const rect = `${bounds.x},${bounds.y},${bounds.w},${bounds.h}`;
  execFileSync("screencapture", ["-R", rect, "-x", "-o", `${OUT_DIR}/${filename}`]);
  console.log(`[screenshots] wrote ${filename}`);
}

// ─── DOM helpers ───────────────────────────────────────────────────────────

async function waitForLibraryWithBooks() {
  await browser.waitUntil(
    async () => (await browser.$$("button.group")).length > 0,
    { timeout: 20000, timeoutMsg: "No book cards rendered" }
  );
  await browser.pause(1500);
}

async function setThemeAndReload(mode) {
  await browser.execute((m) => {
    localStorage.setItem("carrel-theme", m);
    window.location.reload();
  }, mode);
  await browser.pause(2000);
  await waitForLibraryWithBooks();
}

async function pressEscape() {
  await browser.keys("Escape");
  await browser.pause(300);
}

async function openSettings() {
  const btn = await browser.$('button[aria-label="Open settings"]');
  await btn.click();
  await browser.pause(600);
}

async function closeSettings() {
  // The panel has a close button with aria-label from settings.closeLabel.
  // Escape also closes it reliably.
  await pressEscape();
  await browser.pause(400);
}

async function collapseAllAccordions() {
  // Toggle any accordion whose aria-expanded is "true" so we start clean.
  const expanded = await browser.$$('button[aria-expanded="true"]');
  for (const btn of expanded) {
    try {
      await btn.click();
      await browser.pause(150);
    } catch {}
  }
}

async function openAccordion(title) {
  // The accordion button's visible label is an <h3> with the title text.
  // Click the first button[aria-expanded] whose text matches.
  const buttons = await browser.$$('button[aria-expanded]');
  for (const btn of buttons) {
    const text = (await btn.getText()).trim();
    if (text === title || text.startsWith(title)) {
      const expanded = await btn.getAttribute("aria-expanded");
      if (expanded === "false") {
        await btn.click();
        await browser.pause(500);
      }
      // Scroll into view so the expanded content is visible in the capture.
      await btn.scrollIntoView({ block: "start" });
      await browser.pause(300);
      return true;
    }
  }
  throw new Error(`Accordion not found: ${title}`);
}

async function clickNavButton(ariaLabel) {
  const btn = await browser.$(`button[aria-label="${ariaLabel}"]`);
  await btn.waitForExist({ timeout: 5000 });
  await btn.click();
  await browser.pause(600);
}

// Click all known close buttons and Escape repeatedly to dismiss any open
// overlay (settings, collections, stats, catalogs, detail modal, edit modal,
// shortcuts help, bookmarks, highlights, activity log). Then, if we're on
// /reader/*, click the back-to-library button. Idempotent.
async function resetToLibrary() {
  const closeLabels = [
    "Close settings",
    "Close collections",
    "Close bookmarks",
    "Close highlights",
    "Close activity log",
    "Close",
  ];
  // Multiple passes because some overlays stack.
  for (let pass = 0; pass < 3; pass++) {
    for (const label of closeLabels) {
      const btns = await browser.$$(`button[aria-label="${label}"]`);
      for (const btn of btns) {
        try {
          if (await btn.isDisplayed()) {
            await btn.click();
            await browser.pause(150);
          }
        } catch {}
      }
    }
    await browser.keys("Escape");
    await browser.pause(200);
  }
  // If we're in the reader, navigate back.
  try {
    const url = await browser.execute(() => location.pathname);
    if (typeof url === "string" && url.startsWith("/reader")) {
      const back = await browser.$$('button[aria-label*="library" i]');
      for (const b of back) {
        try {
          if (await b.isDisplayed()) {
            await b.click();
            await browser.pause(600);
            break;
          }
        } catch {}
      }
    }
  } catch {}
  await waitForLibraryWithBooks();
}

// Hover over the first book card, then find the given action button
// WITHIN that card (so we don't accidentally match buttons from the
// collections sidebar that also start with "Edit ").
async function clickCardActionButton(ariaPrefix) {
  const card = await browser.$("button.group");
  await card.moveTo();
  await browser.pause(400);
  // Within the card's subtree, find a button whose aria-label begins with
  // the given prefix (e.g. "Edit " or "Details for ").
  const actionBtn = await card.$(`button[aria-label^="${ariaPrefix}"]`);
  await actionBtn.waitForExist({ timeout: 3000 });
  await actionBtn.click();
  await browser.pause(600);
}

async function clickFirstBookToOpenReader() {
  const card = await browser.$("button.group");
  await card.click();
  // Reader takes a moment to mount and load content.
  await browser.pause(2500);
}

// ─── Specs ─────────────────────────────────────────────────────────────────

describe("README screenshots", () => {
  before(() => {
    console.log(`[screenshots] resizing Carrel window to ${WINDOW_SIZE.w}x${WINDOW_SIZE.h}`);
    resizeCarrelWindow(WINDOW_SIZE);
  });

  // ─── Library — theme variants ────────────────────────────────────────────
  // setThemeAndReload reloads the page so state is automatically clean.

  it("01-library-light", async () => {
    await setThemeAndReload("light");
    await capture("01-library-light.png");
  });

  it("03-library-dark", async () => {
    await setThemeAndReload("dark");
    await capture("03-library-dark.png");
  });

  it("04-library-sepia", async () => {
    await setThemeAndReload("sepia");
    await capture("04-library-sepia.png");
  });

  // ─── Library overlays ────────────────────────────────────────────────────

  it("09-collections", async () => {
    await setThemeAndReload("light"); // clean slate
    await browser.$("body").click();
    await browser.keys("c");
    await browser.pause(600);
    await capture("09-collections.png");
  });

  it("15-book-detail", async () => {
    await setThemeAndReload("light");
    await clickCardActionButton("Details for ");
    await capture("15-book-detail.png");
  });

  it("16-edit-book", async () => {
    await setThemeAndReload("light");
    // The Edit dialog is reached via the BookDetail modal's "Edit" button —
    // there is no edit button on the card itself.
    await clickCardActionButton("Details for ");
    const editBtn = await browser.$(
      "//button[normalize-space(.)='Edit' or normalize-space(.)='Modifier']"
    );
    await editBtn.waitForExist({ timeout: 5000 });
    await editBtn.click();
    await browser.pause(800);
    await capture("16-edit-book.png");
  });

  it("14-keyboard-shortcuts", async () => {
    await setThemeAndReload("light");
    // The Library's keyboard handler fires on `?` or `/ + shiftKey`.
    // WebDriver's chorded modifier input doesn't reliably produce the event
    // the React handler is listening for on Tauri/macOS, so dispatch a
    // synthetic KeyboardEvent directly to the window.
    await browser.execute(() => {
      window.dispatchEvent(
        new KeyboardEvent("keydown", {
          key: "?",
          code: "Slash",
          shiftKey: true,
          bubbles: true,
        })
      );
    });
    await browser.pause(600);
    await capture("14-keyboard-shortcuts.png");
  });

  // ─── Top nav modals ──────────────────────────────────────────────────────

  it("07-reading-stats", async () => {
    await setThemeAndReload("light");
    await clickNavButton("Reading stats");
    await capture("07-reading-stats.png");
  });

  it("08-catalogs", async () => {
    await setThemeAndReload("light");
    await clickNavButton("Browse catalogs");
    await capture("08-catalogs.png");
  });

  // ─── Settings — panel + each accordion ───────────────────────────────────
  // Reload once for a clean state, open settings, then toggle accordions.

  it("02-settings-panel", async () => {
    await setThemeAndReload("light");
    await openSettings();
    await collapseAllAccordions();
    await capture("02-settings-panel.png");
  });

  it("05-settings-typography", async () => {
    await collapseAllAccordions();
    await openAccordion("Appearance");
    // Typography is a sub-section (h4) inside Appearance. The settings
    // panel has its own scrollable container — scrollIntoView on the h4
    // works, but we need a larger pause for the scroll to settle.
    await browser.execute(() => {
      const heading = [...document.querySelectorAll("h4")].find(
        (h) =>
          h.textContent?.trim() === "Typography" ||
          h.textContent?.trim() === "Typographie"
      );
      heading?.scrollIntoView({ block: "center", behavior: "instant" });
    });
    await browser.pause(600);
    await capture("05-settings-typography.png");
  });

  it("06-settings-library", async () => {
    await collapseAllAccordions();
    await openAccordion("Library");
    await capture("06-settings-library.png");
  });

  it("17-settings-page-layout", async () => {
    await collapseAllAccordions();
    await openAccordion("Page Layout");
    await capture("17-settings-page-layout.png");
  });

  it("18-settings-backup-restore", async () => {
    await collapseAllAccordions();
    await openAccordion("Backup & Restore");
    await capture("18-settings-backup-restore.png");
  });

  it("19-settings-metadata-scan", async () => {
    await collapseAllAccordions();
    await openAccordion("Metadata Scan");
    await capture("19-settings-metadata-scan.png");
  });

  it("20-settings-activity", async () => {
    await collapseAllAccordions();
    await openAccordion("Activity");
    await capture("20-settings-activity.png");
  });

  it("21-settings-remote-backup", async () => {
    await collapseAllAccordions();
    await openAccordion("Remote Backup");
    await capture("21-settings-remote-backup.png");
  });

  // ─── Reader ──────────────────────────────────────────────────────────────

  it("10-reader-epub", async () => {
    await setThemeAndReload("light");
    await clickFirstBookToOpenReader();
    await capture("10-reader-epub.png");
  });

  it("11-reader-toc", async () => {
    // `t` toggles the TOC sidebar in the reader.
    await browser.keys("t");
    await browser.pause(600);
    await capture("11-reader-toc.png");
    await browser.keys("t"); // close TOC
    await browser.pause(300);
  });

  it("12-reader-highlights", async () => {
    const btn = await browser.$('button[aria-label="Highlights"]');
    if (!(await btn.isExisting())) return;
    await btn.click();
    await browser.pause(600);
    await capture("12-reader-highlights.png");
    await pressEscape();
  });

  it("13-reader-bookmarks", async () => {
    const btn = await browser.$('button[aria-label="Bookmarks"]');
    if (!(await btn.isExisting())) return;
    await btn.click();
    await browser.pause(600);
    await capture("13-reader-bookmarks.png");
    await pressEscape();
  });

  after(async () => {
    await setThemeAndReload("light").catch(() => {});
  });
});
