//! Folder colors and icons (Task 56, parity gap #17), and file icons.
//!
//! A folder of a library source can carry a color (`#rrggbb`) and an icon: one emoji, or
//! `theme:<name>`, the icon theme's folder icon for folders named `<name>` (the app falls
//! back to its own folder icon when no theme has that name). A file can carry an icon
//! (no color): one emoji, or `theme:<name>`, the icon theme's file icon for a file named
//! `<name>` (`theme:.rs` for an extension). Styles live in the source's
//! store next to the tag links, keyed by record (`folder_style`, source schema 11): a move
//! or rename the index records keeps the record, so the style follows the folder; a folder
//! that leaves the index, or a source deleted with its store, drops it. They sync between
//! devices like path-keyed tag assignments (`SyncOp::Style`). Nothing here touches a file.

use crate::library::{RecordRef, Source};
use crate::search::{hit_of, HIT_COLUMNS};
use crate::{Library, LibraryHit};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use unicode_properties::UnicodeEmoji;
use unicode_segmentation::UnicodeSegmentation;

/// `FolderStyle::icon` naming an icon theme's folder icon.
pub const THEME_ICON: &str = "theme:";
/// Bytes in an emoji icon (a family or flag sequence is a few dozen).
const MAX_EMOJI: usize = 64;
/// Bytes in a `theme:` name.
const MAX_THEME_NAME: usize = 64;

/// A folder's color and icon, or a file's icon; both None is no style.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FolderStyle {
    /// `#rrggbb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// One emoji, or `theme:<name>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

impl FolderStyle {
    pub fn is_empty(&self) -> bool {
        self.color.is_none() && self.icon.is_none()
    }

    /// Refuses a color that is not `#rrggbb` and an icon that is neither one emoji nor a
    /// `theme:` name.
    pub fn check(&self) -> Result<()> {
        if let Some(c) = &self.color {
            ensure!(valid_color(c), "a folder color is #rrggbb, not {c:?}");
        }
        if let Some(i) = &self.icon {
            ensure!(
                valid_icon(i),
                "a folder icon is one emoji or theme:<name>, not {i:?}"
            );
        }
        Ok(())
    }

    /// The icon theme folder icon's name (`theme:<name>`).
    pub fn theme_icon(&self) -> Option<&str> {
        self.icon.as_deref()?.strip_prefix(THEME_ICON)
    }

    /// The emoji icon.
    pub fn emoji(&self) -> Option<&str> {
        self.icon.as_deref().filter(|i| !i.starts_with(THEME_ICON))
    }

    /// The color as RGB.
    pub fn rgb(&self) -> Option<[u8; 3]> {
        let c = self.color.as_deref().filter(|c| valid_color(c))?;
        let byte = |i: usize| u8::from_str_radix(&c[i..i + 2], 16).ok();
        Some([byte(1)?, byte(3)?, byte(5)?])
    }
}

/// `#rrggbb` (either case).
pub fn valid_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// One emoji: a single grapheme of emoji characters and emoji components that starts with
/// an emoji (a digit, `#` or `*` only as a keycap).
pub fn valid_emoji(s: &str) -> bool {
    let mut graphemes = s.graphemes(true);
    let (Some(one), None) = (graphemes.next(), graphemes.next()) else {
        return false;
    };
    let Some(first) = one.chars().next() else {
        return false;
    };
    one.len() <= MAX_EMOJI
        && one.chars().all(|c| c.is_emoji_char_or_emoji_component())
        && ((!first.is_ascii() && first.is_emoji_char()) || one.ends_with('\u{20E3}'))
}

/// `theme:<name>`: a folder name an icon theme may map (no separators or control
/// characters, at most 64 bytes). Whether the installed theme has it is the app's check.
pub fn valid_theme_icon(s: &str) -> bool {
    s.strip_prefix(THEME_ICON).is_some_and(|name| {
        !name.is_empty()
            && name.trim() == name
            && !name.contains(['/', '\\'])
            && crate::sync::plain(name, MAX_THEME_NAME)
    })
}

pub fn valid_icon(s: &str) -> bool {
    valid_theme_icon(s) || valid_emoji(s)
}

impl Library {
    fn style_source(&self, record: &RecordRef) -> Result<Arc<Source>> {
        self.source(&record.source)
            .with_context(|| format!("no source {}", record.source))
    }

    /// A folder's style, None when it has none.
    pub fn folder_style(&self, record: &RecordRef) -> Result<Option<FolderStyle>> {
        let src = self.style_source(record)?;
        let c = src.store.get()?;
        Ok(c.query_row(
            "SELECT color, icon FROM folder_style WHERE record = ?1",
            [record.id],
            |r| {
                Ok(FolderStyle {
                    color: r.get(0)?,
                    icon: r.get(1)?,
                })
            },
        )
        .optional()?)
    }

    /// Sets exactly this style on an indexed folder or file (an empty style clears it) and
    /// logs it for library sync. A file with a color, or a record no longer in the index,
    /// is refused.
    pub fn set_folder_style(&self, record: &RecordRef, style: &FolderStyle) -> Result<()> {
        style.check()?;
        let src = self.style_source(record)?;
        let path = self.set_style_row(&src, record.id, style)?;
        self.log_sync(self.style_change(&src, &path, style).into_iter().collect())
    }

    /// `set_folder_style` without checks of the style or the sync log (a change another
    /// device made): the folder's or file's path in its source.
    pub(crate) fn set_style_row(
        &self,
        src: &Source,
        id: i64,
        style: &FolderStyle,
    ) -> Result<String> {
        let c = src.store.get()?;
        let row: Option<(String, i64)> = c
            .query_row("SELECT path, kind FROM record WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let (path, kind) = row.context("that folder is not in the library index")?;
        ensure!(
            kind == crate::fsid::DIR || style.color.is_none(),
            "only folders take a color"
        );
        if style.is_empty() {
            c.execute("DELETE FROM folder_style WHERE record = ?1", [id])?;
        } else {
            c.execute(
                "INSERT OR REPLACE INTO folder_style(record, color, icon) VALUES (?1, ?2, ?3)",
                params![id, style.color, style.icon],
            )?;
        }
        Ok(path)
    }

    /// Every styled folder and file in every source, by path.
    pub fn folder_styles(&self) -> Result<Vec<(LibraryHit, FolderStyle)>> {
        let sources: Vec<Arc<Source>> = self.shared.sources.read().clone();
        let mut out = Vec::new();
        for src in &sources {
            let c = src.store.get()?;
            let mut stmt = c.prepare(&format!(
                "SELECT {HIT_COLUMNS}, s.color, s.icon FROM folder_style s
                 JOIN record r ON r.id = s.record ORDER BY r.path"
            ))?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    hit_of(src, r, 0.0)?,
                    FolderStyle {
                        color: r.get(6)?,
                        icon: r.get(7)?,
                    },
                ))
            })?;
            out.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(out)
    }
}

#[cfg(test)]
#[path = "styles_tests.rs"]
mod tests;
