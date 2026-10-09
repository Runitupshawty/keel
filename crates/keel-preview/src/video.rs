use crate::{Preview, Request, Rgba};
use std::process::Command;

const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "mov", "avi", "webm", "m4v"];

pub(crate) fn accepts(ext: &str) -> bool {
    VIDEO_EXTENSIONS.contains(&ext)
}

pub(crate) fn render(req: &Request) -> Preview {
    let output = match Command::new("ffmpeg")
        .args(["-y", "-ss", "1", "-i"])
        .arg(&req.bytes_path)
        .args([
            "-frames:v",
            "1",
            "-vf",
            &format!("scale={}:-1", req.max_px.max(1)),
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "-",
        ])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Preview::Unsupported,
        Err(error) => return Preview::Error(error.to_string()),
    };
    if !output.status.success() {
        return Preview::Error(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    let image = match image::load_from_memory(&output.stdout) {
        Ok(image) => image.to_rgba8(),
        Err(error) => return Preview::Error(error.to_string()),
    };
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(&req.bytes_path)
        .output();
    let (duration_s, meta) = match probe {
        Ok(output) if output.status.success() => {
            let meta = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            (meta.parse().unwrap_or(0.0), meta)
        }
        _ => (0.0, String::new()),
    };
    Preview::Video {
        thumb: Rgba {
            w: image.width(),
            h: image.height(),
            data: image.into_raw(),
        },
        duration_s,
        meta,
    }
}
