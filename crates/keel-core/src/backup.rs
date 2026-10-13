//! Library backup and restore: a `<library>-<date>.keelbackup` file is a zip holding
//! `manifest.json` and a consistent copy (`VACUUM INTO` on the live connection) of
//! `library.db` and of each source's store, named `<source id>.db`. Thumbnails and the
//! search index inside each store are left out and rebuilt.
//!
//! A backup is a file from outside: `restore` reads it as untrusted. The entries must be
//! exactly the files the manifest lists (plain names, no folders), each bounded in size and
//! in compression ratio, the manifest bounded and parsed with serde, each file's SHA-256 must
//! match the manifest, and each database passes `PRAGMA quick_check` (opened read-only), is
//! not newer than this Keel and holds no triggers or views but Keel's own before it is moved
//! into place. Anything else is a [`BackupError`]. A restore never replaces a library.

use crate::db::{self, Store};
use crate::library::check_name;
use crate::{Library, SourceDef, SourceKind};
use anyhow::{Context, Result};
use keel_vfs::VPath;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The file extension (`<library>-<date>.keelbackup`).
pub const EXTENSION: &str = "keelbackup";
/// The newest backup format this Keel writes and reads.
pub const FORMAT: u32 = 1;
const MANIFEST: &str = "manifest.json";
const LIBRARY_DB: &str = "library.db";
/// The manifest's size limit.
pub const MAX_MANIFEST: u64 = 1 << 20;
/// Any one file's size limit.
pub const MAX_ENTRY: u64 = 4 << 30;
/// Entries over `RATIO_FROM` bytes may expand at most this many times.
pub const MAX_RATIO: u64 = 100;
const RATIO_FROM: u64 = 1 << 20;
/// Sources a manifest may list.
const MAX_SOURCES: usize = 10_000;
/// `library.db` meta key: when the last backup was written (unix seconds).
pub(crate) const LAST_BACKUP: &str = "last_backup";

/// Why a backup file cannot be restored (`err.downcast_ref::<BackupError>()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupError {
    /// Not a zip, no manifest, or a manifest that is not Keel's.
    NotABackup(String),
    /// A Keel backup whose contents do not match its manifest (or a database that fails
    /// its check).
    Damaged(String),
    /// A newer backup format, or a database schema newer than this Keel's.
    Newer(String),
    /// A library of that name exists (restore it under another name).
    NameTaken(String),
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackupError::NotABackup(why) => write!(f, "not a Keel backup ({why})"),
            BackupError::Damaged(why) => write!(f, "damaged backup ({why})"),
            BackupError::Newer(what) => {
                write!(
                    f,
                    "made by a newer Keel ({what}): update Keel to restore it"
                )
            }
            BackupError::NameTaken(name) => write!(
                f,
                "a library named {name} exists; restore it under another name (--as)"
            ),
        }
    }
}

impl std::error::Error for BackupError {}

/// `manifest.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// [`FORMAT`] when written.
    pub format: u32,
    pub keel_version: String,
    /// The library's name and id.
    pub library: String,
    pub library_id: String,
    /// Unix seconds.
    pub created: i64,
    pub sources: Vec<BackupSource>,
    /// Every file in the zip but the manifest.
    pub files: Vec<BackupFile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupSource {
    pub id: String,
    pub label: String,
    pub root: VPath,
    pub kind: SourceKind,
    /// The volume the root was last seen on.
    #[serde(default)]
    pub volume_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupFile {
    /// `library.db` or `<source id>.db`.
    pub name: String,
    pub size: u64,
    /// Lowercase hex.
    pub sha256: String,
}

/// What `restore` created.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Restored {
    pub name: String,
    pub dir: PathBuf,
    pub sources: Vec<RestoredSource>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoredSource {
    pub id: String,
    pub label: String,
    pub root: VPath,
    /// A local root that exists here: it is walked again (an index job queued for the
    /// library's next open). Others stay offline until their root is back (remote and cloud
    /// roots are checked when the library opens).
    pub online: bool,
}

/// `<library>-<YYYY-MM-DD>.keelbackup` (UTC date) for a backup made at `created`.
pub fn file_name(library: &str, created: i64) -> String {
    let (y, m, d) = civil(created.div_euclid(86_400));
    format!("{library}-{y:04}-{m:02}-{d:02}.{EXTENSION}")
}

/// Days since 1970-01-01 to a proleptic Gregorian date (the days-to-civil algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// A source id as `random_id` makes them (also a file name in the zip and a folder name).
fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Removes a folder when dropped (a backup's or a restore's scratch folder).
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `VACUUM INTO` `to` (a consistent snapshot, also of a store being written), then a single
/// file (no WAL) without the search indexes (`strip_fts`: a source store's `record_fts`,
/// and the text read from its documents, which the text job reads again after a restore).
fn snapshot(conn: &Connection, to: &Path, strip_fts: bool) -> Result<()> {
    conn.execute("VACUUM INTO ?1", [to.to_string_lossy()])?;
    let copy = Connection::open(to)?;
    copy.query_row("PRAGMA journal_mode=DELETE", [], |_| Ok(()))?;
    if strip_fts && has_table(&copy, "record_fts")? {
        copy.execute_batch("INSERT INTO record_fts(record_fts) VALUES ('delete-all');")?;
    }
    if strip_fts && has_table(&copy, "record_content")? {
        copy.execute_batch(
            "INSERT INTO record_content(record_content) VALUES ('delete-all');
             DELETE FROM record_text_body;
             DELETE FROM record_text;",
        )?;
    }
    if strip_fts {
        copy.execute_batch("VACUUM;")?;
    }
    Ok(())
}

fn has_table(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .prepare("SELECT 1 FROM sqlite_master WHERE name = ?1")?
        .exists([name])?)
}

/// Moves the finished backup `from` to `to` unless a file is there, atomically where the
/// file system has hard links.
fn place_new(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(from, to) {
        Ok(()) => std::fs::remove_file(from),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        Err(_) if to.try_exists()? => Err(std::io::ErrorKind::AlreadyExists.into()),
        // ponytail: FAT and exFAT have no hard links; a file made between the check and
        // this rename is replaced there.
        Err(_) => std::fs::rename(from, to),
    }
}

/// Scratch folders a crash left behind: `backup-*` in the library's folder (only the
/// process holding the library backs it up, and the caller holds it now) and day-old
/// `.restore-*` in the data folder (a restore may be running in another process).
pub(crate) fn sweep_scratch(data_root: &Path, library_dir: &Path) {
    let day_old = |p: &Path| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() > 86_400)
    };
    for (dir, prefix, any_age) in [
        (library_dir, "backup-", true),
        (data_root, ".restore-", false),
    ] {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let ours = e
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(prefix));
            if ours && e.file_type().is_ok_and(|t| t.is_dir()) && (any_age || day_old(&e.path())) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

impl Library {
    /// Writes a backup of the library to `to` (replacing a file there once it is complete):
    /// `library.db` and every source store, each a consistent snapshot even while jobs
    /// write. Records the time (`LibraryStats::last_backup`).
    pub fn backup(&self, to: &Path) -> Result<Manifest> {
        self.backup_to(to, true)
    }

    /// `backup` that never replaces a file at `to`, also one made while it ran.
    pub fn backup_new(&self, to: &Path) -> Result<Manifest> {
        self.backup_to(to, false)
    }

    fn backup_to(&self, to: &Path, replace: bool) -> Result<Manifest> {
        let created = crate::now();
        let scratch = Scratch(self.dir().join(format!("backup-{}", crate::random_id()?)));
        std::fs::create_dir_all(&scratch.0)?;
        snapshot(&*self.shared.db.get()?, &scratch.0.join(LIBRARY_DB), false)?;
        // The sources as the snapshot has them (one removed meanwhile is left out).
        let lib_copy = Connection::open(scratch.0.join(LIBRARY_DB))?;
        let rows: Vec<(String, String, Option<String>)> = {
            let mut stmt =
                lib_copy.prepare("SELECT id, def, volume_id FROM source ORDER BY rowid")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut sources = Vec::new();
        let mut names = vec![LIBRARY_DB.to_owned()];
        for (id, def, volume_id) in rows {
            let def: SourceDef = serde_json::from_str(&def)?;
            let Some(src) = self.source(&crate::SourceId(id.clone())) else {
                lib_copy.execute("DELETE FROM source WHERE id = ?1", [&id])?;
                continue;
            };
            let name = format!("{id}.db");
            snapshot(&*src.store.get()?, &scratch.0.join(&name), true)
                .with_context(|| format!("source {}", def.label))?;
            names.push(name);
            sources.push(BackupSource {
                id,
                label: def.label,
                root: def.root,
                kind: def.kind,
                volume_id,
            });
        }
        drop(lib_copy);
        // Refused before anything is written (an older backup at `to` stays): a restore
        // reads no file over `MAX_ENTRY`.
        for name in &names {
            fits(name, std::fs::metadata(scratch.0.join(name))?.len())?;
        }
        let partial = to.with_extension(format!("{EXTENSION}.{}.partial", std::process::id()));
        // Only clean up a partial this call created; another backup may own this name.
        let file = std::fs::File::create_new(&partial)
            .with_context(|| format!("writing {}", to.display()))?;
        let written = (|| -> Result<Manifest> {
            let mut zip = zip::ZipWriter::new(file);
            let mut files = Vec::new();
            let mut buf = vec![0u8; 1 << 16];
            for name in names {
                let path = scratch.0.join(&name);
                let size = std::fs::metadata(&path)?.len();
                let options = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated)
                    .large_file(size >= u64::from(u32::MAX));
                zip.start_file(name.as_str(), options)?;
                let mut hasher = Sha256::new();
                let mut file = std::fs::File::open(&path)?;
                loop {
                    let n = file.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buf[..n]);
                    zip.write_all(&buf[..n])?;
                }
                files.push(BackupFile {
                    name,
                    size,
                    sha256: hex(&hasher.finalize()),
                });
            }
            let manifest = Manifest {
                format: FORMAT,
                keel_version: env!("CARGO_PKG_VERSION").to_owned(),
                library: self.name.clone(),
                library_id: self.id.0.clone(),
                created,
                sources,
                files,
            };
            zip.start_file(MANIFEST, zip::write::SimpleFileOptions::default())?;
            zip.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
            zip.finish()?.sync_all()?;
            Ok(manifest)
        })();
        let placed = written.and_then(|m| {
            match replace {
                true => std::fs::rename(&partial, to),
                false => place_new(&partial, to),
            }?;
            Ok(m)
        });
        let manifest = match placed {
            Ok(m) => m,
            Err(e) => {
                let _ = std::fs::remove_file(&partial);
                return Err(e.context(format!("writing {}", to.display())));
            }
        };
        self.shared.db.set_meta(LAST_BACKUP, &created.to_string())?;
        Ok(manifest)
    }
}

/// A backup holds no file a restore would refuse (`MAX_ENTRY`).
fn fits(name: &str, size: u64) -> Result<()> {
    anyhow::ensure!(
        size <= MAX_ENTRY,
        "cannot back up this library: {name} is {size} bytes, and a backup holds files of \
         at most 4 GiB"
    );
    Ok(())
}

/// The opened zip and its checked manifest.
type Opened = (zip::ZipArchive<std::fs::File>, Manifest);

fn not_backup(why: impl Into<String>) -> anyhow::Error {
    BackupError::NotABackup(why.into()).into()
}

fn damaged(why: impl Into<String>) -> anyhow::Error {
    BackupError::Damaged(why.into()).into()
}

/// Opens `file` and checks its manifest and the zip's entries against it (no hashing).
fn open(file: &Path) -> Result<Opened> {
    let f = std::fs::File::open(file).with_context(|| format!("opening {}", file.display()))?;
    let mut zip = zip::ZipArchive::new(f).map_err(|e| not_backup(format!("not a zip: {e}")))?;
    let bytes = {
        let mut entry = zip
            .by_name(MANIFEST)
            .map_err(|_| not_backup("no manifest.json"))?;
        if entry.size() > MAX_MANIFEST {
            return Err(not_backup("manifest.json is over 1 MiB"));
        }
        let mut bytes = Vec::new();
        (&mut entry)
            .take(MAX_MANIFEST + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| damaged(format!("manifest.json: {e}")))?;
        if bytes.len() as u64 > MAX_MANIFEST {
            return Err(not_backup("manifest.json is over 1 MiB"));
        }
        bytes
    };
    #[derive(Deserialize)]
    struct Head {
        format: u32,
    }
    let head: Head =
        serde_json::from_slice(&bytes).map_err(|e| not_backup(format!("manifest.json: {e}")))?;
    if head.format > FORMAT {
        return Err(BackupError::Newer(format!("backup format {}", head.format)).into());
    }
    if head.format == 0 {
        return Err(not_backup("format 0"));
    }
    let m: Manifest =
        serde_json::from_slice(&bytes).map_err(|e| not_backup(format!("manifest.json: {e}")))?;
    check_manifest(&m)?;
    // The entries: exactly the listed files and the manifest, each within bounds.
    if zip.len() != m.files.len() + 1 {
        return Err(damaged(format!(
            "{} entries, the manifest lists {}",
            zip.len(),
            m.files.len() + 1
        )));
    }
    let mut seen = HashSet::new();
    for i in 0..zip.len() {
        let entry = zip.by_index_raw(i).map_err(|e| damaged(e.to_string()))?;
        let name = entry.name().to_owned();
        let listed = m.files.iter().find(|f| f.name == name);
        if name != MANIFEST && listed.is_none() {
            return Err(damaged(format!("unlisted entry {name:?}")));
        }
        if !seen.insert(name.clone()) || entry.is_dir() || entry.encrypted() {
            return Err(damaged(format!("entry {name:?}")));
        }
        let Some(listed) = listed else { continue };
        let (size, packed) = (entry.size(), entry.compressed_size());
        if size != listed.size {
            return Err(damaged(format!("{name}: size differs from the manifest")));
        }
        if size > RATIO_FROM && size / MAX_RATIO > packed {
            return Err(damaged(format!(
                "{name} expands more than {MAX_RATIO} times"
            )));
        }
    }
    Ok((zip, m))
}

/// The manifest's own rules: a valid library name, plain file names that match the sources
/// (`library.db` and `<id>.db` for each), sizes within `MAX_ENTRY`, SHA-256 hex.
fn check_manifest(m: &Manifest) -> Result<()> {
    check_name(&m.library).map_err(|_| not_backup("invalid library name"))?;
    if m.sources.len() > MAX_SOURCES {
        return Err(not_backup("too many sources"));
    }
    let mut want: HashSet<String> = HashSet::from([LIBRARY_DB.to_owned()]);
    for s in &m.sources {
        if !valid_id(&s.id) || !want.insert(format!("{}.db", s.id)) {
            return Err(not_backup(format!("source id {:?}", s.id)));
        }
    }
    let mut names = HashSet::new();
    for f in &m.files {
        if !want.contains(&f.name) || !names.insert(f.name.as_str()) {
            return Err(not_backup(format!("file {:?}", f.name)));
        }
        if f.size > MAX_ENTRY {
            return Err(not_backup(format!("{} is over 4 GiB", f.name)));
        }
        if f.sha256.len() != 64 || !f.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(not_backup(format!("{}: checksum", f.name)));
        }
    }
    if names.len() != want.len() {
        return Err(not_backup("files missing from the manifest"));
    }
    Ok(())
}

/// What a backup file holds (its manifest, checked against the zip; the files are not
/// read): for a preview.
pub fn inspect(file: &Path) -> Result<Manifest> {
    open(file).map(|(_, m)| m)
}

/// Whether the library `name` exists under `data_root` (`<data_root>/library/<name>`).
pub fn exists(data_root: &Path, name: &str) -> bool {
    data_root.join("library").join(name).exists()
}

/// Whether a source root is a local folder that exists here (a restored source with one is
/// walked again; others stay offline until their root is back).
pub fn reachable(root: &VPath) -> bool {
    root.to_local_path().is_some_and(|p| p.is_dir())
}

/// Restores the backup `file` as a new library under `<data_root>/library/` named `name`
/// (else the manifest's): refused when that library exists. Sources whose local root exists
/// get an index job, run when the library next opens; jobs of the backed-up library are
/// dropped, and each store's search index is rebuilt.
pub fn restore(data_root: &Path, file: &Path, name: Option<&str>) -> Result<Restored> {
    let (mut zip, m) = open(file)?;
    let name = name.unwrap_or(&m.library).to_owned();
    check_name(&name)?;
    let dest = data_root.join("library").join(&name);
    if dest.exists() {
        return Err(BackupError::NameTaken(name).into());
    }
    std::fs::create_dir_all(data_root.join("library"))?;
    let need: u64 = m.files.iter().map(|f| f.size).sum();
    let space = keel_vfs::Provider::space(&keel_vfs::LocalProvider, &VPath::local(data_root));
    if let Some(space) = space.filter(|s| s.free < need.saturating_add(64 << 20)) {
        anyhow::bail!(
            "not enough free space to restore: {need} bytes needed, {} free",
            space.free
        );
    }
    let scratch = Scratch(data_root.join(format!(".restore-{}", crate::random_id()?)));
    std::fs::create_dir_all(scratch.0.join("sources"))?;
    let mut buf = vec![0u8; 1 << 16];
    for f in &m.files {
        let mut entry = zip
            .by_name(&f.name)
            .map_err(|e| damaged(format!("{}: {e}", f.name)))?;
        let path = scratch.0.join(&f.name);
        let mut out = std::fs::File::create(&path)?;
        let mut hasher = Sha256::new();
        let mut total = 0u64;
        let mut limited = (&mut entry).take(f.size + 1);
        loop {
            let n = limited
                .read(&mut buf)
                .map_err(|e| damaged(format!("{}: {e}", f.name)))?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > f.size {
                return Err(damaged(format!("{} is longer than listed", f.name)));
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
        }
        if total != f.size || hex(&hasher.finalize()) != f.sha256 {
            return Err(damaged(format!("{} does not match its checksum", f.name)));
        }
        out.sync_all()?;
        drop(out);
        let store = if f.name == LIBRARY_DB {
            Store::Library
        } else {
            Store::Source
        };
        check_db(&path, store).map_err(|e| match e.downcast::<BackupError>() {
            Ok(e) => e.into(),
            Err(e) => damaged(format!("{}: {e:#}", f.name)),
        })?;
    }
    let out = prepare(&scratch.0, &m, &name).map_err(|e| match e.downcast::<BackupError>() {
        Ok(e) => e.into(),
        Err(e) => damaged(format!("{e:#}")),
    })?;
    let lib = scratch.0.join("library");
    std::fs::create_dir_all(&lib)?;
    std::fs::rename(scratch.0.join(LIBRARY_DB), lib.join(LIBRARY_DB))?;
    std::fs::rename(scratch.0.join("sources"), lib.join("sources"))?;
    // Never over a library made meanwhile. Windows' rename refuses an existing folder; a
    // Unix rename replaces an empty one, so the name is claimed first and only that claim
    // is replaced.
    let taken = |e: &std::io::Error| e.kind() == std::io::ErrorKind::AlreadyExists;
    if cfg!(unix) {
        if let Err(e) = std::fs::create_dir(&dest) {
            return Err(match taken(&e) {
                true => BackupError::NameTaken(name).into(),
                false => anyhow::Error::from(e).context(format!("creating {}", dest.display())),
            });
        }
    }
    if let Err(e) = std::fs::rename(&lib, &dest) {
        if cfg!(unix) {
            let _ = std::fs::remove_dir(&dest);
        } else if taken(&e) || dest.exists() {
            return Err(BackupError::NameTaken(name).into());
        }
        return Err(anyhow::Error::from(e).context(format!("creating {}", dest.display())));
    }
    Ok(Restored {
        name,
        dir: dest,
        sources: out,
    })
}

/// In the scratch folder: the library's sources must be the manifest's (ids, roots, labels
/// and kinds); it takes `name`;
/// its jobs go, an
/// index job is queued for each source whose root is here; each store moves to
/// `sources/<id>/source.db` with its search index rebuilt.
fn prepare(scratch: &Path, m: &Manifest, name: &str) -> Result<Vec<RestoredSource>> {
    let conn = Connection::open(scratch.join(LIBRARY_DB))?;
    let rows: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT id, def FROM source")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let listed: HashSet<&str> = m.sources.iter().map(|s| s.id.as_str()).collect();
    let found: HashSet<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    if listed != found || rows.len() != found.len() {
        return Err(damaged("the sources differ from the manifest"));
    }
    let mut out = Vec::new();
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM job", [])?;
    // Its name as `Library::list` reads it before the library is opened.
    db::set_meta(&tx, "name", name)?;
    if has_table(&tx, "job_file")? {
        tx.execute("DELETE FROM job_file", [])?;
    }
    for (id, def) in &rows {
        let def: SourceDef =
            serde_json::from_str(def).map_err(|e| damaged(format!("source {id}: {e}")))?;
        // What the preview showed (the manifest) is what gets indexed.
        let shown = m.sources.iter().find(|s| s.id == *id);
        if !shown.is_some_and(|s| s.root == def.root && s.label == def.label && s.kind == def.kind)
        {
            return Err(damaged(format!("source {id} differs from the manifest")));
        }
        let online = reachable(&def.root);
        if online {
            let state = serde_json::json!({ "source": id }).to_string();
            tx.execute(
                "INSERT INTO job(kind, state, status, created, updated)
                 VALUES ('index', ?1, 'queued', ?2, ?2)",
                rusqlite::params![state, crate::now()],
            )?;
        }
        out.push(RestoredSource {
            id: id.clone(),
            label: def.label,
            root: def.root,
            online,
        });
    }
    tx.commit()?;
    for s in &out {
        let dir = scratch.join("sources").join(&s.id);
        std::fs::create_dir_all(&dir)?;
        let store = dir.join("source.db");
        std::fs::rename(scratch.join(format!("{}.db", s.id)), &store)?;
        let conn = Connection::open(&store)?;
        if has_table(&conn, "record_fts")? {
            conn.execute("INSERT INTO record_fts(record_fts) VALUES ('rebuild')", [])?;
        }
    }
    Ok(out)
}

/// A database from a backup, opened read-only: `quick_check` passes, its schema version is
/// one this Keel knows, and its triggers and views are exactly those Keel's migrations made
/// at that version (no code of the backup's own runs when Keel writes to it).
fn check_db(path: &Path, store: Store) -> Result<()> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    anyhow::ensure!(check == "ok", "quick_check: {check}");
    let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > db::newest(store) {
        return Err(BackupError::Newer(format!("database schema {version}")).into());
    }
    let want = db::reference_objects(store, version)?
        .with_context(|| format!("unknown schema version {version}"))?;
    anyhow::ensure!(
        db::code_objects(&conn)? == want,
        "triggers or views that are not Keel's"
    );
    Ok(())
}

#[cfg(test)]
#[path = "backup_tests.rs"]
mod tests;
