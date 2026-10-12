//! Icons: the active VS Code icon theme (`icon_theme`, set at runtime), else the built-in
//! Keel Finder theme when selected (`finder_icons`), else the built-in Material Icon Theme
//! SVGs (MIT) embedded in the binary.

use crate::finder_icons::{self as finder, Icon};
use crate::icon_theme::Loaded;
use egui::ImageSource;
use keel_vfs::{Entry, Kind, VPath};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

/// The installed theme in use; None = built-in.
static THEME: RwLock<Option<Arc<Loaded>>> = RwLock::new(None);
/// A light color theme is active (icon themes may have a `light` variant).
static LIGHT: AtomicBool = AtomicBool::new(false);
/// Task 68: the built-in Keel Finder theme is selected (and no installed theme is active).
static FINDER: AtomicBool = AtomicBool::new(false);

pub fn set_theme(theme: Option<Arc<Loaded>>) {
    *THEME.write().unwrap_or_else(|e| e.into_inner()) = theme;
}

/// Task 68: switches the Keel Finder icons on or off.
pub fn set_finder(on: bool) {
    FINDER.store(on, Ordering::Relaxed);
}

#[cfg(test)]
pub fn has_theme() -> bool {
    THEME.read().unwrap_or_else(|e| e.into_inner()).is_some()
}

pub fn set_light(light: bool) {
    LIGHT.store(light, Ordering::Relaxed);
}

fn themed(
    pick: impl FnOnce(&Loaded, bool) -> Option<ImageSource<'static>>,
) -> Option<ImageSource<'static>> {
    let theme = THEME.read().unwrap_or_else(|e| e.into_inner());
    pick(theme.as_deref()?, LIGHT.load(Ordering::Relaxed))
}

/// Some(light) while the Keel Finder theme is in use.
fn finder_on() -> Option<bool> {
    FINDER
        .load(Ordering::Relaxed)
        .then(|| LIGHT.load(Ordering::Relaxed))
}

/// `pick`'s Keel Finder icon at 16 px (`sized` redraws it for larger slots), while the
/// theme is in use.
fn finder_icon(pick: impl FnOnce() -> Option<Icon>) -> Option<ImageSource<'static>> {
    let light = finder_on()?;
    Some(finder::source(&pick()?, light, 16))
}

macro_rules! icon {
    ($name:literal) => {
        egui::include_image!(concat!("../../../assets/icons/default/", $name, ".svg"))
    };
}

pub fn folder() -> ImageSource<'static> {
    themed(|t, light| t.folder(None, false, light))
        .or_else(|| finder_icon(|| Some(finder::FOLDER)))
        .unwrap_or(icon!("folder"))
}

/// Task 56: the icon theme's icon for folders named `name` (`theme:<name>` folder icons);
/// None without a theme or when it has none. Keel Finder has its special folders
/// (`theme:downloads`, `theme:music`, ...).
pub fn named_folder(name: &str) -> Option<ImageSource<'static>> {
    themed(|t, light| t.named_folder(name, light)).or_else(|| {
        finder_icon(|| {
            Some(Icon::Folder {
                glyph: Some(finder::Special::from_name(name)?),
                open: false,
                color: None,
            })
        })
    })
}

/// The folder names the icon theme has an icon for (empty without a theme).
pub fn theme_folder_names() -> Vec<String> {
    let theme = THEME.read().unwrap_or_else(|e| e.into_inner());
    match theme.as_deref() {
        Some(t) => t.folder_names(LIGHT.load(Ordering::Relaxed)),
        None if finder_on().is_some() => {
            let mut names: Vec<String> = (finder::Special::ALL.iter())
                .map(|s| s.name().to_owned())
                .collect();
            names.sort();
            names
        }
        None => Vec::new(),
    }
}

pub fn folder_open() -> ImageSource<'static> {
    themed(|t, light| t.folder(None, true, light))
        .or_else(|| {
            finder_icon(|| {
                Some(Icon::Folder {
                    glyph: None,
                    open: true,
                    color: None,
                })
            })
        })
        .unwrap_or(icon!("folder-open"))
}

/// The icon of a place in the sidebar (a user folder, a drive, the Trash) drawn `px`
/// physical pixels wide: Keel Finder's own icon for it, else the (open) folder icon.
pub fn place(path: &VPath, open: bool, px: f32) -> ImageSource<'static> {
    match finder_on() {
        Some(light) => finder::source(&finder::folder(path, open), light, finder::bucket(px)),
        None if open => folder_open(),
        None => folder(),
    }
}

/// A file's icon by name alone (Settings → Icons preview).
pub fn file_icon(name: &str) -> ImageSource<'static> {
    themed(|t, light| t.file(name, light))
        .or_else(|| finder_icon(|| Some(Icon::file(name))))
        .unwrap_or_else(|| {
            let ext = std::path::Path::new(name)
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase());
            icon_for_ext(ext.as_deref().unwrap_or(""))
        })
}

pub fn generic() -> ImageSource<'static> {
    icon!("file")
}

/// Badge for password-protected archive entries.
pub fn lock() -> ImageSource<'static> {
    icon!("lock")
}

pub fn archive() -> ImageSource<'static> {
    finder_icon(|| {
        Some(Icon::Doc {
            kind: finder::DocKind::Archive,
            label: None,
        })
    })
    .unwrap_or(icon!("zip"))
}

/// `src` drawn `px` physical pixels wide and, for a folder, in `color`, when it is a Keel
/// Finder icon (each size bucket is its own texture); None for other icons.
pub fn sized(
    src: &ImageSource<'static>,
    px: f32,
    color: Option<[u8; 3]>,
) -> Option<ImageSource<'static>> {
    finder::resized(src, px, color)
}

/// `src` for `px` physical pixels: `sized` for Keel Finder icons, else `src` itself.
pub fn at(src: ImageSource<'static>, px: f32) -> ImageSource<'static> {
    sized(&src, px, None).unwrap_or(src)
}

/// Settings → Icons: more of the Keel Finder theme to preview (empty when not in use).
pub fn finder_preview() -> Vec<(&'static str, ImageSource<'static>)> {
    let Some(light) = finder_on() else {
        return Vec::new();
    };
    let special = |s| Icon::Folder {
        glyph: Some(s),
        open: false,
        color: None,
    };
    [
        ("Documents", special(finder::Special::Documents)),
        ("Downloads", special(finder::Special::Downloads)),
        ("Pictures", special(finder::Special::Pictures)),
        ("drive", Icon::Place(finder::Place::Drive)),
        ("network", Icon::Place(finder::Place::Network)),
        ("cloud", Icon::Place(finder::Place::Cloud)),
        ("Trash", Icon::Place(finder::Place::TrashFull)),
    ]
    .into_iter()
    .map(|(name, icon)| (name, finder::source(&icon, light, 16)))
    .collect()
}

/// The same icon under a distinct URI: egui caches one texture per URI at the first
/// size it was drawn, so large tiles need their own entry to rasterize sharply.
pub fn large(src: ImageSource<'static>) -> ImageSource<'static> {
    match src {
        ImageSource::Bytes { uri, bytes } => ImageSource::Bytes {
            uri: format!("bytes://large/{}", uri.trim_start_matches("bytes://")).into(),
            bytes,
        },
        other => other,
    }
}

/// Keel Finder's icon for a listed entry: a special folder, a place or a document.
fn finder_entry(entry: &Entry) -> Icon {
    match entry.kind {
        Kind::Dir => finder::folder(&entry.path, false),
        _ => Icon::file(&entry.name),
    }
}

pub fn icon_for(entry: &Entry) -> ImageSource<'static> {
    let dir = entry.kind == Kind::Dir;
    let found = themed(|t, light| match dir {
        true => t.folder(Some(&entry.name), false, light),
        false => t.file(&entry.name, light),
    })
    .or_else(|| finder_icon(|| Some(finder_entry(entry))));
    if let Some(src) = found {
        return src;
    }
    if dir {
        return folder();
    }
    // `.tar.gz` and friends, and archives nested inside archives, open as folders.
    if keel_vfs::VPath::is_archive_name(&entry.name) {
        return archive();
    }
    icon_for_ext(&entry.ext)
}

pub fn icon_for_ext(ext: &str) -> ImageSource<'static> {
    match ext {
        "rs" => icon!("rust"),
        "py" | "pyw" | "pyi" => icon!("python"),
        "js" | "mjs" | "cjs" | "jsx" => icon!("javascript"),
        "ts" | "mts" | "cts" | "tsx" => icon!("typescript"),
        "json" | "jsonc" | "json5" => icon!("json"),
        "md" | "markdown" => icon!("markdown"),
        "html" | "htm" | "xhtml" => icon!("html"),
        "css" | "scss" | "sass" | "less" => icon!("css"),
        "svg" => icon!("svg"),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff" | "heic"
        | "heif" | "avif" | "dng" | "cr2" | "cr3" | "nef" | "arw" | "orf" | "rw2" | "pef"
        | "raf" => icon!("image"),
        "pdf" => icon!("pdf"),
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v" | "wmv" => icon!("video"),
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "opus" => icon!("audio"),
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" => icon!("zip"),
        "doc" | "docx" | "odt" | "rtf" => icon!("word"),
        "xls" | "xlsx" | "ods" | "csv" | "tsv" => icon!("table"),
        "ppt" | "pptx" | "odp" => icon!("powerpoint"),
        "txt" | "text" => icon!("document"),
        "ps1" | "psm1" | "sh" | "bash" | "zsh" | "fish" | "bat" | "cmd" => icon!("console"),
        "toml" => icon!("toml"),
        "yml" | "yaml" => icon!("yaml"),
        "ini" | "cfg" | "conf" | "config" | "env" | "reg" => icon!("settings"),
        "exe" | "msi" | "lnk" | "app" | "appimage" | "dll" | "so" | "dylib" => icon!("exe"),
        "c" | "h" => icon!("c"),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => icon!("cpp"),
        "cs" => icon!("csharp"),
        "go" => icon!("go"),
        "java" | "jar" | "class" => icon!("java"),
        "xml" | "xaml" | "plist" => icon!("xml"),
        "db" | "sqlite" | "sqlite3" | "sql" | "mdb" => icon!("database"),
        "log" => icon!("log"),
        "ttf" | "otf" | "woff" | "woff2" => icon!("font"),
        "lock" => icon!("lock"),
        "gitignore" | "gitattributes" | "gitmodules" | "patch" | "diff" => icon!("git"),
        _ => generic(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(src: ImageSource<'static>) -> String {
        match src {
            ImageSource::Bytes { uri, .. } => uri.into_owned(),
            other => panic!("unexpected image source {other:?}"),
        }
    }

    #[test]
    fn rust_files_get_a_specific_icon() {
        let dir = keel_vfs::VPath::parse("mem://t/").unwrap();
        let rs = crate::tab::test_entry(&dir, "main.rs", Kind::File, 1);
        assert_ne!(uri(icon_for(&rs)), uri(generic()));
        assert_eq!(uri(icon_for_ext("unknownext")), uri(generic()));
        let d = crate::tab::test_entry(&dir, "src", Kind::Dir, 0);
        assert_eq!(uri(icon_for(&d)), uri(folder()));
        let tgz = crate::tab::test_entry(&dir, "logs.tar.zst", Kind::File, 1);
        assert_eq!(uri(icon_for(&tgz)), uri(archive()));
    }

    /// Keel Finder: entries get documents by kind and folders; `sized` redraws a Finder
    /// icon per size bucket and in a folder color, and leaves other icons alone.
    #[test]
    fn finder_entries_and_sizes() {
        let dir = keel_vfs::VPath::parse("mem://t/").unwrap();
        let rs = crate::tab::test_entry(&dir, "main.rs", Kind::File, 1);
        assert_eq!(finder_entry(&rs).key(), "doc-code-RS");
        let tgz = crate::tab::test_entry(&dir, "logs.tar.zst", Kind::File, 1);
        assert_eq!(finder_entry(&tgz).key(), "doc-archive-ZST");
        let d = crate::tab::test_entry(&dir, "src", Kind::Dir, 0);
        assert_eq!(finder_entry(&d), finder::FOLDER);

        let src = finder::source(&finder::FOLDER, true, 16);
        assert_eq!(uri(src.clone()), "bytes://keel-finder/light/16/folder.svg");
        let big = sized(&src, 120.0, None).unwrap();
        assert_eq!(uri(big), "bytes://keel-finder/light/128/folder.svg");
        let red = sized(&src, 40.0, Some([0xe5, 0x48, 0x4d])).unwrap();
        assert_eq!(uri(red), "bytes://keel-finder/light/64/folder-ce5484d.svg");
        assert!(sized(&generic(), 64.0, None).is_none());
        assert_eq!(uri(at(generic(), 64.0)), uri(generic()));
    }
}
