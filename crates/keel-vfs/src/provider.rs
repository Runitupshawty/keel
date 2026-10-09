use crate::{Entry, VPath};
use anyhow::Result;
use std::{
    io::{Read, Write},
    path::PathBuf,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct Caps {
    pub write: bool,
    pub rename: bool,
    pub delete: bool,
    pub watch: bool,
}

pub trait Provider: Send + Sync {
    fn scheme(&self) -> &'static str;
    fn caps(&self) -> Caps;
    fn list(&self, dir: &VPath) -> Result<Vec<Entry>>;
    fn stat(&self, p: &VPath) -> Result<Entry>;
    fn read(&self, p: &VPath) -> Result<Box<dyn Read + Send>>;
    fn write(&self, p: &VPath) -> Result<Box<dyn Write + Send>>;
    fn mkdir(&self, p: &VPath) -> Result<()>;
    fn rename(&self, from: &VPath, to: &VPath) -> Result<()>;
    fn remove(&self, p: &VPath) -> Result<()>;
    fn local_copy(&self, p: &VPath) -> Result<PathBuf>;
}
