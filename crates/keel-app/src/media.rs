//! Media view support (Task 32, spec 2.10 "Media"): sidecar textures for the media grid and
//! the viewer.
//!
//! The UI thread only looks textures up and uploads at most [`MAX_UPLOADS`] decoded images
//! per frame. Off the UI thread, [`LOADERS`] threads read existing sidecars and decode them;
//! [`MAKERS`] threads make missing ones (`Sidecars::ensure`). Both take work from one
//! priority queue that holds only what a view still wants: visible tiles first, then the
//! prefetch rows; tiles that scrolled away are dropped before they are decoded or made.
//! With the library open the sidecars are the library's (the sidecar job fills them, so
//! most tiles are instant); without it they live in a cache under the cache folder.

use egui::{pos2, ColorImage, Rect, TextureHandle, Vec2};
use keel_core::{Library, MediaMeta, SidecarKey, SidecarKind, Sidecars};
use keel_vfs::{Entry, Kind, Router, VPath};
use parking_lot::{Condvar, Mutex, RwLock};
use std::collections::HashMap;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Texture uploads per frame (review focus 1: no frame pays for a burst of arrivals).
pub const MAX_UPLOADS: usize = 8;
/// Textures kept (least recently drawn evicted first)...
pub const TEXTURE_CACHE: usize = 4000;
/// ...and their pixel bytes (RGBA): 384 MiB of GPU memory at most.
pub const TEXTURE_BYTES: usize = 384 << 20;
/// Threads reading and decoding existing sidecars.
pub const LOADERS: usize = 2;
/// Threads making missing sidecars (decode, ffmpeg).
pub const MAKERS: usize = 4;
/// `Media::want` slots: pane 0's grid, pane 1's grid, the viewer.
pub const VIEWER_SLOT: usize = 2;
const SLOTS: usize = 3;
/// Frames per video strip (keel-core's `media::STRIP_FRAMES`, not exported).
pub const STRIP_FRAMES: u32 = 20;
/// The sidecar cache used while the library is off.
const CACHE_BUDGET: u64 = 2 << 30;
/// Library record lookups (content ids per folder) are reused this long.
const RECORDS_TTL: Duration = Duration::from_secs(60);
const DAYS_CHUNK: usize = 2000;

pub const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff", "heic", "heif",
];
pub const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "mov", "avi", "wmv", "webm", "m4v", "mpg", "mpeg", "flv", "3gp",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaType {
    Image,
    Video,
}

/// What sidecars can be made for (keel-core's extension lists); never folders or locked
/// archive entries.
pub fn media_type(e: &Entry) -> Option<MediaType> {
    if e.kind == Kind::Dir || e.encrypted {
        return None;
    }
    let ext = e.ext.as_str();
    if IMAGE_EXTS.contains(&ext) {
        Some(MediaType::Image)
    } else if VIDEO_EXTS.contains(&ext) {
        Some(MediaType::Video)
    } else {
        None
    }
}

/// Media tile size (points); Ctrl+wheel or the view's header buttons change it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TileSize {
    S,
    #[default]
    M,
    L,
}

impl TileSize {
    pub const ALL: [TileSize; 3] = [TileSize::S, TileSize::M, TileSize::L];

    pub fn points(self) -> f32 {
        match self {
            TileSize::S => 96.0,
            TileSize::M => 160.0,
            TileSize::L => 256.0,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TileSize::S => "S",
            TileSize::M => "M",
            TileSize::L => "L",
        }
    }

    pub fn step(self, bigger: bool) -> TileSize {
        match (self, bigger) {
            (TileSize::S, true) | (TileSize::L, false) => TileSize::M,
            (TileSize::M, true) | (TileSize::L, true) => TileSize::L,
            (TileSize::M, false) | (TileSize::S, false) => TileSize::S,
        }
    }
}

/// One texture: a file's sidecar of `kind`, decoded at `px` (shorter side, 0 = as stored).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TexKey {
    pub path: VPath,
    pub mtime: i64,
    pub size: u64,
    pub kind: SidecarKind,
    pub px: u32,
}

impl TexKey {
    pub fn of(e: &Entry, kind: SidecarKind, px: u32) -> TexKey {
        TexKey {
            path: e.path.clone(),
            mtime: unix(e.modified),
            size: e.size,
            kind,
            px,
        }
    }
}

pub fn unix(t: Option<std::time::SystemTime>) -> i64 {
    // Nanoseconds, exactly as keel-core stores a record's mtime: the sidecar job and
    // the grid must build the same key for the same file.
    t.map_or(0, keel_core::unix_ns)
}

/// What a worker needs for one file: the entry and its real path (`library://` resolved).
#[derive(Clone, Debug)]
pub struct Req {
    pub entry: Entry,
    pub real: VPath,
}

/// The sidecar to show in a tile `tile_px` physical pixels wide: the video strip, else
/// the 256 px thumbnail, or the 1024 px one for big HiDPI tiles; decoded at tile size (a
/// strip to the tile's height).
pub fn tile_key(e: &Entry, video: bool, tile_px: u32) -> TexKey {
    if video {
        TexKey::of(e, SidecarKind::Strip, tile_px)
    } else if tile_px > 256 {
        TexKey::of(e, SidecarKind::Thumb1024, tile_px)
    } else {
        TexKey::of(e, SidecarKind::Thumb256, tile_px)
    }
}

// ---------------------------------------------------------------- priority queue

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Read an existing sidecar and decode it.
    Load,
    /// Make the missing sidecar first.
    Make,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Queued(Stage),
    /// A worker has it.
    Busy,
    /// Answered; kept so a view still listing it does not queue it again.
    Done,
}

struct Item<V> {
    prio: u64,
    state: State,
    v: V,
}

struct QState<K, V> {
    items: HashMap<K, Item<V>>,
    /// Per slot: the keys it wants.
    slots: Vec<HashMap<K, u64>>,
    closed: bool,
}

/// Requests ordered by priority (lower first), owned by the views that want them: when no
/// slot wants a queued key any more it is dropped unrun.
pub struct Queue<K, V> {
    s: Mutex<QState<K, V>>,
    cv: Condvar,
}

impl<K: Clone + Eq + Hash, V: Clone> Queue<K, V> {
    pub fn new(slots: usize) -> Self {
        Self {
            s: Mutex::new(QState {
                items: HashMap::new(),
                slots: (0..slots).map(|_| HashMap::new()).collect(),
                closed: false,
            }),
            cv: Condvar::new(),
        }
    }

    /// Replaces what `slot` wants. New keys are queued for loading; keys no slot wants
    /// any more are dropped unless a worker has them.
    pub fn want(&self, slot: usize, list: Vec<(K, u64, V)>) {
        let mut s = self.s.lock();
        let mut map = HashMap::with_capacity(list.len());
        for (k, prio, v) in list {
            map.insert(k.clone(), prio);
            match s.items.get_mut(&k) {
                // ponytail: the latest caller's priority wins when two slots want a key.
                Some(item) => item.prio = prio,
                None => {
                    s.items.insert(
                        k,
                        Item {
                            prio,
                            state: State::Queued(Stage::Load),
                            v,
                        },
                    );
                }
            }
        }
        let old = std::mem::replace(&mut s.slots[slot], map);
        for k in old.keys() {
            if !s.slots.iter().any(|m| m.contains_key(k))
                && s.items.get(k).is_some_and(|i| i.state != State::Busy)
            {
                s.items.remove(k);
            }
        }
        drop(s);
        self.cv.notify_all();
    }

    fn wanted(s: &QState<K, V>, k: &K) -> bool {
        s.slots.iter().any(|m| m.contains_key(k))
    }

    /// The most urgent request of `stage`, without waiting.
    #[cfg(test)]
    pub fn try_pop(&self, stage: Stage) -> Option<(K, V)> {
        Self::take(&mut self.s.lock(), stage)
    }

    // ponytail: a linear scan per pop over at most a few thousand wanted keys; a heap with
    // lazy deletion if the prefetch window ever grows past that.
    fn take(s: &mut QState<K, V>, stage: Stage) -> Option<(K, V)> {
        let k = s
            .items
            .iter()
            .filter(|(_, i)| i.state == State::Queued(stage))
            .min_by_key(|(_, i)| i.prio)
            .map(|(k, _)| k.clone())?;
        let item = s.items.get_mut(&k)?;
        item.state = State::Busy;
        Some((k, item.v.clone()))
    }

    /// Blocks for the most urgent request of `stage`; None once closed.
    pub fn pop(&self, stage: Stage) -> Option<(K, V)> {
        let mut s = self.s.lock();
        loop {
            if s.closed {
                return None;
            }
            if let Some(x) = Self::take(&mut s, stage) {
                return Some(x);
            }
            self.cv.wait(&mut s);
        }
    }

    /// A loaded key whose sidecar is missing: queued for making if still wanted.
    pub fn promote(&self, k: &K) -> bool {
        let mut s = self.s.lock();
        if !Self::wanted(&s, k) {
            s.items.remove(k);
            return false;
        }
        if let Some(i) = s.items.get_mut(k) {
            i.state = State::Queued(Stage::Make);
        }
        drop(s);
        self.cv.notify_all();
        true
    }

    /// A worker answered `k`.
    pub fn done(&self, k: &K) {
        let mut s = self.s.lock();
        if Self::wanted(&s, k) {
            if let Some(i) = s.items.get_mut(k) {
                i.state = State::Done;
            }
        } else {
            s.items.remove(k);
        }
    }

    /// Answered keys whose texture was evicted: wanted again, they queue again.
    pub fn forget(&self, keys: &[K]) {
        let mut s = self.s.lock();
        for k in keys {
            if s.items.get(k).is_some_and(|i| i.state == State::Done) {
                s.items.remove(k);
            }
        }
    }

    pub fn close(&self) {
        self.s.lock().closed = true;
        self.cv.notify_all();
    }

    #[cfg(test)]
    pub fn queued(&self) -> usize {
        let s = self.s.lock();
        (s.items.values())
            .filter(|i| matches!(i.state, State::Queued(_)))
            .count()
    }
}

// ---------------------------------------------------------------- workers

/// A worker's answer: None = no image can be made (not media, corrupt, no ffmpeg).
pub struct Loaded {
    pub key: TexKey,
    pub image: Option<ColorImage>,
}

/// A library record as the sidecar job keyed it.
#[derive(Clone, Copy, Debug)]
struct Rec {
    cas: Option<[u8; 32]>,
    mtime: i64,
    size: u64,
}

type Records = HashMap<(PathBuf, String), (Instant, Arc<HashMap<String, Rec>>)>;

/// Where the file's bytes come from when its sidecar has to be made.
enum Src {
    Local(PathBuf),
    Remote(VPath),
}

pub(crate) struct Shared {
    pub(crate) queue: Queue<TexKey, Req>,
    lib: RwLock<Option<Arc<Library>>>,
    /// The library-off store: None until first used, Some(None) when it cannot open.
    cache: Mutex<Option<Option<Arc<Sidecars>>>>,
    pub(crate) router: Arc<Router>,
    /// Setting `remote_thumbnails`.
    remote: AtomicBool,
    tx: crossbeam_channel::Sender<Loaded>,
    ctx: egui::Context,
    records: Mutex<Records>,
}

impl Shared {
    /// The library's sidecar store, else the cache store (opened here: workers only).
    pub(crate) fn store(&self) -> Option<Arc<Sidecars>> {
        if let Some(lib) = self.lib.read().clone() {
            match lib.sidecars() {
                Ok(s) => return Some(s),
                Err(e) => tracing::warn!("library sidecars: {e:#}"),
            }
        }
        self.cache
            .lock()
            .get_or_insert_with(|| {
                let dir = keel_vfs::cache_dir().join("media-cache");
                Sidecars::open(&dir, CACHE_BUDGET)
                    .map_err(|e| tracing::warn!("media cache {}: {e:#}", dir.display()))
                    .ok()
                    .map(Arc::new)
            })
            .clone()
    }

    /// The sidecar key of `req` exactly as the sidecar job builds it for library records
    /// (source root joined with the record path, the record's content id while the file
    /// is unchanged), else by path. None: no sidecar may be made (remote files with remote
    /// thumbnails off, big archive entries) unless `explicit` (the viewer).
    fn resolve(&self, req: &Req, explicit: bool) -> Option<(SidecarKey, Src)> {
        let e = &req.entry;
        let (mtime, size) = (unix(e.modified), e.size);
        if let Some(local) = req.real.to_local_path() {
            let lib = self.lib.read().clone();
            if let Some((src, rel)) = lib.as_ref().and_then(|l| l.source_for(&req.real)) {
                if let Some(root) = src.def.root.to_local_path() {
                    let path = root.join(&rel);
                    let cas = self
                        .record(src.store_dir(), &rel)
                        .filter(|r| r.mtime == mtime && r.size == size)
                        .and_then(|r| r.cas);
                    return Some((
                        SidecarKey::local(&path, mtime, size, cas),
                        Src::Local(local),
                    ));
                }
            }
            return Some((
                SidecarKey::local(&local, mtime, size, None),
                Src::Local(local),
            ));
        }
        if !explicit {
            if crate::remotes::is_network(&req.real) && !self.remote.load(Ordering::Relaxed) {
                return None;
            }
            // Bounded like the preview: never materialise what the previewer would refuse.
            if size > keel_preview::MAX_PREVIEW_BYTES && req.real.split_archive().is_some() {
                return None;
            }
        }
        let key = SidecarKey::local(Path::new(&req.real.display()), mtime, size, None);
        Some((key, Src::Remote(req.real.clone())))
    }

    /// `rel`'s record in the source store at `store_dir` (its folder's records are read
    /// once per `RECORDS_TTL`).
    // ponytail: reads source.db directly (keel-core keeps the store private); a keel-core
    // `Library::sidecar_key(&VPath)` should replace this.
    fn record(&self, store_dir: &Path, rel: &str) -> Option<Rec> {
        let (parent, name) = rel.rsplit_once('/').unwrap_or(("", rel));
        let key = (store_dir.to_owned(), parent.to_owned());
        let cached = self
            .records
            .lock()
            .get(&key)
            .filter(|(at, _)| at.elapsed() < RECORDS_TTL)
            .map(|(_, m)| m.clone());
        let map = match cached {
            Some(m) => m,
            None => {
                let m = Arc::new(
                    read_children(store_dir, parent)
                        .map_err(|e| tracing::debug!("records of {parent}: {e:#}"))
                        .unwrap_or_default(),
                );
                let mut records = self.records.lock();
                records.retain(|_, (at, _)| at.elapsed() < RECORDS_TTL);
                records.insert(key, (Instant::now(), m.clone()));
                m
            }
        };
        map.get(name).copied().or_else(|| {
            cfg!(windows)
                .then(|| map.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)))
                .flatten()
                .map(|(_, r)| *r)
        })
    }

    fn finish(&self, key: TexKey, image: Option<ColorImage>) {
        self.queue.done(&key);
        let _ = self.tx.send(Loaded { key, image });
        self.ctx.request_repaint();
    }

    fn local(&self, src: Src) -> Option<PathBuf> {
        match src {
            Src::Local(p) => Some(p),
            Src::Remote(v) => self
                .router
                .provider_for(&v)?
                .local_copy(&v)
                .map_err(|e| tracing::debug!("{}: {e:#}", v.display()))
                .ok(),
        }
    }

    /// The file's metadata: its `meta.json`, made when missing (a download for remote
    /// files only when `explicit`).
    pub(crate) fn meta(&self, req: &Req, explicit: bool) -> Option<MediaMeta> {
        let store = self.store()?;
        let (key, src) = self.resolve(req, explicit)?;
        if let Some(m) = store.meta(&key) {
            return Some(m);
        }
        let local = self.local(src)?;
        let path = store.ensure(&key, SidecarKind::Meta, &local).ok()?;
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }
}

/// `parent`'s file records (name -> content id, mtime, size) from a source store.
fn read_children(store_dir: &Path, parent: &str) -> anyhow::Result<HashMap<String, Rec>> {
    use rusqlite::{params, OpenFlags, OptionalExtension};
    let c = rusqlite::Connection::open_with_flags(
        store_dir.join("source.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    c.busy_timeout(Duration::from_secs(1))?;
    let mut id: Option<i64> = c
        .query_row(
            "SELECT id FROM record WHERE parent IS NULL ORDER BY id LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    for part in parent.split('/').filter(|p| !p.is_empty()) {
        let Some(pid) = id else { break };
        let collate = if cfg!(windows) { "COLLATE NOCASE" } else { "" };
        id = c
            .query_row(
                &format!(
                    "SELECT id FROM record WHERE parent = ?1 AND name = ?2 {collate}
                     ORDER BY name = ?2 DESC, id LIMIT 1"
                ),
                params![pid, part],
                |r| r.get(0),
            )
            .optional()?;
    }
    let Some(id) = id else {
        return Ok(HashMap::new());
    };
    let mut stmt = c.prepare(
        "SELECT name, cas_id, coalesce(mtime, 0), size FROM record WHERE parent = ?1 AND kind = 0",
    )?;
    let rows = stmt.query_map([id], |r| {
        let cas: Option<Vec<u8>> = r.get(1)?;
        Ok((
            r.get::<_, String>(0)?,
            Rec {
                cas: cas.and_then(|c| c.try_into().ok()),
                mtime: r.get(2)?,
                size: r.get::<_, i64>(3)? as u64,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn load_loop(sh: Arc<Shared>) {
    while let Some((key, req)) = sh.queue.pop(Stage::Load) {
        let Some((store, (sk, _))) = sh.store().zip(sh.resolve(&req, false)) else {
            sh.finish(key, None);
            continue;
        };
        let pin = store.pin(&sk);
        match store.get(&sk, key.kind) {
            Some(path) => {
                let image = decode_file(&path, key.px);
                drop(pin);
                sh.finish(key, image);
            }
            // A kind that failed before (recorded in its meta.json) is not tried again.
            None if store
                .meta(&sk)
                .is_some_and(|m| m.failure(key.kind).is_some()) =>
            {
                sh.finish(key, None)
            }
            None => {
                sh.queue.promote(&key);
            }
        }
    }
}

fn make_loop(sh: Arc<Shared>) {
    while let Some((key, req)) = sh.queue.pop(Stage::Make) {
        let image = (|| {
            let store = sh.store()?;
            let (sk, src) = sh.resolve(&req, false)?;
            // Pinned from before it is made until it is decoded: eviction cannot delete a
            // sidecar between `ensure` and the read.
            let _pin = store.pin(&sk);
            let local = sh.local(src)?;
            let path = store
                .ensure(&sk, key.kind, &local)
                .map_err(|e| tracing::debug!("sidecar for {}: {e:#}", req.real.display()))
                .ok()?;
            #[cfg(test)]
            if let Some(hook) = &*AFTER_ENSURE.lock() {
                hook();
            }
            decode_file(&path, key.px)
        })();
        sh.finish(key, image);
    }
}

/// Runs between a maker's `ensure` and its decode (tests: eviction right there).
#[cfg(test)]
pub(crate) static AFTER_ENSURE: Mutex<Option<Box<dyn Fn() + Send + Sync>>> = Mutex::new(None);

pub fn decode_file(path: &Path, px: u32) -> Option<ColorImage> {
    let bytes = std::fs::read(path).ok()?;
    let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::WebP).ok()?;
    Some(color_image(img, px))
}

/// `img` scaled down so its shorter side is `px` (0: as is), as an egui image.
pub fn color_image(img: image::DynamicImage, px: u32) -> ColorImage {
    let short = img.width().min(img.height()).max(1);
    let img = if px > 0 && short > px {
        let s = px as f32 / short as f32;
        let w = ((img.width() as f32 * s).round() as u32).max(1);
        let h = ((img.height() as f32 * s).round() as u32).max(1);
        img.thumbnail_exact(w, h)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], &rgba)
}

// ---------------------------------------------------------------- UI side

/// A texture lookup.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tex {
    /// Texture and its size in pixels.
    Ready(egui::TextureId, Vec2),
    /// No image can be made: draw the file icon.
    Failed,
    /// Not loaded (yet): draw the placeholder.
    Missing,
}

/// Day groups for date headers: `name -> day`, filled by a worker in chunks.
struct DayMap {
    gen: u64,
    days: HashMap<String, i64>,
    cancel: Arc<AtomicBool>,
}

type DayChunk = (VPath, u64, Vec<(String, i64)>);

pub struct Media {
    pub(crate) sh: Arc<Shared>,
    rx: crossbeam_channel::Receiver<Loaded>,
    /// Texture (None: failed), last use, pixel bytes.
    cache: HashMap<TexKey, (Option<TextureHandle>, u64, usize)>,
    /// Pixel bytes of the cached textures, and the most kept (`TEXTURE_BYTES`).
    bytes: usize,
    pub(crate) max_bytes: usize,
    clock: u64,
    /// Per slot: the signature of the list last handed to `want` (rebuilt on change only).
    pub sig: [u64; SLOTS],
    pub tile: TileSize,
    pub dates: bool,
    lib_ptr: usize,
    days: HashMap<VPath, DayMap>,
    days_tx: crossbeam_channel::Sender<DayChunk>,
    days_rx: crossbeam_channel::Receiver<DayChunk>,
}

impl Media {
    pub fn new(ctx: egui::Context, router: Arc<Router>) -> Self {
        Self::with_threads(ctx, router, LOADERS, MAKERS)
    }

    fn with_threads(
        ctx: egui::Context,
        router: Arc<Router>,
        loaders: usize,
        makers: usize,
    ) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let sh = Arc::new(Shared {
            queue: Queue::new(SLOTS),
            lib: RwLock::new(None),
            cache: Mutex::new(None),
            router,
            remote: AtomicBool::new(false),
            tx,
            ctx,
            records: Mutex::default(),
        });
        for _ in 0..loaders {
            let sh = sh.clone();
            crate::worker::spawn("keel-media-load", move || load_loop(sh));
        }
        for _ in 0..makers {
            let sh = sh.clone();
            crate::worker::spawn("keel-media-make", move || make_loop(sh));
        }
        let (days_tx, days_rx) = crossbeam_channel::unbounded();
        Self {
            sh,
            rx,
            cache: HashMap::new(),
            bytes: 0,
            max_bytes: TEXTURE_BYTES,
            clock: 0,
            sig: [0; SLOTS],
            tile: TileSize::default(),
            dates: false,
            lib_ptr: 0,
            days: HashMap::new(),
            days_tx,
            days_rx,
        }
    }

    /// Follows the open library (its sidecars and records; None: the cache store).
    pub fn sync_library(&mut self, lib: Option<&Arc<Library>>) {
        let ptr = lib.map_or(0, |l| Arc::as_ptr(l) as usize);
        if ptr != self.lib_ptr {
            self.lib_ptr = ptr;
            *self.sh.lib.write() = lib.cloned();
            self.sh.records.lock().clear();
        }
    }

    #[cfg(test)]
    pub fn with_threads_for_tests(ctx: egui::Context, router: Arc<Router>) -> Self {
        Self::with_threads(ctx, router, 0, 0)
    }

    #[cfg(test)]
    pub fn queued_for_tests(&self) -> usize {
        self.sh.queue.queued()
    }

    /// Uses `store` while the library is off (tests).
    #[cfg(test)]
    pub fn set_store(&self, store: Arc<Sidecars>) {
        *self.sh.cache.lock() = Some(Some(store));
    }

    pub fn set_remote(&self, on: bool) {
        self.sh.remote.store(on, Ordering::Relaxed);
    }

    /// The texture for `key` (marks it used).
    pub fn get(&mut self, key: &TexKey) -> Tex {
        self.clock += 1;
        match self.cache.get_mut(key) {
            Some((tex, used, _)) => {
                *used = self.clock;
                match tex {
                    Some(t) => Tex::Ready(t.id(), t.size_vec2()),
                    None => Tex::Failed,
                }
            }
            None => Tex::Missing,
        }
    }

    pub fn has(&self, key: &TexKey) -> bool {
        self.cache.contains_key(key)
    }

    /// A media tile's texture: a video shows its strip, and its 256 px thumbnail until the
    /// strip is there or when the strip cannot be made. Also says whether it is the strip.
    pub fn tile_tex(&mut self, e: &Entry, video: bool, tile_px: u32) -> (Tex, bool) {
        let main = self.get(&tile_key(e, video, tile_px));
        if !video || matches!(main, Tex::Ready(..)) {
            return (main, video);
        }
        let thumb = match self.get(&thumb_key(e, tile_px)) {
            Tex::Failed if main != Tex::Failed => Tex::Missing,
            t => t,
        };
        (thumb, false)
    }

    /// Replaces what `slot` wants loaded: `(key, priority, request)`, lower first.
    pub fn want(&self, slot: usize, list: Vec<(TexKey, u64, Req)>) {
        self.sh.queue.want(slot, list);
    }

    /// Per frame: uploads at most `MAX_UPLOADS` decoded images, keeps the cache bounded,
    /// merges date chunks. Returns the uploads made.
    pub fn upload(&mut self, ctx: &egui::Context) -> usize {
        let mut n = 0;
        while n < MAX_UPLOADS {
            let Ok(loaded) = self.rx.try_recv() else {
                break;
            };
            let bytes = loaded.image.as_ref().map_or(0, |i| i.pixels.len() * 4);
            let tex = loaded.image.map(|img| {
                ctx.load_texture(loaded.key.path.display(), img, egui::TextureOptions::LINEAR)
            });
            self.clock += 1;
            self.bytes += bytes;
            if let Some((_, _, old)) = self.cache.insert(loaded.key, (tex, self.clock, bytes)) {
                self.bytes -= old;
            }
            n += 1;
        }
        if !self.rx.is_empty() {
            ctx.request_repaint();
        }
        self.evict();
        self.merge_days();
        n
    }

    /// Over `TEXTURE_CACHE` textures or `max_bytes`: drops the least recently drawn until
    /// 90 % of both.
    fn evict(&mut self) {
        if self.cache.len() <= TEXTURE_CACHE && self.bytes <= self.max_bytes {
            return;
        }
        let mut by_use: Vec<(u64, usize, TexKey)> = (self.cache.iter())
            .map(|(k, (_, used, bytes))| (*used, *bytes, k.clone()))
            .collect();
        by_use.sort_unstable_by_key(|(used, ..)| *used);
        let (n_cap, bytes_cap) = (TEXTURE_CACHE * 9 / 10, self.max_bytes / 10 * 9);
        let mut gone = Vec::new();
        for (_, bytes, key) in by_use {
            if self.cache.len() <= n_cap && self.bytes <= bytes_cap {
                break;
            }
            self.cache.remove(&key);
            self.bytes -= bytes;
            gone.push(key);
        }
        self.sh.queue.forget(&gone);
    }

    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    // --- date headers ---

    /// The day (unix days, UTC) of each entry of `dir`'s listing `gen` by name, filled in
    /// on a worker as it reads each file's `taken_at` (else its modified time). `reqs` is
    /// called once per listing.
    pub fn days(
        &mut self,
        dir: &VPath,
        gen: u64,
        reqs: impl FnOnce() -> Vec<(String, Req)>,
    ) -> &HashMap<String, i64> {
        let fresh = self.days.get(dir).is_none_or(|d| d.gen != gen);
        if fresh {
            if let Some(old) = self.days.remove(dir) {
                old.cancel.store(true, Ordering::Relaxed);
            }
            let cancel = Arc::new(AtomicBool::new(false));
            let reqs = reqs();
            let (sh, tx, stop, dir2) = (
                self.sh.clone(),
                self.days_tx.clone(),
                cancel.clone(),
                dir.clone(),
            );
            crate::worker::spawn("keel-media-days", move || {
                for chunk in reqs.chunks(DAYS_CHUNK) {
                    let mut out = Vec::with_capacity(chunk.len());
                    for (name, req) in chunk {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        out.push((name.clone(), day_of(&sh, req)));
                    }
                    if tx.send((dir2.clone(), gen, out)).is_err() {
                        return;
                    }
                    sh.ctx.request_repaint();
                }
            });
            self.days.insert(
                dir.clone(),
                DayMap {
                    gen,
                    days: HashMap::new(),
                    cancel,
                },
            );
        }
        &self.days[dir].days
    }

    fn merge_days(&mut self) {
        while let Ok((dir, gen, chunk)) = self.days_rx.try_recv() {
            if let Some(d) = self.days.get_mut(&dir).filter(|d| d.gen == gen) {
                d.days.extend(chunk);
            }
        }
    }

    /// Drops the date maps of folders no pane shows with date headers (their workers stop);
    /// a shown folder keeps its map however long the app sits idle.
    pub fn keep_days(&mut self, shown: &[&VPath]) {
        self.days.retain(|dir, d| {
            let keep = shown.contains(&dir);
            if !keep {
                d.cancel.store(true, Ordering::Relaxed);
            }
            keep
        });
    }

    #[cfg(test)]
    pub fn has_days(&self, dir: &VPath) -> bool {
        self.days.contains_key(dir)
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        self.sh.queue.close();
        for d in self.days.values() {
            d.cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// `req`'s day: its photo's `taken_at` (metadata read for local images; videos only when
/// already known, ffprobe is slow), else its modified time.
fn day_of(sh: &Shared, req: &Req) -> i64 {
    let image = media_type(&req.entry) == Some(MediaType::Image);
    let local = req.real.to_local_path().is_some();
    let taken = if image && local {
        sh.meta(req, false).and_then(|m| m.taken_at)
    } else {
        sh.store()
            .zip(sh.resolve(req, false))
            .and_then(|(s, (k, _))| s.meta(&k))
            .and_then(|m| m.taken_at)
    };
    day(taken.unwrap_or_else(|| unix(req.entry.modified)))
}

/// The 256 px thumbnail of a video tile (shown until its strip is there).
pub fn thumb_key(e: &Entry, tile_px: u32) -> TexKey {
    TexKey::of(e, SidecarKind::Thumb256, tile_px)
}

/// Unix seconds to unix days (UTC; EXIF times without an offset read as wall clock).
pub fn day(secs: i64) -> i64 {
    secs.div_euclid(86_400)
}

pub fn day_label(day: i64) -> String {
    chrono::DateTime::from_timestamp(day * 86_400, 0)
        .map(|d| d.format("%A, %B %-d, %Y").to_string())
        .unwrap_or_else(|| "Unknown date".into())
}

impl crate::state::AppState {
    /// Per frame: settings and library follow, uploads, and the slots of panes that no
    /// longer show the media view are emptied (their queued work is dropped).
    pub fn media_tick(&mut self) {
        let m = &mut self.media;
        m.set_remote(self.settings.remote_thumbnails);
        m.sync_library(self.library.lib.as_ref());
        self.settings.media_tile = m.tile;
        self.settings.media_dates = m.dates;
        let mut dated = Vec::new();
        for p in 0..2 {
            let pane = &self.panes[p];
            let shown = (p == 0 || self.dual)
                && pane.view == crate::pane::ViewMode::Media
                && pane.tab().kind == crate::tab::TabKind::Dir;
            if !shown && m.sig[p] != 0 {
                m.sig[p] = 0;
                m.want(p, Vec::new());
            }
            if shown && m.dates {
                dated.push(&pane.tab().dir);
            }
        }
        m.keep_days(&dated);
        m.upload(&self.ctx);
    }
}

// ---------------------------------------------------------------- layout math

/// One row of the media grid: a date header or tiles `[start, end)` of the visible list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Header(Option<i64>),
    Tiles(usize, usize),
}

/// Rows for `n` items in `cols` columns; with `day`, a header starts each new day
/// (consecutive items in the current order; None groups folders and non-media).
pub fn layout(n: usize, cols: usize, day: Option<&dyn Fn(usize) -> Option<i64>>) -> Vec<Row> {
    let cols = cols.max(1);
    let mut rows = Vec::new();
    let mut start = 0;
    while start < n {
        let end = match day {
            None => (start + cols).min(n),
            Some(day) => {
                let d = day(start);
                if start == 0 || day(start - 1) != d {
                    rows.push(Row::Header(d));
                }
                let mut end = start + 1;
                while end < n && end - start < cols && day(end) == d {
                    end += 1;
                }
                end
            }
        };
        rows.push(Row::Tiles(start, end));
        start = end;
    }
    rows
}

/// Row tops (and the total height as the last element).
pub fn row_tops(rows: &[Row], tile: f32, header: f32) -> Vec<f32> {
    let mut y = 0.0;
    let mut tops = Vec::with_capacity(rows.len() + 1);
    for r in rows {
        tops.push(y);
        y += match r {
            Row::Header(_) => header,
            Row::Tiles(..) => tile,
        };
    }
    tops.push(y);
    tops
}

/// The strip frame under a pointer at `frac` (0..1) of the tile's width.
pub fn strip_frame(frac: f32) -> u32 {
    ((frac * STRIP_FRAMES as f32).floor().max(0.0) as u32).min(STRIP_FRAMES - 1)
}

/// The texture coordinates of strip frame `i`.
pub fn strip_uv(i: u32) -> Rect {
    let w = 1.0 / STRIP_FRAMES as f32;
    Rect::from_min_max(pos2(i as f32 * w, 0.0), pos2((i + 1) as f32 * w, 1.0))
}

/// `uv` (a part of a texture `size` pixels big) cropped to its centred square.
pub fn cover_uv(size: Vec2, uv: Rect) -> Rect {
    let (w, h) = (uv.width() * size.x, uv.height() * size.y);
    if w <= 0.0 || h <= 0.0 {
        return uv;
    }
    if w > h {
        let cut = uv.width() * (1.0 - h / w) / 2.0;
        Rect::from_min_max(
            pos2(uv.min.x + cut, uv.min.y),
            pos2(uv.max.x - cut, uv.max.y),
        )
    } else {
        let cut = uv.height() * (1.0 - w / h) / 2.0;
        Rect::from_min_max(
            pos2(uv.min.x, uv.min.y + cut),
            pos2(uv.max.x, uv.max.y - cut),
        )
    }
}

/// Fits `size` into `area` without enlarging.
pub fn fit_scale(size: Vec2, area: Vec2) -> f32 {
    (area.x / size.x).min(area.y / size.y).min(1.0)
}

pub const FULL_UV: Rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));

#[cfg(test)]
#[path = "media_tests.rs"]
mod tests;
