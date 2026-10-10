//! Web Share Target uploads (`POST /share` on `--web`). "Share → Keel" on a phone posts the
//! shared files as `multipart/form-data` to `/share`. That request cannot carry the daemon
//! token (the browser makes it, not the page), so the files are only parked: they are
//! written to `<data dir>/shares/<id>/` (at most `SHARE_MAX` bytes, `MAX_FILES` files, at
//! most `MAX_WAITING` uploads waiting at once) and the answer sends the browser to
//! `/?share=<id>`. Nothing more happens until the signed-in client (the token over `/rpc`)
//! claims the id with `share.claim`, once, within `CLAIM_WAIT`; then the user picks a
//! device and sends the files with `spacedrop.send` (previewed). An upload not claimed in
//! time is deleted; a claimed one after `CLAIMED_KEEP` (the drop job reads the files until
//! it is done). At start every unclaimed upload left by an earlier run is deleted.

use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Most bytes one share may post (the whole request body).
pub(crate) const SHARE_MAX: u64 = 512 << 20;
/// Most files in one share.
const MAX_FILES: usize = 100;
/// Uploads being received or waiting to be claimed, at once.
pub(crate) const MAX_WAITING: usize = 4;
/// How long an upload waits for the signed-in client to claim it.
pub(crate) const CLAIM_WAIT: Duration = Duration::from_secs(5 * 60);
/// How long a claimed upload is kept (its drop reads the files).
pub(crate) const CLAIMED_KEEP: Duration = Duration::from_secs(24 * 60 * 60);
/// Written into a claimed upload's folder: a restarted daemon keeps it until
/// `CLAIMED_KEEP`, and deletes folders without it.
const CLAIMED_MARK: &str = ".claimed";
/// Longest part header block.
const PART_HEAD_MAX: usize = 8 << 10;

struct Upload {
    /// Received (or claimed) at.
    at: Instant,
    done: bool,
    claimed: bool,
    files: Vec<(String, u64)>,
}

/// Why a share was refused (an HTTP status and a reason).
#[derive(Debug, PartialEq)]
pub(crate) struct Refused(pub(crate) &'static str, pub(crate) String);

pub(crate) struct Uploads {
    dir: PathBuf,
    max: u64,
    claim_wait: Duration,
    uploads: Mutex<HashMap<String, Upload>>,
}

impl Uploads {
    /// Uploads in `dir`; deletes what an earlier run left unclaimed (or claimed too long ago).
    pub(crate) fn open(dir: PathBuf) -> Self {
        Self::with_limits(dir, SHARE_MAX, CLAIM_WAIT)
    }

    pub(crate) fn with_limits(dir: PathBuf, max: u64, claim_wait: Duration) -> Self {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let mark = std::fs::metadata(e.path().join(CLAIMED_MARK)).ok();
                let fresh = mark
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| SystemTime::now().duration_since(t).ok())
                    .is_some_and(|age| age < CLAIMED_KEEP);
                if !fresh {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        Self {
            dir,
            max,
            claim_wait,
            uploads: Mutex::default(),
        }
    }

    /// Deletes uploads not claimed within the wait, and claimed ones past `CLAIMED_KEEP`.
    pub(crate) fn sweep(&self) {
        let mut uploads = self.uploads.lock();
        uploads.retain(|id, u| {
            let keep = !u.done
                || (u.claimed && u.at.elapsed() < CLAIMED_KEEP)
                || (!u.claimed && u.at.elapsed() < self.claim_wait);
            if !keep {
                let _ = std::fs::remove_dir_all(self.dir.join(id));
            }
            keep
        });
    }

    /// Receives one share: `body` is the request body (`len` bytes, `content_type` its
    /// header). Returns the upload id for `/?share=<id>`.
    pub(crate) fn receive(
        &self,
        body: impl Read,
        len: u64,
        content_type: &str,
    ) -> Result<String, Refused> {
        if len > self.max {
            return Err(Refused(
                "413 Payload Too Large",
                format!("a share may be at most {} MiB", self.max >> 20),
            ));
        }
        let boundary = boundary(content_type).ok_or_else(|| {
            Refused(
                "415 Unsupported Media Type",
                "multipart/form-data only".into(),
            )
        })?;
        self.sweep();
        let id = {
            let mut uploads = self.uploads.lock();
            if uploads.values().filter(|u| !u.claimed).count() >= MAX_WAITING {
                return Err(Refused(
                    "429 Too Many Requests",
                    "too many shares waiting to be opened in Keel".into(),
                ));
            }
            let mut bytes = [0u8; 12];
            getrandom::fill(&mut bytes)
                .map_err(|e| Refused("500 Internal Server Error", e.to_string()))?;
            // 24 hex digits: under the client's "looks like a secret" address rule, and the
            // id grants nothing without the token anyway.
            let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            uploads.insert(
                id.clone(),
                Upload {
                    at: Instant::now(),
                    done: false,
                    claimed: false,
                    files: Vec::new(),
                },
            );
            id
        };
        let folder = self.dir.join(&id);
        let got = keel_api::private::create_dir_all(&folder)
            .map_err(|e| e.to_string())
            .and_then(|()| parse(body.take(len), &boundary, &folder));
        let mut uploads = self.uploads.lock();
        match got {
            Ok(files) if !files.is_empty() => {
                if let Some(u) = uploads.get_mut(&id) {
                    *u = Upload {
                        at: Instant::now(),
                        done: true,
                        claimed: false,
                        files,
                    };
                }
                Ok(id)
            }
            failed => {
                uploads.remove(&id);
                let _ = std::fs::remove_dir_all(&folder);
                Err(Refused(
                    "400 Bad Request",
                    failed.err().unwrap_or_else(|| "no files shared".into()),
                ))
            }
        }
    }

    /// `share.claim`: hands the upload's files to the signed-in client, once.
    pub(crate) fn claim(&self, id: &str) -> Result<Value, String> {
        self.sweep();
        let mut uploads = self.uploads.lock();
        let u = uploads
            .get_mut(id)
            .filter(|u| u.done)
            .ok_or("no such share (it expired after 5 minutes, or never arrived)")?;
        if u.claimed {
            return Err("this share was opened already".into());
        }
        let folder = self.dir.join(id);
        std::fs::write(folder.join(CLAIMED_MARK), b"").map_err(|e| e.to_string())?;
        u.claimed = true;
        u.at = Instant::now();
        let files: Vec<Value> = u
            .files
            .iter()
            .map(|(name, size)| {
                json!({"name": name, "path": folder.join(name).display().to_string(), "size": size})
            })
            .collect();
        Ok(json!({"id": id, "dir": folder.display().to_string(), "files": files}))
    }
}

/// The boundary of a `multipart/form-data` content type.
fn boundary(content_type: &str) -> Option<String> {
    let mut parts = content_type.split(';').map(str::trim);
    if !parts.next()?.eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    let b = parts.find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("boundary")
            .then(|| v.trim().trim_matches('"').to_owned())
    })?;
    (1..=70).contains(&b.len()).then_some(b)
}

/// A shared file's name as a plain file name in the upload folder (None: skip the part).
fn safe_name(raw: &str) -> Option<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let name: String = base
        .chars()
        .map(|c| match c {
            c if c.is_control() => '_',
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c => c,
        })
        .take(200)
        .collect();
    let name = name.trim().trim_end_matches(['.', ' ']).to_owned();
    if name.is_empty() || name.starts_with(".keel-partial-") || name == CLAIMED_MARK {
        return None;
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_lowercase();
    let device = matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
        || (stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && stem.as_bytes()[3].is_ascii_digit());
    Some(if device { format!("_{name}") } else { name })
}

/// `name`, or `name (1).ext`, … : the first that is not in `dir` yet.
fn unique(dir: &Path, name: &str) -> PathBuf {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    let mut n = 0;
    loop {
        let candidate = match n {
            0 => dir.join(name),
            n => dir.join(format!("{stem} ({n}){ext}")),
        };
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// A `Content-Disposition` parameter (`name="files"`).
fn param(head: &str, key: &str) -> Option<String> {
    let line = head
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-disposition:"))?;
    let pat = format!("{key}=\"");
    let lower = line.to_ascii_lowercase();
    // `filename=` must not match inside `name=` and the other way round.
    let at = lower
        .match_indices(&pat)
        .map(|(i, _)| i)
        .find(|&i| i == 0 || matches!(lower.as_bytes()[i - 1], b' ' | b';'))?;
    let rest = &line[at + pat.len()..];
    Some(rest[..rest.find('"')?].to_owned())
}

/// Streams a `multipart/form-data` body into `dir`: one file per part with a file name
/// (form fields without one, the shared title / text / url, are skipped). Returns the
/// stored names and sizes.
fn parse(mut body: impl Read, boundary: &str, dir: &Path) -> Result<Vec<(String, u64)>, String> {
    let first = format!("--{boundary}");
    let delim = format!("\r\n--{boundary}");
    let mut buf: Vec<u8> = Vec::new();
    let mut eof = false;
    let mut fill = |buf: &mut Vec<u8>, eof: &mut bool| -> Result<(), String> {
        let mut chunk = [0u8; 64 << 10];
        let n = body.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            *eof = true;
        }
        buf.extend_from_slice(&chunk[..n]);
        Ok(())
    };
    let find = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).position(|w| w == needle);
    // The first boundary (browsers send no preamble).
    while buf.len() < first.len() && !eof {
        fill(&mut buf, &mut eof)?;
    }
    if !buf.starts_with(first.as_bytes()) {
        return Err("not a multipart body".into());
    }
    buf.drain(..first.len());
    let mut files = Vec::new();
    loop {
        while buf.len() < 2 && !eof {
            fill(&mut buf, &mut eof)?;
        }
        if buf.starts_with(b"--") {
            return Ok(files);
        }
        if !buf.starts_with(b"\r\n") {
            return Err("malformed multipart body".into());
        }
        buf.drain(..2);
        let head_end = loop {
            if let Some(i) = find(&buf, b"\r\n\r\n") {
                break i;
            }
            if buf.len() > PART_HEAD_MAX || eof {
                return Err("malformed part headers".into());
            }
            fill(&mut buf, &mut eof)?;
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        buf.drain(..head_end + 4);
        let name = param(&head, "filename").and_then(|n| safe_name(&n));
        let mut out = match &name {
            Some(n) => {
                if files.len() >= MAX_FILES {
                    return Err(format!("at most {MAX_FILES} files per share"));
                }
                let path = unique(dir, n);
                let file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
                Some((path, io::BufWriter::new(file), 0u64))
            }
            None => None,
        };
        let mut write = |bytes: &[u8]| -> Result<(), String> {
            if let Some((_, w, n)) = &mut out {
                w.write_all(bytes).map_err(|e| e.to_string())?;
                *n += bytes.len() as u64;
            }
            Ok(())
        };
        loop {
            if let Some(i) = find(&buf, delim.as_bytes()) {
                write(&buf[..i])?;
                buf.drain(..i + delim.len());
                break;
            }
            // Keep a tail that may hold the start of the delimiter.
            let keep = delim.len().min(buf.len());
            let emit = buf.len() - keep;
            write(&buf[..emit])?;
            buf.drain(..emit);
            if eof {
                return Err("the share was cut short".into());
            }
            fill(&mut buf, &mut eof)?;
        }
        if let Some((path, mut w, n)) = out {
            w.flush().map_err(|e| e.to_string())?;
            let stored = path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            files.push((stored, n));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(boundary: &str, parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
        let mut b = Vec::new();
        for (name, file, data) in parts {
            b.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match file {
                Some(f) => b.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\n\
                         Content-Type: application/octet-stream\r\n\r\n"
                    )
                    .as_bytes(),
                ),
                None => b.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
                ),
            }
            b.extend_from_slice(data);
            b.extend_from_slice(b"\r\n");
        }
        b.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        b
    }

    /// Reads one byte at a time: delimiters split across reads.
    struct Drip<'a>(&'a [u8]);
    impl Read for Drip<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.split_first() {
                Some((b, rest)) if !buf.is_empty() => {
                    buf[0] = *b;
                    self.0 = rest;
                    Ok(1)
                }
                _ => Ok(0),
            }
        }
    }

    #[test]
    fn parses_files_skips_fields_and_cleans_names() {
        let dir = tempfile::tempdir().unwrap();
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let b = body(
            "XyZ",
            &[
                ("title", None, b"holiday"),
                ("files", Some("a.jpg"), b"\r\n--Xy almost a boundary"),
                ("files", Some("a.jpg"), &big),
                ("files", Some("..\\..\\evil:name?.txt"), b"x"),
                ("files", Some(""), b""),
                ("files", Some("con.txt"), b"c"),
            ],
        );
        let files = parse(Drip(&b), "XyZ", dir.path()).unwrap();
        assert_eq!(
            files,
            vec![
                ("a.jpg".into(), 24),
                ("a (1).jpg".into(), 200_000),
                ("evil_name_.txt".into(), 1),
                ("_con.txt".into(), 1),
            ]
        );
        assert_eq!(
            std::fs::read(dir.path().join("a.jpg")).unwrap(),
            b"\r\n--Xy almost a boundary"
        );
        assert_eq!(std::fs::read(dir.path().join("a (1).jpg")).unwrap(), big);
        let cut = &b[..b.len() - 20];
        assert!(parse(cut, "XyZ", tempfile::tempdir().unwrap().path()).is_err());
        assert_eq!(
            boundary("multipart/form-data; boundary=\"a b\"").as_deref(),
            Some("a b")
        );
        assert_eq!(boundary("text/plain; boundary=x"), None);
        assert_eq!(
            param(
                "Content-Disposition: form-data; name=\"f\"; filename=\"x\"",
                "name"
            )
            .as_deref(),
            Some("f")
        );
    }

    #[test]
    fn claims_once_caps_size_and_waiting_and_expires() {
        let dir = tempfile::tempdir().unwrap();
        let ct = "multipart/form-data; boundary=B";
        let one = body("B", &[("files", Some("n.txt"), b"note")]);
        let up = Uploads::with_limits(dir.path().into(), 1 << 20, Duration::from_millis(300));
        let too_big = up.receive(&one[..], 2 << 20, ct).unwrap_err();
        assert_eq!(too_big.0, "413 Payload Too Large");
        assert_eq!(
            up.receive(&one[..], one.len() as u64, "text/plain")
                .unwrap_err()
                .0,
            "415 Unsupported Media Type"
        );
        let empty = body("B", &[("title", None, b"t")]);
        assert_eq!(
            up.receive(&empty[..], empty.len() as u64, ct)
                .unwrap_err()
                .0,
            "400 Bad Request"
        );

        let id = up.receive(&one[..], one.len() as u64, ct).unwrap();
        assert_eq!(id.len(), 24);
        let claimed = up.claim(&id).unwrap();
        assert_eq!(claimed["files"][0]["name"], "n.txt");
        assert_eq!(claimed["files"][0]["size"], 4);
        let path = PathBuf::from(claimed["files"][0]["path"].as_str().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"note");
        assert!(up.claim(&id).is_err(), "claimed once");
        assert!(up.claim("nope").is_err());

        // At most MAX_WAITING unclaimed at once; unclaimed ones expire and are deleted.
        let waiting: Vec<String> = (0..MAX_WAITING)
            .map(|_| up.receive(&one[..], one.len() as u64, ct).unwrap())
            .collect();
        assert_eq!(
            up.receive(&one[..], one.len() as u64, ct).unwrap_err().0,
            "429 Too Many Requests"
        );
        std::thread::sleep(Duration::from_millis(400));
        up.sweep();
        for id in &waiting {
            assert!(!dir.path().join(id).exists(), "unclaimed upload deleted");
            assert!(up.claim(id).is_err(), "expired");
        }
        assert!(path.exists(), "a claimed upload is kept for its drop");

        // A restart keeps claimed uploads and deletes unclaimed ones.
        let left = up.receive(&one[..], one.len() as u64, ct).unwrap();
        drop(up);
        let _again = Uploads::open(dir.path().into());
        assert!(path.exists());
        assert!(!dir.path().join(left).exists());
    }
}
