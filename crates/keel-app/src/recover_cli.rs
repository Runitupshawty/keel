//! `keel recover scan <drive-or-image>` and `keel recover restore <id>... --to <dir>`
//! (Phase 17 Task 85): deleted-file recovery from the command line, through
//! `keel_vfs::recover`. A scan lists the deleted files a drive, device or volume image
//! still describes (a Recycle Bin folder lists its items) and remembers the list in
//! `<data dir>/recover/last-scan.json`; restore writes files from that list into a
//! folder on another drive, never onto the drive scanned. Reading a drive directly needs
//! administrator rights (root for a device elsewhere).

use crate::cli::RecoverCmd;
use crate::commands::{FAILED, OK, USAGE};
use keel_vfs::recover::{
    self, recycle, Candidate, Confidence, RecoverError, Scan, Source, VolumeId,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};

/// The last scan, for `restore`.
#[derive(Serialize, Deserialize)]
struct Saved {
    /// What was opened: a drive (`E:`) or an absolute path.
    source: String,
    volume: VolumeId,
    scan: Scan,
}

pub fn run(cmd: RecoverCmd, json: bool) -> i32 {
    let result = match cmd {
        RecoverCmd::Scan { source } => scan(&source, json),
        RecoverCmd::Restore { ids, to } => restore(&ids, Path::new(&to), json),
    };
    match result {
        Ok(code) => code,
        Err(Fail::Usage(msg)) => {
            eprintln!("keel: {msg}");
            USAGE
        }
        Err(Fail::Recover(e)) => {
            if json {
                println!("{}", json!({ "error": { "message": e.to_string() } }));
            }
            eprintln!("keel: {e}");
            FAILED
        }
    }
}

enum Fail {
    Usage(String),
    Recover(RecoverError),
}

impl From<RecoverError> for Fail {
    fn from(e: RecoverError) -> Self {
        Fail::Recover(e)
    }
}

impl From<std::io::Error> for Fail {
    fn from(e: std::io::Error) -> Self {
        Fail::Recover(e.into())
    }
}

fn last_scan_file() -> Result<PathBuf, Fail> {
    crate::settings::data_dir()
        .map(|d| d.join("recover").join("last-scan.json"))
        .ok_or_else(|| Fail::Usage("no data folder for this user".into()))
}

/// A drive as `E:`; anything else as an absolute path (restore may run elsewhere).
fn source_spec(source: &str) -> String {
    match recover::drive_letter(source) {
        Some(letter) if cfg!(windows) => format!("{letter}:"),
        _ => std::path::absolute(source)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| source.to_owned()),
    }
}

fn scan(source: &str, json: bool) -> Result<i32, Fail> {
    let spec = source_spec(source);
    let is_drive = cfg!(windows) && recover::drive_letter(&spec).is_some();
    if !is_drive && Path::new(&spec).is_dir() {
        let items = recycle::list(Path::new(&spec))?;
        match json {
            true => println!("{}", json!({ "recycle_bin": recycle_json(&items) })),
            false => print_recycle(&items, true),
        }
        return Ok(OK);
    }
    let mut src = Source::open(&spec)?;
    let found = recover::scan(src.reader())?;
    // The drive's own Recycle Bin, as far as this user may read it.
    let bin = match is_drive {
        true => {
            recycle::list(&Path::new(&format!("{spec}\\")).join("$Recycle.Bin")).unwrap_or_default()
        }
        false => Vec::new(),
    };
    let saved = Saved {
        source: spec,
        volume: src.id,
        scan: found,
    };
    let file = last_scan_file()?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_vec(&saved)
        .map_err(|e| Fail::Recover(RecoverError::Unrecoverable(e.to_string())))?;
    std::fs::write(&file, text)?;
    print_scan(&saved, &bin, json);
    Ok(OK)
}

fn time_text(t: Option<i64>, local: bool) -> String {
    let Some(at) = t.and_then(|t| chrono::DateTime::from_timestamp(t, 0)) else {
        return String::new();
    };
    match local {
        true => at.naive_utc().format("%Y-%m-%d %H:%M").to_string(),
        false => at
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string(),
    }
}

fn candidate_json(id: usize, c: &Candidate) -> serde_json::Value {
    json!({
        "id": id,
        "name": c.name,
        "path": c.path(),
        "folder_known": c.folder.is_some(),
        "first_char_unknown": c.first_char_unknown,
        "size": c.size,
        "created": c.created,
        "modified": c.modified,
        "accessed": c.accessed,
        "local_times": c.local_times,
        "confidence": c.confidence,
    })
}

/// The scan (and the drive's Recycle Bin items): one JSON document with `json`.
fn print_scan(saved: &Saved, bin: &[recycle::Item], json: bool) {
    let s = &saved.scan;
    if json {
        let files: Vec<_> = s
            .candidates
            .iter()
            .enumerate()
            .map(|(i, c)| candidate_json(i + 1, c))
            .collect();
        println!(
            "{}",
            json!({
                "source": saved.source,
                "fs": s.fs,
                "cluster_size": s.cluster_size,
                "volume_size": s.volume_size,
                "files": files,
                "notes": s.notes,
                "recycle_bin": recycle_json(bin),
            })
        );
        return;
    }
    println!(
        "{} on {}: {} deleted file{} found",
        s.fs.label(),
        saved.source,
        s.candidates.len(),
        if s.candidates.len() == 1 { "" } else { "s" }
    );
    if !s.candidates.is_empty() {
        println!(
            "{:>7}  {:<11}  {:>10}  {:<16}  PATH",
            "ID", "CONFIDENCE", "SIZE", "MODIFIED"
        );
    }
    for (i, c) in s.candidates.iter().enumerate() {
        println!(
            "{:>7}  {:<11}  {:>10}  {:<16}  {}",
            i + 1,
            c.confidence.label(),
            crate::view_details::size_text_of(c.size),
            time_text(c.modified, c.local_times),
            c.path(),
        );
    }
    for note in &s.notes {
        println!("Note: {note}");
    }
    if s.candidates.iter().any(|c| c.first_char_unknown) {
        println!("Note: a FAT name without a long name lost its first character (shown as _).");
    }
    if !s.candidates.is_empty() {
        println!("Restore with `keel recover restore <ID>... --to <folder on another drive>`.");
    }
    if !bin.is_empty() {
        print_recycle(bin, false);
    }
}

fn recycle_json(items: &[recycle::Item]) -> serde_json::Value {
    items
        .iter()
        .map(|i| {
            json!({
                "original": i.info.original,
                "size": i.info.size,
                "deleted": i.info.deleted,
                "info_file": i.info_file,
                "data_file": i.data_file,
            })
        })
        .collect()
}

fn print_recycle(items: &[recycle::Item], alone: bool) {
    if alone || !items.is_empty() {
        println!(
            "Recycle Bin: {} item{}",
            items.len(),
            if items.len() == 1 { "" } else { "s" }
        );
    }
    for i in items {
        println!(
            "  {:<16}  {:>10}  {}{}",
            time_text(i.info.deleted, false),
            crate::view_details::size_text_of(i.info.size),
            i.info.original,
            if i.data_file.is_none() {
                "  (its data file is gone)"
            } else {
                ""
            },
        );
    }
}

fn restore(ids: &[usize], to: &Path, json: bool) -> Result<i32, Fail> {
    let file = last_scan_file()?;
    let text = std::fs::read(&file).map_err(|_| {
        Fail::Usage("no scan to restore from: run `keel recover scan` first".into())
    })?;
    let saved: Saved = serde_json::from_slice(&text).map_err(|_| {
        Fail::Usage("the saved scan is unreadable: run `keel recover scan` again".into())
    })?;
    let count = saved.scan.candidates.len();
    if let Some(bad) = ids.iter().find(|&&id| id == 0 || id > count) {
        return Err(Fail::Usage(format!(
            "no file {bad} in the last scan of {} (ids 1 to {count})",
            saved.source
        )));
    }
    if !to.is_dir() {
        return Err(Fail::Usage(format!(
            "{} is not a folder: give an existing folder on another drive",
            to.display()
        )));
    }
    let mut src = Source::open(&saved.source)?;
    if src.id != saved.volume {
        return Err(Fail::Usage(format!(
            "{} is not the volume scanned: run `keel recover scan` again",
            saved.source
        )));
    }
    recover::check_destination(src.id, to)?;
    let mut failed = false;
    let mut done = Vec::new();
    for &id in ids {
        let c = &saved.scan.candidates[id - 1];
        let id_ = src.id;
        match recover::restore(src.reader(), id_, c, to) {
            Ok(path) => {
                if !json {
                    println!(
                        "Restored {} to {} ({})",
                        c.path(),
                        path.display(),
                        c.confidence.label()
                    );
                    if c.confidence != Confidence::Good {
                        println!(
                            "  Its clusters were {}: parts of it may be other data.",
                            match c.confidence {
                                Confidence::Unknown => "not all known to be free",
                                _ => "partly or wholly reused",
                            }
                        );
                    }
                }
                done.push(json!({ "id": id, "path": path, "confidence": c.confidence }));
            }
            Err(e @ RecoverError::SameVolume) => return Err(e.into()),
            Err(e) => {
                failed = true;
                eprintln!("keel: {}: {e}", c.path());
                done.push(json!({ "id": id, "error": e.to_string() }));
            }
        }
    }
    if json {
        println!("{}", json!({ "restored": done }));
    }
    Ok(if failed { FAILED } else { OK })
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_vfs::recover::testimg::{NtfsImg, NT_ROOT};

    #[test]
    fn scan_saves_the_list_and_restore_checks_ids_folder_and_volume() {
        let dir = tempfile::tempdir().unwrap();
        let mut nt = NtfsImg::new();
        nt.add_file(NT_ROOT, "kept.txt", None, b"kept", false);
        let gone = nt.add_file(NT_ROOT, "notes.txt", None, &[b'n'; 3000], false);
        nt.delete(gone);
        let image = dir.path().join("volume.img");
        std::fs::write(&image, nt.image()).unwrap();
        assert_eq!(scan(image.to_str().unwrap(), false).ok(), Some(OK));
        let saved: Saved =
            serde_json::from_slice(&std::fs::read(last_scan_file().ok().unwrap()).unwrap())
                .unwrap();
        assert_eq!(
            saved.source,
            std::path::absolute(&image).unwrap().display().to_string()
        );
        assert_eq!(saved.scan.candidates.len(), 1);
        assert_eq!(saved.scan.candidates[0].name, "notes.txt");

        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        assert!(matches!(restore(&[2], &out, false), Err(Fail::Usage(_))));
        assert!(matches!(restore(&[0], &out, false), Err(Fail::Usage(_))));
        let missing = dir.path().join("missing");
        assert!(matches!(
            restore(&[1], &missing, false),
            Err(Fail::Usage(_))
        ));
        // The image is on the same volume as the temp folder: refused, nothing written.
        assert!(matches!(
            restore(&[1], &out, false),
            Err(Fail::Recover(RecoverError::SameVolume))
        ));
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);

        // A folder lists its Recycle Bin items; garbage is not a volume.
        assert_eq!(scan(out.to_str().unwrap(), true).ok(), Some(OK));
        let junk = dir.path().join("junk.img");
        std::fs::write(&junk, [0u8; 4096]).unwrap();
        assert!(matches!(
            scan(junk.to_str().unwrap(), false),
            Err(Fail::Recover(RecoverError::UnknownFs))
        ));
    }
}
