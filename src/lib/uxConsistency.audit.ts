// Static-analysis helpers for UX-consistency guards.
//
// These pure functions consume source text and return findings. The Vitest
// suite in `uxConsistency.audit.test.ts` walks `src/**/*.{tsx,ts}` and feeds
// each file through them, so the production bundle never imports this module.

import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";

export interface Finding {
  file: string;
  line: number;
  match: string;
}

const SOURCE_EXTENSIONS = new Set([".tsx", ".ts"]);
const TEST_SUFFIXES = [".test.ts", ".test.tsx", ".audit.ts"];

export function collectSourceFiles(root: string): string[] {
  const out: string[] = [];
  const walk = (dir: string) => {
    for (const entry of readdirSync(dir)) {
      const full = join(dir, entry);
      const st = statSync(full);
      if (st.isDirectory()) {
        if (entry === "node_modules" || entry === "dist" || entry === "__tests__") continue;
        walk(full);
        continue;
      }
      const dot = entry.lastIndexOf(".");
      if (dot < 0) continue;
      const ext = entry.slice(dot);
      if (!SOURCE_EXTENSIONS.has(ext)) continue;
      if (TEST_SUFFIXES.some((s) => entry.endsWith(s))) continue;
      out.push(full);
    }
  };
  walk(root);
  return out;
}

// ---------------------------------------------------------------------------
// Spacing — bans arbitrary `[Npx]` / `[Nrem]` spacing values that are not on
// the 4px grid. Tailwind half-step classes (p-1.5, mt-0.5, ...) are explicitly
// allowed: they are the deliberate 2px sub-grid for compact components.
// ---------------------------------------------------------------------------

const SPACING_PROPS = [
  "p", "px", "py", "pt", "pr", "pb", "pl",
  "m", "mx", "my", "mt", "mr", "mb", "ml",
  "gap", "gap-x", "gap-y",
  "space-x", "space-y",
  "inset", "inset-x", "inset-y",
  "top", "right", "bottom", "left",
];

const SPACING_ARBITRARY_RE = new RegExp(
  String.raw`\b(?:${SPACING_PROPS.join("|")})-\[([^\]]+)\]`,
  "g",
);

export function findOffGridSpacing(source: string, file: string): Finding[] {
  const out: Finding[] = [];
  const lines = source.split("\n");
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    SPACING_ARBITRARY_RE.lastIndex = 0;
    let m: RegExpExecArray | null;
    while ((m = SPACING_ARBITRARY_RE.exec(line)) !== null) {
      const value = m[1].trim();
      if (isOnFourPxGrid(value)) continue;
      out.push({ file, line: i + 1, match: m[0] });
    }
  }
  return out;
}

function isOnFourPxGrid(raw: string): boolean {
  // Accept pixel values that are non-negative multiples of 4, and rem values
  // whose pixel equivalent (assuming 16px root) is a multiple of 4.
  const px = raw.match(/^(\d+(?:\.\d+)?)px$/);
  if (px) {
    const n = parseFloat(px[1]);
    return Number.isFinite(n) && n >= 0 && n % 4 === 0;
  }
  const rem = raw.match(/^(\d+(?:\.\d+)?)rem$/);
  if (rem) {
    const n = parseFloat(rem[1]) * 16;
    return Number.isFinite(n) && n >= 0 && Math.abs(n - Math.round(n)) < 1e-9 && n % 4 === 0;
  }
  // Anything non-numeric (CSS variables, calc(), etc.) is allowed — not our concern.
  return true;
}

// Convenience: scan a tree and return all findings.
export function scanTreeForOffGridSpacing(root: string): Finding[] {
  const out: Finding[] = [];
  for (const file of collectSourceFiles(root)) {
    const source = readFileSync(file, "utf8");
    out.push(
      ...findOffGridSpacing(source, relative(root, file)),
    );
  }
  return out;
}

// ---------------------------------------------------------------------------
// SVG stroke-width — the codebase uses two strokes (1.5 for outline icons,
// 2 for filled-edge icons). Loading spinners with `animate-spin` may use a
// thicker stroke (3 or 4) for legibility at small sizes.
// ---------------------------------------------------------------------------

const ALLOWED_STROKES = new Set(["1.5", "2"]);
const SPINNER_STROKES = new Set(["3", "4"]);
const STROKE_RE = /strokeWidth=(?:"([0-9.]+)"|\{([0-9.]+)\})/g;

export function findOffNormStrokeWidth(source: string, file: string): Finding[] {
  const out: Finding[] = [];
  STROKE_RE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = STROKE_RE.exec(source)) !== null) {
    const value = (m[1] ?? m[2]).trim();
    if (ALLOWED_STROKES.has(value)) continue;
    if (SPINNER_STROKES.has(value) && enclosingSvgIsSpinner(source, m.index)) continue;
    const line = source.slice(0, m.index).split("\n").length;
    out.push({ file, line, match: m[0] });
  }
  return out;
}

function enclosingSvgIsSpinner(source: string, pos: number): boolean {
  // Find the nearest preceding "<svg" tag. We don't need a real parser:
  // there are no nested SVGs in this codebase.
  const head = source.slice(0, pos);
  const svgStart = head.lastIndexOf("<svg");
  if (svgStart < 0) return false;
  // Slice from "<svg" through the strokeWidth match — captures the opening
  // tag's attributes plus any intervening JSX. If "animate-spin" appears
  // anywhere in that slice (typically as a className on the svg element or
  // a wrapping element), treat the match as a spinner.
  const slice = source.slice(svgStart, pos);
  return slice.includes("animate-spin");
}

export function scanTreeForOffNormStrokeWidth(root: string): Finding[] {
  const out: Finding[] = [];
  for (const file of collectSourceFiles(root)) {
    const source = readFileSync(file, "utf8");
    out.push(
      ...findOffNormStrokeWidth(source, relative(root, file)),
    );
  }
  return out;
}

// ---------------------------------------------------------------------------
// Animation timing — Tailwind duration classes must come from the cluster the
// codebase already converged on: 150 / 200 / 300 ms. Arbitrary `duration-[…]`
// brackets are banned outright.
// ---------------------------------------------------------------------------

const ALLOWED_DURATIONS = new Set(["150", "200", "300"]);
const DURATION_RE = /\bduration-(\[[^\]]+\]|\d+)(?![a-z0-9_-])/g;

export function findOffClusterDuration(source: string, file: string): Finding[] {
  const out: Finding[] = [];
  const lines = source.split("\n");
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    DURATION_RE.lastIndex = 0;
    let m: RegExpExecArray | null;
    while ((m = DURATION_RE.exec(line)) !== null) {
      const value = m[1];
      if (value.startsWith("[")) {
        out.push({ file, line: i + 1, match: m[0] });
        continue;
      }
      if (!ALLOWED_DURATIONS.has(value)) {
        out.push({ file, line: i + 1, match: m[0] });
      }
    }
  }
  return out;
}

export function scanTreeForOffClusterDuration(root: string): Finding[] {
  const out: Finding[] = [];
  for (const file of collectSourceFiles(root)) {
    const source = readFileSync(file, "utf8");
    out.push(
      ...findOffClusterDuration(source, relative(root, file)),
    );
  }
  return out;
}

// ---------------------------------------------------------------------------
// SettingsPanel section ordering — locks the accordion list so future
// reorders are intentional. Tracked as a static-text snapshot so an empty
// 1-button section (like the old "Activity" launcher) can't sneak back in
// without explicitly updating the expected list.
// ---------------------------------------------------------------------------

const ACCORDION_TITLE_RE = /<Accordion\s+title=\{t\(["']settings\.([a-zA-Z]+)["']\)\}/g;

export function findSettingsSections(source: string): string[] {
  const out: string[] = [];
  ACCORDION_TITLE_RE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = ACCORDION_TITLE_RE.exec(source)) !== null) {
    out.push(m[1]);
  }
  return out;
}

// Asserts that a button labelled by `i18nKey` calls `handlerName(true)` in
// its onClick. Used to lock the wiring of moved/relocated launcher buttons
// (e.g. View Activity Log inside the Library section) without standing up
// a full DOM test harness.
export function findButtonOpensModal(
  source: string,
  i18nKey: string,
  handlerName: string,
): boolean {
  // Locate every <button …onClick=… >…t("<i18nKey>")…</button> block and
  // check the onClick line invokes handlerName(true). Buttons in the file
  // are short (≤8 lines) so a small window is enough.
  const labelRe = new RegExp(String.raw`t\(["']${escapeRe(i18nKey)}["']\)`, "g");
  let m: RegExpExecArray | null;
  while ((m = labelRe.exec(source)) !== null) {
    // Slice ~12 lines back from the label match — that covers the whole
    // <button …> opening tag including its onClick.
    const before = source.slice(0, m.index);
    const start = before.split("\n").slice(-12).join("\n");
    if (start.includes(`onClick={() => ${handlerName}(true)}`)) {
      return true;
    }
  }
  return false;
}

function escapeRe(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

// ---------------------------------------------------------------------------
// Dark-mode pass — Tailwind classes using "extreme" shades (50/100/200 for
// pale tints; 700/800/900 for deep saturations) of non-semantic palettes
// (red, amber, gray, …) must have a `dark:` companion in the same className,
// otherwise the surface looks broken on the opposite theme.
//
// Carrel's primary theming is CSS-variable-based (bg-paper, text-ink, …) and
// auto-swaps; the only places that need explicit `dark:` prefixes are
// non-semantic palette colors used for status (errors, warnings) or hard-
// coded surfaces.
// ---------------------------------------------------------------------------

const DARK_PALETTES = ["red", "amber", "yellow", "green", "blue", "gray", "slate", "zinc", "neutral", "stone"];
// Per-property risk shades — only flag what would actually look broken on
// the opposite theme:
//   • bg-{p}-50/100/200 — light tints unreadable on dark surfaces
//   • text-{p}-700/800/900 — deep tints unreadable as text on dark
//   • border-{p}-50/100/200 — light borders disappear on dark surfaces
// Mid-shade saturated bg (red-600/700) is intentional emphasis (destructive
// buttons, badges) and stays the same in both themes.
const RISK_SHADES_BY_PROPERTY: Record<string, string[]> = {
  bg: ["50", "100", "200"],
  text: ["700", "800", "900"],
  border: ["50", "100", "200"],
};

const CLASSNAME_RE = /className=(?:"([^"]*)"|\{`([^`]*)`\})/g;

export function findMissingDarkVariants(source: string, file: string): Finding[] {
  const out: Finding[] = [];
  CLASSNAME_RE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = CLASSNAME_RE.exec(source)) !== null) {
    const classes = m[1] ?? m[2];
    const hits = riskClasses(classes);
    for (const hit of hits) {
      if (!hasDarkCompanion(classes, hit)) {
        const line = source.slice(0, m.index).split("\n").length;
        out.push({ file, line, match: hit.full });
      }
    }
  }
  return out;
}

interface RiskHit {
  full: string; // e.g. "bg-red-50"
  property: string; // "bg" | "text" | "border"
  palette: string;
  shade: string;
}

function riskClasses(classNames: string): RiskHit[] {
  const out: RiskHit[] = [];
  for (const [property, shades] of Object.entries(RISK_SHADES_BY_PROPERTY)) {
    const re = new RegExp(
      String.raw`(?:^|\s|:)((${property})-(${DARK_PALETTES.join("|")})-(${shades.join("|")}))(?![\w-])`,
      "g",
    );
    let m: RegExpExecArray | null;
    while ((m = re.exec(classNames)) !== null) {
      out.push({ full: m[1], property: m[2], palette: m[3], shade: m[4] });
    }
  }
  return out;
}

function hasDarkCompanion(classNames: string, hit: RiskHit): boolean {
  // A companion is any `dark:<state-prefix?>{property}-{palette}-…` token
  // for the same property + palette. We don't insist on the same shade —
  // common patterns include red-50 → dark:red-900/20.
  const re = new RegExp(
    String.raw`\bdark:(?:hover:|focus:|active:|disabled:)?${hit.property}-${hit.palette}-`,
  );
  return re.test(classNames);
}

export function scanTreeForMissingDarkVariants(root: string): Finding[] {
  const out: Finding[] = [];
  for (const file of collectSourceFiles(root)) {
    const source = readFileSync(file, "utf8");
    out.push(
      ...findMissingDarkVariants(source, relative(root, file)),
    );
  }
  return out;
}
