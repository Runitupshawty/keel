use crate::VPath;
use std::time::SystemTime;

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
    pub modified: Option<SystemTime>,
    pub hidden: bool,
    pub ext: String,
}
