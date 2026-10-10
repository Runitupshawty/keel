//! Embeds the web client bundle (`crates/keel-web/dist`, made by `scripts/build-web.*`)
//! into keel-daemon for `--web`. Without a bundle the daemon serves a page that says how
//! to build one (and the PWA manifest, service worker and icons from `keel-web/static`).
//! With the `winfsp` mount backend it also delay-loads WinFsp's DLL.

use std::path::Path;

fn main() {
    delay_load_winfsp();
    let web = Path::new(env!("CARGO_MANIFEST_DIR")).join("../keel-web");
    let dist = web.join("dist");
    // The crate folder always exists, so this does not rerun every build; a new dist
    // changes it.
    println!("cargo:rerun-if-changed={}", web.display());
    if dist.is_dir() {
        println!("cargo:rerun-if-changed={}", dist.display());
    }
    let mut files = Vec::new();
    if dist.join("index.html").is_file() {
        for entry in std::fs::read_dir(&dist)
            .expect("read keel-web/dist")
            .flatten()
        {
            let path = entry.path();
            if path.is_file() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = path.canonicalize().expect("canonical bundle path");
                files.push(format!("    ({name:?}, include_bytes!({path:?})),\n"));
            }
        }
    }
    // The PWA files the daemon answers for even without a built client (the manifest names
    // the share target the daemon serves; tests check them).
    for name in [
        "manifest.webmanifest",
        "sw.js",
        "icon-192.png",
        "icon-512.png",
    ] {
        let path = web.join("static").join(name);
        let built = files
            .iter()
            .any(|f| f.starts_with(&format!("    ({name:?},")));
        if !built && path.is_file() {
            println!("cargo:rerun-if-changed={}", path.display());
            let path = path.canonicalize().expect("canonical static path");
            files.push(format!("    ({name:?}, include_bytes!({path:?})),\n"));
        }
    }
    files.sort();
    let code = format!(
        "/// The web client's files by name (empty: not built).\n\
         pub(crate) static FILES: &[(&str, &[u8])] = &[\n{}];\n",
        files.concat()
    );
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("web_bundle.rs");
    std::fs::write(out, code).expect("write web_bundle.rs");
}

fn delay_load_winfsp() {
    // With the `winfsp` mount backend, WinFsp's DLL is delay-loaded: keel-mount loads it
    // from the WinFsp install folder when the first mount is made, so the daemon still
    // starts where WinFsp is not installed.
    if std::env::var_os("CARGO_FEATURE_WINFSP").is_some()
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        let dll = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
            Ok("x86") => "winfsp-x86.dll",
            Ok("aarch64") => "winfsp-a64.dll",
            _ => "winfsp-x64.dll",
        };
        println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
        println!("cargo:rustc-link-lib=dylib=delayimp");
    }
}
