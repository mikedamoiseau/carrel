# Folio — OWASP Security Review: File-Parsing & EPUB-Rendering Paths

Scope: EPUB/MOBI/comic parsing, archive & cover extraction, and the EPUB→webview render path.
Method: OWASP Top 10:2025 code review (static). Not a running-app scan — see the ZAP baseline for that.
Reviewer: automated review via the `owasp-security` skill. Local file; not for the public plugin repo.

## Summary

The parsing layer is in better shape than the average media-parsing codebase. Path traversal / zip-slip and decompression bombs are actively defended, and XML is handled in a way that sidesteps XXE. The findings below are mostly **defense-in-depth gaps and one documentation drift**, not open holes. Nothing here is a confirmed RCE; the highest-value risk is the unavoidable C-FFI memory surface in MOBI.

| # | Finding | OWASP | Severity | Confidence | Status |
|---|---------|-------|----------|------------|--------|
| 1 | Client-side DOMPurify layer claimed but absent | A02 / A05 | Medium | High | ✅ Resolved (PR #107, M1) |
| 2 | MOBI parsed via `unsafe` C FFI (libmobi) on untrusted input | A06 (memory safety) | Medium | High | ✅ Documented (PR #107, M2) |
| 3 | Hand-rolled `<img>` src rewriter runs post-sanitization | A05 | Low–Med | Medium | ✅ Fixed (PR #107, M3) |
| 4 | `sanitizeCss` is a regex blocklist (bypass-prone) | A05 / A03 | Low | Medium | Open (out of scope) |
| 5 | `dangerouslySetInnerHTML` on i18n + QR strings | A03 (XSS) | Low | Medium | Open (out of scope) |

> **Update 2026-07-23:** #1–#3 addressed on `main` via PR #107 (merge commit `eef85f6`).
> - #1 — real DOMPurify layer added (`src/lib/sanitizeHtml.ts`), both `ReaderPane` sinks sanitized; the CLAUDE.md claim is now accurate.
> - #2 — code guards + `LIBMOBI_VERSION` pin already existed; added a trust-boundary / CVE-bump doc atop `folio-core/src/mobi/mod.rs`. Not code-hardened further (system-linked lib; no fuzz harness — pinned stable toolchain).
> - #3 — probing found a *real reachable* bug (ammonia leaves `>` literal in attr values → tag truncated → `src` unrewritten). Fixed with a quote-aware `find_tag_end`.
> #4 and #5 (both Low) were deliberately left out of scope.

## Positives (verified, worth keeping)

- **Path traversal / zip-slip is defended twice.** `sanitize_cover_href` (`folio-core/src/epub.rs:524`) rejects null bytes, absolute paths, Windows drive prefixes, and `..` escapes; `storage::validate_key` (`folio-core/src/storage.rs:88`) independently rejects `\`, absolute/drive keys, empty and `.`/`..` segments. Cover/image bytes are read into memory and re-stored under derived keys (`{book_id}/{index}/{hash}-{basename}`), never written to an attacker-named path.
- **Decompression-bomb caps.** `MAX_ARCHIVE_ENTRIES = 10_000` and `MAX_ENTRY_SIZE = 100 MB` are enforced with an O(n) pre-scan over the central directory (`validate_archive`, `folio-core/src/epub.rs`), and entry reads are additionally bounded by `Read::take` (`MAX_TEXT_ENTRY_SIZE = 16 MB` for text entries, `MAX_ENTRY_SIZE` for binary ones) so an entry that lies about its decompressed size cannot expand past the cap.
  - **Correction (2026-07-27):** until this date the pre-scan was *not* reached by `folio-core`'s own path-based entry points — only `CachedEpubArchive::open`, `cbz::open_archive`, and the desktop import (which calls `validate_archive` itself) enforced it. `parse_epub_metadata`, `get_chapter_content`, `get_chapter_list`, `extract_cover`, and `get_toc` opened the zip with a bare `ZipArchive::new`, so the LAN web server's chapter route (`api.rs:752`) parsed unvalidated archives. Fixed by routing every path-based wrapper through one `open_validated` helper, plus the bounded reads described above. Reads in `cbz.rs` (`:84` ComicInfo XML, `:156`/`:205` page bytes) are still unbounded — the archive pre-scan applies there, but a size-understating entry is not capped on read.
- **XXE effectively mitigated.** OPF/ComicInfo metadata is pulled with hand-rolled string extraction (`extract_tag_text`, `find_cover_href`), and `quick-xml` (used elsewhere) does not resolve external entities/DTDs by default. No entity-expansion (billion-laughs) surface.
- **Comic archives read into memory** (`cbz.rs`, `cbr.rs` via `entry.read()`), not extracted to disk — so zip-slip does not apply to comics either.
- **All EPUB chapter HTML passes through `ammonia::clean()`** before it reaches the UI (`epub.rs:699/745/852/921/946`).

---

## Findings

### 1. Client-side DOMPurify layer is claimed but does not exist — A02 / A05 — Medium

`CLAUDE.md` states *"EPUB HTML is sanitized server-side (ammonia) and client-side (DOMPurify)."* Code comments repeat this (`epub.rs:881`, the img-rewriter note "DOMPurify strips SVG data URIs by default", `ReaderPane.tsx`). **There is no DOMPurify dependency or call in the frontend** — the only match in `src/` is a comment in `utils.test.ts:652`, and it is absent from `package.json`.

The EPUB chapter HTML is injected at **two** sinks in `ReaderPane.tsx`:

```tsx
// src/components/ReaderPane.tsx:3136
<div className="reader-content" dangerouslySetInnerHTML={{ __html: html }} />
// src/components/ReaderPane.tsx:3154 — search-highlight variant
<div ... dangerouslySetInnerHTML={{ __html: searchHighlightedHtml }} />
```

`searchHighlightedHtml` (`ReaderPane.tsx:1547`) derives from the same server-sanitized HTML and only wraps the user's own (regex-escaped, text-run-only) search query in `<mark>`, so it adds no new injection class — but it inherits the same single-layer exposure and must be covered by any fix.

So the documented two-layer defense-in-depth is actually **single-layer**: server-side `ammonia` is the only thing between an attacker-authored EPUB and the DOM. Ammonia's default policy is solid, but any gap in it (or in the post-clean `<img>` rewriter, finding #3) reaches the webview directly. Tauri's CSP mitigates script execution, but CSP is a backstop, not sanitization.

**Fix (pick one):**
- Restore the intended layer: run `DOMPurify.sanitize(html)` on the string before `dangerouslySetInnerHTML` (and keep server-side ammonia). This is the option the docs/comments assume.
- Or, if ammonia-only is a deliberate decision, correct `CLAUDE.md` and the comments so the security model is documented accurately, and add a test asserting ammonia strips `onerror`/`<script>`/`javascript:` for the chapter path.

### 2. MOBI parsed via `unsafe` C FFI (libmobi) on fully untrusted input — A06 — Medium

`folio-core/src/mobi/` wraps libmobi through `unsafe extern "C"` (`ffi.rs`, `mod.rs`). Parsing is applied to attacker-controlled `.mobi` files, including raw pointer/length trust at **two** sites:

```rust
// folio-core/src/mobi/mod.rs:193
let data = slice::from_raw_parts(rec_ref.data, rec_ref.size);
// folio-core/src/mobi/mod.rs:314
unsafe { slice::from_raw_parts(part.data, part.size) }
```

If libmobi mishandles a malformed MOBI (heap overflow, OOB read, integer overflow on `size`), this is memory corruption in the app process. This is inherent to using a C parser, and the wrapper does the right things (`NonNull` handles, RAII `Drop` frees) — but opening untrusted ebooks is Folio's core function, so the exposure is real.

**Fix / hardening:**
- Pin the libmobi version and track its CVEs/commits; document the bump process alongside the toolchain pin.
- Fuzz the adapter (`cargo-fuzz`) with a malformed-MOBI corpus; the existing fixture harness is a good starting point.
- Sanity-check `part.size` against the record/file bounds before `from_raw_parts` where libmobi's own validation is unclear.
- Consider running MOBI import in a lower-privilege/isolated context long-term (defense-in-depth for the whole native-parser class).

### 3. Hand-rolled `<img>` src rewriter runs after sanitization — A05 — Low–Medium

`rewrite_img_srcs_to_asset_urls` (`epub.rs:1045`) string-scans for `<img`, extracts `src`, and rebuilds tags *after* `ammonia::clean()`. It correctly operates on already-sanitized HTML (so event handlers are already gone) and leaves external/`data:`/`svg` sources untouched. Risk is low, but hand-rolled HTML string manipulation is fragile and easy to regress. **Note:** the function already has ~10 unit tests (`epub.rs:1816–2016`), so the original "add tests" advice is partly moot.

**Fix:** extend the existing tests for the still-uncovered edge cases (attributes with `>` inside quoted values, duplicate `src`), or rewrite over a parsed DOM/`ammonia`'s tree rather than raw strings. Keep sanitize-before-rewrite ordering.

### 4. `sanitizeCss` is a regex blocklist — A05 / A03 — Low

`src/lib/utils.ts:389` strips `url(`, `@import`, `expression(`, `javascript:`, `-moz-binding`, `@font-face` via regex. Blocklist CSS sanitization is bypass-prone (the OWASP AST08 "pattern-matching is not enough" point applies to CSS too). Scope is limited: it's the **user's own** custom CSS applied to their own app, so worst case is self-inflicted (CSS-based data exfiltration is still theoretically possible via remaining vectors).

**Fix:** prefer an allowlist of properties/values, or document explicitly that custom CSS is trusted user input and out of the threat model.

### 5. `dangerouslySetInnerHTML` on i18n and QR strings — A03 — Low

Uses at `BookmarksPanel.tsx:152`, `SettingsPanel.tsx:1632` (i18n `t(...)`) and `SettingsPanel.tsx:2536` (`webServerQr` SVG). These are bundle/developer-controlled or locally generated, so risk is low today. It becomes real **if translations ever accept community/user contributions** or the QR SVG is ever built from remote input.

**Fix:** avoid `dangerouslySetInnerHTML` for i18n (render structured nodes instead); confirm `webServerQr` is generated locally from the LAN URL only.

---

## Not in scope / next step

This is static review of the parsing/render paths. The embedded web server (`src-tauri/src/web_server/`) is a separate HTTP attack surface — run the `zap-scan` baseline against the `web_e2e_server` harness to cover headers, cookies, CSP, and info disclosure there.
