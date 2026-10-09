//! Background work. Every worker sends a `Msg` and then calls `ctx.request_repaint()`;
//! nothing here runs on the UI thread.

use crate::preview_panel::PreviewKey;
use crate::remotes::SftpMap;
use crate::state::Msg;
use crate::tab::Listing;
use crossbeam_channel::{Sender, TrySendError};
use keel_preview::Preview;
use keel_search::{Query, Searcher};
use keel_vfs::{Entry, Router, VPath};
use std::sync::Arc;

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
            not_found
                && dir
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

pub fn spawn_drives(tx: Sender<Msg>, ctx: egui::Context) {
    spawn("keel-drives", move || {
        let drives = keel_vfs::drives();
        send(&tx, &ctx, Msg::Drives(drives));
    });
}

/// Materialises `path` (may be remote: progress toast when large) and runs `f` on the
/// local copy; errors become toasts.
pub fn spawn_local(
    router: Arc<Router>,
    sftp: SftpMap,
    path: VPath,
    tx: Sender<Msg>,
    ctx: egui::Context,
    f: impl FnOnce(&std::path::Path) -> std::io::Result<()> + Send + 'static,
) {
    spawn("keel-launch", move || {
        let result = crate::remotes::materialise(&router, &sftp, &path, &tx, &ctx)
            .and_then(|local| Ok(f(&local)?));
        if let Err(e) = result {
            send(&tx, &ctx, Msg::Toast(format!("{}: {e:#}", path.display())));
        }
    });
}

/// Materialises `entry` and renders it into a fit box of `max_px`. Blocks: workers only.
pub fn render(router: &Router, entry: Entry, page: u32, max_px: u32) -> Preview {
    let local = router
        .provider_for(&entry.path)
        .ok_or_else(|| anyhow::anyhow!("no provider for {}", entry.path.display()))
        .and_then(|p| p.local_copy(&entry.path));
    render_local(local, entry, page, max_px)
}

fn render_local(
    local: anyhow::Result<std::path::PathBuf>,
    entry: Entry,
    page: u32,
    max_px: u32,
) -> Preview {
    match local {
        Ok(bytes_path) => keel_preview::preview(&keel_preview::Request {
            entry,
            bytes_path,
            page,
            max_px,
        }),
        Err(e) => Preview::Error(format!("{e:#}")),
    }
}

/// A preview panel request: what to render and the panel width in physical pixels.
pub type PreviewJob = (PreviewKey, Entry, u32);

/// The preview panel's single worker. Requests queued while it renders are skipped
/// except the newest, so fast cursor movement never queues a backlog of renders.
pub fn spawn_previewer(
    router: Arc<Router>,
    sftp: SftpMap,
    tx: Sender<Msg>,
    ctx: egui::Context,
) -> Sender<PreviewJob> {
    let (jobs, rx) = crossbeam_channel::unbounded::<PreviewJob>();
    spawn("keel-preview", move || {
        while let Ok(first) = rx.recv() {
            let (key, entry, max_px) = rx.try_iter().last().unwrap_or(first);
            let local = crate::remotes::materialise(&router, &sftp, &entry.path, &tx, &ctx);
            let preview = render_local(local, entry, key.page, max_px);
            send(&tx, &ctx, Msg::Preview { key, preview });
        }
    });
    jobs
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

pub fn spawn_search(
    searcher: Arc<dyn Searcher>,
    text: String,
    id: u64,
    tx: Sender<Msg>,
    ctx: egui::Context,
) {
    spawn("keel-search", move || {
        let result = searcher.query(&Query {
            text,
            max: crate::search_tab::MAX_HITS,
            ..Query::default()
        });
        send(&tx, &ctx, Msg::Search { id, result });
    });
}

/// Four thumbnail threads behind a small bounded queue. When the queue is full the
/// request is refused and the grid asks again next frame, so fast scrolling never
/// piles up stale work.
pub struct ThumbPool {
    jobs: Sender<(PreviewKey, Entry)>,
}

pub const THUMB_PX: u32 = 96;

impl ThumbPool {
    pub fn new(tx: Sender<Msg>, ctx: egui::Context, router: Arc<Router>) -> Self {
        let (jobs, rx) = crossbeam_channel::bounded::<(PreviewKey, Entry)>(16);
        for _ in 0..4 {
            let (rx, tx, ctx, router) = (rx.clone(), tx.clone(), ctx.clone(), router.clone());
            spawn("keel-thumb", move || {
                while let Ok((key, entry)) = rx.recv() {
                    let preview = render(&router, entry, 0, THUMB_PX);
                    send(&tx, &ctx, Msg::Thumb { key, preview });
                }
            });
        }
        Self { jobs }
    }

    /// False when the queue is full; ask again later.
    pub fn request(&self, key: PreviewKey, entry: Entry) -> bool {
        !matches!(self.jobs.try_send((key, entry)), Err(TrySendError::Full(_)))
    }
}
