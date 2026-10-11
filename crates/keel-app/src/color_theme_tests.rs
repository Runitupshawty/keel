use super::*;
use std::collections::HashMap;

/// A hand-written package: `themes/base.json` <- `themes/mid.json` <- `themes/night.json`
/// (comments, trailing commas, an alpha color, a missing key), and a light theme.
fn files() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        (
            "extension/themes/base.json",
            r##"{
                // the shared base
                "colors": {
                    "editor.background": "#101820",
                    "sideBar.background": "#18202a",
                    "descriptionForeground": "#8899aa",
                    "focusBorder": "#3399ff",
                },
            }"##,
        ),
        (
            "extension/themes/mid.json",
            r##"{ "include": "./base.json", /* block */ "colors": { "foreground": "#e0e0e0" } }"##,
        ),
        (
            "extension/themes/night.json",
            r##"{
                "name": "Night", "include": "mid.json",
                "colors": {
                    "list.activeSelectionBackground": "#ffffff40", // alpha
                    "editor.background": "#101820ff",
                    "url": "http://example.invalid/a//b",
                },
            }"##,
        ),
        (
            "extension/themes/day.json",
            r##"{ "colors": { "editor.background": "#fafafa", "foreground": "#202020" } }"##,
        ),
        (
            "extension/themes/faint.json",
            r##"{ "colors": { "editor.background": "#202020", "foreground": "#303030",
                 "descriptionForeground": "#282828" } }"##,
        ),
        (
            "extension/themes/loop-a.json",
            r#"{ "include": "loop-b.json" }"#,
        ),
        (
            "extension/themes/loop-b.json",
            r#"{ "include": "loop-a.json" }"#,
        ),
        ("extension/themes/d0.json", r#"{ "include": "d1.json" }"#),
        ("extension/themes/d1.json", r#"{ "include": "d2.json" }"#),
        ("extension/themes/d2.json", r#"{ "include": "d3.json" }"#),
        ("extension/themes/d3.json", r#"{ "include": "d4.json" }"#),
        ("extension/themes/d4.json", r#"{ "include": "d5.json" }"#),
        ("extension/themes/d5.json", r#"{ "colors": {} }"#),
        (
            "extension/themes/out.json",
            r#"{ "include": "../../../x.json" }"#,
        ),
    ])
}

fn colors(path: &str) -> Result<HashMap<String, String>> {
    let files = files();
    theme_colors(
        &mut |p| {
            files
                .get(p)
                .map(|t| t.as_bytes().to_vec())
                .with_context(|| format!("{p} not in the package"))
        },
        path,
    )
}

#[test]
fn jsonc_comments_and_trailing_commas_go_but_strings_stay() {
    let text = "{ \"a\": \"x // y /* z */\", // gone\n \"b\": [1, 2,], /* gone */ }";
    let v: serde_json::Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
    assert_eq!(v["a"], "x // y /* z */");
    assert_eq!(v["b"], serde_json::json!([1, 2]));
    let escaped = r#"{"q": "a \" , ]"}"#;
    assert_eq!(strip_jsonc(escaped), escaped);
}

#[test]
fn colors_parse_in_every_form() {
    assert_eq!(parse_color("#fff"), Some([255, 255, 255, 255]));
    assert_eq!(parse_color("#0f08"), Some([0, 255, 0, 136]));
    assert_eq!(parse_color("#102030"), Some([16, 32, 48, 255]));
    assert_eq!(parse_color("#10203040"), Some([16, 32, 48, 64]));
    for bad in ["fff", "#ff", "#ggg", "#1234567", "#\u{e9}\u{e9}\u{e9}", ""] {
        assert_eq!(parse_color(bad), None, "{bad}");
    }
}

#[test]
fn an_include_chain_maps_onto_keels_colors() {
    let c = colors("extension/themes/night.json").unwrap();
    let t = map("Night", &c, false);
    assert!(t.dark);
    assert_eq!(t.bg, [0x10, 0x18, 0x20], "own value over the included one");
    assert_eq!(
        t.panel,
        [0x18, 0x20, 0x2a],
        "sideBar.background (no section header)"
    );
    assert_eq!(t.text, [0xe0, 0xe0, 0xe0], "from the middle file");
    assert_eq!(t.accent, [0x33, 0x99, 0xff]);
    // #ffffff at 64/255 over the background.
    let mix = |b: u8| ((255 * 64 + b as u32 * 191) / 255) as u8;
    assert_eq!(
        t.selection,
        [mix(0x10), mix(0x18), mix(0x20)],
        "alpha flattened"
    );
    assert!(!t.adjusted, "{t:?}");
}

#[test]
fn missing_keys_fall_back_and_light_themes_are_light() {
    let c = colors("extension/themes/day.json").unwrap();
    let t = map("Day", &c, true);
    let light = Theme::load("light");
    assert!(!t.dark);
    assert_eq!((t.bg, t.text), ([0xfa; 3], [0x20; 3]));
    assert_eq!(
        (t.panel, t.accent, t.selection),
        (light.panel, light.accent, light.selection)
    );
}

#[test]
fn cycles_too_deep_includes_and_escapes_are_refused() {
    let e = format!("{:#}", colors("extension/themes/loop-a.json").unwrap_err());
    assert!(e.contains("includes itself"), "{e}");
    let e = format!("{:#}", colors("extension/themes/d0.json").unwrap_err());
    assert!(e.contains("more than 4 deep"), "{e}");
    assert!(
        colors("extension/themes/d1.json").is_ok(),
        "4 levels are fine"
    );
    let e = format!("{:#}", colors("extension/themes/out.json").unwrap_err());
    assert!(e.contains("outside the package"), "{e}");
}

#[test]
fn low_contrast_text_is_adjusted_to_four_and_a_half() {
    let c = colors("extension/themes/faint.json").unwrap();
    let t = map("Faint", &c, false);
    assert!(t.adjusted);
    for fg in [t.text, t.muted, t.accent] {
        for bg in [t.bg, t.panel] {
            let r = ratio(rgb(fg), rgb(bg));
            assert!(r >= 4.5, "{fg:?} on {bg:?}: {r:.2}");
        }
    }
}

#[test]
fn install_writes_one_file_per_theme_and_remove_takes_them() {
    let dir = tempfile::tempdir().unwrap();
    let night = map(
        "Night",
        &colors("extension/themes/night.json").unwrap(),
        false,
    );
    let day = map("Day", &colors("extension/themes/day.json").unwrap(), true);
    let v = ColorVsix {
        id: "pub.themes".into(),
        label: "Themes".into(),
        license: String::new(),
        themes: vec![night.clone(), day],
    };
    let names = install(&v, dir.path()).unwrap();
    assert_eq!(names, ["pub.themes~0", "pub.themes~1"]);
    std::fs::write(dir.path().join("mine.toml"), "x").unwrap();
    assert_eq!(
        installed(dir.path()),
        [("pub.themes".into(), "Night".into())]
    );
    let back: Theme =
        toml::from_str(&std::fs::read_to_string(dir.path().join("pub.themes~0.toml")).unwrap())
            .unwrap();
    assert_eq!(
        (back.name.as_str(), back.bg, back.dark),
        ("Night", night.bg, true)
    );
    // Reinstalling a package with fewer themes leaves no stale file.
    let one = ColorVsix {
        themes: vec![night],
        ..v
    };
    install(&one, dir.path()).unwrap();
    assert!(!dir.path().join("pub.themes~1.toml").exists());
    remove(dir.path(), "pub.themes").unwrap();
    assert!(installed(dir.path()).is_empty());
    assert!(
        dir.path().join("mine.toml").exists(),
        "the user's own file stays"
    );
    assert!(remove(dir.path(), "../x").is_err());
}

/// Live: downloads a color theme package (`KEEL_COLOR_THEME`, default an MIT-licensed one)
/// and maps its themes; installs nothing, so no license is accepted.
#[test]
#[ignore = "live: downloads from the VS Code Marketplace"]
fn marketplace_live_inspect() {
    let id = std::env::var("KEEL_COLOR_THEME").unwrap_or("dracula-theme.theme-dracula".into());
    let bytes = icon_theme::download(&id, &mut |_, _| {}).unwrap();
    let v = inspect(&id, bytes).unwrap();
    for t in &v.themes {
        eprintln!("{}: {t:?}", v.id);
    }
    eprintln!("license: {}", v.license.lines().next().unwrap_or(""));
    assert!(!v.themes.is_empty());
    assert!(!v.license.is_empty());
}
