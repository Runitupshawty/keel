//! Icons: the active VS Code icon theme (`icon_theme`, set at runtime), else the built-in
//! Material Icon Theme SVGs (MIT) embedded in the binary.

use crate::icon_theme::Loaded;
use egui::ImageSource;
use keel_vfs::{Entry, Kind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

/// The installed theme in use; None = built-in.
static THEME: RwLock<Option<Arc<Loaded>>> = RwLock::new(None);
/// A light color theme is active (icon themes may have a `light` variant).
static LIGHT: AtomicBool = AtomicBool::new(false);

pub fn set_theme(theme: Option<Arc<Loaded>>) {
    *THEME.write().unwrap_or_else(|e| e.into_inner()) = theme;
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

macro_rules! icon {
    ($name:literal) => {
        egui::include_image!(concat!("../../../assets/icons/default/", $name, ".svg"))
    };
}

pub fn folder() -> ImageSource<'static> {
    themed(|t, light| t.folder(None, false, light)).unwrap_or(icon!("folder"))
}

pub fn folder_open() -> ImageSource<'static> {
    themed(|t, light| t.folder(None, true, light)).unwrap_or(icon!("folder-open"))
}

/// A file's icon by name alone (Settings → Icons preview).
pub fn file_icon(name: &str) -> ImageSource<'static> {
    themed(|t, light| t.file(name, light)).unwrap_or_else(|| {
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
    icon!("zip")
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

pub fn icon_for(entry: &Entry) -> ImageSource<'static> {
    let dir = entry.kind == Kind::Dir;
    let found = themed(|t, light| match dir {
        true => t.folder(Some(&entry.name), false, light),
        false => t.file(&entry.name, light),
    });
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
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff" | "heic" => {
            icon!("image")
        }
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
}
