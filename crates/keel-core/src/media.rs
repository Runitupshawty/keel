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
    "jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff", "heic", "heif",
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
    /// recorded once per key like `error`. Timeouts are not recorded (retried).
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub failed: std::collections::BTreeMap<String, String>,
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
/// fault, so not recorded (tried again next time).
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

fn is_heif(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("heic") || e.eq_ignore_ascii_case("heif"))
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
    let dims = match ImageReader::open(path)?
        .with_guessed_format()?
        .into_dimensions()
    {
        Ok(d) => Some(d),
        Err(_) if is_heif(path) => heif_dimensions(path, exif.as_ref())?,
        Err(e) => return Err(image_error(e)),
    };
    let (w, h) = dims.context("no dimensions")?;
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

/// HEIF dimensions: from libheif with feature `heic`, else from EXIF (None: unknown, an
/// error that is not recorded so a build with `heic` retries it).
#[cfg(not(feature = "heic"))]
fn heif_dimensions(_path: &Path, exif: Option<&exif::Exif>) -> Result<Option<(u32, u32)>> {
    let dim = |tag| {
        exif?
            .get_field(tag, exif::In::PRIMARY)
            .and_then(|f| f.value.get_uint(0))
    };
    match (
        dim(exif::Tag::PixelXDimension),
        dim(exif::Tag::PixelYDimension),
    ) {
        (Some(w), Some(h)) => Ok(Some((w, h))),
        _ => anyhow::bail!("HEIC needs Keel built with feature `heic`"),
    }
}

#[cfg(feature = "heic")]
fn heif_dimensions(path: &Path, _exif: Option<&exif::Exif>) -> Result<Option<(u32, u32)>> {
    let ctx = libheif_rs::HeifContext::read_from_file(&path.to_string_lossy()).map_err(corrupt)?;
    let handle = ctx.primary_image_handle().map_err(corrupt)?;
    Ok(Some((handle.width(), handle.height())))
}

/// TIFFs bigger than this are not searched for EXIF: kamadak-exif reads a whole TIFF
/// into memory first (other containers are scanned).
const TIFF_EXIF_MAX: u64 = 64 << 20;

/// None when the file has no (readable) EXIF, or is a TIFF over [`TIFF_EXIF_MAX`].
fn read_exif(path: &Path) -> Result<Option<exif::Exif>> {
    let file = File::open(path)?;
    let tiff = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"));
    if tiff && file.metadata()?.len() > TIFF_EXIF_MAX {
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
    let v: serde_json::Value = serde_json::from_slice(&stdout).map_err(corrupt)?;
    Ok(parse_probe(&v))
}

/// MediaMeta from `ffprobe -print_format json -show_format -show_streams`.
pub(crate) fn parse_probe(v: &serde_json::Value) -> MediaMeta {
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

/// Decodes an image with its EXIF orientation applied, refusing ones over [`MAX_PIXELS`].
pub(crate) fn decode_image(path: &Path) -> Result<DynamicImage> {
    if is_heif(path) {
        return decode_heif(path);
    }
    let (w, h) = ImageReader::open(path)?
        .with_guessed_format()?
        .into_dimensions()
        .map_err(image_error)?;
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(corrupt(format!(
            "{w}x{h} is over {} MP: not decoded",
            MAX_PIXELS / 1_000_000
        )));
    }
    let mut reader = ImageReader::open(path)?.with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_PIXELS * 8);
    reader.limits(limits);
    let mut img = reader.decode().map_err(image_error)?;
    img.apply_orientation(orientation_of(path)?);
    Ok(img)
}

fn orientation_of(path: &Path) -> Result<image::metadata::Orientation> {
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
fn decode_heif(path: &Path) -> Result<DynamicImage> {
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

#[cfg(not(feature = "heic"))]
fn decode_heif(_path: &Path) -> Result<DynamicImage> {
    anyhow::bail!("HEIC needs Keel built with feature `heic`")
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
        _ => decode_image(path)?,
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
    // limit; a feature film can still time out (retried next time). Per-frame seeks
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
fn find_tool(name: &str) -> Option<PathBuf> {
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
/// timeout is [`TimedOut`] (tried again next time, not recorded).
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
