//! The desktop app's parsers of outside bytes under the in-tree fuzz runs (docs/fuzzing.md;
//! keel-app is a binary, so these have no libFuzzer targets).

use keel_vfs::fuzz_harness::{run, Capped};
use std::path::PathBuf;

/// The whole keel-app test binary's allocator, as in the other crates' fuzz tests.
#[global_allocator]
static ALLOC: Capped = Capped;

#[test]
fn fuzz_clipboard() {
    run("clipboard", |data| {
        use crate::x11_clipboard::{parse_gnome, uri_paths};
        let _ = parse_gnome(data);
        let text = String::from_utf8_lossy(data);
        let _ = uri_paths(text.lines());
        let _ = uri_paths(text.split("\r\n"));
    });
}

#[test]
fn fuzz_desktop_entry() {
    run("desktop_entry", |data| {
        use crate::platform::{
            apps_in_list, desktop_exec, desktop_name, exec_argvs, open_with_argvs, Os,
        };
        let text = String::from_utf8_lossy(data);
        let _ = desktop_name(&text);
        let _ = apps_in_list(&text, "text/plain");
        let paths = [PathBuf::from("/a b/c.txt"), PathBuf::from("/d%e")];
        let exec = desktop_exec(&text).unwrap_or_else(|| text.lines().next().unwrap_or("").into());
        let _ = exec_argvs(&exec, &paths);
        let _ = open_with_argvs(Os::Other, "app.desktop", &paths, Some(&exec));
    });
}

#[test]
fn fuzz_icon_svg() {
    run("icon_svg", |data| {
        let _ = crate::icon_theme::svg_unsafe(data);
    });
}

#[test]
fn fuzz_vsix() {
    run("vsix", |data| {
        use crate::icon_theme as theme;
        let Ok(vsix) = theme::inspect("pub.theme", data.to_vec()) else {
            return;
        };
        let root = tempfile::tempdir().unwrap();
        if theme::install_capped(&vsix, root.path(), 1 << 20, 64).is_ok() {
            if let Ok(loaded) = theme::load(&root.path().join("pub.theme"), 1) {
                let _ = loaded.file("a.rs", false);
                let _ = loaded.folder(Some("src"), true, true);
            }
        }
    });
}

/// Task 47: VS Code color theme JSON (comments, trailing commas, `include`), its mapping
/// onto Keel's colors, and whole packages.
#[test]
fn fuzz_color_theme() {
    run("color_theme", |data| {
        use crate::color_theme as ct;
        let _ = ct::strip_jsonc(&String::from_utf8_lossy(data));
        // The input as a theme file: any include it names reads the input again (a cycle
        // or a chain), or a small sibling.
        let colors = ct::theme_colors(
            &mut |p| {
                Ok(match p {
                    "extension/sibling.json" => br##"{"colors":{"foreground":"#fff"}}"##.to_vec(),
                    _ => data.to_vec(),
                })
            },
            "extension/themes/t.json",
        );
        if let Ok(colors) = colors {
            let _ = ct::map("t", &colors, data.first() == Some(&b'l'));
        }
        let _ = ct::inspect("pub.theme", data.to_vec());
    });
}
