use crate::VPath;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// A link whose target cannot be read (dangling or inaccessible).
    Symlink,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: VPath,
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    pub modified: Option<std::time::SystemTime>,
    pub hidden: bool,
    /// A symlink or junction; `kind`, `size` and `modified` describe its target.
    pub is_link: bool,
    /// A password-protected archive entry: listed, never read or extracted.
    pub encrypted: bool,
    /// Lowercase extension without the dot.
    pub ext: String,
}
