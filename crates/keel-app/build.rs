use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

fn is_runtime_lib(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        ["dll", "dylib", "so"]
            .iter()
            .any(|x| e.eq_ignore_ascii_case(x))
    })
}

fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)?.flatten() {
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if src.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Icon + manifest (long paths, per-monitor DPI v2) for keel.exe. build.rs runs on the
/// host, so the target OS comes from Cargo, not `cfg!(windows)`.
fn windows_resources() {
    for f in ["keel.rc", "keel.exe.manifest", "../../assets/keel.ico"] {
        println!("cargo:rerun-if-changed={f}");
    }
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    if let Err(e) = embed_resource::compile("keel.rc", embed_resource::NONE).manifest_optional() {
        println!("cargo:warning=keel.exe built without its icon and manifest ({e})");
    }
}

fn main() {
    windows_resources();
    let (Some(manifest_dir), Some(out_dir)) = (
        env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from),
        env::var_os("OUT_DIR").map(PathBuf::from),
    ) else {
        println!("cargo:warning=CARGO_MANIFEST_DIR/OUT_DIR not set; skipping runtime library copy");
        return;
    };
    let Some(workspace_root) = manifest_dir.ancestors().nth(2) else {
        println!("cargo:warning=unexpected crate layout; skipping runtime library copy");
        return;
    };
    let deps_dir = workspace_root.join("target").join("deps");
    println!("cargo:rerun-if-changed={}", deps_dir.display());

    // OUT_DIR is target/<profile>/build/<package>/out; the binary lives in target/<profile>.
    let Some(exe_dir) = out_dir.ancestors().nth(3) else {
        println!("cargo:warning=cannot derive binary directory from OUT_DIR; skipping runtime library copy");
        return;
    };

    let entries = match fs::read_dir(&deps_dir) {
        Ok(entries) => entries,
        Err(error) => {
            println!(
                "cargo:warning=runtime dependency directory {} is unavailable ({error}); run scripts/fetch-deps.ps1 or scripts/fetch-deps.sh",
                deps_dir.display()
            );
            return;
        }
    };

    let mut copied = 0;
    for entry in entries.flatten() {
        let src = entry.path();
        if !src.is_file() || !is_runtime_lib(&src) {
            continue;
        }
        let dst = exe_dir.join(entry.file_name());
        match fs::copy(&src, &dst) {
            Ok(_) => copied += 1,
            Err(error) => println!(
                "cargo:warning=could not copy {} to {} ({error}); it may be locked",
                src.display(),
                dst.display()
            ),
        }
    }
    if copied == 0 {
        println!(
            "cargo:warning=no .dll/.dylib/.so found in {}; run scripts/fetch-deps.ps1 or scripts/fetch-deps.sh",
            deps_dir.display()
        );
    }

    let licenses = deps_dir.join("licenses");
    if licenses.is_dir() {
        if let Err(error) = copy_dir(&licenses, &exe_dir.join("licenses")) {
            println!("cargo:warning=could not copy licenses ({error})");
        }
    }
}
