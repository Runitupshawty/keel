//! The registry and its handlers.

use crate::error::{ApiError, Result};
use crate::plans::Input;
use crate::types::*;
use crate::{check, parse, schema, to_value, Ctx, Operation, Preview, Run};
use keel_core::{LibraryHit, LibraryQuery, RecordRef, SourceId, SourceStatus, FAVORITES};
use keel_vfs::VPath;
use serde_json::json;
use std::sync::Arc;

/// A read (or a direct removal when `mutating`).
macro_rules! now {
    ($name:literal, $summary:literal, $P:ty => $R:ty, $f:path, $ex:expr) => {
        now!($name, $summary, false, $P => $R, $f, $ex)
    };
    ($name:literal, $summary:literal, $mutating:expr, $P:ty => $R:ty, $f:path, $ex:expr) => {
        Operation {
            name: $name,
            summary: $summary,
            mutating: $mutating,
            params: schema::<$P>,
            result: schema::<$R>,
            example: || $ex,
            check: check::<$P>,
            run: Run::Now(|ctx, v| to_value::<$R>($f(ctx, parse::<$P>(v)?)?)),
        }
    };
}

/// Preview-first: a direct call previews, `execute` applies.
macro_rules! previewed {
    ($name:literal, $summary:literal, $P:ty => $R:ty, $preview:path, $apply:path, $ex:expr) => {
        previewed!($name, $summary, false, $P => $R, $preview, $apply, $ex)
    };
    ($name:literal, $summary:literal, $secret:expr, $P:ty => $R:ty, $preview:path, $apply:path, $ex:expr) => {
        Operation {
            name: $name,
            summary: $summary,
            mutating: true,
            params: schema::<$P>,
            result: schema::<PlanPreview>,
            example: || $ex,
            check: check::<$P>,
            run: Run::Previewed {
                preview: |ctx, v| $preview(ctx, &parse::<$P>(v.clone())?),
                apply: |ctx, v| to_value::<$R>($apply(ctx, parse::<$P>(v)?)?),
                applied: schema::<$R>,
                secret: $secret,
            },
        }
    };
}

/// Every operation, in the order `tools/list` and `docs/api.md` show them.
pub static OPS: &[Operation] = &[
    now!("version", "Keel version, API revision and the open library.",
        NoParams => VersionInfo, version, json!({})),
    now!("sources.list", "The library's sources with their status.",
        NoParams => Vec<SourceInfo>, sources_list, json!({})),
    previewed!("sources.add", "Adds a folder, drive or share as a library source (index it with sources.index).",
        AddSourceParams => AddedSource, sources_add_preview, sources_add, json!({"root": example_dir(), "label": "Photos"})),
    previewed!("sources.remove", "Forgets a source (its files are never touched; delete_store also deletes its index store, with its tags and favorites).",
        RemoveSourceParams => RemovedSource, sources_remove_preview, sources_remove, json!({"id": "0123456789abcdef0123456789abcdef"})),
    previewed!("sources.index", "Indexes a source as a background job (adopt: first accept whatever folder is at its root now).",
        IndexParams => JobStarted, sources_index_preview, sources_index, json!({"id": "0123456789abcdef0123456789abcdef"})),
    now!("list", "Lists a folder: a library path (library://<source>/<rel>, from the index, works offline) or any path Keel can reach.",
        ListParams => Listing, list, json!({"path": example_dir(), "max": 100})),
    now!("stat", "One file or folder, with its library record, tags and favorite state when indexed.",
        PathParams => StatInfo, stat, json!({"path": example_dir()})),
    now!("read", "Reads a byte range of a file (at most 4 MiB per call, base64).",
        ReadParams => Chunk, crate::files::read, json!({"path": example_file(), "offset": 0, "len": 65536})),
    now!("preview.render", "Renders a file's preview as the desktop app shows it: text, or a PNG (images, a PDF page, a video frame) of bounded size.",
        RenderParams => Rendered, crate::files::render, json!({"path": example_file(), "page": 0, "max_px": 1024})),
    now!("media.thumb", "A photo or video thumbnail (256 or 1024 px WebP) from the sidecar store, made on first request.",
        ThumbParams => Thumb, crate::files::thumb, json!({"path": example_file(), "size": "thumb256"})),
    now!("file.get", "A one-time download link (/file/<token> on keel-daemon's --web address, valid 60 s) for a file.",
        PathParams => FileLink, crate::files::file_get, json!({"path": example_file()})),
    now!("search", "Searches the library index across all sources (words, \"phrases\", kind:, ext:, size:, dm:, source:, tag:); words also match the text inside documents (content:, -content:, name:, contents:off), and such hits carry a snippet of it.",
        SearchParams => Vec<Hit>, search, json!({"query": "invoice ext:pdf", "max": 20})),
    now!("files.names", "The full paths of indexed files in the online sources, each source's newest first, for a fuzzy file finder (at most `max`, default and largest 100000; `truncated` when there were more); read-only.",
        FileNamesParams => FileNames, files_names, json!({"max": 1000})),
    now!("system.search", "Searches every file on this computer with the platform's search (the window's search pane: Everything or Keel's own index on Windows, Spotlight or locate elsewhere), not only the library: words, \"phrases\", wildcards or a regex, with Everything's match options (case, whole word, path, diacritics, prefix, suffix, ignore punctuation or white space), sort (name, path, date_modified, size) and paging (count at most 1000; total over the first 10000 found); read-only.",
        SystemSearchParams => SystemSearchResult, system_search, json!({"query": "invoice *.pdf", "count": 20, "sort": "date_modified", "ascending": false})),
    now!("tags.list", "All tags, or the tags on one indexed path.",
        TagsListParams => Vec<TagInfo>, tags_list, json!({})),
    now!("tags.tagged", "Every tagged path (Favorites included) with its tag ids.",
        NoParams => Vec<TaggedPath>, tags_tagged, json!({})),
    now!("views.list", "Saved views (bookmarks): a name and a search query each, with the backend, sort, columns, match toggles and filter they open with and their folder, position and shortcut in the Bookmarks list.",
        NoParams => Vec<ViewInfo>, views_list, json!({})),
    previewed!("views.create", "Saves a search as a view (a bookmark); a library view's query must be a valid search query. Changes only the library.",
        ViewCreateParams => ViewInfo, views_create_preview, views_create, json!({"name": "Invoices", "query": "invoice ext:pdf", "sort": "modified:desc", "folder": "Work"})),
    previewed!("views.update", "Changes a saved view; omitted fields stay as they are. Changes only the library.",
        ViewUpdateParams => ViewInfo, views_update_preview, views_update, json!({"id": 1, "name": "Invoices 2026", "query": "invoice ext:pdf dm:2026"})),
    previewed!("views.delete", "Deletes a saved view (the files it finds are never touched).",
        ViewIdParams => ViewInfo, views_delete_preview, views_delete, json!({"id": 1})),
    previewed!("tags.add", "Tags indexed paths (creating the tag when missing; no paths: only creates it).",
        TagParams => Tagged, tags_add_preview, tags_add, json!({"tag": "receipts", "paths": [example_file()]})),
    previewed!("tags.remove", "Removes a tag from indexed paths.",
        TagParams => Tagged, tags_remove_preview, tags_remove, json!({"tag": "receipts", "paths": [example_file()]})),
    previewed!("tags.set", "Sets exactly these tags on indexed paths (Favorites is kept).",
        TagsSetParams => Tagged, tags_set_preview, tags_set, json!({"paths": [example_file()], "tags": ["receipts", "2026"]})),
    now!("favorites.list", "Favorite files and folders.",
        NoParams => Vec<Hit>, favorites_list, json!({})),
    previewed!("favorites.set", "Adds indexed paths to Favorites (on=false removes them).",
        FavoritesSetParams => Tagged, favorites_set_preview, favorites_set, json!({"paths": [example_file()], "on": true})),
    now!("recents", "Recently opened files, newest first.",
        LimitParams => Vec<Hit>, recents, json!({"limit": 20})),
    now!("recents.note", "Notes that an indexed file was opened (it moves to the top of recents); acts directly.",
        true, PathParams => Done, recents_note, json!({"path": example_file()})),
    now!("jobs.list", "Library jobs (indexing, hashing, file operations), newest first.",
        NoParams => Vec<JobInfo>, jobs_list, json!({})),
    now!("jobs.info", "One job with its log.",
        JobParams => JobInfo, jobs_info, json!({"id": 1})),
    previewed!("jobs.cancel", "Cancels a queued or running job.",
        JobParams => JobInfo, jobs_cancel_preview, jobs_cancel, json!({"id": 1})),
    now!("duplicates", "Groups of indexed files with the same content, most wasted bytes first.",
        DuplicatesParams => Vec<DupGroup>, duplicates, json!({"min_size": 1048576, "max": 50})),
    now!("redundancy", "How many copies of a file's content the library knows, and where.",
        PathParams => Copies, redundancy, json!({"path": example_file()})),
    now!("redundancy.folder", "The copies of every indexed file in a folder (the first 5000 files).",
        PathParams => Vec<FileCopies>, redundancy_folder, json!({"path": example_dir()})),
    now!("library.folder_style", "A folder's color and icon or a file's icon, or (no path) every folder and file in the library that has one; read-only.",
        FolderStyleParams => Vec<FolderStyleInfo>, folder_style, json!({"path": example_dir()})),
    previewed!("library.set_folder_style", "Sets an indexed folder's color (#rrggbb) and icon (one emoji, or theme:<name> for the icon theme's folder icon), or a file's icon (one emoji, or theme:<file name or .ext> for the icon theme's file icon); an omitted field stays, \"\" clears it. Changes only the library.",
        SetFolderStyleParams => FolderStyleInfo, set_folder_style_preview, set_folder_style, json!({"path": example_dir(), "color": "#3b82f6", "icon": "📷"})),
    now!("library.comment", "The Get Info comment on an indexed file or folder (absent: none); read-only.",
        PathParams => CommentInfo, library_comment, json!({"path": example_file()})),
    previewed!("library.set_comment", "Sets the Get Info comment on an indexed file or folder (\"\" clears it). Changes only the library.",
        SetCommentParams => CommentInfo, set_comment_preview, set_comment, json!({"path": example_file(), "comment": "Send to the printer"})),
    now!("library.stats", "Counts over every source (records, files, bytes, distinct contents, running jobs) and per source (files, folders, bytes, hashed files, last walk, offline), and the remote hashing policy.",
        NoParams => LibraryStats, library_stats, json!({})),
    now!("library.folder_sizes", "A folder's total size and file count, and each child's size (folders by their totals), biggest first (at most `max`, default 1000; `dirs_only` for the folders alone), from the library index; read-only.",
        FolderSizesParams => FolderSizes, library_folder_sizes, json!({"path": example_dir()})),
    now!("library.inspect", "Everything the library knows about one path, for an inspector: the entry from the file system, and when indexed its source, volume, content id, first seen, last hashed, protection count, the other files with its content, its history from the operation log (copied, moved, renamed, deleted, tagged; newest first), its tags and a folder's totals. Lists at most `max` (default 50); read-only.",
        InspectParams => Inspection, library_inspect, json!({"path": example_file(), "max": 50})),
    previewed!("library.backup", "Writes a backup of the library (its index with tags, favorites and content ids, and its sources) to a new .keelbackup file on the host; thumbnails and the search index are rebuilt after a restore.",
        BackupParams => BackupWritten, backup_preview, backup, json!({"file": example_backup()})),
    previewed!("library.restore", "Restores a .keelbackup file on the host as a new library (never replacing one: `name` picks another name); sources whose folder exists there are walked again when it opens, the others are offline.",
        RestoreParams => RestoredLibrary, restore_preview, restore, json!({"file": example_backup(), "name": "restored"})),
    now!("protection.summary", "How safe the library's contents are: single copies, one failure domain, not backed up, drift, files not checked yet.",
        NoParams => Protection, protection_summary, json!({})),
    now!("volumes.list", "The drive inventory: volumes holding sources, their failure domain, state and backup mark.",
        NoParams => Vec<VolumeInfo>, volumes_list, json!({})),
    previewed!("volumes.set", "Sets a volume's state (archived, lost, retired; online makes it automatic), backup mark or failure domain.",
        VolumeSetParams => VolumeInfo, volumes_set_preview, volumes_set, json!({"volume": "source:0123456789abcdef0123456789abcdef", "backup": true})),
    previewed!("integrity.check", "Re-hashes a sample of hashed files as a job and marks files whose bytes changed (due_days: only when the last check is that old).",
        IntegrityParams => MaybeJob, integrity_preview, integrity_check, json!({"sample_pct": 1.0})),
    previewed!("hashing.set", "Turns content hashing after walks on or off (off cancels a running hash job; on starts one); remote, cloud and max_remote_bytes choose which remote files it downloads (omitted: unchanged).",
        HashingParams => MaybeJob, hashing_preview, hashing_set, json!({"on": true, "idle_only": true, "remote": true, "cloud": false, "max_remote_bytes": 1073741824})),
    previewed!("contents.set", "Turns indexing the text inside documents on or off for the library (Index file contents) or one source (id; default: back to its kind's default, local folders and drives on, shares, SFTP, cloud and devices off). On reads the text at idle priority; off deletes the text read so far.",
        ContentsParams => MaybeJob, contents_preview, contents_set, json!({"on": true, "id": "0123456789abcdef0123456789abcdef"})),
    previewed!("media.index", "Makes thumbnails and reads metadata of a source's photos and videos as an idle-priority job.",
        SourceIdParams => JobStarted, media_index_preview, media_index, json!({"id": "0123456789abcdef0123456789abcdef"})),
    now!("activity.note", "Notes that the user is working: idle-only hashing and integrity jobs pause for the next 5 s, sidecar jobs for 1 s; acts directly.",
        true, NoParams => Done, activity_note, json!({})),
    now!("plan", "Previews a file operation (copy, move, delete, rename) from the index without touching anything. Returns a preview; call execute with the plan id and input hash to apply.",
        PlanParams => PlanPreview, plan, json!({"op": "copy", "paths": [example_file()], "to": example_dir(), "on_conflict": "skip"})),
    now!("plan.merge", "Previews merging folders: everything in `paths` (but `into`) moves into `into` with the conflict rule, then the folders left empty are removed. Links stay. Call execute to apply.",
        MergeParams => PlanPreview, plan_merge, json!({"paths": [example_dir(), format!("{}2", example_dir())], "into": example_dir(), "on_conflict": "rename_new"})),
    now!("plan.flatten", "Previews flattening a folder: every file in its subfolders, at any depth, moves up into it with the conflict rule, then the subfolders left empty are removed. Links stay. Call execute to apply.",
        FlattenParams => PlanPreview, plan_flatten, json!({"path": example_dir(), "on_conflict": "skip"})),
    now!("plan.organize", "Previews organizing a folder's files into subfolders by type (Images, Videos, Audio, Documents, Archives, Other), year or year-month of the modified date; missing subfolders are made. Call execute to apply.",
        OrganizeParams => PlanPreview, plan_organize, json!({"path": example_dir(), "by": "year"})),
    now!("plan.remove_empty", "Previews removing the empty folders below a folder: every folder holding no files and no links, at any depth, goes to the Recycle Bin or Trash (the outermost of each empty branch, whole). Call execute to apply.",
        RemoveEmptyParams => PlanPreview, plan_remove_empty, json!({"path": example_dir()})),
    now!("plan.set_attributes", "Previews setting attributes on files or folders on the host: hidden (Windows, macOS), read-only, or the Unix mode bits (octal). Not recursive. Call execute to apply.",
        AttributesParams => PlanPreview, plan_set_attributes, json!({"paths": [example_file()], "readonly": true})),
    now!("plan.link", "Previews making a link on the host: a symbolic link (on Windows only with Developer Mode or elevation), a junction to a folder (Windows) or a hard link to a file. Call execute to apply.",
        LinkParams => PlanPreview, plan_link, json!({"target": example_dir(), "link": format!("{}-link", example_dir()), "kind": if cfg!(windows) { "junction" } else { "symlink" }})),
    now!("plan.folder_dates", "Previews setting the modified time of each folder at and under `path` on the host to that of the newest file below it, at any depth (links are not followed; a folder with no file below keeps its time). Each change's detail is the new time and the old one. Call execute to apply.",
        FolderDatesParams => PlanPreview, plan_folder_dates, json!({"path": example_dir()})),
    now!("execute", "Applies a previewed plan: pass the plan_id and input_hash of the preview being confirmed. Refuses expired, tampered or changed plans.",
        true, ExecuteParams => Executed, execute, json!({"plan_id": "0123456789abcdef0123456789abcdef", "input_hash": "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"})),
    now!("library.sync", "Pulls tag, favorite, folder style and content-id changes now from one paired device, or every device library sync is on with (both sides must turn it on); applies them to the library at once.",
        true, LibrarySyncParams => Vec<SyncedDevice>, library_sync, json!({})),
    now!("devices.list", "This device and its paired devices with their link state, library sync switch and last sync.",
        NoParams => Devices, devices_list, json!({})),
    previewed!("devices.pair_code", "Creates a one-time pairing code (valid 10 minutes) for another device to enter.",
        NoParams => PairCodeInfo, pair_code_preview, pair_code, json!({})),
    previewed!("devices.pair_with", "Pairs with the device that showed this code. Pairing grants nothing by itself.",
        true, PairWithParams => PeerInfo, pair_with_preview, pair_with, json!({"code": "abcdefghijklmnopqrstuvwxyz"})),
    previewed!("devices.forget", "Forgets a paired device (its sessions close and its grants are removed).",
        PeerParams => Done, forget_preview, forget, json!({"peer": "a".repeat(52)})),
    now!("devices.settings", "This device's label, Spacedrop inbox, always-accept list, relay setting and the devices library sync is on with.",
        NoParams => DeviceSettingsInfo, devices_settings, json!({})),
    previewed!("devices.settings_set", "Changes this device's label, Spacedrop inbox, always-accept list or library sync devices at once; relays when the host starts again.",
        DeviceSettingsParams => DeviceSettingsSet, settings_set_preview, settings_set, json!({"label": "Laptop", "auto_accept": ["a".repeat(52)], "sync": ["a".repeat(52)]})),
    now!("shares.list", "Grants this device gives its paired devices.",
        NoParams => Vec<GrantInfo>, shares_list, json!({})),
    previewed!("shares.grant", "Grants a paired device read or read-write access to a source or a subtree of it.",
        GrantParams => GrantInfo, grant_preview, grant, json!({"peer": "a".repeat(52), "source": "0123456789abcdef0123456789abcdef", "subtree": "Photos/2026", "access": "read"})),
    now!("shares.revoke", "Revokes a grant at once (the device's next request is refused) and returns what it revoked.",
        true, RevokeParams => Revoked, revoke, json!({"peer": "a".repeat(52), "source": "0123456789abcdef0123456789abcdef", "subtree": "Photos/2026"})),
    now!("mounts.list", "Sources keel-daemon serves as drives or mount folders.",
        NoParams => Vec<MountInfo>, mounts_list, json!({})),
    previewed!("mounts.add", "Mounts a source, or a subtree of it, as a drive letter or folder: listings from the index while the source is offline, reads on demand, writes published when the file closes. keel-daemon only; unmounted when it stops.",
        MountParams => MountInfo, mounts_add_preview, mounts_add, json!({"source": "0123456789abcdef0123456789abcdef", "subtree": "Photos/2026", "target": example_mount()})),
    previewed!("mounts.remove", "Unmounts a mount; writes still in progress there are discarded (those files stay as they were).",
        UnmountParams => MountInfo, mounts_remove_preview, mounts_remove, json!({"target": example_mount()})),
    previewed!("spacedrop.send", "Sends files or folders on this machine to a paired device (Spacedrop) as a job; the preview lists every file and its size.",
        SpacedropSendParams => JobStarted, drop_send_preview, drop_send, json!({"peer": "a".repeat(52), "paths": [example_file()]})),
    now!("spacedrop.inbox", "The Spacedrop inbox on this machine: offers waiting for an answer and what arrived, newest first (download with file.get).",
        NoParams => Inbox, drop_inbox, json!({})),
    previewed!("spacedrop.answer", "Accepts or declines a Spacedrop offer waiting in the inbox.",
        SpacedropAnswerParams => Done, drop_answer_preview, drop_answer, json!({"peer": "a".repeat(52), "id": "0123456789abcdef0123456789abcdef", "accept": true})),
    now!("du.scan", "Starts a disk usage scan of folders or drives on the host (or all_drives: every local fixed drive) in the background and returns its id at once; size on disk, hard links counted once (count_hard_links_each: every name), links followed only on request, filters by name glob, path, path regex, minimum size, age, hidden and system. Keel's configuration folder is left out. Read-only.",
        DuScanParams => DuScanStarted, crate::du::scan, json!({"paths": [example_dir()], "exclude_names": ["*.tmp"], "min_size": 4096})),
    now!("du.result", "A disk usage scan's state (running, done, failed) and progress, and when done its totals and each root's biggest items (`top`, default 20; size_on_disk ranks by bytes on disk); wait_ms waits up to 10 s for it to finish. Read-only.",
        DuResultParams => DuResult, crate::du::result, json!({"id": "0123456789abcdef", "top": 10, "wait_ms": 1000})),
    now!("du.export", "A finished disk usage scan as text: every item (`tree`; JSON is a scan file `keel du --load-from` reads), the file types (`types`), the largest files (`largest`, `top` default 100) as CSV or JSON, or the HTML report (`report`). At most 64 MB. Read-only.",
        DuExportParams => DuExport, crate::du::export, json!({"id": "0123456789abcdef", "list": "types", "format": "csv"})),
];

fn example_dir() -> &'static str {
    if cfg!(windows) {
        r"C:\Users\me\Pictures"
    } else {
        "/home/me/Pictures"
    }
}

fn example_mount() -> &'static str {
    if cfg!(windows) {
        "K:"
    } else {
        "/home/me/Keel"
    }
}

fn example_backup() -> &'static str {
    if cfg!(windows) {
        r"D:\Backups\main-2026-10-11.keelbackup"
    } else {
        "/media/backup/main-2026-10-11.keelbackup"
    }
}

fn example_file() -> &'static str {
    if cfg!(windows) {
        r"C:\Users\me\Documents\invoice.pdf"
    } else {
        "/home/me/Documents/invoice.pdf"
    }
}

// --- helpers ---

/// An absolute local path (archive paths included) or a VPath URI.
pub(crate) fn vpath(s: &str) -> Result<VPath> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ApiError::invalid_params("empty path"));
    }
    if s.contains("://") {
        return VPath::parse(s).map_err(|e| ApiError::invalid_params(format!("{e:#}")));
    }
    let outer = s.split("!/").next().unwrap_or(s);
    if !std::path::Path::new(outer).is_absolute() {
        return Err(ApiError::invalid_params(format!(
            "not an absolute path: {s} (the host's working folder is not the caller's)"
        )));
    }
    Ok(VPath::local(s))
}

fn unix(t: Option<std::time::SystemTime>) -> Option<i64> {
    t.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

fn hit(h: &LibraryHit) -> Hit {
    Hit {
        path: h.path.display(),
        name: h.name.clone(),
        is_dir: h.is_dir,
        size: h.size,
        modified: h.modified,
        source: h.record.source.0.clone(),
        source_label: h.source_label.clone(),
        record: h.record.id,
        score: h.score,
        snippet: h.snippet.as_ref().map(|s| Snippet {
            text: s.text.clone(),
            marks: s.marks.iter().map(|&(a, b)| [a, b]).collect(),
        }),
    }
}

fn hits(v: Vec<LibraryHit>) -> Vec<Hit> {
    v.iter().map(hit).collect()
}

fn kind_name(k: keel_core::SourceKind) -> SourceKindName {
    use keel_core::SourceKind as K;
    match k {
        K::Folder => SourceKindName::Folder,
        K::Drive => SourceKindName::Drive,
        K::Share => SourceKindName::Share,
        K::Cloud => SourceKindName::Cloud,
        K::Device => SourceKindName::Device,
    }
}

fn source_info(s: &keel_core::SourceSummary) -> SourceInfo {
    let mut info = SourceInfo {
        id: s.id.0.clone(),
        label: s.label.clone(),
        root: s.root.display(),
        kind: kind_name(s.kind),
        status: String::new(),
        indexed_at: None,
        last_seen: None,
        detail: None,
        generation: s.generation,
    };
    match &s.status {
        SourceStatus::Online { indexed_at } => {
            info.status = "online".into();
            info.indexed_at = *indexed_at;
        }
        SourceStatus::Indexing { done, total } => {
            info.status = "indexing".into();
            info.detail = Some(format!("{done} of about {total} records"));
        }
        SourceStatus::Offline { last_seen, reason } => {
            info.status = "offline".into();
            info.last_seen = *last_seen;
            info.detail = Some(format!("{reason:?}").to_lowercase());
        }
        SourceStatus::Error(e) => {
            info.status = "error".into();
            info.detail = Some(e.clone());
        }
    }
    info
}

fn find_source(ctx: &Ctx, id: &str) -> Result<keel_core::SourceSummary> {
    ctx.lib
        .sources()
        .into_iter()
        .find(|s| s.id.0 == id)
        .ok_or_else(|| ApiError::not_found(format!("no source {id}")))
}

/// `(source, path relative to it)` of a library path or a path inside a source.
fn locate(ctx: &Ctx, p: &VPath) -> Result<(SourceId, String)> {
    if let Some((id, rel)) = keel_vfs::library::split(p) {
        return Ok((SourceId(id.to_owned()), rel.to_owned()));
    }
    let (src, rel) = ctx
        .lib
        .source_for(p)
        .ok_or_else(|| ApiError::not_found(format!("{} is in no library source", p.display())))?;
    Ok((src.id.clone(), rel))
}

/// The indexed record at `path`.
pub(crate) fn record_at(ctx: &Ctx, path: &str) -> Result<LibraryHit> {
    let p = vpath(path)?;
    let (source, rel) = locate(ctx, &p)?;
    if rel.is_empty() {
        return Err(ApiError::invalid_params(format!(
            "{path} is a source root, not a record"
        )));
    }
    let (parent, name) = rel.rsplit_once('/').unwrap_or(("", &rel));
    let children = ctx
        .lib
        .list_children(&source, parent)
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?;
    children
        .iter()
        .find(|h| h.name == name)
        .or_else(|| children.iter().find(|h| h.name.eq_ignore_ascii_case(name)))
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("{path} is not in the library index yet")))
}

fn records(ctx: &Ctx, paths: &[String]) -> Result<Vec<LibraryHit>> {
    if paths.is_empty() {
        return Err(ApiError::invalid_params("no paths"));
    }
    paths.iter().map(|p| record_at(ctx, p)).collect()
}

fn refs(hits: &[LibraryHit]) -> Vec<RecordRef> {
    hits.iter().map(|h| h.record.clone()).collect()
}

fn change(action: &str, path: Option<String>, detail: Option<String>) -> Change {
    Change {
        action: action.into(),
        path,
        to: None,
        detail,
        files: None,
        bytes: None,
    }
}

fn warning(kind: &str, path: Option<String>, message: String) -> Warning {
    Warning {
        kind: kind.into(),
        path,
        files: None,
        message,
    }
}

// --- library ---

fn version(ctx: &Ctx, _: NoParams) -> Result<VersionInfo> {
    Ok(VersionInfo {
        name: "keel".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        api: crate::API_VERSION,
        library: ctx.lib.name.clone(),
        net: ctx.node.is_some(),
        pid: std::process::id(),
    })
}

fn sources_list(ctx: &Ctx, _: NoParams) -> Result<Vec<SourceInfo>> {
    Ok(ctx.lib.sources().iter().map(source_info).collect())
}

fn source_def(p: &AddSourceParams) -> Result<keel_core::SourceDef> {
    let root = vpath(&p.root)?;
    if let Some(local) = root.to_local_path() {
        if !local.is_dir() {
            return Err(ApiError::invalid_params(format!(
                "{} is not a folder",
                root.display()
            )));
        }
    }
    let label = match &p.label {
        Some(l) if !l.trim().is_empty() => l.trim().to_owned(),
        _ => match root.name() {
            "" => root.display(),
            n => n.to_owned(),
        },
    };
    use keel_core::SourceKind as K;
    Ok(keel_core::SourceDef {
        label,
        root,
        kind: match p.kind.unwrap_or(SourceKindName::Folder) {
            SourceKindName::Folder => K::Folder,
            SourceKindName::Drive => K::Drive,
            SourceKindName::Share => K::Share,
            SourceKindName::Cloud => K::Cloud,
            SourceKindName::Device => K::Device,
        },
        include_hidden: p.include_hidden,
        ignore: p.ignore.clone(),
        poll_secs: None,
        hash_shares: false,
    })
}

fn sources_add_preview(ctx: &Ctx, p: &AddSourceParams) -> Result<Preview> {
    let def = source_def(p)?;
    if let Some((s, _)) = ctx.lib.source_for(&def.root) {
        return Err(ApiError::invalid_params(format!(
            "{} is inside source {}",
            def.root.display(),
            s.def.label
        )));
    }
    Ok(Preview {
        pin: None,
        summary: format!("Add source {} at {}", def.label, def.root.display()),
        changes: vec![change(
            "source.add",
            Some(def.root.display()),
            Some(def.label.clone()),
        )],
        warnings: Vec::new(),
    })
}

fn sources_add(ctx: &Ctx, p: AddSourceParams) -> Result<AddedSource> {
    let id = ctx.lib.add_source(source_def(&p)?)?;
    Ok(AddedSource { id: id.0 })
}

fn sources_remove_preview(ctx: &Ctx, p: &RemoveSourceParams) -> Result<Preview> {
    let s = find_source(ctx, &p.id)?;
    let usage = ctx.lib.store_usage(&s.id)?;
    let held = format!(
        "{} bytes, {} tag(s), {} favorite(s)",
        usage.bytes, usage.tags, usage.favorites
    );
    let mut warnings = Vec::new();
    let summary = if p.delete_store {
        warnings.push(warning(
            "deletes_store",
            Some(s.root.display()),
            format!("the index store of {} is deleted for good: {held}", s.label),
        ));
        format!(
            "Remove source {} ({}) and delete its index store ({held})",
            s.label,
            s.root.display()
        )
    } else {
        format!(
            "Remove source {} ({}); its index store is kept ({held})",
            s.label,
            s.root.display()
        )
    };
    Ok(Preview {
        pin: None,
        summary,
        changes: vec![Change {
            action: "source.remove".into(),
            path: Some(s.root.display()),
            to: None,
            detail: Some(s.label.clone()),
            files: None,
            bytes: p.delete_store.then_some(usage.bytes),
        }],
        warnings,
    })
}

fn sources_remove(ctx: &Ctx, p: RemoveSourceParams) -> Result<RemovedSource> {
    let removed = source_info(&find_source(ctx, &p.id)?);
    ctx.lib
        .remove_source(&SourceId(p.id.clone()), p.delete_store)?;
    Ok(RemovedSource {
        removed,
        store_deleted: p.delete_store,
    })
}

fn sources_index_preview(ctx: &Ctx, p: &IndexParams) -> Result<Preview> {
    let s = find_source(ctx, &p.id)?;
    let mut warnings = Vec::new();
    if p.adopt {
        warnings.push(warning(
            "adopts_root",
            Some(s.root.display()),
            format!(
                "whatever is at {} now becomes the content of {}",
                s.root.display(),
                s.label
            ),
        ));
    }
    Ok(Preview {
        pin: None,
        summary: format!("Index {} ({})", s.label, s.root.display()),
        changes: vec![change(
            "source.index",
            Some(s.root.display()),
            Some(s.label.clone()),
        )],
        warnings,
    })
}

fn sources_index(ctx: &Ctx, p: IndexParams) -> Result<JobStarted> {
    find_source(ctx, &p.id)?;
    let id = SourceId(p.id);
    if p.adopt {
        let src = ctx
            .lib
            .source(&id)
            .ok_or_else(|| ApiError::not_found(format!("no source {}", id.0)))?;
        keel_core::Indexer::adopt_root(&src)?;
    }
    Ok(JobStarted {
        job: ctx.lib.index(&id)?,
    })
}

fn list(ctx: &Ctx, p: ListParams) -> Result<Listing> {
    let max = p.max.unwrap_or(1000).max(1);
    let dir = vpath(&p.path)?;
    crate::files::readable(ctx, &crate::files::real(ctx, &dir)?)?;
    let mut entries: Vec<EntryInfo> = if let Some((id, rel)) = keel_vfs::library::split(&dir) {
        let children = ctx
            .lib
            .list_children(&SourceId(id.to_owned()), rel)
            .map_err(|e| ApiError::not_found(format!("{e:#}")))?;
        children
            .iter()
            .map(|h| EntryInfo {
                name: h.name.clone(),
                path: keel_vfs::library::path(id, &format!("{rel}/{}", h.name)).display(),
                is_dir: h.is_dir,
                size: h.size,
                modified: h.modified,
                hidden: false,
            })
            .collect()
    } else {
        let provider = ctx
            .router
            .provider_for(&dir)
            .ok_or_else(|| ApiError::not_found(format!("nothing serves {}", dir.display())))?;
        let mut v: Vec<EntryInfo> = provider
            .list(&dir)?
            .into_iter()
            .map(|e| EntryInfo {
                name: e.name.clone(),
                path: e.path.display(),
                is_dir: e.kind == keel_vfs::Kind::Dir,
                size: e.size,
                modified: unix(e.modified),
                hidden: e.hidden,
            })
            .collect();
        v.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        v
    };
    let truncated = entries.len() > max;
    entries.truncate(max);
    Ok(Listing { entries, truncated })
}

fn indexed(ctx: &Ctx, h: &LibraryHit) -> Result<IndexedInfo> {
    let ids = ctx.lib.tags_of(&h.record)?;
    let tags = ctx.lib.tags()?;
    Ok(IndexedInfo {
        source: h.record.source.0.clone(),
        source_label: h.source_label.clone(),
        record: h.record.id,
        tags: tags
            .iter()
            .filter(|t| ids.contains(&t.id))
            .map(|t| t.name.clone())
            .collect(),
        favorite: ids.contains(&FAVORITES),
    })
}

fn stat(ctx: &Ctx, p: PathParams) -> Result<StatInfo> {
    let path = vpath(&p.path)?;
    crate::files::readable(ctx, &crate::files::real(ctx, &path)?)?;
    if keel_vfs::library::split(&path).is_some() {
        let h = record_at(ctx, &p.path)?;
        return Ok(StatInfo {
            entry: EntryInfo {
                name: h.name.clone(),
                path: path.display(),
                is_dir: h.is_dir,
                size: h.size,
                modified: h.modified,
                hidden: false,
            },
            indexed: Some(indexed(ctx, &h)?),
        });
    }
    let provider = ctx
        .router
        .provider_for(&path)
        .ok_or_else(|| ApiError::not_found(format!("nothing serves {}", path.display())))?;
    let e = provider
        .stat(&path)
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?;
    let indexed = match record_at(ctx, &p.path) {
        Ok(h) => Some(indexed(ctx, &h)?),
        Err(_) => None,
    };
    Ok(StatInfo {
        entry: EntryInfo {
            name: e.name.clone(),
            path: e.path.display(),
            is_dir: e.kind == keel_vfs::Kind::Dir,
            size: e.size,
            modified: unix(e.modified),
            hidden: e.hidden,
        },
        indexed,
    })
}

fn search(ctx: &Ctx, p: SearchParams) -> Result<Vec<Hit>> {
    let mut q = LibraryQuery::parse(&p.query, ctx.utc_offset)
        .map_err(|e| ApiError::invalid_params(format!("{e:#}")))?;
    q.max = p.max.unwrap_or(100).clamp(1, 10_000);
    if p.contents == Some(false) {
        q.contents = false;
    }
    // Snippets for the first page only, whatever `max` is.
    q.snippets = q.max.min(keel_core::SNIPPET_PAGE);
    Ok(hits(ctx.lib.search(&q)?))
}

fn system_search(ctx: &Ctx, p: SystemSearchParams) -> Result<SystemSearchResult> {
    let searcher = ctx.system.get();
    let found = crate::system::run(&*searcher, &p, crate::system::MAX_SCAN)?;
    Ok(SystemSearchResult {
        backend: found.backend,
        total: found.total,
        capped: found.capped,
        offset: found.offset,
        hits: found
            .hits
            .iter()
            .map(|h| SystemHit {
                path: h.path.display(),
                name: h.path.name().to_owned(),
                parent: h.path.parent().map(|p| p.display()).unwrap_or_default(),
                is_dir: h.is_dir,
                size: if h.is_dir { 0 } else { h.size },
                modified: unix(h.modified),
            })
            .collect(),
    })
}

// --- tags ---

fn tags_list(ctx: &Ctx, p: TagsListParams) -> Result<Vec<TagInfo>> {
    let mut tags = ctx.lib.tags()?;
    if let Some(path) = &p.path {
        let ids = ctx.lib.tags_of(&record_at(ctx, path)?.record)?;
        tags.retain(|t| ids.contains(&t.id));
    }
    Ok(tags
        .into_iter()
        .map(|t| TagInfo {
            id: t.id,
            name: t.name,
            color: t.color,
            parent: t.parent,
        })
        .collect())
}

fn tag_named(ctx: &Ctx, name: &str) -> Result<Option<keel_core::Tag>> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::invalid_params("empty tag name"));
    }
    let tags = ctx.lib.tags()?;
    Ok(tags
        .iter()
        .find(|t| t.name == name && t.parent.is_none())
        .or_else(|| tags.iter().find(|t| t.name.eq_ignore_ascii_case(name)))
        .cloned())
}

fn tag_or_create(ctx: &Ctx, name: &str, color: Option<&str>) -> Result<i64> {
    match tag_named(ctx, name)? {
        Some(t) => Ok(t.id),
        None => Ok(ctx.lib.create_tag(name.trim(), color, None)?),
    }
}

fn tags_tagged(ctx: &Ctx, _: NoParams) -> Result<Vec<TaggedPath>> {
    let mut by_path: std::collections::BTreeMap<String, Vec<i64>> = Default::default();
    let ids = ctx.lib.tags()?.into_iter().map(|t| t.id).chain([FAVORITES]);
    for tag in ids {
        for hit in ctx.lib.records_with_tag(tag)? {
            by_path.entry(hit.path.display()).or_default().push(tag);
        }
    }
    Ok(by_path
        .into_iter()
        .map(|(path, tags)| TaggedPath { path, tags })
        .collect())
}

// --- folder styles ---

/// The indexed file or folder at `path`, for comments.
fn commented_at(ctx: &Ctx, path: &str) -> Result<RecordRef> {
    let p = vpath(path)?;
    locate(ctx, &p).map_err(|_| {
        ApiError::not_found(format!(
            "{path} is in no library source: add the folder's drive to the library to comment on it"
        ))
    })?;
    ctx.lib
        .record_at(&p)?
        .ok_or_else(|| ApiError::not_found(format!("{path} is not in the library index yet")))
}

fn library_comment(ctx: &Ctx, p: PathParams) -> Result<CommentInfo> {
    let record = commented_at(ctx, &p.path)?;
    Ok(CommentInfo {
        path: vpath(&p.path)?.display(),
        comment: ctx.lib.comment(&record)?,
    })
}

fn set_comment_preview(ctx: &Ctx, p: &SetCommentParams) -> Result<Preview> {
    commented_at(ctx, &p.path)?;
    if p.comment.len() > keel_core::MAX_COMMENT {
        return Err(ApiError::invalid_params("a comment is at most 64 KiB"));
    }
    let path = vpath(&p.path)?.display();
    let what = match p.comment.trim().is_empty() {
        true => "no comment".to_owned(),
        false => format!("{:?}", p.comment),
    };
    Ok(Preview {
        pin: None,
        summary: format!("Set the comment of {path}: {what}"),
        changes: vec![change("comment", Some(path), Some(what))],
        warnings: Vec::new(),
    })
}

fn set_comment(ctx: &Ctx, p: SetCommentParams) -> Result<CommentInfo> {
    let record = commented_at(ctx, &p.path)?;
    ctx.lib
        .set_comment(&record, &p.comment)
        .map_err(|e| ApiError::invalid_params(format!("{e:#}")))?;
    Ok(CommentInfo {
        path: vpath(&p.path)?.display(),
        comment: ctx.lib.comment(&record)?,
    })
}

/// The indexed folder or file at `path` (a source's root included), and whether it is a
/// folder.
fn folder_at(ctx: &Ctx, path: &str) -> Result<(RecordRef, bool)> {
    let p = vpath(path)?;
    locate(ctx, &p).map_err(|_| {
        ApiError::not_found(format!(
            "{path} is in no library source: add the folder's drive to the library to style it"
        ))
    })?;
    let found = ctx.lib.record_at(&p)?;
    let record = found
        .ok_or_else(|| ApiError::not_found(format!("{path} is not in the library index yet")))?;
    let is_dir = ctx.lib.record(&record)?.is_some_and(|h| h.is_dir);
    Ok((record, is_dir))
}

fn style_info(path: String, s: keel_core::FolderStyle) -> FolderStyleInfo {
    FolderStyleInfo {
        path,
        color: s.color,
        icon: s.icon,
    }
}

fn folder_style(ctx: &Ctx, p: FolderStyleParams) -> Result<Vec<FolderStyleInfo>> {
    match &p.path {
        Some(path) => {
            let style = ctx.lib.folder_style(&folder_at(ctx, path)?.0)?;
            Ok(vec![style_info(
                vpath(path)?.display(),
                style.unwrap_or_default(),
            )])
        }
        None => Ok(ctx
            .lib
            .folder_styles()?
            .into_iter()
            .map(|(h, s)| style_info(h.path.display(), s))
            .collect()),
    }
}

/// The folder and the style it gets: the given fields over its current style.
fn wanted_style(
    ctx: &Ctx,
    p: &SetFolderStyleParams,
) -> Result<(RecordRef, keel_core::FolderStyle)> {
    let (record, is_dir) = folder_at(ctx, &p.path)?;
    let mut style = ctx.lib.folder_style(&record)?.unwrap_or_default();
    let field = |v: &Option<String>, old: Option<String>| match v.as_deref() {
        None => old,
        Some("") => None,
        Some(v) => Some(v.to_owned()),
    };
    style.color = field(&p.color, style.color.take());
    style.icon = field(&p.icon, style.icon.take());
    style
        .check()
        .map_err(|e| ApiError::invalid_params(format!("{e:#}")))?;
    if !is_dir && style.color.is_some() {
        return Err(ApiError::invalid_params(format!(
            "{} is a file: only folders take a color",
            p.path
        )));
    }
    Ok((record, style))
}

fn style_text(s: &keel_core::FolderStyle) -> String {
    let parts: Vec<String> = [
        s.color.as_ref().map(|c| format!("color {c}")),
        s.icon.as_ref().map(|i| format!("icon {i}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    match parts.is_empty() {
        true => "no color or icon".into(),
        false => parts.join(", "),
    }
}

fn set_folder_style_preview(ctx: &Ctx, p: &SetFolderStyleParams) -> Result<Preview> {
    let (_, style) = wanted_style(ctx, p)?;
    let path = vpath(&p.path)?.display();
    Ok(Preview {
        pin: None,
        summary: format!("Set the folder style of {path}: {}", style_text(&style)),
        changes: vec![change("folder_style", Some(path), Some(style_text(&style)))],
        warnings: Vec::new(),
    })
}

fn set_folder_style(ctx: &Ctx, p: SetFolderStyleParams) -> Result<FolderStyleInfo> {
    let (record, style) = wanted_style(ctx, &p)?;
    ctx.lib
        .set_folder_style(&record, &style)
        .map_err(|e| ApiError::invalid_params(format!("{e:#}")))?;
    Ok(style_info(vpath(&p.path)?.display(), style))
}

fn views_list(ctx: &Ctx, _: NoParams) -> Result<Vec<ViewInfo>> {
    Ok(ctx.lib.views()?.into_iter().map(view_info).collect())
}

/// Task 76: a core view as the API shows it.
pub(crate) fn view_info(v: keel_core::View) -> ViewInfo {
    let m = v.matching;
    ViewInfo {
        id: v.id,
        name: v.name,
        query: v.query,
        layout: v.layout,
        backend: v.backend.as_str().to_owned(),
        sort: v.sort,
        columns: v.columns,
        matching: MatchInfo {
            case: m.case,
            whole_words: m.whole_words,
            path: m.path,
            diacritics: m.diacritics,
            regex: m.regex,
            prefix: m.prefix,
            suffix: m.suffix,
            ignore_punctuation: m.ignore_punctuation,
            ignore_whitespace: m.ignore_whitespace,
        },
        filter: v.filter,
        folder: v.folder,
        position: v.position,
        shortcut: v.shortcut,
    }
}

fn match_toggles(m: MatchInfo) -> keel_core::MatchToggles {
    keel_core::MatchToggles {
        case: m.case,
        whole_words: m.whole_words,
        path: m.path,
        diacritics: m.diacritics,
        regex: m.regex,
        prefix: m.prefix,
        suffix: m.suffix,
        ignore_punctuation: m.ignore_punctuation,
        ignore_whitespace: m.ignore_whitespace,
    }
}

fn view_backend(s: &str) -> Result<keel_core::ViewBackend> {
    keel_core::ViewBackend::parse(s).ok_or_else(|| {
        ApiError::invalid_params(format!("backend must be library or system, not {s:?}"))
    })
}

/// The view `views.create` would save (checked, not saved).
fn new_view(p: &ViewCreateParams) -> Result<keel_core::View> {
    let v = keel_core::View {
        id: 0,
        name: p.name.trim().to_owned(),
        query: p.query.clone(),
        layout: p.layout.clone().unwrap_or_else(|| "details".into()),
        backend: view_backend(p.backend.as_deref().unwrap_or("library"))?,
        sort: p.sort.clone().unwrap_or_default(),
        columns: p.columns.clone().unwrap_or_default(),
        matching: match_toggles(p.matching.unwrap_or_default()),
        filter: p.filter.clone().unwrap_or_default(),
        folder: p.folder.clone().unwrap_or_default(),
        position: p.position.unwrap_or(0),
        shortcut: p.shortcut.clone().unwrap_or_default(),
    };
    keel_core::check_view(&v).map_err(|e| ApiError::invalid_params(format!("{e:#}")))?;
    Ok(v)
}

fn existing_view(ctx: &Ctx, id: i64) -> Result<keel_core::View> {
    ctx.lib
        .view(id)?
        .ok_or_else(|| ApiError::not_found(format!("no view {id}")))
}

/// The view after `views.update` (checked, not saved).
fn updated_view(ctx: &Ctx, p: &ViewUpdateParams) -> Result<(keel_core::View, keel_core::View)> {
    let old = existing_view(ctx, p.id)?;
    let mut v = old.clone();
    if let Some(name) = &p.name {
        v.name = name.trim().to_owned();
    }
    let fields: [(&Option<String>, &mut String); 6] = [
        (&p.query, &mut v.query),
        (&p.layout, &mut v.layout),
        (&p.sort, &mut v.sort),
        (&p.filter, &mut v.filter),
        (&p.folder, &mut v.folder),
        (&p.shortcut, &mut v.shortcut),
    ];
    for (new, field) in fields {
        if let Some(new) = new {
            new.clone_into(field);
        }
    }
    if let Some(backend) = &p.backend {
        v.backend = view_backend(backend)?;
    }
    if let Some(columns) = &p.columns {
        v.columns = columns.clone();
    }
    if let Some(m) = p.matching {
        v.matching = match_toggles(m);
    }
    if let Some(position) = p.position {
        v.position = position;
    }
    keel_core::check_view(&v).map_err(|e| ApiError::invalid_params(format!("{e:#}")))?;
    Ok((old, v))
}

/// A view's place for a preview: `Work/Invoices` (folder and name).
fn view_place(v: &keel_core::View) -> String {
    match v.folder.as_str() {
        "" => v.name.clone(),
        folder => format!("{folder}/{}", v.name),
    }
}

/// Refuses a shortcut another view already has, at the preview.
fn free_shortcut(ctx: &Ctx, v: &keel_core::View) -> Result<()> {
    let clash = (ctx.lib.views()?.into_iter()).find(|o| {
        o.id != v.id && !v.shortcut.is_empty() && o.shortcut.eq_ignore_ascii_case(&v.shortcut)
    });
    match clash {
        Some(o) => Err(ApiError::invalid_params(format!(
            "the shortcut {} is taken by the view {:?}",
            v.shortcut, o.name
        ))),
        None => Ok(()),
    }
}

fn views_create_preview(ctx: &Ctx, p: &ViewCreateParams) -> Result<Preview> {
    let v = new_view(p)?;
    free_shortcut(ctx, &v)?;
    Ok(Preview {
        pin: None,
        summary: format!("Save the view {}: {}", view_place(&v), v.query),
        changes: vec![change("view.create", None, Some(view_place(&v)))],
        warnings: Vec::new(),
    })
}

fn views_create(ctx: &Ctx, p: ViewCreateParams) -> Result<ViewInfo> {
    let v = new_view(&p)?;
    let saved = ctx
        .lib
        .add_view(&v)
        .map_err(|e| ApiError::invalid_params(format!("{e:#}")))?;
    Ok(view_info(saved))
}

fn views_update_preview(ctx: &Ctx, p: &ViewUpdateParams) -> Result<Preview> {
    let (old, v) = updated_view(ctx, p)?;
    free_shortcut(ctx, &v)?;
    let summary = match view_place(&old) == view_place(&v) {
        true => format!("Change the view {}", view_place(&v)),
        false => format!("Change the view {} to {}", view_place(&old), view_place(&v)),
    };
    Ok(Preview {
        pin: None,
        summary,
        changes: vec![change("view.update", None, Some(view_place(&v)))],
        warnings: Vec::new(),
    })
}

fn views_update(ctx: &Ctx, p: ViewUpdateParams) -> Result<ViewInfo> {
    let (_, v) = updated_view(ctx, &p)?;
    ctx.lib
        .update_view(&v)
        .map_err(|e| ApiError::invalid_params(format!("{e:#}")))?;
    Ok(view_info(v))
}

fn views_delete_preview(ctx: &Ctx, p: &ViewIdParams) -> Result<Preview> {
    let v = existing_view(ctx, p.id)?;
    Ok(Preview {
        pin: None,
        summary: format!("Delete the view {}", view_place(&v)),
        changes: vec![change("view.delete", None, Some(view_place(&v)))],
        warnings: Vec::new(),
    })
}

fn views_delete(ctx: &Ctx, p: ViewIdParams) -> Result<ViewInfo> {
    let v = existing_view(ctx, p.id)?;
    ctx.lib.delete_view(p.id)?;
    Ok(view_info(v))
}

fn tag_changes(action: &str, hits: &[LibraryHit], detail: &str) -> Vec<Change> {
    hits.iter()
        .map(|h| change(action, Some(h.path.display()), Some(detail.to_owned())))
        .collect()
}

/// `tags.add`'s paths: none only creates the tag.
fn tag_targets(ctx: &Ctx, p: &TagParams) -> Result<Vec<LibraryHit>> {
    match p.paths.is_empty() {
        true => Ok(Vec::new()),
        false => records(ctx, &p.paths),
    }
}

fn tags_add_preview(ctx: &Ctx, p: &TagParams) -> Result<Preview> {
    let hits = tag_targets(ctx, p)?;
    let mut warnings = Vec::new();
    if tag_named(ctx, &p.tag)?.is_none() {
        warnings.push(warning(
            "creates_tag",
            None,
            format!(
                "tag {:?} does not exist yet and will be created",
                p.tag.trim()
            ),
        ));
    }
    let summary = match hits.is_empty() {
        true => format!("Create tag {}", p.tag.trim()),
        false => format!("Tag {} item(s) with {}", hits.len(), p.tag.trim()),
    };
    Ok(Preview {
        pin: None,
        summary,
        changes: tag_changes("tag.add", &hits, p.tag.trim()),
        warnings,
    })
}

fn tags_add(ctx: &Ctx, p: TagParams) -> Result<Tagged> {
    let hits = tag_targets(ctx, &p)?;
    let tag = tag_or_create(ctx, &p.tag, p.color.as_deref())?;
    ctx.lib.set_tag(tag, &refs(&hits), true)?;
    Ok(Tagged {
        records: hits.len(),
    })
}

fn existing_tag(ctx: &Ctx, name: &str) -> Result<keel_core::Tag> {
    tag_named(ctx, name)?.ok_or_else(|| ApiError::not_found(format!("no tag {name:?}")))
}

fn tags_remove_preview(ctx: &Ctx, p: &TagParams) -> Result<Preview> {
    let tag = existing_tag(ctx, &p.tag)?;
    let hits = records(ctx, &p.paths)?;
    Ok(Preview {
        pin: None,
        summary: format!("Remove tag {} from {} item(s)", tag.name, hits.len()),
        changes: tag_changes("tag.remove", &hits, &tag.name),
        warnings: Vec::new(),
    })
}

fn tags_remove(ctx: &Ctx, p: TagParams) -> Result<Tagged> {
    let tag = existing_tag(ctx, &p.tag)?;
    let hits = records(ctx, &p.paths)?;
    ctx.lib.set_tag(tag.id, &refs(&hits), false)?;
    Ok(Tagged {
        records: hits.len(),
    })
}

fn tags_set_preview(ctx: &Ctx, p: &TagsSetParams) -> Result<Preview> {
    let hits = records(ctx, &p.paths)?;
    let mut warnings = Vec::new();
    for name in &p.tags {
        if tag_named(ctx, name)?.is_none() {
            warnings.push(warning(
                "creates_tag",
                None,
                format!(
                    "tag {:?} does not exist yet and will be created",
                    name.trim()
                ),
            ));
        }
    }
    let names = p
        .tags
        .iter()
        .map(|t| t.trim())
        .collect::<Vec<_>>()
        .join(", ");
    Ok(Preview {
        pin: None,
        summary: format!("Set the tags of {} item(s) to [{names}]", hits.len()),
        changes: tag_changes("tags.set", &hits, &names),
        warnings,
    })
}

fn tags_set(ctx: &Ctx, p: TagsSetParams) -> Result<Tagged> {
    let hits = records(ctx, &p.paths)?;
    let want = p
        .tags
        .iter()
        .map(|t| tag_or_create(ctx, t, None))
        .collect::<Result<Vec<_>>>()?;
    for h in &hits {
        for id in ctx.lib.tags_of(&h.record)? {
            if id != FAVORITES && !want.contains(&id) {
                ctx.lib
                    .set_tag(id, std::slice::from_ref(&h.record), false)?;
            }
        }
    }
    for id in want {
        ctx.lib.set_tag(id, &refs(&hits), true)?;
    }
    Ok(Tagged {
        records: hits.len(),
    })
}

fn favorites_list(ctx: &Ctx, _: NoParams) -> Result<Vec<Hit>> {
    Ok(hits(ctx.lib.favorites()?))
}

fn favorites_set_preview(ctx: &Ctx, p: &FavoritesSetParams) -> Result<Preview> {
    let hits = records(ctx, &p.paths)?;
    let (action, verb) = match p.on {
        true => ("favorite.add", "Add"),
        false => ("favorite.remove", "Remove"),
    };
    Ok(Preview {
        pin: None,
        summary: format!(
            "{verb} {} item(s) {} Favorites",
            hits.len(),
            if p.on { "to" } else { "from" }
        ),
        changes: tag_changes(action, &hits, "Favorites"),
        warnings: Vec::new(),
    })
}

fn favorites_set(ctx: &Ctx, p: FavoritesSetParams) -> Result<Tagged> {
    let hits = records(ctx, &p.paths)?;
    ctx.lib.set_favorite(&refs(&hits), p.on)?;
    Ok(Tagged {
        records: hits.len(),
    })
}

fn recents(ctx: &Ctx, p: LimitParams) -> Result<Vec<Hit>> {
    Ok(hits(ctx.lib.recents(p.limit.unwrap_or(50).clamp(1, 1000))?))
}

fn files_names(ctx: &Ctx, p: FileNamesParams) -> Result<FileNames> {
    let max = p.max.unwrap_or(keel_core::FILE_NAMES_MAX);
    let (paths, truncated) = ctx.lib.file_names(max.min(keel_core::FILE_NAMES_MAX))?;
    Ok(FileNames {
        paths: paths.iter().map(VPath::display).collect(),
        truncated,
    })
}

fn recents_note(ctx: &Ctx, p: PathParams) -> Result<Done> {
    ctx.lib.note_open(&record_at(ctx, &p.path)?.record)?;
    Ok(Done { ok: true })
}

// --- jobs ---

fn job_info(j: keel_core::JobInfo, with_log: bool) -> JobInfo {
    JobInfo {
        id: j.id,
        kind: j.kind,
        status: format!("{:?}", j.status).to_lowercase(),
        progress: j.progress,
        created: j.created,
        updated: j.updated,
        result: j.result,
        log: with_log.then_some(j.log),
    }
}

fn jobs_list(ctx: &Ctx, _: NoParams) -> Result<Vec<JobInfo>> {
    Ok(ctx
        .lib
        .jobs()
        .list()?
        .into_iter()
        .map(|j| job_info(j, false))
        .collect())
}

fn job(ctx: &Ctx, id: i64) -> Result<keel_core::JobInfo> {
    ctx.lib
        .jobs()
        .info(id)
        .map_err(|_| ApiError::not_found(format!("no job {id}")))
}

fn jobs_info(ctx: &Ctx, p: JobParams) -> Result<JobInfo> {
    Ok(job_info(job(ctx, p.id)?, true))
}

fn jobs_cancel_preview(ctx: &Ctx, p: &JobParams) -> Result<Preview> {
    let j = job_info(job(ctx, p.id)?, false);
    let mut warnings = Vec::new();
    if j.finished() {
        warnings.push(warning(
            "finished",
            None,
            format!("job {} already ended ({})", j.id, j.status),
        ));
    }
    Ok(Preview {
        pin: None,
        summary: format!("Cancel job {} ({}, {})", j.id, j.kind, j.status),
        changes: vec![change("job.cancel", None, Some(j.kind.clone()))],
        warnings,
    })
}

fn jobs_cancel(ctx: &Ctx, p: JobParams) -> Result<JobInfo> {
    job(ctx, p.id)?;
    ctx.lib.jobs().cancel(p.id)?;
    Ok(job_info(job(ctx, p.id)?, false))
}

// --- content ---

fn duplicates(ctx: &Ctx, p: DuplicatesParams) -> Result<Vec<DupGroup>> {
    let mut groups = ctx.lib.duplicates(p.min_size.unwrap_or(1))?;
    groups.sort_by_key(|g| std::cmp::Reverse(g.size * (g.records.len() as u64 - 1)));
    groups.truncate(p.max.unwrap_or(100).clamp(1, 10_000));
    groups
        .into_iter()
        .map(|g| {
            let mut paths = Vec::new();
            for r in &g.records {
                if let Some(h) = ctx.lib.record(r)? {
                    paths.push(h.path.display());
                }
            }
            Ok(DupGroup {
                content_id: g.cas_id.iter().map(|b| format!("{b:02x}")).collect(),
                size: g.size,
                paths,
            })
        })
        .collect()
}

fn redundancy(ctx: &Ctx, p: PathParams) -> Result<Copies> {
    let h = record_at(ctx, &p.path)?;
    Ok(ctx.lib.redundancy(&h.record)?.into())
}

/// Files per folder `redundancy.folder` answers for.
const FOLDER_COPIES: usize = 5_000;

fn redundancy_folder(ctx: &Ctx, p: PathParams) -> Result<Vec<FileCopies>> {
    let dir = vpath(&p.path)?;
    let (source, rel) = locate(ctx, &dir)?;
    let files: Vec<LibraryHit> = ctx
        .lib
        .list_children(&source, &rel)
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?
        .into_iter()
        .filter(|h| !h.is_dir)
        .take(FOLDER_COPIES)
        .collect();
    let all = ctx.lib.redundancies(&refs(&files))?;
    Ok(files
        .into_iter()
        .zip(all)
        .filter_map(|(h, r)| {
            r.map(|r| FileCopies {
                path: h.path.display(),
                copies: r.into(),
            })
        })
        .collect())
}

/// A redundancy as the API shows it (every location with its volume's state).
impl From<keel_core::Redundancy> for Copies {
    fn from(c: keel_core::Redundancy) -> Copies {
        Copies {
            copies: c.copies,
            failure_domains: c.failure_domains,
            backed_up: c.backed_up,
            offline_copies: c.offline_copies,
            locations: c
                .locations
                .into_iter()
                .map(|l| Location {
                    path: l.path.display(),
                    source_label: l.source_label,
                    volume: l.volume.label.clone(),
                    failure_domain: l.volume.failure_domain.clone(),
                    state: Some(volume_state(l.volume.state)),
                    backup: l.volume.backup,
                    claimed: l.claimed,
                })
                .collect(),
        }
    }
}

// --- protection, volumes, background work ---

// --- backup ---

/// A backup file's path: absolute and local (the host's working folder is not the
/// caller's; nothing remote).
fn local_file(s: &str) -> Result<std::path::PathBuf> {
    let s = s.trim();
    let path = std::path::PathBuf::from(s);
    if s.contains("://") || !path.is_absolute() {
        return Err(ApiError::invalid_params(format!(
            "not an absolute local path: {s}"
        )));
    }
    Ok(path)
}

/// The data folder holding the open library (`<data>/library/<name>/`).
fn data_root(ctx: &Ctx) -> Result<std::path::PathBuf> {
    (ctx.lib.dir().parent().and_then(std::path::Path::parent))
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| ApiError::failed("no data folder"))
}

/// Where `library.backup` writes: the file, or a dated name in the folder; never an
/// existing file, always `.keelbackup`.
fn backup_target(ctx: &Ctx, p: &BackupParams) -> Result<std::path::PathBuf> {
    let mut path = local_file(&p.file)?;
    if path.is_dir() {
        path = path.join(keel_core::backup::file_name(&ctx.lib.name, unix_now()));
    }
    if path.extension().and_then(|e| e.to_str()) != Some(keel_core::backup::EXTENSION) {
        return Err(ApiError::invalid_params(format!(
            "a backup file ends in .{}",
            keel_core::backup::EXTENSION
        )));
    }
    if path.exists() {
        return Err(ApiError::failed(format!(
            "{} exists: choose another file",
            path.display()
        )));
    }
    if !path.parent().is_some_and(std::path::Path::is_dir) {
        return Err(ApiError::failed(format!(
            "no folder {}",
            path.parent().unwrap_or(&path).display()
        )));
    }
    Ok(path)
}

fn unix_now() -> i64 {
    unix(Some(std::time::SystemTime::now())).unwrap_or(0)
}

fn backup_preview(ctx: &Ctx, p: &BackupParams) -> Result<Preview> {
    let path = backup_target(ctx, p)?;
    let stats = ctx.lib.stats();
    Ok(Preview {
        pin: None,
        summary: format!(
            "Back up library {} ({} source(s), {} files) to {}",
            ctx.lib.name,
            stats.sources,
            stats.files,
            path.display()
        ),
        changes: vec![change(
            "library.backup",
            Some(path.display().to_string()),
            Some(format!(
                "library.db and {} source store(s); thumbnails and the search index are not included",
                stats.sources
            )),
        )],
        warnings: Vec::new(),
    })
}

fn backup(ctx: &Ctx, p: BackupParams) -> Result<BackupWritten> {
    let path = backup_target(ctx, &p)?;
    let m = ctx.lib.backup_new(&path)?;
    Ok(BackupWritten {
        file: path.display().to_string(),
        bytes: std::fs::metadata(&path).map_or(0, |m| m.len()),
        created: m.created,
        sources: m.sources.len(),
    })
}

fn restore_preview(ctx: &Ctx, p: &RestoreParams) -> Result<Preview> {
    let file = local_file(&p.file)?;
    let m = keel_core::backup::inspect(&file)?;
    let name = p.name.clone().unwrap_or_else(|| m.library.clone());
    let root = data_root(ctx)?;
    if keel_core::backup::exists(&root, &name) {
        return Err(ApiError::failed(
            keel_core::BackupError::NameTaken(name).to_string(),
        ));
    }
    let mut changes = vec![change(
        "library.restore",
        Some(root.join("library").join(&name).display().to_string()),
        Some(format!(
            "library {name} from {} (backed up by Keel {})",
            m.library, m.keel_version
        )),
    )];
    for s in &m.sources {
        let state = match keel_core::backup::reachable(&s.root) {
            true => "walked again when the library opens",
            false => "offline until its folder is back",
        };
        changes.push(change(
            "source.restore",
            Some(s.root.display()),
            Some(format!("{}: {state}", s.label)),
        ));
    }
    Ok(Preview {
        pin: None,
        summary: format!(
            "Restore library {} ({} source(s)) from {} as a new library {name}",
            m.library,
            m.sources.len(),
            file.display()
        ),
        changes,
        warnings: vec![warning(
            "not_opened",
            None,
            format!(
                "Library {} stays open here: switch to {name} in Settings → Library",
                ctx.lib.name
            ),
        )],
    })
}

fn restore(ctx: &Ctx, p: RestoreParams) -> Result<RestoredLibrary> {
    let file = local_file(&p.file)?;
    let r = keel_core::backup::restore(&data_root(ctx)?, &file, p.name.as_deref())?;
    Ok(RestoredLibrary {
        name: r.name,
        dir: r.dir.display().to_string(),
        sources: r
            .sources
            .into_iter()
            .map(|s| RestoredSourceInfo {
                id: s.id,
                label: s.label,
                root: s.root.display(),
                online: s.online,
            })
            .collect(),
    })
}

fn library_folder_sizes(ctx: &Ctx, p: FolderSizesParams) -> Result<FolderSizes> {
    let dir = vpath(&p.path)?;
    let (source, rel) = locate(ctx, &dir)?;
    let max = p.max.unwrap_or(1000).max(1);
    let sizes = ctx
        .lib
        .folder_sizes(&source, &rel, p.dirs_only, max)
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?;
    let split = |t: Option<keel_core::Total>| (t.map(|t| t.bytes), t.map(|t| t.files));
    let (bytes, files) = split(sizes.total);
    Ok(FolderSizes {
        path: dir.display(),
        bytes,
        files,
        truncated: sizes.truncated,
        children: sizes
            .children
            .into_iter()
            .map(|c| {
                let (bytes, files) = split(c.total);
                ChildSize {
                    path: dir.join(&c.name).display(),
                    name: c.name,
                    is_dir: c.is_dir,
                    bytes,
                    files,
                }
            })
            .collect(),
    })
}

/// `library.inspect`'s longest lists.
const INSPECT_MAX: usize = 1000;

fn library_inspect(ctx: &Ctx, p: InspectParams) -> Result<Inspection> {
    let path = vpath(&p.path)?;
    let real = crate::files::real(ctx, &path)?;
    crate::files::readable(ctx, &real)?;
    let max = p.max.unwrap_or(50).clamp(1, INSPECT_MAX);
    let found = ctx.lib.inspect(&path, max)?;
    let live = (ctx.router.provider_for(&real)).and_then(|pr| pr.stat(&real).ok());
    let entry = match (live, &found) {
        (Some(e), _) => EntryInfo {
            name: e.name.clone(),
            path: path.display(),
            is_dir: e.kind == keel_vfs::Kind::Dir,
            size: e.size,
            modified: unix(e.modified),
            hidden: e.hidden,
        },
        (None, Some(i)) => EntryInfo {
            name: i.path.name().to_owned(),
            path: path.display(),
            is_dir: i.is_dir,
            size: i.size,
            modified: i.modified,
            hidden: false,
        },
        (None, None) => {
            return Err(ApiError::not_found(format!(
                "{} cannot be reached and is not in the library",
                path.display()
            )))
        }
    };
    let Some(i) = found else {
        return Ok(Inspection {
            entry,
            library: None,
        });
    };
    let all = ctx.lib.tags()?;
    let hex = |b: &[u8]| b.iter().map(|b| format!("{b:02x}")).collect::<String>();
    Ok(Inspection {
        entry,
        library: Some(InspectedRecord {
            source: i.record.source.0.clone(),
            source_label: i.source_label,
            record: i.record.id,
            volume: volume_info(i.volume),
            content_id: i.content_id.map(|c| hex(&c)),
            first_seen: i.first_seen,
            last_hashed: i.last_hashed,
            copies: i.redundancy.map(Copies::from),
            more_copies: i.more_copies,
            tags: all
                .into_iter()
                .filter(|t| i.tags.contains(&t.id))
                .map(|t| TagInfo {
                    id: t.id,
                    name: t.name,
                    color: t.color,
                    parent: t.parent,
                })
                .collect(),
            favorite: i.tags.contains(&FAVORITES),
            history: i
                .history
                .into_iter()
                .map(|h| HistoryEntry {
                    id: h.id,
                    ts: h.ts,
                    kind: h.kind,
                    ok: h.ok,
                    from: h.from,
                    to: h.to,
                    tag: h.tag,
                })
                .collect(),
            more_history: i.more_history,
            folder: i.folder.map(|f| FolderTotals {
                bytes: f.total.map(|t| t.bytes),
                files: f.total.map(|t| t.files),
                folders: f.folders,
                biggest: f.biggest.map(|c| ChildSize {
                    path: i.path.join(&c.name).display(),
                    name: c.name,
                    is_dir: c.is_dir,
                    bytes: c.total.map(|t| t.bytes),
                    files: c.total.map(|t| t.files),
                }),
            }),
        }),
    })
}

fn library_stats(ctx: &Ctx, _: NoParams) -> Result<LibraryStats> {
    let s = ctx.lib.stats();
    let hashing = ctx.lib.remote_hash_settings();
    Ok(LibraryStats {
        sources: s.sources,
        offline_sources: s.offline_sources,
        records: s.records,
        files: s.files,
        bytes: s.bytes,
        unique_content: s.unique_content,
        running_jobs: s.running_jobs,
        per_source: s
            .per_source
            .into_iter()
            .map(|p| SourceStats {
                id: p.id.0,
                label: p.label,
                files: p.files,
                folders: p.folders,
                bytes: p.bytes,
                hashed_files: p.hashed_files,
                last_walk: p.last_walk,
                offline: p.offline,
                text: Some(text_stats(p.contents, &p.text)),
            })
            .collect(),
        text: Some(text_stats(s.index_contents, &s.text)),
        hashing: Some(RemoteHashing {
            remote: hashing.hash_remote,
            cloud: hashing.hash_cloud,
            max_remote_bytes: hashing.remote_hash_max_bytes,
        }),
        last_backup: s.last_backup,
    })
}

fn text_stats(on: bool, t: &keel_core::TextCounts) -> TextStats {
    TextStats {
        on,
        documents: t.documents,
        skipped: t.skipped,
        failed: t.failed,
        chars: t.chars,
    }
}

fn protection_summary(ctx: &Ctx, _: NoParams) -> Result<Protection> {
    let p = ctx.lib.protection_summary()?;
    Ok(Protection {
        single_copy: p.single_copy,
        single_domain: p.single_domain,
        unbacked: p.unbacked,
        drifted: p.drifted,
        unchecked: p.unchecked,
        offline_volumes: p.offline_volumes,
    })
}

fn volume_state(s: keel_core::VolumeState) -> VolumeStateName {
    use keel_core::VolumeState as S;
    match s {
        S::Online => VolumeStateName::Online,
        S::Offline => VolumeStateName::Offline,
        S::Archived => VolumeStateName::Archived,
        S::Lost => VolumeStateName::Lost,
        S::Retired => VolumeStateName::Retired,
    }
}

fn volume_info(v: keel_core::Volume) -> VolumeInfo {
    use keel_core::VolumeKind as K;
    VolumeInfo {
        kind: match v.kind {
            K::Fixed => VolumeKindName::Fixed,
            K::Removable => VolumeKindName::Removable,
            K::Network => VolumeKindName::Network,
            K::Cloud => VolumeKindName::Cloud,
            K::Device => VolumeKindName::Device,
        },
        state: volume_state(v.state),
        used: v.capacity.map(|c| c.0),
        total: v.capacity.map(|c| c.1),
        id: v.id,
        label: v.label,
        failure_domain: v.failure_domain,
        domain_set: v.domain_set,
        last_seen: v.last_seen,
        backup: v.backup,
    }
}

fn volumes_list(ctx: &Ctx, _: NoParams) -> Result<Vec<VolumeInfo>> {
    Ok(ctx.lib.volumes()?.into_iter().map(volume_info).collect())
}

fn find_volume(ctx: &Ctx, id: &str) -> Result<keel_core::Volume> {
    ctx.lib
        .volumes()?
        .into_iter()
        .find(|v| v.id == id)
        .ok_or_else(|| ApiError::not_found(format!("no volume {id}")))
}

fn volumes_set_preview(ctx: &Ctx, p: &VolumeSetParams) -> Result<Preview> {
    let v = find_volume(ctx, &p.volume)?;
    let mut changes = Vec::new();
    if let Some(state) = p.state {
        let to = match state {
            VolumeStateName::Online | VolumeStateName::Offline => "automatic".to_owned(),
            s => format!("{s:?}").to_lowercase(),
        };
        changes.push(change("volume.state", None, Some(to)));
    }
    if let Some(on) = p.backup {
        let detail = if on { "backup" } else { "not a backup" };
        changes.push(change("volume.backup", None, Some(detail.into())));
    }
    if let Some(domain) = &p.failure_domain {
        let detail = match domain.trim() {
            "" => "detected".to_owned(),
            d => d.to_owned(),
        };
        changes.push(change("volume.failure_domain", None, Some(detail)));
    }
    if changes.is_empty() {
        return Err(ApiError::invalid_params(
            "nothing to set: give state, backup or failure_domain",
        ));
    }
    Ok(Preview {
        pin: None,
        summary: format!("Change volume {} ({})", v.label, v.id),
        changes,
        warnings: Vec::new(),
    })
}

fn volumes_set(ctx: &Ctx, p: VolumeSetParams) -> Result<VolumeInfo> {
    find_volume(ctx, &p.volume)?;
    if let Some(state) = p.state {
        use keel_core::VolumeState as S;
        let state = match state {
            VolumeStateName::Online => S::Online,
            VolumeStateName::Offline => S::Offline,
            VolumeStateName::Archived => S::Archived,
            VolumeStateName::Lost => S::Lost,
            VolumeStateName::Retired => S::Retired,
        };
        ctx.lib.set_volume_state(&p.volume, state)?;
    }
    if let Some(on) = p.backup {
        ctx.lib.set_backup(&p.volume, on)?;
    }
    if let Some(domain) = &p.failure_domain {
        let domain = Some(domain.trim()).filter(|d| !d.is_empty());
        ctx.lib.set_failure_domain(&p.volume, domain)?;
    }
    Ok(volume_info(find_volume(ctx, &p.volume)?))
}

fn sample_pct(p: &IntegrityParams) -> Result<f64> {
    match p.sample_pct.unwrap_or(1.0) {
        pct if pct > 0.0 && pct <= 100.0 => Ok(pct),
        pct => Err(ApiError::invalid_params(format!(
            "sample_pct {pct} is not in (0, 100]"
        ))),
    }
}

fn integrity_preview(ctx: &Ctx, p: &IntegrityParams) -> Result<Preview> {
    let pct = sample_pct(p)?;
    if p.source.is_some() && p.due_days.is_some() {
        return Err(ApiError::invalid_params(
            "due_days checks every source: leave out source",
        ));
    }
    let what = match &p.source {
        Some(id) => find_source(ctx, id)?.label,
        None => "every source".into(),
    };
    let when = match p.due_days {
        Some(d) => format!(" when the last check is {d} day(s) old"),
        None => String::new(),
    };
    Ok(Preview {
        pin: None,
        summary: format!("Re-hash {pct}% of the hashed files of {what}{when}"),
        changes: vec![change("integrity.check", None, Some(what))],
        warnings: Vec::new(),
    })
}

fn integrity_check(ctx: &Ctx, p: IntegrityParams) -> Result<MaybeJob> {
    let pct = sample_pct(&p)?;
    let job = match (p.due_days, p.source) {
        (Some(days), _) => {
            let every = std::time::Duration::from_secs(u64::from(days) * 24 * 60 * 60);
            ctx.lib.schedule_integrity(pct, every)?
        }
        (None, source) => Some(ctx.lib.integrity(source.map(SourceId), pct)?),
    };
    Ok(MaybeJob { job })
}

fn hashing_preview(ctx: &Ctx, p: &HashingParams) -> Result<Preview> {
    let summary = match (p.on, p.idle_only) {
        (false, _) => "Turn content hashing off (a running hash job is cancelled)",
        (true, true) => "Hash contents while the user is idle",
        (true, false) => "Hash contents, also while the user works",
    };
    let r = hashing_settings(ctx, p);
    let yes = |on: bool| if on { "hashed" } else { "not hashed" };
    let summary = format!(
        "{summary}; SFTP sources {}, cloud sources {} (downloads can cost egress fees), remote files over {} bytes skipped",
        yes(r.hash_remote),
        yes(r.hash_cloud),
        r.remote_hash_max_bytes
    );
    Ok(Preview {
        pin: None,
        summary: summary.clone(),
        changes: vec![change("hashing", None, Some(summary))],
        warnings: Vec::new(),
    })
}

fn hashing_settings(ctx: &Ctx, p: &HashingParams) -> keel_core::RemoteHashSettings {
    let mut settings = ctx.lib.remote_hash_settings();
    if let Some(remote) = p.remote {
        settings.hash_remote = remote;
    }
    if let Some(cloud) = p.cloud {
        settings.hash_cloud = cloud;
    }
    if let Some(max) = p.max_remote_bytes {
        settings.remote_hash_max_bytes = max;
    }
    settings
}

fn hashing_set(ctx: &Ctx, p: HashingParams) -> Result<MaybeJob> {
    ctx.lib
        .set_remote_hash_settings(hashing_settings(ctx, &p))?;
    ctx.lib.set_hash_after_walk(p.on);
    ctx.lib.set_hash_idle_only(p.idle_only);
    if p.on {
        return Ok(MaybeJob {
            job: Some(ctx.lib.hash()?),
        });
    }
    for j in ctx.lib.jobs().list()? {
        let active = matches!(
            j.status,
            keel_core::JobStatus::Queued | keel_core::JobStatus::Running
        );
        if j.kind == "hash" && active {
            ctx.lib.jobs().cancel(j.id)?;
        }
    }
    Ok(MaybeJob { job: None })
}

fn contents_preview(ctx: &Ctx, p: &ContentsParams) -> Result<Preview> {
    let what = match &p.id {
        Some(id) => format!("in {}", find_source(ctx, id)?.label),
        None => "in every source".to_owned(),
    };
    let summary = match (p.id.is_some() && p.default, p.on) {
        (true, _) => format!("Index file contents {what} as its kind does by default (local folders and drives on; shares, SFTP, cloud and devices off)"),
        (false, true) => format!("Index file contents {what}: their text is read at idle priority (on a remote source every document is downloaded once)"),
        (false, false) => format!("Stop indexing file contents {what}: the text read so far is deleted"),
    };
    Ok(Preview {
        pin: None,
        summary: summary.clone(),
        changes: vec![change("contents", None, Some(summary))],
        warnings: Vec::new(),
    })
}

fn contents_set(ctx: &Ctx, p: ContentsParams) -> Result<MaybeJob> {
    let Some(id) = p.id else {
        ctx.lib.set_index_contents(p.on)?;
        return Ok(MaybeJob { job: None });
    };
    find_source(ctx, &id)?;
    let id = SourceId(id);
    ctx.lib
        .set_source_contents(&id, (!p.default).then_some(p.on))?;
    Ok(MaybeJob { job: None })
}

fn activity_note(ctx: &Ctx, _: NoParams) -> Result<Done> {
    ctx.lib.note_activity();
    Ok(Done { ok: true })
}

fn media_index_preview(ctx: &Ctx, p: &SourceIdParams) -> Result<Preview> {
    let s = find_source(ctx, &p.id)?;
    Ok(Preview {
        pin: None,
        summary: format!(
            "Make thumbnails and read metadata of the photos and videos in {}",
            s.label
        ),
        changes: vec![change("media.index", Some(s.root.display()), Some(s.label))],
        warnings: Vec::new(),
    })
}

fn media_index(ctx: &Ctx, p: SourceIdParams) -> Result<JobStarted> {
    find_source(ctx, &p.id)?;
    Ok(JobStarted {
        job: ctx.lib.media_job(&SourceId(p.id))?,
    })
}

// --- file plans ---

fn plan(ctx: &Ctx, p: PlanParams) -> Result<PlanPreview> {
    use keel_core::OnConflict as C;
    let on_conflict = match p.on_conflict.unwrap_or(OnConflict::Skip) {
        OnConflict::Skip => C::Skip,
        OnConflict::Overwrite => C::Overwrite,
        OnConflict::RenameNew => C::RenameNew,
    };
    if p.paths.is_empty() {
        return Err(ApiError::invalid_params("no paths"));
    }
    // Library paths (library://<source>/<rel>) are planned on their real paths.
    let real = |s: &str| crate::files::real(ctx, &vpath(s)?);
    let paths = p
        .paths
        .iter()
        .map(|s| real(s))
        .collect::<Result<Vec<_>>>()?;
    let dst = || -> Result<VPath> {
        real(
            p.to.as_deref()
                .ok_or_else(|| ApiError::invalid_params("copy and move need `to`"))?,
        )
    };
    let op = match p.op {
        FileOpKind::Copy => keel_core::Op::Copy {
            src: paths,
            dst_dir: dst()?,
            on_conflict,
        },
        FileOpKind::Move => keel_core::Op::Move {
            src: paths,
            dst_dir: dst()?,
            on_conflict,
        },
        FileOpKind::Delete => keel_core::Op::Delete { paths },
        FileOpKind::Rename => {
            let [path] = <[VPath; 1]>::try_from(paths)
                .map_err(|_| ApiError::invalid_params("rename takes exactly one path"))?;
            let new_name = p
                .new_name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .ok_or_else(|| ApiError::invalid_params("rename needs `new_name`"))?;
            keel_core::Op::Rename { path, new_name }
        }
    };
    file_plan(ctx, op)
}

fn file_plan(ctx: &Ctx, op: keel_core::Op) -> Result<PlanPreview> {
    let plan = keel_core::validate_preview_execute(&ctx.lib, op)
        .map_err(|e| ApiError::invalid_params(format!("{e:#}")))?;
    store_file_plan(ctx, plan)
}

fn conflict(c: Option<OnConflict>) -> keel_core::OnConflict {
    match c.unwrap_or(OnConflict::Skip) {
        OnConflict::Skip => keel_core::OnConflict::Skip,
        OnConflict::Overwrite => keel_core::OnConflict::Overwrite,
        OnConflict::RenameNew => keel_core::OnConflict::RenameNew,
    }
}

/// A path the caller gave, as the real path (library paths resolved), refused where the
/// read operations are (Keel's configuration folder: no link to the daemon token, no
/// attribute change there).
fn real_path(ctx: &Ctx, s: &str) -> Result<VPath> {
    let p = crate::files::real(ctx, &vpath(s)?)?;
    crate::files::readable(ctx, &p)?;
    Ok(p)
}

fn plan_merge(ctx: &Ctx, p: MergeParams) -> Result<PlanPreview> {
    let folders = (p.paths.iter())
        .map(|s| real_path(ctx, s))
        .collect::<Result<Vec<_>>>()?;
    let what = keel_core::FolderOp::Merge {
        folders,
        into: real_path(ctx, &p.into)?,
    };
    file_plan(ctx, keel_core::Op::arrange(what, conflict(p.on_conflict)))
}

fn plan_flatten(ctx: &Ctx, p: FlattenParams) -> Result<PlanPreview> {
    let what = keel_core::FolderOp::Flatten {
        folder: real_path(ctx, &p.path)?,
    };
    file_plan(ctx, keel_core::Op::arrange(what, conflict(p.on_conflict)))
}

fn plan_organize(ctx: &Ctx, p: OrganizeParams) -> Result<PlanPreview> {
    let what = keel_core::FolderOp::Organize {
        folder: real_path(ctx, &p.path)?,
        by: match p.by {
            OrganizeBy::Type => keel_core::OrganizeBy::Type,
            OrganizeBy::Year => keel_core::OrganizeBy::Year,
            OrganizeBy::Month => keel_core::OrganizeBy::Month,
        },
    };
    file_plan(ctx, keel_core::Op::arrange(what, conflict(p.on_conflict)))
}

fn plan_remove_empty(ctx: &Ctx, p: RemoveEmptyParams) -> Result<PlanPreview> {
    let what = keel_core::FolderOp::RemoveEmpty {
        folder: real_path(ctx, &p.path)?,
    };
    file_plan(
        ctx,
        keel_core::Op::arrange(what, keel_core::OnConflict::Skip),
    )
}

fn plan_set_attributes(ctx: &Ctx, p: AttributesParams) -> Result<PlanPreview> {
    let mode = match p.mode.as_deref() {
        None => None,
        Some(m) => Some(u32::from_str_radix(m.trim(), 8).map_err(|_| {
            ApiError::invalid_params(format!("mode {m:?} is not octal digits (\"755\")"))
        })?),
    };
    let paths = (p.paths.iter())
        .map(|s| real_path(ctx, s))
        .collect::<Result<Vec<_>>>()?;
    let attrs = keel_vfs::Attributes {
        hidden: p.hidden,
        readonly: p.readonly,
        mode,
    };
    file_plan(ctx, keel_core::Op::SetAttributes { paths, attrs })
}

fn plan_link(ctx: &Ctx, p: LinkParams) -> Result<PlanPreview> {
    let op = keel_core::Op::Link {
        target: real_path(ctx, &p.target)?,
        link: real_path(ctx, &p.link)?,
        kind: match p.kind {
            LinkKind::Symlink => keel_vfs::LinkKind::Symlink,
            LinkKind::Junction => keel_vfs::LinkKind::Junction,
            LinkKind::Hardlink => keel_vfs::LinkKind::Hardlink,
        },
    };
    file_plan(ctx, op)
}

fn plan_folder_dates(ctx: &Ctx, p: FolderDatesParams) -> Result<PlanPreview> {
    file_plan(ctx, keel_core::Op::folder_dates(real_path(ctx, &p.path)?))
}

/// `2014-05-13T16:53:20Z` (UTC) for a time in nanoseconds since the Unix epoch.
fn utc(ns: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(
        ns.div_euclid(1_000_000_000),
        ns.rem_euclid(1_000_000_000) as u32,
    )
    .map_or_else(
        || ns.to_string(),
        |t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

fn store_file_plan(ctx: &Ctx, plan: keel_core::Plan) -> Result<PlanPreview> {
    let summary = file_summary(&plan);
    let mut changes: Vec<Change> = plan.changes.iter().map(file_change).collect();
    if let keel_core::Op::FolderDates { dates, .. } = &plan.op {
        // The new time and the old one, for each folder.
        for (c, d) in changes.iter_mut().zip(dates) {
            c.detail = Some(format!("{}, was {}", utc(d.to), utc(d.was)));
            (c.files, c.bytes) = (None, None);
        }
    }
    let warnings = plan.warnings.iter().map(file_warning).collect();
    let (plan_id, input_hash, expires_at) = ctx.plans.insert(Input::Files(plan), false)?;
    Ok(PlanPreview {
        plan_id,
        input_hash,
        operation: "plan".into(),
        summary,
        changes,
        warnings,
        expires_at,
    })
}

fn file_summary(plan: &keel_core::Plan) -> String {
    if let Some(headline) = plan.headline() {
        return headline;
    }
    let files: u64 = plan.changes.iter().map(|c| c.files).sum();
    let bytes: u64 = plan.changes.iter().map(|c| c.bytes).sum();
    // The first three paths, so a summary echoed for approval says what it touches.
    let mut named: Vec<String> = plan
        .changes
        .iter()
        .take(3)
        .map(|c| c.from.display())
        .collect();
    if plan.changes.len() > 3 {
        named.push(format!("{} more", plan.changes.len() - 3));
    }
    let what = format!(
        "{} item(s) ({}), {files} file(s), {bytes} bytes",
        plan.changes.len(),
        named.join(", ")
    );
    match &plan.op {
        keel_core::Op::Copy { dst_dir, .. } => format!("Copy {what} to {}", dst_dir.display()),
        keel_core::Op::Move { dst_dir, .. } => format!("Move {what} to {}", dst_dir.display()),
        keel_core::Op::Delete { .. } => format!("Delete {what}"),
        keel_core::Op::Rename { path, new_name } => {
            format!("Rename {} to {new_name}", path.display())
        }
        // Worded by `headline`.
        _ => what,
    }
}

fn file_change(c: &keel_core::Change) -> Change {
    use keel_core::Action as A;
    let action = match c.action {
        A::Copy => "copy",
        A::Move => "move",
        A::Delete => "delete",
        A::Rename => "rename",
        A::MakeFolder => "make_folder",
        A::RemoveFolder => "remove_folder",
        A::SetAttributes => "set_attributes",
        A::Link => "link",
        A::SetFolderDate => "set_folder_date",
    };
    Change {
        action: action.into(),
        path: Some(c.from.display()),
        to: c.to.as_ref().map(VPath::display),
        detail: None,
        files: Some(c.files),
        bytes: Some(c.bytes),
    }
}

fn file_warning(w: &keel_core::Warning) -> Warning {
    use keel_core::Warning as W;
    let (kind, path, files, message) = match w {
        W::LastCopy { path, files } => (
            "last_copy",
            Some(path),
            Some(*files),
            format!(
                "{files} file(s) in {} are the last indexed copy of their content",
                path.display()
            ),
        ),
        W::SingleDomain { path, files } => (
            "single_domain",
            Some(path),
            Some(*files),
            format!(
                "{files} file(s) in {} would leave every remaining copy on one disk or account",
                path.display()
            ),
        ),
        W::CopiesOffline { path, files } => (
            "copies_offline",
            Some(path),
            Some(*files),
            format!(
                "{files} file(s) in {} would leave their other copies only on offline or archived drives",
                path.display()
            ),
        ),
        W::OfflineSource { label, .. } => (
            "offline_source",
            None,
            None,
            format!("source {label} is offline: previewed from its last index; it must be online to run"),
        ),
        W::NotIndexed { path } => (
            "not_indexed",
            Some(path),
            None,
            format!("{} is not indexed: previewed from the folder itself", path.display()),
        ),
        W::Exists { path, on_conflict } => (
            "exists",
            Some(path),
            None,
            format!("{} exists already ({on_conflict:?})", path.display()),
        ),
        W::Permanent { path } => (
            "permanent",
            Some(path),
            None,
            format!("{} is deleted for good (no trash)", path.display()),
        ),
        W::ContentUnverified { path, files } => (
            "content_unverified",
            Some(path),
            Some(*files),
            format!(
                "{files} file(s) in {} have no content id yet: whether other copies exist is unknown",
                path.display()
            ),
        ),
        W::RewritesArchive { path, bytes } => (
            "rewrites_archive",
            Some(path),
            None,
            format!(
                "rewrites the {} archive {}: entries that stay are copied as they are, then the new archive replaces the old one",
                size_text(*bytes),
                path.display()
            ),
        ),
    };
    Warning {
        kind: kind.into(),
        path: path.map(VPath::display),
        files,
        message,
    }
}

/// `1.2 GB` (decimal units, as the file lists show sizes).
fn size_text(bytes: u64) -> String {
    let units = ["bytes", "kB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < units.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} bytes")
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

fn execute(ctx: &Ctx, p: ExecuteParams) -> Result<Executed> {
    let input = ctx.plans.take(&p.plan_id, &p.input_hash)?;
    let operation = input.method().to_owned();
    match input {
        Input::Files(plan) => match plan.execute(&ctx.lib, true) {
            Ok(job) => Ok(Executed {
                plan_id: p.plan_id,
                operation,
                job: Some(job),
                result: None,
            }),
            Err(e) => match e.downcast::<keel_core::PlanChanged>() {
                Ok(keel_core::PlanChanged(fresh)) => {
                    let preview = store_file_plan(ctx, *fresh)?;
                    Err(ApiError::new(
                        ApiError::PLAN_CHANGED,
                        "the sources changed since the preview: confirm the new preview",
                    )
                    .with_data(to_value(preview)?))
                }
                Err(e) => Err(e.into()),
            },
        },
        Input::Call { method, params } => {
            let Some(Run::Previewed { apply, .. }) = crate::find(&method).map(|op| &op.run) else {
                return Err(ApiError::new(
                    ApiError::INTERNAL,
                    format!("plan for unknown operation {method}"),
                ));
            };
            let result = apply(ctx, params)?;
            Ok(Executed {
                plan_id: p.plan_id,
                operation,
                // An operation that starts a job (`JobStarted`) is followed like a file plan.
                job: result.get("job").and_then(serde_json::Value::as_i64),
                result: Some(result),
            })
        }
    }
}

// --- devices and shares ---

fn node(ctx: &Ctx) -> Result<(&Arc<keel_net::Node>, &tokio::runtime::Handle)> {
    match (&ctx.node, &ctx.rt) {
        (Some(n), Some(rt)) => Ok((n, rt)),
        _ => Err(ApiError::new(
            ApiError::NET_DISABLED,
            "devices are off: turn on Settings → Devices in Keel (or set [devices] enabled = true and explicit = true in the profile's config.toml) and restart keel-daemon",
        )),
    }
}

fn peer_info(p: &keel_net::Peer) -> PeerInfo {
    PeerInfo {
        id: p.id.0.to_string(),
        label: p.label.clone(),
        link: format!("{:?}", p.link).to_lowercase(),
        last_seen: p.last_seen,
        storage_used: p.storage.as_ref().map(|s| s.used),
        storage_total: p.storage.as_ref().map(|s| s.total),
        sync: false,
        last_sync: None,
    }
}

fn peer_id(s: &str) -> Result<keel_net::PeerId> {
    s.trim()
        .parse::<keel_net::NodeId>()
        .map(keel_net::PeerId)
        .map_err(|e| ApiError::invalid_params(format!("bad peer id: {e:#}")))
}

fn paired(ctx: &Ctx, s: &str) -> Result<keel_net::Peer> {
    let (node, _) = node(ctx)?;
    let id = peer_id(s)?;
    node.peers()
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| ApiError::not_found(format!("no paired device {s}")))
}

fn devices_list(ctx: &Ctx, _: NoParams) -> Result<Devices> {
    let (node, _) = node(ctx)?;
    let synced = ctx.lib.sync_peers()?;
    let peers = node
        .peers()
        .iter()
        .map(|p| {
            let id = p.id.0.to_string();
            PeerInfo {
                sync: node.syncs_with(&p.id),
                last_sync: synced
                    .iter()
                    .find(|s| s.device == id)
                    .and_then(|s| s.last_sync),
                ..peer_info(p)
            }
        })
        .collect();
    Ok(Devices {
        id: node.id().to_string(),
        label: node.label(),
        peers,
    })
}

fn library_sync(ctx: &Ctx, p: LibrarySyncParams) -> Result<Vec<SyncedDevice>> {
    let (node, rt) = node(ctx)?;
    let peers: Vec<keel_net::Peer> = match p.peer.as_deref() {
        Some(id) => {
            let peer = paired(ctx, id)?;
            if !node.syncs_with(&peer.id) {
                return Err(ApiError::invalid_params(format!(
                    "library sync with {} is off: turn it on in Settings → Devices (devices.settings_set sync)",
                    peer.label
                )));
            }
            vec![peer]
        }
        None => {
            let on = node.sync_peers();
            node.peers()
                .into_iter()
                .filter(|p| on.contains(&p.id))
                .collect()
        }
    };
    Ok(peers
        .into_iter()
        .map(|peer| {
            let pulled = rt.block_on(node.sync_now(&peer.id));
            SyncedDevice {
                peer: peer.id.0.to_string(),
                label: peer.label,
                applied: *pulled.as_ref().unwrap_or(&0),
                error: pulled.err().map(|e| format!("{e:#}")),
            }
        })
        .collect())
}

fn devices_settings(ctx: &Ctx, _: NoParams) -> Result<DeviceSettingsInfo> {
    let (node, _) = node(ctx)?;
    let drops = ctx.drops.as_ref();
    Ok(DeviceSettingsInfo {
        label: node.label(),
        inbox: drops.map_or_else(String::new, |d| d.inbox().display().to_string()),
        auto_accept: drops.map(|d| d.auto_accept()).unwrap_or_default(),
        relay: drops.is_none_or(|d| d.relay),
        sync: node.sync_peers().iter().map(|p| p.0.to_string()).collect(),
    })
}

/// `p` checked: its label, its inbox folder (never inside Keel's configuration folder,
/// nor its data folder but for its inbox there, nor a folder holding either), its device
/// ids parsed.
fn settings_check(ctx: &Ctx, p: &DeviceSettingsParams) -> Result<Option<std::path::PathBuf>> {
    node(ctx)?;
    if let Some(label) = p.label.as_deref().map(str::trim) {
        if !keel_net::valid_label(label) {
            return Err(ApiError::invalid_params(
                "not a device name: at most 256 bytes, no control or direction characters",
            ));
        }
    }
    if p.inbox.is_some() || p.auto_accept.is_some() {
        drops(ctx)?;
    }
    for id in p.auto_accept.iter().chain(&p.sync).flatten() {
        peer_id(id)?;
    }
    let Some(inbox) = p.inbox.as_deref().map(str::trim) else {
        return Ok(None);
    };
    let dir = std::path::PathBuf::from(inbox);
    if !dir.is_absolute() {
        return Err(ApiError::invalid_params(format!(
            "not an absolute folder: {inbox}"
        )));
    }
    use crate::files::inside;
    let kept = |d: &Option<std::path::PathBuf>| d.as_ref().is_some_and(|d| inside(&dir, d));
    let holds = |d: &Option<std::path::PathBuf>| d.as_ref().is_some_and(|d| inside(d, &dir));
    let data_inbox = ctx.data_dir.as_ref().map(|d| d.join("inbox"));
    if kept(&ctx.config_dir)
        || holds(&ctx.config_dir)
        || holds(&ctx.data_dir)
        || (kept(&ctx.data_dir) && !kept(&data_inbox))
    {
        return Err(ApiError::invalid_params(format!(
            "{inbox}: Keel's configuration and data folders cannot be the inbox"
        )));
    }
    Ok(Some(dir))
}

fn settings_set_preview(ctx: &Ctx, p: &DeviceSettingsParams) -> Result<Preview> {
    let inbox = settings_check(ctx, p)?;
    let now = devices_settings(ctx, NoParams {})?;
    let mut changes = Vec::new();
    let mut warnings = Vec::new();
    if let Some(label) = &p.label {
        changes.push(change("device.label", None, Some(label.trim().to_owned())));
    }
    if let Some(dir) = inbox {
        changes.push(change(
            "device.inbox",
            Some(dir.display().to_string()),
            None,
        ));
    }
    if let Some(ids) = &p.auto_accept {
        let detail = format!("{} device(s)", ids.len());
        changes.push(change("device.auto_accept", None, Some(detail)));
    }
    if let Some(ids) = &p.sync {
        let detail = format!("{} device(s)", ids.len());
        changes.push(change("device.sync", None, Some(detail)));
    }
    if let Some(relay) = p.relay {
        let on = if relay { "on" } else { "off" };
        changes.push(change("device.relay", None, Some(on.into())));
        if relay != now.relay {
            warnings.push(warning(
                "restart",
                None,
                format!("relays turn {on} when the host starts again (the node cannot re-bind)"),
            ));
        }
    }
    let what: Vec<&str> = changes
        .iter()
        .map(|c| &c.action["device.".len()..])
        .collect();
    Ok(Preview {
        pin: None,
        summary: match what.is_empty() {
            true => "Change no device setting".into(),
            false => format!("Change this device's {}", what.join(", ")),
        },
        changes,
        warnings,
    })
}

fn settings_set(ctx: &Ctx, p: DeviceSettingsParams) -> Result<DeviceSettingsSet> {
    let inbox = settings_check(ctx, &p)?;
    let (node, _) = node(ctx)?;
    if let Some(label) = &p.label {
        node.try_set_label(label.trim())
            .map_err(|e| ApiError::invalid_params(format!("{e:#}")))?;
    }
    if let Some(dir) = inbox {
        drops(ctx)?.set_inbox(dir);
    }
    if let Some(ids) = p.auto_accept {
        drops(ctx)?.set_auto_accept(ids.iter().map(|i| i.trim().to_owned()).collect());
    }
    if let Some(ids) = &p.sync {
        node.set_sync_peers(ids.iter().filter_map(|i| peer_id(i).ok()));
    }
    let settings = devices_settings(ctx, NoParams {})?;
    Ok(DeviceSettingsSet {
        restart: p.relay.is_some_and(|r| r != settings.relay),
        settings,
    })
}

fn pair_code_preview(ctx: &Ctx, _: &NoParams) -> Result<Preview> {
    node(ctx)?;
    Ok(Preview {
        pin: None,
        summary: "Create a one-time pairing code, valid 10 minutes".into(),
        changes: vec![change("device.pair_code", None, None)],
        warnings: vec![warning(
            "secret",
            None,
            "whoever has the code can pair with this device: share it only with the other device"
                .into(),
        )],
    })
}

fn pair_code(ctx: &Ctx, _: NoParams) -> Result<PairCodeInfo> {
    let (node, rt) = node(ctx)?;
    let code = rt.block_on(node.pair_code())?;
    Ok(PairCodeInfo {
        code: code.to_string(),
        ticket: code.ticket(),
        expires_at: keel_now() + 600,
    })
}

fn keel_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn pair_with_preview(ctx: &Ctx, p: &PairWithParams) -> Result<Preview> {
    node(ctx)?;
    p.code
        .trim()
        .parse::<keel_net::PairCode>()
        .map_err(|e| ApiError::invalid_params(format!("bad pairing code: {e:#}")))?;
    Ok(Preview {
        pin: None,
        summary: "Pair with the device that showed this code (no access is granted)".into(),
        changes: vec![change("device.pair", None, None)],
        warnings: Vec::new(),
    })
}

fn pair_with(ctx: &Ctx, p: PairWithParams) -> Result<PeerInfo> {
    let (node, rt) = node(ctx)?;
    let code = p
        .code
        .trim()
        .parse::<keel_net::PairCode>()
        .map_err(|e| ApiError::invalid_params(format!("bad pairing code: {e:#}")))?;
    Ok(peer_info(&rt.block_on(node.pair_with(&code))?))
}

fn forget_preview(ctx: &Ctx, p: &PeerParams) -> Result<Preview> {
    let peer = paired(ctx, &p.peer)?;
    let (node, _) = node(ctx)?;
    let grants = node.grants().iter().filter(|g| g.peer == peer.id).count();
    Ok(Preview {
        pin: None,
        summary: format!("Forget device {} and its {grants} grant(s)", peer.label),
        changes: vec![change(
            "device.forget",
            None,
            Some(format!("{} ({})", peer.label, p.peer.trim())),
        )],
        warnings: Vec::new(),
    })
}

fn forget(ctx: &Ctx, p: PeerParams) -> Result<Done> {
    let peer = paired(ctx, &p.peer)?;
    node(ctx)?.0.forget_peer(&peer.id)?;
    Ok(Done { ok: true })
}

fn grant_info(g: &keel_net::Grant) -> GrantInfo {
    GrantInfo {
        peer: g.peer.0.to_string(),
        source: g.source.clone(),
        subtree: g.subtree.clone(),
        access: match g.access {
            keel_net::Access::Read => Access::Read,
            keel_net::Access::ReadWrite => Access::ReadWrite,
        },
        created: g.created,
    }
}

fn shares_list(ctx: &Ctx, _: NoParams) -> Result<Vec<GrantInfo>> {
    Ok(node(ctx)?.0.grants().iter().map(grant_info).collect())
}

fn grant_preview(ctx: &Ctx, p: &GrantParams) -> Result<Preview> {
    let peer = paired(ctx, &p.peer)?;
    let source = find_source(ctx, &p.source)?;
    let what = match p.subtree.trim_matches('/') {
        "" => source.label.clone(),
        sub => format!("{}/{sub}", source.label),
    };
    let access = match p.access {
        Access::Read => "read",
        Access::ReadWrite => "read-write",
    };
    let mut warnings = Vec::new();
    if p.access == Access::ReadWrite {
        warnings.push(warning(
            "read_write",
            None,
            format!(
                "{} will be able to change and delete files in {what}",
                peer.label
            ),
        ));
        // Device writes reach SFTP and cloud sources too, through their provider.
        if ctx
            .router
            .provider_for(&source.root)
            .is_some_and(|p| p.remove_kind() == keel_vfs::RemoveKind::Permanent)
        {
            warnings.push(warning(
                "deletes_permanent",
                Some(source.root.display()),
                format!(
                    "files {} deletes in {what} are gone for good (no trash)",
                    peer.label
                ),
            ));
        }
    }
    Ok(Preview {
        pin: None,
        summary: format!("Give {} {access} access to {what}", peer.label),
        changes: vec![Change {
            action: "share.grant".into(),
            path: Some(source.root.display()),
            to: Some(peer.label.clone()),
            detail: Some(format!("{access}: {what}")),
            files: None,
            bytes: None,
        }],
        warnings,
    })
}

fn grant(ctx: &Ctx, p: GrantParams) -> Result<GrantInfo> {
    let peer = paired(ctx, &p.peer)?;
    find_source(ctx, &p.source)?;
    let g = keel_net::Grant {
        peer: peer.id,
        source: p.source,
        subtree: p.subtree.trim_matches('/').to_owned(),
        access: match p.access {
            Access::Read => keel_net::Access::Read,
            Access::ReadWrite => keel_net::Access::ReadWrite,
        },
        created: keel_now(),
    };
    node(ctx)?.0.grant(g.clone())?;
    Ok(grant_info(&g))
}

fn revoke(ctx: &Ctx, p: RevokeParams) -> Result<Revoked> {
    let (node, _) = node(ctx)?;
    let peer = peer_id(&p.peer)?;
    let subtree = p.subtree.trim_matches('/').to_owned();
    let existed = node
        .grants()
        .iter()
        .any(|g| g.peer == peer && g.source == p.source && g.subtree == subtree);
    node.revoke(&peer, &p.source, &subtree)?;
    Ok(Revoked {
        peer: p.peer.trim().to_owned(),
        source: p.source,
        subtree,
        existed,
    })
}

// --- mounts ---

fn mount_info(m: keel_mount::MountInfo) -> MountInfo {
    MountInfo {
        target: m.target,
        source: m.source,
        source_label: m.label,
        subtree: m.subtree,
        root: m.root,
        backend: m.backend.to_owned(),
    }
}

/// The host's mounts, when it serves them and was built with a backend.
fn mounts(ctx: &Ctx) -> Result<&Arc<keel_mount::Mounts>> {
    let m = ctx.mounts.as_ref().ok_or_else(|| {
        ApiError::new(
            ApiError::MOUNTS_UNAVAILABLE,
            "mounts are served by keel-daemon: start it with `keel daemon start`",
        )
    })?;
    if keel_mount::backend().is_none() {
        return Err(ApiError::new(
            ApiError::MOUNTS_UNAVAILABLE,
            keel_mount::NO_BACKEND,
        ));
    }
    // Built in, but its driver (WinFsp, fuse3, macFUSE) is not installed.
    if let Some(why) = keel_mount::driver_missing() {
        return Err(ApiError::new(ApiError::MOUNTS_UNAVAILABLE, why));
    }
    Ok(m)
}

fn mounts_list(ctx: &Ctx, _: NoParams) -> Result<Vec<MountInfo>> {
    Ok(ctx
        .mounts
        .as_ref()
        .map(|m| m.list())
        .unwrap_or_default()
        .into_iter()
        .map(mount_info)
        .collect())
}

fn mounts_add_preview(ctx: &Ctx, p: &MountParams) -> Result<Preview> {
    let m = mounts(ctx)?;
    let s = find_source(ctx, &p.source)?;
    let (target, fs) = m
        .check(
            &ctx.lib,
            &ctx.router,
            &SourceId(p.source.clone()),
            &p.subtree,
            &p.target,
        )
        .map_err(|e| ApiError::invalid_params(format!("{e:#}")))?;
    let root = fs.map().root.display();
    let mut warnings = Vec::new();
    if matches!(s.status, SourceStatus::Offline { .. }) {
        warnings.push(warning(
            "source_offline",
            Some(root.clone()),
            format!(
                "{} is offline: the mount lists it from the library index and cannot open or change files until it is back",
                s.label
            ),
        ));
    }
    if ctx
        .router
        .provider_for(&fs.map().root)
        .is_some_and(|p| p.remove_kind() == keel_vfs::RemoveKind::Permanent)
    {
        warnings.push(warning(
            "deletes_permanent",
            Some(root.clone()),
            "files deleted through the mount are deleted permanently on this source".into(),
        ));
    }
    let what = match fs.map().subtree.as_str() {
        "" => s.label.clone(),
        sub => format!("{}/{sub}", s.label),
    };
    Ok(Preview {
        pin: None,
        summary: format!("Mount {what} at {target}"),
        changes: vec![change("mount.add", Some(root), Some(target))],
        warnings,
    })
}

fn mounts_add(ctx: &Ctx, p: MountParams) -> Result<MountInfo> {
    let m = mounts(ctx)?;
    find_source(ctx, &p.source)?;
    Ok(mount_info(m.add(
        &ctx.lib,
        &ctx.router,
        &SourceId(p.source),
        &p.subtree,
        &p.target,
    )?))
}

fn mounted(ctx: &Ctx, target: &str) -> Result<(Arc<keel_mount::Mounts>, keel_mount::MountInfo)> {
    let m = ctx
        .mounts
        .clone()
        .ok_or_else(|| ApiError::not_found(format!("{target} is not a Keel mount")))?;
    let info = m
        .get(target)
        .ok_or_else(|| ApiError::not_found(format!("{target} is not a Keel mount")))?;
    Ok((m, info))
}

fn mounts_remove_preview(ctx: &Ctx, p: &UnmountParams) -> Result<Preview> {
    let (m, info) = mounted(ctx, &p.target)?;
    let mut warnings = Vec::new();
    let pending = m.pending_writes(&info.target);
    if pending > 0 {
        warnings.push(warning(
            "discards_writes",
            Some(info.target.clone()),
            format!(
                "{pending} file(s) are being written through the mount: unmounting discards those unsaved changes (the files stay as they were)"
            ),
        ));
    }
    Ok(Preview {
        pin: None,
        summary: format!("Unmount {} ({})", info.target, info.root),
        changes: vec![change("mount.remove", Some(info.root), Some(info.target))],
        warnings,
    })
}

fn mounts_remove(ctx: &Ctx, p: UnmountParams) -> Result<MountInfo> {
    let (m, info) = mounted(ctx, &p.target)?;
    Ok(mount_info(m.remove(&info.target)?))
}

// --- Spacedrop ---

/// Files a `spacedrop.send` preview lists one by one (its summary counts them all).
const DROP_LISTED: usize = 500;
/// Entries `spacedrop.inbox` returns.
const INBOX_MAX: usize = 500;

/// Where a drop may read (a drop must never carry the daemon token, device keys or the
/// library database to another device): never Keel's configuration folder, never its data
/// folder except the Spacedrop inbox and share uploads the signed-in client opened
/// (`share.claim`), and otherwise only what a library source holds. Folders resolved once.
struct DropScope {
    config: Option<std::path::PathBuf>,
    data: Option<std::path::PathBuf>,
    /// The inbox and the opened shares.
    open: Vec<std::path::PathBuf>,
}

impl DropScope {
    fn new(ctx: &Ctx) -> Self {
        use crate::files::resolved;
        let mut open: Vec<_> = ctx
            .drops
            .iter()
            .filter_map(|d| resolved(&d.inbox()))
            .collect();
        if let Some(data) = &ctx.data_dir {
            let shares = std::fs::read_dir(data.join("shares")).into_iter().flatten();
            open.extend(
                shares
                    .flatten()
                    .map(|e| e.path())
                    .filter(|d| d.join(crate::SHARE_CLAIMED).is_file())
                    .filter_map(|d| resolved(&d)),
            );
        }
        Self {
            config: ctx.config_dir.as_deref().and_then(resolved),
            data: ctx.data_dir.as_deref().and_then(resolved),
            open,
        }
    }

    /// Checks one path given or reached. `top`: given (it must not hold a closed folder).
    fn check(&self, ctx: &Ctx, path: &VPath, top: bool) -> Result<()> {
        let refuse = |why: &str| Err(ApiError::failed(format!("{}: {why}", path.display())));
        if let Some(real) = path
            .to_local_path()
            .and_then(|l| crate::files::resolved(&l))
        {
            let closed = [&self.config, &self.data];
            if top
                && closed
                    .iter()
                    .any(|c| c.as_ref().is_some_and(|c| c.starts_with(&real)))
            {
                return refuse("holds Keel's configuration or data folder, which is never sent");
            }
            if self.config.as_ref().is_some_and(|c| real.starts_with(c)) {
                return refuse("Keel's configuration folder is never sent");
            }
            if self.open.iter().any(|o| real.starts_with(o)) {
                return Ok(());
            }
            if self.data.as_ref().is_some_and(|d| real.starts_with(d)) {
                return refuse("Keel's data folder is never sent");
            }
        }
        if ctx.lib.source_for(path).is_none() {
            return refuse(
                "only files in a library source, the Spacedrop inbox or an opened share can be sent",
            );
        }
        Ok(())
    }
}

/// The files `p` sends, every one checked against [`DropScope`] (links inside folders are
/// skipped by the walk), and the hash a preview pins.
fn drop_files(
    ctx: &Ctx,
    p: &SpacedropSendParams,
) -> Result<(Vec<keel_net::spacedrop::DropFile>, String)> {
    if p.paths.is_empty() {
        return Err(ApiError::invalid_params("nothing to send"));
    }
    let scope = DropScope::new(ctx);
    let paths = p
        .paths
        .iter()
        .map(|s| {
            let path = crate::files::real(ctx, &vpath(s)?)?;
            crate::files::readable(ctx, &path)?;
            scope.check(ctx, &path, true)?;
            Ok(path)
        })
        .collect::<Result<Vec<_>>>()?;
    let files = keel_net::spacedrop::files(&ctx.router, &paths)?;
    for f in &files {
        scope.check(ctx, &f.src, false)?;
    }
    let bytes = serde_json::to_vec(&files).map_err(|e| ApiError::failed(e.to_string()))?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    Ok((files, hash))
}

fn sum(files: &[(String, u64)]) -> u64 {
    files.iter().fold(0u64, |n, f| n.saturating_add(f.1))
}

fn drop_send_preview(ctx: &Ctx, p: &SpacedropSendParams) -> Result<Preview> {
    let peer = paired(ctx, &p.peer)?;
    let (files, hash) = drop_files(ctx, p)?;
    let mut changes: Vec<Change> = files
        .iter()
        .take(DROP_LISTED)
        .map(|f| Change {
            action: "drop.send".into(),
            path: Some(f.rel.clone()),
            to: Some(peer.label.clone()),
            detail: None,
            files: Some(1),
            bytes: Some(f.size),
        })
        .collect();
    if files.len() > DROP_LISTED {
        changes.push(change(
            "drop.send",
            None,
            Some(format!("and {} more file(s)", files.len() - DROP_LISTED)),
        ));
    }
    let mut warnings = Vec::new();
    if peer.link == keel_net::Link::Offline {
        warnings.push(warning(
            "offline",
            None,
            format!(
                "{} is offline: the drop waits for it and fails after 10 minutes without progress",
                peer.label
            ),
        ));
    }
    let bytes = files.iter().fold(0u64, |n, f| n.saturating_add(f.size));
    Ok(Preview {
        pin: Some(json!(hash)),
        summary: format!(
            "Send {} file(s), {bytes} bytes, to {}",
            files.len(),
            peer.label
        ),
        changes,
        warnings,
    })
}

/// Sends exactly the files the preview listed: the folders are walked again and a
/// different list (a file added, removed or resized since) is refused with PLAN_CHANGED
/// and a fresh preview.
fn drop_send(ctx: &Ctx, p: SpacedropSendParams) -> Result<JobStarted> {
    let peer = paired(ctx, &p.peer)?;
    let (files, hash) = drop_files(ctx, &p)?;
    if p.pinned.as_deref() != Some(hash.as_str()) {
        let fresh = crate::call(
            ctx,
            "spacedrop.send",
            json!({"peer": p.peer, "paths": p.paths}),
        )?;
        return Err(ApiError::new(
            ApiError::PLAN_CHANGED,
            "the files changed since the preview: confirm the new preview",
        )
        .with_data(fresh));
    }
    let (node, _) = node(ctx)?;
    let job = keel_net::spacedrop::send_files(node, &ctx.lib, peer.id, files)?;
    Ok(JobStarted { job })
}

fn drops(ctx: &Ctx) -> Result<&Arc<crate::net::Drops>> {
    node(ctx)?;
    ctx.drops
        .as_ref()
        .ok_or_else(|| ApiError::failed("this host takes no Spacedrops"))
}

fn drop_inbox(ctx: &Ctx, _: NoParams) -> Result<Inbox> {
    let drops = drops(ctx)?;
    let pending = drops
        .pending()
        .into_iter()
        .map(|w| DropOffer {
            peer: w.peer.0.to_string(),
            label: w.label,
            id: w.id,
            files: w.files.len() as u64,
            bytes: sum(&w.files),
            names: w.files.into_iter().take(10).map(|f| f.0).collect(),
        })
        .collect();
    let inbox = drops.inbox();
    let mut entries: Vec<EntryInfo> = match std::fs::read_dir(&inbox) {
        Ok(dir) => dir
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                // Staging folders of drops still arriving.
                if name.starts_with(".keel-partial-") {
                    return None;
                }
                let meta = e.metadata().ok()?;
                Some(EntryInfo {
                    name,
                    path: e.path().display().to_string(),
                    is_dir: meta.is_dir(),
                    size: if meta.is_dir() { 0 } else { meta.len() },
                    modified: unix(meta.modified().ok()),
                    hidden: false,
                })
            })
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(ApiError::failed(format!("{}: {e}", inbox.display()))),
    };
    entries.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.name.cmp(&b.name)));
    entries.truncate(INBOX_MAX);
    Ok(Inbox {
        dir: inbox.display().to_string(),
        pending,
        entries,
    })
}

/// The waiting offer `p` names: its sender's label and files.
fn waiting_offer(ctx: &Ctx, p: &SpacedropAnswerParams) -> Result<(String, Vec<(String, u64)>)> {
    let peer = peer_id(&p.peer)?;
    drops(ctx)?
        .pending()
        .into_iter()
        .find(|w| w.peer == peer && w.id == p.id)
        .map(|w| (w.label, w.files))
        .ok_or_else(|| ApiError::not_found(format!("no waiting offer {}", p.id)))
}

fn drop_answer_preview(ctx: &Ctx, p: &SpacedropAnswerParams) -> Result<Preview> {
    let (label, files) = waiting_offer(ctx, p)?;
    let inbox = drops(ctx)?.inbox().display().to_string();
    let (verb, into, action) = match p.accept {
        true => ("Accept", format!(" into {inbox}"), "drop.accept"),
        false => ("Decline", String::new(), "drop.decline"),
    };
    Ok(Preview {
        pin: None,
        summary: format!(
            "{verb} {} file(s), {} bytes, from {label}{into}",
            files.len(),
            sum(&files)
        ),
        changes: files
            .iter()
            .take(DROP_LISTED)
            .map(|(rel, size)| Change {
                action: action.into(),
                path: Some(rel.clone()),
                to: p.accept.then(|| inbox.clone()),
                detail: None,
                files: Some(1),
                bytes: Some(*size),
            })
            .collect(),
        warnings: Vec::new(),
    })
}

fn drop_answer(ctx: &Ctx, p: SpacedropAnswerParams) -> Result<Done> {
    let peer = peer_id(&p.peer)?;
    if !drops(ctx)?.answer(&peer, &p.id, p.accept) {
        return Err(ApiError::not_found(format!(
            "no waiting offer {} (answered, or the sender stopped waiting)",
            p.id
        )));
    }
    Ok(Done { ok: true })
}
