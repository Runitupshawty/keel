use crate::{Preview, Request, Rgba};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "mov", "avi", "webm", "m4v"];
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);
/// Checked after PATH: GUI apps launched from Finder/Explorer often get a minimal PATH.
const FALLBACK_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", r"C:\ffmpeg\bin"];

pub(crate) fn accepts(ext: &str) -> bool {
    VIDEO_EXTENSIONS.contains(&ext)
}

pub(crate) fn render(req: &Request) -> Preview {
    let Some(ffmpeg) = find_tool("ffmpeg") else {
        return Preview::Unsupported;
    };
    // `-ss 1` before `-i` yields no frame for clips shorter than a second: retry at 0.
    let mut last_error = String::from("ffmpeg produced no frame");
    let mut png = None;
    for seek in ["1", "0"] {
        let mut command = Command::new(&ffmpeg);
        command
            .args(["-v", "error", "-y", "-ss", seek, "-i"])
            .arg(&req.bytes_path)
            .args(["-frames:v", "1", "-vf"])
            .arg(format!("scale={}:-1", req.max_px.max(1)))
            .args(["-f", "image2pipe", "-vcodec", "png", "-"]);
        match run(command) {
            Ok((status, stdout, _)) if status.success() && !stdout.is_empty() => {
                png = Some(stdout);
                break;
            }
            Ok((_, _, stderr)) => {
                let stderr = String::from_utf8_lossy(&stderr).trim().to_owned();
                if !stderr.is_empty() {
                    last_error = stderr;
                }
            }
            Err(error) => return Preview::Error(error),
        }
    }
    let Some(png) = png else {
        return Preview::Error(last_error);
    };
    let image = match image::load_from_memory(&png) {
        Ok(image) => image.to_rgba8(),
        Err(error) => return Preview::Error(error.to_string()),
    };
    let (duration_s, meta) = probe(&req.bytes_path).unwrap_or_default();
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

fn probe(path: &Path) -> Option<(f64, String)> {
    let mut command = Command::new(find_tool("ffprobe")?);
    command
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path);
    let (status, stdout, _) = run(command).ok()?;
    if !status.success() {
        return None;
    }
    let meta = String::from_utf8_lossy(&stdout).trim().to_owned();
    Some((meta.parse().unwrap_or(0.0), meta))
}

/// First `name` on PATH, then in FALLBACK_DIRS. On Windows `.cmd`/`.bat` count too
/// (shims), which `Command::new("ffmpeg")` alone would not find.
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

type Output = (ExitStatus, Vec<u8>, Vec<u8>);

/// Runs with piped output and a hard TOOL_TIMEOUT; a hung tool is killed, never waited on.
fn run(mut command: Command) -> Result<Output, String> {
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
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    // Drain both pipes on threads so a chatty tool cannot fill a pipe and stall.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buffer);
            }
            buffer
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    match child.wait_timeout(TOOL_TIMEOUT) {
        Ok(Some(status)) => Ok((
            status,
            stdout.join().unwrap_or_default(),
            stderr.join().unwrap_or_default(),
        )),
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            // The drain threads are left to finish on their own: a grandchild (e.g. a
            // shim's real process) may still hold the pipes open.
            Err("ffmpeg timed out".to_owned())
        }
        Err(error) => {
            let _ = child.kill();
            Err(error.to_string())
        }
    }
}
