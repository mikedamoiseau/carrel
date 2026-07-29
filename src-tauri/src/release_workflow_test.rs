//! Compile-time guards on `.github/workflows/release.yml`.
//!
//! The matrix combines platform targets with feature flags; regressions
//! here only surface at tag-push time, which is too late. These tests
//! assert the invariants we rely on:
//!
//! * Linux + arm64 macOS ship with `--features mobi`.
//! * x86_64 macOS must not enable `mobi` — the macos-latest runner is
//!   arm64, so Homebrew installs an arm64 `libmobi.dylib`. Cross-
//!   linking that into an `x86_64-apple-darwin` target fails at the
//!   linker step ("building for macOS-x86_64 but attempting to link
//!   with file built for macOS-arm64"). Until we ship a universal
//!   libmobi (or drop x86_64 Mac entirely), Intel-Mac users get
//!   EPUB/PDF/CBZ/CBR only.
//! * Windows builds libmobi from source via CMake + MSVC (libmobi has
//!   first-class CMake support since v0.10) with `USE_ZLIB=OFF` and
//!   `USE_LIBXML2=OFF` so the build has no external deps to satisfy.
//!   The libmobi artifact is cached so a rerun on an unchanged pin is
//!   fast.
//! * Windows AND arm64 macOS MOBI linkage is **static**
//!   (`BUILD_SHARED_LIBS=OFF`). Windows produces `mobi.lib`; macOS builds
//!   `libmobi.a` from source and merges in miniz, then `build.rs` links it
//!   statically (gated on the `LIBMOBI_STATIC` env var). This keeps both
//!   releases self-contained — no `mobi.dll` / `libmobi.dylib` to ship, and
//!   crucially no runtime dependency on a Homebrew `libmobi.0.dylib` at an
//!   absolute path (which crashed the app on launch for users without
//!   `brew install libmobi`). Linux keeps dynamic linking (apt libmobi);
//!   the platform difference lives in `folio-core/build.rs`.

#[cfg(test)]
mod tests {
    const RELEASE_YML: &str = include_str!("../../.github/workflows/release.yml");
    const CI_YML: &str = include_str!("../../.github/workflows/ci.yml");

    /// Pull the value of `LIBMOBI_VERSION:` out of a workflow file.
    /// We deliberately *don't* depend on a YAML crate — the only
    /// shape we care about (`LIBMOBI_VERSION: "<value>"`) is stable
    /// and trivial to extract by hand.
    fn parse_libmobi_version(yml: &str) -> &str {
        let line = yml
            .lines()
            .find(|l| l.trim_start().starts_with("LIBMOBI_VERSION:"))
            .expect("workflow must define `LIBMOBI_VERSION`");
        let value = line
            .split_once(':')
            .expect("LIBMOBI_VERSION line must contain a colon")
            .1
            .trim();
        // Strip the surrounding quotes that the YAML uses.
        value.trim_matches('"')
    }

    /// Locate the single `args: '…'` matrix line that contains `needle`
    /// and return the whole line. We do string scanning instead of
    /// pulling in a YAML crate because the workspace doesn't otherwise
    /// need one — the structure we care about is stable and easy to
    /// pattern-match. Lines are filtered to those starting with
    /// `args: '` so surrounding YAML comments can't masquerade as matrix
    /// entries.
    fn args_for(needle: &str) -> &'static str {
        let mut matches = RELEASE_YML
            .lines()
            .filter(|l| l.trim_start().starts_with("args: '"))
            .filter(|l| l.contains(needle));
        let hit = matches
            .next()
            .unwrap_or_else(|| panic!("release.yml has no `args: '…'` line containing `{needle}`"));
        assert!(
            matches.next().is_none(),
            "release.yml has multiple `args: '…'` lines containing `{needle}`; needle is ambiguous"
        );
        hit
    }

    #[test]
    fn linux_build_enables_mobi_feature() {
        let line = args_for("tauri.linux.mobi.conf.json");
        assert!(
            line.contains("--features mobi"),
            "Linux build must enable --features mobi, got: {line}"
        );
    }

    #[test]
    fn aarch64_macos_build_enables_mobi_feature() {
        let line = args_for("aarch64-apple-darwin");
        assert!(
            line.contains("--features mobi"),
            "aarch64 macOS build must enable --features mobi, got: {line}"
        );
    }

    #[test]
    fn x86_64_macos_build_does_not_enable_mobi_feature() {
        // Regression guard: `--features mobi` on x86_64 + arm64 runner
        // fails to link against the arm64 libmobi.dylib Homebrew ships.
        // Flipping this back on requires also producing an x86_64 libmobi
        // (universal dylib, manual build, or Rosetta-cross-install).
        let line = args_for("x86_64-apple-darwin");
        assert!(
            !line.contains("--features mobi"),
            "x86_64 macOS build must NOT enable --features mobi — Homebrew's \
             libmobi is arm64 on macos-latest runners and fails to link into \
             an x86_64 target. Got: {line}"
        );
    }

    #[test]
    fn windows_build_disables_default_features() {
        let line = args_for("-- --no-default-features");
        assert!(
            line.contains("--no-default-features"),
            "Windows build must keep --no-default-features (sftp incompatible with Windows build)"
        );
    }

    /// Windows users get MOBI from the v1.5 series onwards. The matrix
    /// entry must enable the feature explicitly; the libmobi build
    /// step's `if:` guard keys off `--features mobi` being present in
    /// `matrix.args`, so without this flag the libmobi build is also
    /// skipped (and the resulting Windows binary has no MOBI support).
    #[test]
    fn windows_build_enables_mobi_feature() {
        let line = args_for("-- --no-default-features");
        assert!(
            line.contains("--features mobi"),
            "Windows release must enable --features mobi alongside \
             --no-default-features so MOBI/AZW/AZW3 import is compiled \
             into the shipped binary. Got: {line}"
        );
    }

    /// We statically link libmobi on Windows (`BUILD_SHARED_LIBS=OFF`)
    /// so the Windows release ships as a single self-contained
    /// `carrel.exe` — no `mobi.dll` next to the binary, no Tauri
    /// bundler config to coerce the loader's DLL search path.
    /// Switching back to a shared build would require simultaneous
    /// changes to `tauri.conf.json` (resource layout), the OS
    /// DLL-loading story, and `folio-core/build.rs` (link kind), so
    /// the test guards the static decision at the build-config level.
    #[test]
    fn windows_libmobi_build_is_static() {
        assert!(
            RELEASE_YML.contains("-DBUILD_SHARED_LIBS=OFF"),
            "release.yml must build libmobi with `BUILD_SHARED_LIBS=OFF` \
             on Windows so the resulting `mobi.lib` static archive can \
             be linked directly into carrel.exe — flipping this to ON \
             would require shipping `mobi.dll` next to the binary, \
             which Tauri's bundler does not do out of the box."
        );
    }

    /// Once `--features mobi` is on, the Tauri build step needs to
    /// know where libmobi's headers/libs live — `folio-core/build.rs`
    /// uses pkg-config first, falls back to LIBMOBI_INCLUDE_DIR /
    /// LIBMOBI_LIB_DIR. MSVC has no pkg-config in PATH on a stock
    /// runner, so the fallback is the actual exercised path. Without
    /// these env vars the bindgen step fails with "is libmobi
    /// installed?" at link time.
    #[test]
    fn windows_release_passes_libmobi_env_to_tauri_build() {
        assert!(
            RELEASE_YML.contains("LIBMOBI_INCLUDE_DIR:"),
            "release.yml must export `LIBMOBI_INCLUDE_DIR` to the \
             Tauri build step on Windows — folio-core/build.rs needs \
             it to locate `mobi.h` for bindgen. Without this, the \
             Windows release fails at bindgen time."
        );
        assert!(
            RELEASE_YML.contains("LIBMOBI_LIB_DIR:"),
            "release.yml must export `LIBMOBI_LIB_DIR` to the Tauri \
             build step on Windows — folio-core/build.rs needs it to \
             tell rustc where `mobi.lib` lives. Without this, the \
             Windows release fails at link time with \"cannot find \
             -lmobi\"."
        );
    }

    /// Pinning the libmobi commit/tag is what makes the cache deterministic.
    /// If the pin disappears, every CI run rebuilds from `master` and the
    /// cache key collapses to whatever the last run produced.
    #[test]
    fn windows_libmobi_version_is_pinned() {
        assert!(
            RELEASE_YML.contains("LIBMOBI_VERSION:"),
            "release.yml must define a `LIBMOBI_VERSION` env var so the \
             Windows libmobi build is reproducible and cache-keyable. \
             Without a pinned version, every push rebuilds from libmobi's \
             moving `master` branch."
        );
    }

    /// Each build of libmobi from source on a stock Windows runner takes
    /// minutes; without an actions/cache step keyed on version + arch the
    /// release workflow would pay that cost on every tag push. The cache
    /// key also encodes the *build flavor* (static + no-zlib +
    /// no-libxml2) so a flag flip cannot silently reuse an incompatible
    /// cached archive. Bumping the trailing `-vN` is the escape hatch
    /// for "same flavor name, but the build is actually different now."
    #[test]
    fn windows_libmobi_build_is_cached() {
        let cache_block =
            "libmobi-${{ env.LIBMOBI_VERSION }}-windows-x64-static-nozlib-nolibxml2-v1";
        assert!(
            RELEASE_YML.contains(cache_block),
            "release.yml must cache the Windows libmobi build under a key \
             that pins both the source revision and the build flavor — \
             expected substring `{cache_block}` not found. Without the \
             flavor token, a future flip of BUILD_SHARED_LIBS or the \
             zlib/libxml2 options could reuse a stale cached archive of \
             the previous flavor and silently link the wrong thing."
        );
    }

    /// Both ci.yml and release.yml must build the *same* libmobi
    /// revision; otherwise PR CI could green-light a build against a
    /// libmobi the release pipeline never sees, masking MSVC-specific
    /// regressions until tag-push. The pin is a full commit SHA in
    /// both files, so this is a string equality check.
    #[test]
    fn libmobi_version_matches_between_ci_and_release() {
        let release_pin = parse_libmobi_version(RELEASE_YML);
        let ci_pin = parse_libmobi_version(CI_YML);
        assert_eq!(
            release_pin, ci_pin,
            "LIBMOBI_VERSION drift: release.yml pins `{release_pin}`, \
             ci.yml pins `{ci_pin}`. Both files must build the same \
             libmobi revision so PR CI exercises the exact source \
             release.yml will ship against."
        );
    }

    /// We pin a full commit SHA (40 lowercase hex chars) rather than
    /// a tag. Tags are mutable — a retargeted upstream tag can change
    /// the source CMake runs against on a cache miss. Catching this
    /// in a test means a future "let's switch back to a friendly tag"
    /// edit is rejected loudly instead of quietly weakening the pin.
    #[test]
    fn libmobi_version_is_a_full_commit_sha() {
        let pin = parse_libmobi_version(RELEASE_YML);
        assert_eq!(
            pin.len(),
            40,
            "LIBMOBI_VERSION must be a 40-char commit SHA, not a tag \
             or short SHA. Got `{pin}` (len {}). Tags are mutable; a \
             retargeted upstream tag would silently change what the \
             release runner builds.",
            pin.len()
        );
        assert!(
            pin.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "LIBMOBI_VERSION must be lowercase hex (a commit SHA). \
             Got `{pin}`."
        );
    }

    /// The libmobi build step must be Windows-only — it uses CMake +
    /// MSVC and would error or duplicate work if it ran on the
    /// Linux/macOS matrix entries.
    #[test]
    fn windows_libmobi_build_step_is_windows_only() {
        let needle = "Build libmobi (Windows)";
        assert!(
            RELEASE_YML.contains(needle),
            "release.yml must include a step named `{needle}` so the \
             Windows libmobi build path is auditable from the workflow at \
             a glance. The step itself gates on `matrix.platform == \
             'windows-latest'` to keep it off the Unix runners."
        );
        // The step's `if:` guard is the actual gating mechanism — a missing
        // guard would silently run the libmobi build on every matrix entry.
        let after_marker = RELEASE_YML.split_once(needle).expect("checked above").1;
        let next_300 = &after_marker[..after_marker.len().min(300)];
        assert!(
            next_300.contains("matrix.platform == 'windows-latest'"),
            "the `{needle}` step must be gated on \
             `matrix.platform == 'windows-latest'` — without the guard it \
             would also run on the Linux/macOS matrix entries and fail \
             (no MSVC toolchain). Got: {next_300}"
        );
    }

    /// arm64 macOS builds libmobi from source as a static archive and links
    /// it statically (via `LIBMOBI_STATIC`), so the shipped `.app` carries no
    /// runtime dependency on a Homebrew `libmobi.0.dylib`. Regression guard:
    /// reverting to `brew install libmobi` (dynamic) reintroduces the
    /// launch-time dyld "Library not loaded" crash for users without it.
    #[test]
    fn aarch64_macos_libmobi_build_is_static_and_gated() {
        let needle = "Build libmobi (macOS, static)";
        assert!(
            RELEASE_YML.contains(needle),
            "release.yml must include a step named `{needle}` that builds a \
             static libmobi from source for arm64 macOS — dynamic linkage to \
             Homebrew's dylib crashes the app on launch for users without \
             `brew install libmobi`."
        );
        let after = RELEASE_YML.split_once(needle).expect("checked above").1;
        let window = &after[..after.len().min(400)];
        assert!(
            window.contains("aarch64-apple-darwin"),
            "the `{needle}` step must gate on the aarch64 macOS matrix entry. \
             Got: {window}"
        );
        assert!(
            RELEASE_YML.contains("LIBMOBI_STATIC:"),
            "release.yml must export `LIBMOBI_STATIC` so folio-core/build.rs \
             links the from-source `libmobi.a` statically on macOS instead of \
             a dylib."
        );
    }
}
