//! Embeds the web client bundle (`crates/keel-web/dist`, made by `scripts/build-web.*`)
//! into keel-daemon for `--web`. Without a bundle the daemon serves a page that says how
//! to build one.

use std::path::Path;

fn main() {
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
    files.sort();
    let code = format!(
        "/// The web client's files by name (empty: not built).\n\
         pub(crate) static FILES: &[(&str, &[u8])] = &[\n{}];\n",
        files.concat()
    );
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("web_bundle.rs");
    std::fs::write(out, code).expect("write web_bundle.rs");
}
