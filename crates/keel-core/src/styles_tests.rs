use super::*;
use crate::index::tests::{id_of, walk, write};
use crate::library::tests::folder;
use crate::{ChangeEvent, Indexer};
use keel_vfs::VPath;

fn style(color: Option<&str>, icon: Option<&str>) -> FolderStyle {
    FolderStyle {
        color: color.map(str::to_owned),
        icon: icon.map(str::to_owned),
    }
}

#[test]
fn colors_and_icons_are_validated() {
    for ok in ["#e5484d", "#FFFFFF", "#000000"] {
        assert!(valid_color(ok), "{ok}");
    }
    for bad in [
        "e5484d", "#e5484", "#e5484dd", "#gggggg", "red", "", "#e5 84d",
    ] {
        assert!(!valid_color(bad), "{bad}");
    }
    // One emoji, however many code points: skin tones, ZWJ families, flags, keycaps.
    for ok in ["📁", "⭐", "❤️", "👍🏽", "👨‍👩‍👧", "🇳🇴", "1️⃣", "©️"]
    {
        assert!(valid_emoji(ok), "{ok}");
        assert!(valid_icon(ok), "{ok}");
    }
    // Not emoji, or more than one grapheme.
    for bad in [
        "a",
        "ab",
        "1",
        "#",
        "é",
        "→",
        "📁📁",
        "📁 ",
        " 📁",
        "📁a",
        "",
        "\u{202e}📁",
    ] {
        assert!(!valid_emoji(bad), "{bad:?}");
        assert!(!valid_icon(bad), "{bad:?}");
    }
    for ok in [
        "theme:src",
        "theme:node_modules",
        "theme:.github",
        "theme:c++",
    ] {
        assert!(valid_theme_icon(ok), "{ok}");
    }
    for bad in [
        "theme:",
        "theme: src",
        "theme:a/b",
        "theme:a\\b",
        "theme:\u{7}",
        "src",
    ] {
        assert!(!valid_theme_icon(bad), "{bad:?}");
    }
    assert!(!valid_theme_icon(&format!("theme:{}", "a".repeat(65))));
    assert!(style(Some("#e5484d"), Some("📁")).check().is_ok());
    assert!(style(Some("red"), None).check().is_err());
    assert!(style(None, Some("📁📁")).check().is_err());
    assert!(style(None, Some("folder")).check().is_err());
    let s = style(Some("#3B82F6"), Some("theme:src"));
    assert_eq!(s.rgb(), Some([0x3b, 0x82, 0xf6]));
    assert_eq!(s.theme_icon(), Some("src"));
    assert_eq!(s.emoji(), None);
    assert_eq!(style(None, Some("📁")).emoji(), Some("📁"));
}

struct Fixture {
    files: tempfile::TempDir,
    _data: tempfile::TempDir,
    lib: Library,
    src: Arc<Source>,
}

fn fixture() -> Fixture {
    let files = tempfile::tempdir().unwrap();
    for name in [
        "Work/plan.txt",
        "Work/old/notes.txt",
        "Photos/a.jpg",
        "top.txt",
    ] {
        write(&files.path().join(name), name);
    }
    let data = tempfile::tempdir().unwrap();
    let lib = Library::open(data.path(), "t").unwrap();
    let src = lib
        .source(&lib.add_source(folder("F", files.path())).unwrap())
        .unwrap();
    walk(&src, &lib.router()).unwrap();
    Fixture {
        files,
        _data: data,
        lib,
        src,
    }
}

fn rec(src: &Source, rel: &str) -> RecordRef {
    RecordRef {
        source: src.id.clone(),
        id: id_of(src, rel).unwrap(),
    }
}

fn styled(lib: &Library) -> Vec<(String, FolderStyle)> {
    lib.folder_styles()
        .unwrap()
        .into_iter()
        .map(|(h, s)| (h.name, s))
        .collect()
}

#[test]
fn set_read_and_clear() {
    let f = fixture();
    let (lib, work) = (&f.lib, rec(&f.src, "Work"));
    assert_eq!(lib.folder_style(&work).unwrap(), None);
    let red = style(Some("#e5484d"), None);
    lib.set_folder_style(&work, &red).unwrap();
    assert_eq!(lib.folder_style(&work).unwrap(), Some(red.clone()));
    let both = style(Some("#e5484d"), Some("📁"));
    lib.set_folder_style(&work, &both).unwrap();
    lib.set_folder_style(&rec(&f.src, ""), &style(None, Some("theme:src")))
        .unwrap();
    // By path: the source's root first.
    let all = styled(lib);
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].1, style(None, Some("theme:src")));
    assert_eq!(all[1], ("Work".into(), both.clone()));
    let hits = lib.folder_styles().unwrap();
    assert_eq!(hits[1].0.path, VPath::local(f.files.path().join("Work")));
    assert!(hits[1].0.is_dir);
    // Files and bad styles are refused; nothing changes.
    let plan = rec(&f.src, "Work/plan.txt");
    assert!(lib.set_folder_style(&plan, &red).is_err());
    assert!(lib
        .set_folder_style(&work, &style(Some("#e5484"), None))
        .is_err());
    assert_eq!(lib.folder_style(&work).unwrap(), Some(both));
    // An empty style clears it.
    lib.set_folder_style(&work, &FolderStyle::default())
        .unwrap();
    assert_eq!(lib.folder_style(&work).unwrap(), None);
    assert_eq!(styled(lib).len(), 1);
    // A record that is gone, or a source that is not in the library.
    let gone = RecordRef {
        source: f.src.id.clone(),
        id: 999_999,
    };
    assert!(lib.set_folder_style(&gone, &red).is_err());
    let elsewhere = RecordRef {
        source: crate::SourceId("nope".into()),
        id: work.id,
    };
    assert!(lib.folder_style(&elsewhere).is_err());
}

#[test]
fn a_moved_or_renamed_folder_keeps_its_style() {
    let f = fixture();
    let (lib, src, root) = (&f.lib, &f.src, f.files.path());
    let blue = style(Some("#3b82f6"), Some("⭐"));
    let old = rec(src, "Work/old");
    lib.set_folder_style(&rec(src, "Work"), &blue).unwrap();
    lib.set_folder_style(&old, &style(None, Some("📦")))
        .unwrap();
    let changed = |p: &std::path::Path| {
        Indexer::apply_change(src, ChangeEvent::Changed(VPath::local(p))).unwrap();
    };
    // Renamed: the index records the move (the record keeps its id), and the style follows.
    std::fs::rename(root.join("Work"), root.join("Jobs")).unwrap();
    changed(&root.join("Jobs"));
    changed(&root.join("Work"));
    assert_eq!(id_of(src, "Work"), None);
    assert_eq!(lib.folder_style(&rec(src, "Jobs")).unwrap(), Some(blue));
    // The folder inside moved with it, and moves again on its own.
    assert_eq!(rec(src, "Jobs/old"), old);
    std::fs::rename(root.join("Jobs/old"), root.join("Photos/old")).unwrap();
    changed(&root.join("Photos/old"));
    changed(&root.join("Jobs/old"));
    assert_eq!(rec(src, "Photos/old"), old);
    let names: Vec<String> = lib
        .folder_styles()
        .unwrap()
        .into_iter()
        .map(|(h, _)| h.path.display())
        .collect();
    assert_eq!(
        names,
        [
            VPath::local(root.join("Jobs")).display(),
            VPath::local(root.join("Photos/old")).display()
        ]
    );
    // A folder that leaves the index drops its style.
    std::fs::remove_dir_all(root.join("Photos/old")).unwrap();
    Indexer::apply_change(
        src,
        ChangeEvent::Removed(VPath::local(root.join("Photos/old"))),
    )
    .unwrap();
    assert_eq!(styled(lib).len(), 1);
    let left: i64 = src
        .store
        .get()
        .unwrap()
        .query_row("SELECT count(*) FROM folder_style", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 1);
}

#[test]
fn a_removed_source_drops_its_styles() {
    let f = fixture();
    let lib = &f.lib;
    lib.set_folder_style(&rec(&f.src, "Photos"), &style(Some("#30a46c"), None))
        .unwrap();
    assert_eq!(styled(lib).len(), 1);
    lib.remove_source(&f.src.id, true).unwrap();
    assert!(styled(lib).is_empty());
    // Added again: a fresh store, no style.
    let src = lib
        .source(&lib.add_source(folder("F", f.files.path())).unwrap())
        .unwrap();
    walk(&src, &lib.router()).unwrap();
    assert!(styled(lib).is_empty());
    assert_eq!(lib.folder_style(&rec(&src, "Photos")).unwrap(), None);
}

#[test]
fn styles_survive_closing_and_opening_the_library() {
    let f = fixture();
    let work = rec(&f.src, "Work");
    let red = style(Some("#e5484d"), Some("🔥"));
    f.lib.set_folder_style(&work, &red).unwrap();
    assert!(f.lib.close(std::time::Duration::from_secs(10)));
    drop(f.lib);
    let lib = Library::open(f._data.path(), "t").unwrap();
    assert_eq!(lib.folder_style(&work).unwrap(), Some(red));
    lib.close(std::time::Duration::from_secs(10));
}
