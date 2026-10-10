use super::*;
use std::time::{Duration, Instant};

#[test]
fn probe_rates_and_sizes() {
    assert_eq!(
        rate("30000/1001").map(|r| (r * 1000.0).round()),
        Some(29970.0)
    );
    assert_eq!(rate("25"), Some(25.0));
    assert_eq!(rate("0/0"), None);
    assert_eq!(rate("x"), None);
    let v = serde_json::json!({
        "format": {"duration": "12.500"},
        "streams": [
            {"codec_type": "video", "width": 1920, "height": 1080, "avg_frame_rate": "0/0",
             "r_frame_rate": "30000/1001", "side_data_list": [{"rotation": -90}]},
            {"codec_type": "audio"}
        ]
    });
    let p = probe_of(&v);
    assert_eq!((p.width, p.height), (1080, 1920), "rotation applied");
    assert_eq!(p.fps, 29.97);
    assert_eq!(p.duration, 12.5);
    assert!(p.audio);
    let slomo =
        serde_json::json!({"streams": [{"codec_type": "video", "avg_frame_rate": "240/1"}]});
    let p = probe_of(&slomo);
    assert_eq!((p.fps, p.duration, p.audio), (60.0, 0.0, false));

    assert_eq!(decode_size([3840, 2160], [2000.0, 1200.0]), [1920, 1080]);
    assert_eq!(
        decode_size([3840, 2160], [4000.0, 4000.0]),
        [1920, 1080],
        "cap"
    );
    assert_eq!(
        decode_size([320, 240], [2000.0, 1200.0]),
        [320, 240],
        "no upscale"
    );
    assert_eq!(decode_size([1920, 1080], [960.0, 1080.0]), [960, 540]);
    assert_eq!(decode_size([101, 51], [500.0, 500.0]), [100, 50], "even");
    assert_eq!(decode_size([0, 0], [0.0, 0.0]), [2, 2]);
}

#[test]
fn ffmpeg_arguments() {
    let f = Path::new("clip.mp4");
    let s = |a: Vec<OsString>| {
        a.into_iter()
            .map(|a| a.into_string().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        s(video_args(f, 12.25, [640, 360], 29.97)),
        [
            "-v",
            "error",
            "-nostdin",
            "-ss",
            "12.250",
            "-i",
            "clip.mp4",
            "-an",
            "-sn",
            "-dn",
            "-vf",
            "fps=29.970,scale=640:360",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-"
        ]
    );
    assert_eq!(
        s(audio_args(f, 0.0, 48_000, 2)),
        [
            "-v", "error", "-nostdin", "-ss", "0.000", "-i", "clip.mp4", "-vn", "-sn", "-dn",
            "-ac", "2", "-ar", "48000", "-f", "f32le", "-"
        ]
    );
    assert_eq!(
        s(audio_args(f, -3.0, 44_100, 6))[4],
        "0.000",
        "never before 0"
    );
}

#[test]
fn frame_queue_bound() {
    assert_eq!(queue_len(1920 * 1080 * 4), FRAME_QUEUE);
    assert_eq!(queue_len(1920 * 1920 * 4), 2, "64 MiB with the two outside");
    assert_eq!(queue_len(40 << 20), 1, "never zero");
    assert_eq!(queue_len(0), FRAME_QUEUE);
    // A bounded channel of that length refuses the next frame.
    let (tx, _rx) = bounded::<u8>(queue_len(1920 * 1080 * 4));
    for i in 0..FRAME_QUEUE {
        tx.try_send(i as u8).unwrap();
    }
    assert!(tx.try_send(9).is_err());
}

/// Frames at 25 fps: the due one is shown, late ones dropped, early ones kept.
#[test]
fn frame_timing() {
    let mut q: std::collections::VecDeque<f64> = (0..6).map(|i| i as f64 * 0.04).collect();
    let mut next = None;
    let mut at = |next: &mut Option<f64>, t| take_due(next, || q.pop_front(), |f| *f, t);
    assert_eq!(at(&mut next, 0.0), (Some(0.0), 0));
    assert_eq!(next, Some(0.04), "kept, not due");
    assert_eq!(at(&mut next, 0.02), (None, 0), "nothing new is due");
    assert_eq!(
        at(&mut next, 0.13),
        (Some(0.12), 2),
        "0.04 and 0.08 were late"
    );
    assert_eq!(at(&mut next, 0.16), (Some(0.16), 0));
    assert_eq!(at(&mut next, 9.0), (Some(0.2), 0));
    assert_eq!(at(&mut next, 10.0), (None, 0), "queue empty");
    assert_eq!(next, None);
    assert_eq!(clock_text(75.4), "1:15");
    assert_eq!(clock_text(3725.0), "1:02:05");
}

/// The sound clock leads; once the sound is done a paused or running wall clock carries
/// on from where it stopped.
#[test]
fn clock_follows_sound_then_wall() {
    let audio = Arc::new(Audio::default());
    let mut c = Clock::new(audio.clone());
    assert_eq!(c.now(), 0.0, "device not open yet");
    audio.rate.store(48_000, Ordering::Release);
    audio.played.store(24_000, Ordering::Release);
    c.set_playing(true);
    assert_eq!(c.now(), 0.5);
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(c.now(), 0.5, "no sound played: the clock waits");
    audio.played.store(48_000, Ordering::Release);
    audio.done.store(true, Ordering::Release);
    let t = c.now();
    assert!((1.0..1.01).contains(&t), "{t}");
    std::thread::sleep(Duration::from_millis(30));
    assert!(c.now() >= 1.025, "wall clock runs on");
    c.set_playing(false);
    assert!(audio.paused.load(Ordering::Relaxed));
    let paused = c.now();
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(c.now(), paused);
}

/// The player's state machine: play/pause, seek, loop, end, volume, mute.
#[test]
fn transport_state_machine() {
    let mut t = Transport::default();
    assert_eq!(t.apply(Cmd::Toggle, 10.0), None);
    assert!(t.playing);
    assert_eq!(t.apply(Cmd::Seek(4.0), 10.0), Some(4.0));
    assert!(t.playing, "a seek keeps playing");
    assert_eq!(t.apply(Cmd::Seek(99.0), 10.0), Some(10.0));
    assert_eq!(t.apply(Cmd::Seek(-1.0), 0.0), Some(0.0));
    assert_eq!(t.apply(Cmd::Seek(99.0), 0.0), Some(99.0), "unknown length");
    assert_eq!(t.apply(Cmd::Toggle, 10.0), None);
    assert!(!t.playing);
    assert_eq!(t.apply(Cmd::Toggle, 10.0), None);
    // Loop on: the end restarts at 0 and keeps playing.
    assert_eq!(t.apply(Cmd::Loop, 10.0), None);
    assert_eq!(t.apply(Cmd::Ended, 10.0), Some(0.0));
    assert!(t.playing && !t.ended);
    // Loop off: the end stops; Play starts over.
    t.apply(Cmd::Loop, 10.0);
    assert_eq!(t.apply(Cmd::Ended, 10.0), None);
    assert!(!t.playing && t.ended);
    assert_eq!(t.apply(Cmd::Toggle, 10.0), Some(0.0));
    assert!(t.playing && !t.ended);
    // A seek after the end leaves the end state (paused there, Play goes on from it).
    t.apply(Cmd::Ended, 10.0);
    assert_eq!(t.apply(Cmd::Seek(3.0), 10.0), Some(3.0));
    assert!(!t.ended && !t.playing);
    assert_eq!(t.apply(Cmd::Toggle, 10.0), None);
    assert!(t.playing);
    // Volume in tenths, clamped; a change unmutes.
    assert_eq!(t.gain(), 1.0);
    t.apply(Cmd::Volume(0.1), 0.0);
    assert_eq!(t.volume, 1.0);
    for _ in 0..3 {
        t.apply(Cmd::Volume(-0.1), 0.0);
    }
    assert_eq!(t.volume, 0.7);
    t.apply(Cmd::Mute, 0.0);
    assert_eq!(t.gain(), 0.0);
    t.apply(Cmd::Volume(-0.1), 0.0);
    assert!(!t.muted);
    assert_eq!(t.gain(), 0.6);
    for _ in 0..12 {
        t.apply(Cmd::Volume(-0.1), 0.0);
    }
    assert_eq!(t.volume, 0.0);
}

/// A 1.5 s clip with sound made by ffmpeg (lavfi test sources), or None without ffmpeg.
fn test_clip(name: &str) -> Option<PathBuf> {
    let Some(ffmpeg) = keel_core::find_tool("ffmpeg") else {
        eprintln!("skipped: ffmpeg is not installed");
        return None;
    };
    let dir = std::env::temp_dir().join(format!("keel-video-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("clip.mp4");
    let status = Command::new(ffmpeg)
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc=duration=1.5:size=160x120:rate=25")
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1.5"])
        .args(["-c:v", "mpeg4", "-c:a", "aac", "-shortest"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success(), "ffmpeg could not make the test clip");
    Some(out)
}

fn player(path: &Path, at: f64) -> VideoPlayer {
    let path = path.to_owned();
    let muted = Transport {
        muted: true,
        ..Transport::default()
    };
    VideoPlayer::new(
        egui::Context::default(),
        Arc::new(move || Ok(path.clone())),
        &muted,
        at,
        [320.0, 240.0],
    )
}

fn run_until(p: &mut VideoPlayer, secs: u64, done: impl Fn(&mut VideoPlayer) -> bool) {
    let until = Instant::now() + Duration::from_secs(secs);
    while !done(p) {
        assert!(Instant::now() < until, "timed out; error: {:?}", p.error);
        p.frame([320.0, 240.0]);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Plays a generated clip to the end (sound muted, still clocked by the device when there
/// is one), seeks, and checks every ffmpeg process was killed and waited for.
#[test]
fn plays_a_clip_and_reaps_its_processes() {
    let Some(clip) = test_clip("play") else {
        return;
    };
    let mut p = player(&clip, 0.0);
    run_until(&mut p, 15, |p| p.tex.is_some());
    assert_eq!(p.tex.as_ref().unwrap().size(), [160, 120]);
    let k = p.known.clone().unwrap();
    assert_eq!((k.probe.fps, k.probe.audio), (25.0, true));
    assert!((1.4..1.7).contains(&k.probe.duration), "{:?}", k.probe);
    assert_eq!(p.position(), 0.0, "paused at the start");

    p.command(Cmd::Toggle);
    let started = Instant::now();
    run_until(&mut p, 20, |p| p.transport.ended);
    let took = started.elapsed().as_secs_f64();
    eprintln!(
        "played in {took:.2} s, {} frames shown, {:.2} s of sound through the device",
        p.shown,
        p.sound_played()
    );
    assert!(took >= 1.2, "real time, not as fast as decoding: {took}");
    assert!(p.shown >= 10, "{}", p.shown);
    assert!(!p.transport.playing);
    assert!(p.error.is_none());
    let first = p.procs();

    // Seek: the old processes are gone (killed or ended, waited for), new ones run.
    p.command(Cmd::Seek(0.8));
    assert!(first.stopped());
    let (started_n, reaped) = first.counts();
    assert_eq!(started_n, reaped);
    assert!(started_n >= 1);
    run_until(&mut p, 15, |p| p.position() >= 0.8 && p.shown > 0);
    let second = p.procs();
    p.command(Cmd::Toggle);
    run_until(&mut p, 5, |p| p.position() > 1.0);
    drop(p);
    assert!(second.stopped());
    let (n, reaped) = second.counts();
    assert_eq!(n, reaped, "every process waited for");
    let _ = std::fs::remove_dir_all(clip.parent().unwrap());
}

/// Loop on: the end restarts at 0 and keeps playing. Killing mid-stream reaps at once.
#[test]
fn loops_and_stops_mid_stream() {
    let Some(clip) = test_clip("loop") else {
        return;
    };
    let mut p = player(&clip, 1.2);
    p.command(Cmd::Loop);
    p.command(Cmd::Toggle);
    run_until(&mut p, 15, |p| p.position() < 0.5 && p.shown > 3);
    assert!(p.transport.playing && !p.transport.ended);
    let procs = p.procs();
    let t = Instant::now();
    drop(p);
    assert!(t.elapsed() < Duration::from_secs(2));
    let (n, reaped) = procs.counts();
    assert_eq!(n, reaped);
    let _ = std::fs::remove_dir_all(clip.parent().unwrap());
}

/// A file ffmpeg cannot read fails with a message (the viewer then falls back).
#[test]
fn unreadable_file_fails() {
    if keel_core::find_tool("ffmpeg").is_none() {
        eprintln!("skipped: ffmpeg is not installed");
        return;
    }
    let dir = std::env::temp_dir().join(format!("keel-video-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bad = dir.join("bad.mp4");
    std::fs::write(&bad, b"not a video").unwrap();
    let mut p = player(&bad, 0.0);
    p.command(Cmd::Toggle);
    run_until(&mut p, 15, |p| p.error.is_some());
    let _ = std::fs::remove_dir_all(&dir);
}
