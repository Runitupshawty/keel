//! Task 68: the built-in **Keel Finder** icon theme. Rounded blue folders with a darker
//! tab, special folders with an embossed glyph, places (drives, network, cloud, Trash) and
//! documents with a folded corner and a band naming the extension, colored by kind.
//!
//! The artwork is original, drawn here as SVG source (no traced or copied artwork), and is
//! licensed like the rest of Keel (MIT OR Apache-2.0). Each icon is generated once per
//! (icon, light/dark, size bucket) and handed to egui as SVG bytes: egui_extras rasterizes
//! it (usvg/resvg) and keeps one texture per URI, so every bucket in `SIZES` gets its own
//! URI and texture and stays sharp. The 16 px bucket draws a simpler picture (thicker
//! outlines, no emboss, no document glyph) and the label text appears from 64 px up.

use egui::ImageSource;
use keel_vfs::VPath;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// `Settings::icon_theme` for this theme. Has no `.`, so it is never an installed theme id.
pub const ID: &str = "keel-finder";
/// Its name in Settings → Icons.
pub const LABEL: &str = "Keel Finder (built-in)";
/// Size buckets in pixels: an icon is drawn for the smallest bucket at least as large as
/// the pixels it covers.
pub const SIZES: [u32; 6] = [16, 32, 64, 128, 256, 512];
/// URI prefix of every icon of this theme.
const SCHEME: &str = "bytes://keel-finder/";
/// Generated icons kept at most (they are a few KB each); the cache starts over beyond.
const MAX_CACHED: usize = 4096;

/// A folder with a glyph of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Special {
    Desktop,
    Documents,
    Downloads,
    Pictures,
    Music,
    Videos,
    Home,
    Programs,
}

impl Special {
    pub const ALL: [Special; 8] = [
        Special::Desktop,
        Special::Documents,
        Special::Downloads,
        Special::Pictures,
        Special::Music,
        Special::Videos,
        Special::Home,
        Special::Programs,
    ];

    /// Its name in keys and for `theme:<name>` folder icons.
    pub fn name(self) -> &'static str {
        match self {
            Special::Desktop => "desktop",
            Special::Documents => "documents",
            Special::Downloads => "downloads",
            Special::Pictures => "pictures",
            Special::Music => "music",
            Special::Videos => "videos",
            Special::Home => "home",
            Special::Programs => "applications",
        }
    }

    pub fn from_name(name: &str) -> Option<Special> {
        let name = name.to_ascii_lowercase();
        Special::ALL.into_iter().find(|s| s.name() == name)
    }
}

/// Places drawn as objects rather than folders.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Place {
    Drive,
    Network,
    Cloud,
    TrashEmpty,
    TrashFull,
}

impl Place {
    pub const ALL: [Place; 5] = [
        Place::Drive,
        Place::Network,
        Place::Cloud,
        Place::TrashEmpty,
        Place::TrashFull,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Place::Drive => "drive",
            Place::Network => "network",
            Place::Cloud => "cloud",
            Place::TrashEmpty => "trash",
            Place::TrashFull => "trash-full",
        }
    }
}

/// What a document is, by extension: the color of its band and the glyph on its page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DocKind {
    Image,
    Video,
    Audio,
    Archive,
    Code,
    Text,
    Pdf,
    Spreadsheet,
    Presentation,
    Executable,
    Generic,
}

impl DocKind {
    pub const ALL: [DocKind; 11] = [
        DocKind::Image,
        DocKind::Video,
        DocKind::Audio,
        DocKind::Archive,
        DocKind::Code,
        DocKind::Text,
        DocKind::Pdf,
        DocKind::Spreadsheet,
        DocKind::Presentation,
        DocKind::Executable,
        DocKind::Generic,
    ];

    pub fn name(self) -> &'static str {
        match self {
            DocKind::Image => "image",
            DocKind::Video => "video",
            DocKind::Audio => "audio",
            DocKind::Archive => "archive",
            DocKind::Code => "code",
            DocKind::Text => "text",
            DocKind::Pdf => "pdf",
            DocKind::Spreadsheet => "spreadsheet",
            DocKind::Presentation => "presentation",
            DocKind::Executable => "executable",
            DocKind::Generic => "generic",
        }
    }

    /// The band's color (white label text on it).
    fn color(self) -> [u8; 3] {
        match self {
            DocKind::Image => [0x1c, 0x93, 0x6f],
            DocKind::Video => [0x74, 0x4d, 0xd0],
            DocKind::Audio => [0xd6, 0x3d, 0x73],
            DocKind::Archive => [0xa8, 0x6b, 0x14],
            DocKind::Code => [0x34, 0x55, 0xd1],
            DocKind::Text => [0x5b, 0x67, 0x7a],
            DocKind::Pdf => [0xcf, 0x35, 0x2b],
            DocKind::Spreadsheet => [0x23, 0x84, 0x3a],
            DocKind::Presentation => [0xd9, 0x55, 0x0c],
            DocKind::Executable => [0x3b, 0x43, 0x4f],
            DocKind::Generic => [0x7a, 0x84, 0x93],
        }
    }
}

/// The kind of a lowercase extension.
pub fn kind_of(ext: &str) -> DocKind {
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff" | "heic"
        | "heif" | "avif" | "svg" | "psd" | "dng" | "cr2" | "cr3" | "nef" | "arw" | "orf"
        | "rw2" | "pef" | "raf" | "jxl" => DocKind::Image,
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v" | "wmv" | "mpg" | "mpeg" | "flv" | "3gp" => {
            DocKind::Video
        }
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "opus" | "wma" | "aiff" | "aif"
        | "mid" | "midi" => DocKind::Audio,
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "jar" | "iso"
        | "dmg" | "cab" | "lz" | "lzma" => DocKind::Archive,
        "rs" | "py" | "pyw" | "pyi" | "js" | "mjs" | "cjs" | "jsx" | "ts" | "mts" | "cts"
        | "tsx" | "json" | "jsonc" | "json5" | "html" | "htm" | "xhtml" | "css" | "scss"
        | "sass" | "less" | "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "cs" | "go"
        | "java" | "kt" | "kts" | "swift" | "rb" | "php" | "pl" | "lua" | "sh" | "bash" | "zsh"
        | "fish" | "ps1" | "psm1" | "bat" | "cmd" | "xml" | "xaml" | "yml" | "yaml" | "toml"
        | "ini" | "cfg" | "conf" | "sql" | "vue" | "svelte" | "dart" | "scala" | "hs" | "zig"
        | "nim" | "r" | "m" | "mm" | "gradle" | "cmake" | "mk" | "diff" | "patch" => DocKind::Code,
        "txt" | "text" | "md" | "markdown" | "rst" | "log" | "rtf" | "doc" | "docx" | "odt"
        | "pages" | "tex" | "nfo" | "srt" | "vtt" => DocKind::Text,
        "pdf" | "xps" | "epub" | "djvu" => DocKind::Pdf,
        "xls" | "xlsx" | "xlsm" | "xlsb" | "ods" | "csv" | "tsv" | "numbers" => {
            DocKind::Spreadsheet
        }
        "ppt" | "pptx" | "odp" | "key" => DocKind::Presentation,
        "exe" | "msi" | "lnk" | "app" | "appimage" | "dll" | "so" | "dylib" | "com" | "deb"
        | "rpm" | "apk" | "pkg" | "bin" | "run" => DocKind::Executable,
        _ => DocKind::Generic,
    }
}

/// Up to four characters of A-Z and 0-9 printed on a document's band.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Label {
    chars: [u8; 4],
    len: u8,
}

impl Label {
    /// The first four characters of `ext` in capitals; None (no text) when `ext` is empty
    /// or has a character the band cannot print.
    pub fn new(ext: &str) -> Option<Label> {
        let bytes = ext.as_bytes();
        if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_alphanumeric) {
            return None;
        }
        let mut chars = [0; 4];
        let len = bytes.len().min(4);
        for (c, b) in chars.iter_mut().zip(bytes) {
            *c = b.to_ascii_uppercase();
        }
        Some(Label {
            chars,
            len: len as u8,
        })
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.chars[..self.len as usize]).unwrap_or_default()
    }
}

/// One icon of the theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    /// `open`: the folder being shown (the sidebar's current folder). `color`: a folder
    /// color from the library (the folder is drawn in it instead of blue).
    Folder {
        glyph: Option<Special>,
        open: bool,
        color: Option<[u8; 3]>,
    },
    Place(Place),
    Doc {
        kind: DocKind,
        label: Option<Label>,
    },
}

pub const FOLDER: Icon = Icon::Folder {
    glyph: None,
    open: false,
    color: None,
};

impl Icon {
    /// The icon of a file named `name`.
    pub fn file(name: &str) -> Icon {
        let lower = name.to_ascii_lowercase();
        let ext = lower.rsplit_once('.').map_or("", |(stem, e)| match stem {
            "" => "", // `.gitignore` has no extension here
            _ => e,
        });
        let kind = match VPath::is_archive_name(&lower) {
            true => DocKind::Archive,
            false => kind_of(ext),
        };
        Icon::Doc {
            kind,
            label: Label::new(ext),
        }
    }

    /// A folder with `color` (folders and special folders only).
    pub fn colored(self, rgb: [u8; 3]) -> Icon {
        match self {
            Icon::Folder { glyph, open, .. } => Icon::Folder {
                glyph,
                open,
                color: Some(rgb),
            },
            other => other,
        }
    }

    /// A stable name, part of the icon's URI: `folder`, `folder-open-downloads`,
    /// `folder-c1a2b3c`, `drive`, `doc-image-PNG`.
    pub fn key(&self) -> String {
        match self {
            Icon::Folder { glyph, open, color } => {
                let mut key = String::from("folder");
                if *open {
                    key.push_str("-open");
                }
                if let Some(g) = glyph {
                    let _ = write!(key, "-{}", g.name());
                }
                if let Some([r, g, b]) = color {
                    let _ = write!(key, "-c{r:02x}{g:02x}{b:02x}");
                }
                key
            }
            Icon::Place(p) => p.name().to_owned(),
            Icon::Doc { kind, label } => match label {
                Some(l) => format!("doc-{}-{}", kind.name(), l.as_str()),
                None => format!("doc-{}", kind.name()),
            },
        }
    }

    /// `key` read back.
    pub fn parse_key(key: &str) -> Option<Icon> {
        if let Some(p) = Place::ALL.into_iter().find(|p| p.name() == key) {
            return Some(Icon::Place(p));
        }
        if let Some(rest) = key.strip_prefix("doc-") {
            let (kind, label) = rest
                .split_once('-')
                .map_or((rest, None), |(k, l)| (k, Some(l)));
            let kind = DocKind::ALL.into_iter().find(|k| k.name() == kind)?;
            let label = match label {
                Some(l) => Some(Label::new(l).filter(|p| p.as_str() == l)?),
                None => None,
            };
            return Some(Icon::Doc { kind, label });
        }
        let mut parts = key.split('-');
        (parts.next()? == "folder").then_some(())?;
        let (mut glyph, mut open, mut color) = (None, false, None);
        for part in parts {
            if part == "open" && !open && glyph.is_none() && color.is_none() {
                open = true;
            } else if let Some(s) = Special::from_name(part).filter(|_| glyph.is_none()) {
                (color.is_none()).then_some(())?;
                glyph = Some(s);
            } else {
                let hex = (part.strip_prefix('c'))
                    .filter(|h| h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()))?;
                (color.is_none()).then_some(())?;
                let v = u32::from_str_radix(hex, 16).ok()?;
                color = Some([(v >> 16) as u8, (v >> 8) as u8, v as u8]);
            }
        }
        Some(Icon::Folder { glyph, open, color })
    }

    /// Every icon variant the theme draws (documents with a sample label), for the tests
    /// and the preview sheet.
    #[cfg(test)]
    pub fn catalog() -> Vec<Icon> {
        let mut out = Vec::new();
        for open in [false, true] {
            out.push(Icon::Folder {
                glyph: None,
                open,
                color: None,
            });
        }
        for s in Special::ALL {
            out.push(Icon::Folder {
                glyph: Some(s),
                open: false,
                color: None,
            });
        }
        out.push(FOLDER.colored([0xe5, 0x48, 0x4d]));
        out.push(
            Icon::Folder {
                glyph: Some(Special::Music),
                open: false,
                color: None,
            }
            .colored([0xf5, 0xc0, 0x26]),
        );
        out.extend(Place::ALL.map(Icon::Place));
        let samples = [
            (DocKind::Image, "png"),
            (DocKind::Video, "mp4"),
            (DocKind::Audio, "flac"),
            (DocKind::Archive, "zip"),
            (DocKind::Code, "rs"),
            (DocKind::Text, "txt"),
            (DocKind::Pdf, "pdf"),
            (DocKind::Spreadsheet, "xlsx"),
            (DocKind::Presentation, "pptx"),
            (DocKind::Executable, "exe"),
            (DocKind::Generic, "dat"),
        ];
        for (kind, ext) in samples {
            out.push(Icon::Doc {
                kind,
                label: Label::new(ext),
            });
        }
        out.push(Icon::Doc {
            kind: DocKind::Generic,
            label: None,
        });
        // Every character the band can print.
        for chunk in [
            "ABCD", "EFGH", "IJKL", "MNOP", "QRST", "UVWX", "YZ01", "2345", "6789",
        ] {
            out.push(Icon::Doc {
                kind: DocKind::Text,
                label: Label::new(chunk),
            });
        }
        out
    }
}

/// The bucket for an icon covering `px` physical pixels.
pub fn bucket(px: f32) -> u32 {
    SIZES
        .into_iter()
        .find(|&s| s as f32 >= px * 0.95)
        .unwrap_or(SIZES[SIZES.len() - 1])
}

/// The icon's URI in a bucket and variant.
pub fn uri(icon: &Icon, light: bool, px: u32) -> String {
    let variant = if light { "light" } else { "dark" };
    format!("{SCHEME}{variant}/{px}/{}.svg", icon.key())
}

/// A URI of this theme read back: (icon, light, bucket).
pub fn parse_uri(uri: &str) -> Option<(Icon, bool, u32)> {
    let rest = uri.strip_prefix(SCHEME)?;
    let (variant, rest) = rest.split_once('/')?;
    let (px, rest) = rest.split_once('/')?;
    let light = match variant {
        "light" => true,
        "dark" => false,
        _ => return None,
    };
    let px: u32 = px.parse().ok().filter(|p| SIZES.contains(p))?;
    Some((Icon::parse_key(rest.strip_suffix(".svg")?)?, light, px))
}

type Cache = HashMap<(Icon, bool, u32), ImageSource<'static>>;

/// The icon as an egui image source, generated once per (icon, variant, bucket).
pub fn source(icon: &Icon, light: bool, px: u32) -> ImageSource<'static> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let px = bucket(px as f32);
    let cache = CACHE.get_or_init(Default::default);
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(src) = cache.get(&(*icon, light, px)) {
        return src.clone();
    }
    if cache.len() >= MAX_CACHED {
        cache.clear();
    }
    let bytes: std::sync::Arc<[u8]> = svg(icon, light, px).into_bytes().into();
    let src = ImageSource::Bytes {
        uri: uri(icon, light, px).into(),
        bytes: egui::load::Bytes::Shared(bytes),
    };
    cache.insert((*icon, light, px), src.clone());
    src
}

/// `src` (an icon of this theme) for `px` physical pixels and, for folders, in `color`;
/// None when `src` is not one of this theme's icons.
pub fn resized(
    src: &ImageSource<'static>,
    px: f32,
    color: Option<[u8; 3]>,
) -> Option<ImageSource<'static>> {
    let ImageSource::Bytes { uri, .. } = src else {
        return None;
    };
    let (icon, light, _) = parse_uri(uri)?;
    let icon = match color {
        Some(rgb) => icon.colored(rgb),
        None => icon,
    };
    Some(source(&icon, light, bucket(px)))
}

// ---------------------------------------------------------------------------------------
// Which icon a folder gets.

/// Known folders: (path, icon). Built once from the OS's user folders.
fn known() -> &'static [(VPath, Icon)] {
    static KNOWN: OnceLock<Vec<(VPath, Icon)>> = OnceLock::new();
    KNOWN.get_or_init(|| {
        let mut out = Vec::new();
        let mut add = |p: Option<&std::path::Path>, s: Special| {
            if let Some(p) = p {
                out.push((
                    VPath::local(p),
                    Icon::Folder {
                        glyph: Some(s),
                        open: false,
                        color: None,
                    },
                ));
            }
        };
        if let Some(u) = directories::UserDirs::new() {
            add(Some(u.home_dir()), Special::Home);
            add(u.desktop_dir(), Special::Desktop);
            add(u.document_dir(), Special::Documents);
            add(u.download_dir(), Special::Downloads);
            add(u.picture_dir(), Special::Pictures);
            add(u.audio_dir(), Special::Music);
            add(u.video_dir(), Special::Videos);
        }
        let mut programs: Vec<std::path::PathBuf> = Vec::new();
        if cfg!(windows) {
            for var in ["ProgramFiles", "ProgramFiles(x86)"] {
                programs.extend(std::env::var_os(var).map(Into::into));
            }
        } else if cfg!(target_os = "macos") {
            programs.push("/Applications".into());
        }
        for p in &programs {
            add(Some(p), Special::Programs);
        }
        out
    })
}

/// A path at the root of its file system: `C:/`, `/`, `//server/share`, `sftp://host/`.
fn is_root(p: &VPath) -> bool {
    let path = p.path.trim_end_matches('/');
    if path.is_empty() {
        return true;
    }
    p.scheme == "file"
        && cfg!(windows)
        && (path.len() == 2 && path.ends_with(':')
            || path.starts_with("//") && path[2..].split('/').count() <= 2)
}

fn is_trash_root(p: &VPath) -> bool {
    p.scheme == keel_vfs::trashbin::SCHEME && p.path.trim_matches('/').is_empty()
}

/// Whether the Recycle Bin / Trash had items when last listed (empty until then).
static TRASH_FULL: AtomicBool = AtomicBool::new(false);

/// Per listing (`Tab::set_listing`): a listing of the Trash itself says whether it is full.
pub fn note_listing(dir: &VPath, entries: &[keel_vfs::Entry]) {
    if is_trash_root(dir) {
        TRASH_FULL.store(!entries.is_empty(), Ordering::Relaxed);
    }
}

/// The icon a folder at `path` gets: a place for roots and the Trash (full or empty), a
/// special folder for the user's folders in `known`, else a plain folder.
pub fn folder_in(path: &VPath, open: bool, known: &[(VPath, Icon)], trash_full: bool) -> Icon {
    if is_trash_root(path) {
        return Icon::Place(match trash_full {
            true => Place::TrashFull,
            false => Place::TrashEmpty,
        });
    }
    if is_root(path) && path.split_archive().is_none() {
        let place = match path.scheme.as_str() {
            "file" if path.path.starts_with("//") || !path.authority.is_empty() => {
                Some(Place::Network)
            }
            "file" => Some(Place::Drive),
            "cloud" => Some(Place::Cloud),
            "sftp" | "ftp" | "smb" | "webdav" | "node" => Some(Place::Network),
            _ => None,
        };
        if let Some(place) = place {
            return Icon::Place(place);
        }
    }
    let same = |a: &VPath| {
        a.scheme == path.scheme
            && a.authority == path.authority
            && match cfg!(windows) {
                true => a
                    .path
                    .trim_end_matches('/')
                    .eq_ignore_ascii_case(path.path.trim_end_matches('/')),
                false => a.path.trim_end_matches('/') == path.path.trim_end_matches('/'),
            }
    };
    if let Some((_, icon)) = known.iter().find(|(p, _)| same(p)) {
        return *icon;
    }
    Icon::Folder {
        glyph: None,
        open,
        color: None,
    }
}

/// `folder_in` with this user's folders.
pub fn folder(path: &VPath, open: bool) -> Icon {
    folder_in(path, open, known(), TRASH_FULL.load(Ordering::Relaxed))
}

// ---------------------------------------------------------------------------------------
// Drawing. Every icon is drawn on a 64 x 64 grid; glyphs on a 24 x 24 grid placed on it.

/// A color as `#rrggbb`.
fn hex([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// `a` moved toward `b` by `t` (0..1).
fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    [m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2])]
}

const WHITE: [u8; 3] = [255, 255, 255];
const BLACK: [u8; 3] = [0, 0, 0];
/// The folder blue, a touch brighter in the dark variant to stand off dark backgrounds.
const BLUE_LIGHT: [u8; 3] = [0x4c, 0xa2, 0xec];
const BLUE_DARK: [u8; 3] = [0x58, 0xab, 0xf1];

/// A rounded rectangle as path data.
fn rrect(x: f32, y: f32, w: f32, h: f32, r: f32) -> String {
    format!(
        "M{a} {y}H{b}A{r} {r} 0 0 1 {x2} {c}V{d}A{r} {r} 0 0 1 {b} {y2}H{a}A{r} {r} 0 0 1 {x} {d}V{c}A{r} {r} 0 0 1 {a} {y}Z",
        a = x + r,
        b = x + w - r,
        c = y + r,
        d = y + h - r,
        x2 = x + w,
        y2 = y + h,
    )
}

/// A circle as path data.
fn circle(cx: f32, cy: f32, r: f32) -> String {
    format!(
        "M{a} {cy}A{r} {r} 0 1 0 {b} {cy}A{r} {r} 0 1 0 {a} {cy}Z",
        a = cx - r,
        b = cx + r
    )
}

/// Glyph strokes (`false`) and fills (`true`) on the 24 x 24 grid.
type Glyph = Vec<(bool, String)>;

fn s(d: &str) -> (bool, String) {
    (false, d.to_owned())
}

fn f(d: String) -> (bool, String) {
    (true, d)
}

fn pictures_glyph() -> Glyph {
    vec![
        (false, rrect(3.5, 4.5, 17.0, 15.0, 1.8)),
        s("M3.8 16.2l5-5 4.4 4.4 2.6-2.6 4.6 4.6"),
        f(circle(15.6, 8.9, 1.9)),
    ]
}

fn video_glyph() -> Glyph {
    vec![
        (false, rrect(3.0, 5.0, 18.0, 14.0, 2.2)),
        f("M10 8.8v6.4l5.6-3.2z".to_owned()),
    ]
}

fn music_glyph() -> Glyph {
    vec![
        s("M9.2 17.6V6.2l10.6-2.4v11.6"),
        f(circle(6.7, 17.6, 2.6)),
        f(circle(17.3, 15.4, 2.6)),
    ]
}

fn special_glyph(sp: Special) -> Glyph {
    match sp {
        Special::Desktop => vec![
            (false, rrect(3.0, 4.5, 18.0, 12.5, 1.6)),
            s("M12 17v3.2"),
            s("M8 20.5h8"),
        ],
        Special::Documents => vec![
            s("M7 3h7l5 5v11.5a1.5 1.5 0 0 1-1.5 1.5H7a1.5 1.5 0 0 1-1.5-1.5v-15A1.5 1.5 0 0 1 7 3z"),
            s("M14 3v5h5"),
            s("M9 12.8h6"),
            s("M9 16.4h6"),
        ],
        Special::Downloads => vec![
            s("M12 3.5v11"),
            s("M7.4 10.4l4.6 4.6 4.6-4.6"),
            s("M4 16.5v2.6a1.4 1.4 0 0 0 1.4 1.4h13.2a1.4 1.4 0 0 0 1.4-1.4v-2.6"),
        ],
        Special::Pictures => pictures_glyph(),
        Special::Music => music_glyph(),
        Special::Videos => video_glyph(),
        Special::Home => vec![
            s("M3.4 11.6L12 4.2l8.6 7.4"),
            s("M6 10v9.6h4.4v-5.4h3.2v5.4H18V10"),
        ],
        Special::Programs => vec![
            (false, rrect(4.0, 4.0, 6.5, 6.5, 1.6)),
            (false, rrect(13.5, 4.0, 6.5, 6.5, 1.6)),
            (false, rrect(4.0, 13.5, 6.5, 6.5, 1.6)),
            (false, rrect(13.5, 13.5, 6.5, 6.5, 1.6)),
        ],
    }
}

fn gear() -> String {
    // Eight teeth around a hole (even-odd fill).
    let mut d = String::new();
    let (cx, cy) = (12.0_f32, 12.0_f32);
    let n = 8;
    for i in 0..n * 4 {
        let a = (i as f32 + 0.5) / (n * 4) as f32 * std::f32::consts::TAU;
        let r = if (i / 2) % 2 == 0 { 9.4 } else { 7.0 };
        let (x, y) = (cx + r * a.cos(), cy + r * a.sin());
        let _ = write!(d, "{}{x:.2} {y:.2}", if i == 0 { 'M' } else { 'L' });
    }
    d.push('Z');
    d.push_str(&circle(cx, cy, 3.2));
    d
}

fn doc_glyph(kind: DocKind) -> Glyph {
    match kind {
        DocKind::Image => pictures_glyph(),
        DocKind::Video => video_glyph(),
        DocKind::Audio => music_glyph(),
        DocKind::Archive => vec![
            s("M12 2.5v1.8"),
            s("M12 6.3v1.8"),
            s("M12 10.1v1.8"),
            (false, rrect(9.4, 13.4, 5.2, 7.4, 1.2)),
            s("M12 16v2.2"),
        ],
        DocKind::Code => vec![
            s("M8.4 7l-5 5 5 5"),
            s("M15.6 7l5 5-5 5"),
            s("M13.6 4.4l-3.2 15.2"),
        ],
        DocKind::Text => vec![
            s("M5 5.5h14"),
            s("M5 9.5h14"),
            s("M5 13.5h14"),
            s("M5 17.5h9"),
        ],
        DocKind::Pdf => vec![
            s("M4.5 5.5h10"),
            s("M4.5 9.5h15"),
            s("M4.5 13.5h8"),
            f("M15 13h5.2v8.4l-2.6-2-2.6 2z".to_owned()),
        ],
        DocKind::Spreadsheet => vec![
            (false, rrect(3.5, 4.5, 17.0, 15.0, 1.6)),
            s("M3.5 9.5h17"),
            s("M3.5 14.5h17"),
            s("M9.5 4.5v15"),
            s("M15 4.5v15"),
        ],
        DocKind::Presentation => vec![
            (false, rrect(3.5, 3.5, 17.0, 12.0, 1.6)),
            s("M12 15.5v4"),
            s("M8 20.6l4-1.2 4 1.2"),
            s("M8 12.5v-2.6"),
            s("M12 12.5V7.2"),
            s("M16 12.5V9.4"),
        ],
        DocKind::Executable => vec![f(gear())],
        DocKind::Generic => Vec::new(),
    }
}

/// `glyph` drawn in `color`, centered at (cx, cy) on the 64 grid, `size` units across.
fn draw_glyph(out: &mut String, glyph: &Glyph, color: &str, cx: f32, cy: f32, size: f32, sw: f32) {
    let k = size / 24.0;
    let _ = write!(
        out,
        r#"<g transform="translate({:.2} {:.2}) scale({k:.4})" stroke-linecap="round" stroke-linejoin="round">"#,
        cx - 12.0 * k,
        cy - 12.0 * k,
    );
    for (fill, d) in glyph {
        match fill {
            true => {
                let _ = write!(
                    out,
                    r#"<path d="{d}" fill="{color}" fill-rule="evenodd" stroke="none"/>"#
                );
            }
            false => {
                let _ = write!(
                    out,
                    r#"<path d="{d}" fill="none" stroke="{color}" stroke-width="{sw:.2}"/>"#
                );
            }
        }
    }
    out.push_str("</g>");
}

/// Strokes of a character on a 4 x 6 grid (y down), for the document band.
fn letter(c: u8) -> &'static [&'static [(f32, f32)]] {
    const O: &[(f32, f32)] = &[
        (1.0, 0.0),
        (3.0, 0.0),
        (4.0, 1.0),
        (4.0, 5.0),
        (3.0, 6.0),
        (1.0, 6.0),
        (0.0, 5.0),
        (0.0, 1.0),
        (1.0, 0.0),
    ];
    const P: &[(f32, f32)] = &[
        (0.0, 6.0),
        (0.0, 0.0),
        (3.0, 0.0),
        (4.0, 1.0),
        (4.0, 2.0),
        (3.0, 3.0),
        (0.0, 3.0),
    ];
    match c {
        b'A' => &[
            &[(0.0, 6.0), (0.0, 2.0), (2.0, 0.0), (4.0, 2.0), (4.0, 6.0)],
            &[(0.0, 3.6), (4.0, 3.6)],
        ],
        b'B' => &[&[
            (0.0, 3.0),
            (3.0, 3.0),
            (4.0, 4.0),
            (4.0, 5.0),
            (3.0, 6.0),
            (0.0, 6.0),
            (0.0, 0.0),
            (2.8, 0.0),
            (3.8, 1.0),
            (3.8, 2.0),
            (2.8, 3.0),
        ]],
        b'C' => &[&[
            (4.0, 1.0),
            (3.0, 0.0),
            (1.0, 0.0),
            (0.0, 1.0),
            (0.0, 5.0),
            (1.0, 6.0),
            (3.0, 6.0),
            (4.0, 5.0),
        ]],
        b'D' => &[&[
            (0.0, 0.0),
            (0.0, 6.0),
            (2.4, 6.0),
            (4.0, 4.4),
            (4.0, 1.6),
            (2.4, 0.0),
            (0.0, 0.0),
        ]],
        b'E' => &[
            &[(4.0, 0.0), (0.0, 0.0), (0.0, 6.0), (4.0, 6.0)],
            &[(0.0, 3.0), (3.0, 3.0)],
        ],
        b'F' => &[
            &[(4.0, 0.0), (0.0, 0.0), (0.0, 6.0)],
            &[(0.0, 3.0), (3.0, 3.0)],
        ],
        b'G' => &[&[
            (4.0, 1.0),
            (3.0, 0.0),
            (1.0, 0.0),
            (0.0, 1.0),
            (0.0, 5.0),
            (1.0, 6.0),
            (3.0, 6.0),
            (4.0, 5.0),
            (4.0, 3.4),
            (2.4, 3.4),
        ]],
        b'H' => &[
            &[(0.0, 0.0), (0.0, 6.0)],
            &[(4.0, 0.0), (4.0, 6.0)],
            &[(0.0, 3.0), (4.0, 3.0)],
        ],
        b'I' => &[
            &[(1.0, 0.0), (3.0, 0.0)],
            &[(2.0, 0.0), (2.0, 6.0)],
            &[(1.0, 6.0), (3.0, 6.0)],
        ],
        b'J' => &[&[(4.0, 0.0), (4.0, 5.0), (3.0, 6.0), (1.0, 6.0), (0.0, 5.0)]],
        b'K' => &[
            &[(0.0, 0.0), (0.0, 6.0)],
            &[(4.0, 0.0), (0.0, 4.0)],
            &[(1.4, 2.6), (4.0, 6.0)],
        ],
        b'L' => &[&[(0.0, 0.0), (0.0, 6.0), (4.0, 6.0)]],
        b'M' => &[&[(0.0, 6.0), (0.0, 0.0), (2.0, 3.0), (4.0, 0.0), (4.0, 6.0)]],
        b'N' => &[&[(0.0, 6.0), (0.0, 0.0), (4.0, 6.0), (4.0, 0.0)]],
        b'O' => &[O],
        b'P' => &[P],
        b'Q' => &[O, &[(2.6, 4.6), (4.0, 6.0)]],
        b'R' => &[P, &[(2.0, 3.0), (4.0, 6.0)]],
        b'S' => &[&[
            (4.0, 1.0),
            (3.0, 0.0),
            (1.0, 0.0),
            (0.0, 1.0),
            (0.0, 2.0),
            (1.0, 3.0),
            (3.0, 3.0),
            (4.0, 4.0),
            (4.0, 5.0),
            (3.0, 6.0),
            (1.0, 6.0),
            (0.0, 5.0),
        ]],
        b'T' => &[&[(0.0, 0.0), (4.0, 0.0)], &[(2.0, 0.0), (2.0, 6.0)]],
        b'U' => &[&[
            (0.0, 0.0),
            (0.0, 5.0),
            (1.0, 6.0),
            (3.0, 6.0),
            (4.0, 5.0),
            (4.0, 0.0),
        ]],
        b'V' => &[&[(0.0, 0.0), (2.0, 6.0), (4.0, 0.0)]],
        b'W' => &[&[(0.0, 0.0), (1.0, 6.0), (2.0, 3.0), (3.0, 6.0), (4.0, 0.0)]],
        b'X' => &[&[(0.0, 0.0), (4.0, 6.0)], &[(4.0, 0.0), (0.0, 6.0)]],
        b'Y' => &[
            &[(0.0, 0.0), (2.0, 3.0), (4.0, 0.0)],
            &[(2.0, 3.0), (2.0, 6.0)],
        ],
        b'Z' => &[&[(0.0, 0.0), (4.0, 0.0), (0.0, 6.0), (4.0, 6.0)]],
        b'0' => &[O, &[(3.6, 1.2), (0.4, 4.8)]],
        b'1' => &[
            &[(0.8, 1.2), (2.0, 0.0), (2.0, 6.0)],
            &[(0.8, 6.0), (3.2, 6.0)],
        ],
        b'2' => &[&[
            (0.0, 1.0),
            (1.0, 0.0),
            (3.0, 0.0),
            (4.0, 1.0),
            (4.0, 2.2),
            (0.0, 6.0),
            (4.0, 6.0),
        ]],
        b'3' => &[
            &[
                (0.0, 1.0),
                (1.0, 0.0),
                (3.0, 0.0),
                (4.0, 1.0),
                (4.0, 2.0),
                (3.0, 3.0),
                (4.0, 4.0),
                (4.0, 5.0),
                (3.0, 6.0),
                (1.0, 6.0),
                (0.0, 5.0),
            ],
            &[(1.6, 3.0), (3.0, 3.0)],
        ],
        b'4' => &[&[(3.0, 6.0), (3.0, 0.0), (0.0, 4.2), (4.0, 4.2)]],
        b'5' => &[&[
            (4.0, 0.0),
            (0.4, 0.0),
            (0.0, 3.0),
            (3.0, 2.8),
            (4.0, 3.8),
            (4.0, 5.0),
            (3.0, 6.0),
            (0.0, 6.0),
        ]],
        b'6' => &[&[
            (3.4, 0.0),
            (1.0, 0.0),
            (0.0, 1.0),
            (0.0, 5.0),
            (1.0, 6.0),
            (3.0, 6.0),
            (4.0, 5.0),
            (4.0, 4.0),
            (3.0, 3.0),
            (0.0, 3.0),
        ]],
        b'7' => &[&[(0.0, 0.0), (4.0, 0.0), (1.4, 6.0)]],
        b'8' => &[&[
            (1.0, 3.0),
            (0.2, 2.2),
            (0.2, 0.9),
            (1.1, 0.0),
            (2.9, 0.0),
            (3.8, 0.9),
            (3.8, 2.2),
            (3.0, 3.0),
            (1.0, 3.0),
            (0.0, 4.0),
            (0.0, 5.0),
            (1.0, 6.0),
            (3.0, 6.0),
            (4.0, 5.0),
            (4.0, 4.0),
            (3.0, 3.0),
        ]],
        b'9' => &[&[
            (4.0, 3.0),
            (1.0, 3.0),
            (0.0, 2.0),
            (0.0, 1.0),
            (1.0, 0.0),
            (3.0, 0.0),
            (4.0, 1.0),
            (4.0, 5.0),
            (3.0, 6.0),
            (0.6, 6.0),
        ]],
        _ => &[],
    }
}

/// `text` in white strokes, centered at (cx, cy), `h` units tall.
fn draw_label(out: &mut String, text: &str, cx: f32, cy: f32, h: f32) {
    let k = h / 6.0;
    let advance = 5.5;
    let width = text.len() as f32 * advance - 1.5;
    let (x0, y0) = (cx - width * k / 2.0, cy - h / 2.0);
    let mut d = String::new();
    for (i, c) in text.bytes().enumerate() {
        for stroke in letter(c) {
            for (j, (x, y)) in stroke.iter().enumerate() {
                let px = x0 + (i as f32 * advance + x) * k;
                let py = y0 + y * k;
                let _ = write!(d, "{}{px:.2} {py:.2}", if j == 0 { 'M' } else { 'L' });
            }
        }
    }
    let _ = write!(
        out,
        r##"<path d="{d}" fill="none" stroke="#ffffff" stroke-width="{:.2}" stroke-linecap="round" stroke-linejoin="round"/>"##,
        1.25 * k
    );
}

/// A vertical gradient `id` from `top` to `bottom`.
fn gradient(out: &mut String, id: &str, top: [u8; 3], bottom: [u8; 3]) {
    let _ = write!(
        out,
        r#"<linearGradient id="{id}" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{}"/><stop offset="1" stop-color="{}"/></linearGradient>"#,
        hex(top),
        hex(bottom)
    );
}

/// The outline width on the 64 grid for an icon drawn at `px`: about 0.8 px at small sizes,
/// never thicker than needed at large ones.
fn outline(px: u32) -> f32 {
    (0.8 * 64.0 / px as f32).clamp(0.5, 3.2)
}

/// The icon as SVG source.
pub fn svg(icon: &Icon, light: bool, px: u32) -> String {
    let mut out = String::with_capacity(4096);
    out.push_str(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" viewBox="0 0 64 64">"#,
    );
    match *icon {
        Icon::Folder { glyph, open, color } => folder_svg(&mut out, glyph, open, color, light, px),
        Icon::Place(p) => place_svg(&mut out, p, light, px),
        Icon::Doc { kind, label } => doc_svg(&mut out, kind, label, light, px),
    }
    out.push_str("</svg>");
    out
}

fn folder_svg(
    out: &mut String,
    glyph: Option<Special>,
    open: bool,
    color: Option<[u8; 3]>,
    light: bool,
    px: u32,
) {
    let small = px <= 16;
    let base = color.unwrap_or(if light { BLUE_LIGHT } else { BLUE_DARK });
    let ow = outline(px);
    let edge = mix(base, BLACK, if light { 0.42 } else { 0.32 });
    out.push_str("<defs>");
    gradient(out, "back", mix(base, BLACK, 0.10), mix(base, BLACK, 0.26));
    gradient(out, "front", mix(base, WHITE, 0.34), mix(base, BLACK, 0.02));
    out.push_str("</defs>");
    // The back with its tab.
    let back = "M6 13.2Q6 10 9.2 10H22.6Q24.6 10 25.9 11.5L28.2 14H54.8Q58 14 58 17.2V50Q58 53 55 53H9Q6 53 6 50Z";
    let front = match open {
        false => rrect(4.0, 18.5, 56.0, 36.5, 3.6),
        // Tipped forward: the top edge sits lower and further right.
        true => "M12.6 25H58.4Q61.6 25 61 28.2L57.4 51.8Q56.9 55 53.7 55H7.2Q4 55 4.3 51.8L9.4 28.2Q10 25 12.6 25Z".to_owned(),
    };
    if !small {
        let _ = write!(
            out,
            r##"<path d="{front}" transform="translate(0 1.3)" fill="#000000" opacity="0.16"/>"##
        );
    }
    let _ = write!(
        out,
        r#"<path d="{back}" fill="url(#back)" stroke="{}" stroke-width="{ow:.2}" stroke-opacity="0.55" stroke-linejoin="round"/>"#,
        hex(edge)
    );
    if !small {
        // A sheet of paper between back and front.
        let paper = if light {
            [0xf7, 0xfa, 0xfd]
        } else {
            [0xe4, 0xea, 0xf1]
        };
        let sheet = if open {
            rrect(10.0, 15.6, 45.0, 14.0, 1.6)
        } else {
            rrect(9.0, 15.6, 46.0, 8.0, 1.6)
        };
        let _ = write!(out, r#"<path d="{sheet}" fill="{}"/>"#, hex(paper));
    }
    let _ = write!(
        out,
        r#"<path d="{front}" fill="url(#front)" stroke="{}" stroke-width="{ow:.2}" stroke-opacity="0.55" stroke-linejoin="round"/>"#,
        hex(edge)
    );
    if !small {
        let lip = if open {
            "M12.2 26.2H58.6"
        } else {
            "M7 19.6H57"
        };
        let _ = write!(
            out,
            r##"<path d="{lip}" stroke="#ffffff" stroke-opacity="0.6" stroke-width="0.9" stroke-linecap="round"/>"##
        );
    }
    if let Some(g) = glyph {
        let glyph = special_glyph(g);
        let (cx, cy) = if open { (33.0, 40.5) } else { (32.0, 37.2) };
        let size = if small { 26.0 } else { 21.0 };
        let sw = if small {
            3.4
        } else if px <= 32 {
            2.4
        } else {
            2.0
        };
        if !small {
            // Embossed: a light edge under a darker glyph.
            let mut hi = String::new();
            draw_glyph(&mut hi, &glyph, "#ffffff", cx, cy + 0.9, size, sw);
            let _ = write!(out, r#"<g opacity="0.5">{hi}</g>"#);
        }
        let ink = hex(mix(base, BLACK, 0.36));
        let mut g = String::new();
        draw_glyph(&mut g, &glyph, &ink, cx, cy, size, sw);
        let _ = write!(out, r#"<g opacity="0.92">{g}</g>"#);
    }
}

fn doc_svg(out: &mut String, kind: DocKind, label: Option<Label>, light: bool, px: u32) {
    let small = px <= 16;
    let ow = outline(px);
    let (paper_top, paper_bottom, edge, fold) = match light {
        true => (
            [0xff, 0xff, 0xff],
            [0xf0, 0xf3, 0xf7],
            [0x9a, 0xa4, 0xb1],
            [0xdc, 0xe1, 0xe8],
        ),
        false => (
            [0xf3, 0xf5, 0xf8],
            [0xd9, 0xde, 0xe5],
            [0x6f, 0x79, 0x86],
            [0xc4, 0xcb, 0xd4],
        ),
    };
    out.push_str("<defs>");
    gradient(out, "paper", paper_top, paper_bottom);
    out.push_str("</defs>");
    let page = "M16 4H39.5L51 15.5V57Q51 60 48 60H16Q13 60 13 57V7Q13 4 16 4Z";
    if !small {
        let _ = write!(
            out,
            r##"<path d="{page}" transform="translate(0 1.2)" fill="#000000" opacity="0.14"/>"##
        );
    }
    let _ = write!(
        out,
        r#"<path d="{page}" fill="url(#paper)" stroke="{}" stroke-width="{ow:.2}" stroke-linejoin="round"/>"#,
        hex(edge)
    );
    let _ = write!(
        out,
        r#"<path d="M39.5 4V12.5Q39.5 15.5 42.5 15.5H51Z" fill="{}" stroke="{}" stroke-width="{ow:.2}" stroke-linejoin="round"/>"#,
        hex(fold),
        hex(edge)
    );
    let band = kind.color();
    let has_band = label.is_some() || kind != DocKind::Generic;
    if !small {
        let glyph = doc_glyph(kind);
        if !glyph.is_empty() {
            let cy = if has_band { 26.5 } else { 32.0 };
            draw_glyph(out, &glyph, &hex(band), 31.0, cy, 19.0, 2.1);
        }
    }
    if has_band {
        let (y, h) = if small { (38.0, 14.0) } else { (42.0, 11.0) };
        let _ = write!(
            out,
            r#"<rect x="13" y="{y}" width="38" height="{h}" fill="{}"/>"#,
            hex(band)
        );
        if let Some(l) = label.filter(|_| px >= 64) {
            draw_label(out, l.as_str(), 32.0, y + h / 2.0, 6.0);
        }
    }
}

fn place_svg(out: &mut String, place: Place, light: bool, px: u32) {
    let small = px <= 16;
    let ow = outline(px);
    match place {
        Place::Drive => {
            let (top, bottom, face_top, face_bottom, edge) = match light {
                true => (
                    [0xf4, 0xf6, 0xf8],
                    [0xd2, 0xd7, 0xde],
                    [0xc6, 0xcd, 0xd6],
                    [0xa3, 0xac, 0xb8],
                    [0x6f, 0x79, 0x86],
                ),
                false => (
                    [0xe6, 0xe9, 0xee],
                    [0xc0, 0xc6, 0xcf],
                    [0xae, 0xb6, 0xc1],
                    [0x8a, 0x94, 0xa1],
                    [0x55, 0x5e, 0x6a],
                ),
            };
            out.push_str("<defs>");
            gradient(out, "top", top, bottom);
            gradient(out, "face", face_top, face_bottom);
            out.push_str("</defs>");
            let body = rrect(5.0, 19.0, 54.0, 28.0, 6.0);
            if !small {
                let _ = write!(
                    out,
                    r##"<path d="{body}" transform="translate(0 1.4)" fill="#000000" opacity="0.16"/>"##
                );
            }
            let _ = write!(
                out,
                r#"<path d="{body}" fill="url(#top)" stroke="{}" stroke-width="{ow:.2}"/>"#,
                hex(edge)
            );
            let _ = write!(
                out,
                r#"<path d="M5 34H59V41Q59 47 53 47H11Q5 47 5 41Z" fill="url(#face)" stroke="{}" stroke-width="{ow:.2}" stroke-linejoin="round"/>"#,
                hex(edge)
            );
            if !small {
                let _ = write!(
                    out,
                    r#"<path d="M12 37.5v6M16 37.5v6M20 37.5v6M24 37.5v6" stroke="{}" stroke-width="1.6" stroke-linecap="round" stroke-opacity="0.7"/>"#,
                    hex(edge)
                );
                out.push_str(
                    r##"<path d="M9 21.8H55" stroke="#ffffff" stroke-opacity="0.7" stroke-width="0.9" stroke-linecap="round"/>"##,
                );
            }
            let _ = write!(
                out,
                r##"<path d="{}" fill="#2fbf62"/>"##,
                circle(51.0, 40.5, if small { 3.2 } else { 2.2 })
            );
        }
        Place::Network => {
            let (top, bottom) = match light {
                true => ([0x7c, 0xc8, 0xf2], [0x23, 0x74, 0xc4]),
                false => ([0x86, 0xcf, 0xf6], [0x2b, 0x7f, 0xcf]),
            };
            out.push_str("<defs>");
            gradient(out, "globe", top, bottom);
            out.push_str("</defs>");
            let ball = circle(32.0, 32.0, 25.0);
            if !small {
                let _ = write!(
                    out,
                    r##"<path d="{ball}" transform="translate(0 1.3)" fill="#000000" opacity="0.16"/>"##
                );
            }
            let _ = write!(
                out,
                r#"<path d="{ball}" fill="url(#globe)" stroke="{}" stroke-width="{ow:.2}"/>"#,
                hex(mix(bottom, BLACK, 0.3))
            );
            let lines = match small {
                true => "M7 32H57M32 7V57".to_owned(),
                false => format!(
                    "M7 32H57M10.5 20.5Q32 26 53.5 20.5M10.5 43.5Q32 38 53.5 43.5{}M32 7V57",
                    "M32 7Q18 32 32 57M32 7Q46 32 32 57"
                ),
            };
            let _ = write!(
                out,
                r##"<path d="{lines}" fill="none" stroke="#ffffff" stroke-opacity="0.62" stroke-width="{:.2}" stroke-linecap="round"/>"##,
                if small { 3.2 } else { 1.5 }
            );
            if !small {
                out.push_str(
                    r##"<path d="M16 17Q22 10.5 31 9.4" fill="none" stroke="#ffffff" stroke-opacity="0.55" stroke-width="2.2" stroke-linecap="round"/>"##,
                );
            }
        }
        Place::Cloud => {
            let (top, bottom, edge) = match light {
                true => ([0xff, 0xff, 0xff], [0xc9, 0xdf, 0xf5], [0x6d, 0x9c, 0xcc]),
                false => ([0xf4, 0xf8, 0xfc], [0xb8, 0xd3, 0xef], [0x5a, 0x8b, 0xbf]),
            };
            out.push_str("<defs>");
            gradient(out, "cloud", top, bottom);
            out.push_str("</defs>");
            let cloud =
                "M18.5 48H47A11 11 0 0 0 47 26A15 15 0 0 0 19 24.5A11.75 11.75 0 0 0 18.5 48Z";
            if !small {
                let _ = write!(
                    out,
                    r##"<path d="{cloud}" transform="translate(0 1.3)" fill="#000000" opacity="0.14"/>"##
                );
            }
            let _ = write!(
                out,
                r#"<path d="{cloud}" fill="url(#cloud)" stroke="{}" stroke-width="{:.2}" stroke-linejoin="round"/>"#,
                hex(edge),
                ow.max(1.0)
            );
        }
        Place::TrashEmpty | Place::TrashFull => {
            let (top, bottom, edge, inside) = match light {
                true => (
                    [0xf1, 0xf3, 0xf6],
                    [0xc3, 0xca, 0xd3],
                    [0x6f, 0x79, 0x86],
                    [0x4d, 0x55, 0x60],
                ),
                false => (
                    [0xe2, 0xe6, 0xeb],
                    [0xaa, 0xb2, 0xbd],
                    [0x55, 0x5e, 0x6a],
                    [0x3a, 0x41, 0x4b],
                ),
            };
            out.push_str("<defs>");
            gradient(out, "bin", top, bottom);
            out.push_str("</defs>");
            let body = "M14.5 19H49.5L46.6 56Q46.3 59.5 42.8 59.5H21.2Q17.7 59.5 17.4 56Z";
            if !small {
                let _ = write!(
                    out,
                    r##"<path d="{body}" transform="translate(0 1.2)" fill="#000000" opacity="0.15"/>"##
                );
            }
            let _ = write!(
                out,
                r#"<path d="{body}" fill="url(#bin)" stroke="{}" stroke-width="{ow:.2}" stroke-linejoin="round"/>"#,
                hex(edge)
            );
            if !small {
                let _ = write!(
                    out,
                    r#"<path d="M24.6 24L25.4 54.5M32 24V54.5M39.4 24L38.6 54.5" stroke="{}" stroke-opacity="0.55" stroke-width="1.8" stroke-linecap="round"/>"#,
                    hex(edge)
                );
            }
            // The rim, its opening, and what is in it.
            let _ = write!(
                out,
                r#"<path d="{}" fill="url(#bin)" stroke="{}" stroke-width="{ow:.2}"/>"#,
                rrect(11.5, 12.5, 41.0, 7.5, 3.75),
                hex(edge)
            );
            let _ = write!(
                out,
                r#"<path d="{}" fill="{}" opacity="0.75"/>"#,
                rrect(14.5, 14.0, 35.0, 3.6, 1.8),
                hex(inside)
            );
            if place == Place::TrashFull {
                let papers = [
                    ("M17 16.5L18.6 8.4L25.8 6.2L30.4 10.4L28.6 16.5Z", "#ffffff"),
                    ("M27 16.5L30.6 4.2L38.8 4.8L43.6 10.8L40.6 16.5Z", "#f6f2e4"),
                    ("M37.5 16.5L40.6 9.4L46.6 9.8L47.2 16.5Z", "#e6eff8"),
                ];
                for (d, fill) in papers {
                    let _ = write!(
                        out,
                        r#"<path d="{d}" fill="{fill}" stroke="{}" stroke-width="{ow:.2}" stroke-linejoin="round"/>"#,
                        hex(edge)
                    );
                }
                if !small {
                    let _ = write!(
                        out,
                        r#"<path d="M21 10.6L24.6 12.8M33.2 7.6L36.4 11.6M42.4 11.6L44 14.6" stroke="{}" stroke-opacity="0.6" stroke-width="0.9" stroke-linecap="round"/>"#,
                        hex(edge)
                    );
                }
                // The front lip over the papers.
                let _ = write!(
                    out,
                    r#"<path d="M11.5 16.25H52.5Q52.5 20 48.75 20H15.25Q11.5 20 11.5 16.25Z" fill="url(#bin)" stroke="{}" stroke-width="{ow:.2}" stroke-linejoin="round"/>"#,
                    hex(edge)
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "finder_icons_tests.rs"]
mod tests;
