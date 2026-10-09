//! Own test binary: it rewrites PATH, which would race the other preview tests.
use keel_preview::{preview, Preview, Request};
use keel_vfs::{Entry, Kind, VPath};
use std::time::{Duration, Instant};

#[test]
fn hung_ffmpeg_is_killed_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    std::fs::write(
        dir.path().join("ffmpeg.cmd"),
        "@ping -n 31 127.0.0.1 >nul\r\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.path().join("ffmpeg");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs = vec![dir.path().to_owned()];
    dirs.extend(std::env::split_paths(&path));
    std::env::set_var("PATH", std::env::join_paths(dirs).unwrap());

    let video = dir.path().join("clip.mp4");
    std::fs::write(&video, b"not really a video").unwrap();
    let req = Request {
        entry: Entry {
            path: VPath::local(&video),
            name: "clip.mp4".into(),
            kind: Kind::File,
            size: 18,
            modified: None,
            hidden: false,
            is_link: false,
            encrypted: false,
            ext: "mp4".into(),
        },
        bytes_path: video,
        page: 0,
        max_px: 64,
        fit_width: false,
    };
    let started = Instant::now();
    let result = preview(&req);
    let elapsed = started.elapsed();
    assert!(
        matches!(&result, Preview::Error(message) if message == "ffmpeg timed out"),
        "{result:?}"
    );
    assert!(elapsed < Duration::from_secs(12), "took {elapsed:?}");
}
