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
    let img = decode_image(&path).unwrap();
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
        let img = decode_image(&path).unwrap();
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
