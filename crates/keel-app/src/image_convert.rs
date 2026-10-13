//! Local image conversion: new sibling files, never replacing an existing path.

use crate::a11y::Named as _;
use crate::keys::Action;
use anyhow::{Context, Result};
use keel_vfs::{Progress, VPath};
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
    WebP,
}

impl Format {
    fn encoding(self) -> image::ImageFormat {
        match self {
            Self::Png => image::ImageFormat::Png,
            Self::Jpeg => image::ImageFormat::Jpeg,
            Self::WebP => image::ImageFormat::WebP,
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::WebP => "webp",
        }
    }
}

pub fn supported(path: &VPath) -> bool {
    path.to_local_path().is_some_and(|p| {
        !keel_core::is_converted_photo(&p)
            && image::ImageFormat::from_path(p).is_ok_and(|f| f.reading_enabled())
    })
}

pub struct ConvertDialog {
    src: Vec<PathBuf>,
    format: Format,
    max: String,
    error: Option<String>,
}

impl ConvertDialog {
    pub fn new(src: Vec<PathBuf>) -> Self {
        Self {
            src,
            format: Format::Png,
            max: String::new(),
            error: None,
        }
    }

    fn action(&self) -> Result<Action> {
        let max = match self.max.trim() {
            "" => None,
            s => Some(s.parse::<u32>().ok().filter(|n| *n > 0).ok_or_else(|| {
                anyhow::anyhow!("Enter a positive maximum dimension, or leave it blank")
            })?),
        };
        Ok(Action::ConvertImages {
            src: self.src.clone(),
            format: self.format,
            max,
        })
    }
}

pub fn ui(ui: &mut egui::Ui, d: &mut ConvertDialog, cancel: &mut bool) -> Option<Action> {
    ui.strong("Convert image");
    ui.label(format!(
        "Convert {} beside the originals",
        crate::jobs::items(d.src.len())
    ));
    ui.horizontal(|ui| {
        for (label, f) in [
            ("PNG", Format::Png),
            ("JPEG", Format::Jpeg),
            ("WebP", Format::WebP),
        ] {
            ui.radio_value(&mut d.format, f, label);
        }
    });
    ui.label("Maximum dimension (pixels, blank keeps size)");
    ui.text_edit_singleline(&mut d.max)
        .named("Maximum dimension");
    ui.weak("Keeps proportions, never enlarges. Existing files are kept. JPEG removes transparency; animation and metadata are not retained.");
    if let Some(e) = &d.error {
        ui.colored_label(ui.visuals().error_fg_color, e);
    }
    let mut out = None;
    ui.horizontal(|ui| {
        let go = ui.button("Convert");
        crate::a11y::first_focus(&go);
        if go.clicked() {
            match d.action() {
                Ok(a) => out = Some(a),
                Err(e) => d.error = Some(e.to_string()),
            }
        }
        *cancel |= ui.button("Cancel").clicked();
    });
    out
}

pub fn run(
    src: &[PathBuf],
    format: Format,
    max: Option<u32>,
    report: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<()> {
    anyhow::ensure!(max != Some(0), "maximum dimension must be positive");
    let mut progress = Progress {
        total_items: src.len(),
        ..Progress::default()
    };
    for path in src {
        let check = || -> Result<()> {
            anyhow::ensure!(!cancel.load(Ordering::Relaxed), "operation cancelled");
            Ok(())
        };
        check()?;
        progress.current = format!("Reading {}", path.display());
        report(progress.clone());
        // Reuse the bounded, fuzzed decoder and bake orientation into the pixels before
        // dropping metadata and calculating the resized dimensions.
        anyhow::ensure!(
            supported(&VPath::local(path)),
            "Unsupported local image: {}",
            path.display()
        );
        let mut decoded = keel_core::decode_image(path, u32::MAX)
            .with_context(|| format!("Could not decode {}", path.display()))?;
        check()?;
        if let Some(max) = max {
            if decoded.width() > max || decoded.height() > max {
                progress.current = format!("Resizing {}", path.display());
                report(progress.clone());
                decoded = decoded.resize(max, max, image::imageops::FilterType::Lanczos3);
            }
        }
        if format == Format::Jpeg {
            decoded = image::DynamicImage::ImageRgb8(decoded.to_rgb8());
        }
        check()?;
        let stem = path
            .file_stem()
            .ok_or_else(|| anyhow::anyhow!("image has no name"))?;
        let mut number = 0;
        let (output, file) = loop {
            let mut name = stem.to_os_string();
            if number > 0 {
                name.push(format!(" - converted {number}"));
            }
            name.push(format!(".{}", format.extension()));
            let output = path.with_file_name(name);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&output)
            {
                Ok(file) => break (output, file),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => number += 1,
                Err(e) => return Err(e.into()),
            }
        };
        progress.current = format!("Writing {}", output.display());
        report(progress.clone());
        let result = (|| -> Result<()> {
            let mut writer = BufWriter::new(file);
            decoded.write_to(&mut writer, format.encoding())?;
            writer.flush()?;
            check()
        })();
        if let Err(e) = result {
            std::fs::remove_file(&output).with_context(|| {
                format!(
                    "Could not remove unfinished {} after {e:#}",
                    output.display()
                )
            })?;
            return Err(e);
        }
        progress.done_items += 1;
        report(progress.clone());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bakes_exif_orientation_into_output_before_resizing() {
        let dir = tempfile::tempdir().unwrap();
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_pixel(40, 20, image::Rgb([255, 0, 0]))
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();
        let jpeg = jpeg.into_inner();
        // APP1 Exif: big-endian TIFF, Orientation (SHORT) = 6, as in the viewer test.
        let tiff: &[u8] = &[
            b'M', b'M', 0, 42, 0, 0, 0, 8, 0, 1, 0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0, 0, 0, 0,
            0,
        ];
        let payload = [b"Exif\0\0".as_slice(), tiff].concat();
        let mut bytes = jpeg[..2].to_vec();
        bytes.extend([0xff, 0xe1]);
        bytes.extend(((payload.len() + 2) as u16).to_be_bytes());
        bytes.extend(payload);
        bytes.extend(&jpeg[2..]);
        let src = dir.path().join("rotated.jpg");
        std::fs::write(&src, bytes).unwrap();
        run(
            &[src],
            Format::Png,
            Some(10),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        let output = image::open(dir.path().join("rotated.png")).unwrap();
        assert_eq!((output.width(), output.height()), (5, 10));
    }

    #[test]
    fn supported_formats_are_local_raster_images() {
        assert!(supported(&VPath::local("photo.png")));
        assert!(supported(&VPath::local("photo.JPG")));
        assert!(!supported(&VPath::local("notes.txt")));
        assert!(!supported(&VPath::local("drawing.svg")));
        assert!(!supported(&VPath::local("phone.heic")));
        assert!(!supported(&VPath::local("photo.avif")));
        assert!(!supported(&VPath::parse("sftp://host/photo.png").unwrap()));
        assert!(!supported(&VPath::join_archive(
            &VPath::local("photos.zip"),
            "photo.png"
        )));
    }

    #[test]
    fn converts_all_formats_resizes_and_keeps_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("photo.png");
        image::RgbaImage::from_pixel(12, 6, image::Rgba([90, 60, 30, 128]))
            .save(&src)
            .unwrap();
        let original = std::fs::read(&src).unwrap();
        std::fs::write(dir.path().join("photo.jpg"), b"keep").unwrap();
        for format in [Format::Png, Format::Jpeg, Format::WebP] {
            let reports = std::cell::RefCell::new(Vec::new());
            run(
                std::slice::from_ref(&src),
                format,
                Some(8),
                &|p| reports.borrow_mut().push(p),
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(reports.borrow().last().unwrap().done_items, 1);
            let name = if format == Format::WebP {
                "photo.webp".to_owned()
            } else {
                format!("photo - converted 1.{}", format.extension())
            };
            let output = image::open(dir.path().join(name)).unwrap();
            assert_eq!((output.width(), output.height()), (8, 4));
            if format != Format::Jpeg {
                assert_eq!(output.to_rgba8().get_pixel(0, 0)[3], 128);
            }
        }
        assert_eq!(std::fs::read(&src).unwrap(), original);
        assert_eq!(
            std::fs::read(dir.path().join("photo.jpg")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn batch_failure_keeps_completed_outputs_and_names_the_failed_source() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.png");
        let bad = dir.path().join("bad.png");
        image::RgbImage::new(3, 2).save(&good).unwrap();
        std::fs::write(&bad, b"broken").unwrap();
        let progress = std::cell::RefCell::new(Vec::new());
        let error = run(
            &[good, bad],
            Format::WebP,
            None,
            &|p| progress.borrow_mut().push(p),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("bad.png"));
        assert!(dir.path().join("good.webp").is_file());
        assert!(!dir.path().join("bad.webp").exists());
        assert_eq!(progress.borrow().last().unwrap().done_items, 1);
        assert_eq!(progress.borrow().last().unwrap().total_items, 2);
    }

    #[test]
    fn cancellation_invalid_images_and_dimensions_leave_no_output() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("bad.png");
        std::fs::write(&src, b"not an image").unwrap();
        for (max, cancelled) in [(None, true), (None, false), (Some(0), false)] {
            assert!(run(
                std::slice::from_ref(&src),
                Format::WebP,
                max,
                &|_| {},
                &AtomicBool::new(cancelled)
            )
            .is_err());
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        let mut d = ConvertDialog::new(vec![src]);
        for invalid in ["0", "-2", "abc", "4294967296"] {
            d.max = invalid.into();
            assert!(d.action().is_err());
        }
        d.max.clear();
        assert!(d.action().is_ok());
    }

    #[test]
    fn never_enlarges_and_removes_cancelled_output() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("small.png");
        image::RgbImage::new(2, 1).save(&src).unwrap();
        let cancel = AtomicBool::new(false);
        run(
            std::slice::from_ref(&src),
            Format::WebP,
            Some(20),
            &|_| {},
            &cancel,
        )
        .unwrap();
        let output = image::open(dir.path().join("small.webp")).unwrap();
        assert_eq!((output.width(), output.height()), (2, 1));
        std::fs::remove_file(dir.path().join("small.webp")).unwrap();
        assert!(run(
            &[src],
            Format::WebP,
            None,
            &|p| {
                if p.current.starts_with("Writing") {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel
        )
        .is_err());
        assert!(!dir.path().join("small.webp").exists());
    }
}
