//! Build script for folio-core.
//!
//! When the `mobi` feature is enabled (default), we locate libmobi via
//! pkg-config, tell cargo how to link against it, and generate Rust FFI
//! bindings from `mobi.h` into `$OUT_DIR/libmobi_bindings.rs`.

use std::env;
use std::path::PathBuf;

fn main() {
    if env::var_os("CARGO_FEATURE_MOBI").is_some() {
        build_libmobi_bindings();
    }
}

fn build_libmobi_bindings() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=LIBMOBI_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=LIBMOBI_LIB_DIR");
    println!("cargo:rerun-if-env-changed=LIBMOBI_STATIC");

    let (include_paths, link_dir) = resolve_libmobi_paths();

    // Static linkage bakes libmobi into the binary so the shipped
    // artifact is self-contained — no `mobi.dll` / `libmobi.dylib`
    // next to it that the OS loader (or an absolute Homebrew path)
    // must resolve at runtime. We link statically when:
    //   * Windows — always (release ships a single self-contained
    //     `carrel.exe`), or
    //   * `LIBMOBI_STATIC` is set — the macOS release build points this
    //     at a from-source static `libmobi.a` so end users no longer
    //     need `brew install libmobi` (without it the app crashed on
    //     launch with a dyld "Library not loaded" error).
    // Otherwise (local dev, Linux, CI tests) keep dynamic linkage so
    // libmobi can come from the distro package manager (apt, brew).
    let link_kind = if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        || env::var_os("LIBMOBI_STATIC").is_some()
    {
        "static"
    } else {
        "dylib"
    };
    println!("cargo:rustc-link-lib={link_kind}=mobi");
    if let Some(dir) = link_dir {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }

    let mut builder = bindgen::Builder::default()
        .header_contents("wrapper.h", "#include <mobi.h>")
        .allowlist_function("mobi_.*")
        .allowlist_type("MOBI.*")
        .allowlist_var("MOBI_.*")
        .derive_debug(true)
        .derive_default(true)
        .generate_comments(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));

    for path in &include_paths {
        builder = builder.clang_arg(format!("-I{}", path.display()));
    }

    let bindings = builder
        .generate()
        .expect("failed to generate libmobi bindings — is libmobi installed?");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("libmobi_bindings.rs");
    bindings
        .write_to_file(&out_path)
        .expect("failed to write libmobi_bindings.rs");
}

fn resolve_libmobi_paths() -> (Vec<PathBuf>, Option<PathBuf>) {
    if let Ok(lib) = pkg_config::Config::new().probe("libmobi") {
        let link_dir = lib.link_paths.into_iter().next();
        return (lib.include_paths, link_dir);
    }

    // GitHub Actions can pass empty strings for these env vars on
    // platforms that don't need them (the release workflow uses a
    // ternary expression to set them only on Windows). Treat empty
    // values as "unset" so we don't end up emitting a useless `-I`
    // or `-L` to the bindgen / linker invocation.
    let include = env::var("LIBMOBI_INCLUDE_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let lib_dir = env::var("LIBMOBI_LIB_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    (include.into_iter().collect(), lib_dir)
}
