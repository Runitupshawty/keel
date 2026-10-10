//! Parameter and result types of the registered operations. Paths are strings: an
//! absolute local path (`D:\x`, `/home/x`, an archive path `D:\x.zip!/a.txt`) or a VPath
//! URI (`sftp://host/x`, `library://<source id>/<rel>`). Results show paths the same way.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// No parameters.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoParams {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VersionInfo {
    pub name: String,
    pub version: String,
    /// The API revision (bumped on incompatible changes).
    pub api: u32,
    /// The open library.
    pub library: String,
    /// Whether devices and shares (keel-net) are available.
    pub net: bool,
    /// The process hosting the library (the daemon, or the CLI itself in-process).
    pub pid: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceKindName {
    Folder,
    Drive,
    Share,
    Cloud,
    Device,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SourceInfo {
    pub id: String,
    pub label: String,
    pub root: String,
    pub kind: SourceKindName,
    /// `online`, `indexing`, `offline` or `error`.
    pub status: String,
    /// Online: when the last full walk ended (unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed_at: Option<i64>,
    /// Offline: when it was last reachable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<i64>,
    /// Offline: why; error: the message; indexing: progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddSourceParams {
    /// The folder, drive or share to index (absolute).
    pub root: String,
    /// Shown name; the folder name when absent.
    #[serde(default)]
    pub label: Option<String>,
    /// `folder` when absent.
    #[serde(default)]
    pub kind: Option<SourceKindName>,
    #[serde(default)]
    pub include_hidden: bool,
    /// gitignore-style patterns relative to the root.
    #[serde(default)]
    pub ignore: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceIdParams {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoveSourceParams {
    pub id: String,
    /// Also delete the index store of the source; the files themselves are never touched.
    #[serde(default)]
    pub delete_store: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemovedSource {
    pub removed: SourceInfo,
    pub store_deleted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AddedSource {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct JobStarted {
    pub job: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathParams {
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    /// A folder: a library path (`library://<source>/<rel>`, from the index, works offline)
    /// or any path the VFS router reaches.
    pub path: String,
    /// At most this many entries (default 1000).
    #[serde(default)]
    pub max: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EntryInfo {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<i64>,
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Listing {
    pub entries: Vec<EntryInfo>,
    /// More entries exist than `max`.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct IndexedInfo {
    pub source: String,
    pub source_label: String,
    pub record: i64,
    pub tags: Vec<String>,
    pub favorite: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StatInfo {
    pub entry: EntryInfo,
    /// The library record, when the path is in an indexed source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed: Option<IndexedInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchParams {
    /// Library query: words, "phrases", `kind:`, `ext:`, `size:`, `dm:`, `source:`, `tag:`.
    pub query: String,
    /// At most this many hits (default 100).
    #[serde(default)]
    pub max: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Hit {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<i64>,
    pub source: String,
    pub source_label: String,
    pub record: i64,
    pub score: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TagsListParams {
    /// Only the tags on this indexed path.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TagInfo {
    pub id: i64,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TagParams {
    /// A tag name; `tags.add` creates it when missing.
    pub tag: String,
    /// Indexed paths.
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TagsSetParams {
    /// Indexed paths.
    pub paths: Vec<String>,
    /// Exactly these tags end up on each path (missing tags are created; Favorites stays).
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FavoritesSetParams {
    /// Indexed paths.
    pub paths: Vec<String>,
    /// false removes them from Favorites (default true).
    #[serde(default = "yes")]
    pub on: bool,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Tagged {
    pub records: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LimitParams {
    /// Default 50.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobParams {
    pub id: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct JobInfo {
    pub id: i64,
    pub kind: String,
    /// `queued`, `running`, `done`, `failed` or `cancelled`.
    pub status: String,
    /// 0..=1.
    pub progress: f32,
    pub created: i64,
    pub updated: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// `jobs.info` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
}

impl JobInfo {
    /// Done, failed or cancelled.
    pub fn finished(&self) -> bool {
        matches!(self.status.as_str(), "done" | "failed" | "cancelled")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DuplicatesParams {
    /// Ignore files smaller than this (default 1).
    #[serde(default)]
    pub min_size: Option<u64>,
    /// At most this many groups (default 100), most bytes wasted first.
    #[serde(default)]
    pub max: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DupGroup {
    /// BLAKE3 content id, hex.
    pub content_id: String,
    /// Bytes per copy.
    pub size: u64,
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Location {
    /// The copy's path.
    pub path: String,
    /// The source holding it.
    pub source_label: String,
    /// The volume it sits on and that volume's failure domain (disk serial, server or
    /// cloud account).
    pub volume: String,
    pub failure_domain: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Copies {
    /// Files holding this content on volumes that count (hard links once; at least 1).
    pub copies: u64,
    /// Distinct failure domains among those copies.
    pub failure_domains: u64,
    /// A copy is on a backup volume in a second failure domain.
    pub backed_up: bool,
    /// Copies on offline or archived volumes.
    pub offline_copies: u64,
    /// Every record holding it (lost and retired volumes included).
    pub locations: Vec<Location>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileOpKind {
    Copy,
    Move,
    Delete,
    Rename,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnConflict {
    Skip,
    Overwrite,
    /// Keep both: the new one gets a free name.
    RenameNew,
}

/// A file operation to preview.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanParams {
    pub op: FileOpKind,
    /// copy / move / delete: the items; rename: exactly one.
    pub paths: Vec<String>,
    /// copy / move: the destination folder.
    #[serde(default)]
    pub to: Option<String>,
    /// rename: the new name.
    #[serde(default)]
    pub new_name: Option<String>,
    /// copy / move: what to do with names that exist (default skip).
    #[serde(default)]
    pub on_conflict: Option<OnConflict>,
}

/// One projected change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Change {
    /// `copy`, `move`, `delete`, `rename`, or the operation's own (`tag.add`, `source.add`…).
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Files (not folders) affected and their total size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Warning {
    /// `last_copy`, `single_domain`, `copies_offline`, `offline_source`, `not_indexed`, `exists`,
    /// `permanent`, `content_unverified`, `creates_tag`, `secret`…
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<u64>,
    pub message: String,
}

/// What a mutating operation would do. Nothing has happened yet: `execute` with
/// `plan_id` and `input_hash` applies exactly this input.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlanPreview {
    pub plan_id: String,
    /// BLAKE3 of the canonical input, hex: pass it back to `execute` unchanged.
    pub input_hash: String,
    /// The operation the plan runs (`plan` for file operations).
    pub operation: String,
    pub summary: String,
    pub changes: Vec<Change>,
    pub warnings: Vec<Warning>,
    /// Unix seconds; `execute` refuses the plan after this.
    pub expires_at: i64,
}

impl PlanPreview {
    /// The summary, then up to `max_changes` changes (one per line) and every warning: what
    /// a person confirms.
    pub fn describe(&self, max_changes: usize) -> String {
        let mut text = format!("{}\n", self.summary);
        for c in self.changes.iter().take(max_changes) {
            text.push_str(&format!(
                "  {:<8} {}",
                c.action,
                c.path.as_deref().unwrap_or("")
            ));
            if let Some(to) = &c.to {
                text.push_str(&format!(" -> {to}"));
            }
            if let (Some(files), Some(bytes)) = (c.files, c.bytes) {
                text.push_str(&format!("  ({files} file(s), {bytes} bytes)"));
            }
            if let Some(d) = &c.detail {
                text.push_str(&format!("  [{d}]"));
            }
            text.push('\n');
        }
        if self.changes.len() > max_changes {
            let more = self.changes.len() - max_changes;
            text.push_str(&format!("  ... and {more} more\n"));
        }
        if !self.warnings.is_empty() {
            text.push_str("Warnings:\n");
            for w in &self.warnings {
                text.push_str(&format!("  ! {}\n", w.message));
            }
        }
        text
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecuteParams {
    pub plan_id: String,
    /// The `input_hash` of the preview being confirmed.
    pub input_hash: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Executed {
    pub plan_id: String,
    pub operation: String,
    /// Set when the operation runs as a job (file operations, `sources.index`,
    /// `spacedrop.send`): follow it with `jobs.info`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<i64>,
    /// Other operations: their result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PeerInfo {
    pub id: String,
    pub label: String,
    /// `lan`, `relay` or `offline`.
    pub link: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_total: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Devices {
    /// This device.
    pub id: String,
    pub label: String,
    pub peers: Vec<PeerInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PairCodeInfo {
    /// Short code to type on the other device. A bearer secret: share it only with it.
    pub code: String,
    /// Full ticket (for a QR code; works without public address lookup).
    pub ticket: String,
    /// Unix seconds.
    pub expires_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PairWithParams {
    /// The code (or ticket) shown on the other device.
    pub code: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PeerParams {
    pub peer: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Read,
    ReadWrite,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GrantInfo {
    pub peer: String,
    pub source: String,
    /// Slash-separated, relative to the source; "" = all of it.
    pub subtree: String,
    pub access: Access,
    pub created: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrantParams {
    pub peer: String,
    /// A library source id.
    pub source: String,
    /// Slash-separated, relative to the source; "" (default) = all of it.
    #[serde(default)]
    pub subtree: String,
    pub access: Access,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RevokeParams {
    pub peer: String,
    pub source: String,
    #[serde(default)]
    pub subtree: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Revoked {
    pub peer: String,
    pub source: String,
    pub subtree: String,
    /// Whether a grant matched (revoking a missing grant is not an error).
    pub existed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Done {
    pub ok: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadParams {
    pub path: String,
    /// First byte to read.
    #[serde(default)]
    pub offset: u64,
    /// Bytes to read (default and most: 4 MiB).
    #[serde(default)]
    pub len: Option<u64>,
}

/// A byte range of a file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Chunk {
    pub offset: u64,
    /// The bytes, base64 (standard alphabet, padded).
    pub data: String,
    /// Nothing follows this range.
    pub eof: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenderParams {
    pub path: String,
    /// PDF page, from 0.
    #[serde(default)]
    pub page: u32,
    /// Longest side of an image (PDF: page width), 64..=2048 (default 1024).
    #[serde(default)]
    pub max_px: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RenderKind {
    /// `text` holds it (code, Markdown, tables as tab-separated rows, documents, hex).
    Text,
    /// `png` holds it (images, a PDF page, a video frame).
    Image,
    /// Nothing to show; `message` says why (too large, unsupported, a missing helper).
    None,
}

/// A preview rendered by the host, exactly as the desktop app's preview panel would.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Rendered {
    pub kind: RenderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// PNG, base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub png: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// PDF page count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<u32>,
    /// The text was cut short.
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Hex BLAKE3 content id when the library knows it (a cache key for clients).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThumbSize {
    /// Fits 256 px.
    #[default]
    Thumb256,
    /// Fits 1024 px.
    Thumb1024,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThumbParams {
    /// An image or video.
    pub path: String,
    #[serde(default)]
    pub size: ThumbSize,
}

/// A media thumbnail from the sidecar store (made on first request).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Thumb {
    /// `image/webp`.
    pub mime: String,
    /// The image, base64.
    pub data: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
}

/// A one-time download link.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FileLink {
    /// `/file/<token>` on keel-daemon's `--web` address: works once, until `expires_at`.
    pub url: String,
    pub name: String,
    pub size: u64,
    /// Unix seconds.
    pub expires_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MountParams {
    /// A library source id.
    pub source: String,
    /// Slash-separated, relative to the source; "" (default) = all of it.
    #[serde(default)]
    pub subtree: String,
    /// A drive letter (`K:`, Windows) or an absolute folder (Windows: one that does not
    /// exist yet; Linux and macOS: an existing empty folder).
    pub target: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnmountParams {
    /// The mount's drive letter or folder, as `mounts.list` shows it.
    pub target: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MountInfo {
    /// `K:` or the mount folder.
    pub target: String,
    pub source: String,
    pub source_label: String,
    /// Relative to the source; "" = all of it.
    pub subtree: String,
    /// What the mount shows: the source root joined with the subtree.
    pub root: String,
    /// `winfsp` or `fuse`.
    pub backend: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpacedropSendParams {
    /// A paired device's id (`devices.list`).
    pub peer: String,
    /// Files or folders on this machine (folders are sent with their contents).
    pub paths: Vec<String>,
    /// Set by the preview (the hash of the files it listed), never by a caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub pinned: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpacedropAnswerParams {
    /// The offering device's id.
    pub peer: String,
    /// The offer's id (`spacedrop.inbox`).
    pub id: String,
    pub accept: bool,
}

/// A Spacedrop offer waiting for this device's answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DropOffer {
    pub peer: String,
    /// The sending device's label.
    pub label: String,
    pub id: String,
    pub files: u64,
    pub bytes: u64,
    /// The first few relative names.
    pub names: Vec<String>,
}

/// The Spacedrop inbox of the host's device.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Inbox {
    /// The inbox folder on the host's machine.
    pub dir: String,
    /// Offers waiting for an answer (`spacedrop.answer`).
    pub pending: Vec<DropOffer>,
    /// What arrived, newest first (download with `file.get`).
    pub entries: Vec<EntryInfo>,
}
