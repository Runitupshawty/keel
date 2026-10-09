use std::{env, fs, path::PathBuf};

const EXPECTED_DLLS: [&str; 2] = ["Everything64.dll", "pdfium.dll"];

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by Cargo"),
    );
    let workspace_root = manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .expect("keel-app must be inside the workspace crates directory");
    let deps_dir = workspace_root.join("target").join("deps");
    println!("cargo:rerun-if-changed={}", deps_dir.display());

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR must be set by Cargo"));
    let exe_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("OUT_DIR must be under target/<profile>/build/<package>/out");

    let entries = match fs::read_dir(&deps_dir) {
        Ok(entries) => entries,
        Err(error) => {
            println!(
                "cargo:warning=runtime dependency directory {} is unavailable: {error}",
                deps_dir.display()
            );
            for expected in EXPECTED_DLLS {
                println!("cargo:warning=missing runtime dependency {expected}");
            }
            return;
        }
    };

    let mut copied = Vec::new();
    for entry in entries.flatten() {
        let source = entry.path();
        let is_dll = source
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("dll"));
        if !is_dll {
            continue;
        }

        let destination = exe_dir.join(entry.file_name());
        fs::copy(&source, &destination).unwrap_or_else(|error| {
            panic!(
                "failed to copy {} to {}: {error}",
                source.display(),
                destination.display()
            )
        });
        copied.push(entry.file_name());
    }

    for expected in EXPECTED_DLLS {
        let was_copied = copied.iter().any(|name| {
            name.to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(expected))
        });
        if !was_copied {
            println!(
                "cargo:warning=missing runtime dependency {expected}; run scripts/fetch-deps.ps1"
            );
        }
    }
}
