//! Disk usage scans over the API (Phase 17 Task 86): `du.scan` starts a scan on the host in
//! the background and answers with its id at once; `du.result` reports its progress and,
//! when done, its roots with their biggest items; `du.export` returns the tree, the file
//! types, the largest files (CSV or JSON) or the HTML report as text.
//!
//! The host keeps the last [`KEEP`] scans in memory (a new one drops the oldest finished
//! one). The read rules of the other operations apply to the roots (no UNC or device paths
//! outside library sources on Windows), and Keel's configuration folder is always left out
//! of a scan.

use crate::error::{ApiError, Result};
use crate::types::*;
use crate::Ctx;
use keel_vfs::du;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Scans kept at once (running or finished).
pub const KEEP: usize = 4;
/// The biggest export `du.export` answers with.
pub const MAX_EXPORT: usize = 64 << 20;
/// The longest `du.result` waits.
const MAX_WAIT: Duration = Duration::from_secs(10);

enum Slot {
    Running,
    Done(Arc<du::Tree>),
    Failed(String),
}

struct Scan {
    id: String,
    handle: du::ScanHandle,
    slot: Arc<Mutex<Slot>>,
}

/// The host's disk usage scans ([`Ctx::du`]).
#[derive(Default)]
pub struct Scans {
    scans: Mutex<Vec<Scan>>,
}

impl Drop for Scans {
    fn drop(&mut self) {
        for s in self.scans.get_mut().iter() {
            s.handle.cancel();
        }
    }
}

fn new_id() -> Result<String> {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).map_err(|e| ApiError::failed(format!("no random id: {e}")))?;
    Ok(b.iter().map(|b| format!("{b:02x}")).collect())
}

/// A root's local path, checked like the read operations' paths.
fn root(ctx: &Ctx, s: &str) -> Result<std::path::PathBuf> {
    let p = crate::files::real(ctx, &crate::ops::vpath(s)?)?;
    crate::files::readable(ctx, &p)?;
    match p.to_local_path() {
        Some(local) if p.split_archive().is_none() => Ok(local),
        _ => Err(ApiError::invalid_params(format!(
            "{s}: a folder or drive on the host"
        ))),
    }
}

pub(crate) fn scan(ctx: &Ctx, p: DuScanParams) -> Result<DuScanStarted> {
    let mut roots: Vec<du::Root> = (p.paths.iter())
        .map(|s| root(ctx, s).map(du::Root::Path))
        .collect::<Result<_>>()?;
    if p.all_drives {
        roots.push(du::Root::AllLocalDrives);
    }
    if roots.is_empty() {
        return Err(ApiError::invalid_params("paths or all_drives"));
    }
    let mut exclude_paths = Vec::new();
    for s in &p.exclude_paths {
        let path = std::path::PathBuf::from(s.trim());
        if !path.is_absolute() {
            return Err(ApiError::invalid_params(format!(
                "not an absolute path: {s}"
            )));
        }
        exclude_paths.push(path);
    }
    exclude_paths.extend(ctx.config_dir.clone());
    let options = du::ScanOptions {
        count_hard_links_once: !p.count_hard_links_each,
        follow_symlinks: p.follow_symlinks,
        follow_junctions: p.follow_junctions,
        follow_mount_points: p.follow_mount_points,
        filters: du::Filters {
            exclude_paths,
            exclude_names: p.exclude_names,
            exclude_path_regex: p.exclude_regex,
            min_size: p.min_size,
            exclude_older_than: p.older_than_secs,
            exclude_newer_than: p.newer_than_secs,
            exclude_hidden: p.exclude_hidden,
            exclude_system: p.exclude_system,
        },
        ..du::ScanOptions::default()
    };
    options
        .filters
        .check()
        .map_err(|e| ApiError::invalid_params(format!("{e:#}")))?;
    let mut scans = ctx.du.scans.lock();
    if scans.len() >= KEEP {
        let finished = (scans.iter())
            .position(|s| !matches!(*s.slot.lock(), Slot::Running))
            .ok_or_else(|| {
                ApiError::failed(format!(
                    "{KEEP} disk usage scans are running; try again when one has finished"
                ))
            })?;
        scans.remove(finished);
    }
    let id = new_id()?;
    let handle = du::ScanHandle::new();
    let slot = Arc::new(Mutex::new(Slot::Running));
    let (h, s) = (handle.clone(), slot.clone());
    std::thread::Builder::new()
        .name("du-scan".into())
        .spawn(move || {
            let done = match du::scan(&roots, &options, &h) {
                Ok(tree) => Slot::Done(Arc::new(tree)),
                Err(e) => Slot::Failed(format!("{e:#}")),
            };
            *s.lock() = done;
        })
        .map_err(|e| ApiError::failed(format!("cannot start the scan: {e}")))?;
    scans.push(Scan {
        id: id.clone(),
        handle,
        slot,
    });
    Ok(DuScanStarted { id })
}

/// The scan `id`'s handle and slot.
fn find(ctx: &Ctx, id: &str) -> Result<(du::ScanHandle, Arc<Mutex<Slot>>)> {
    let scans = ctx.du.scans.lock();
    let s = (scans.iter().find(|s| s.id == id))
        .ok_or_else(|| ApiError::not_found(format!("no disk usage scan {id}")))?;
    Ok((s.handle.clone(), s.slot.clone()))
}

pub(crate) fn result(ctx: &Ctx, p: DuResultParams) -> Result<DuResult> {
    let (handle, slot) = find(ctx, &p.id)?;
    let deadline = Instant::now() + Duration::from_millis(p.wait_ms.unwrap_or(0)).min(MAX_WAIT);
    while matches!(*slot.lock(), Slot::Running) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let pr = handle.progress();
    let progress = DuProgress {
        files: pr.files,
        folders: pr.folders,
        bytes: pr.bytes,
        elapsed_ms: pr.elapsed.as_millis() as u64,
        current: pr.current.display().to_string(),
    };
    let (state, summary, error) = match &*slot.lock() {
        Slot::Running => (DuState::Running, None, None),
        Slot::Failed(e) => (DuState::Failed, None, Some(e.clone())),
        Slot::Done(tree) => {
            let s = tree.summary_by(p.top.unwrap_or(20), p.size_on_disk);
            let v = serde_json::to_value(&s).map_err(|e| ApiError::failed(e.to_string()))?;
            let s: DuSummary = serde_json::from_value(v)
                .map_err(|e| ApiError::new(ApiError::INTERNAL, e.to_string()))?;
            (DuState::Done, Some(s), None)
        }
    };
    Ok(DuResult {
        id: p.id,
        state,
        progress,
        summary,
        error,
    })
}

pub(crate) fn export(ctx: &Ctx, p: DuExportParams) -> Result<DuExport> {
    use du::export::{self as ex, Format, List, Options};
    let (_, slot) = find(ctx, &p.id)?;
    let tree = match &*slot.lock() {
        Slot::Done(tree) => tree.clone(),
        Slot::Running => {
            return Err(ApiError::failed(format!(
                "disk usage scan {} is still running",
                p.id
            )))
        }
        Slot::Failed(e) => return Err(ApiError::failed(format!("the scan failed: {e}"))),
    };
    let format = p.format.unwrap_or(DuFormat::Json);
    let mut options = Options {
        bom: p.bom,
        top: p.top.unwrap_or(100),
        size_on_disk: p.size_on_disk,
    };
    let mut out = Limited(Vec::new());
    let written = match p.list {
        DuList::Report => {
            options.top = p.top.unwrap_or(20);
            du::report::write(&tree, options, &mut out)
        }
        list => {
            let list = match list {
                DuList::Tree => List::Tree,
                DuList::Types => List::Types,
                _ => List::Largest,
            };
            let format = match format {
                DuFormat::Csv => Format::Csv,
                DuFormat::Json => Format::Json,
            };
            ex::write(&tree, list, format, options, &mut out)
        }
    };
    written.map_err(|e| ApiError::failed(format!("{e:#}")))?;
    let content = String::from_utf8(out.0)
        .map_err(|e| ApiError::new(ApiError::INTERNAL, e.to_string()))?;
    let format = match (p.list, format) {
        (DuList::Report, _) => "html",
        (_, DuFormat::Csv) => "csv",
        (_, DuFormat::Json) => "json",
    };
    Ok(DuExport {
        format: format.into(),
        content,
    })
}

/// A buffer that refuses to grow past [`MAX_EXPORT`].
struct Limited(Vec<u8>);

impl std::io::Write for Limited {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.0.len() + buf.len() > MAX_EXPORT {
            return Err(std::io::Error::other(format!(
                "the export is over {} MB: use `keel du --save-to` on the host, or a smaller top",
                MAX_EXPORT >> 20
            )));
        }
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
