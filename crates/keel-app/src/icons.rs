//! Default icon theme: Material Icon Theme SVGs (MIT), embedded in the binary.

use egui::ImageSource;
use keel_vfs::{Entry, Kind};

macro_rules! icon {
    ($name:literal) => {
        egui::include_image!(concat!("../../../assets/icons/default/", $name, ".svg"))
    };
}

pub fn folder() -> ImageSource<'static> {
    icon!("folder")
}

pub fn folder_open() -> ImageSource<'static> {
    icon!("folder-open")
}

pub fn generic() -> ImageSource<'static> {
    icon!("file")
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
    if entry.kind == Kind::Dir {
        return folder();
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
    }
}
