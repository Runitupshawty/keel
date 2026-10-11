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
}
