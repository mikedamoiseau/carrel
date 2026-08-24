//! Compile-time checks on the split Tauri bundle configuration.
//!
//! `tauri.conf.json` is the base config used by every build. Linux-only
//! runtime dependencies (`libmobi0` for `.deb`, `libmobi` for `.rpm`) are
//! pulled out into `tauri.linux.mobi.conf.json` and merged in via
//! `--config` only when the release workflow builds with `--features mobi`.
//!
//! These tests guard the split so a future edit can't silently
//! put the depends back into the base config (which would ship them in
//! `--no-default-features` Linux builds that don't actually link libmobi)
//! or strip them from the overlay (which would un-declare the dependency
//! on the shipping Linux path).

#[cfg(test)]
mod tests {
    use serde_json::Value;

    const BASE: &str = include_str!("../tauri.conf.json");
    const OVERLAY_MOBI_LINUX: &str = include_str!("../tauri.linux.mobi.conf.json");

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).expect("valid JSON")
    }

    #[test]
    fn base_config_has_no_linux_libmobi_depends() {
        // The base config must not carry Linux libmobi depends — those are
        // conditional on the `mobi` feature and live in the overlay so
        // non-mobi Linux builds produce honest package metadata.
        let base = parse(BASE);
        let linux = base.pointer("/bundle/linux");
        // `/bundle/linux` may exist for other reasons, but `.deb.depends`
        // and `.rpm.depends` must not be in the base.
        if let Some(linux) = linux {
            assert!(
                linux.pointer("/deb/depends").is_none(),
                "tauri.conf.json must not carry bundle.linux.deb.depends; \
                 those belong in tauri.linux.mobi.conf.json"
            );
            assert!(
                linux.pointer("/rpm/depends").is_none(),
                "tauri.conf.json must not carry bundle.linux.rpm.depends; \
                 those belong in tauri.linux.mobi.conf.json"
            );
        }
    }

    #[test]
    fn overlay_declares_libmobi_deb_depends() {
        let overlay = parse(OVERLAY_MOBI_LINUX);
        let depends = overlay
            .pointer("/bundle/linux/deb/depends")
            .expect("tauri.linux.mobi.conf.json must declare bundle.linux.deb.depends");
        let arr = depends.as_array().expect("depends must be an array");
        assert!(
            arr.iter().any(|v| v.as_str() == Some("libmobi0")),
            "Debian depends must include `libmobi0`, got {arr:?}"
        );
    }

    #[test]
    fn overlay_declares_libmobi_rpm_depends() {
        let overlay = parse(OVERLAY_MOBI_LINUX);
        let depends = overlay
            .pointer("/bundle/linux/rpm/depends")
            .expect("tauri.linux.mobi.conf.json must declare bundle.linux.rpm.depends");
        let arr = depends.as_array().expect("depends must be an array");
        assert!(
            arr.iter().any(|v| v.as_str() == Some("libmobi")),
            "RPM depends must include `libmobi`, got {arr:?}"
        );
    }

    /// The WiX upgrade code must stay pinned to the value every shipped Folio
    /// MSI carries. Tauri derives it from `uuid5(DNS, "{productName}.exe.app.x64")`
    /// when unset, so leaving it unset makes any `productName` change (like the
    /// Folio → Carrel rename) silently produce MSIs that install *side-by-side*
    /// with the user's existing install instead of upgrading it. `MajorUpgrade`
    /// matches on this code alone, so pinning it is what keeps in-place upgrades
    /// working across the rename. See CLAUDE.md, "Persistence-boundary identifiers".
    #[test]
    fn wix_upgrade_code_is_pinned_to_the_carrel_era_value() {
        const FOLIO_ERA_UPGRADE_CODE: &str = "21c2cdba-327a-5023-94aa-a2fbf307774c";
        let base = parse(BASE);
        let code = base
            .pointer("/bundle/windows/wix/upgradeCode")
            .and_then(Value::as_str)
            .expect(
                "tauri.conf.json must pin bundle.windows.wix.upgradeCode; without it \
                 Tauri derives it from productName and the MSI stops upgrading in place",
            );
        assert_eq!(
            code, FOLIO_ERA_UPGRADE_CODE,
            "the WiX upgrade code must never change — it is what links a new MSI to \
             the user's existing install"
        );
    }

    /// The main window must *open* wide enough for the library toolbar in the
    /// widest locale, and must not be resizable below what the narrowest one
    /// needs. Measured against the built CSS with the app's own fonts, the
    /// toolbar needs 928 px in English and 1090 px in French for
    /// `+ Add books` to be fully visible — and that is already with the
    /// search field collapsed to its magnifier. The old 800x600 default
    /// clipped the button off the right edge on every launch (there is no
    /// window-state plugin, so the config default *is* every launch).
    ///
    /// The floor is deliberately set to the English figure rather than the
    /// French one: a floor above 1024 would exceed the width of a 1024-px
    /// display entirely, leaving such a user unable to resize the window to
    /// fit their screen at all. The default covers every locale; the floor
    /// only stops a resize from hiding the button outright.
    #[test]
    fn main_window_opens_and_stays_wide_enough_for_the_library_toolbar() {
        /// Narrowest locale (English) toolbar requirement, rounded up.
        const TOOLBAR_MIN_WIDTH: u64 = 960;
        /// Widest locale (French) toolbar requirement, rounded up.
        const TOOLBAR_DEFAULT_WIDTH: u64 = 1090;
        let base = parse(BASE);
        let window = base
            .pointer("/app/windows/0")
            .expect("tauri.conf.json must configure a main window");

        let num = |key: &str| -> u64 {
            window
                .get(key)
                .and_then(Value::as_u64)
                .unwrap_or_else(|| panic!("main window must set `{key}`"))
        };

        let (min_width, min_height) = (num("minWidth"), num("minHeight"));
        assert!(
            min_width >= TOOLBAR_MIN_WIDTH,
            "minWidth must be at least {TOOLBAR_MIN_WIDTH} so the library toolbar's \
             `+ Add books` button cannot be resized off-screen, got {min_width}"
        );
        assert!(
            min_height >= 600,
            "minHeight must be at least 600, got {min_height}"
        );

        let (width, height) = (num("width"), num("height"));
        assert!(
            width >= TOOLBAR_DEFAULT_WIDTH,
            "the default width must be at least {TOOLBAR_DEFAULT_WIDTH} so the \
             window opens with `+ Add books` visible in every locale, got {width}"
        );
        assert!(
            width >= min_width && height >= min_height,
            "the default size ({width}x{height}) must not be smaller than the \
             minimum ({min_width}x{min_height})"
        );
    }

    #[test]
    fn overlay_is_schema_valid_for_tauri_merge() {
        // The overlay must have the `$schema` key so IDE tooling picks it up
        // and it parses under the Tauri v2 config schema — catches typos like
        // `bundles` vs `bundle`.
        let overlay = parse(OVERLAY_MOBI_LINUX);
        assert!(
            overlay.get("$schema").is_some(),
            "overlay must include $schema"
        );
        assert!(
            overlay.get("bundle").is_some(),
            "overlay root must be {{ bundle: ... }}"
        );
    }
}
