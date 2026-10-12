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
    /// Library query: words, "phrases", `kind:`, `ext:`, `size:`, `dm:`, `source:`, `tag:`,
    /// `content:`, `-content:`, `name:`.
    pub query: String,
    /// At most this many hits (default 100).
    #[serde(default)]
    pub max: Option<usize>,
    /// Words also match the text inside documents (default true; false: names only, as
    /// `contents:off` in the query).
    #[serde(default)]
    pub contents: Option<bool>,
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
    /// A hit found by the text inside the document: at most 160 characters of it around
    /// the words (the first 100 such hits only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<Snippet>,
}

/// A line of a document's text; `marks` are the byte ranges (UTF-8) of the words that
/// matched.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Snippet {
    pub text: String,
    #[serde(default)]
    pub marks: Vec<[usize; 2]>,
}

/// `system.search`'s orders (Everything's HTTP `sort=` values).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SystemSort {
    #[default]
    Name,
    Path,
    DateModified,
    Size,
}

/// `system.search`: the platform's file search (the window's search pane backend), with
/// Everything's match options. Every switch is off by default.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SystemSearchParams {
    /// Words (all must match), "quoted phrases", wildcards (`*.pdf` matches the whole
    /// name); with `regex`, one regular expression. Other syntax (`ext:`, `!`, `|`) goes
    /// to the backend as it is. At most 4096 bytes.
    pub query: String,
    /// Results skipped (default 0).
    #[serde(default)]
    pub offset: Option<usize>,
    /// Results returned (default 32, at most 1000; 0 returns only the total).
    #[serde(default)]
    pub count: Option<usize>,
    #[serde(default)]
    pub match_case: bool,
    /// A word matches only whole words of the name (both `prefix` and `suffix`).
    #[serde(default)]
    pub whole_word: bool,
    /// Words match the full path, not only the name (a word with a `\` or `/` always does).
    #[serde(default)]
    pub match_path: bool,
    /// `é` does not match `e` (off: accents of Latin letters are ignored).
    #[serde(default)]
    pub match_diacritics: bool,
    #[serde(default)]
    pub regex: bool,
    /// A word matches only at the start of a word of the name.
    #[serde(default)]
    pub prefix: bool,
    /// A word matches only at the end of a word of the name.
    #[serde(default)]
    pub suffix: bool,
    /// Punctuation in names and words is skipped (`foobar` finds `foo-bar.txt`).
    #[serde(default)]
    pub ignore_punctuation: bool,
    /// White space in names and words is skipped.
    #[serde(default)]
    pub ignore_whitespace: bool,
    /// Default `name`.
    #[serde(default)]
    pub sort: Option<SystemSort>,
    /// Default true.
    #[serde(default)]
    pub ascending: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SystemHit {
    /// The full path.
    pub path: String,
    pub name: String,
    /// The folder it is in.
    pub parent: String,
    pub is_dir: bool,
    /// Bytes (0 for a folder).
    pub size: u64,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SystemSearchResult {
    /// The backend that answered ("Everything", "Keel index", ...).
    pub backend: String,
    /// Results matching (counted over the first 10000 the backend finds).
    pub total: usize,
    /// The backend found 10000 or more: `total` and the order cover those only.
    pub capped: bool,
    pub offset: usize,
    pub hits: Vec<SystemHit>,
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
    /// Indexed paths (`tags.add`: none only creates the tag).
    pub paths: Vec<String>,
    /// `tags.add`: the color of a tag it creates (`#3b82f6`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
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
pub struct FileNamesParams {
    /// At most this many names (default and largest: 100000).
    #[serde(default)]
    pub max: Option<usize>,
}

/// Indexed files of the online sources, each source's newest first (`files.names`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FileNames {
    /// Full paths.
    pub paths: Vec<String>,
    /// There were more than `max`.
    pub truncated: bool,
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
    /// The volume's state (`online`, `offline`, `archived`, `lost`, `retired`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<VolumeStateName>,
    /// The volume is marked as a backup.
    #[serde(default)]
    pub backup: bool,
    /// A device's word that it holds the content: never counted as a copy.
    #[serde(default)]
    pub claimed: bool,
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

/// `plan.merge`: everything in the folders `paths` (but `into`) moves into `into`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeParams {
    /// The folders to merge; `into` may be one of them.
    pub paths: Vec<String>,
    /// The folder they merge into.
    pub into: String,
    /// What to do with names that exist in `into` (default skip).
    #[serde(default)]
    pub on_conflict: Option<OnConflict>,
}

/// `plan.flatten`: every file in the folder's subfolders moves up into it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FlattenParams {
    pub path: String,
    /// What to do with names taken in the folder, or by an earlier file (default skip).
    #[serde(default)]
    pub on_conflict: Option<OnConflict>,
}

/// `plan.remove_empty`: every empty folder below the folder goes to the trash.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoveEmptyParams {
    pub path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrganizeBy {
    /// Images, Videos, Audio, Documents, Archives, Other (by extension).
    Type,
    /// `2024`, from the modified date (local time).
    Year,
    /// `2024-03`.
    Month,
}

/// `plan.organize`: the folder's files move into subfolders by type, year or month.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrganizeParams {
    pub path: String,
    pub by: OrganizeBy,
    /// What to do with names that exist in a subfolder (default skip).
    #[serde(default)]
    pub on_conflict: Option<OnConflict>,
}

/// `plan.set_attributes`: the attributes given are set on each path (not recursive).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttributesParams {
    pub paths: Vec<String>,
    /// Windows and macOS.
    #[serde(default)]
    pub hidden: Option<bool>,
    #[serde(default)]
    pub readonly: Option<bool>,
    /// Unix permission bits in octal (`"755"`, `"0640"`).
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    /// A symbolic link (on Windows: needs Developer Mode or an elevated host).
    Symlink,
    /// Windows: a junction to a folder.
    Junction,
    /// A second name for a file on the same drive.
    Hardlink,
}

/// `plan.link`: a new link at `link` pointing to `target`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkParams {
    pub target: String,
    /// The new link's full path (it must not exist).
    pub link: String,
    pub kind: LinkKind,
}

/// `plan.folder_dates`: each folder at and under `path` gets the modified time of the newest
/// file below it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FolderDatesParams {
    pub path: String,
}

/// One projected change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Change {
    /// `copy`, `move`, `delete`, `rename`, `make_folder`, `remove_folder`, `set_attributes`,
    /// `link`, `set_folder_date`, or the operation's own (`tag.add`, `source.add`…).
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
    /// `permanent`, `content_unverified`, `rewrites_archive`, `creates_tag`, `secret`…
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
    /// Library sync (tags, favorites) is on with this device here.
    #[serde(default)]
    pub sync: bool,
    /// The last completed library sync pull from this device (unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<i64>,
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

/// This device's keel-net settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DeviceSettingsInfo {
    /// This device's name on the others.
    pub label: String,
    /// Where Spacedrops land ("" when this host takes none).
    pub inbox: String,
    /// Device ids whose Spacedrops are accepted without asking.
    pub auto_accept: Vec<String>,
    /// Whether the node uses public relays when no direct path works (as it started).
    pub relay: bool,
    /// Device ids library sync (tags, favorites) is on with.
    #[serde(default)]
    pub sync: Vec<String>,
}

/// What `devices.settings_set` changes (left out: kept).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeviceSettingsParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// An absolute folder (not in Keel's configuration or data folder, but its inbox).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbox: Option<String>,
    /// Device ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_accept: Option<Vec<String>>,
    /// Public relays on or off: the node cannot re-bind, so this applies when the host
    /// starts again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<bool>,
    /// Device ids to sync the library (tags, favorites) with: every paired device listed
    /// is on, every other one off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<Vec<String>>,
}

/// `library.sync`: pull from one device (its id), or from every device sync is on with.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LibrarySyncParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
}

/// One device's pull (`library.sync`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SyncedDevice {
    pub peer: String,
    pub label: String,
    /// Changes that were newer than this library's and were applied.
    pub applied: usize,
    /// Why the pull failed (the device is offline, its switch for this device is off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DeviceSettingsSet {
    pub settings: DeviceSettingsInfo,
    /// `relay` differs from the running node's: it applies when the host starts again.
    pub restart: bool,
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
    /// An image or video ("" when `content_id` names the content).
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub size: ThumbSize,
    /// A content id (64 hex digits): answered from a sidecar made earlier for that content
    /// (wherever it was found), before `path` is looked at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
    /// Make a missing sidecar now (default); false: only one that exists, else NOT_FOUND.
    #[serde(default = "yes")]
    pub make: bool,
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
    /// exist yet; Linux and macOS: an empty folder, made when it is missing).
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

// --- what the desktop app needs while it is attached to a daemon ---

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IndexParams {
    pub id: String,
    /// Index whatever is at the root now, for a source held offline because another folder
    /// (or nothing) is there.
    #[serde(default)]
    pub adopt: bool,
}

/// Counts over every source (the Overview's cards).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LibraryStats {
    pub sources: usize,
    pub offline_sources: usize,
    /// Files and folders across every source's last generation.
    pub records: u64,
    pub files: u64,
    pub bytes: u64,
    /// Distinct content across sources, as of the last hashing run.
    pub unique_content: u64,
    pub running_jobs: usize,
    /// Each source's own counts (absent from daemons before 0.12).
    #[serde(default)]
    pub per_source: Vec<SourceStats>,
    /// Which remote files hashing downloads, as `hashing.set` last set them (absent from
    /// daemons before 0.12). Windows read it from here rather than from their settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hashing: Option<RemoteHashing>,
    /// When the last backup was written (`library.backup`; unix seconds), absent before the
    /// first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_backup: Option<i64>,
    /// Text inside documents (`contents.set`), absent from daemons before 0.22.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<TextStats>,
}

/// The text read from documents (library-wide, or one source's).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TextStats {
    /// Index file contents: the library's setting, or whether this source's are read now.
    pub on: bool,
    /// Documents whose text is indexed.
    pub documents: u64,
    /// Files skipped: no reader for their kind, or over the size cap.
    pub skipped: u64,
    /// Damaged files, and readers that ran out of time.
    pub failed: u64,
    /// Characters indexed.
    pub chars: u64,
}

/// The library's remote hashing policy (`hashing.set`'s remote, cloud, max_remote_bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteHashing {
    /// SFTP and other non-cloud remotes.
    pub remote: bool,
    /// Cloud accounts.
    pub cloud: bool,
    /// Remote files over this size are not hashed.
    pub max_remote_bytes: u64,
}

/// One source's counts on the Overview, from its index as last walked.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SourceStats {
    pub id: String,
    pub label: String,
    pub files: u64,
    /// Folders below the source's root.
    pub folders: u64,
    pub bytes: u64,
    /// Files with a content hash (`hashing.set`); the rest are not checked for copies yet.
    pub hashed_files: u64,
    /// The last completed full walk, unix seconds (absent: never indexed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_walk: Option<i64>,
    pub offline: bool,
    /// Text inside its documents (absent from daemons before 0.22).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<TextStats>,
}

/// The protection card: how safe the library's contents are.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Protection {
    /// Hashed contents held by one file only.
    pub single_copy: u64,
    /// Contents with two or more copies, all in one failure domain.
    pub single_domain: u64,
    /// Contents without a copy on a backup volume in a second failure domain.
    pub unbacked: u64,
    /// Files whose bytes changed while their size and times did not.
    pub drifted: u64,
    /// Files not hashed yet: in none of the counts above.
    pub unchecked: u64,
    pub offline_volumes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VolumeKindName {
    Fixed,
    Removable,
    Network,
    Cloud,
    Device,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VolumeStateName {
    Online,
    Offline,
    Archived,
    Lost,
    Retired,
}

/// A volume (disk, share, cloud account, device) holding sources.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VolumeInfo {
    pub id: String,
    pub label: String,
    pub kind: VolumeKindName,
    pub failure_domain: String,
    /// The failure domain was set by hand.
    pub domain_set: bool,
    pub state: VolumeStateName,
    /// Unix seconds a source on it was last reachable (0: never).
    pub last_seen: i64,
    pub backup: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// Drive inventory: one or more of a volume's settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VolumeSetParams {
    /// A volume id (`volumes.list`).
    pub volume: String,
    /// `archived`, `lost` or `retired`; `online` / `offline` make it automatic again.
    #[serde(default)]
    pub state: Option<VolumeStateName>,
    /// Mark (or unmark) as a backup volume.
    #[serde(default)]
    pub backup: Option<bool>,
    /// The failure domain; "" goes back to the detected one.
    #[serde(default)]
    pub failure_domain: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IntegrityParams {
    /// One source id; every source when absent.
    #[serde(default)]
    pub source: Option<String>,
    /// Percentage of each source's hashed files to re-hash (default 1).
    #[serde(default)]
    pub sample_pct: Option<f64>,
    /// Only when the last check ended this many days ago or longer (every source; the first
    /// call only starts the clock).
    #[serde(default)]
    pub due_days: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HashingParams {
    /// Hash contents after each walk (off cancels a running hash job).
    pub on: bool,
    /// Pause hashing while the user works (as the other background jobs do).
    #[serde(default)]
    pub idle_only: bool,
    /// Hash SFTP and other non-cloud remotes (default on); omitted keeps the setting.
    #[serde(default)]
    pub remote: Option<bool>,
    /// Hash cloud files (default off: downloads may incur egress charges).
    #[serde(default)]
    pub cloud: Option<bool>,
    /// Maximum bytes per remote file (default 1 GiB); omitted keeps the setting.
    #[serde(default)]
    pub max_remote_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContentsParams {
    /// Read and search the text inside documents.
    pub on: bool,
    /// One source's switch (omitted: the library's Index file contents setting).
    #[serde(default)]
    pub id: Option<String>,
    /// With `id`: back to the source's default (local folders and drives on; shares,
    /// SFTP, cloud and paired devices off); `on` is then ignored.
    #[serde(default)]
    pub default: bool,
}

/// A job, when one was started.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MaybeJob {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<i64>,
}

/// A path with every tag on it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TaggedPath {
    pub path: String,
    /// Tag ids (`tags.list`); Favorites is 1.
    pub tags: Vec<i64>,
}

/// `library.folder_style`: one folder, or every styled folder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FolderStyleParams {
    /// A folder in a library source; omitted: every folder that has a color or icon.
    #[serde(default)]
    pub path: Option<String>,
}

/// A folder's color and icon (both absent: none).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FolderStyleInfo {
    pub path: String,
    /// `#rrggbb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// One emoji, or `theme:<name>` (the icon theme's folder icon for folders of that
    /// name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

/// `library.set_folder_style`: omitted fields stay as they are, `""` clears one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetFolderStyleParams {
    /// An indexed folder in a library source.
    pub path: String,
    /// `#rrggbb`, or `""` to clear it.
    #[serde(default)]
    pub color: Option<String>,
    /// One emoji or `theme:<name>`, or `""` to clear it.
    #[serde(default)]
    pub icon: Option<String>,
}

/// `library.comment` / `library.set_comment`: a file's or folder's Get Info comment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CommentInfo {
    pub path: String,
    /// Absent: no comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// `library.set_comment`: blank clears it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetCommentParams {
    /// An indexed file or folder in a library source.
    pub path: String,
    /// The comment (at most 64 KiB), or `""` to clear it.
    pub comment: String,
}

/// A saved view: a bookmark of a search (the window's Bookmarks list and sidebar).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ViewInfo {
    pub id: i64,
    pub name: String,
    /// A `search` query (backend `library`), or the platform search's text (`system`).
    pub query: String,
    pub layout: String,
    /// `library` (the library index) or `system` (the platform's search: Everything or the
    /// Keel index on Windows, Spotlight, locate).
    #[serde(default = "library_backend")]
    pub backend: String,
    /// `<column>` or `<column>:desc`, up to three comma-separated keys; `""`: default order.
    #[serde(default)]
    pub sort: String,
    /// Column names in order; empty: the default columns.
    #[serde(default)]
    pub columns: Vec<String>,
    /// The match toggles that are on.
    #[serde(default)]
    pub matching: MatchInfo,
    /// The filter's name (`Audio`, `Document`, a custom filter); `""`: Everything.
    #[serde(default)]
    pub filter: String,
    /// Folder in the Bookmarks list, `/`-separated; `""`: the top level.
    #[serde(default)]
    pub folder: String,
    /// Order inside the folder (ascending).
    #[serde(default)]
    pub position: i64,
    /// Shortcut key in the window (`Ctrl+Shift+1`); `""`: none.
    #[serde(default)]
    pub shortcut: String,
}

fn library_backend() -> String {
    "library".into()
}

/// Search match toggles; omitted ones are off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MatchInfo {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub case: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub whole_words: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub path: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub diacritics: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub regex: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub prefix: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub suffix: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub ignore_punctuation: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub ignore_whitespace: bool,
}

/// `views.create`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewCreateParams {
    pub name: String,
    pub query: String,
    /// Default `details`.
    #[serde(default)]
    pub layout: Option<String>,
    /// `library` (default; the query must be a valid `search` query) or `system`.
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub sort: Option<String>,
    #[serde(default)]
    pub columns: Option<Vec<String>>,
    #[serde(default)]
    pub matching: Option<MatchInfo>,
    #[serde(default)]
    pub filter: Option<String>,
    #[serde(default)]
    pub folder: Option<String>,
    /// Omitted: last in its folder.
    #[serde(default)]
    pub position: Option<i64>,
    #[serde(default)]
    pub shortcut: Option<String>,
}

/// `views.update`: omitted fields stay as they are.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewUpdateParams {
    pub id: i64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub layout: Option<String>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub sort: Option<String>,
    #[serde(default)]
    pub columns: Option<Vec<String>>,
    #[serde(default)]
    pub matching: Option<MatchInfo>,
    #[serde(default)]
    pub filter: Option<String>,
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub position: Option<i64>,
    /// `""` removes the shortcut.
    #[serde(default)]
    pub shortcut: Option<String>,
}

/// `views.delete`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewIdParams {
    pub id: i64,
}

/// The copies of one file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FileCopies {
    pub path: String,
    pub copies: Copies,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FolderSizesParams {
    pub path: String,
    /// At most this many children, the biggest (default 1000).
    #[serde(default)]
    pub max: Option<usize>,
    /// Only the child folders.
    #[serde(default)]
    pub dirs_only: bool,
}

/// A folder's indexed total and its children's sizes, biggest first (`library.folder_sizes`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FolderSizes {
    pub path: String,
    /// Apparent bytes below the folder; absent until its source's first walk has finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Files below the folder (absent with `bytes`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<u64>,
    pub children: Vec<ChildSize>,
    /// More children exist than `max`.
    #[serde(default)]
    pub truncated: bool,
}

/// One child of a folder: a file's length, a folder's total (absent while unknown, and for
/// a link).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ChildSize {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BackupParams {
    /// An absolute local path: a new `.keelbackup` file, or a folder to write
    /// `<library>-<YYYY-MM-DD>.keelbackup` in. An existing file is never replaced.
    pub file: String,
}

/// A backup written (`library.backup`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BackupWritten {
    pub file: String,
    pub bytes: u64,
    /// Unix seconds.
    pub created: i64,
    pub sources: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoreParams {
    /// The backup file (an absolute local path).
    pub file: String,
    /// The new library's name (default: the backed-up library's). Never an existing one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A library restored from a backup (`library.restore`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RestoredLibrary {
    pub name: String,
    /// Its folder.
    pub dir: String,
    pub sources: Vec<RestoredSourceInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RestoredSourceInfo {
    pub id: String,
    pub label: String,
    pub root: String,
    /// Its root is a local folder here: walked again when the library opens. Others stay
    /// offline until their root is back.
    pub online: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectParams {
    /// A real path or a library path (`library://<source>/<rel>`).
    pub path: String,
    /// At most this many other copies and history entries (default 50, at most 1000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<usize>,
}

/// One path as the inspector shows it (`library.inspect`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Inspection {
    /// From the file system; from the index for a library path, or when the file system
    /// cannot be reached (an offline source).
    pub entry: EntryInfo,
    /// What the library knows; absent when the path is in no source or not indexed yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library: Option<InspectedRecord>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InspectedRecord {
    pub source: String,
    pub source_label: String,
    pub record: i64,
    /// The volume its source is on, with its state and backup mark.
    pub volume: VolumeInfo,
    /// The content id (64 hex digits); absent before hashing, for a folder, or after drift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
    /// Unix seconds the index first saw it (absent when indexed by a Keel before 0.21).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_seen: Option<i64>,
    /// Unix seconds its bytes were last hashed here (absent: not hashed here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_hashed: Option<i64>,
    /// A file's protection count; `locations` are the OTHER files holding its content (at
    /// most `max`, device claims last). Absent for a folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copies: Option<Copies>,
    /// More other copies exist than `max`.
    #[serde(default)]
    pub more_copies: bool,
    /// Its tags (Favorites is `favorite`).
    pub tags: Vec<TagInfo>,
    pub favorite: bool,
    /// The operations that touched it (copied, moved, renamed, deleted, tagged), newest
    /// first, at most `max`; it is followed back through its moves and renames.
    pub history: Vec<HistoryEntry>,
    /// More history exists than `max`.
    #[serde(default)]
    pub more_history: bool,
    /// A folder's totals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<FolderTotals>,
}

/// One operation log entry that touched an item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HistoryEntry {
    pub id: i64,
    /// Unix seconds.
    pub ts: i64,
    /// `copy`, `move`, `rename`, `delete`, `tag` or `untag`.
    pub kind: String,
    /// Whether it completed (absent while it runs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    /// The item came from here (moved, renamed or copied from it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// A copy of it was made here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// `tag` / `untag`: the tag's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

/// A folder's indexed totals.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FolderTotals {
    /// Apparent bytes below it; absent until its source's first walk has finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Files below it (absent with `bytes`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<u64>,
    /// Folders below it (links not counted).
    pub folders: u64,
    /// Its biggest child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub biggest: Option<ChildSize>,
}

// --- disk usage (Phase 17 Task 86) ---

/// `du.scan`: what to scan and how. Filters leave items out of the scan entirely.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DuScanParams {
    /// Absolute paths of folders or drives on the host.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Also every local fixed drive.
    #[serde(default)]
    pub all_drives: bool,
    /// Leave out files and folders whose name matches one of these globs (`*`, `?`).
    #[serde(default)]
    pub exclude_names: Vec<String>,
    /// Leave out these absolute paths and what is below them.
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    /// Leave out items whose full path matches this regular expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_regex: Option<String>,
    /// Leave out files smaller than this many bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_size: Option<u64>,
    /// Leave out files last written more than this many seconds ago.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub older_than_secs: Option<u64>,
    /// Leave out files last written less than this many seconds ago.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newer_than_secs: Option<u64>,
    /// Leave out hidden items.
    #[serde(default)]
    pub exclude_hidden: bool,
    /// Leave out items with the system attribute (Windows).
    #[serde(default)]
    pub exclude_system: bool,
    #[serde(default)]
    pub follow_symlinks: bool,
    /// Windows.
    #[serde(default)]
    pub follow_junctions: bool,
    #[serde(default)]
    pub follow_mount_points: bool,
    /// Count every name of a hard-linked file (by default a file counts once).
    #[serde(default)]
    pub count_hard_links_each: bool,
}

/// A disk usage scan started in the background (`du.scan`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DuScanStarted {
    /// For `du.result` and `du.export`.
    pub id: String,
}

/// `du.result`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DuResultParams {
    pub id: String,
    /// At most this many items of each root, biggest first (default 20).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<usize>,
    /// Rank by bytes on disk instead of bytes.
    #[serde(default)]
    pub size_on_disk: bool,
    /// Wait up to this many milliseconds (at most 10000) for a running scan to finish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DuState {
    Running,
    Done,
    Failed,
}

/// How far a scan got.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DuProgress {
    pub files: u64,
    pub folders: u64,
    /// Bytes of the files found so far.
    pub bytes: u64,
    pub elapsed_ms: u64,
    /// The folder being listed (recently).
    pub current: String,
}

/// A scan's state, and when done its roots with their biggest items (`du.result`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DuResult {
    pub id: String,
    pub state: DuState,
    pub progress: DuProgress,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<DuSummary>,
    /// Why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A finished scan, as `keel du --scan --json` prints it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DuSummary {
    /// False when the scan was cut short.
    pub complete: bool,
    pub elapsed_ms: u64,
    pub bytes: u64,
    /// Bytes on disk.
    pub allocated: u64,
    pub files: u64,
    pub folders: u64,
    pub items: u64,
    /// Folders that could not be read.
    pub unreadable: u64,
    /// Items the filters left out.
    pub excluded: u64,
    pub roots: Vec<DuRoot>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DuRoot {
    pub path: String,
    pub bytes: u64,
    pub allocated: u64,
    pub files: u64,
    pub folders: u64,
    pub items: u64,
    /// The newest last write below it (unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest: Option<i64>,
    /// A drive's free space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free: Option<u64>,
    /// A drive's used space the scan did not find.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Its biggest items.
    pub children: Vec<DuItem>,
    /// More, smaller items were left out.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DuItem {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// dir, file or link.
    pub kind: String,
    pub bytes: u64,
    pub allocated: u64,
    pub files: u64,
    pub folders: u64,
    pub items: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest: Option<i64>,
    /// Hard link, unreadable, cloud placeholder, ...
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// What `du.export` writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DuList {
    /// Every item (JSON: a scan file `keel du --load-from` reads).
    Tree,
    /// One row per extension.
    Types,
    /// The biggest files.
    Largest,
    /// The HTML report (format is ignored).
    Report,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DuFormat {
    Csv,
    Json,
}

/// `du.export`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DuExportParams {
    pub id: String,
    pub list: DuList,
    /// CSV or JSON (default JSON).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<DuFormat>,
    /// Start CSV with a UTF-8 byte order mark.
    #[serde(default)]
    pub bom: bool,
    /// At most this many largest files (default 100), or top folders and file types in a
    /// report (default 20); 0: all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<usize>,
    /// Rank by bytes on disk instead of bytes.
    #[serde(default)]
    pub size_on_disk: bool,
}

/// An export's text (`du.export`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DuExport {
    /// csv, json or html.
    pub format: String,
    pub content: String,
}
