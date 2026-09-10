import { readFileSync } from "fs";
import { resolve } from "path";
import type { Plugin } from "vite";

export interface ReleaseEntry {
  title: string;
  description: string;
}

export interface ReleaseVersion {
  version: string;
  date: string;
  categories: Record<string, ReleaseEntry[]>;
}

const VERSION_RE = /^## \[(.+?)\]\s*-\s*(.+)$/;
const CATEGORY_RE = /^### (.+)$/;
const ENTRY_RE = /^- \*\*(.+?)\*\*(.*)$/;
const PLAIN_ENTRY_RE = /^- (.+)$/;

/**
 * CHANGELOG entries wrap across several lines, but the parser below matches one
 * line at a time — so a `- **Title**` whose closing `**` sits on the next line
 * would otherwise surface in the UI as a sentence cut mid-word with a literal
 * `**` on the front. Join each entry's continuation lines (indented two spaces,
 * not themselves a bullet) back onto its `- ` line first. Nested bullets and
 * their own deeper-indented continuations are dropped, as they always were.
 */
function unwrapEntries(lines: string[]): string[] {
  const out: string[] = [];
  let entryIndex = -1;

  for (const line of lines) {
    if (/^- /.test(line)) {
      out.push(line);
      entryIndex = out.length - 1;
      continue;
    }

    if (entryIndex >= 0 && /^ {2}(?![-*] )\S/.test(line)) {
      out[entryIndex] += " " + line.trim();
      continue;
    }

    if (/^\s+\S/.test(line)) {
      // A nested bullet or one of its continuations: skipped, and it ends the
      // parent entry so nothing after it can be glued back on.
      entryIndex = -1;
      continue;
    }

    out.push(line);
    entryIndex = -1;
  }

  return out;
}

export function parseChangelog(raw: string, maxVersions = 3): ReleaseVersion[] {
  const lines = unwrapEntries(raw.split("\n"));
  const versions: ReleaseVersion[] = [];
  let current: ReleaseVersion | null = null;
  let currentCategory = "";

  for (const line of lines) {
    const versionMatch = line.match(VERSION_RE);
    if (versionMatch) {
      if (versionMatch[1] === "Unreleased") {
        current = null;
        continue;
      }
      if (versions.length >= maxVersions) break;
      current = {
        version: versionMatch[1],
        date: versionMatch[2].trim(),
        categories: {},
      };
      versions.push(current);
      currentCategory = "";
      continue;
    }

    if (!current) continue;

    const categoryMatch = line.match(CATEGORY_RE);
    if (categoryMatch) {
      currentCategory = categoryMatch[1];
      if (!current.categories[currentCategory]) {
        current.categories[currentCategory] = [];
      }
      continue;
    }

    if (!currentCategory) continue;

    const entryMatch = line.match(ENTRY_RE);
    if (entryMatch) {
      current.categories[currentCategory].push({
        title: entryMatch[1],
        description: entryMatch[2].replace(/^\s*[.\-—]\s*/, "").replace(/\.\s*$/, ".").trim(),
      });
      continue;
    }

    const plainMatch = line.match(PLAIN_ENTRY_RE);
    if (plainMatch && !line.startsWith("  ")) {
      current.categories[currentCategory].push({
        title: plainMatch[1].replace(/\.\s*$/, ".").trim(),
        description: "",
      });
    }
  }

  return versions;
}

export default function releaseNotesPlugin(): Plugin {
  const virtualModuleId = "virtual:release-notes";
  const resolvedId = "\0" + virtualModuleId;

  return {
    name: "vite-plugin-release-notes",
    resolveId(id) {
      if (id === virtualModuleId) return resolvedId;
    },
    load(id) {
      if (id !== resolvedId) return;

      const changelogPath = resolve(__dirname, "CHANGELOG.md");
      const raw = readFileSync(changelogPath, "utf-8");
      const notes = parseChangelog(raw, 3);

      const pkgPath = resolve(__dirname, "package.json");
      const pkg = JSON.parse(readFileSync(pkgPath, "utf-8"));

      return `export const releaseNotes = ${JSON.stringify(notes)};
export const appVersion = ${JSON.stringify(pkg.version)};
`;
    },
  };
}
