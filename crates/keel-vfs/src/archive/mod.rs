//! Archive readers. Everything here blocks: call it off the UI thread. Links and special
//! files inside archives are not listed or extracted.
#[cfg(feature = "rar")]
mod rar;
#[cfg(feature = "sevenz")]
mod sevenz;
#[cfg(feature = "tar")]
mod tar;
#[cfg(feature = "zip")]
mod zip;

use anyhow::{bail, Context, Result};
use std::{
    fs::File,
    io::{self, Read, Write},
    path::Path,
    sync::mpsc,
    time::SystemTime,
};

pub trait ArchiveReader: Send {
    /// Metadata only (regular files and folders, raw archive names); never entry bodies.
    fn entries(&mut self) -> Result<Vec<ArchiveEntry>>;
    /// One entry body by its raw name, streaming.
    fn read(&mut self, inner: &str) -> Result<Box<dyn Read + Send>>;
    /// Streams every regular file whose raw name `want` accepts, in archive order. Formats
    /// that decode sequentially (tar, 7z, rar) override this so extraction is one pass.
    fn visit(
        &mut self,
        want: &dyn Fn(&str) -> bool,
        each: &mut dyn FnMut(&str, &mut dyn Read) -> Result<()>,
    ) -> Result<()> {
        for entry in self.entries()? {
            if !entry.is_dir && want(&entry.inner) {
                each(&entry.inner, &mut self.read(&entry.inner)?)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ArchiveEntry {
    /// The raw name as stored; `safe_name` normalises it.
    pub inner: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub encrypted: bool,
}

/// Dispatch by magic bytes, then by extension.
pub fn open_archive(local: &Path) -> Result<Box<dyn ArchiveReader>> {
    let mut magic = [0; 512];
    let mut file = File::open(local).with_context(|| format!("open {}", local.display()))?;
    let n = read_up_to(&mut file, &mut magic)?;
    let magic = &magic[..n];
    let name = local.to_string_lossy().to_ascii_lowercase();
    #[cfg(feature = "zip")]
    if magic.starts_with(b"PK\x03\x04") || magic.starts_with(b"PK\x05\x06") {
        return Ok(Box::new(zip::Reader::open(local)?));
    }
    #[cfg(feature = "sevenz")]
    if magic.starts_with(b"7z\xbc\xaf\x27\x1c") {
        return Ok(Box::new(sevenz::Reader::open(local)?));
    }
    #[cfg(feature = "rar")]
    if magic.starts_with(b"Rar!\x1a\x07") {
        return Ok(Box::new(rar::Reader::open(local)?));
    }
    #[cfg(feature = "tar")]
    {
        use tar::Compression::*;
        let compression = if magic.starts_with(&[0x1f, 0x8b]) {
            Some(Gzip)
        } else if magic.starts_with(b"BZh") {
            Some(Bzip2)
        } else if magic.starts_with(b"\xfd7zXZ\0") {
            Some(Xz)
        } else if magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            Some(Zstd)
        } else if magic.get(257..262) == Some(b"ustar") {
            Some(Plain)
        } else {
            None
        };
        if let Some(compression) = compression {
            return Ok(Box::new(tar::Reader::new(local, compression)));
        }
    }
    // No known magic (e.g. an old v7 tar, or a truncated file): fall back to the extension.
    #[cfg(feature = "zip")]
    if name.ends_with(".zip") || name.ends_with(".jar") {
        return Ok(Box::new(zip::Reader::open(local)?));
    }
    #[cfg(feature = "sevenz")]
    if name.ends_with(".7z") {
        return Ok(Box::new(sevenz::Reader::open(local)?));
    }
    #[cfg(feature = "rar")]
    if name.ends_with(".rar") {
        return Ok(Box::new(rar::Reader::open(local)?));
    }
    #[cfg(feature = "tar")]
    if name.ends_with(".tar") {
        return Ok(Box::new(tar::Reader::new(local, tar::Compression::Plain)));
    }
    let _ = (magic, name);
    bail!("unsupported or corrupt archive: {}", local.display())
}

fn read_up_to(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Runs `work` on a thread and reads what it writes. At most two 64 KiB chunks are in
/// flight; dropping the reader stops the producer on its next write; errors arrive as IO
/// errors.
#[cfg_attr(
    not(any(feature = "zip", feature = "tar", feature = "sevenz")),
    allow(dead_code)
)]
fn stream(
    work: impl FnOnce(&mut dyn Write) -> Result<()> + Send + 'static,
) -> Box<dyn Read + Send> {
    let (tx, rx) = mpsc::sync_channel(2);
    std::thread::spawn(move || {
        let mut output = StreamWriter(tx.clone());
        if let Err(error) = work(&mut output) {
            let _ = tx.send(Err(io::Error::other(format!("{error:#}"))));
        }
    });
    Box::new(StreamReader {
        rx,
        pending: io::Cursor::new(Vec::new()),
    })
}
struct StreamWriter(mpsc::SyncSender<io::Result<Vec<u8>>>);
impl Write for StreamWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = bytes.len().min(65536);
        self.0
            .send(Ok(bytes[..n].to_vec()))
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct StreamReader {
    rx: mpsc::Receiver<io::Result<Vec<u8>>>,
    pending: io::Cursor<Vec<u8>>,
}
impl Read for StreamReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        loop {
            let n = self.pending.read(bytes)?;
            if n > 0 {
                return Ok(n);
            }
            match self.rx.recv() {
                Ok(chunk) => self.pending = io::Cursor::new(chunk?),
                Err(_) => return Ok(0),
            }
        }
    }
}

/// Normalises an archive name to `a/b/c` (`\` and `/` both separate, `.` and empty parts
/// dropped) and refuses anything that could leave an extraction folder: `..`, absolute
/// paths, drive letters and ADS (`:`), NUL. A deliberately portable subset: Windows-only
/// hazards (trailing dots/spaces, CON/NUL/COM1…) are refused on every OS, so an archive
/// extracted anywhere stays safe to copy to Windows.
pub fn safe_name(name: &str) -> Result<String> {
    let name = name.replace('\\', "/");
    anyhow::ensure!(
        !name.starts_with('/') && !name.contains([':', '\0']),
        "unsafe archive path: {name}"
    );
    let parts: Vec<&str> = name
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    anyhow::ensure!(!parts.is_empty(), "empty archive path");
    for part in &parts {
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit());
        anyhow::ensure!(
            *part != ".." && !part.ends_with(['.', ' ']) && !device,
            "unsafe archive path: {name}"
        );
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::safe_name;
    #[test]
    fn safe_names() {
        assert_eq!(safe_name("./a//b/").unwrap(), "a/b");
        assert_eq!(safe_name("dir\\x.txt").unwrap(), "dir/x.txt");
        for bad in [
            "../x",
            "a/../../x",
            "/etc/x",
            "C:/x",
            "C:x",
            "a/b:s",
            "\\\\srv\\x",
            "a/CON.txt",
            "a/b.",
            "a/b ",
            ".",
            "",
            "lpt1",
        ] {
            assert!(safe_name(bad).is_err(), "{bad}");
        }
    }
}
