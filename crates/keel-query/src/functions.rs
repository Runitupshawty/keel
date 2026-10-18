//! Search functions (`size:`, `dm:`, `ext:`, ...): the ones this crate evaluates (item
//! properties, tree functions over parent links, set functions over the whole result), the
//! date functions and their component forms, the built-in filter macros, the UI-state
//! functions (recorded for the search UI) and every other Everything function, which is
//! refused with "not yet supported" and the task that adds it.

use crate::props::DateProp;

/// A text property.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextProp {
    /// The name.
    Name,
    /// The name without its extension (folders: the whole name).
    Stem,
    /// The extension contains the text.
    Extension,
    /// The whole extension (`ext:jpg;png`).
    Ext,
    /// The full path.
    FullPath,
    /// The exact parent folder path.
    Parent,
    /// The parent folder's name.
    ParentName,
    /// The file type description.
    Type,
    /// Any folder above the item: its full path (`ancestor:`).
    Ancestor,
    /// Any folder above the item: its name (`ancestor-name:`).
    AncestorName,
    /// Any of the file's names (hard links), full paths.
    HardLinkNames,
    /// Any of the file's alternate data stream names.
    StreamNames,
    /// Any path inside an archive.
    ContainerNames,
}

impl TextProp {
    /// Properties with several values; a term matches when any value does.
    pub fn is_list(self) -> bool {
        matches!(
            self,
            TextProp::Ancestor
                | TextProp::AncestorName
                | TextProp::HardLinkNames
                | TextProp::StreamNames
                | TextProp::ContainerNames
        )
    }
}

/// A number property.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NumProp {
    Size,
    SizeOnDisk,
    NameLength,
    FullPathLength,
    ParentCount,
    ChildCount,
    ChildFileCount,
    ChildFolderCount,
    RunCount,
    FileId,
    ParentFileId,
    DescendantCount,
    DescendantFileCount,
    DescendantFolderCount,
    HardLinkCount,
    StreamCount,
    ContainerFileCount,
}

/// A component of a date (`dmyear:`, `dcdow:`, `date-accessed-time:`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DatePart {
    /// The local date, ignoring the time.
    Date,
    /// Day of the month, 1..31.
    Day,
    /// Day of the week: names, or 0 (Sunday) ..6.
    DayOfWeek,
    /// Hour, 0..23.
    Hour,
    /// Month: names, or 1..12.
    Month,
    /// Time of day.
    Time,
    /// Year.
    Year,
}

/// What a function does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnKind {
    Text(TextProp),
    Number(NumProp),
    Date(DateProp, Option<DatePart>),
    Attributes,
    /// Items without a parent folder.
    Root,
    /// Folders with no files or subfolders.
    Empty,
    /// Keel: `kind:image`, `kind:folder`, ...
    Kind,
    /// Keel: under a folder (`in:C:\work`).
    In,
    /// A filter macro: files with these extensions (`audio:`, `zip:`, ...).
    Macro(&'static str),
    /// An item related through the tree has a matching name (`child:`, `sibling:`);
    /// `Some(true)`: folders only, `Some(false)`: files only.
    Related(Rel, Option<bool>),
    /// A name relative to the item's folder, or a full path, is in the index (`exists:`);
    /// `Some(true)`: a folder, `Some(false)`: a file.
    Exists(Option<bool>),
    /// A function evaluated on the parent folder (`parentsize:`, `parentdm:`).
    OfParent(&'static FnKind),
    /// The full path is one of these (`pathlist:`).
    PathList,
    /// A function over the whole result set (`dupe:`, `unique:`, `count:`).
    Set(SetFn),
    /// A UI-state function (`sort:`, `columns:`, ...): recorded, never filters.
    Ui,
    /// Known, not evaluated yet: the reason (which task adds it).
    Unsupported(&'static str),
}

/// How a related item is found from the item, for [`FnKind::Related`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rel {
    /// A direct child of a folder.
    Child,
    /// Anything below a folder.
    Descendant,
    /// Another item in the same folder.
    Sibling,
    /// Another item in the parent folder's folder.
    ParentSibling,
    /// Another item next to any folder above the item.
    AncestorSibling,
}

/// Functions over the whole result set, applied after every item was matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetFn {
    /// Items that share these property values with another result (`dupe:size;name`).
    Dupe,
    /// How many items a duplicate group has.
    DupeCount,
    /// Duplicate groups with an item inside these folders.
    DupeFrom,
    /// Duplicate groups of at least this many items.
    DupeMin,
    /// Duplicate groups of at most this many items.
    DupeMax,
    /// The first result for each distinct value.
    Distinct,
    /// The first result for each distinct value, sorted by the value.
    DistinctSort,
    /// Results whose value no other result has.
    Unique,
    /// At most this many results.
    Count,
    /// Everything 1.4's `sizedupe:` and friends: `dupe:` on this property.
    Legacy(&'static str),
}

/// One row of the function table.
pub struct FunctionInfo {
    /// Canonical name first, then aliases.
    pub names: &'static [&'static str],
    pub kind: FnKind,
    pub summary: &'static str,
}

macro_rules! f {
    ($names:expr, $k:expr, $s:expr) => {
        FunctionInfo {
            names: $names,
            kind: $k,
            summary: $s,
        }
    };
}

use FnKind as K;
use NumProp as N;
use TextProp as T;

/// The functions this crate evaluates (dates are in [`DATES`]).
pub static FUNCTIONS: &[FunctionInfo] = &[
    f!(
        &["name"],
        K::Text(T::Name),
        "the name contains the text (wildcards: the whole name)"
    ),
    f!(
        &["stem"],
        K::Text(T::Stem),
        "the name without its extension (folders: the whole name)"
    ),
    f!(
        &["extension"],
        K::Text(T::Extension),
        "the extension contains the text"
    ),
    f!(
        &["ext"],
        K::Text(T::Ext),
        "the whole extension, `;` lists (`ext:jpg;png`), leading `.` ignored"
    ),
    f!(
        &["full-path", "fullpath"],
        K::Text(T::FullPath),
        "the full path contains the text"
    ),
    f!(
        &["full-path-length", "fullpathlen"],
        K::Number(N::FullPathLength),
        "length of the full path in UTF-16 units"
    ),
    f!(
        &["name-length", "namelen"],
        K::Number(N::NameLength),
        "length of the name in UTF-16 units"
    ),
    f!(
        &["parent", "infolder", "nosubfolders"],
        K::Text(T::Parent),
        "the exact parent folder (no subfolders); a trailing `\\` is ignored"
    ),
    f!(
        &["parent-name", "parentname"],
        K::Text(T::ParentName),
        "the parent folder's name contains the text"
    ),
    f!(
        &["parent-count", "depth", "parents"],
        K::Number(N::ParentCount),
        "the number of folders above the item"
    ),
    f!(
        &["parent-file-id", "parentfrn"],
        K::Number(N::ParentFileId),
        "the parent folder's file id (when the source has one)"
    ),
    f!(
        &["file-id", "frn"],
        K::Number(N::FileId),
        "the file id (NTFS file reference number, inode)"
    ),
    f!(
        &["root"],
        K::Root,
        "items with no parent folder (drives, `/`)"
    ),
    f!(
        &["type"],
        K::Text(T::Type),
        "the file type description (`Text Document`, `File folder`)"
    ),
    f!(
        &["kind"],
        K::Kind,
        "Keel: `folder`, `file`, `audio`, `video`, `image`, `document`, `executable`, `archive`"
    ),
    f!(
        &["in"],
        K::In,
        "Keel: the item is inside this folder, at any depth"
    ),
    f!(
        &["size"],
        K::Number(N::Size),
        "size in bytes; units and constants"
    ),
    f!(
        &["size-on-disk", "sizeondisk"],
        K::Number(N::SizeOnDisk),
        "allocated size in bytes"
    ),
    f!(
        &["child-count", "childcount"],
        K::Number(N::ChildCount),
        "files and folders directly inside a folder"
    ),
    f!(
        &["child-file-count", "childfilecount"],
        K::Number(N::ChildFileCount),
        "files directly inside a folder"
    ),
    f!(
        &["child-folder-count", "childfoldercount"],
        K::Number(N::ChildFolderCount),
        "folders directly inside a folder"
    ),
    f!(
        &["empty"],
        K::Empty,
        "folders with no files and no subfolders"
    ),
    f!(
        &["attributes", "attr", "attrib", "attribute"],
        K::Attributes,
        "has all these attributes: letters (`RHSA`) or a hex mask"
    ),
    f!(
        &["run-count", "runcount"],
        K::Number(N::RunCount),
        "how many times Keel opened the item"
    ),
    // Tree functions.
    f!(
        &["ancestor"],
        K::Text(T::Ancestor),
        "a folder above the item has this full path (wildcards allowed)"
    ),
    f!(
        &["ancestor-name"],
        K::Text(T::AncestorName),
        "a folder above the item has this name"
    ),
    f!(
        &["child"],
        K::Related(Rel::Child, None),
        "folders with a file or folder directly inside whose name matches"
    ),
    f!(
        &["child-file"],
        K::Related(Rel::Child, Some(false)),
        "folders with a file directly inside whose name matches"
    ),
    f!(
        &["child-folder"],
        K::Related(Rel::Child, Some(true)),
        "folders with a folder directly inside whose name matches"
    ),
    f!(
        &["descendant"],
        K::Related(Rel::Descendant, None),
        "folders with a file or folder anywhere below whose name matches"
    ),
    f!(
        &["descendant-file"],
        K::Related(Rel::Descendant, Some(false)),
        "folders with a file anywhere below whose name matches"
    ),
    f!(
        &["descendant-folder"],
        K::Related(Rel::Descendant, Some(true)),
        "folders with a folder anywhere below whose name matches"
    ),
    f!(
        &["descendant-count"],
        K::Number(N::DescendantCount),
        "files and folders anywhere below a folder"
    ),
    f!(
        &["descendant-file-count"],
        K::Number(N::DescendantFileCount),
        "files anywhere below a folder"
    ),
    f!(
        &["descendant-folder-count"],
        K::Number(N::DescendantFolderCount),
        "folders anywhere below a folder"
    ),
    f!(
        &["sibling"],
        K::Related(Rel::Sibling, None),
        "another file or folder in the same folder has a matching name"
    ),
    f!(
        &["sibling-file"],
        K::Related(Rel::Sibling, Some(false)),
        "another file in the same folder has a matching name"
    ),
    f!(
        &["sibling-folder"],
        K::Related(Rel::Sibling, Some(true)),
        "another folder in the same folder has a matching name"
    ),
    f!(
        &["parent-sibling"],
        K::Related(Rel::ParentSibling, None),
        "the parent folder has a sibling with a matching name"
    ),
    f!(
        &["ancestor-sibling"],
        K::Related(Rel::AncestorSibling, None),
        "a folder above the item has a sibling with a matching name"
    ),
    f!(
        &["exists"],
        K::Exists(None),
        "this name (next to the item; `..` and subfolders allowed) or full path is indexed"
    ),
    f!(
        &["file-exists"],
        K::Exists(Some(false)),
        "like `exists:`, and it is a file"
    ),
    f!(
        &["folder-exists"],
        K::Exists(Some(true)),
        "like `exists:`, and it is a folder"
    ),
    f!(
        &["parentsize"],
        K::OfParent(&K::Number(N::Size)),
        "the parent folder's size"
    ),
    f!(
        &["parentdm"],
        K::OfParent(&K::Date(DateProp::Modified, None)),
        "the parent folder's date modified"
    ),
    f!(
        &["pathlist"],
        K::PathList,
        "the full path is one of these (`;` list)"
    ),
    // Set functions.
    f!(
        &["dupe"],
        K::Set(SetFn::Dupe),
        "results that share these properties with another result (`dupe:size;name`; default `name`)"
    ),
    f!(
        &["dupe-count"],
        K::Set(SetFn::DupeCount),
        "duplicate groups of this many results (`dupe-count:>=3`)"
    ),
    f!(
        &["dupe-from"],
        K::Set(SetFn::DupeFrom),
        "duplicate groups with a result inside one of these folders"
    ),
    f!(
        &["dupe-min"],
        K::Set(SetFn::DupeMin),
        "duplicate groups of at least this many results (default 2)"
    ),
    f!(
        &["dupe-max"],
        K::Set(SetFn::DupeMax),
        "duplicate groups of at most this many results"
    ),
    f!(
        &["distinct"],
        K::Set(SetFn::Distinct),
        "the first result for each distinct value of these properties"
    ),
    f!(
        &["distinct-sort"],
        K::Set(SetFn::DistinctSort),
        "like `distinct:`, sorted by the value"
    ),
    f!(
        &["unique"],
        K::Set(SetFn::Unique),
        "results whose value of these properties no other result has"
    ),
    f!(
        &["count"],
        K::Set(SetFn::Count),
        "at most this many results"
    ),
    f!(
        &["sizedupe"],
        K::Set(SetFn::Legacy("size")),
        "Everything 1.4: `dupe:size`"
    ),
    f!(
        &["namepartdupe"],
        K::Set(SetFn::Legacy("stem")),
        "Everything 1.4: `dupe:stem`"
    ),
    f!(
        &["dmdupe"],
        K::Set(SetFn::Legacy("dm")),
        "Everything 1.4: `dupe:dm`"
    ),
    f!(
        &["dcdupe"],
        K::Set(SetFn::Legacy("dc")),
        "Everything 1.4: `dupe:dc`"
    ),
    f!(
        &["dadupe"],
        K::Set(SetFn::Legacy("da")),
        "Everything 1.4: `dupe:da`"
    ),
    f!(
        &["attribdupe"],
        K::Set(SetFn::Legacy("attributes")),
        "Everything 1.4: `dupe:attributes`"
    ),
    // Hard links, streams and containers: read lazily, unknown when the source has none.
    f!(
        &["hard-link-count"],
        K::Number(N::HardLinkCount),
        "how many names (hard links) the file has"
    ),
    f!(
        &["hard-link-filenames"],
        K::Text(T::HardLinkNames),
        "any of the file's names (full paths) matches"
    ),
    f!(
        &["alternate-data-stream-count", "adscount"],
        K::Number(N::StreamCount),
        "how many alternate data streams the file has"
    ),
    f!(
        &["alternate-data-stream-names", "adsnames"],
        K::Text(T::StreamNames),
        "any alternate data stream name matches"
    ),
    f!(
        &["container-file-count"],
        K::Number(N::ContainerFileCount),
        "how many files an archive holds"
    ),
    f!(
        &["container-filenames"],
        K::Text(T::ContainerNames),
        "any path inside an archive matches"
    ),
];

/// Date functions: names (canonical first), the property, and the short form that takes
/// component suffixes without a dash (`dmyear:`).
pub static DATES: &[(&[&str], DateProp, Option<&str>)] = &[
    (
        &["date-modified", "dm", "datemodified"],
        DateProp::Modified,
        Some("dm"),
    ),
    (
        &["date-created", "dc", "datecreated"],
        DateProp::Created,
        Some("dc"),
    ),
    (
        &["date-accessed", "da", "dateaccessed"],
        DateProp::Accessed,
        Some("da"),
    ),
    (
        &[
            "date-recently-changed",
            "rc",
            "recentchange",
            "recentlychanged",
        ],
        DateProp::RecentlyChanged,
        Some("rc"),
    ),
    (&["date-run", "dr", "daterun"], DateProp::Run, Some("dr")),
    (&["date-deleted"], DateProp::Deleted, None),
    (&["date-saved"], DateProp::Saved, None),
    (&["date-printed"], DateProp::Printed, None),
    (&["date-received"], DateProp::Received, None),
    (&["date-sent"], DateProp::Sent, None),
    (&["date-released"], DateProp::Released, None),
    (&["date-acquired"], DateProp::Acquired, None),
    (&["date-changed"], DateProp::Changed, None),
    (&["date-content-created"], DateProp::ContentCreated, None),
    (&["date-taken"], DateProp::Taken, None),
    (
        &["date-encoded", "date-media-created"],
        DateProp::Encoded,
        None,
    ),
];

/// Component suffixes (`-year`, or `year` after a short form).
pub static DATE_PARTS: &[(&str, DatePart)] = &[
    ("date", DatePart::Date),
    ("day-of-week", DatePart::DayOfWeek),
    ("dow", DatePart::DayOfWeek),
    ("day", DatePart::Day),
    ("hour", DatePart::Hour),
    ("month", DatePart::Month),
    ("time", DatePart::Time),
    ("year", DatePart::Year),
];

/// Built-in filter macros and Everything's default extension lists.
pub static FILTER_MACROS: &[(&[&str], &str)] = &[
    (
        &["audio"],
        "aac;ac3;aif;aifc;aiff;au;cda;dts;fla;flac;it;m1a;m2a;m3u;m4a;mid;midi;mka;mod;mp2;mp3;mpa;ogg;opus;ra;rmi;snd;spc;umx;voc;wav;wma;xm",
    ),
    (
        &["zip"],
        "7z;ace;arj;bz2;cab;gz;gzip;jar;r00;r01;r02;r03;r04;r05;r06;r07;r08;r09;r10;r11;r12;r13;r14;r15;r16;r17;r18;r19;r20;r21;r22;r23;r24;r25;r26;r27;r28;r29;rar;tar;tgz;xz;z;zip;zst",
    ),
    (
        &["doc"],
        "c;chm;cpp;csv;cxx;doc;docm;docx;dot;dotm;dotx;h;hpp;htm;html;hxx;ini;java;lua;md;mht;mhtml;odp;ods;odt;pdf;potm;potx;ppam;pps;ppsm;ppsx;ppt;pptm;pptx;rtf;sldm;sldx;thmx;txt;vsd;wpd;wps;wri;xlam;xls;xlsb;xlsm;xlsx;xltm;xltx;xml",
    ),
    (&["exe"], "bat;cmd;com;exe;msi;msp;ps1;scr"),
    (
        &["pic", "image"],
        "ani;avif;bmp;gif;heic;heif;ico;jfif;jpe;jpeg;jpg;pcx;png;psd;svg;tga;tif;tiff;webp;wmf",
    ),
    (
        &["video"],
        "3g2;3gp;3gp2;3gpp;amr;amv;asf;avi;bdmv;bik;d2v;divx;drc;dsa;dsm;dss;dsv;evo;f4v;flc;fli;flic;flv;hdmov;ifo;ivf;m1v;m2p;m2t;m2ts;m2v;m4b;m4p;m4v;mkv;mov;mp2v;mp4;mp4v;mpe;mpeg;mpg;mpls;mpv2;mpv4;mts;ogm;ogv;pss;pva;qt;ram;ratdvd;rm;rmm;rmvb;roq;rpm;smil;smk;swf;tp;tpr;ts;vob;vp6;webm;wm;wmp;wmv",
    ),
];

/// Everything functions recognised but not evaluated by this crate yet: names and the
/// reason shown in the error.
pub static UNSUPPORTED: &[(&[&str], &str)] = &[
    (
        &[
            "content", "utf8content", "ansicontent", "utf16content", "utf16becontent", "textplain",
        ],
        "a content function (Phase 15, Task 73; `utf8content:` will be `utf8:content:`)",
    ),
    (
        &[
            "perceived-type", "content-type", "document-content-type", "file-signature",
            "shortcut-target", "volume-label", "location", "loc", "computer", "url", "owner",
            "protected", "offline", "online", "offline-availability", "offline-status",
            "is-running", "shared-with", "status", "content-status", "character-encoding",
            "line-count", "word-count", "page-count", "slide-count", "total-editing-time", "md5",
            "sha1", "sha256", "crc32", "crc64", "crc", "digital-signature-name",
            "digital-signature-timestamp", "version", "product-name", "product-version", "company",
            "copyright", "trademarks", "description", "details", "system-index", "si",
        ],
        "a file property read from disk (Phase 15, Task 73)",
    ),
    (
        &[
            "width", "wide", "height", "high", "dimensions", "orientation", "bit-depth", "bpp",
            "horizontal-resolution", "vertical-resolution", "aspect-ratio", "camera-maker",
            "camera-model", "lens-model", "aperture", "f-stop", "max-aperture", "exposure-time",
            "exposure-bias", "exposure-program", "exif-version", "flash-mode", "focal-length",
            "35mm-focal-length", "iso-speed", "light-source", "metering-mode", "brightness",
            "contrast", "saturation", "white-balance", "subject-distance", "latitude", "longitude",
            "altitude",
        ],
        "an image property (Phase 15, Task 73)",
    ),
    (
        &[
            "album", "album-artist", "artist", "authors", "composer", "conductor", "director",
            "producer", "writer", "genre", "mood", "title", "subtitle", "track", "year",
            "beats-per-minute", "bpm", "initial-key", "length", "duration", "audio-bitrate",
            "audio-bits-per-sample", "audio-channels", "audio-format", "audio-sample-rate",
            "audio-track-count", "video-bitrate", "video-format", "video-track-count",
            "subtitle-track-count", "total-bitrate", "frame-count", "frame-rate", "fps", "language",
            "part-of-a-compilation", "part-of-set", "period", "encoded-by", "publisher",
            "parental-rating", "parental-rating-reason",
        ],
        "an audio or video property (Phase 15, Task 73)",
    ),
    (
        &[
            "subject", "comment", "category", "tags", "tag", "rating", "people", "last-author",
            "software", "from", "to",
        ],
        "a document property (Phase 15, Task 73)",
    ),
    (
        &["filelist", "filelist-filename", "filelist-full-path"],
        "a file list function (Phase 16)",
    ),
    (
        &["network-index-host"],
        "a network index function: Keel's search tab reads it (Settings → General → Search → Include paired devices)",
    ),
    (
        &["camera", "taken", "has", "contents", "source", "is", "w", "h"],
        "a library search term; keel-query does not handle it yet (the library keeps its own parser until Task 71's rewire)",
    ),
];

/// UI-state functions: the search UI applies them (Phase 16); they never filter.
pub static UI_FUNCTIONS: &[&str] = &[
    "columns",
    "add-columns",
    "addcol",
    "remove-columns",
    "layout",
    "add-layout",
    "remove-layout",
    "sort",
    "sort-ascending",
    "sort-descending",
    "sort-mix",
    "no-sort-mix",
    "folders-first",
    "no-folders-first",
    "folders-first-ascending",
    "folders-first-descending",
    "alternate-row-color",
    "no-alternate-row-color",
    "on-top",
    "no-on-top",
    "folders-sidebar",
    "no-folders-sidebar",
    "bookmarks-sidebar",
    "no-bookmarks-sidebar",
    "filters-sidebar",
    "no-filters-sidebar",
    "preview",
    "no-preview",
    "thumbnails",
    "max-threads",
];

/// The function called `name` (lower case, without `:` or `*`).
pub fn lookup(name: &str) -> Option<FnKind> {
    if let Some(f) = FUNCTIONS.iter().find(|f| f.names.contains(&name)) {
        return Some(f.kind);
    }
    if let Some(d) = date_lookup(name) {
        return Some(d);
    }
    if let Some((_, exts)) = FILTER_MACROS.iter().find(|(n, _)| n.contains(&name)) {
        return Some(K::Macro(exts));
    }
    if UI_FUNCTIONS.contains(&name) || is_regmatch(name) {
        return Some(K::Ui);
    }
    UNSUPPORTED
        .iter()
        .find(|(n, _)| n.contains(&name))
        .map(|(_, why)| K::Unsupported(why))
}

fn is_regmatch(name: &str) -> bool {
    ["regular-expression-match", "regmatch"].iter().any(|p| {
        name.strip_prefix(p)
            .is_some_and(|d| d.len() == 1 && d.as_bytes()[0].is_ascii_digit())
    })
}

fn date_lookup(name: &str) -> Option<FnKind> {
    for (names, prop, short) in DATES {
        if names.contains(&name) {
            return Some(K::Date(*prop, None));
        }
        for n in names.iter() {
            let Some(rest) = name.strip_prefix(n) else {
                continue;
            };
            let rest = match rest.strip_prefix('-') {
                Some(r) => r,
                None if Some(*n) == *short => rest,
                None => continue,
            };
            if let Some((_, part)) = DATE_PARTS.iter().find(|(p, _)| *p == rest) {
                return Some(K::Date(*prop, Some(*part)));
            }
        }
    }
    None
}

/// Every name [`lookup`] evaluates, for documentation checks: plain functions, date
/// functions with their component forms, and filter macros.
pub fn implemented_names() -> Vec<String> {
    let mut out: Vec<String> = FUNCTIONS
        .iter()
        .flat_map(|f| f.names.iter().map(|n| n.to_string()))
        .collect();
    for (names, _, short) in DATES {
        out.extend(names.iter().map(|n| n.to_string()));
        for (part, _) in DATE_PARTS {
            out.push(format!("{}-{part}", names[0]));
            if let Some(s) = short {
                out.push(format!("{s}{part}"));
            }
        }
    }
    for (names, _) in FILTER_MACROS {
        out.extend(names.iter().map(|n| n.to_string()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_components() {
        assert_eq!(
            lookup("dmyear"),
            Some(K::Date(DateProp::Modified, Some(DatePart::Year)))
        );
        assert_eq!(
            lookup("dm-year"),
            Some(K::Date(DateProp::Modified, Some(DatePart::Year)))
        );
        assert_eq!(
            lookup("date-created-day-of-week"),
            Some(K::Date(DateProp::Created, Some(DatePart::DayOfWeek)))
        );
        assert_eq!(
            lookup("dadow"),
            Some(K::Date(DateProp::Accessed, Some(DatePart::DayOfWeek)))
        );
        assert_eq!(
            lookup("date-taken-month"),
            Some(K::Date(DateProp::Taken, Some(DatePart::Month)))
        );
        assert_eq!(lookup("date-takenmonth"), None);
        assert_eq!(lookup("regmatch3"), Some(K::Ui));
        assert_eq!(lookup("sizedupe"), Some(K::Set(SetFn::Legacy("size"))));
        assert!(matches!(lookup("md5"), Some(K::Unsupported(_))));
    }

    #[test]
    fn names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for n in implemented_names() {
            assert!(seen.insert(n.clone()), "{n} twice");
        }
        for (names, _) in UNSUPPORTED {
            for n in names.iter() {
                assert!(seen.insert(n.to_string()), "{n} twice");
            }
        }
        for n in UI_FUNCTIONS {
            assert!(seen.insert(n.to_string()), "{n} twice");
        }
        for m in crate::modifiers::MODIFIERS {
            for n in m.names {
                // `file:`/`folder:` are modifiers only.
                assert!(!seen.contains(*n), "{n} is a function and a modifier");
            }
        }
    }
}
