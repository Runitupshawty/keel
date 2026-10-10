//! Resumable staging files shared by host writes and Spacedrop (see `WriteAt`).
use crate::WriteAt;
use anyhow::{bail, ensure, Result};
use parking_lot::Mutex;
use std::{
    collections::HashSet,
    io::{Seek, SeekFrom, Write},
    path::PathBuf,
    sync::LazyLock,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt};

/// Pieces up to this size are read whole first, then written in one blocking step (cheap
/// small files); bigger ones stream.
const SMALL: u64 = 256 << 10;

/// Staging files being written now (one writer each).
static WRITING: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Mutex::default);

/// A written piece. Unless `done` is set, dropping it puts the staging file back at the
/// piece's offset (removes it when that is 0), also when the request is dropped.
pub(crate) struct Piece {
    pub path: PathBuf,
    pub offset: u64,
    pub done: bool,
    /// BLAKE3 of the whole staged file, when this piece was all of it (no re-read).
    hash: Option<[u8; 32]>,
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
            let got = match self.hash {
                Some(h) => h,
                None => {
                    let mut h = blake3::Hasher::new();
                    h.update_reader(std::fs::File::open(&self.path)?)?;
                    *h.finalize().as_bytes()
                }
            };
            if got != expect {
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
        hash: None,
    };
    if at.size <= SMALL {
        let mut buf = Vec::with_capacity(at.size as usize);
        (&mut body).take(at.size).read_to_end(&mut buf).await?;
        ensure!(
            buf.len() as u64 == at.size && body.read(&mut [0u8]).await? == 0,
            "body size mismatch"
        );
        return tokio::task::spawn_blocking(move || write_small(piece, &buf, at)).await?;
    }
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
    // Earlier pieces were synced when they were written: an empty one adds nothing to flush.
    if copied > 0 {
        file.sync_all().await?;
    }
    drop(file);
    piece.done = !at.final_;
    Ok(piece)
}

/// `write_piece` for a small piece already in memory (blocking).
fn write_small(mut piece: Piece, buf: &[u8], at: WriteAt) -> Result<Piece> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(at.offset == 0)
        .open(&piece.path)?;
    let len = file.metadata()?.len();
    ensure!(
        len == at.offset,
        "offset {} != staged length {len}",
        at.offset
    );
    piece.done = false;
    file.seek(SeekFrom::Start(at.offset))?;
    file.write_all(buf)?;
    // Earlier pieces were synced when they were written: an empty one adds nothing to flush.
    if !buf.is_empty() {
        file.sync_all()?;
    }
    if at.offset == 0 {
        piece.hash = Some(*blake3::hash(buf).as_bytes());
    }
    piece.done = !at.final_;
    Ok(piece)
}
