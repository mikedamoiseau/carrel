import { describe, expect, it } from "vitest";
import { parseChangelog } from "./vite-plugin-release-notes";

describe("parseChangelog", () => {
  it("joins an entry whose bold title wraps across lines", () => {
    const raw = [
      "## [3.2.0] - 2026-09-07",
      "",
      "### Changed",
      "- **The desktop/LAN web server's OPDS feeds now render through",
      "  `carrel-core::opds_feed`** instead of a second, independently-maintained",
      "  copy of the same Atom-building code.",
      "",
    ].join("\n");

    const [release] = parseChangelog(raw);

    expect(release.categories.Changed).toEqual([
      {
        title: "The desktop/LAN web server's OPDS feeds now render through `carrel-core::opds_feed`",
        description:
          "instead of a second, independently-maintained copy of the same Atom-building code.",
      },
    ]);
  });

  it("joins a plain entry's wrapped lines", () => {
    const raw = [
      "## [3.2.0] - 2026-09-07",
      "",
      "### Fixed",
      "- OPDS clients could be served an empty page in the two paginated",
      "  feeds.",
      "",
    ].join("\n");

    const [release] = parseChangelog(raw);

    expect(release.categories.Fixed).toEqual([
      { title: "OPDS clients could be served an empty page in the two paginated feeds.", description: "" },
    ]);
  });

  it("drops nested bullets and does not glue them onto the parent entry", () => {
    const raw = [
      "## [3.2.0] - 2026-09-07",
      "",
      "### Fixed",
      "- **Two advisories** were reviewed:",
      "  - **RUSTSEC-2026-0194** was reachable. Every place Carrel reads XML",
      "    attributes went through the affected code path.",
      "  - **RUSTSEC-2026-0195** was not reachable.",
      "",
    ].join("\n");

    const [release] = parseChangelog(raw);

    expect(release.categories.Fixed).toEqual([
      { title: "Two advisories", description: "were reviewed:" },
    ]);
  });
});
