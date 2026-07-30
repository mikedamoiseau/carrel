import { describe, it, expect } from "vitest";
import { parseChangelog } from "../../../vite-plugin-release-notes";

const SAMPLE = `# Changelog

## [Unreleased]

### Added
- **Unreleased feature**. Should be skipped.

## [2.0.3] - 2026-05-18

### Added
- **OPDS feed primitives**. Public primitives for rendering OPDS Atom feeds.

## [2.0.0] - 2026-05-03

### Added
- **MOBI / AZW / AZW3 reading** (ROADMAP #34). Mobipocket and Kindle formats via libmobi.
- **Navigation history** (ROADMAP #36). Back/forward stack across the reader.

### Fixed
- **Web server deadlock on auto-start**. The auto-start path held the mutex.

### Changed
- **carrel-core crate extraction** (ROADMAP #63). Modules now live in a separately-tested crate.

## [1.4.1] - 2026-04-15

### Added
- **Tag filter in library toolbar**. Searchable multi-select combobox.
`;

describe("parseChangelog", () => {
  const result = parseChangelog(SAMPLE, 3);

  it("skips Unreleased section", () => {
    expect(result.find((r) => r.version === "Unreleased")).toBeUndefined();
  });

  it("parses version and date", () => {
    expect(result[0]).toMatchObject({ version: "2.0.3", date: "2026-05-18" });
    expect(result[1]).toMatchObject({ version: "2.0.0", date: "2026-05-03" });
  });

  it("groups entries by category", () => {
    const v200 = result.find((r) => r.version === "2.0.0")!;
    expect(Object.keys(v200.categories)).toContain("Added");
    expect(Object.keys(v200.categories)).toContain("Fixed");
    expect(Object.keys(v200.categories)).toContain("Changed");
  });

  it("extracts bold title and description", () => {
    const v200 = result.find((r) => r.version === "2.0.0")!;
    expect(v200.categories["Added"][0]).toEqual({
      title: "MOBI / AZW / AZW3 reading",
      description: "(ROADMAP #34). Mobipocket and Kindle formats via libmobi.",
    });
  });

  it("limits to maxVersions", () => {
    expect(result).toHaveLength(3);
    expect(result[2]).toMatchObject({ version: "1.4.1" });
  });

  it("handles entries without bold title gracefully", () => {
    const plain = parseChangelog("## [1.0.0] - 2026-01-01\n\n### Fixed\n- Plain entry without bold.\n", 1);
    expect(plain[0].categories["Fixed"][0]).toEqual({
      title: "Plain entry without bold.",
      description: "",
    });
  });
});
