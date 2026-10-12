//! Comments (Task 67): Get Info's Comments box, kept in the source's store by record
//! (`record_comment`, source schema 13), so a comment follows its file through the moves and
//! renames the index records and goes when the file leaves the index. Comments stay on this
//! device (library sync does not carry them) and in the library's backups (the store is
//! backed up whole). Nothing here touches a file.

use crate::library::RecordRef;
use crate::Library;
use anyhow::{ensure, Context, Result};
use rusqlite::OptionalExtension;

/// Longest comment, in bytes.
pub const MAX_COMMENT: usize = 64 * 1024;

impl Library {
    /// The comment on an indexed file or folder (None: no comment).
    pub fn comment(&self, record: &RecordRef) -> Result<Option<String>> {
        let src = (self.source(&record.source))
            .with_context(|| format!("no source {}", record.source))?;
        let c = src.store.get()?;
        Ok(c.query_row(
            "SELECT text FROM record_comment WHERE record = ?1",
            [record.id],
            |r| r.get(0),
        )
        .optional()?)
    }

    /// Sets the comment on an indexed file or folder; blank text clears it. A record no
    /// longer in the index is refused.
    pub fn set_comment(&self, record: &RecordRef, text: &str) -> Result<()> {
        ensure!(
            text.len() <= MAX_COMMENT,
            "a comment is at most {} KiB",
            MAX_COMMENT / 1024
        );
        let src = (self.source(&record.source))
            .with_context(|| format!("no source {}", record.source))?;
        let c = src.store.get()?;
        let known: Option<i64> = c
            .query_row("SELECT id FROM record WHERE id = ?1", [record.id], |r| {
                r.get(0)
            })
            .optional()?;
        known.context("that item is not in the library index")?;
        if text.trim().is_empty() {
            c.execute("DELETE FROM record_comment WHERE record = ?1", [record.id])?;
        } else {
            c.execute(
                "INSERT OR REPLACE INTO record_comment(record, text) VALUES (?1, ?2)",
                rusqlite::params![record.id, text],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "comments_tests.rs"]
mod tests;
