//! Background work. Every worker sends a `Msg` and then calls `ctx.request_repaint()`;
//! nothing here runs on the UI thread.

use crate::preview_panel::PreviewKey;
use crate::state::Msg;
use crate::tab::Listing;
use crossbeam_channel::{Receiver, Sender};
use keel_preview::Preview;
use keel_search::{Query, Searcher};
use keel_vfs::{Entry, Router, VPath};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub fn send(tx: &Sender<Msg>, ctx: &egui::Context, msg: Msg) {
    let _ = tx.send(msg);
    ctx.request_repaint();
}

/// Starts a named thread; false (and logged) when the OS refuses one.
pub fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> bool {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .map_err(|e| tracing::error!("spawn {name}: {e}"))
        .is_ok()
}

pub fn spawn_list(router: Arc<Router>, dir: VPath, req: u64, tx: Sender<Msg>, ctx: egui::Context) {
    spawn("keel-list", move || {
        let result = match router.provider_for(&dir) {
            Some(p) => p.list(&dir).map(Listing::new),
            None => Err(anyhow::anyhow!("no provider for {}", dir.display())),
        };
        let gone = result.as_ref().is_err_and(|e| {
            let not_found = e.chain().any(|c| {
                c.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
            });
            // Inside an archive: the archive file itself is what is gone.
            let file = crate::state::outermost_archive(&dir).unwrap_or_else(|| dir.clone());
            not_found
                && file
                    .to_local_path()
                    .is_some_and(|d| keel_vfs::is_fixed_disk(&d))
        });
        send(
            &tx,
            &ctx,
            Msg::Listed {
                dir,
                req,
                gone,
                result,
            },
        );
    });
}

/// False when the thread could not start (the caller clears its in-flight flag).
pub fn spawn_drives(tx: Sender<Msg>, ctx: egui::Context) -> bool {
    spawn("keel-drives", move || {
        // Task 51: the WSL list is a registry read; it follows drives being added.
        send(&tx, &ctx, Msg::WslDistros(keel_term::wsl_distros()));
        let drives = keel_vfs::drives();
        let names: Vec<String> = drives.iter().map(|d| d.0.clone()).collect();
        send(&tx, &ctx, Msg::Drives(drives));
        // Phase 14: after the list, as a dead mapped drive can block here.
        send(&tx, &ctx, Msg::DriveKinds(crate::sidebar::kinds_of(&names)));
    })
}

/// Opens WSL distribution `name` with `open` (`wsl::open`, which starts a stopped one) and
/// answers `Msg::WslOpened`; an `open` still busy after `wsl::OPEN_TIMEOUT` is left behind
/// and answered as an error. False when the thread could not start.
pub fn spawn_wsl(
    name: String,
    open: fn(&str) -> anyhow::Result<VPath>,
    tx: Sender<Msg>,
    ctx: egui::Context,
) -> bool {
    spawn("keel-wsl", move || {
        let (answer, answered) = crossbeam_channel::bounded(1);
        let distro = name.clone();
        let result = if spawn("keel-wsl-open", move || {
            let _ = answer.send(open(&distro));
        }) {
            answered
                .recv_timeout(crate::wsl::OPEN_TIMEOUT)
                .unwrap_or_else(|e| match e {
                    crossbeam_channel::RecvTimeoutError::Timeout => Err(anyhow::anyhow!(
                        "no answer after {} seconds",
                        crate::wsl::OPEN_TIMEOUT.as_secs()
                    )),
                    crossbeam_channel::RecvTimeoutError::Disconnected => {
                        Err(anyhow::anyhow!("opening it failed"))
                    }
                })
        } else {
            Err(anyhow::anyhow!("no thread to open it on"))
        };
        send(&tx, &ctx, Msg::WslOpened { name, result });
    })
}

/// Materialises `path` (may be remote or cloud: progress toast when large) and runs `f` on
/// the local copy; errors become toasts.
pub fn spawn_local(
    router: Arc<Router>,
    path: VPath,
    tx: Sender<Msg>,
    ctx: egui::Context,
    f: impl FnOnce(&std::path::Path) -> std::io::Result<()> + Send + 'static,
) {
    spawn("keel-launch", move || {
        let result =
            crate::remotes::materialise(&router, &path, &tx, &ctx).and_then(|local| Ok(f(&local)?));
        if let Err(e) = result {
            send(&tx, &ctx, Msg::Toast(format!("{}: {e:#}", path.display())));
        }
    });
}

/// Materialises `entry` and renders it into a fit box of `max_px` (a thumbnail). Blocks:
/// workers only.
pub fn render(router: &Router, entry: Entry, page: u32, max_px: u32) -> Preview {
    let local = router
        .provider_for(&entry.path)
        .ok_or_else(|| anyhow::anyhow!("no provider for {}", entry.path.display()))
        .and_then(|p| p.local_copy(&entry.path));
    render_local(local, entry, page, max_px, false)
}

fn render_local(
    local: anyhow::Result<PathBuf>,
    entry: Entry,
    page: u32,
    max_px: u32,
    fit_width: bool,
) -> Preview {
    match local {
        Ok(bytes_path) => keel_preview::preview(&keel_preview::Request {
            entry,
            bytes_path,
            page,
            max_px,
            fit_width,
        }),
        Err(e) => Preview::Error(format!("{e:#}")),
    }
}

/// A preview panel request: what to render and the panel width in physical pixels.
pub type PreviewJob = (PreviewKey, Entry, u32);

/// A render that has not answered after this long is abandoned (Review Focus 4).
pub const RENDER_TIMEOUT: Duration = Duration::from_secs(15);
/// At most this many abandoned renders may still be running; past that, previews fail
/// fast until one of them returns.
pub const MAX_ABANDONED: usize = 3;

type RenderFn = dyn Fn(anyhow::Result<PathBuf>, Entry, u32, u32) -> Preview + Send + Sync;
type Work = (anyhow::Result<PathBuf>, Entry, u32, u32);

/// The preview panel's worker. Requests queued while it renders are skipped except the
/// newest, so fast cursor movement never queues a backlog of renders. Downloads (remote
/// files) run here; the render itself runs on a helper thread that is abandoned after
/// `RENDER_TIMEOUT` (shown as "timed out") and replaced by a fresh one.
pub fn spawn_previewer(
    router: Arc<Router>,
    tx: Sender<Msg>,
    ctx: egui::Context,
) -> Sender<PreviewJob> {
    let render: Arc<RenderFn> =
        Arc::new(|local, entry, page, px| render_local(local, entry, page, px, true));
    previewer_with(router, tx, ctx, render, RENDER_TIMEOUT)
}

fn previewer_with(
    router: Arc<Router>,
    tx: Sender<Msg>,
    ctx: egui::Context,
    render: Arc<RenderFn>,
    timeout: Duration,
) -> Sender<PreviewJob> {
    let (jobs, rx) = crossbeam_channel::unbounded::<PreviewJob>();
    spawn("keel-preview", move || {
        let abandoned = Arc::new(AtomicUsize::new(0));
        // The live render helper: work in, previews out.
        let mut helper: Option<(Sender<Work>, Receiver<Preview>)> = None;
        while let Ok(first) = rx.recv() {
            let (key, entry, max_px) = rx.try_iter().last().unwrap_or(first);
            let local = crate::remotes::materialise(&router, &entry.path, &tx, &ctx);
            if helper.is_none() {
                if abandoned.load(Ordering::Relaxed) >= MAX_ABANDONED {
                    let preview = Preview::Error("previews are stuck; try again shortly".into());
                    send(&tx, &ctx, Msg::Preview { key, preview });
                    continue;
                }
                helper = render_helper(render.clone());
            }
            let preview = match &helper {
                None => Preview::Error("could not start a preview thread".into()),
                Some((work, done)) => match work.send((local, entry, key.page, max_px)) {
                    Err(_) => Preview::Error("preview worker stopped".into()),
                    Ok(()) => match done.recv_timeout(timeout) {
                        Ok(preview) => preview,
                        Err(_) => {
                            // Stuck (or died): it finishes on its own, counted until then.
                            let (work, done) = helper.take().expect("helper is live");
                            drop(work);
                            let count = abandoned.clone();
                            count.fetch_add(1, Ordering::Relaxed);
                            spawn("keel-render-reaper", move || {
                                let _ = done.recv();
                                count.fetch_sub(1, Ordering::Relaxed);
                            });
                            Preview::Error("timed out".into())
                        }
                    },
                },
            };
            send(&tx, &ctx, Msg::Preview { key, preview });
        }
    });
    jobs
}

fn render_helper(render: Arc<RenderFn>) -> Option<(Sender<Work>, Receiver<Preview>)> {
    let (work_tx, work_rx) = crossbeam_channel::bounded::<Work>(1);
    let (done_tx, done_rx) = crossbeam_channel::bounded::<Preview>(1);
    spawn("keel-render", move || {
        while let Ok((local, entry, page, px)) = work_rx.recv() {
            if done_tx.send(render(local, entry, page, px)).is_err() {
                break;
            }
        }
    })
    .then_some((work_tx, done_rx))
}

/// Loads `searcher`, else the platform searcher (the Everything DLL load and IPC probe
/// may block), and probes it.
pub fn spawn_searcher(
    searcher: Option<Arc<dyn Searcher>>,
    index: keel_search::IndexSettings,
    tx: Sender<Msg>,
    ctx: egui::Context,
) {
    spawn("keel-searcher", move || {
        let searcher =
            searcher.unwrap_or_else(|| Arc::from(keel_search::default_searcher_with(&index)));
        let reason = crate::search_tab::probe(searcher.as_ref());
        send(&tx, &ctx, Msg::Searcher { searcher, reason });
    });
}

/// Re-checks whether search works (e.g. Everything was started after Keel).
pub fn spawn_probe(searcher: Arc<dyn Searcher>, tx: Sender<Msg>, ctx: egui::Context) {
    spawn("keel-probe", move || {
        let reason = crate::search_tab::probe(searcher.as_ref());
        send(&tx, &ctx, Msg::SearchProbe(reason));
    });
}

/// A query, the filter applied to it (`search_filters::apply`: its extensions are checked
/// on every backend's hits), the omissions to leave out and the request id.
type SearchJob = (
    Arc<dyn Searcher>,
    Query,
    crate::search_filters::Applied,
    crate::omissions::Filter,
    u64,
);

/// One search thread for the whole app. Queries wait their turn (Everything serves one
/// at a time); a query whose tab has moved on to a newer one is dropped unrun, so typing
/// never piles stale queries up behind a slow one. Hits the backend left without a size
/// and date (Keel's NTFS index above its stat limit, after Load more) get them here.
/// Most results is capped by settings (`search_tab::MAX_RESULTS`), so the stats stay
/// bounded.
pub struct SearchWorker {
    jobs: Sender<SearchJob>,
    /// Request numbers still wanted (each search tab's newest).
    live: Arc<Mutex<HashSet<u64>>>,
}

impl SearchWorker {
    pub fn new(tx: Sender<Msg>, ctx: egui::Context) -> Self {
        let (jobs, rx) = crossbeam_channel::unbounded::<SearchJob>();
        let live: Arc<Mutex<HashSet<u64>>> = Arc::default();
        let wanted = live.clone();
        spawn("keel-search", move || {
            while let Ok((searcher, query, applied, omissions, id)) = rx.recv() {
                if !wanted.lock().contains(&id) {
                    continue;
                }
                let mut result = run_search(&searcher, &query, &applied, &omissions, &wanted, id);
                if let Ok(hits) = &mut result {
                    if wanted.lock().contains(&id) {
                        fill_missing_meta(hits);
                    }
                }
                wanted.lock().remove(&id);
                send(&tx, &ctx, Msg::Search { id, result });
            }
        });
        Self { jobs, live }
    }

    /// Queues query `id`, superseding `replaces` (the tab's previous request).
    pub fn search(
        &self,
        searcher: Arc<dyn Searcher>,
        query: Query,
        applied: crate::search_filters::Applied,
        omissions: crate::omissions::Filter,
        id: u64,
        replaces: u64,
    ) {
        {
            let mut live = self.live.lock();
            live.remove(&replaces);
            live.insert(id);
        }
        let _ = self.jobs.send((searcher, query, applied, omissions, id));
    }
}

/// One search: the backend's first page through the tab's filter (the extensions and the
/// omissions), then, when that filter leaves fewer than `query.max`
/// hits and the backend still has more, bounded paging: the backend is asked for more
/// (doubling, until it runs out) until the filter has kept `query.max` hits or the tab
/// moved on; cancellation ends the scan. A filter the backend already applied (Everything's
/// extension prefix) keeps the page full, so no extra query happens.
fn run_search(
    searcher: &Arc<dyn Searcher>,
    query: &Query,
    applied: &crate::search_filters::Applied,
    omissions: &crate::omissions::Filter,
    wanted: &Mutex<HashSet<u64>>,
    id: u64,
) -> anyhow::Result<Vec<keel_search::Hit>> {
    let target = query.max as usize;
    let mut cap = query.max;
    loop {
        let page = searcher.query(&Query {
            max: cap,
            ..query.clone()
        })?;
        let full = page.len() >= cap as usize;
        let filtered = filter_page(page, applied, omissions);
        if filtered.len() >= target {
            let mut hits = filtered;
            hits.truncate(target);
            return Ok(hits);
        }
        if !full || !wanted.lock().contains(&id) {
            return Ok(filtered);
        }
        cap = cap.saturating_mul(2);
    }
}

/// The tab's filter over one backend page: the extensions, then the omissions.
fn filter_page(
    mut hits: Vec<keel_search::Hit>,
    applied: &crate::search_filters::Applied,
    omissions: &crate::omissions::Filter,
) -> Vec<keel_search::Hit> {
    hits.retain(|h| applied.keeps(h.path.name(), h.is_dir));
    omissions.apply(hits)
}

/// Fills in the size and date of local hits that came without them (a date is always
/// known for a file that was statted), in parallel.
fn fill_missing_meta(hits: &mut [keel_search::Hit]) {
    let missing: Vec<usize> = (hits.iter().enumerate())
        .filter(|(_, h)| h.modified.is_none() && h.path.to_local_path().is_some())
        .map(|(i, _)| i)
        .collect();
    if missing.is_empty() {
        return;
    }
    let mut part: Vec<keel_search::Hit> = missing.iter().map(|&i| hits[i].clone()).collect();
    keel_search::fill_meta(&mut part);
    for (i, hit) in missing.into_iter().zip(part) {
        hits[i] = hit;
    }
}

/// Four thumbnail threads behind a small bounded queue. When the queue is full the
/// request is refused and the grid asks again next frame, so fast scrolling never piles
/// up stale work; queued requests for tiles that scrolled away are dropped unrendered.
pub struct ThumbPool {
    jobs: Sender<(PreviewKey, Entry)>,
    /// Queued keys no longer wanted (their tile left the view): skipped by the threads.
    pub cancelled: Arc<Mutex<HashSet<PreviewKey>>>,
}

/// Thumbnail box in points; rendered at `THUMB_PX * pixels_per_point` pixels.
pub const THUMB_PX: u32 = 96;

impl ThumbPool {
    pub fn new(tx: Sender<Msg>, ctx: egui::Context, router: Arc<Router>) -> Self {
        let (jobs, rx) = crossbeam_channel::bounded::<(PreviewKey, Entry)>(16);
        let cancelled: Arc<Mutex<HashSet<PreviewKey>>> = Arc::default();
        for _ in 0..4 {
            let (rx, tx, ctx, router) = (rx.clone(), tx.clone(), ctx.clone(), router.clone());
            let cancelled = cancelled.clone();
            spawn("keel-thumb", move || {
                while let Ok((key, entry)) = rx.recv() {
                    if cancelled.lock().remove(&key) {
                        continue;
                    }
                    // Task 66: a big icon size asks for a bigger thumbnail (`key.width`).
                    let px = sized_thumb_px(key.width, ctx.pixels_per_point());
                    let preview = render(&router, entry, 0, px);
                    send(&tx, &ctx, Msg::Thumb { key, preview });
                }
            });
        }
        Self { jobs, cancelled }
    }

    /// False when the queue is full or the threads are gone; ask again later.
    pub fn request(&self, key: PreviewKey, entry: Entry) -> bool {
        self.cancelled.lock().remove(&key);
        self.jobs.try_send((key, entry)).is_ok()
    }
}

/// Thumbnail render size in physical pixels: sharp on HiDPI screens.
pub fn thumb_px(pixels_per_point: f32) -> u32 {
    ((THUMB_PX as f32 * pixels_per_point).round() as u32).max(THUMB_PX)
}

/// Task 66: a thumbnail box of `points` (0 = the usual `THUMB_PX`) in physical pixels.
pub fn sized_thumb_px(points: u32, pixels_per_point: f32) -> u32 {
    match points {
        0 => thumb_px(pixels_per_point),
        p => ((p as f32 * pixels_per_point).round() as u32).max(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_vfs::Kind;
    use std::sync::atomic::AtomicBool;

    fn q(text: &str) -> Query {
        Query {
            text: text.into(),
            ..Query::default()
        }
    }

    fn entry(name: &str) -> Entry {
        let dir = VPath::local(std::env::temp_dir());
        crate::tab::test_entry(&dir, name, Kind::File, 1)
    }

    #[test]
    fn thumbnails_render_at_physical_pixels() {
        assert_eq!(thumb_px(1.0), 96);
        assert_eq!(thumb_px(1.5), 144);
        assert_eq!(thumb_px(2.0), 192);
        assert_eq!(thumb_px(0.5), 96, "never below the box");
        assert_eq!(sized_thumb_px(0, 2.0), 192, "0: the usual box");
        assert_eq!(sized_thumb_px(512, 1.5), 768);
    }

    /// Review Focus 4 / polish backlog: a stuck render times out, a fresh helper serves
    /// the next request, and abandoned helpers are capped.
    #[test]
    fn stuck_render_times_out_and_a_fresh_helper_takes_over() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let release = Arc::new(AtomicBool::new(false));
        let gate = release.clone();
        let render: Arc<RenderFn> = Arc::new(move |_, e, _, _| {
            if e.name.starts_with("stuck") {
                while !gate.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            Preview::Unsupported
        });
        let jobs = previewer_with(
            Arc::new(Router::new()),
            tx,
            egui::Context::default(),
            render,
            Duration::from_millis(200),
        );
        let ask = |name: &str| {
            let e = entry(name);
            jobs.send((PreviewKey::of(&e, 0, 0), e, 64)).unwrap();
            match rx.recv_timeout(Duration::from_secs(5)).expect("answer") {
                Msg::Preview { key, preview } => {
                    assert_eq!(key.path.name(), name);
                    preview
                }
                _ => panic!("unexpected message"),
            }
        };
        let timed_out = |p: Preview| matches!(p, Preview::Error(e) if e == "timed out");
        assert!(timed_out(ask("stuck1.pdf")));
        assert!(matches!(ask("fine.txt"), Preview::Unsupported));
        for i in 2..=MAX_ABANDONED {
            assert!(timed_out(ask(&format!("stuck{i}.pdf"))));
        }
        // Every helper is stuck: fail fast instead of starting more threads.
        assert!(matches!(ask("fine.txt"), Preview::Error(e) if e.contains("stuck")));
        release.store(true, Ordering::Relaxed);
        let until = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match ask("fine.txt") {
                Preview::Unsupported => break,
                _ if std::time::Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                p => panic!("helpers never recovered: {p:?}"),
            }
        }
    }

    /// Task 76: omitted paths never reach the tab.
    #[test]
    fn search_worker_applies_omissions() {
        struct Fixed;
        impl Searcher for Fixed {
            fn query(&self, _: &Query) -> anyhow::Result<Vec<keel_search::Hit>> {
                Ok(["/x/keep.txt", "/x/omit.txt", "/x/later.txt"]
                    .into_iter()
                    .map(|p| keel_search::Hit {
                        path: keel_vfs::VPath::local(p),
                        is_dir: false,
                        size: 0,
                        modified: None,
                        snippet: None,
                    })
                    .collect())
            }
            fn available(&self) -> bool {
                true
            }
        }
        let (tx, rx) = crossbeam_channel::unbounded();
        let worker = SearchWorker::new(tx, egui::Context::default());
        let mut omissions = crate::omissions::Omissions::default();
        omissions.add(vec![keel_vfs::VPath::local("/x/omit.txt")], false);
        omissions.add(vec![keel_vfs::VPath::local("/x/later.txt")], true);
        for (id, show) in [(1, false), (2, true)] {
            omissions.show_omitted = show;
            let applied = crate::search_filters::Applied::default();
            worker.search(Arc::new(Fixed), q("x"), applied, omissions.filter(), id, 0);
            let names = match rx.recv_timeout(Duration::from_secs(5)).expect("answer") {
                Msg::Search { id: got, result } => {
                    assert_eq!(got, id);
                    let hits = result.unwrap();
                    hits.iter()
                        .map(|h| h.path.name().to_owned())
                        .collect::<Vec<_>>()
                }
                _ => panic!("unexpected message"),
            };
            let want: &[&str] = if show {
                &["keep.txt", "omit.txt", "later.txt"]
            } else {
                &["keep.txt"]
            };
            assert_eq!(names, want);
        }
    }

    /// Polish backlog: one search thread; a tab's superseded query is never run.
    #[test]
    fn search_worker_skips_superseded_queries() {
        struct Slow(Mutex<Vec<String>>);
        impl Searcher for Slow {
            fn query(&self, q: &Query) -> anyhow::Result<Vec<keel_search::Hit>> {
                self.0.lock().push(q.text.clone());
                std::thread::sleep(Duration::from_millis(150));
                Ok(Vec::new())
            }
            fn available(&self) -> bool {
                true
            }
        }
        let (tx, rx) = crossbeam_channel::unbounded();
        let worker = SearchWorker::new(tx, egui::Context::default());
        let slow = Arc::new(Slow(Mutex::default()));
        let search = |text: &str, id: u64, replaces: u64| {
            let (applied, omissions) = (Default::default(), Default::default());
            worker.search(slow.clone(), q(text), applied, omissions, id, replaces)
        };
        search("a", 1, 0);
        // Wait until "a" is really running (a slow machine may not have scheduled the
        // worker yet; a later query for the same tab would then supersede it).
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while slow.0.lock().is_empty() {
            assert!(
                std::time::Instant::now() < until,
                "the first query never started"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // While "a" runs, the tab types "ab" then "abc"; another tab searches "x".
        search("ab", 2, 1);
        search("x", 3, 0);
        search("abc", 4, 2);
        let mut answered = Vec::new();
        while answered.len() < 3 {
            match rx.recv_timeout(Duration::from_secs(5)).expect("answer") {
                Msg::Search { id, .. } => answered.push(id),
                _ => panic!("unexpected message"),
            }
        }
        assert_eq!(answered, [1, 3, 4]);
        assert_eq!(*slow.0.lock(), ["a", "x", "abc"]);
    }

    /// A backend that honours `query.max` as a stable prefix: the first `max` items of an
    /// ordered list. Items from `matching_from` are `.pdf`, the rest `.txt`; an optional
    /// per-query delay and a query counter make paging and cancellation observable.
    struct Paged {
        total: usize,
        matching_from: usize,
        delay: Duration,
        queries: Arc<AtomicUsize>,
    }
    impl Searcher for Paged {
        fn query(&self, q: &Query) -> anyhow::Result<Vec<keel_search::Hit>> {
            self.queries.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            let end = (q.max as usize).min(self.total);
            let mut hits = Vec::with_capacity(end);
            for i in 0..end {
                let ext = if i >= self.matching_from {
                    "pdf"
                } else {
                    "txt"
                };
                let name = format!("/x/item{i:06}.{ext}");
                hits.push(keel_search::Hit {
                    path: keel_vfs::VPath::local(&name),
                    is_dir: false,
                    size: 1,
                    modified: Some(std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
                    snippet: None,
                });
            }
            Ok(hits)
        }
        fn available(&self) -> bool {
            true
        }
    }

    /// K06: the whole first page misses the extension filter; the worker pages the backend until the
    /// requested number of matches (beyond that page) is returned.
    #[test]
    fn search_worker_pages_past_a_nonmatching_first_page() {
        let queries = Arc::new(AtomicUsize::new(0));
        let searcher = Arc::new(Paged {
            total: 1_000,
            matching_from: 500,
            delay: Duration::ZERO,
            queries: queries.clone(),
        });
        let (tx, rx) = crossbeam_channel::unbounded();
        let worker = SearchWorker::new(tx, egui::Context::default());
        let applied = crate::search_filters::Applied {
            keep: Some(vec!["pdf".into()]),
            ..Default::default()
        };
        worker.search(searcher, q("x"), applied, Default::default(), 1, 0);
        let hits = match rx.recv_timeout(Duration::from_secs(5)).expect("answer") {
            Msg::Search { id, result } => {
                assert_eq!(id, 1);
                result.unwrap()
            }
            _ => panic!("unexpected message"),
        };
        let names: Vec<_> = hits.iter().map(|h| h.path.name().to_owned()).collect();
        let page = Query::default().max as usize;
        assert_eq!(names.len(), page, "the requested number is still returned");
        assert!(names.iter().all(|n| n.ends_with(".pdf")));
        assert!(
            queries.load(Ordering::SeqCst) >= 2,
            "paged past the first page"
        );
    }

    /// K06: a filter that matches nothing would page the backend to its end; a superseded
    /// query ends the scan early instead.
    #[test]
    fn search_worker_cancellation_ends_the_paging_scan() {
        let queries = Arc::new(AtomicUsize::new(0));
        let paged = Arc::new(Paged {
            total: 4_000,
            matching_from: 4_000,
            delay: Duration::from_millis(100),
            queries: queries.clone(),
        });
        let (tx, rx) = crossbeam_channel::unbounded();
        let worker = SearchWorker::new(tx, egui::Context::default());
        let applied = crate::search_filters::Applied {
            keep: Some(vec!["pdf".into()]),
            ..Default::default()
        };
        worker.search(paged, q("x"), applied.clone(), Default::default(), 1, 0);
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while queries.load(Ordering::SeqCst) < 1 {
            assert!(
                std::time::Instant::now() < until,
                "the first page never started"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // The tab moves on while the scan is running.
        let other = Arc::new(Paged {
            total: 0,
            matching_from: 0,
            delay: Duration::ZERO,
            queries: Arc::new(AtomicUsize::new(0)),
        });
        worker.search(other, q("y"), applied, Default::default(), 2, 1);
        let mut answered = 0;
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while answered < 2 && std::time::Instant::now() < until {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Msg::Search { .. }) => answered += 1,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert_eq!(answered, 2, "both queries answered");
        // Left live the scan pages 500, 1000, 2000, 4000, 8000; the superseded query must
        // stop within a couple of pages.
        assert!(
            queries.load(Ordering::SeqCst) <= 3,
            "cancellation ended the scan"
        );
    }
}
