//! `.metadata.efu` sidecars (Everything 1.5): an EFU file named `.metadata.efu` in a folder
//! gives that folder's files extra properties. Its `Filename` column names a file of the
//! folder (a bare name, or a full path whose folder is this one; any other path is
//! ignored), and every column besides the five EFU ones (`Filename`, `Size`,
//! `Date Modified`, `Date Created`, `Attributes`) is a property (`Rating`, `Tags`,
//! `Comment`, ...). The file's own size, dates and attributes always come from the disk.
//!
//! Reading is bounded ([`MAX_SIDECAR_BYTES`], [`SIDECAR_LIMITS`]) and never touches the
//! files a sidecar names.

use std::collections::HashMap;
use std::io::{BufRead, Read};
use std::path::Path;

use anyhow::{bail, Context};

use super::{read, ListKind, Limits};

/// The sidecar's file name.
pub const SIDECAR_NAME: &str = ".metadata.efu";

/// A larger sidecar is refused.
pub const MAX_SIDECAR_BYTES: u64 = 16 << 20;

/// Rows and line length of a sidecar.
pub const SIDECAR_LIMITS: Limits = Limits {
    max_rows: 100_000,
    max_line: 64 * 1024,
};

/// The properties a folder's `.metadata.efu` gives its files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sidecar {
    /// The property columns, as the header names them.
    pub columns: Vec<String>,
    /// By file name without regard to case: a value per column ("" when the row has none).
    rows: HashMap<String, Vec<String>>,
}

/// File names are matched without regard to case (as on Windows, where sidecars are made).
fn key(name: &str) -> String {
    name.to_lowercase()
}

/// `path` with `/` as `\` and no trailing separator, lower case: for comparing folders.
fn folder_key(path: &str) -> String {
    path.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

impl Sidecar {
    /// Reads a sidecar from `r`. `folder` is the folder it describes: a row may then name
    /// a file by its full path in that folder.
    pub fn parse(r: impl BufRead, folder: Option<&str>, limits: &Limits) -> anyhow::Result<Self> {
        let list = read(r, ListKind::Efu, limits)?;
        let folder = folder.map(folder_key);
        let mut rows: HashMap<String, Vec<String>> = HashMap::new();
        for e in list.entries {
            let name = e.name();
            let bare = !e.path.contains(['/', '\\']);
            let in_folder = || {
                let trimmed = e.path.trim_end_matches(['/', '\\']);
                let parent = trimmed.strip_suffix(name).unwrap_or_default();
                folder
                    .as_deref()
                    .is_some_and(|f| !parent.is_empty() && folder_key(parent) == f)
            };
            if name.is_empty() || name == "." || name == ".." || !(bare || in_folder()) {
                continue;
            }
            // A later row for the same file fills in what the earlier one left empty.
            let slot = rows.entry(key(name)).or_insert_with(|| e.extra.clone());
            for (old, new) in slot.iter_mut().zip(&e.extra) {
                if !new.is_empty() {
                    old.clone_from(new);
                }
            }
        }
        Ok(Self {
            columns: list.columns,
            rows,
        })
    }

    /// The sidecar of `folder`, if it has one. Only `folder/.metadata.efu` is opened, and
    /// only when it is a file of at most [`MAX_SIDECAR_BYTES`].
    pub fn read(folder: &Path) -> anyhow::Result<Option<Self>> {
        let path = folder.join(SIDECAR_NAME);
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("{}", path.display())),
        };
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Ok(None);
        }
        if meta.len() > MAX_SIDECAR_BYTES {
            bail!(
                "{}: larger than {} MiB",
                path.display(),
                MAX_SIDECAR_BYTES >> 20
            );
        }
        let r = std::io::BufReader::new(file.take(MAX_SIDECAR_BYTES));
        let folder = folder.to_string_lossy();
        Self::parse(r, Some(&folder), &SIDECAR_LIMITS)
            .map(Some)
            .with_context(|| format!("{}", path.display()))
    }

    /// How many files the sidecar describes.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The properties of the file `name` that have a value: (column, value) in column order.
    pub fn properties(&self, name: &str) -> Vec<(&str, &str)> {
        let Some(values) = self.rows.get(&key(name)) else {
            return Vec::new();
        };
        (self.columns.iter())
            .zip(values)
            .filter(|(_, v)| !v.is_empty())
            .map(|(c, v)| (c.as_str(), v.as_str()))
            .collect()
    }

    /// One property of the file `name` (the column without regard to case; the first one
    /// with a value when the header names it twice), if it has a value.
    pub fn property(&self, name: &str, column: &str) -> Option<&str> {
        let values = self.rows.get(&key(name))?;
        (self.columns.iter())
            .zip(values)
            .find(|(c, v)| c.eq_ignore_ascii_case(column) && !v.is_empty())
            .map(|(_, v)| v.as_str())
    }
}

/// The properties the `.metadata.efu` of `path`'s folder gives `path` (none when there is
/// no sidecar, or it does not name the file). Reads the sidecar.
pub fn properties_of(path: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let (Some(folder), Some(name)) = (path.parent(), path.file_name()) else {
        return Ok(Vec::new());
    };
    let name = name.to_string_lossy();
    if name.eq_ignore_ascii_case(SIDECAR_NAME) {
        return Ok(Vec::new());
    }
    Ok(match Sidecar::read(folder)? {
        Some(s) => (s.properties(&name).into_iter())
            .map(|(c, v)| (c.to_owned(), v.to_owned()))
            .collect(),
        None => Vec::new(),
    })
}
