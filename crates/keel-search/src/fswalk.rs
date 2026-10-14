//! The plain walk: a folder searched by listing it, with the shared search language. Used
//! when no index holds the folder (a share, a volume not indexed yet) and as the last
//! resort backend. A name is checked first with the query's lead literal; only the
//! entries that pass are asked for their size, dates and attributes.

use std::cell::OnceCell;
use std::fs::Metadata;
use std::path::Path;
use std::time::SystemTime;

use keel_query::{DateProp, Props, Timestamp};

use crate::{language, Hit, Query};

struct WalkProps {
    path: String,
    name: String,
    is_dir: bool,
    meta: OnceCell<Option<Metadata>>,
}

impl WalkProps {
    fn new(entry: &ignore::DirEntry) -> Self {
        let path = entry.path();
        let is_dir = match entry.file_type() {
            Some(t) if t.is_symlink() => path.is_dir(),
            Some(t) => t.is_dir(),
            None => path.is_dir(),
        };
        Self {
            path: path.to_string_lossy().into_owned(),
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir,
            meta: OnceCell::new(),
        }
    }

    /// Read when a term needs more than the name; `None` when the entry is gone.
    fn meta(&self) -> Option<&Metadata> {
        self.meta
            .get_or_init(|| std::fs::metadata(&self.path).ok())
            .as_ref()
    }

    fn hit(&self) -> Option<Hit> {
        let meta = self.meta()?;
        Some(Hit {
            path: keel_vfs::VPath::local(Path::new(&self.path)),
            is_dir: self.is_dir,
            size: if self.is_dir { 0 } else { meta.len() },
            modified: meta.modified().ok(),
            snippet: None,
        })
    }
}

fn millis(t: std::io::Result<SystemTime>) -> Option<Timestamp> {
    match t.ok()?.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).ok(),
        Err(e) => i64::try_from(e.duration().as_millis()).ok().map(|ms| -ms),
    }
}

impl Props for WalkProps {
    fn name(&self) -> &str {
        &self.name
    }

    fn path(&self) -> &str {
        &self.path
    }

    fn is_dir(&self) -> bool {
        self.is_dir
    }

    fn size(&self) -> Option<u64> {
        let meta = self.meta()?;
        (!self.is_dir).then(|| meta.len())
    }

    fn allocated(&self) -> Option<u64> {
        None
    }

    fn date(&self, which: DateProp) -> Option<Timestamp> {
        let meta = self.meta()?;
        match which {
            DateProp::Modified => millis(meta.modified()),
            DateProp::Created => millis(meta.created()),
            DateProp::Accessed => millis(meta.accessed()),
            _ => None,
        }
    }

    fn attributes(&self) -> Option<u32> {
        let meta = self.meta()?;
        #[cfg(windows)]
        let bits = {
            use std::os::windows::fs::MetadataExt;
            meta.file_attributes()
        };
        #[cfg(not(windows))]
        let bits = {
            use keel_query::attr;
            let mut bits = 0;
            if meta.permissions().readonly() {
                bits |= attr::READONLY;
            }
            if self.name.starts_with('.') {
                bits |= attr::HIDDEN;
            }
            bits
        };
        Some(
            bits | if self.is_dir {
                keel_query::attr::DIRECTORY
            } else {
                0
            },
        )
    }
}

/// The entries of `query` under `root`, in walk order, up to `query.max`.
pub(crate) fn walk(root: &Path, query: &Query) -> anyhow::Result<Vec<Hit>> {
    let max = query.max as usize;
    let p = language::prefilter(query)?;
    if max == 0 {
        return Ok(Vec::new());
    }
    let sets = p.query.has_set_functions();
    let mut found: Vec<WalkProps> = Vec::new();
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        if entry.depth() == 0 {
            continue;
        }
        if let Some(scan) = &p.scan {
            if !scan.is_match(&entry.file_name().to_string_lossy()) {
                continue;
            }
        }
        let props = WalkProps::new(&entry);
        if (query.folders_only && !props.is_dir) || !p.query.matches(&props) {
            continue;
        }
        if props.meta().is_none() {
            continue;
        }
        found.push(props);
        // Set functions (`dupe:`, `unique:`, `count:`) see every match.
        if !sets && found.len() >= max {
            break;
        }
    }
    if let Some(mut pass) = p.query.set_pass() {
        for (i, props) in found.iter().enumerate() {
            if !pass.push(i as u64, props, None)? {
                break;
            }
        }
        let keep = pass.finish();
        let mut slots: Vec<Option<WalkProps>> = found.into_iter().map(Some).collect();
        found = keep
            .into_iter()
            .filter_map(|i| slots.get_mut(i as usize)?.take())
            .collect();
    }
    Ok(found.iter().take(max).filter_map(WalkProps::hit).collect())
}
