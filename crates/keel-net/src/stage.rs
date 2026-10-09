//! Resumable staging files shared by host writes and Spacedrop (see `WriteAt`).
use crate::WriteAt;
use anyhow::{bail, ensure, Result};
use parking_lot::Mutex;
use std::{collections::HashSet, io::SeekFrom, path::PathBuf, sync::LazyLock};
use tokio::io::{AsyncRead, AsyncSeekExt};

/// Staging files being written now (one writer each).
static WRITING: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Mutex::default);

/// A written piece. Unless `done` is set, dropping it puts the staging file back at the
/// piece's offset (removes it when that is 0), also when the request is dropped.
pub(crate) struct Piece {
    pub path: PathBuf,
    pub offset: u64,
    pub done: bool,
}
impl Drop for Piece {
    fn drop(&mut self) {
        if !self.done {
            if self.offset == 0 {
                let _ = std::fs::remove_file(&self.path);
            } else if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&self.path) {
                let _ = f.set_len(self.offset);
            }
        }
        WRITING.lock().remove(&self.path);
    }
}
impl Piece {
    /// Blocking: the staging file's BLAKE3 must be `expect`; else it is dropped whole.
    pub fn verify(&mut self, expect: Option<[u8; 32]>) -> Result<()> {
        if let Some(expect) = expect {
            let mut h = blake3::Hasher::new();
            h.update_reader(std::fs::File::open(&self.path)?)?;
            if h.finalize().as_bytes() != &expect {
                self.offset = 0;
                bail!("content check failed");
            }
        }
        Ok(())
    }
}

/// Appends `body` (exactly `at.size` bytes) to `staging` at `at.offset`, which must be
/// its length (0 starts over). A non-final piece comes back done; a final one stays
/// armed for the caller to verify and publish (then set `done`).
pub(crate) async fn write_piece(
    staging: PathBuf,
    mut body: Box<dyn AsyncRead + Send + Unpin>,
    at: WriteAt,
) -> Result<Piece> {
    ensure!(
        WRITING.lock().insert(staging.clone()),
        "this file is being written already"
    );
    // Armed (done = false) once the piece is known to start at the staged end.
    let mut piece = Piece {
        path: staging,
        offset: at.offset,
        done: true,
    };
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(at.offset == 0)
        .open(&piece.path)
        .await?;
    let len = file.metadata().await?.len();
    ensure!(
        len == at.offset,
        "offset {} != staged length {len}",
        at.offset
    );
    piece.done = false;
    file.seek(SeekFrom::Start(at.offset)).await?;
    let copied = tokio::io::copy(&mut body, &mut file).await?;
    ensure!(copied == at.size, "body size mismatch");
    file.sync_all().await?;
    drop(file);
    piece.done = !at.final_;
    Ok(piece)
}
