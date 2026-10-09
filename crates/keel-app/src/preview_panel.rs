//! Preview panel. Placeholder until Task 7 renders `keel_preview::Preview` here;
//! `PreviewKey` is already used for grid thumbnails.

use keel_vfs::{Entry, VPath};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PreviewKey {
    pub path: VPath,
    pub mtime: u64,
    pub size: u64,
    pub page: u32,
}

impl PreviewKey {
    pub fn of(entry: &Entry, page: u32) -> Self {
        Self {
            path: entry.path.clone(),
            mtime: entry
                .modified
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs()),
            size: entry.size,
            page,
        }
    }
}

/// Task 7 fills this in (right side panel, Ctrl+Shift+V / F3).
#[allow(dead_code)]
#[derive(Default)]
pub struct PreviewPanel {
    pub open: bool,
    pub key: Option<PreviewKey>,
    pub current: Option<keel_preview::Preview>,
    pub page: u32,
    pub tex: Option<egui::TextureHandle>,
}
