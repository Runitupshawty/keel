//! `keel recover scan <drive-or-image>` and `keel recover restore <id>... --to <dir>`
//! (Phase 17 Task 85): deleted-file recovery from the command line, through
//! `keel_vfs::recover`. A scan lists the deleted files a drive, device or volume image
//! still describes (a Recycle Bin folder lists its items) and saves the list for restore
//! (`--save <file>`, else `<data dir>/recover/last-scan.json`, else a folder of this
//! user's in the temporary folder), never on the drive being scanned: a file written
//! there could take the very clusters a deleted file still holds. Restore writes files
//! from that list into a folder on another drive, never onto the drive scanned, after
//! reading the drive again: the saved list only names the files (where they were found,
//! name, size), so a list someone else edited can only pick files that are really there.
//! Reading a drive directly needs administrator rights (root for a device elsewhere).
//! Names from the volume are shown with control characters escaped.

use crate::cli::RecoverCmd;
use crate::commands::{FAILED, OK, USAGE};
use keel_vfs::recover::{
    self, recycle, Candidate, Confidence, FsKind, RecoverError, Scan, Source, VolumeId,
};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::json;
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The saved list's format; another one is refused (scan again).
const SAVED_VERSION: u32 = 2;
const SCAN_FILE: &str = "last-scan.json";

/// A scan as saved for `restore`: the volume and, per id, what finds the file again.
#[derive(Serialize, Deserialize)]
struct Saved {
    version: u32,
    /// What was opened: a drive (`E:`) or an absolute path.
    source: String,
    volume: VolumeId,
    fs: FsKind,
    files: Vec<SavedFile>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct SavedFile {
    /// The MFT record (NTFS) or the directory entry's byte offset (FAT, exFAT).
    location: u64,
    name: String,
    size: u64,
}

/// [`Saved`] written straight from a scan (no copy of the list).
#[derive(Serialize)]
struct SavedRef<'a> {
    version: u32,
    source: &'a str,
    volume: VolumeId,
    fs: FsKind,
    files: SavedFiles<'a>,
}

struct SavedFiles<'a>(&'a [Candidate]);

impl Serialize for SavedFiles<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct One<'a> {
            location: u64,
            name: &'a str,
            size: u64,
        }
        s.collect_seq(self.0.iter().map(|c| One {
            location: c.location,
            name: &c.name,
            size: c.size,
        }))
    }
}

pub fn run(cmd: RecoverCmd, json: bool) -> i32 {
    let result = match cmd {
        RecoverCmd::Scan { source, save } => scan(&source, save.as_deref().map(Path::new), json),
        RecoverCmd::Restore { ids, to, from } => {
            restore(&ids, Path::new(&to), from.as_deref().map(Path::new), json)
        }
    };
    match result {
        Ok(code) => code,
        Err(Fail::Usage(msg)) => {
            eprintln!("keel: {}", shown(&msg));
            USAGE
        }
        Err(Fail::Recover(e)) => {
            if json {
                println!("{}", json!({ "error": { "message": e.to_string() } }));
            }
            eprintln!("keel: {}", shown(&e.to_string()));
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

/// `s` with control characters (C0, DEL, C1) and bidirectional formatting characters
/// written as `\u{..}`: a name from a scanned volume cannot move the cursor, retitle the
/// window, set the clipboard (OSC 52) or reorder what the line shows.
fn shown(s: &str) -> Cow<'_, str> {
    fn unsafe_char(c: char) -> bool {
        c.is_control()
            || matches!(
                c,
                '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
            )
    }
    if !s.chars().any(unsafe_char) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match unsafe_char(c) {
            true => out.extend(c.escape_unicode()),
            false => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// The default places of the saved list, in order: the data folder, then a folder of
/// this user's among the temporary ones (Windows `%TEMP%`, macOS `$TMPDIR`,
/// `$XDG_RUNTIME_DIR` elsewhere: never a folder other users share).
fn default_places() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(d) = crate::settings::data_dir() {
        out.push(d.join("recover").join(SCAN_FILE));
    }
    if let Some(t) = temp_place() {
        out.push(t);
    }
    out
}

fn temp_place() -> Option<PathBuf> {
    #[cfg(test)]
    let base = Some(crate::settings::test_dir().join("tmp"));
    #[cfg(all(not(test), any(windows, target_os = "macos")))]
    let base = Some(std::env::temp_dir());
    #[cfg(all(not(test), not(any(windows, target_os = "macos"))))]
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    base.map(|b| b.join("keel-recover").join(SCAN_FILE))
}

/// The nearest folder of `path` that exists (where a new one would be made).
fn existing_ancestor(path: &Path) -> Option<&Path> {
    path.ancestors().find(|p| p.is_dir())
}

/// Whether a file at `path` would be written on the `source` volume (unknown counts as
/// yes).
fn on_volume(path: &Path, source: VolumeId) -> bool {
    existing_ancestor(path)
        .and_then(|d| recover::volume_of(d).ok())
        .is_none_or(|v| v == source)
}

/// Where to save a scan of `source`: `--save`, else the first default place not on it.
/// The volume matters only for a drive or device: writing a file beside an image file
/// cannot change the image. None: nowhere safe (the list is shown, not saved).
fn save_place(
    source: VolumeId,
    device: bool,
    explicit: Option<&Path>,
) -> Result<Option<PathBuf>, Fail> {
    if let Some(file) = explicit {
        let file = std::path::absolute(file)?;
        let dir = file.parent().filter(|d| d.is_dir()).ok_or_else(|| {
            Fail::Usage(format!(
                "--save {}: give a file in an existing folder",
                file.display()
            ))
        })?;
        if device && on_volume(dir, source) {
            return Err(Fail::Usage(format!(
                "--save {}: that is on the drive being recovered; save the list on another drive",
                file.display()
            )));
        }
        return Ok(Some(file));
    }
    for file in default_places() {
        let Some(dir) = file.parent() else {
            continue;
        };
        // Checked before the folder is made: making it is a write too.
        if device && on_volume(dir, source) {
            continue;
        }
        if make_private_dir(dir).is_ok() {
            return Ok(Some(file));
        }
    }
    Ok(None)
}

#[cfg(unix)]
fn make_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn make_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

fn write_saved(file: &Path, source: &str, volume: VolumeId, scan: &Scan) -> Result<(), Fail> {
    let saved = SavedRef {
        version: SAVED_VERSION,
        source,
        volume,
        fs: scan.fs,
        files: SavedFiles(&scan.candidates),
    };
    let mut out = std::io::BufWriter::new(std::fs::File::create(file)?);
    serde_json::to_writer(&mut out, &saved)
        .map_err(|e| Fail::Recover(RecoverError::Unrecoverable(e.to_string())))?;
    out.flush()?;
    Ok(())
}

/// The most recent list among the default places.
fn last_saved() -> Option<PathBuf> {
    default_places()
        .into_iter()
        .filter_map(|p| {
            let t = std::fs::metadata(&p).ok()?.modified().ok()?;
            Some((t, p))
        })
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
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

fn scan(source: &str, save: Option<&Path>, json: bool) -> Result<i32, Fail> {
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
    // Decided before reading: a refused --save costs no scan.
    let place = save_place(src.id, src.device, save)?;
    let found = recover::scan(src.reader())?;
    // The drive's own Recycle Bin, as far as this user may read it.
    let bin = match is_drive {
        true => {
            recycle::list(&Path::new(&format!("{spec}\\")).join("$Recycle.Bin")).unwrap_or_default()
        }
        false => Vec::new(),
    };
    if let Some(file) = &place {
        write_saved(file, &spec, src.id, &found)?;
    }
    print_scan(&spec, &found, &bin, place.as_deref(), json);
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

fn candidate_json(scan: &Scan, id: usize, c: &Candidate) -> serde_json::Value {
    json!({
        "id": id,
        "name": c.name,
        "path": scan.path(c),
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

/// A scan's `files` for JSON output, one candidate at a time.
struct FilesJson<'a>(&'a Scan);

impl Serialize for FilesJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let scan = self.0;
        s.collect_seq(
            scan.candidates
                .iter()
                .enumerate()
                .map(|(i, c)| candidate_json(scan, i + 1, c)),
        )
    }
}

#[derive(Serialize)]
struct ScanJson<'a> {
    source: &'a str,
    fs: FsKind,
    cluster_size: u64,
    volume_size: u64,
    files: FilesJson<'a>,
    notes: &'a [String],
    recycle_bin: serde_json::Value,
    /// Where the list was saved for restore; null when it was not.
    saved: Option<&'a Path>,
}

/// The scan (and the drive's Recycle Bin items): one JSON document with `json`, written
/// as it goes.
fn print_scan(
    source: &str,
    s: &Scan,
    bin: &[recycle::Item],
    saved: Option<&Path>,
    json: bool,
) {
    if json {
        let doc = ScanJson {
            source,
            fs: s.fs,
            cluster_size: s.cluster_size,
            volume_size: s.volume_size,
            files: FilesJson(s),
            notes: &s.notes,
            recycle_bin: recycle_json(bin),
            saved,
        };
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        let _ = serde_json::to_writer(&mut out, &doc);
        let _ = writeln!(out);
        return;
    }
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    let _ = writeln!(
        out,
        "{} on {}: {} deleted file{} found",
        s.fs.label(),
        shown(source),
        s.candidates.len(),
        if s.candidates.len() == 1 { "" } else { "s" }
    );
    if !s.candidates.is_empty() {
        let _ = writeln!(
            out,
            "{:>7}  {:<11}  {:>10}  {:<16}  PATH",
            "ID", "CONFIDENCE", "SIZE", "MODIFIED"
        );
    }
    for (i, c) in s.candidates.iter().enumerate() {
        let _ = writeln!(
            out,
            "{:>7}  {:<11}  {:>10}  {:<16}  {}",
            i + 1,
            c.confidence.label(),
            crate::view_details::size_text_of(c.size),
            time_text(c.modified, c.local_times),
            shown(&s.path(c)),
        );
    }
    for note in &s.notes {
        let _ = writeln!(out, "Note: {note}");
    }
    if s.candidates.iter().any(|c| c.first_char_unknown) {
        let _ = writeln!(
            out,
            "Note: a FAT name without a long name lost its first character (shown as _)."
        );
    }
    if !s.candidates.is_empty() {
        match saved {
            Some(_) => {
                let _ = writeln!(
                    out,
                    "Restore with `keel recover restore <ID>... --to <folder on another drive>`."
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "Note: the list was not saved: Keel's data folder and temporary folder are on this drive, and writing there could overwrite deleted data. To restore, scan again with `--save <file on another drive>`, then `keel recover restore <ID>... --from <that file> --to <folder on another drive>`."
                );
            }
        }
    }
    drop(out);
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
            shown(&i.info.original),
            if i.data_file.is_none() {
                "  (its data file is gone)"
            } else {
                ""
            },
        );
    }
}

/// The file of `fresh` (the volume read again) a saved entry names: the same location,
/// name and size, or None when it is no longer there as scanned.
fn find_again<'a>(
    fresh: &'a Scan,
    at: &HashMap<u64, usize>,
    want: &SavedFile,
) -> Option<&'a Candidate> {
    at.get(&want.location)
        .map(|&i| &fresh.candidates[i])
        .filter(|c| c.name == want.name && c.size == want.size)
}

fn restore(ids: &[usize], to: &Path, from: Option<&Path>, json: bool) -> Result<i32, Fail> {
    let file = match from {
        Some(f) => f.to_owned(),
        None => last_saved().ok_or_else(|| {
            Fail::Usage("no scan to restore from: run `keel recover scan` first".into())
        })?,
    };
    let text = std::fs::read(&file).map_err(|e| {
        Fail::Usage(format!(
            "{}: {e}: run `keel recover scan` first",
            file.display()
        ))
    })?;
    let saved: Saved = serde_json::from_slice(&text)
        .ok()
        .filter(|s: &Saved| s.version == SAVED_VERSION)
        .ok_or_else(|| {
            Fail::Usage(format!(
                "{} is not a scan this Keel can read: run `keel recover scan` again",
                file.display()
            ))
        })?;
    let count = saved.files.len();
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
    // The saved list only names the files: where their data is comes from reading the
    // volume again here, so an edited list can only pick files that are really there.
    if !json {
        eprintln!("Reading {} again to find the files...", shown(&saved.source));
    }
    let fresh = recover::scan(src.reader())?;
    if fresh.fs != saved.fs {
        return Err(Fail::Usage(format!(
            "{} is not the volume scanned: run `keel recover scan` again",
            saved.source
        )));
    }
    let at: HashMap<u64, usize> = fresh
        .candidates
        .iter()
        .enumerate()
        .map(|(i, c)| (c.location, i))
        .collect();
    let mut failed = false;
    let mut done = Vec::new();
    for &id in ids {
        let want = &saved.files[id - 1];
        let Some(c) = find_again(&fresh, &at, want) else {
            failed = true;
            let why = format!(
                "file {id} ({}) is no longer on {} as scanned: run `keel recover scan` again",
                want.name, saved.source
            );
            eprintln!("keel: {}", shown(&why));
            done.push(json!({ "id": id, "error": why }));
            continue;
        };
        let source = src.id;
        match recover::restore(src.reader(), source, c, to) {
            Ok(path) => {
                if !json {
                    println!(
                        "Restored {} to {} ({})",
                        shown(&fresh.path(c)),
                        shown(&path.display().to_string()),
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
                eprintln!("keel: {}: {}", shown(&fresh.path(c)), shown(&e.to_string()));
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
        assert_eq!(scan(image.to_str().unwrap(), None, false).ok(), Some(OK));
        let file = last_saved().unwrap();
        assert_eq!(file, default_places()[0], "an image: saved in the data folder");
        let saved: Saved = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(
            saved.source,
            std::path::absolute(&image).unwrap().display().to_string()
        );
        assert_eq!(saved.version, SAVED_VERSION);
        assert_eq!(saved.fs, FsKind::Ntfs);
        assert_eq!(saved.files.len(), 1);
        assert_eq!(saved.files[0].name, "notes.txt");
        assert_eq!(saved.files[0].size, 3000);

        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        assert!(matches!(restore(&[2], &out, None, false), Err(Fail::Usage(_))));
        assert!(matches!(restore(&[0], &out, None, false), Err(Fail::Usage(_))));
        let missing = dir.path().join("missing");
        assert!(matches!(
            restore(&[1], &missing, None, false),
            Err(Fail::Usage(_))
        ));
        // The image is on the same volume as the temp folder: refused, nothing written.
        assert!(matches!(
            restore(&[1], &out, None, false),
            Err(Fail::Recover(RecoverError::SameVolume))
        ));
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
        // --save and --from: another list file; garbage there is refused.
        let other = dir.path().join("scan.json");
        assert_eq!(
            scan(image.to_str().unwrap(), Some(&other), true).ok(),
            Some(OK)
        );
        assert!(matches!(
            restore(&[1], &out, Some(&other), false),
            Err(Fail::Recover(RecoverError::SameVolume))
        ));
        std::fs::write(&other, b"{\"version\": 1}").unwrap();
        assert!(matches!(
            restore(&[1], &out, Some(&other), false),
            Err(Fail::Usage(_))
        ));

        // A folder lists its Recycle Bin items; garbage is not a volume.
        assert_eq!(scan(out.to_str().unwrap(), None, true).ok(), Some(OK));
        let junk = dir.path().join("junk.img");
        std::fs::write(&junk, [0u8; 4096]).unwrap();
        assert!(matches!(
            scan(junk.to_str().unwrap(), None, false),
            Err(Fail::Recover(RecoverError::UnknownFs))
        ));
    }

    /// A drive or device scan never saves its list on the volume being scanned: not in the
    /// data folder, not in the temporary folder, not where `--save` says.
    #[test]
    fn the_list_of_a_drive_is_never_saved_on_that_drive() {
        let data = crate::settings::data_dir().unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let here = recover::volume_of(&data).unwrap();
        match save_place(here, true, None).ok().unwrap() {
            None => {}
            Some(file) => assert!(!on_volume(file.parent().unwrap(), here), "{file:?}"),
        }
        assert!(matches!(
            save_place(here, true, Some(&data.join("scan.json"))),
            Err(Fail::Usage(_))
        ));
        // Not a drive (an image file): the data folder is fine.
        assert_eq!(
            save_place(here, false, None).ok().unwrap(),
            Some(default_places()[0].clone())
        );
        // Some other volume (not a real one): the data folder.
        let elsewhere: VolumeId = serde_json::from_str("18446744073709551615").unwrap();
        assert_eq!(
            save_place(elsewhere, true, None).ok().unwrap(),
            Some(default_places()[0].clone())
        );
    }

    #[test]
    fn restore_takes_files_from_the_volume_not_from_the_list() {
        let mut nt = NtfsImg::new();
        let a = nt.add_file(NT_ROOT, "a.txt", None, &[b'a'; 3000], false);
        let b = nt.add_file(NT_ROOT, "b.txt", None, b"bee", false);
        nt.delete(a);
        nt.delete(b);
        let fresh = recover::scan(&mut std::io::Cursor::new(nt.image())).unwrap();
        let at: HashMap<u64, usize> = fresh
            .candidates
            .iter()
            .enumerate()
            .map(|(i, c)| (c.location, i))
            .collect();
        let real = &fresh.candidates[0];
        let want = SavedFile {
            location: real.location,
            name: real.name.clone(),
            size: real.size,
        };
        assert_eq!(find_again(&fresh, &at, &want), Some(real));
        // An entry edited to another size, name or place names nothing.
        for edited in [
            SavedFile {
                size: 1 << 40,
                ..want.clone()
            },
            SavedFile {
                name: "SAM".into(),
                ..want.clone()
            },
            SavedFile {
                location: 9999,
                ..want.clone()
            },
        ] {
            assert_eq!(find_again(&fresh, &at, &edited), None, "{edited:?}");
        }
    }

    #[test]
    fn names_cannot_drive_the_terminal() {
        assert_eq!(shown("plain name.txt"), "plain name.txt");
        assert!(matches!(shown("x"), Cow::Borrowed(_)));
        assert_eq!(
            shown("\u{1b}]52;c;aGk=\u{7}x.txt"),
            "\\u{1b}]52;c;aGk=\\u{7}x.txt"
        );
        assert_eq!(shown("a\u{9b}2Jb"), "a\\u{9b}2Jb");
        assert_eq!(shown("tab\there\r\n"), "tab\\u{9}here\\u{d}\\u{a}");
        assert_eq!(shown("evil\u{202E}txt.exe"), "evil\\u{202e}txt.exe");
        assert_eq!(shown("del\u{7f}"), "del\\u{7f}");
        assert_eq!(shown("日本語.txt"), "日本語.txt");
    }
}
