//! What the evaluator asks of an item: [`Props`]. Indexed properties (name, path, size,
//! dates, attributes) are plain reads; the optional ones (`type_name`, child counts, run
//! count, file ids, dates other than the indexed four, hard links, streams, archive
//! contents) default to unknown, and the evaluator asks for them only after the cheap terms
//! passed.

use std::borrow::Cow;

/// Milliseconds since 1970-01-01 UTC.
pub type Timestamp = i64;

/// A date property.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DateProp {
    Modified,
    Created,
    Accessed,
    /// The last change Keel saw (USN record time), `rc:`.
    RecentlyChanged,
    /// The last time Keel opened the item, `date-run:`.
    Run,
    Deleted,
    Saved,
    Printed,
    Received,
    Sent,
    Released,
    Acquired,
    Changed,
    ContentCreated,
    Taken,
    Encoded,
}

impl DateProp {
    /// Indexed dates are column reads; the rest are read lazily (property system).
    pub fn is_indexed(self) -> bool {
        matches!(
            self,
            DateProp::Modified
                | DateProp::Created
                | DateProp::Accessed
                | DateProp::RecentlyChanged
                | DateProp::Run
        )
    }
}

/// Direct children of a folder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChildCounts {
    pub files: u64,
    pub folders: u64,
}

/// Windows file attribute bits, and the letters Everything uses for them.
pub mod attr {
    pub const READONLY: u32 = 0x1;
    pub const HIDDEN: u32 = 0x2;
    pub const SYSTEM: u32 = 0x4;
    pub const DIRECTORY: u32 = 0x10;
    pub const ARCHIVE: u32 = 0x20;
    pub const NORMAL: u32 = 0x80;
    pub const TEMPORARY: u32 = 0x100;
    pub const SPARSE: u32 = 0x200;
    pub const REPARSE_POINT: u32 = 0x400;
    pub const COMPRESSED: u32 = 0x800;
    pub const OFFLINE: u32 = 0x1000;
    pub const NOT_CONTENT_INDEXED: u32 = 0x2000;
    pub const ENCRYPTED: u32 = 0x4000;
    pub const INTEGRITY_STREAM: u32 = 0x8000;
    pub const NO_SCRUB_DATA: u32 = 0x2_0000;
    pub const RECALL_ON_OPEN: u32 = 0x4_0000;
    pub const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

    /// `attributes:` letters.
    pub const LETTERS: &[(char, u32)] = &[
        ('A', ARCHIVE),
        ('C', COMPRESSED),
        ('D', DIRECTORY),
        ('E', ENCRYPTED),
        ('H', HIDDEN),
        ('I', NOT_CONTENT_INDEXED),
        ('L', REPARSE_POINT),
        ('M', RECALL_ON_DATA_ACCESS),
        ('N', NORMAL),
        ('O', OFFLINE),
        ('P', SPARSE),
        ('R', READONLY),
        ('S', SYSTEM),
        ('T', TEMPORARY),
        ('U', RECALL_ON_OPEN),
        ('V', INTEGRITY_STREAM),
        ('X', NO_SCRUB_DATA),
    ];
}

/// One file or folder as the evaluator sees it. `None` means unknown (not indexed, or
/// could not be read): it matches `unknown` and fails every comparison.
pub trait Props {
    /// The name (for a drive root, the drive: `C:`).
    fn name(&self) -> &str;
    /// The full path, with the platform's separators.
    fn path(&self) -> &str;
    fn is_dir(&self) -> bool;
    /// Size in bytes (folders: the total of what is below, when the source keeps it).
    fn size(&self) -> Option<u64>;
    /// Size on disk (allocated).
    fn allocated(&self) -> Option<u64>;
    fn date(&self, which: DateProp) -> Option<Timestamp>;
    /// Windows attribute bits ([`attr`]); other platforms map what they have.
    fn attributes(&self) -> Option<u32>;

    /// The extension without the dot; folders have none.
    fn extension(&self) -> &str {
        if self.is_dir() {
            ""
        } else {
            extension_of(self.name())
        }
    }
    /// The name without the extension; folders keep the whole name.
    fn stem(&self) -> &str {
        if self.is_dir() {
            self.name()
        } else {
            stem_of(self.name())
        }
    }
    /// How many folders are above the item.
    fn depth(&self) -> u32 {
        depth_of(&normalize(self.path()))
    }
    fn run_count(&self) -> Option<u64> {
        None
    }
    /// Direct children, for folders whose source counts them.
    fn child_counts(&self) -> Option<ChildCounts> {
        None
    }
    fn file_id(&self) -> Option<u64> {
        None
    }
    fn parent_file_id(&self) -> Option<u64> {
        None
    }
    /// The type description from the platform (the registry on Windows); `None` uses
    /// Keel's built-in table by extension.
    fn type_name(&self) -> Option<Cow<'_, str>> {
        None
    }
    /// How many names (hard links) the file has; 1 for most files. `None`: the source does
    /// not say, and [`hard_link_names`](Props::hard_link_names) is counted instead.
    fn hard_link_count(&self) -> Option<u64> {
        None
    }
    /// The full paths of every name of the file, this one included (read lazily).
    fn hard_link_names(&self) -> Option<Vec<String>> {
        None
    }
    /// How many alternate data streams (named streams) the file has; `None` counts
    /// [`stream_names`](Props::stream_names).
    fn stream_count(&self) -> Option<u64> {
        None
    }
    /// The names of the file's alternate data streams (`Zone.Identifier`), read lazily.
    fn stream_names(&self) -> Option<Vec<String>> {
        None
    }
    /// How many files an archive holds; `None` counts
    /// [`container_file_names`](Props::container_file_names).
    fn container_file_count(&self) -> Option<u64> {
        None
    }
    /// The paths inside an archive (`docs\a.txt`), read lazily and bounded by the source.
    fn container_file_names(&self) -> Option<Vec<String>> {
        None
    }
    /// Whether the Windows Search index found the item for `text` (`si:text`); `None`:
    /// the source did not ask it (no match).
    fn system_index(&self, _text: &str) -> Option<bool> {
        None
    }
}

/// An item with the run history's count and last open (`run-count:`, `date-run:`) over
/// what its source says: Keel's run history lives beside the index, not in it.
pub struct WithRuns<'a> {
    pub item: &'a dyn Props,
    pub count: Option<u64>,
    /// When Keel last opened the item.
    pub last_run: Option<Timestamp>,
}

impl Props for WithRuns<'_> {
    fn name(&self) -> &str {
        self.item.name()
    }
    fn path(&self) -> &str {
        self.item.path()
    }
    fn is_dir(&self) -> bool {
        self.item.is_dir()
    }
    fn size(&self) -> Option<u64> {
        self.item.size()
    }
    fn allocated(&self) -> Option<u64> {
        self.item.allocated()
    }
    fn date(&self, which: DateProp) -> Option<Timestamp> {
        match which {
            DateProp::Run => self.last_run.or_else(|| self.item.date(which)),
            _ => self.item.date(which),
        }
    }
    fn attributes(&self) -> Option<u32> {
        self.item.attributes()
    }
    fn extension(&self) -> &str {
        self.item.extension()
    }
    fn stem(&self) -> &str {
        self.item.stem()
    }
    fn depth(&self) -> u32 {
        self.item.depth()
    }
    fn run_count(&self) -> Option<u64> {
        self.count.or_else(|| self.item.run_count())
    }
    fn child_counts(&self) -> Option<ChildCounts> {
        self.item.child_counts()
    }
    fn file_id(&self) -> Option<u64> {
        self.item.file_id()
    }
    fn parent_file_id(&self) -> Option<u64> {
        self.item.parent_file_id()
    }
    fn type_name(&self) -> Option<Cow<'_, str>> {
        self.item.type_name()
    }
    fn hard_link_count(&self) -> Option<u64> {
        self.item.hard_link_count()
    }
    fn hard_link_names(&self) -> Option<Vec<String>> {
        self.item.hard_link_names()
    }
    fn stream_count(&self) -> Option<u64> {
        self.item.stream_count()
    }
    fn stream_names(&self) -> Option<Vec<String>> {
        self.item.stream_names()
    }
    fn container_file_count(&self) -> Option<u64> {
        self.item.container_file_count()
    }
    fn container_file_names(&self) -> Option<Vec<String>> {
        self.item.container_file_names()
    }
    fn system_index(&self, text: &str) -> Option<bool> {
        self.item.system_index(text)
    }
}

/// The extension of a file name (`"a.tar.gz"` → `"gz"`, `".bashrc"` → `"bashrc"`).
pub fn extension_of(name: &str) -> &str {
    name.rsplit_once('.').map_or("", |(_, e)| e)
}

/// The name without its extension.
pub fn stem_of(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// `/` as `\`, so one set of rules matches Windows and Unix paths.
pub fn normalize(path: &str) -> Cow<'_, str> {
    if path.contains('/') {
        Cow::Owned(path.replace('/', "\\"))
    } else {
        Cow::Borrowed(path)
    }
}

/// The parent folder of a normalized path, or `None` for a root (`C:\`, `\`, `\\server\share`).
pub fn parent_of(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('\\');
    if trimmed.is_empty() || (trimmed.len() == 2 && trimmed.ends_with(':')) {
        return None;
    }
    if let Some(unc) = trimmed.strip_prefix("\\\\") {
        if unc.split('\\').count() <= 2 {
            return None;
        }
    }
    let (parent, _) = trimmed.rsplit_once('\\')?;
    Some(if parent.is_empty() {
        "\\".to_string()
    } else if parent.len() == 2 && parent.ends_with(':') {
        format!("{parent}\\")
    } else {
        parent.to_string()
    })
}

/// How many folders are above a normalized path.
pub fn depth_of(path: &str) -> u32 {
    let trimmed = path.trim_end_matches('\\');
    let parts = trimmed.split('\\').count() as u32;
    if trimmed.starts_with("\\\\") {
        parts.saturating_sub(4)
    } else {
        parts.saturating_sub(1)
    }
}

/// The last component of a normalized path.
pub fn name_of(path: &str) -> &str {
    let trimmed = path.trim_end_matches('\\');
    trimmed.rsplit_once('\\').map_or(trimmed, |(_, n)| n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(parent_of("C:\\a\\b.txt").as_deref(), Some("C:\\a"));
        assert_eq!(parent_of("C:\\a").as_deref(), Some("C:\\"));
        assert_eq!(parent_of("C:\\"), None);
        assert_eq!(parent_of("\\home\\x").as_deref(), Some("\\home"));
        assert_eq!(parent_of("\\home").as_deref(), Some("\\"));
        assert_eq!(parent_of("\\"), None);
        assert_eq!(parent_of("\\\\srv\\share"), None);
        assert_eq!(
            parent_of("\\\\srv\\share\\x").as_deref(),
            Some("\\\\srv\\share")
        );
        assert_eq!(depth_of("C:\\a\\b.txt"), 2);
        assert_eq!(depth_of("C:\\"), 0);
        assert_eq!(depth_of("\\home\\me"), 2);
        assert_eq!(depth_of("\\"), 0);
        assert_eq!(depth_of("\\\\srv\\share\\x"), 1);
        assert_eq!(extension_of("a.tar.gz"), "gz");
        assert_eq!(stem_of("a.tar.gz"), "a.tar");
        assert_eq!(extension_of("README"), "");
    }
}
