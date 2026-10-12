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

/// Merge, flatten and organize (folder operations through plans).
mod arrange;
/// Library backup and restore (`keel library backup` / `restore`).
pub mod backup;
/// Filter chips for library search (shared with keel-web by path).
pub mod chips;
mod db;
/// Set folder dates to the newest change below (a plan like the folder operations).
mod folder_dates;
mod fsid;
/// Fuzz entry points (`fuzz/` and the in-tree runs; docs/fuzzing.md).
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzz;
mod hash;
mod index;
mod inspect;
mod integrity;
mod jobs;
mod library;
mod media;
mod oplog;
#[cfg(test)]
mod perf_tests;
mod plan;
mod protect;
mod search;
mod sidecar_job;
mod sidecars;
mod styles;
mod sync;
mod tags;
mod text_job;
mod totals;

pub use arrange::{attributes_text, FolderOp, OrganizeBy};
pub use backup::{
    BackupError, BackupFile, BackupSource, Manifest as BackupManifest, Restored, RestoredSource,
};
pub use folder_dates::{ns_time, FolderDate};
pub use fsid::unix_ns;
pub use hash::{
    background_thread, is_background_thread, on_battery, DupGroup, HashJob, HashResult,
    RemoteHashSettings, SkipReason, SkippedSource, SAMPLE, WHOLE,
};
pub use index::{
    ChangeEvent, IndexProgress, Indexer, WatchConfig, WatchHandle, BATCH, POLL_INTERVAL,
    RECONCILE_INTERVAL, WALK_INTERVAL,
};
pub use inspect::{FolderTotals, History, Inspection};
pub use integrity::{IntegrityJob, IntegrityResult, DEFAULT_SAMPLE_PCT, INTEGRITY_EVERY};
pub use jobs::{Job, JobCtx, JobEvent, JobId, JobInfo, JobStatus, Jobs, Restore};
pub use library::{
    Library, LibraryId, LibraryStats, LibrarySummary, OfflineReason, RecordRef, Source, SourceDef,
    SourceId, SourceKind, SourceStats, SourceStatus, SourceSummary, StoreUsage,
};
pub use media::{
    decode_image, ffmpeg_available, ffprobe_json, find_tool, is_converted_photo, parse_probe,
    read_meta, MediaMeta, NeedsFfmpeg, TimedOutAt, MAX_PIXELS, RAW_EXTENSIONS, STILL_MIN_FFMPEG,
};
pub use oplog::{OpDone, OpLogEntry};
pub use plan::{
    validate_preview_execute, Action, Change, OnConflict, Op, Plan, PlanChanged, Warning,
};
pub use protect::{CopyAt, ProtectionSummary, Redundancy, Volume, VolumeKind, VolumeState};
pub use search::{
    KindFilter, LibraryHit, LibraryQuery, LibrarySearcher, Snippet, DEFAULT_MAX, FILE_NAMES_MAX,
    SNIPPET_CHARS, SNIPPET_PAGE,
};
pub use sidecar_job::SidecarJob;
pub use sidecars::{
    strip_retry_due, Pinned, SidecarKey, SidecarKind, SidecarStats, Sidecars, StripWaits,
    DEFAULT_BUDGET as DEFAULT_SIDECAR_BUDGET, STRIP_RETRY,
};
pub use styles::{valid_color, valid_emoji, valid_icon, valid_theme_icon, FolderStyle, THEME_ICON};
pub use sync::{
    plain_text, valid_device, valid_tag_name, SyncApplied, SyncEntry, SyncOp, SyncPage, SyncPeer,
    SyncTarget, FAVORITES_UID, MAX_LAMPORT, SYNC_PAGE, TOMBSTONE_SECS,
};
pub use tags::{Tag, TagId, View, FAVORITES};
pub use text_job::{
    ExtractFn, TextCaps, TextCounts, TextExtractor, TextJob, TextOutcome, TEXT_CAPS,
};
pub use totals::{ChildSize, FolderSizes, Total};

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
