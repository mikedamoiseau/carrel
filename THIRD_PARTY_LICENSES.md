# Third-Party Licenses

Carrel is released under the MIT License (see [`LICENSE`](./LICENSE)). It links
against or bundles the following third-party components. Each entry below
credits the upstream project, states how Carrel uses it, and points at the
canonical license text.

The full text of every license referenced here is available at
<https://opensource.org/licenses> and mirrored by the upstream projects.

Rust crate dependencies are enumerated with their licenses in
[`Cargo.lock`](./Cargo.lock); JavaScript dependencies in
[`package-lock.json`](./package-lock.json). The entries below call out only
the components that affect distribution — native libraries that Carrel links
to or ships in its release artifacts.

---

## libmobi

- **Upstream:** <https://github.com/bfabiszewski/libmobi>
- **License:** GNU Lesser General Public License v3.0 or later (LGPL-3.0-or-later)
- **License text:** <https://www.gnu.org/licenses/lgpl-3.0.html>
- **How Carrel uses it:** Carrel links against libmobi to parse MOBI / AZW /
  AZW3 (KF8) ebook files. The link mode differs by platform — see
  *Distribution* below. Enabled when Carrel is built with the `mobi` cargo
  feature.
- **Platform availability:** MOBI support is compiled into **Linux**,
  **arm64 (Apple Silicon) macOS**, and **Windows** release builds. The
  **x86_64 (Intel) macOS** build is the only release that ships without
  MOBI support — Homebrew on the macos-latest CI runner installs an
  arm64-only libmobi.dylib that cannot be linked into an x86_64 target.

### Distribution

Carrel's link mode for libmobi is platform-specific:

- **Linux** and **arm64 macOS**: libmobi is **dynamically linked** at
  process load time. The release binaries do **not** ship libmobi —
  end users **must install libmobi before first launch**, otherwise the
  app fails to start (not just MOBI open). Install from the system
  package manager:
  - macOS (Apple Silicon): `brew install libmobi`
  - Debian / Ubuntu: `sudo apt install libmobi0` (the `.deb` package
    declares this as a dependency, so a typical `apt install` of Carrel
    pulls libmobi in automatically)
  - Fedora / RHEL: `sudo dnf install libmobi`

- **Windows**: libmobi is **statically linked** into `Carrel.exe`. The
  Tauri bundler does not place sibling DLLs where the OS loader expects
  them at process start, so the Windows build of libmobi is produced as
  a self-contained `mobi.lib` static archive (CMake configured with
  `BUILD_SHARED_LIBS=OFF`, `USE_ZLIB=OFF`, `USE_LIBXML2=OFF`) and baked
  directly into the executable. End users do **not** install libmobi
  separately on Windows.

### LGPL compliance notes

LGPL v3+ permits both dynamic and static linking from MIT-licensed
applications, subject to the obligations below. The dynamic and static
paths satisfy the *relinking* obligation differently; both are addressed.

1. **Attribution.** This file and the in-app About screen identify
   libmobi, its license, and its upstream source.
2. **License availability.** The LGPL v3 text is available at the link
   above and is reproduced in the upstream libmobi repository's
   `COPYING` file.
3. **Source availability.** libmobi's source is published on GitHub at
   the upstream URL. The exact commit Carrel links against is pinned by
   `LIBMOBI_VERSION` in
   [`.github/workflows/release.yml`](./.github/workflows/release.yml)
   so any release can be matched to the specific upstream revision.
   Carrel does not carry a fork or apply patches.
4. **Relinking.**
   - **Linux / arm64 macOS (dynamic):** Users may replace the shipped
     shared library with their own compatible build of libmobi without
     modifying or rebuilding Carrel. No further action is required from
     the application beyond the dynamic-library layout.
   - **Windows (static):** LGPL §6 requires that recipients be able to
     re-link Carrel against a modified libmobi. The complete chain
     needed for this is published openly:
     - Carrel's source is on GitHub under MIT — see [the repository root](./).
     - The libmobi commit Carrel statically links is pinned by SHA in
       `release.yml` (see *Source availability* above) and the exact
       CMake recipe used to produce the static archive is in the
       same workflow file (`Build libmobi (Windows)` step).
     - A user wishing to substitute a modified libmobi can rebuild
       Carrel from source against a different libmobi by changing the
       pinned SHA (or by replacing `.libmobi-windows/lib/mobi.lib`
       in the build tree before `cargo build`) and re-running the
       Tauri build. The same `LIBMOBI_INCLUDE_DIR` / `LIBMOBI_LIB_DIR`
       env vars used by CI are honoured by `folio-core/build.rs`.

     If you need pre-built object files (`.obj`) or the unmodified
     `mobi.lib` Carrel shipped a particular Windows release with, open
     an issue on the GitHub repository and we will provide them.
5. **Modifications.** Carrel does not modify libmobi. If that ever
   changes, the modified source will be published alongside the
   release.

---

## pdfium (Chromium PDF engine)

- **Upstream:** <https://pdfium.googlesource.com/pdfium/>
- **Binary distribution:** <https://github.com/bblanchon/pdfium-binaries>
- **License:** BSD-3-Clause (Carrel uses the Chromium-style "New BSD" license
  text)
- **License text:** <https://pdfium.googlesource.com/pdfium/+/refs/heads/main/LICENSE>
- **How Carrel uses it:** Dynamically linked to render PDF pages into
  bitmaps for the Reader. Bindings are provided by the
  [`pdfium-render`](https://crates.io/crates/pdfium-render) Rust crate.
- **Distribution:** Release builds bundle the pdfium shared library in
  `src-tauri/resources/`, fetched at build time by
  [`scripts/download-pdfium.sh`](./scripts/download-pdfium.sh).

---

## unrar / unrar_sys

- **Upstream:** <https://www.rarlab.com/rar_add.htm> (via the
  [`unrar`](https://crates.io/crates/unrar) Rust crate)
- **License:** The UnRAR source is distributed under the **UnRAR license**,
  which permits use for decompression but explicitly forbids using the
  source to develop RAR-compatible compression tools. Carrel only reads RAR
  / CBR archives, so this restriction is not at issue.
- **License text:** Included in the upstream source and reproduced in the
  `unrar_sys` crate.
- **How Carrel uses it:** Statically linked for CBR (comic book RAR) archive
  support.

---

## Princeton WordNet 3.1 (in-reader dictionary)

- **Upstream:** <https://wordnet.princeton.edu/> (WNdb-3.1 database)
- **License:** The **WordNet License** — a permissive BSD-style license that
  allows use, copying, modification, and distribution (including commercial)
  provided the copyright notice and license text are retained. See
  <https://wordnet.princeton.edu/license-and-commercial-use>.
- **Copyright:** WordNet 3.1 Copyright 2011 by Princeton University. All
  rights reserved.
- **How Carrel uses it:** The optional offline dictionary artifact (feature
  F-1-1) is a prebuilt SQLite database derived from WordNet 3.1 — single-word
  lemmas, parts of speech, glosses, examples, and morphological exception
  tables. It is downloaded on demand when the reader dictionary is enabled;
  the full license text is embedded in the artifact's `meta` table, and the
  attribution is shown in Settings.

---

## Other notable Rust crates

The following crates ship compiled code in the Carrel binary. Full license
metadata is in `Cargo.lock`; a quick summary of the major runtime
dependencies:

- `tauri`, `tauri-plugin-*` — Apache-2.0 / MIT
- `tokio`, `axum`, `reqwest`, `serde`, `rusqlite`, `r2d2`, `opendal`,
  `zip`, `quick-xml`, `ammonia`, `image` — Apache-2.0 / MIT (dual)
- `pdfium-render` — MIT / Apache-2.0
- `bindgen` (build dependency only) — BSD-3-Clause

---

## JavaScript / TypeScript dependencies

Frontend dependencies (React, Tailwind CSS, DOMPurify, etc.) are documented
in `package.json` / `package-lock.json`. All ship under permissive licenses
(MIT, BSD-3-Clause, Apache-2.0). Tailwind CSS icon sets and fonts bundled
in the app are MIT or SIL Open Font Licensed.

---

## Reporting an omission

If you believe a dependency is missing from this file or that the
attribution is incomplete, please open an issue at the Carrel repository
with the component name and a pointer to its license.
