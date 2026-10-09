use crate::VPath;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
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
    /// Lowercase extension without the dot.
    pub ext: String,
}
