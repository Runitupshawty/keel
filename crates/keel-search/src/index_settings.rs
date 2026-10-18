//! What Keel's own index covers beyond the NTFS volumes: Settings → Search
//! (`[search_index]` in the profile's `config.toml`). Mirrors Everything's Folders,
//! Volumes and Exclude tabs: a list of folders, per-volume choices for FAT, exFAT,
//! ReFS, removable and network volumes, the automatic include/remove switches, and
//! the exclude rules applied while indexing.

use serde::{Deserialize, Serialize};

/// Settings → Search.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct IndexSettings {
    /// Folders (any path that lists: a local folder, a share, `\\server\share`).
    pub folders: Vec<FolderRoot>,
    /// Volumes Keel indexes by walking (FAT, exFAT, ReFS, removable, network), with
    /// their choices; a volume missing here follows the automatic switches.
    pub volumes: Vec<VolumeRoot>,
    /// New fixed volumes are indexed when they appear (and added to `volumes`).
    pub auto_include_fixed: bool,
    /// New removable volumes are indexed when they appear (and added to `volumes`).
    pub auto_include_removable: bool,
    /// An offline volume is removed from `volumes` and its index deleted. Off: it stays
    /// searchable from its last scan, marked offline, and is rescanned when it is back.
    pub auto_remove_offline: bool,
    /// The Exclude tab, for every folder and volume above.
    pub exclude: ExcludeSettings,
    /// The index journal ([`crate::journal`]).
    pub journal: JournalSettings,
    /// The Properties tab: file properties read into the property store
    /// ([`crate::propindex`]).
    pub properties: PropertySettings,
    /// Run counts and dates of what Keel opens ([`crate::runs`]).
    pub run_history: crate::runs::RunHistorySettings,
}

impl Default for IndexSettings {
    fn default() -> Self {
        Self {
            folders: Vec::new(),
            volumes: Vec::new(),
            auto_include_fixed: true,
            auto_include_removable: false,
            auto_remove_offline: false,
            exclude: ExcludeSettings::default(),
            journal: JournalSettings::default(),
            properties: PropertySettings::default(),
            run_history: Default::default(),
        }
    }
}

/// One property of the Properties tab.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(default)]
pub struct IndexedProperty {
    /// A property function name (`artist`, `width`, `sha256`, `digital-signature-name`)
    /// or, on Windows, a canonical property name (`System.Photo.FNumber`).
    pub name: String,
    /// Keep a sort order of this property, so sorting by it is a read, not a sort.
    pub fast_sort: bool,
}

/// Settings → Search index → Properties (Everything 1.5's Properties tab): which file
/// properties an idle background job reads into the property store next to the index,
/// and for which files.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct PropertySettings {
    /// The properties indexed; empty: the job does not run.
    pub properties: Vec<IndexedProperty>,
    /// Only files inside these folders; empty: every folder Keel's index holds (the
    /// home folder and the Folders and Volumes of this page).
    pub include_only_folders: Vec<String>,
    /// Folders left out with everything below them (paths or wildcards, as the
    /// Exclude tab's).
    pub exclude_folders: Vec<String>,
    /// When not empty, only files whose name matches one of these wildcards.
    pub include_only_files: Vec<String>,
    /// Files whose name matches one of these wildcards are left out.
    pub exclude_files: Vec<String>,
    /// Leave out files with the "not content indexed" attribute (Windows).
    pub exclude_not_content_indexed: bool,
    /// Leave out cloud placeholders whose data is not on this device (recall on data
    /// access, recall on open, offline): reading them would download them.
    pub exclude_recall_on_data_access: bool,
    /// Files over this many MB are left out; 0: no limit.
    pub max_file_mb: u32,
}

impl Default for PropertySettings {
    fn default() -> Self {
        Self {
            properties: Vec::new(),
            include_only_folders: Vec::new(),
            exclude_folders: Vec::new(),
            include_only_files: Vec::new(),
            exclude_files: Vec::new(),
            exclude_not_content_indexed: false,
            exclude_recall_on_data_access: true,
            max_file_mb: 0,
        }
    }
}

impl PropertySettings {
    /// The size cap in bytes; `u64::MAX` for no limit.
    pub fn max_file_bytes(&self) -> u64 {
        match self.max_file_mb {
            0 => u64::MAX,
            mb => u64::from(mb) * 1024 * 1024,
        }
    }

    /// The property names, in order, without duplicates or blanks.
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for p in &self.properties {
            let n = p.name.trim();
            if !n.is_empty() && !out.iter().any(|o| o.eq_ignore_ascii_case(n)) {
                out.push(n.to_owned());
            }
        }
        out
    }
}

/// The journal's size cap when nothing else is set, in MB.
pub const DEFAULT_JOURNAL_MB: u32 = 64;

/// The index journal: every change the index applies, kept up to a size.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct JournalSettings {
    pub enabled: bool,
    /// The size cap in MB; the oldest entries go first.
    pub max_mb: u32,
}

impl Default for JournalSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_mb: DEFAULT_JOURNAL_MB,
        }
    }
}

impl JournalSettings {
    /// The cap in bytes (at least 1 MB).
    pub fn max_bytes(&self) -> u64 {
        u64::from(self.max_mb.max(1)) * 1024 * 1024
    }
}

/// One entry of the Folders list.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct FolderRoot {
    pub path: String,
    pub scan: ScanOptions,
}

/// One entry of the Volumes list: a drive root (`E:\`, `/media/usb`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct VolumeRoot {
    pub root: String,
    /// "Include in database": off keeps the volume out of the index.
    pub include: bool,
    /// "Include only": index just these folders of the volume (relative to its root,
    /// or absolute on it); empty = the whole volume.
    pub include_only: Vec<String>,
    pub scan: ScanOptions,
}

impl Default for VolumeRoot {
    fn default() -> Self {
        Self {
            root: String::new(),
            include: true,
            include_only: Vec::new(),
            scan: ScanOptions::default(),
        }
    }
}

/// How one folder or volume is kept current.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ScanOptions {
    /// Watch for changes (ReadDirectoryChangesW on Windows, FSEvents on macOS,
    /// inotify on Linux).
    pub monitor: bool,
    /// The change buffer in KiB (Windows; at most 64 for a network path).
    pub buffer_kib: u32,
    /// Rescan when the change buffer overflowed (changes were lost).
    pub rescan_on_overflow: bool,
    pub schedule: Schedule,
    /// A rescan visits only the folders whose modified time moved.
    pub fast_rescan: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            monitor: true,
            buffer_kib: 64,
            rescan_on_overflow: true,
            schedule: Schedule::Never,
            fast_rescan: false,
        }
    }
}

/// When a folder or volume is rescanned besides its changes.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Schedule {
    #[default]
    Never,
    /// Every this many hours after the last scan.
    EveryHours(u32),
    /// Every day at this local time, "HH:MM".
    Daily(String),
}

/// The Exclude tab. Applied while indexing: what it leaves out is never stored.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct ExcludeSettings {
    /// Leave out hidden files and folders (the hidden attribute on Windows, a leading
    /// dot elsewhere).
    pub hidden: bool,
    /// Leave out files and folders with the system attribute (Windows).
    pub system: bool,
    /// Folders left out with everything below them: a path, or a wildcard on the
    /// folder's name (`node_modules`, `*.git`) or whole path (`*\cache\*`).
    pub folders: Vec<String>,
    /// When not empty, only what is inside these folders is indexed.
    pub include_only_folders: Vec<String>,
    /// When not empty, only files whose name matches one of these wildcards
    /// (`*.pdf`) are indexed; folders are kept.
    pub include_only_files: Vec<String>,
    /// Files whose name matches one of these wildcards are left out.
    pub files: Vec<String>,
}

/// Splits a `;`-separated list as typed in Settings (`*.tmp; *.bak`), trimmed.
pub fn split_list(text: &str) -> Vec<String> {
    text.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_toml_and_defaults_missing_fields() {
        let mut s = IndexSettings::default();
        s.folders.push(FolderRoot {
            path: "/data/share".into(),
            scan: ScanOptions {
                schedule: Schedule::Daily("03:30".into()),
                fast_rescan: true,
                ..ScanOptions::default()
            },
        });
        s.volumes.push(VolumeRoot {
            root: "E:\\".into(),
            include_only: vec!["Photos".into()],
            scan: ScanOptions {
                schedule: Schedule::EveryHours(6),
                ..ScanOptions::default()
            },
            ..VolumeRoot::default()
        });
        s.exclude.files = split_list("*.tmp; *.bak;;");
        let text = toml::to_string(&s).unwrap();
        let back: IndexSettings = toml::from_str(&text).unwrap();
        assert_eq!(back, s);
        assert_eq!(s.exclude.files, ["*.tmp", "*.bak"]);

        let partial: IndexSettings =
            toml::from_str("auto_remove_offline = true\n[[folders]]\npath = \"/x\"\n").unwrap();
        assert!(partial.auto_remove_offline && partial.auto_include_fixed);
        assert_eq!(partial.folders[0].scan, ScanOptions::default());
        assert!(partial.run_history.enabled);
        assert!(partial.properties.exclude_recall_on_data_access);
    }

    #[test]
    fn property_settings_round_trip_and_clean_names() {
        let mut s = IndexSettings::default();
        s.properties.properties = vec![
            IndexedProperty {
                name: "artist".into(),
                fast_sort: true,
            },
            IndexedProperty {
                name: " Artist ".into(),
                fast_sort: false,
            },
            IndexedProperty::default(),
            IndexedProperty {
                name: "System.Photo.FNumber".into(),
                fast_sort: false,
            },
        ];
        s.properties.max_file_mb = 5;
        s.run_history.enabled = false;
        let back: IndexSettings = toml::from_str(&toml::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        assert_eq!(s.properties.names(), ["artist", "System.Photo.FNumber"]);
        assert_eq!(s.properties.max_file_bytes(), 5 << 20);
        assert_eq!(PropertySettings::default().max_file_bytes(), u64::MAX);
    }
}
