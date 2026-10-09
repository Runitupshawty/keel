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
        let drives = keel_vfs::drives();
        send(&tx, &ctx, Msg::Drives(drives));
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

/// Loads the platform searcher (the Everything DLL load and IPC probe may block).
pub fn spawn_searcher(tx: Sender<Msg>, ctx: egui::Context) {
    spawn("keel-searcher", move || {
        let searcher: Arc<dyn Searcher> = Arc::from(keel_search::default_searcher());
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

type SearchJob = (Arc<dyn Searcher>, String, u64);

/// One search thread for the whole app. Queries wait their turn (Everything serves one
/// at a time); a query whose tab has moved on to a newer one is dropped unrun, so typing
/// never piles stale queries up behind a slow one.
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
            while let Ok((searcher, text, id)) = rx.recv() {
                if !wanted.lock().contains(&id) {
                    continue;
                }
                let result = searcher.query(&Query {
                    text,
                    max: crate::search_tab::MAX_HITS,
                    ..Query::default()
                });
                wanted.lock().remove(&id);
                send(&tx, &ctx, Msg::Search { id, result });
            }
        });
        Self { jobs, live }
    }

    /// Queues query `id`, superseding `replaces` (the tab's previous request).
    pub fn search(&self, searcher: Arc<dyn Searcher>, text: String, id: u64, replaces: u64) {
        {
            let mut live = self.live.lock();
            live.remove(&replaces);
            live.insert(id);
        }
        let _ = self.jobs.send((searcher, text, id));
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
                    let preview = render(&router, entry, 0, thumb_px(ctx.pixels_per_point()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use keel_vfs::Kind;
    use std::sync::atomic::AtomicBool;

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
        worker.search(slow.clone(), "a".into(), 1, 0);
        std::thread::sleep(Duration::from_millis(50));
        // While "a" runs, the tab types "ab" then "abc"; another tab searches "x".
        worker.search(slow.clone(), "ab".into(), 2, 1);
        worker.search(slow.clone(), "x".into(), 3, 0);
        worker.search(slow.clone(), "abc".into(), 4, 2);
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
}
