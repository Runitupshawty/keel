//! Fuzz entry points (docs/fuzzing.md): one function per target, shared by the libFuzzer
//! targets in `fuzz/` and the bounded runs in `cargo test`. Each must return (Ok or Err)
//! for every input: never panic, hang or allocate by a size the input only declares.

use std::io::{self, Read};
use std::path::Path;

/// Every archive reader: dispatch by magic (else read as a plain tar), the entry list, every
/// body, then the zip rewriter (`zipedit`) deleting, renaming and adding entries.
pub fn archive(data: &[u8]) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("input.tar");
    std::fs::write(&path, data).unwrap();
    let Ok(mut reader) = crate::archive::open_archive(&path) else {
        return;
    };
    let names = match reader.entries() {
        Ok(entries) => entries.into_iter().map(|e| e.inner).collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };
    for name in &names {
        let _ = crate::archive::safe_name(name);
    }
    let _ = reader.visit(&|_| true, &mut |_, body| {
        io::copy(&mut body.take(64 << 20), &mut io::sink())?;
        Ok(())
    });
    #[cfg(feature = "zip")]
    zip_edit(&path, &names);
}

#[cfg(feature = "zip")]
fn zip_edit(path: &Path, names: &[String]) {
    use crate::archive::{safe_name, zipedit};
    if !data_starts(path, b"PK") {
        return;
    }
    let safe: Vec<String> = names.iter().filter_map(|n| safe_name(n).ok()).collect();
    let mut edits = vec![
        zipedit::Edit::Mkdir("new-folder".into()),
        zipedit::Edit::Add {
            name: "added.txt".into(),
            size: 5,
            modified: None,
            open: Box::new(|| Ok(Box::new(io::Cursor::new(b"added".to_vec())) as _)),
        },
    ];
    if let Some(first) = safe.first() {
        edits.push(zipedit::Edit::Rename {
            from: first.clone(),
            to: "renamed".into(),
        });
    }
    if let Some(last) = safe.get(1) {
        edits.push(zipedit::Edit::Delete(last.clone()));
    }
    let staged = zipedit::stage(
        path,
        edits,
        &|_| {},
        &std::sync::atomic::AtomicBool::new(false),
    );
    // What the rewriter wrote must read back.
    if let Ok(staged) = staged {
        if let Some(out) = staged.path() {
            let _ = crate::archive::open_archive(out).and_then(|mut r| r.entries());
        }
    }
}

#[cfg(feature = "zip")]
fn data_starts(path: &Path, magic: &[u8]) -> bool {
    let mut head = vec![0; magic.len()];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok()
        && head == magic
}

/// `~/.ssh/config` and the aliases it names, resolved. Includes that could leave the test
/// folder (absolute or `..` paths) are skipped: they would read the machine's own files.
pub fn ssh_config(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let escapes = text
        .lines()
        .any(|l| l.to_ascii_lowercase().contains("include") && l.contains(['/', '\\', ':', '~']));
    if escapes {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ssh = dir.path().join(".ssh");
    std::fs::create_dir(&ssh).unwrap();
    std::fs::write(ssh.join("config"), data).unwrap();
    std::fs::write(ssh.join("extra"), data).unwrap();
    let resolver =
        crate::sftp::ssh_config::Resolver::new(ssh.join("config"), dir.path(), "local".into());
    let mut aliases = vec!["host".to_owned(), "a".to_owned()];
    aliases.extend(
        text.lines()
            .filter_map(|l| l.trim().strip_prefix("Host "))
            .flat_map(str::split_whitespace)
            .take(4)
            .map(|p| p.trim_start_matches('!').replace(['*', '?'], "x")),
    );
    for alias in aliases {
        let _ = resolver.resolve(&alias);
    }
}

/// A fixed "now" for the listings' year-less dates (a run must reproduce).
fn listing_now() -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000)
}

/// An FTP `LIST` reply as it comes off the data connection: split into bounded lines, each
/// parsed as a UNIX `ls -l` or a DOS (IIS) line.
pub fn ftp_list(data: &[u8]) {
    let Ok(lines) = crate::ftp::list::lines(data) else {
        return;
    };
    for line in &lines {
        let _ = crate::ftp::list::list(line, listing_now());
    }
}

/// An FTP `MLSD` reply (and each line as an `MLST` answer): bounded lines of facts.
pub fn ftp_mlsd(data: &[u8]) {
    let Ok(lines) = crate::ftp::list::lines(data) else {
        return;
    };
    for line in &lines {
        let _ = crate::ftp::list::mlsd(line);
        let _ = crate::ftp::list::mlst(line);
    }
}

/// Deleted-file recovery over an NTFS volume image: the boot sector, the `$MFT` walk
/// (fixups, attributes, data runs), `$Bitmap`, folder paths, then each candidate's data.
pub fn recover_ntfs(data: &[u8]) {
    recover(data, crate::recover::scan_ntfs);
}

/// Deleted-file recovery over an exFAT volume image: entry sets, chains, the bitmap.
pub fn recover_exfat(data: &[u8]) {
    recover(data, crate::recover::scan_exfat);
}

/// Deleted-file recovery over a FAT12/16/32 volume image: folders, long names, the FAT.
pub fn recover_fat(data: &[u8]) {
    recover(data, crate::recover::scan_fat);
}

/// A Recycle Bin `$I` file.
pub fn recycle_bin(data: &[u8]) {
    if let Ok(info) = crate::recover::recycle::parse_info(data) {
        let _ = info.name();
    }
}

type Volume = crate::recover::BlockReader<io::Cursor<Vec<u8>>>;

/// Scans, then reads the first 64 candidates' data into a sink that refuses more than
/// 1 MiB (a sparse run may declare any length).
fn recover(
    data: &[u8],
    scan: impl Fn(&mut Volume) -> crate::recover::Result<crate::recover::Scan>,
) {
    let mut r = crate::recover::BlockReader::new(io::Cursor::new(data.to_vec()));
    let Ok(found) = scan(&mut r) else {
        return;
    };
    for c in found.candidates.iter().take(64) {
        let _ = found.path(c);
        let _ = crate::recover::read_data(&mut r, c, &mut Capped(1 << 20));
    }
}

struct Capped(usize);

impl io::Write for Capped {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if b.len() > self.0 {
            return Err(io::Error::other("over the fuzz cap"));
        }
        self.0 -= b.len();
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::fuzz_harness::{run, Capped};

    #[global_allocator]
    static ALLOC: Capped = Capped;

    #[test]
    fn fuzz_archive() {
        run("archive", super::archive);
    }
    #[test]
    fn fuzz_ssh_config() {
        run("ssh_config", super::ssh_config);
    }
    #[test]
    fn fuzz_ftp_list() {
        run("ftp_list", super::ftp_list);
    }
    #[test]
    fn fuzz_ftp_mlsd() {
        run("ftp_mlsd", super::ftp_mlsd);
    }
    #[test]
    fn fuzz_recover_ntfs() {
        run("recover_ntfs", super::recover_ntfs);
    }
    #[test]
    fn fuzz_recover_exfat() {
        run("recover_exfat", super::recover_exfat);
    }
    #[test]
    fn fuzz_recover_fat() {
        run("recover_fat", super::recover_fat);
    }
    #[test]
    fn fuzz_recycle_bin() {
        run("recycle_bin", super::recycle_bin);
    }

    /// The recovery targets' seeds (`fuzz/corpus/recover_*`), built by the test image
    /// builders: `cargo test -p keel-vfs write_recover_seeds -- --ignored` rewrites them.
    #[test]
    #[ignore]
    fn write_recover_seeds() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus");
        for (target, name, bytes) in crate::recover::testimg::seeds() {
            let at = dir.join(target);
            std::fs::create_dir_all(&at).unwrap();
            std::fs::write(at.join(&name), &bytes).unwrap();
            eprintln!("{target}/{name}: {} bytes", bytes.len());
        }
    }
}
