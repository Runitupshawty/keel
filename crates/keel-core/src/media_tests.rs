use super::*;
use std::process::Command;

/// A 40x20 JPEG (left half red, right half blue) with EXIF orientation 6, camera, time,
/// GPS, and an XMP packet (rating 4, keywords "beach", "R&D").
pub(crate) fn exif_jpeg(path: &Path) {
    use exif::{experimental::Writer, Field, In, Rational, Tag, Value};
    let img = image::RgbImage::from_fn(40, 20, |x, _| {
        if x < 20 {
            image::Rgb([255, 0, 0])
        } else {
            image::Rgb([0, 0, 255])
        }
    });
    let mut jpeg = Cursor::new(Vec::new());
    img.write_to(&mut jpeg, ImageFormat::Jpeg).unwrap();
    let jpeg = jpeg.into_inner();

    let ascii = |s: &str| Value::Ascii(vec![s.as_bytes().to_vec()]);
    let dms = |d: u32, m: u32, s: u32| {
        Value::Rational(vec![
            Rational { num: d, denom: 1 },
            Rational { num: m, denom: 1 },
            Rational { num: s, denom: 1 },
        ])
    };
    let field = |tag, value| Field {
        tag,
        ifd_num: In::PRIMARY,
        value,
    };
    let fields = [
        field(Tag::Orientation, Value::Short(vec![6])),
        field(Tag::Make, ascii("Keel")),
        field(Tag::Model, ascii("Keel Cam 1")),
        field(Tag::LensModel, ascii("50mm")),
        field(Tag::DateTimeOriginal, ascii("2024:05:01 12:34:56")),
        field(Tag::OffsetTimeOriginal, ascii("+02:00")),
        field(Tag::GPSLatitudeRef, ascii("N")),
        field(Tag::GPSLatitude, dms(39, 30, 0)),
        field(Tag::GPSLongitudeRef, ascii("W")),
        field(Tag::GPSLongitude, dms(77, 15, 0)),
    ];
    let mut writer = Writer::new();
    for f in &fields {
        writer.push_field(f);
    }
    let mut tiff = Cursor::new(Vec::new());
    writer.write(&mut tiff, false).unwrap();
    let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmp:Rating="4"><dc:subject><rdf:Bag><rdf:li>beach</rdf:li><rdf:li>R&amp;D</rdf:li></rdf:Bag></dc:subject></rdf:Description></rdf:RDF></x:xmpmeta>"#;
    let app1 = |payload: &[u8]| {
        let mut seg = vec![0xff, 0xe1];
        seg.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        seg.extend_from_slice(payload);
        seg
    };
    let mut out = jpeg[..2].to_vec(); // SOI
    out.extend(app1(&[b"Exif\0\0".as_slice(), &tiff.into_inner()].concat()));
    out.extend(app1(
        &[b"http://ns.adobe.com/xap/1.0/\0".as_slice(), xmp.as_bytes()].concat(),
    ));
    out.extend_from_slice(&jpeg[2..]);
    std::fs::write(path, out).unwrap();
}

pub(crate) fn png(path: &Path, w: u32, h: u32) {
    image::RgbImage::from_fn(w, h, |x, y| image::Rgb([x as u8, y as u8, 128]))
        .save(path)
        .unwrap();
}

/// A 2 s 320x240 test clip, or None without ffmpeg.
pub(crate) fn clip(path: &Path) -> Option<()> {
    let ffmpeg = find_tool("ffmpeg")?;
    let ok = Command::new(ffmpeg)
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc=duration=2:size=320x240:rate=25")
        .args(["-c:v", "mpeg4", "-pix_fmt", "yuv420p"])
        .arg(path)
        .status()
        .ok()?
        .success();
    assert!(ok, "ffmpeg made no clip");
    Some(())
}

fn rgb(img: &DynamicImage, x: u32, y: u32) -> [u8; 3] {
    let p = img.to_rgb8().get_pixel(x, y).0;
    [p[0], p[1], p[2]]
}

#[test]
fn jpeg_exif_and_xmp_are_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("photo.JPG");
    exif_jpeg(&path);
    assert!(std::fs::metadata(&path).unwrap().len() < 4096);
    let meta = read_meta(&path).unwrap();
    assert_eq!((meta.width, meta.height), (20, 40), "as displayed");
    assert_eq!(meta.orientation, 6);
    assert_eq!(meta.camera.as_deref(), Some("Keel Cam 1"));
    assert_eq!(meta.lens.as_deref(), Some("50mm"));
    let local = unix_time(2024, 5, 1, 12, 34, 56).unwrap();
    assert_eq!(meta.taken_at, Some(local - 2 * 3600));
    let (lat, lon) = meta.gps.unwrap();
    assert!(
        (lat - 39.5).abs() < 1e-9 && (lon + 77.25).abs() < 1e-9,
        "{lat} {lon}"
    );
    assert_eq!(meta.rating, Some(4));
    assert_eq!(meta.keywords, ["beach", "R&D"]);
    assert_eq!(meta.error, None);
    assert_eq!(meta.duration_ms, None);
}

#[test]
fn thumbnails_apply_exif_orientation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("photo.jpg");
    exif_jpeg(&path);
    let img = decode_image(&path, u32::MAX).unwrap();
    assert_eq!((img.width(), img.height()), (20, 40));
    // Orientation 6 (rotate 90 degrees clockwise): the left (red) half ends up on top.
    let [r, _, b] = rgb(&img, 10, 5);
    assert!(r > 200 && b < 60, "top is red: {r} {b}");
    let [r, _, b] = rgb(&img, 10, 35);
    assert!(b > 200 && r < 60, "bottom is blue: {r} {b}");
    let thumb = image::load_from_memory(&thumbnail(&path, 256).unwrap()).unwrap();
    assert_eq!((thumb.width(), thumb.height()), (20, 40), "never enlarged");
    let [r, _, _] = rgb(&thumb, 10, 5);
    assert!(r > 200);
}

/// A 40 x 20 RGB TIFF, left half red and right half blue, with orientation 6, in either
/// byte order.
fn tiff(path: &std::path::Path, le: bool) {
    let (w, h) = (40u32, 20u32);
    let mut pixels = Vec::new();
    for _ in 0..h {
        for x in 0..w {
            pixels.extend(if x < w / 2 { [255, 0, 0] } else { [0, 0, 255] });
        }
    }
    let p16 = |v: u16| if le { v.to_le_bytes() } else { v.to_be_bytes() };
    let p32 = |v: u32| if le { v.to_le_bytes() } else { v.to_be_bytes() };
    // Header, pixels, the bits-per-sample values, then the IFD.
    let data = 8u32;
    let bits = data + pixels.len() as u32;
    let ifd = bits + 6;
    let mut out = Vec::new();
    out.extend(if le { *b"II" } else { *b"MM" });
    out.extend(p16(42));
    out.extend(p32(ifd));
    out.extend(&pixels);
    for _ in 0..3 {
        out.extend(p16(8));
    }
    // SHORT values sit left-aligned in the 4-byte value field.
    let short = |v: u16| {
        let mut f = p16(v).to_vec();
        f.extend([0, 0]);
        f
    };
    let entries: [(u16, u16, u32, Vec<u8>); 10] = [
        (256, 3, 1, short(w as u16)),
        (257, 3, 1, short(h as u16)),
        (258, 3, 3, p32(bits).to_vec()),
        (259, 3, 1, short(1)),
        (262, 3, 1, short(2)),
        (273, 4, 1, p32(data).to_vec()),
        (274, 3, 1, short(6)),
        (277, 3, 1, short(3)),
        (278, 3, 1, short(h as u16)),
        (279, 4, 1, p32(pixels.len() as u32).to_vec()),
    ];
    out.extend(p16(entries.len() as u16));
    for (tag, kind, count, value) in entries {
        out.extend(p16(tag));
        out.extend(p16(kind));
        out.extend(p32(count));
        out.extend(value);
    }
    out.extend(p32(0));
    std::fs::write(path, out).unwrap();
}

#[test]
fn tiff_orientation_comes_from_the_header_in_either_byte_order() {
    let dir = tempfile::tempdir().unwrap();
    for le in [true, false] {
        let path = dir.path().join(format!("photo-{le}.tif"));
        tiff(&path, le);
        assert_eq!(tiff_orientation(&path).unwrap(), Some(6), "le {le}");
        let meta = read_meta(&path).unwrap();
        assert_eq!(meta.orientation, 6);
        assert_eq!((meta.width, meta.height), (20, 40), "as displayed");
        let img = decode_image(&path, u32::MAX).unwrap();
        assert_eq!((img.width(), img.height()), (20, 40));
        // Rotated 90 degrees clockwise: the left (red) half ends up on top.
        let [r, _, b] = rgb(&img, 10, 5);
        assert!(r > 200 && b < 60, "top is red: {r} {b}");
        let [r, _, b] = rgb(&img, 10, 35);
        assert!(b > 200 && r < 60, "bottom is blue: {r} {b}");
    }
    // Not a TIFF, or cut off after the header: no orientation, no error.
    let junk = dir.path().join("junk.tif");
    std::fs::write(&junk, [b'I', b'I', 42, 0, 0xff, 0xff, 0xff, 0x7f]).unwrap();
    assert_eq!(tiff_orientation(&junk).unwrap(), None);
    std::fs::write(&junk, b"GIF89a").unwrap();
    assert_eq!(tiff_orientation(&junk).unwrap(), None);
}

#[test]
fn png_meta_and_thumbnail_sizes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wide.png");
    png(&path, 600, 300);
    let meta = read_meta(&path).unwrap();
    assert_eq!((meta.width, meta.height, meta.orientation), (600, 300, 1));
    assert_eq!(meta.taken_at, None);
    let webp = thumbnail(&path, 256).unwrap();
    assert_eq!(&webp[8..12], b"WEBP");
    let small = image::load_from_memory(&webp).unwrap();
    assert_eq!((small.width(), small.height()), (256, 128));
    let big = image::load_from_memory(&thumbnail(&path, 1024).unwrap()).unwrap();
    assert_eq!((big.width(), big.height()), (600, 300));
}

#[test]
fn corrupt_and_oversized_images_are_content_errors() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.jpg");
    std::fs::write(&bad, b"\xff\xd8\xff\xe0 definitely not a jpeg").unwrap();
    assert!(is_corrupt(&read_meta(&bad).unwrap_err()));
    assert!(is_corrupt(&thumbnail(&bad, 256).unwrap_err()));

    // A BMP header claiming 20000 x 20000 px (400 MP) and no pixels.
    let huge = dir.path().join("huge.bmp");
    let mut bmp = b"BM".to_vec();
    bmp.extend(54u32.to_le_bytes()); // file size (lies)
    bmp.extend([0; 4]);
    bmp.extend(54u32.to_le_bytes()); // pixel offset
    bmp.extend(40u32.to_le_bytes()); // BITMAPINFOHEADER
    bmp.extend(20_000i32.to_le_bytes());
    bmp.extend(20_000i32.to_le_bytes());
    bmp.extend(1u16.to_le_bytes());
    bmp.extend(24u16.to_le_bytes());
    bmp.extend([0; 24]);
    std::fs::write(&huge, bmp).unwrap();
    let err = thumbnail(&huge, 256).unwrap_err();
    assert!(
        is_corrupt(&err) && err.to_string().contains("200 MP"),
        "{err}"
    );

    let missing = dir.path().join("gone.png");
    assert!(
        !is_corrupt(&thumbnail(&missing, 256).unwrap_err()),
        "IO is transient"
    );
}

/// Fuzzing (fuzz/corpus/image_meta/gif-frame-larger-than-screen.gif): a 52-byte GIF whose
/// 4x3 screen holds a 14693x14649 frame passed the pixel limit (it checks the screen) and
/// had the decoder allocate 860 MB for the frame. Decoding is held to the declared size.
#[test]
fn a_gif_frame_larger_than_its_screen_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let gif = dir.path().join("frame.gif");
    let corpus = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fuzz/corpus/image_meta/");
    std::fs::copy(format!("{corpus}gif-frame-larger-than-screen.gif"), &gif).unwrap();
    assert!(is_corrupt(&thumbnail(&gif, 64).unwrap_err()));
    let small = dir.path().join("small.gif");
    DynamicImage::new_rgb8(4, 3).save(&small).unwrap();
    assert!(thumbnail(&small, 64).is_ok());
}

#[test]
fn xmp_rating_as_element_and_odd_packets() {
    let x = parse_xmp(
        "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:Description><xmp:Rating>5</xmp:Rating>\
         <dc:subject><rdf:Bag><rdf:li>a&#x20;b</rdf:li><rdf:li/><rdf:li>c</rdf:li></rdf:Bag>\
         </dc:subject><dc:title><rdf:Alt><rdf:li>not a keyword</rdf:li></rdf:Alt></dc:title>\
         </rdf:Description></x:xmpmeta>",
    )
    .unwrap();
    assert_eq!(
        x,
        Xmp {
            rating: Some(5),
            keywords: vec!["a b".into(), "c".into()]
        }
    );
    // Rejected (-1) is no rating.
    assert_eq!(parse_xmp("<x xmp:Rating='-1'/>").unwrap().rating, None);
    assert_eq!(parse_xmp("<x><y></x>"), None);
}

#[test]
fn civil_and_iso_times() {
    assert_eq!(unix_time(1970, 1, 1, 0, 0, 0), Some(0));
    assert_eq!(unix_time(2000, 3, 1, 0, 0, 0), Some(951_868_800));
    assert_eq!(unix_time(2024, 2, 29, 23, 59, 59), Some(1_709_251_199));
    assert_eq!(unix_time(2024, 13, 1, 0, 0, 0), None);
    assert_eq!(iso_time("1970-01-01T00:00:10.000000Z"), Some(10));
    assert_eq!(iso_time("1970-01-01T02:00:00+02:00"), Some(0));
    assert_eq!(iso_time("1970-01-01T00:00:00-0130"), Some(5400));
    assert_eq!(iso6709("+39.5000-077.2500+010.000/"), Some((39.5, -77.25)));
}

#[test]
fn probe_json_is_parsed() {
    let v = serde_json::json!({
        "streams": [
            {"codec_type": "audio", "codec_name": "aac"},
            {"codec_type": "video", "codec_name": "mjpeg", "width": 600, "height": 600,
             "disposition": {"attached_pic": 1}},
            {"codec_type": "video", "codec_name": "hevc", "width": 1920, "height": 1080,
             "side_data_list": [{"rotation": -90}], "disposition": {"attached_pic": 0}}
        ],
        "format": {"duration": "12.345600",
                   "tags": {"creation_time": "2024-05-01T10:34:56.000000Z",
                            "com.apple.quicktime.model": "iPhone 15",
                            "com.apple.quicktime.location.ISO6709": "+39.5000-077.2500+010.000/"}}
    });
    let meta = parse_probe(&v);
    assert_eq!((meta.width, meta.height), (1080, 1920), "rotated");
    assert_eq!(meta.codec.as_deref(), Some("hevc"));
    assert_eq!(meta.duration_ms, Some(12_346));
    assert_eq!(meta.taken_at, unix_time(2024, 5, 1, 10, 34, 56));
    assert_eq!(meta.camera.as_deref(), Some("iPhone 15"));
    assert_eq!(meta.gps, Some((39.5, -77.25)));
}

#[test]
fn video_meta_thumb_and_strip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clip.mp4");
    if clip(&path).is_none() {
        eprintln!("ffmpeg not found: skipped");
        return;
    }
    let meta = read_meta(&path).unwrap();
    assert_eq!((meta.width, meta.height), (320, 240));
    assert_eq!(meta.codec.as_deref(), Some("mpeg4"));
    let ms = meta.duration_ms.unwrap();
    assert!((1900..=2100).contains(&ms), "{ms}");
    let thumb = image::load_from_memory(&thumbnail(&path, 256).unwrap()).unwrap();
    assert_eq!((thumb.width(), thumb.height()), (256, 192));
    let strip = image::load_from_memory(&strip(&path, meta.duration_ms).unwrap()).unwrap();
    assert_eq!(
        (strip.width(), strip.height()),
        (STRIP_FRAMES * STRIP_FRAME_PX, 120)
    );
    // testsrc changes over time: the first and last frames differ.
    let (a, b) = (
        strip.crop_imm(0, 0, 160, 120).to_rgb8(),
        strip.crop_imm(19 * 160, 0, 160, 120).to_rgb8(),
    );
    assert_ne!(a, b);

    let bad = dir.path().join("bad.mp4");
    std::fs::write(&bad, b"not a video at all").unwrap();
    assert!(is_corrupt(&read_meta(&bad).unwrap_err()));
    assert!(is_corrupt(&thumbnail(&bad, 256).unwrap_err()));
}

#[test]
fn ffmpeg_version_lines() {
    let major = parse_ffmpeg_major;
    assert_eq!(
        major("ffmpeg version 6.1.1-3ubuntu5 Copyright (c)"),
        Some(6)
    );
    assert_eq!(major("ffmpeg version n7.1 Copyright"), Some(7));
    assert_eq!(
        major("ffmpeg version 9.0.1-full_build-www.gyan.dev"),
        Some(9)
    );
    assert_eq!(
        major("ffmpeg version N-113000-g1234abcd"),
        None,
        "git build"
    );
    assert_eq!(major("something else"), None);
}

/// A committed synthetic photo (keel-preview's fixtures, made as their README says).
pub(crate) fn photo_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../keel-preview/tests/fixtures")
        .join(name)
}

/// HEIC (also with an `irot` quarter turn) and AVIF decode through ffmpeg, scaled to the
/// asked size and upright. Skipped without ffmpeg.
#[test]
fn heic_and_avif_decode_through_ffmpeg() {
    if find_tool("ffmpeg").is_none() {
        eprintln!("ffmpeg not found: skipped");
        return;
    }
    for name in ["pattern.heic", "pattern.avif"] {
        let img = decode_image(&photo_fixture(name), 1024).unwrap();
        assert_eq!(
            (img.width(), img.height()),
            (64, 48),
            "{name}: never enlarged"
        );
        let [r, _, b] = rgb(&img, 4, 40);
        assert!(r > 150 && b < 100, "{name}: red on the left");
        let [_, g, _] = rgb(&img, 32, 4);
        assert!(g > 150, "{name}: green on top");
        let small = decode_image(&photo_fixture(name), 32).unwrap();
        assert_eq!((small.width(), small.height()), (32, 24), "{name}");
    }
    let turned = decode_image(&photo_fixture("pattern-rot90.heic"), 1024).unwrap();
    assert_eq!((turned.width(), turned.height()), (48, 64));
    let [_, g, _] = rgb(&turned, 44, 32);
    assert!(
        g > 150,
        "the green band is on the right after a quarter turn"
    );
    let thumb =
        image::load_from_memory(&thumbnail(&photo_fixture("pattern.heic"), 32).unwrap()).unwrap();
    assert_eq!((thumb.width(), thumb.height()), (32, 24));

    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.heic");
    std::fs::write(&bad, b"not a photo").unwrap();
    assert!(is_corrupt(&decode_image(&bad, 64).unwrap_err()));
}

/// A JPEG of `w` x `h`, left half red, right half blue.
fn jpeg_bytes(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, _| {
        if x < w / 2 {
            image::Rgb([255, 0, 0])
        } else {
            image::Rgb([0, 0, 255])
        }
    });
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&img)
        .unwrap();
    out
}

/// One little-endian IFD: (tag, type, count, value) entries and the next IFD's offset.
fn ifd(entries: &[(u16, u16, u32, u32)], next: u32) -> Vec<u8> {
    let mut out = (entries.len() as u16).to_le_bytes().to_vec();
    for (tag, kind, count, value) in entries {
        out.extend(tag.to_le_bytes());
        out.extend(kind.to_le_bytes());
        out.extend(count.to_le_bytes());
        out.extend(value.to_le_bytes());
    }
    out.extend(next.to_le_bytes());
    out
}

/// A tiny DNG-shaped file: IFD0 (orientation 6) with an 8 x 6 JPEG thumbnail and two
/// SubIFDs, one a "lossless JPEG" strip (a frame header only, 200 x 100) and one a 40 x 20
/// baseline JPEG strip. The committed `tiny.dng` fixture is this file (`write_dng_fixture`).
pub(crate) fn tiny_dng() -> Vec<u8> {
    let (small, big) = (jpeg_bytes(8, 6), jpeg_bytes(40, 20));
    let lossless = [
        0xFF, 0xD8, 0xFF, 0xC3, 0x00, 0x0B, 0x08, 0x00, 0x64, 0x00, 0xC8, 0x01, 0x01, 0x11, 0x00,
        0xFF, 0xD9,
    ];
    let mut buf = vec![0u8; 8];
    let put = |buf: &mut Vec<u8>, data: &[u8]| {
        let at = buf.len() as u32;
        buf.extend_from_slice(data);
        at
    };
    let small_at = put(&mut buf, &small);
    let big_at = put(&mut buf, &big);
    let lossless_at = put(&mut buf, &lossless);
    let strip = |at: u32, len: usize| {
        ifd(
            &[
                (0x0103, 3, 1, 7),
                (0x0111, 4, 1, at),
                (0x0117, 4, 1, len as u32),
            ],
            0,
        )
    };
    let sub_a = put(&mut buf, &strip(lossless_at, lossless.len()));
    let sub_b = put(&mut buf, &strip(big_at, big.len()));
    let subs: Vec<u8> = [sub_a, sub_b]
        .iter()
        .flat_map(|o| o.to_le_bytes())
        .collect();
    let subs_at = put(&mut buf, &subs);
    let ifd0 = ifd(
        &[
            (0x0112, 3, 1, 6),
            (0x014A, 4, 2, subs_at),
            (0x0201, 4, 1, small_at),
            (0x0202, 4, 1, small.len() as u32),
        ],
        0,
    );
    let ifd0_at = put(&mut buf, &ifd0);
    buf[..4].copy_from_slice(b"II*\0");
    buf[4..8].copy_from_slice(&ifd0_at.to_le_bytes());
    buf
}

#[test]
fn raw_files_show_their_largest_decodable_preview() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("photo.dng");
    std::fs::write(&path, tiny_dng()).unwrap();
    let found: Vec<(u32, u32)> = raw_previews(&path)
        .unwrap()
        .iter()
        .map(|p| (p.width, p.height))
        .collect();
    assert_eq!(found, [(40, 20), (8, 6)], "the lossless strip is left out");
    let img = decode_image(&path, 256).unwrap();
    assert_eq!(
        (img.width(), img.height()),
        (20, 40),
        "orientation 6 applied"
    );
    let [r, _, b] = rgb(&img, 10, 4);
    assert!(r > 200 && b < 60, "red on top after the quarter turn");
    let thumb = image::load_from_memory(&thumbnail(&path, 256).unwrap()).unwrap();
    assert_eq!((thumb.width(), thumb.height()), (20, 40));

    // The same bytes under an Olympus header ("IIRO") read the same.
    let mut orf = tiny_dng();
    orf[2..4].copy_from_slice(b"RO");
    let orf_path = dir.path().join("photo.orf");
    std::fs::write(&orf_path, orf).unwrap();
    assert_eq!(raw_previews(&orf_path).unwrap().len(), 2);
    assert_eq!(tiff_orientation(&orf_path).unwrap(), Some(6));

    // No preview at all, or not a TIFF: a content error.
    let empty = dir.path().join("empty.nef");
    std::fs::write(
        &empty,
        [b"II*\0".as_slice(), &8u32.to_le_bytes(), &ifd(&[], 0)].concat(),
    )
    .unwrap();
    assert!(is_corrupt(&decode_image(&empty, 64).unwrap_err()));
    let text = dir.path().join("text.cr2");
    std::fs::write(&text, b"not a raw file").unwrap();
    assert!(raw_previews(&text).unwrap().is_empty());
    assert!(is_corrupt(&decode_image(&text, 64).unwrap_err()));
}

/// Loops, huge counts and offsets past the end end the walk; nothing is read for them.
#[test]
fn hostile_raw_directories_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("loop.dng");
    // IFD0 points at itself as its next IFD and claims 65,535 entries, a SubIFD array of
    // a billion offsets, and a preview past the end of the file.
    let mut buf = b"II*\0".to_vec();
    buf.extend(8u32.to_le_bytes());
    buf.extend(ifd(
        &[
            (0x014A, 4, 1_000_000_000, 8),
            (0x0201, 4, 1, 4_000_000_000),
            (0x0202, 4, 1, 4_000_000),
        ],
        8,
    ));
    buf[8..10].copy_from_slice(&u16::MAX.to_le_bytes());
    std::fs::write(&path, &buf).unwrap();
    assert!(raw_previews(&path).unwrap().is_empty());
    assert!(decode_image(&path, 64).is_err());
}

/// Writes keel-preview's `tiny.dng` fixture and the `raw` fuzz seed: run it after changing
/// [`tiny_dng`].
#[test]
#[ignore = "writes the tiny.dng fixture and corpus seed"]
fn write_dng_fixture() {
    std::fs::write(photo_fixture("tiny.dng"), tiny_dng()).unwrap();
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/raw");
    std::fs::create_dir_all(&corpus).unwrap();
    std::fs::write(corpus.join("tiny.dng"), tiny_dng()).unwrap();
}

/// The media view's facts for HEIC, AVIF and RAW: size as shown, orientation 1 for the
/// photos ffmpeg turns upright, EXIF capture time and camera from a HEIC.
#[test]
fn heic_avif_and_raw_metadata() {
    assert_eq!(media_type(Path::new("a.HEIC")), Some(MediaType::Image));
    for ext in ["avif", "dng", "nef", "cr2", "arw", "orf", "rw2", "pef"] {
        assert_eq!(
            media_type(Path::new(&format!("a.{ext}"))),
            Some(MediaType::Image)
        );
    }
    // EXIF sizes need no ffmpeg.
    let dated = read_meta(&photo_fixture("pattern-dated.heic")).unwrap();
    assert_eq!((dated.width, dated.height, dated.orientation), (64, 48, 1));
    assert_eq!(dated.taken_at, unix_time(2024, 5, 1, 10, 34, 56));
    assert_eq!(dated.camera.as_deref(), Some("Keel Test Camera"));

    let dir = tempfile::tempdir().unwrap();
    let dng = dir.path().join("photo.dng");
    std::fs::write(&dng, tiny_dng()).unwrap();
    let raw = read_meta(&dng).unwrap();
    assert_eq!((raw.width, raw.height, raw.orientation), (20, 40, 6));

    if find_tool("ffprobe").is_none() {
        eprintln!("ffprobe not found: the rest skipped");
        return;
    }
    for (name, size) in [
        ("pattern.heic", (64, 48)),
        ("pattern-rot90.heic", (48, 64)),
        ("pattern.avif", (64, 48)),
    ] {
        let meta = read_meta(&photo_fixture(name)).unwrap();
        assert_eq!((meta.width, meta.height), size, "{name}");
        assert_eq!(meta.orientation, 1, "{name}");
    }
}

#[test]
fn displayed_size_takes_off_cropping_and_turns() {
    let v = serde_json::json!({"streams": [{"codec_type": "video", "width": 64, "height": 64,
        "side_data_list": [{"crop_top": 0, "crop_bottom": 16, "crop_left": 0, "crop_right": 0},
                           {"rotation": -90}]}]});
    assert_eq!(displayed_size(&v), Some((48, 64)));
    let bad = serde_json::json!({"streams": [{"codec_type": "video", "width": 8, "height": 8,
        "side_data_list": [{"crop_top": 9}]}]});
    assert_eq!(displayed_size(&bad), None);
    assert_eq!(displayed_size(&serde_json::json!({})), None);
}

/// A HEIF whose `meta` box runs to the end of a file larger than the cap: the EXIF reader
/// is offered the head only, never the whole box.
#[test]
fn heif_exif_reads_at_most_the_cap() {
    use std::cell::Cell;
    use std::io::{Read, Seek, SeekFrom};
    use std::rc::Rc;
    struct Counting(std::io::Cursor<Vec<u8>>, Rc<Cell<u64>>);
    impl Read for Counting {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.read(buf)?;
            self.1.set(self.1.get() + n as u64);
            Ok(n)
        }
    }
    impl Seek for Counting {
        fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(to)
        }
    }
    let mut file = Vec::new();
    // ftyp: major brand heic, minor version, compatible brands mif1 heic.
    file.extend_from_slice(&24u32.to_be_bytes());
    file.extend_from_slice(b"ftypheic\0\0\0\0mif1heic");
    // meta, size 0: "to the end of the file".
    file.extend_from_slice(&0u32.to_be_bytes());
    file.extend_from_slice(b"meta");
    file.resize(3 << 20, 0);
    let len = file.len() as u64;
    let cap = 1 << 20;
    let read = Rc::new(Cell::new(0));
    let r = heif_exif(
        Counting(std::io::Cursor::new(file.clone()), read.clone()),
        len,
        cap,
    );
    assert!(r.is_err(), "no EXIF in it");
    assert!(read.get() <= cap, "read {} bytes", read.get());
    // Without the cap the whole box is read: the test sees the difference.
    let all = Rc::new(Cell::new(0));
    let _ = heif_exif(Counting(std::io::Cursor::new(file), all.clone()), len, len);
    assert!(all.get() > cap, "read {} bytes", all.get());
}
