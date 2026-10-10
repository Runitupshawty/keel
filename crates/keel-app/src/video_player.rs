//! Video playback with sound in the media viewer. Each play position runs two `ffmpeg`
//! processes: one decodes RGBA frames at the viewer's size (a worker reads them into a
//! queue of at most [`FRAME_QUEUE`] frames), the other decodes interleaved f32 PCM that a
//! `cpal` output stream plays. The sound card is the master clock: a frame is shown once its
//! timestamp is due and frames that are already late are dropped. Without sound (no audio
//! stream, no output device, audio ended first) a wall clock stands in; the output stream
//! is paused with playback (the device is not held running). A seek, a file change or
//! closing the viewer kills both processes (reaped on a thread: the UI never waits for a
//! process stuck in I/O) and cancels a download that is still running; a remote file is
//! downloaded once per player, seeks before it is there wait for it. Nothing here decodes
//! on the UI thread; it only uploads the due frame into one reused texture.

use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
use egui::{ColorImage, TextureHandle, TextureOptions, Vec2};
use parking_lot::Mutex;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Longest side decoded (rawvideo bandwidth: 1920 x 1080 RGBA at 60 fps is 500 MB/s).
pub const MAX_SIDE: u32 = 1920;
/// Decoded frames waiting to be shown...
pub const FRAME_QUEUE: usize = 3;
/// ...and the most bytes of frames in flight: the queue plus three outside it (the read
/// buffer, the frame made from it, and the one waiting on the UI side).
pub const FRAME_BYTES: usize = 64 << 20;
/// Frames in flight outside the queue.
const OUTSIDE_QUEUE: usize = 3;
/// Decoded frame rate cap (240 fps slow motion plays at 60).
const MAX_FPS: f64 = 60.0;
/// PCM chunks between the decoder and the output callback (about 0.7 s at 48 kHz).
const AUDIO_CHUNKS: usize = 16;
const AUDIO_CHUNK_FRAMES: usize = 2048;
/// The most output latency taken off the sound clock (a bogus device timestamp).
const MAX_LATENCY: Duration = Duration::from_millis(500);
/// Download progress reaches the UI at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// Opens the file's bytes: its local path, or a local copy of a remote file (blocking,
/// called on a worker, at most once per player). The download stops when the flag is set
/// (the player stopped) and reports bytes done and the size (0: unknown); `(0, 0)` first
/// says a download starts.
pub type Resolve =
    Arc<dyn Fn(&AtomicBool, &dyn Fn(u64, u64)) -> anyhow::Result<PathBuf> + Send + Sync>;

// ---------------------------------------------------------------- pure parts

/// What playback needs to know about a video.
#[derive(Clone, Debug, PartialEq)]
pub struct Probe {
    /// As displayed (rotation applied; ffmpeg rotates the frames too).
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// Seconds (0 when unknown).
    pub duration: f64,
    pub audio: bool,
}

/// A [`Probe`] from `ffprobe -show_format -show_streams` JSON.
pub fn probe_of(v: &serde_json::Value) -> Probe {
    let meta = keel_core::parse_probe(v);
    let streams = v["streams"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let video = streams.iter().find(|s| {
        s["codec_type"] == "video" && s["disposition"]["attached_pic"].as_i64() != Some(1)
    });
    let fps = video
        .and_then(|s| {
            ["avg_frame_rate", "r_frame_rate"]
                .iter()
                .find_map(|k| s[*k].as_str().and_then(rate))
        })
        .unwrap_or(30.0)
        .min(MAX_FPS);
    // Non-square pixels: the display size stretches the coded width (the height once a
    // quarter turn swapped them).
    let (mut width, mut height) = (meta.width, meta.height);
    let sar = video
        .and_then(|s| s["sample_aspect_ratio"].as_str())
        .and_then(|r| {
            let (n, d) = r.split_once(':')?;
            let r = n.parse::<f64>().ok()? / d.parse::<f64>().ok()?;
            (r.is_finite() && r > 0.0).then_some(r)
        })
        .unwrap_or(1.0);
    if sar != 1.0 {
        let coded = video.map_or([0, 0], |s| {
            [s["width"].as_u64(), s["height"].as_u64()].map(|v| v.unwrap_or(0) as u32)
        });
        // ponytail: a square coded frame turned a quarter is stretched along the wrong axis.
        let turned = coded[0] != coded[1] && [width, height] == [coded[1], coded[0]];
        let stretch = |v: u32| ((f64::from(v) * sar).round() as u32).max(1);
        if turned {
            height = stretch(height);
        } else {
            width = stretch(width);
        }
    }
    Probe {
        width,
        height,
        // Rounded as passed to ffmpeg's fps filter, so timestamps match its output.
        fps: (fps * 1000.0).round() / 1000.0,
        duration: meta.duration_ms.map_or(0.0, |d| d as f64 / 1000.0),
        audio: streams.iter().any(|s| s["codec_type"] == "audio"),
    }
}

/// `30000/1001` or `25` as frames per second (None for `0/0` and below 1 fps).
pub fn rate(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/').unwrap_or((s, "1"));
    let r = n.trim().parse::<f64>().ok()? / d.trim().parse::<f64>().ok()?;
    (r.is_finite() && r >= 1.0).then_some(r)
}

/// The decode size: the video (`src` pixels) fitted into `area` physical pixels, never
/// enlarged, the long side at most [`MAX_SIDE`], even sides of at least 2.
pub fn decode_size(src: [u32; 2], area: [f32; 2]) -> [u32; 2] {
    let (w, h) = (src[0].max(1) as f32, src[1].max(1) as f32);
    let s = (area[0] / w)
        .min(area[1] / h)
        .min(MAX_SIDE as f32 / w.max(h))
        .min(1.0);
    let side = |x: f32| ((x * s).round() as u32).max(2) & !1;
    [side(w), side(h)]
}

/// Frames the queue may hold for frames of `frame_bytes`: [`FRAME_QUEUE`], fewer when they
/// and the three frames outside the queue would pass [`FRAME_BYTES`]; at least one.
pub fn queue_len(frame_bytes: usize) -> usize {
    (FRAME_BYTES / frame_bytes.max(1))
        .saturating_sub(OUTSIDE_QUEUE)
        .clamp(1, FRAME_QUEUE)
}

fn common_args(file: &Path, start: f64) -> Vec<OsString> {
    let mut a: Vec<OsString> = ["-v", "error", "-nostdin", "-ss"]
        .map(OsString::from)
        .to_vec();
    a.push(format!("{:.3}", start.max(0.0)).into());
    a.push("-i".into());
    a.push(file.into());
    a
}

/// ffmpeg arguments for RGBA frames of `size` at a constant `fps` from `start` seconds.
pub fn video_args(file: &Path, start: f64, size: [u32; 2], fps: f64) -> Vec<OsString> {
    let mut a = common_args(file, start);
    a.extend(["-an", "-sn", "-dn", "-vf"].map(OsString::from));
    a.push(format!("fps={fps:.3},scale={}:{}", size[0], size[1]).into());
    a.extend(["-f", "rawvideo", "-pix_fmt", "rgba", "-"].map(OsString::from));
    a
}

/// ffmpeg arguments for interleaved f32 PCM (`rate` Hz, `channels`) from `start` seconds.
pub fn audio_args(file: &Path, start: f64, rate: u32, channels: u16) -> Vec<OsString> {
    let mut a = common_args(file, start);
    a.extend(["-vn", "-sn", "-dn", "-ac"].map(OsString::from));
    a.push(channels.to_string().into());
    a.push("-ar".into());
    a.push(rate.to_string().into());
    a.extend(["-f", "f32le", "-"].map(OsString::from));
    a
}

/// The frame to show at clock `t`: pulls frames (oldest first) while they are due and
/// keeps the newest due one; due frames before it are late and dropped (counted). `next`
/// keeps a pulled frame that is not due yet.
pub fn take_due<F>(
    next: &mut Option<F>,
    mut pull: impl FnMut() -> Option<F>,
    pts: impl Fn(&F) -> f64,
    t: f64,
) -> (Option<F>, usize) {
    let (mut shown, mut dropped) = (None, 0);
    loop {
        if next.is_none() {
            *next = pull();
        }
        if !next.as_ref().is_some_and(|f| pts(f) <= t) {
            return (shown, dropped);
        }
        if shown.replace(next.take().expect("due")).is_some() {
            dropped += 1;
        }
    }
}

/// `secs` as `m:ss` (or `h:mm:ss`).
pub fn clock_text(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// A transport command.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cmd {
    /// Play / pause (at the end: play from the start).
    Toggle,
    /// Go to this second.
    Seek(f64),
    /// Volume up or down by this much (unmutes).
    Volume(f32),
    Mute,
    Loop,
    /// Both streams ran out.
    Ended,
}

/// Play state, volume and loop: the player's state machine.
#[derive(Clone, Debug, PartialEq)]
pub struct Transport {
    pub playing: bool,
    pub looping: bool,
    /// 0..=1.
    pub volume: f32,
    pub muted: bool,
    /// Stopped at the end (not looping).
    pub ended: bool,
}

impl Default for Transport {
    fn default() -> Self {
        Self {
            playing: false,
            looping: false,
            volume: 1.0,
            muted: false,
            ended: false,
        }
    }
}

impl Transport {
    /// Applies `cmd` (`duration` in seconds, 0 when unknown). Some(t): decoding restarts
    /// at `t` seconds.
    pub fn apply(&mut self, cmd: Cmd, duration: f64) -> Option<f64> {
        match cmd {
            Cmd::Toggle if self.ended => {
                self.ended = false;
                self.playing = true;
                Some(0.0)
            }
            Cmd::Toggle => {
                self.playing = !self.playing;
                None
            }
            Cmd::Seek(t) => {
                self.ended = false;
                let t = t.max(0.0);
                Some(if duration > 0.0 { t.min(duration) } else { t })
            }
            Cmd::Volume(d) => {
                self.volume = ((self.volume + d) * 10.0).round().clamp(0.0, 10.0) / 10.0;
                self.muted = false;
                None
            }
            Cmd::Mute => {
                self.muted = !self.muted;
                None
            }
            Cmd::Loop => {
                self.looping = !self.looping;
                None
            }
            Cmd::Ended if self.looping => Some(0.0),
            Cmd::Ended => {
                self.playing = false;
                self.ended = true;
                None
            }
        }
    }

    /// The output gain.
    pub fn gain(&self) -> f32 {
        if self.muted {
            0.0
        } else {
            self.volume
        }
    }
}

// ---------------------------------------------------------------- clock

/// Shared with the audio thread and the output callback.
#[derive(Default)]
pub struct Audio {
    /// Frames (samples per channel) handed to the device since the session's start.
    played: AtomicU64,
    /// Of those, still in the device's buffer (microseconds, the last callback's).
    latency_us: AtomicU64,
    /// The output rate (0 until the device is open).
    rate: AtomicU32,
    /// No more sound comes (ended, none, or failed): the clock goes on without it.
    done: AtomicBool,
    paused: AtomicBool,
    /// f32 bits.
    gain: AtomicU32,
    /// The output stream runs (tests).
    #[cfg(test)]
    stream_on: AtomicBool,
}

/// Sets `done` when dropped: the sound's end, however its thread ends (a panic too).
struct DoneOnDrop(Arc<Audio>);

impl Drop for DoneOnDrop {
    fn drop(&mut self) {
        self.0.done.store(true, Ordering::Release);
    }
}

/// Seconds heard: `played` frames at `rate` Hz less what is still in the device's buffer.
fn heard(played: u64, rate: u32, latency_us: u64) -> f64 {
    (played as f64 / f64::from(rate.max(1)) - latency_us as f64 / 1e6).max(0.0)
}

/// The session's clock (seconds since its start): the sound played while there is sound,
/// else a pausable wall clock carrying on from it.
pub struct Clock {
    audio: Arc<Audio>,
    on_audio: bool,
    base: f64,
    since: Option<Instant>,
}

impl Clock {
    pub fn new(audio: Arc<Audio>) -> Self {
        audio.paused.store(true, Ordering::Relaxed);
        Self {
            audio,
            on_audio: true,
            base: 0.0,
            since: None,
        }
    }

    pub fn now(&mut self) -> f64 {
        if self.on_audio {
            // `done` first: once it is set, `played` is final.
            let done = self.audio.done.load(Ordering::Acquire);
            let rate = self.audio.rate.load(Ordering::Acquire);
            if rate > 0 {
                let played = self.audio.played.load(Ordering::Acquire);
                self.base = heard(played, rate, self.audio.latency_us.load(Ordering::Acquire));
            }
            if self.since.is_some() {
                self.since = Some(Instant::now());
            }
            if !done {
                return self.base;
            }
            self.on_audio = false;
        }
        self.base + self.since.map_or(0.0, |s| s.elapsed().as_secs_f64())
    }

    pub fn set_playing(&mut self, on: bool) {
        self.base = self.now();
        self.since = on.then(Instant::now);
        self.audio.paused.store(!on, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------- processes

/// A started process as [`Procs`] keeps it (tests stand in a hung one).
trait Proc: Send {
    fn kill(&mut self);
    /// Waits for it to exit; true once it is reaped.
    fn wait(&mut self) -> bool;
}

impl Proc for Child {
    fn kill(&mut self) {
        let _ = Child::kill(self);
    }

    fn wait(&mut self) -> bool {
        Child::wait(self).is_ok()
    }
}

/// One session's processes: killed when it stops and reaped on a thread (no orphans, no
/// zombies, no wait on the stopping thread), as is one a worker starts after that; one
/// whose output ended is reaped by its worker. Stopping also cancels the session's
/// download.
#[derive(Default)]
pub struct Procs {
    inner: Mutex<ProcState>,
    /// Set (under `inner`) by `stop`; the download's cancel flag.
    cancel: AtomicBool,
    reaped: Arc<AtomicUsize>,
}

#[derive(Default)]
struct ProcState {
    /// Running ones, by start number.
    children: Vec<(usize, Box<dyn Proc>)>,
    started: usize,
}

impl ProcState {
    fn keep(&mut self, p: Box<dyn Proc>) -> usize {
        let id = self.started;
        self.started += 1;
        self.children.push((id, p));
        id
    }
}

/// A started process: its number (for [`Procs::reap`]) and output.
type Started = (usize, ChildStdout, ChildStderr);

impl Procs {
    /// Starts `cmd` with piped output and no console window; None once stopped.
    fn spawn(&self, mut cmd: Command) -> std::io::Result<Option<Started>> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        // Spawned under the lock: `stop` cannot slip in between spawning and keeping it.
        let mut s = self.inner.lock();
        if self.stopped() {
            return Ok(None);
        }
        let mut child = cmd.spawn()?;
        let pipes = child.stdout.take().zip(child.stderr.take());
        let id = s.keep(Box::new(child));
        Ok(pipes.map(|(out, err)| (id, out, err)))
    }

    pub fn stopped(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Kills every process now and reaps them on a thread (a process stuck in I/O, such
    /// as a read of a hung network file, can take long to exit after the kill).
    // ponytail: kills the process started; an ffmpeg behind a `.cmd`/`.bat` shim (which
    // `find_tool` accepts) would outlive its killed cmd.exe. Kill the process tree (a job
    // object on Windows) if shims show up in practice.
    pub fn stop(&self) {
        let children = {
            let mut s = self.inner.lock();
            self.cancel.store(true, Ordering::SeqCst);
            std::mem::take(&mut s.children)
        };
        if children.is_empty() {
            return;
        }
        let mut children: Vec<Box<dyn Proc>> = children.into_iter().map(|(_, c)| c).collect();
        for c in &mut children {
            c.kill();
        }
        let reaped = self.reaped.clone();
        // ponytail: should the OS refuse the thread, the killed processes stay unreaped
        // (zombies on Unix) until the app exits.
        crate::worker::spawn("keel-video-reap", move || {
            for mut c in children {
                if c.wait() {
                    reaped.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
    }

    /// Process `id`, whose output ended (it is exiting by itself): killed and reaped here
    /// (a worker), not left until the session stops.
    fn reap(&self, id: usize) {
        let child = {
            let mut s = self.inner.lock();
            let at = s.children.iter().position(|(i, _)| *i == id);
            at.map(|at| s.children.swap_remove(at).1)
        };
        if let Some(mut c) = child {
            c.kill();
            if c.wait() {
                self.reaped.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    /// Processes started, and how many of them were waited for.
    #[cfg(test)]
    pub fn counts(&self) -> (usize, usize) {
        let started = self.inner.lock().started;
        (started, self.reaped.load(Ordering::SeqCst))
    }

    /// Keeps `p` as if started here (tests).
    #[cfg(test)]
    fn adopt(&self, p: Box<dyn Proc>) {
        self.inner.lock().keep(p);
    }
}

/// The last 1000 bytes of a process's stderr, read on a thread.
fn stderr_tail(mut err: ChildStderr) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut all = Vec::new();
        let _ = err.read_to_end(&mut all);
        let s = String::from_utf8_lossy(&all);
        let s = s.trim();
        s[s.floor_char_boundary(s.len().saturating_sub(1000))..].to_owned()
    })
}

// ---------------------------------------------------------------- session

#[derive(Clone, Debug)]
pub struct Known {
    pub path: PathBuf,
    pub probe: Probe,
}

pub struct Frame {
    /// Seconds since the session's start.
    pub pts: f64,
    pub image: ColorImage,
}

enum Msg {
    /// The file is resolved and probed (in the player's `known`).
    Ready,
    /// Downloading: bytes done and the size (0: unknown).
    Download(u64, u64),
    Frames(Receiver<Frame>),
    Failed(String),
}

/// Decoding from one position: its processes, frame queue and clock.
struct Session {
    start: f64,
    msgs: Receiver<Msg>,
    frames: Option<Receiver<Frame>>,
    next: Option<Frame>,
    clock: Clock,
    audio: Arc<Audio>,
    procs: Arc<Procs>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.procs.stop();
    }
}

struct Job {
    resolve: Resolve,
    known: Arc<OnceLock<Known>>,
    start: f64,
    area: [f32; 2],
    audio: Arc<Audio>,
    procs: Arc<Procs>,
    msgs: Sender<Msg>,
    ctx: egui::Context,
}

impl Session {
    fn start(job_of: impl FnOnce(Arc<Audio>, Arc<Procs>, Sender<Msg>) -> Job) -> Session {
        let audio = Arc::new(Audio::default());
        let procs = Arc::new(Procs::default());
        let (tx, msgs) = crossbeam_channel::unbounded();
        let job = job_of(audio.clone(), procs.clone(), tx);
        let start = job.start;
        if !crate::worker::spawn("keel-video", move || video_worker(job)) {
            audio.done.store(true, Ordering::Release);
        }
        Session {
            start,
            msgs,
            frames: None,
            next: None,
            clock: Clock::new(audio.clone()),
            audio,
            procs,
        }
    }
}

/// Resolves and probes (first session only), starts the sound, then reads frames into the
/// queue until the end, a full queue whose reader is gone, or a kill.
fn video_worker(job: Job) {
    let fail = |e: String| {
        job.audio.done.store(true, Ordering::Release);
        let _ = job.msgs.send(Msg::Failed(e));
        job.ctx.request_repaint();
    };
    let known = match job.known.get() {
        Some(k) => k.clone(),
        None => {
            // The last report sent: when, and the size it gave.
            let last = std::cell::Cell::new(None::<(Instant, u64)>);
            let progress = |done: u64, total: u64| {
                let soon = last
                    .get()
                    .is_some_and(|(t, size)| t.elapsed() < PROGRESS_EVERY && size == total);
                if soon && done < total {
                    return;
                }
                last.set(Some((Instant::now(), total)));
                let _ = job.msgs.send(Msg::Download(done, total));
                job.ctx.request_repaint();
            };
            let probed = (job.resolve)(&job.procs.cancel, &progress).and_then(|path| {
                let probe = probe_of(&keel_core::ffprobe_json(&path)?);
                Ok(Known { path, probe })
            });
            match probed {
                Ok(k) => {
                    let k = job.known.get_or_init(|| k).clone();
                    let _ = job.msgs.send(Msg::Ready);
                    k
                }
                Err(e) => return fail(format!("{e:#}")),
            }
        }
    };
    let p = &known.probe;
    if p.width == 0 || p.height == 0 {
        return fail("no video stream".into());
    }
    let Some(ffmpeg) = keel_core::find_tool("ffmpeg") else {
        return fail("ffmpeg not found".into());
    };
    if p.audio {
        let (path, ffmpeg, start) = (known.path.clone(), ffmpeg.clone(), job.start);
        let (audio, procs) = (job.audio.clone(), job.procs.clone());
        // Dropped with the closure if the thread cannot start.
        let done = DoneOnDrop(audio.clone());
        crate::worker::spawn("keel-audio", move || {
            let _done = done;
            if let Err(e) = audio_worker(&path, &ffmpeg, start, &audio, &procs) {
                tracing::info!("video sound off: {e:#}");
            }
        });
    } else {
        job.audio.done.store(true, Ordering::Release);
    }
    let [w, h] = decode_size([p.width, p.height], job.area);
    let mut cmd = Command::new(&ffmpeg);
    cmd.args(video_args(&known.path, job.start, [w, h], p.fps));
    let (id, mut out, err) = match job.procs.spawn(cmd) {
        Ok(Some(started)) => started,
        Ok(None) => return,
        Err(e) => return fail(format!("cannot start ffmpeg: {e}")),
    };
    let err = stderr_tail(err);
    let frame_bytes = w as usize * h as usize * 4;
    let (tx, rx) = bounded(queue_len(frame_bytes));
    if job.msgs.send(Msg::Frames(rx)).is_err() {
        return;
    }
    let mut buf = vec![0u8; frame_bytes];
    let mut n = 0u64;
    while out.read_exact(&mut buf).is_ok() {
        let image = ColorImage::from_rgba_premultiplied([w as usize, h as usize], &buf);
        let pts = n as f64 / p.fps;
        if tx.send(Frame { pts, image }).is_err() {
            return;
        }
        n += 1;
        job.ctx.request_repaint();
    }
    drop(out);
    let err = err.join().unwrap_or_default();
    job.procs.reap(id);
    // Nothing at the very start is a failure; past the end it is just the end.
    if n == 0 && job.start <= 0.0 && !job.procs.stopped() {
        fail(if err.is_empty() {
            "ffmpeg produced no frame".into()
        } else {
            err
        });
    }
    job.ctx.request_repaint();
}

/// Runs the output stream while the player plays and pauses it while it does not (a
/// paused stream does not keep the device running).
fn follow(stream: &cpal::Stream, audio: &Audio, on: &mut bool) -> anyhow::Result<()> {
    use cpal::traits::StreamTrait;
    let want = !audio.paused.load(Ordering::Relaxed);
    if want == *on {
        return Ok(());
    }
    if want {
        stream.play()?;
    } else if let Err(e) = stream.pause() {
        // The callback plays silence while paused.
        tracing::debug!("sound output pause: {e}");
    }
    *on = want;
    #[cfg(test)]
    audio.stream_on.store(want, Ordering::Release);
    Ok(())
}

/// Opens the default output device, starts the PCM decoder and feeds the device until the
/// sound ends (then waits for it to drain) or the session stops. Owns the stream: it is
/// dropped on this thread.
fn audio_worker(
    path: &Path,
    ffmpeg: &Path,
    start: f64,
    audio: &Arc<Audio>,
    procs: &Procs,
) -> anyhow::Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no output device"))?;
    // ponytail: f32 output only (WASAPI and CoreAudio mix in f32; ALSA's default device
    // converts); an i16-only device plays silently on the wall clock.
    let config = device.default_output_config()?.config();
    let (rate, channels) = (config.sample_rate, config.channels);
    let (tx, rx) = bounded::<Vec<f32>>(AUDIO_CHUNKS);
    let shared = audio.clone();
    let (mut cur, mut pos) = (Vec::<f32>::new(), 0usize);
    let stream = device.build_output_stream::<f32, _, _>(
        config,
        move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
            let a = &shared;
            let ts = info.timestamp();
            let latency = ts.playback.duration_since(ts.callback).min(MAX_LATENCY);
            a.latency_us
                .store(latency.as_micros() as u64, Ordering::Release);
            if a.paused.load(Ordering::Relaxed) || a.done.load(Ordering::Relaxed) {
                out.fill(0.0);
                return;
            }
            let gain = f32::from_bits(a.gain.load(Ordering::Relaxed));
            let mut i = 0;
            let mut ended = false;
            while i < out.len() {
                if pos == cur.len() {
                    match rx.try_recv() {
                        Ok(c) => (cur, pos) = (c, 0),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            ended = true;
                            break;
                        }
                    }
                }
                let n = (out.len() - i).min(cur.len() - pos);
                for (o, s) in out[i..i + n].iter_mut().zip(&cur[pos..pos + n]) {
                    *o = s * gain;
                }
                (i, pos) = (i + n, pos + n);
            }
            out[i..].fill(0.0);
            a.played
                .fetch_add((i / usize::from(channels.max(1))) as u64, Ordering::AcqRel);
            if ended {
                a.done.store(true, Ordering::Release);
            }
        },
        {
            let audio = audio.clone();
            move |e| {
                tracing::info!("sound output: {e}");
                audio.done.store(true, Ordering::Release);
            }
        },
        None,
    )?;
    audio.rate.store(rate, Ordering::Release);
    let mut on = false;
    follow(&stream, audio, &mut on)?;
    let mut cmd = Command::new(ffmpeg);
    cmd.args(audio_args(path, start, rate, channels));
    let Some((id, mut out, err)) = procs.spawn(cmd)? else {
        return Ok(());
    };
    drop(stderr_tail(err));
    // Whole frames per chunk; a partial sample or frame waits for the next read.
    let chunk_bytes = AUDIO_CHUNK_FRAMES * usize::from(channels) * 4;
    let mut buf = vec![0u8; chunk_bytes];
    let mut have = 0;
    loop {
        follow(&stream, audio, &mut on)?;
        let n = match out.read(&mut buf[have..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        have += n;
        let whole = have - have % (usize::from(channels) * 4);
        if whole == 0 {
            continue;
        }
        let mut chunk: Vec<f32> = (buf[..whole].as_chunks::<4>().0.iter())
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        buf.copy_within(whole..have, 0);
        have -= whole;
        // A paused stream takes nothing: wait, but notice a stop.
        loop {
            match tx.send_timeout(chunk, Duration::from_millis(50)) {
                Ok(()) => break,
                Err(crossbeam_channel::SendTimeoutError::Timeout(c)) if !procs.stopped() => {
                    follow(&stream, audio, &mut on)?;
                    chunk = c
                }
                Err(_) => return Ok(()),
            }
        }
    }
    drop(out);
    procs.reap(id);
    drop(tx);
    // Keep the stream until the callback has played what is queued.
    while !audio.done.load(Ordering::Acquire) && !procs.stopped() {
        follow(&stream, audio, &mut on)?;
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

// ---------------------------------------------------------------- player

/// Playback of one file in the viewer.
pub struct VideoPlayer {
    resolve: Resolve,
    /// The file and its probe once the first session resolved it (the sessions share it:
    /// a restart never resolves, that is downloads, again).
    known: Arc<OnceLock<Known>>,
    /// Where to restart once the first session has resolved the file (a seek before).
    pending: Option<f64>,
    /// While the file downloads: bytes done and its size (0: unknown).
    pub download: Option<(u64, u64)>,
    pub transport: Transport,
    session: Option<Session>,
    tex: Option<TextureHandle>,
    /// Physical pixels the video is decoded for (the viewer's image area).
    area: [f32; 2],
    /// Why playback failed (the viewer falls back to still frames).
    pub error: Option<String>,
    ctx: egui::Context,
    #[cfg(test)]
    pub shown: usize,
}

impl VideoPlayer {
    /// A paused player at `at` seconds, keeping `prefs`' volume and loop.
    pub fn new(
        ctx: egui::Context,
        resolve: Resolve,
        prefs: &Transport,
        at: f64,
        area: [f32; 2],
    ) -> Self {
        let mut p = Self {
            resolve,
            known: Arc::default(),
            pending: None,
            download: None,
            transport: Transport {
                playing: false,
                ended: false,
                ..prefs.clone()
            },
            session: None,
            tex: None,
            area,
            error: None,
            ctx,
            #[cfg(test)]
            shown: 0,
        };
        p.restart(at);
        p
    }

    pub fn command(&mut self, cmd: Cmd) {
        let duration = self.duration().unwrap_or(0.0);
        if let Some(at) = self.transport.apply(cmd, duration) {
            self.restart(at);
        }
        if let Some(s) = &mut self.session {
            s.clock.set_playing(self.transport.playing);
            s.audio
                .gain
                .store(self.transport.gain().to_bits(), Ordering::Relaxed);
        }
    }

    /// Kills the running decoders and starts new ones at `at` seconds; while the first
    /// session still resolves the file (downloads it), only once that is done.
    fn restart(&mut self, at: f64) {
        if self.session.is_some() && self.known.get().is_none() && self.error.is_none() {
            self.pending = Some(at);
            return;
        }
        self.pending = None;
        self.session = None;
        let (resolve, known, area, ctx) = (
            self.resolve.clone(),
            self.known.clone(),
            self.area,
            self.ctx.clone(),
        );
        let mut s = Session::start(|audio, procs, msgs| Job {
            resolve,
            known,
            start: at,
            area,
            audio,
            procs,
            msgs,
            ctx,
        });
        s.clock.set_playing(self.transport.playing);
        s.audio
            .gain
            .store(self.transport.gain().to_bits(), Ordering::Relaxed);
        self.session = Some(s);
    }

    /// Seconds (once probed).
    pub fn duration(&self) -> Option<f64> {
        self.known
            .get()
            .map(|k| k.probe.duration)
            .filter(|d| *d > 0.0)
    }

    /// "Downloading…", with the progress when the provider reports it, while the file
    /// downloads.
    pub fn download_text(&self) -> Option<String> {
        let (done, total) = self.download?;
        let size = crate::view_details::size_text_of;
        Some(match total {
            0 => "Downloading…".into(),
            _ => format!("Downloading… {} of {}", size(done), size(total)),
        })
    }

    /// The playing position in seconds (a seek waiting for the download: its target).
    pub fn position(&mut self) -> f64 {
        if let Some(at) = self.pending {
            return at;
        }
        let at = self
            .session
            .as_mut()
            .map_or(0.0, |s| s.start + s.clock.now());
        self.duration().map_or(at, |d| at.min(d))
    }

    /// Per UI frame: takes worker news, shows the due frame (uploaded into the one
    /// texture), handles the end. Returns the texture and its size.
    pub fn frame(&mut self, area: [f32; 2]) -> Option<(egui::TextureId, Vec2)> {
        self.area = area;
        let mut ready = false;
        if let Some(s) = &mut self.session {
            while let Ok(m) = s.msgs.try_recv() {
                match m {
                    Msg::Ready => ready = true,
                    Msg::Download(done, total) => self.download = Some((done, total)),
                    Msg::Frames(rx) => s.frames = Some(rx),
                    Msg::Failed(e) => self.error = Some(e),
                }
            }
        }
        if ready || self.error.is_some() {
            self.download = None;
        }
        if ready {
            if let Some(at) = self.pending.take() {
                self.restart(at);
            }
        }
        let mut ended = false;
        if let Some(s) = &mut self.session {
            let t = s.clock.now();
            let mut gone = false;
            let due = match &s.frames {
                Some(rx) => {
                    let pull = || match rx.try_recv() {
                        Ok(f) => Some(f),
                        Err(TryRecvError::Disconnected) => {
                            gone = true;
                            None
                        }
                        Err(TryRecvError::Empty) => None,
                    };
                    take_due(&mut s.next, pull, |f: &Frame| f.pts, t).0
                }
                None => None,
            };
            if let Some(f) = due {
                match &mut self.tex {
                    Some(tex) if tex.size() == f.image.size => {
                        tex.set(f.image, TextureOptions::LINEAR)
                    }
                    _ => {
                        self.tex = Some(self.ctx.load_texture(
                            "keel-video",
                            f.image,
                            TextureOptions::LINEAR,
                        ))
                    }
                }
                #[cfg(test)]
                {
                    self.shown += 1;
                }
            }
            ended = gone && s.next.is_none() && s.audio.done.load(Ordering::Acquire);
        }
        if ended && self.transport.playing {
            self.command(Cmd::Ended);
        }
        if self.transport.playing {
            self.ctx.request_repaint();
        }
        self.tex.as_ref().map(|t| (t.id(), t.size_vec2()))
    }

    /// Seconds of sound played in this session (tests).
    #[cfg(test)]
    pub fn sound_played(&self) -> f64 {
        let a = &self.session.as_ref().unwrap().audio;
        let rate = a.rate.load(Ordering::Acquire);
        a.played.load(Ordering::Acquire) as f64 / f64::from(rate.max(1))
    }

    #[cfg(test)]
    pub fn procs(&self) -> Arc<Procs> {
        self.session.as_ref().unwrap().procs.clone()
    }

    #[cfg(test)]
    fn audio(&self) -> Arc<Audio> {
        self.session.as_ref().unwrap().audio.clone()
    }
}

#[cfg(test)]
#[path = "video_player_tests.rs"]
mod tests;
