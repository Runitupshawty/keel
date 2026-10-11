//! Media metadata and sidecar images (spec 2.10 "Media"): EXIF via `kamadak-exif`, an XMP
//! packet scan (rating, keywords) via `quick-xml`, video facts via `ffprobe`, thumbnails via
//! `image` (EXIF orientation applied, lossless WebP) and video thumbstrips via `ffmpeg`.
//! Blocking; called from the sidecar job (or a worker), never the UI thread.

use anyhow::{Context, Result};
use image::{DynamicImage, ImageFormat, ImageReader, Limits};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{BufReader, Cursor, Read},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::Duration,
};
use wait_timeout::ChildExt;

/// Images bigger than this are not decoded (their thumbnail is recorded as an error).
pub const MAX_PIXELS: u64 = 200_000_000;
/// Longest side of [`crate::SidecarKind::Strip`] frames, and frames per strip.
pub const STRIP_FRAME_PX: u32 = 160;
pub const STRIP_FRAMES: u32 = 20;
/// How much of a file the XMP scan reads.
// ponytail: XMP after the first 2 MiB (rare: some TIFF/RAW writers append it) is missed;
// follow the TIFF XMP tag (700) if that matters.
const XMP_SCAN: u64 = 2 << 20;
/// Hard limit per ffmpeg/ffprobe run; a hung or very slow decode is killed.
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);
/// Checked after PATH: GUI apps launched from Finder/Explorer often get a minimal PATH.
const FALLBACK_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", r"C:\ffmpeg\bin"];

pub(crate) const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff", "heic", "heif", "avif", "dng",
    "cr2", "nef", "arw", "orf", "rw2", "pef",
];
pub(crate) const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "mov", "avi", "wmv", "webm", "m4v", "mpg", "mpeg", "flv", "3gp",
];

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaMeta {
    /// As displayed (after EXIF orientation / video rotation).
    pub width: u32,
    pub height: u32,
    /// EXIF orientation 1..=8 (1 when absent; videos are 1, their frames come out upright).
    pub orientation: u8,
    /// Unix seconds. EXIF times without an offset tag are taken as UTC.
    pub taken_at: Option<i64>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    /// (latitude, longitude), degrees, south and west negative.
    pub gps: Option<(f64, f64)>,
    pub duration_ms: Option<u64>,
    pub codec: Option<String>,
    /// XMP rating 0..=5.
    pub rating: Option<u8>,
    pub keywords: Vec<String>,
    /// Why this file's metadata could not be read (a corrupt file). Recorded once per
    /// sidecar key: it is not read again until the file changes.
    pub error: Option<String>,
    /// Image sidecars that cannot be made, by file name (`thumb-256.webp`, `strip.webp`):
    /// why. Each kind fails on its own (a strip that fails leaves the thumbnail usable);
    /// recorded once per key like `error`. Timeouts are not recorded here (see `timed_out`).
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub failed: std::collections::BTreeMap<String, String>,
    /// Video strips whose `ffmpeg` ran out of time, by file name (`strip.webp`): tried
    /// again only after [`crate::STRIP_RETRY`] or when the file changes
    /// ([`crate::strip_retry_due`]), or when asked (`Sidecars::retry`).
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub timed_out: std::collections::BTreeMap<String, TimedOutAt>,
}

/// When a sidecar timed out, and the file as it was then.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedOutAt {
    /// Unix seconds.
    pub at: i64,
    pub mtime: i64,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaType {
    Image,
    Video,
}

/// Image or video by file extension (what sidecars can be made for).
pub(crate) fn media_type(path: &Path) -> Option<MediaType> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        Some(MediaType::Image)
    } else if VIDEO_EXTENSIONS.contains(&ext.as_str()) {
        Some(MediaType::Video)
    } else {
        None
    }
}

/// The file's content cannot be decoded (corrupt, unsupported, oversized, a tool timed out
/// on it): recorded in `meta.json` and not retried until the file changes. Other errors (IO,
/// a missing tool) are transient.
#[derive(Debug)]
pub(crate) struct Corrupt(pub String);
impl std::fmt::Display for Corrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Corrupt {}

pub(crate) fn corrupt(e: impl std::fmt::Display) -> anyhow::Error {
    Corrupt(e.to_string()).into()
}

pub(crate) fn is_corrupt(e: &anyhow::Error) -> bool {
    e.is::<Corrupt>()
}

/// A tool ran past [`TOOL_TIMEOUT`]: maybe a slow disk or a busy machine, not the file's
/// fault, so not recorded as a failure (a strip's is remembered for a while, `timed_out`).
#[derive(Debug)]
pub(crate) struct TimedOut;
impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "timed out after {} s", TOOL_TIMEOUT.as_secs())
    }
}
impl std::error::Error for TimedOut {}

/// Truncated or malformed data is the file's fault; other IO errors (gone, locked) are not.
fn image_error(e: image::ImageError) -> anyhow::Error {
    use std::io::ErrorKind::{InvalidData, UnexpectedEof};
    match e {
        image::ImageError::IoError(io) if !matches!(io.kind(), UnexpectedEof | InvalidData) => {
            io.into()
        }
        e => corrupt(e),
    }
}

/// Reads a photo's or video's metadata (EXIF + XMP for images, ffprobe for videos).
pub fn read_meta(path: &Path) -> Result<MediaMeta> {
    match media_type(path) {
        Some(MediaType::Video) => video_meta(path),
        Some(MediaType::Image) => image_meta(path),
        None => anyhow::bail!("not an image or video: {}", path.display()),
    }
}

fn has_ext(path: &Path, exts: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

fn is_heif(path: &Path) -> bool {
    has_ext(path, &["heic", "heif"])
}

/// AVIF: decoded by ffmpeg (the `image` crate's AVIF decoder needs dav1d, a C library).
fn is_avif(path: &Path) -> bool {
    has_ext(path, &["avif"])
}

/// Camera RAW files shown by their embedded JPEG preview (the TIFF-based ones).
pub const RAW_EXTENSIONS: &[&str] = &["dng", "cr2", "nef", "arw", "orf", "rw2", "pef"];

fn is_raw(path: &Path) -> bool {
    has_ext(path, RAW_EXTENSIONS)
}

/// Photos the `image` crate cannot open, which [`decode_image`] converts: HEIC and HEIF
/// (ffmpeg, or libheif with feature `heic`), AVIF (ffmpeg) and camera RAW (the embedded
/// preview).
pub fn is_converted_photo(path: &Path) -> bool {
    is_heif(path) || is_avif(path) || is_raw(path)
}

fn image_meta(path: &Path) -> Result<MediaMeta> {
    let exif = read_exif(path)?;
    let mut meta = MediaMeta {
        orientation: 1,
        ..MediaMeta::default()
    };
    if let Some(exif) = &exif {
        apply_exif(&mut meta, exif);
    }
    if is_tiff(path) || is_raw(path) {
        if let Some(o) = tiff_orientation(path)? {
            meta.orientation = o;
        }
    }
    let (w, h) = if is_raw(path) {
        let p = raw_previews(path)?.first().copied();
        let p = p.ok_or_else(|| corrupt("no embedded preview"))?;
        (p.width, p.height)
    } else if is_heif(path) || is_avif(path) {
        let dims = converted_dimensions(path, exif.as_ref(), meta.orientation)?;
        // Decoded upright (ffmpeg and libheif turn the picture by the file's own rotation):
        // the size is as shown and no orientation applies on top.
        meta.orientation = 1;
        dims
    } else {
        ImageReader::open(path)?
            .with_guessed_format()?
            .into_dimensions()
            .map_err(image_error)?
    };
    (meta.width, meta.height) = if meta.orientation >= 5 {
        (h, w)
    } else {
        (w, h)
    };
    if let Some(xmp) = read_xmp(path)? {
        meta.rating = xmp.rating;
        meta.keywords = xmp.keywords;
    }
    Ok(meta)
}

/// The size as shown of a HEIC, HEIF or AVIF file: from libheif (HEIC with feature
/// `heic`), else from EXIF (turned by the EXIF `orientation`), else from ffprobe (the
/// picture less its cropping, turned by its rotation). Without any of them it is unknown,
/// an error that is not recorded (installing ffmpeg brings it).
// ponytail: a tiled HEIF without EXIF sizes would report one tile's size from ffprobe
// (phone photos carry EXIF sizes); read ffprobe's tile-grid stream group if that shows up.
fn converted_dimensions(
    path: &Path,
    exif: Option<&exif::Exif>,
    orientation: u8,
) -> Result<(u32, u32)> {
    #[cfg(feature = "heic")]
    if is_heif(path) {
        let ctx =
            libheif_rs::HeifContext::read_from_file(&path.to_string_lossy()).map_err(corrupt)?;
        let handle = ctx.primary_image_handle().map_err(corrupt)?;
        return Ok((handle.width(), handle.height()));
    }
    let dim = |tag| {
        exif?
            .get_field(tag, exif::In::PRIMARY)
            .and_then(|f| f.value.get_uint(0))
            .filter(|v| *v > 0)
    };
    if let (Some(w), Some(h)) = (
        dim(exif::Tag::PixelXDimension),
        dim(exif::Tag::PixelYDimension),
    ) {
        return Ok(if orientation >= 5 { (h, w) } else { (w, h) });
    }
    if find_tool("ffprobe").is_none() {
        return Err(NeedsFfmpeg.into());
    }
    displayed_size(&ffprobe_json(path)?).ok_or_else(|| corrupt("no picture in the file"))
}

/// A still picture's size as shown, from ffprobe JSON: the coded size less the cropping
/// (HEIF pictures are coded in whole blocks), turned by the rotation.
pub(crate) fn displayed_size(v: &serde_json::Value) -> Option<(u32, u32)> {
    let s = v["streams"]
        .as_array()?
        .iter()
        .find(|s| s["codec_type"] == "video")?;
    let side = s["side_data_list"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let crop = |k: &str| side.iter().find_map(|d| d[k].as_u64()).unwrap_or(0);
    let w = s["width"]
        .as_u64()?
        .checked_sub(crop("crop_left").saturating_add(crop("crop_right")))?;
    let h = s["height"]
        .as_u64()?
        .checked_sub(crop("crop_top").saturating_add(crop("crop_bottom")))?;
    let (w, h) = (u32::try_from(w).ok()?, u32::try_from(h).ok()?);
    let turned = side
        .iter()
        .find_map(|d| d["rotation"].as_i64())
        .is_some_and(|r| r.rem_euclid(180) == 90);
    (w > 0 && h > 0).then_some(if turned { (h, w) } else { (w, h) })
}

/// TIFFs bigger than this are not searched for EXIF: kamadak-exif reads a whole TIFF
/// into memory first (other containers are scanned). Their orientation still comes from
/// [`tiff_orientation`].
const TIFF_EXIF_MAX: u64 = 64 << 20;

fn is_tiff(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"))
}

/// Most entries read from a TIFF's first IFD (each 12 bytes, 20 in a BigTIFF).
const TIFF_IFD_MAX: u64 = 4096;

/// The orientation (tag 0x0112, 1..=8) in a TIFF's first IFD, read from the header and
/// that one directory (a few KiB, whatever the file's size): little- and big-endian,
/// classic and BigTIFF. None when the file is not a TIFF or has no valid tag.
pub(crate) fn tiff_orientation(path: &Path) -> Result<Option<u8>> {
    use std::io::{Seek, SeekFrom};
    let mut file = File::open(path)?;
    let mut head = [0u8; 16];
    if file.read_exact(&mut head[..8]).is_err() {
        return Ok(None);
    }
    let le = match &head[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return Ok(None),
    };
    let u16_at = |b: &[u8]| {
        let b = [b[0], b[1]];
        if le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        }
    };
    let u32_at = |b: &[u8]| {
        let b = [b[0], b[1], b[2], b[3]];
        if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        }
    };
    let u64_at = |b: &[u8]| {
        let b: [u8; 8] = b[..8].try_into().expect("8 bytes");
        if le {
            u64::from_le_bytes(b)
        } else {
            u64::from_be_bytes(b)
        }
    };
    // (count field size, entry size, value offset in an entry, first IFD offset)
    let (count_len, entry_len, value_at, ifd) = match u16_at(&head[2..4]) {
        // TIFF, and the TIFF-like headers of Olympus ("RO", "RS") and Panasonic RAW files.
        42 | 0x4F52 | 0x5352 | 0x55 => (2usize, 12usize, 8usize, u64::from(u32_at(&head[4..8]))),
        43 => {
            if file.read_exact(&mut head[8..16]).is_err() {
                return Ok(None);
            }
            (8, 20, 12, u64_at(&head[8..16]))
        }
        _ => return Ok(None),
    };
    let mut count = [0u8; 8];
    file.seek(SeekFrom::Start(ifd))?;
    if file.read_exact(&mut count[..count_len]).is_err() {
        return Ok(None);
    }
    let n = if count_len == 2 {
        u64::from(u16_at(&count))
    } else {
        u64_at(&count)
    };
    let mut entries = vec![0u8; n.min(TIFF_IFD_MAX) as usize * entry_len];
    if file.read_exact(&mut entries).is_err() {
        return Ok(None);
    }
    for e in entries.chunks_exact(entry_len) {
        // SHORT (type 3): the first value sits at the start of the value field.
        if u16_at(&e[0..2]) == 0x0112 && u16_at(&e[2..4]) == 3 {
            let o = u16_at(&e[value_at..value_at + 2]);
            return Ok((1..=8).contains(&o).then_some(o as u8));
        }
    }
    Ok(None)
}

/// None when the file has no (readable) EXIF, or is a TIFF or RAW over [`TIFF_EXIF_MAX`].
fn read_exif(path: &Path) -> Result<Option<exif::Exif>> {
    let file = File::open(path)?;
    if (is_tiff(path) || is_raw(path)) && file.metadata()?.len() > TIFF_EXIF_MAX {
        return Ok(None);
    }
    let mut r = BufReader::new(file);
    match exif::Reader::new().read_from_container(&mut r) {
        Ok(e) => Ok(Some(e)),
        Err(exif::Error::Io(e)) if e.kind() != std::io::ErrorKind::UnexpectedEof => Err(e.into()),
        Err(_) => Ok(None),
    }
}

fn ascii(exif: &exif::Exif, tag: exif::Tag) -> Option<String> {
    match &exif.get_field(tag, exif::In::PRIMARY)?.value {
        exif::Value::Ascii(v) => {
            let s = String::from_utf8_lossy(v.first()?);
            let s = s.trim_matches(|c: char| c == '\0' || c.is_whitespace());
            (!s.is_empty()).then(|| s.to_owned())
        }
        _ => None,
    }
}

fn apply_exif(meta: &mut MediaMeta, exif: &exif::Exif) {
    use exif::{In, Tag, Value};
    if let Some(o) = exif
        .get_field(Tag::Orientation, In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .filter(|o| (1..=8).contains(o))
    {
        meta.orientation = o as u8;
    }
    meta.taken_at = [
        (Tag::DateTimeOriginal, Tag::OffsetTimeOriginal),
        (Tag::DateTimeDigitized, Tag::OffsetTimeDigitized),
        (Tag::DateTime, Tag::OffsetTime),
    ]
    .into_iter()
    .find_map(|(when, offset)| {
        let mut dt = exif::DateTime::from_ascii(ascii(exif, when)?.as_bytes()).ok()?;
        if let Some(o) = ascii(exif, offset) {
            let _ = dt.parse_offset(o.as_bytes());
        }
        let t = unix_time(
            dt.year.into(),
            dt.month.into(),
            dt.day.into(),
            dt.hour.into(),
            dt.minute.into(),
            dt.second.into(),
        )?;
        Some(t - i64::from(dt.offset.unwrap_or(0)) * 60)
    });
    let make = ascii(exif, Tag::Make);
    let model = ascii(exif, Tag::Model);
    meta.camera = match (make, model) {
        (Some(make), Some(model)) if !model.to_lowercase().starts_with(&make.to_lowercase()) => {
            Some(format!("{make} {model}"))
        }
        (make, model) => model.or(make),
    };
    meta.lens = ascii(exif, Tag::LensModel);
    let coord = |tag, ref_tag, negative: &str| -> Option<f64> {
        let Value::Rational(v) = &exif.get_field(tag, In::PRIMARY)?.value else {
            return None;
        };
        let deg = v.first()?.to_f64()
            + v.get(1).map_or(0.0, |m| m.to_f64() / 60.0)
            + v.get(2).map_or(0.0, |s| s.to_f64() / 3600.0);
        let sign = if ascii(exif, ref_tag).is_some_and(|r| r.eq_ignore_ascii_case(negative)) {
            -1.0
        } else {
            1.0
        };
        deg.is_finite().then_some(sign * deg)
    };
    if let (Some(lat), Some(lon)) = (
        coord(Tag::GPSLatitude, Tag::GPSLatitudeRef, "S"),
        coord(Tag::GPSLongitude, Tag::GPSLongitudeRef, "W"),
    ) {
        meta.gps = Some((lat, lon));
    }
}

/// Unix seconds of a UTC civil time (None when out of range).
pub(crate) fn unix_time(y: i64, m: i64, d: i64, hh: i64, mm: i64, ss: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days from civil (H. Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400 + hh * 3600 + mm * 60 + ss)
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Xmp {
    pub(crate) rating: Option<u8>,
    pub(crate) keywords: Vec<String>,
}

/// The `<x:xmpmeta>` packet in the first [`XMP_SCAN`] bytes (JPEG APP1, PNG iTXt and TIFF
/// all embed it as plain XML), parsed.
fn read_xmp(path: &Path) -> Result<Option<Xmp>> {
    let mut buf = Vec::new();
    File::open(path)?.take(XMP_SCAN).read_to_end(&mut buf)?;
    Ok(find(&buf, b"<x:xmpmeta").and_then(|start| {
        let end = start + find(&buf[start..], b"</x:xmpmeta>")? + b"</x:xmpmeta>".len();
        parse_xmp(&String::from_utf8_lossy(&buf[start..end]))
    }))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Rating (`xmp:Rating`, attribute or element) and keywords (`dc:subject` list items).
/// None when the packet is not well-formed XML.
pub(crate) fn parse_xmp(xml: &str) -> Option<Xmp> {
    use quick_xml::events::Event;
    #[derive(PartialEq)]
    enum In {
        Nothing,
        Rating,
        Keyword,
    }
    fn parse_rating(s: &str) -> Option<u8> {
        s.trim()
            .parse::<f64>()
            .ok()
            .filter(|r| (0.0..=5.0).contains(r))
            .map(|r| r as u8)
    }
    fn rating_attr(e: &quick_xml::events::BytesStart, out: &mut Xmp) {
        for a in e.attributes().flatten() {
            if a.key.local_name().as_ref() == "Rating" {
                if let Ok(v) = a.normalized_value(quick_xml::XmlVersion::default()) {
                    out.rating = parse_rating(&v).or(out.rating);
                }
            }
        }
    }
    let mut out = Xmp::default();
    let mut reader = quick_xml::Reader::from_str(xml);
    let (mut subject, mut inside, mut text) = (false, In::Nothing, String::new());
    loop {
        match reader.read_event().ok()? {
            Event::Empty(e) => rating_attr(&e, &mut out),
            Event::Start(e) => {
                rating_attr(&e, &mut out);
                match e.local_name().as_ref() {
                    "Rating" => inside = In::Rating,
                    "subject" => subject = true,
                    "li" if subject => inside = In::Keyword,
                    _ => {}
                }
                text.clear();
            }
            Event::Text(t) if inside != In::Nothing => text.push_str(&t.xml10_content()),
            Event::GeneralRef(r) if inside != In::Nothing => {
                match r.resolve_char_ref().ok().flatten() {
                    Some(c) => text.push(c),
                    None => text.push_str(match &*r {
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        "quot" => "\"",
                        "apos" => "'",
                        _ => "",
                    }),
                }
            }
            Event::End(e) => {
                match (e.local_name().as_ref(), &inside) {
                    ("Rating", In::Rating) => out.rating = parse_rating(&text).or(out.rating),
                    ("li", In::Keyword) if !text.trim().is_empty() => {
                        out.keywords.push(text.trim().to_owned());
                    }
                    ("subject", _) => subject = false,
                    _ => {}
                }
                inside = In::Nothing;
            }
            Event::Eof => return Some(out),
            _ => {}
        }
    }
}

fn video_meta(path: &Path) -> Result<MediaMeta> {
    Ok(parse_probe(&ffprobe_json(path)?))
}

/// `ffprobe -show_format -show_streams` of `path` as JSON (10 s limit). Blocking.
pub fn ffprobe_json(path: &Path) -> Result<serde_json::Value> {
    let ffprobe = find_tool("ffprobe").context("ffprobe not found (install ffmpeg)")?;
    let mut command = Command::new(ffprobe);
    command
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path);
    let (status, stdout, stderr) = run(command)?;
    if !status.success() {
        return Err(corrupt(format!(
            "ffprobe: {}",
            String::from_utf8_lossy(&stderr).trim()
        )));
    }
    serde_json::from_slice(&stdout).map_err(corrupt)
}

/// MediaMeta from `ffprobe -print_format json -show_format -show_streams`.
pub fn parse_probe(v: &serde_json::Value) -> MediaMeta {
    let str_of = |v: &serde_json::Value| v.as_str().map(str::to_owned);
    let format = &v["format"];
    let tags = &format["tags"];
    let tag = |names: &[&str]| names.iter().find_map(|n| str_of(&tags[*n]));
    let mut meta = MediaMeta {
        orientation: 1,
        duration_ms: format["duration"]
            .as_str()
            .and_then(|d| d.parse::<f64>().ok())
            .filter(|d| d.is_finite() && *d >= 0.0)
            .map(|d| (d * 1000.0).round() as u64),
        taken_at: tag(&["creation_time", "com.apple.quicktime.creationdate"])
            .and_then(|t| iso_time(&t)),
        camera: tag(&["com.apple.quicktime.model", "model"]),
        gps: tag(&["com.apple.quicktime.location.ISO6709", "location"]).and_then(|l| iso6709(&l)),
        ..MediaMeta::default()
    };
    let streams = v["streams"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    // Cover art in mp4/mkv is a video stream too.
    if let Some(s) = streams.iter().find(|s| {
        s["codec_type"] == "video" && s["disposition"]["attached_pic"].as_i64() != Some(1)
    }) {
        let (w, h) = (
            s["width"].as_u64().unwrap_or(0) as u32,
            s["height"].as_u64().unwrap_or(0) as u32,
        );
        let rotation = s["side_data_list"]
            .as_array()
            .and_then(|l| l.iter().find_map(|d| d["rotation"].as_i64()))
            .or_else(|| s["tags"]["rotate"].as_str().and_then(|r| r.parse().ok()))
            .unwrap_or(0);
        (meta.width, meta.height) = if rotation.rem_euclid(180) == 90 {
            (h, w)
        } else {
            (w, h)
        };
        meta.codec = str_of(&s["codec_name"]);
    }
    meta
}

/// `2024-05-01T12:34:56.000000Z` (or with a `+hh:mm` offset) as unix seconds.
fn iso_time(s: &str) -> Option<i64> {
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let t = unix_time(
        n(0..4)?,
        n(5..7)?,
        n(8..10)?,
        n(11..13)?,
        n(14..16)?,
        n(17..19)?,
    )?;
    let rest = s
        .get(19..)?
        .trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset = match rest.as_bytes().first() {
        Some(sign @ (b'+' | b'-')) => {
            let hh: i64 = rest.get(1..3)?.parse().ok()?;
            let mm: i64 = rest.get(4..6).or(rest.get(3..5))?.parse().ok()?;
            (hh * 60 + mm) * 60 * if *sign == b'-' { -1 } else { 1 }
        }
        _ => 0,
    };
    Some(t - offset)
}

/// `+37.7749-122.4194+010.000/` as (lat, lon).
fn iso6709(s: &str) -> Option<(f64, f64)> {
    let mut parts = Vec::new();
    let mut start = 0;
    for (i, c) in s.char_indices().skip(1) {
        if c == '+' || c == '-' || c == '/' {
            parts.push(s.get(start..i)?.parse::<f64>().ok()?);
            start = i;
            if parts.len() == 2 {
                break;
            }
        }
    }
    match parts[..] {
        [lat, lon] => Some((lat, lon)),
        _ => None,
    }
}

/// Decodes an image with its orientation applied, refusing ones over [`MAX_PIXELS`]. The
/// [converted](is_converted_photo) kinds come at most `max` px on the longer side (ffmpeg
/// scales them); the others come full size.
pub fn decode_image(path: &Path, max: u32) -> Result<DynamicImage> {
    if is_heif(path) {
        return decode_heif(path, max);
    }
    if is_avif(path) {
        return ffmpeg_still(path, max);
    }
    if is_raw(path) {
        return raw_image(path);
    }
    let mut img = decode_bounded(|| Ok(ImageReader::open(path)?.with_guessed_format()?))?;
    img.apply_orientation(orientation_of(path)?);
    Ok(img)
}

/// Decodes what `open` reads, refusing pictures over [`MAX_PIXELS`] by their header.
fn decode_bounded<R: std::io::BufRead + std::io::Seek>(
    open: impl Fn() -> Result<ImageReader<R>>,
) -> Result<DynamicImage> {
    let (w, h) = open()?.into_dimensions().map_err(image_error)?;
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(corrupt(format!(
            "{w}x{h} is over {} MP: not decoded",
            MAX_PIXELS / 1_000_000
        )));
    }
    let mut reader = open()?;
    let mut limits = Limits::default();
    // Decoders allocate by what each frame declares, which can be far more than the header
    // says (a 52-byte GIF with a 4x3 screen and a 14693x14649 frame): hold them to the
    // header's size, with room for 16 bytes a pixel.
    limits.max_alloc = Some((u64::from(w) * u64::from(h) * 16).clamp(64 << 20, MAX_PIXELS * 8));
    reader.limits(limits);
    reader.decode().map_err(image_error)
}

/// The oldest ffmpeg major version that reads HEIF still images (with the tile grids of
/// phone photos) and AVIF image items.
pub const STILL_MIN_FFMPEG: u32 = 7;
/// The longest side ffmpeg is asked for (a photo is never decoded bigger than this).
const STILL_MAX_SIDE: u32 = 8192;

/// No ffmpeg on PATH for a photo only ffmpeg can decode. Not the file's fault.
#[derive(Debug)]
pub struct NeedsFfmpeg;
impl std::fmt::Display for NeedsFfmpeg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Install ffmpeg to see HEIC and AVIF photos")
    }
}
impl std::error::Error for NeedsFfmpeg {}

/// The picture of a HEIC, HEIF or AVIF file through ffmpeg, with the file's rotation and
/// mirroring applied (ffmpeg turns the frame by its `irot`/`imir`), at most `max` px on the
/// longer side (never enlarged). A failure with an ffmpeg older than [`STILL_MIN_FFMPEG`]
/// says so and is not recorded against the file (a newer ffmpeg may read it).
fn ffmpeg_still(path: &Path, max: u32) -> Result<DynamicImage> {
    let ffmpeg = find_tool("ffmpeg").ok_or(NeedsFfmpeg)?;
    let max = max.clamp(1, STILL_MAX_SIDE);
    let mut command = Command::new(&ffmpeg);
    command
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-frames:v", "1", "-vf"])
        .arg(format!(
            "scale=w='min(iw,{max})':h='min(ih,{max})':force_original_aspect_ratio=decrease"
        ))
        .args(["-f", "image2pipe", "-vcodec", "png", "-"]);
    let (status, stdout, stderr) = run(command)?;
    if status.success() && !stdout.is_empty() {
        return decode_bounded(|| {
            Ok(ImageReader::with_format(
                Cursor::new(stdout.as_slice()),
                ImageFormat::Png,
            ))
        });
    }
    let kind = if is_avif(path) { "AVIF" } else { "HEIC" };
    if let Some(v) = ffmpeg_major(&ffmpeg).filter(|v| *v < STILL_MIN_FFMPEG) {
        anyhow::bail!("{kind} photos need ffmpeg {STILL_MIN_FFMPEG} or newer (this one is {v})");
    }
    let err = String::from_utf8_lossy(&stderr).trim().to_owned();
    Err(corrupt(if err.is_empty() {
        format!("ffmpeg could not read this {kind} file")
    } else {
        err
    }))
}

/// The major version of `ffmpeg` (None for a git build or an unreadable answer).
fn ffmpeg_major(ffmpeg: &Path) -> Option<u32> {
    let mut command = Command::new(ffmpeg);
    command.arg("-version");
    let (_, stdout, _) = run(command).ok()?;
    parse_ffmpeg_major(String::from_utf8_lossy(&stdout).lines().next()?)
}

/// `ffmpeg version 6.1.1-3ubuntu5 ...` or `ffmpeg version n7.1 ...` as 6 or 7; None for a
/// git build (`N-113-g...`), which is newer than any release.
pub(crate) fn parse_ffmpeg_major(line: &str) -> Option<u32> {
    let v = line.strip_prefix("ffmpeg version ")?;
    let v = v.strip_prefix('n').unwrap_or(v);
    let digits: String = v.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn orientation_of(path: &Path) -> Result<image::metadata::Orientation> {
    if is_tiff(path) || is_raw(path) {
        let o = tiff_orientation(path)?.unwrap_or(1);
        return Ok(image::metadata::Orientation::from_exif(o)
            .unwrap_or(image::metadata::Orientation::NoTransforms));
    }
    let o = read_exif(path)?
        .and_then(|e| {
            e.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
                .and_then(|f| f.value.get_uint(0))
        })
        .unwrap_or(1);
    Ok(image::metadata::Orientation::from_exif(o as u8)
        .unwrap_or(image::metadata::Orientation::NoTransforms))
}

/// libheif applies the file's own rotation/mirroring, so EXIF orientation is not applied
/// again.
#[cfg(feature = "heic")]
fn decode_heif(path: &Path, _max: u32) -> Result<DynamicImage> {
    use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};
    let ctx = HeifContext::read_from_file(&path.to_string_lossy()).map_err(corrupt)?;
    let handle = ctx.primary_image_handle().map_err(corrupt)?;
    if u64::from(handle.width()) * u64::from(handle.height()) > MAX_PIXELS {
        return Err(corrupt("over the pixel limit: not decoded"));
    }
    let img = LibHeif::new()
        .decode(&handle, ColorSpace::Rgb(RgbChroma::Rgb), None)
        .map_err(corrupt)?;
    let planes = img.planes();
    let p = planes
        .interleaved
        .context("libheif returned no RGB plane")?;
    let row = p.width as usize * 3;
    let mut rgb = Vec::with_capacity(row * p.height as usize);
    for y in 0..p.height as usize {
        rgb.extend_from_slice(&p.data[y * p.stride..y * p.stride + row]);
    }
    let rgb = image::RgbImage::from_raw(p.width, p.height, rgb).context("HEIF plane size")?;
    Ok(DynamicImage::ImageRgb8(rgb))
}

/// Without libheif: ffmpeg.
#[cfg(not(feature = "heic"))]
fn decode_heif(path: &Path, max: u32) -> Result<DynamicImage> {
    ffmpeg_still(path, max)
}

/// Directories, SubIFD depth and entries per directory the RAW walk reads at most.
const RAW_MAX_IFDS: usize = 32;
const RAW_MAX_DEPTH: u32 = 3;
const RAW_IFD_ENTRIES: usize = 512;
/// SubIFD offsets read from one directory.
const RAW_MAX_SUBIFDS: u32 = 8;
/// The largest embedded preview read (bytes).
const RAW_PREVIEW_MAX: u64 = 64 << 20;
/// JPEG segments skipped looking for the frame header.
const JPEG_MAX_SEGMENTS: usize = 64;

/// A JPEG preview inside a RAW file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RawPreview {
    pub(crate) offset: u64,
    pub(crate) len: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// The JPEG previews embedded in a TIFF-based RAW file (DNG, CR2, NEF, ARW, ORF, RW2,
/// PEF) that `image` can decode (baseline or progressive, at most [`MAX_PIXELS`]), largest
/// picture first. Found through `JPEGInterchangeFormat` and its length, single JPEG strips
/// (compression 6 or 7) and Panasonic's `JpgFromRaw`, walking IFD0's chain and the SubIFDs
/// (at most [`RAW_MAX_IFDS`] directories, [`RAW_MAX_DEPTH`] deep, [`RAW_IFD_ENTRIES`]
/// entries each). Reads the directories and the JPEG headers only. Empty for other files.
pub(crate) fn raw_previews(path: &Path) -> Result<Vec<RawPreview>> {
    use std::io::{Seek, SeekFrom};
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut head = [0u8; 8];
    if file.read_exact(&mut head).is_err() {
        return Ok(Vec::new());
    }
    let le = match &head[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return Ok(Vec::new()),
    };
    let u16_of = |b: &[u8]| {
        let b = [b[0], b[1]];
        if le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        }
    };
    let u32_of = |b: &[u8]| {
        let b = [b[0], b[1], b[2], b[3]];
        if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        }
    };
    if !matches!(u16_of(&head[2..4]), 42 | 0x4F52 | 0x5352 | 0x55) {
        return Ok(Vec::new());
    }
    let mut found: Vec<(u64, u64)> = Vec::new();
    let mut todo = vec![(u64::from(u32_of(&head[4..8])), 0u32)];
    let mut seen = std::collections::HashSet::new();
    while let Some((ifd, depth)) = todo.pop() {
        if seen.len() >= RAW_MAX_IFDS || ifd < 8 || ifd >= size || !seen.insert(ifd) {
            continue;
        }
        file.seek(SeekFrom::Start(ifd))?;
        let mut count = [0u8; 2];
        if file.read_exact(&mut count).is_err() {
            continue;
        }
        let n = usize::from(u16_of(&count)).min(RAW_IFD_ENTRIES);
        let mut entries = vec![0u8; n * 12 + 4];
        let got = file
            .by_ref()
            .take(entries.len() as u64)
            .read(&mut entries)?;
        let n = n.min(got / 12);
        let next = (got >= n * 12 + 4).then(|| u32_of(&entries[n * 12..]));
        let mut jpeg = (None, None);
        let mut strip = (None, None, 0u32);
        let mut compression = 0;
        for e in entries[..n * 12].as_chunks::<12>().0 {
            let (tag, kind, count) = (u16_of(&e[0..2]), u16_of(&e[2..4]), u32_of(&e[4..8]));
            // The first value of a SHORT entry, else the 32-bit value or offset field.
            let value = if kind == 3 {
                u32::from(u16_of(&e[8..10]))
            } else {
                u32_of(&e[8..12])
            };
            match tag {
                0x0103 => compression = value,
                0x0111 => (strip.0, strip.2) = (Some(value), count),
                0x0117 => strip.1 = Some(value),
                0x0201 => jpeg.0 = Some(value),
                0x0202 => jpeg.1 = Some(value),
                // Panasonic's JpgFromRaw: the entry's data is a whole JPEG.
                0x002E if kind == 7 => found.push((u64::from(value), u64::from(count))),
                0x014A if depth < RAW_MAX_DEPTH && count == 1 => {
                    todo.push((u64::from(value), depth + 1));
                }
                0x014A if depth < RAW_MAX_DEPTH => {
                    let mut offsets = vec![0u8; count.min(RAW_MAX_SUBIFDS) as usize * 4];
                    let here = file.stream_position()?;
                    file.seek(SeekFrom::Start(u64::from(value)))?;
                    if file.read_exact(&mut offsets).is_ok() {
                        for o in offsets.as_chunks::<4>().0 {
                            todo.push((u64::from(u32_of(o)), depth + 1));
                        }
                    }
                    file.seek(SeekFrom::Start(here))?;
                }
                _ => {}
            }
        }
        if let (Some(at), Some(len)) = jpeg {
            found.push((u64::from(at), u64::from(len)));
        }
        if let (Some(at), Some(len), 1) = strip {
            if matches!(compression, 6 | 7) {
                found.push((u64::from(at), u64::from(len)));
            }
        }
        if let Some(next) = next {
            todo.push((u64::from(next), depth));
        }
    }
    found.sort_unstable();
    found.dedup();
    let mut previews = Vec::new();
    for (offset, len) in found {
        if !(2..=RAW_PREVIEW_MAX).contains(&len) || offset.saturating_add(len) > size {
            continue;
        }
        if let Some((width, height)) = jpeg_frame(&mut file, offset, len)? {
            if u64::from(width) * u64::from(height) <= MAX_PIXELS {
                previews.push(RawPreview {
                    offset,
                    len,
                    width,
                    height,
                });
            }
        }
    }
    previews.sort_by_key(|p| std::cmp::Reverse(u64::from(p.width) * u64::from(p.height)));
    Ok(previews)
}

/// Width and height from the frame header of the JPEG at `offset`, when it is baseline,
/// extended or progressive (SOF0 to SOF2, what `image` decodes; not lossless or
/// arithmetic-coded ones, which RAW files use for their sensor data).
fn jpeg_frame(file: &mut File, offset: u64, len: u64) -> Result<Option<(u32, u32)>> {
    use std::io::{Seek, SeekFrom};
    let end = offset + len;
    let mut read_at = |at: u64, buf: &mut [u8]| -> Result<bool> {
        if at + buf.len() as u64 > end {
            return Ok(false);
        }
        file.seek(SeekFrom::Start(at))?;
        Ok(file.read_exact(buf).is_ok())
    };
    let mut soi = [0u8; 2];
    if !read_at(offset, &mut soi)? || soi != [0xFF, 0xD8] {
        return Ok(None);
    }
    let mut at = offset + 2;
    for _ in 0..JPEG_MAX_SEGMENTS {
        let mut seg = [0u8; 9];
        if !read_at(at, &mut seg[..4])? || seg[0] != 0xFF {
            return Ok(None);
        }
        match seg[1] {
            // A fill byte before the marker.
            0xFF => at += 1,
            0xC0..=0xC2 => {
                if !read_at(at, &mut seg)? {
                    return Ok(None);
                }
                let h = u32::from(u16::from_be_bytes([seg[5], seg[6]]));
                let w = u32::from(u16::from_be_bytes([seg[7], seg[8]]));
                return Ok((w > 0 && h > 0).then_some((w, h)));
            }
            // Other frame types, or the scan or the end before any frame.
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF | 0xD9 | 0xDA => return Ok(None),
            _ => at += 2 + u64::from(u16::from_be_bytes([seg[2], seg[3]])),
        }
    }
    Ok(None)
}

/// The largest embedded preview of a RAW file that decodes, with the file's orientation
/// applied.
fn raw_image(path: &Path) -> Result<DynamicImage> {
    use std::io::{Seek, SeekFrom};
    let previews = raw_previews(path)?;
    let mut last = corrupt("no embedded preview");
    let mut file = File::open(path)?;
    for p in previews.iter().take(3) {
        let mut jpeg = vec![0u8; p.len as usize];
        file.seek(SeekFrom::Start(p.offset))?;
        file.read_exact(&mut jpeg)?;
        let decoded = decode_bounded(|| {
            Ok(ImageReader::with_format(
                Cursor::new(jpeg.as_slice()),
                ImageFormat::Jpeg,
            ))
        });
        match decoded {
            Ok(mut img) => {
                img.apply_orientation(orientation_of(path)?);
                return Ok(img);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Fits `img` within `max` x `max` (never enlarging) and encodes it as lossless WebP (the
/// `image` crate's pure-Rust encoder; lossy WebP would need libwebp).
pub(crate) fn webp(img: DynamicImage, max: u32) -> Result<Vec<u8>> {
    let img = if img.width() > max || img.height() > max {
        img.thumbnail(max, max)
    } else {
        img
    };
    let img = if img.color().has_alpha() {
        DynamicImage::ImageRgba8(img.to_rgba8())
    } else {
        DynamicImage::ImageRgb8(img.to_rgb8())
    };
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::WebP)?;
    Ok(out.into_inner())
}

/// A WebP thumbnail with the longest side at most `max` px.
pub(crate) fn thumbnail(path: &Path, max: u32) -> Result<Vec<u8>> {
    let img = match media_type(path) {
        Some(MediaType::Video) => video_frame(path)?,
        _ => decode_image(path, max)?,
    };
    webp(img, max)
}

/// One frame (at 1 s, or the first for shorter clips), full size.
fn video_frame(path: &Path) -> Result<DynamicImage> {
    let ffmpeg = find_tool("ffmpeg").context("ffmpeg not found")?;
    let mut last_error = String::from("ffmpeg produced no frame");
    // `-ss 1` before `-i` yields no frame for clips shorter than a second: retry at 0.
    for seek in ["1", "0"] {
        let mut command = Command::new(&ffmpeg);
        command
            .args(["-v", "error", "-nostdin", "-y", "-ss", seek, "-i"])
            .arg(path)
            .args(["-frames:v", "1", "-f", "image2pipe", "-vcodec", "png", "-"]);
        let (status, stdout, stderr) = run(command)?;
        if status.success() && !stdout.is_empty() {
            return image::load_from_memory(&stdout).map_err(corrupt);
        }
        let stderr = String::from_utf8_lossy(&stderr).trim().to_owned();
        if !stderr.is_empty() {
            last_error = stderr;
        }
    }
    Err(corrupt(last_error))
}

/// [`STRIP_FRAMES`] frames evenly over `duration_ms`, [`STRIP_FRAME_PX`] wide, tiled left to
/// right, as WebP.
pub(crate) fn strip(path: &Path, duration_ms: Option<u64>) -> Result<Vec<u8>> {
    let ffmpeg = find_tool("ffmpeg").context("ffmpeg not found")?;
    let secs = (duration_ms.unwrap_or(0) as f64 / 1000.0).max(0.1);
    let mut command = Command::new(ffmpeg);
    command.args(["-v", "error", "-nostdin", "-y"]);
    // ponytail: past a minute only keyframes are decoded so long videos fit in the 10 s
    // limit; a feature film can still time out (retried after a week). Per-frame seeks
    // (20 runs of `-ss t -frames:v 1`) if that bites.
    if secs > 60.0 {
        command.args(["-skip_frame", "nokey"]);
    }
    command.arg("-i").arg(path).args([
        "-vf",
        &format!("fps={STRIP_FRAMES}/{secs:.3},scale={STRIP_FRAME_PX}:-1,tile={STRIP_FRAMES}x1"),
        "-frames:v",
        "1",
        "-f",
        "image2pipe",
        "-vcodec",
        "png",
        "-",
    ]);
    let (status, stdout, stderr) = run(command)?;
    if !status.success() || stdout.is_empty() {
        let err = String::from_utf8_lossy(&stderr).trim().to_owned();
        return Err(corrupt(if err.is_empty() {
            "ffmpeg produced no strip".to_owned()
        } else {
            err
        }));
    }
    let img = image::load_from_memory(&stdout).map_err(corrupt)?;
    webp(img, u32::MAX)
}

/// Whether ffmpeg (for video sidecars) can be found.
pub fn ffmpeg_available() -> bool {
    find_tool("ffmpeg").is_some()
}

/// First `name` on PATH, then in FALLBACK_DIRS. On Windows `.cmd`/`.bat` count too (shims).
/// Same lookup as keel-preview's video renderer.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let extensions: &[&str] = if cfg!(windows) {
        &["exe", "cmd", "bat"]
    } else {
        &[""]
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(FALLBACK_DIRS.iter().map(PathBuf::from))
        .flat_map(|dir| {
            extensions
                .iter()
                .map(move |ext| dir.join(name).with_extension(ext))
        })
        .find(|candidate| candidate.is_file())
}

/// Runs with piped output and a hard [`TOOL_TIMEOUT`] (no console window on Windows); a
/// timeout is [`TimedOut`] (not a failure of the file).
fn run(mut command: Command) -> Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn()?;
    // Drain both pipes on threads so a chatty tool cannot fill a pipe and stall.
    fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buffer);
            }
            buffer
        })
    }
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    match child.wait_timeout(TOOL_TIMEOUT) {
        Ok(Some(status)) => Ok((
            status,
            stdout.join().unwrap_or_default(),
            stderr.join().unwrap_or_default(),
        )),
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            // The drain threads finish on their own once every holder of the pipes exits.
            Err(TimedOut.into())
        }
        Err(e) => {
            let _ = child.kill();
            Err(e.into())
        }
    }
}

#[cfg(test)]
#[path = "media_tests.rs"]
pub(crate) mod tests;
