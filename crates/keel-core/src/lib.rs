//! Keel's library layer (spec 2.10): libraries of sources, each with a portable SQLite store,
//! a streaming indexer with stable record identity, durable jobs and
//! `validate -> preview -> execute` for mutating operations. No UI types: every call is a
//! typed request/response so a daemon can wrap it later.
//!
//! Blocking: every `Library` call reads or writes SQLite stores, and some also touch the
//! filesystem or the network (`validate_preview_execute`, `Plan::execute` with `recheck`,
//! `Indexer::full_walk`, `Indexer::apply_change`, `Library::close`, `Library::watch`). Call
//! them off the UI thread; long work runs as jobs (`Library::index`, `Library::hash`,
//! `Plan::execute`) whose progress arrives through `Jobs::subscribe`. Only
//! `Library::note_activity`, `Library::sources`, `Library::refresh_status` and
//! `Jobs::subscribe` return at once. Protection calls (`volumes`, `redundancy`,
//! `protection_summary`, …) may also look at a source's root.

mod db;
mod fsid;
mod hash;
mod index;
mod integrity;
mod jobs;
mod library;
mod media;
mod oplog;
mod plan;
mod protect;
mod search;
mod sidecar_job;
mod sidecars;
mod tags;

pub use fsid::unix_ns;
pub use hash::{
    on_battery, DupGroup, HashJob, HashResult, SkipReason, SkippedSource, SAMPLE, WHOLE,
};
pub use index::{
    ChangeEvent, IndexProgress, Indexer, WatchConfig, WatchHandle, BATCH, POLL_INTERVAL,
    RECONCILE_INTERVAL,
};
pub use integrity::{IntegrityJob, IntegrityResult, DEFAULT_SAMPLE_PCT, INTEGRITY_EVERY};
pub use jobs::{Job, JobCtx, JobEvent, JobId, JobInfo, JobStatus, Jobs, Restore};
pub use library::{
    Library, LibraryId, LibraryStats, LibrarySummary, OfflineReason, RecordRef, Source, SourceDef,
    SourceId, SourceKind, SourceStatus, SourceSummary, StoreUsage,
};
pub use media::{ffmpeg_available, read_meta, MediaMeta, MAX_PIXELS};
pub use oplog::{OpDone, OpLogEntry};
pub use plan::{
    validate_preview_execute, Action, Change, OnConflict, Op, Plan, PlanChanged, Warning,
};
pub use protect::{CopyAt, ProtectionSummary, Redundancy, Volume, VolumeKind, VolumeState};
pub use search::{KindFilter, LibraryHit, LibraryQuery, LibrarySearcher, DEFAULT_MAX};
pub use sidecar_job::SidecarJob;
pub use sidecars::{
    Pinned, SidecarKey, SidecarKind, SidecarStats, Sidecars,
    DEFAULT_BUDGET as DEFAULT_SIDECAR_BUDGET,
};
pub use tags::{Tag, TagId, View, FAVORITES};

use std::path::PathBuf;

/// The error a cancelled walk or job ends with (`err.is::<Cancelled>()`).
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// `KEEL_DATA_DIR`, else `%LOCALAPPDATA%\Keel`, `~/Library/Application Support/Keel`,
/// `~/.local/share/keel`. Libraries live under `<data dir>/library/<name>/`.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("KEEL_DATA_DIR").filter(|d| !d.is_empty()) {
        return Some(dir.into());
    }
    let base = directories::BaseDirs::new()?;
    let app = if cfg!(any(windows, target_os = "macos")) {
        "Keel"
    } else {
        "keel"
    };
    Some(base.data_local_dir().join(app))
}

/// Unix seconds.
pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// 128 random bits as hex.
pub(crate) fn random_id() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("random id: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
