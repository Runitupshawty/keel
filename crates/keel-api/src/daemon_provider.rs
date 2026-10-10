//! [`DaemonProvider`]: a keel-vfs [`Provider`] whose every call goes to a daemon through a
//! [`Backend`] (the local socket [`crate::client::Client`], a WebSocket, a test double).
//! Paths are passed through unchanged (`VPath` URIs), so the provider serves the daemon's
//! view of one scheme (`library://` lists from its index, offline sources included).
//! Reads stream in `read` ranges; rename and remove go through `plan` + `execute` (the same
//! previews and checks as every client) and wait for the job.

use crate::types::*;
use crate::Backend;
use anyhow::{bail, Context, Result};
use keel_vfs::{Caps, Entry, Kind, Provider, RemoveKind, VPath};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

/// Bytes asked for per `read` call.
const CHUNK: u64 = 1 << 20;
/// Most entries one listing returns.
const LIST_MAX: usize = 1_000_000;
/// How often a running job is polled.
const POLL: Duration = Duration::from_millis(200);

type Shared<B> = Arc<Mutex<B>>;

pub struct DaemonProvider<B> {
    scheme: &'static str,
    backend: Shared<B>,
}

impl<B: Backend + Send + 'static> DaemonProvider<B> {
    pub fn new(scheme: &'static str, backend: B) -> Self {
        Self {
            scheme,
            backend: Arc::new(Mutex::new(backend)),
        }
    }

    fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        call(&self.backend, method, params)
    }

    /// Previews `params` with `plan`, confirms exactly that preview and waits for its job.
    fn plan_and_run(&self, params: Value) -> Result<()> {
        let preview: PlanPreview = self.call("plan", params)?;
        let done: Executed = self.call(
            "execute",
            json!({"plan_id": preview.plan_id, "input_hash": preview.input_hash}),
        )?;
        let Some(job) = done.job else { return Ok(()) };
        loop {
            let info: JobInfo = self.call("jobs.info", json!({ "id": job }))?;
            match info.status.as_str() {
                "done" => return Ok(()),
                "failed" | "cancelled" => bail!(
                    "{} {}: {}",
                    preview.summary,
                    info.status,
                    info.log.unwrap_or_default().trim()
                ),
                _ => std::thread::sleep(POLL),
            }
        }
    }
}

fn call<B: Backend, T: DeserializeOwned>(b: &Mutex<B>, method: &str, params: Value) -> Result<T> {
    let v = b
        .lock()
        .call(method, params)
        .map_err(|e| anyhow::anyhow!("{method}: {}", e.message))?;
    serde_json::from_value(v).with_context(|| format!("{method}: unexpected answer"))
}

fn entry(e: EntryInfo) -> Result<Entry> {
    let path = VPath::parse(&e.path).or_else(|_| Ok::<_, anyhow::Error>(VPath::local(&e.path)))?;
    let ext = match e.name.rsplit_once('.') {
        Some((stem, ext)) if !e.is_dir && !stem.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    };
    Ok(Entry {
        path,
        name: e.name,
        kind: if e.is_dir { Kind::Dir } else { Kind::File },
        size: e.size,
        modified: e
            .modified
            .and_then(|s| u64::try_from(s).ok())
            .map(|s| UNIX_EPOCH + Duration::from_secs(s)),
        hidden: e.hidden,
        is_link: false,
        encrypted: false,
        ext,
    })
}

/// Reads a file in `read` ranges.
struct RangeReader<B> {
    backend: Shared<B>,
    path: String,
    offset: u64,
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
}

impl<B: Backend> Read for RangeReader<B> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos == self.buf.len() {
            if self.eof {
                return Ok(0);
            }
            let chunk: Chunk = call(
                &self.backend,
                "read",
                json!({"path": self.path, "offset": self.offset, "len": CHUNK}),
            )
            .map_err(std::io::Error::other)?;
            use base64::Engine;
            self.buf = base64::engine::general_purpose::STANDARD
                .decode(chunk.data)
                .map_err(std::io::Error::other)?;
            self.pos = 0;
            self.offset += self.buf.len() as u64;
            self.eof = chunk.eof || self.buf.is_empty();
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl<B: Backend + Send + 'static> Provider for DaemonProvider<B> {
    fn scheme(&self) -> &'static str {
        self.scheme
    }

    fn caps(&self) -> Caps {
        Caps {
            write: false,
            rename: true,
            delete: true,
            watch: false,
        }
    }

    fn list(&self, dir: &VPath) -> Result<Vec<Entry>> {
        let l: Listing = self.call("list", json!({"path": dir.display(), "max": LIST_MAX}))?;
        l.entries.into_iter().map(entry).collect()
    }

    fn list_complete(&self, dir: &VPath) -> Result<Vec<Entry>> {
        let l: Listing = self.call("list", json!({"path": dir.display(), "max": LIST_MAX}))?;
        if l.truncated {
            bail!("{} has more than {LIST_MAX} entries", dir.display());
        }
        l.entries.into_iter().map(entry).collect()
    }

    fn stat(&self, p: &VPath) -> Result<Entry> {
        let s: StatInfo = self.call("stat", json!({"path": p.display()}))?;
        entry(s.entry)
    }

    fn read(&self, p: &VPath) -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(RangeReader {
            backend: self.backend.clone(),
            path: p.display(),
            offset: 0,
            buf: Vec::new(),
            pos: 0,
            eof: false,
        }))
    }

    // ponytail: no upload operation in the API yet; writes arrive with spacedrop.send.
    fn write(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        bail!(
            "writing through keel-daemon is not supported: {}",
            p.display()
        )
    }

    fn mkdir(&self, p: &VPath) -> Result<()> {
        bail!("the daemon API has no folder creation yet: {}", p.display())
    }

    /// Same folder: a rename; same name: a move; both at once is refused.
    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        let (src, dst) = (from.display(), to.display());
        if from.parent() == to.parent() {
            self.plan_and_run(json!({"op": "rename", "paths": [src], "new_name": to.name()}))
        } else if from.name() == to.name() {
            let dir = to.parent().context("destination has no folder")?;
            self.plan_and_run(json!({"op": "move", "paths": [src], "to": dir.display()}))
        } else {
            bail!("move and rename at once is not supported: {src} -> {dst}")
        }
    }

    fn remove(&self, p: &VPath) -> Result<()> {
        self.plan_and_run(json!({"op": "delete", "paths": [p.display()]}))
    }

    /// Unknown here (it depends on the daemon's provider): the strongest warning.
    fn remove_kind(&self) -> RemoveKind {
        RemoveKind::Permanent
    }

    // ponytail: previews come from preview.render; a local copy (download) arrives with
    // the first native client that needs one.
    fn local_copy(&self, p: &VPath) -> Result<PathBuf> {
        bail!("no local copy through keel-daemon: {}", p.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApiError;

    type Calls = Arc<Mutex<Vec<(String, Value)>>>;

    /// Answers like a daemon, recording every call.
    struct Mock {
        calls: Calls,
        file: Vec<u8>,
        jobs: u32,
    }

    impl Backend for Mock {
        fn call(&mut self, method: &str, params: Value) -> crate::Result<Value> {
            self.calls.lock().push((method.into(), params.clone()));
            use base64::Engine;
            let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
            Ok(match method {
                "list" => json!({"entries": [
                    {"name": "a.txt", "path": "library://s1/a.txt", "is_dir": false, "size": 3, "modified": 60},
                    {"name": "sub", "path": "library://s1/sub", "is_dir": true, "size": 0},
                ], "truncated": params["path"] == "library://s1/huge"}),
                "stat" => {
                    json!({"entry": {"name": "a.txt", "path": "library://s1/a.txt", "is_dir": false, "size": 3}})
                }
                "read" => {
                    let (off, len) = (
                        params["offset"].as_u64().unwrap() as usize,
                        params["len"].as_u64().unwrap() as usize,
                    );
                    let end = (off + len).min(self.file.len());
                    let start = off.min(end);
                    json!({"offset": off, "data": b64(&self.file[start..end]), "eof": end == self.file.len()})
                }
                "plan" => {
                    json!({"plan_id": "p1", "input_hash": "h1", "operation": "plan", "summary": "Delete 1 item(s)",
                    "changes": [], "warnings": [], "expires_at": 0})
                }
                "execute" => json!({"plan_id": "p1", "operation": "plan", "job": 7}),
                "jobs.info" => {
                    self.jobs += 1;
                    let status = if self.jobs < 2 { "running" } else { "done" };
                    json!({"id": 7, "kind": "file_op", "status": status, "progress": 0.5, "created": 0, "updated": 0})
                }
                _ => return Err(ApiError::new(ApiError::METHOD_NOT_FOUND, method)),
            })
        }
    }

    fn mock(file: &[u8]) -> (DaemonProvider<Mock>, Calls) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let m = Mock {
            calls: calls.clone(),
            file: file.to_vec(),
            jobs: 0,
        };
        (DaemonProvider::new("library", m), calls)
    }

    #[test]
    fn lists_stats_and_reads_over_rpc() {
        let (p, calls) = mock(b"abc");
        let dir = VPath::parse("library://s1/").unwrap();
        let list = p.list(&dir).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].ext, "txt");
        assert_eq!(list[0].path, VPath::parse("library://s1/a.txt").unwrap());
        assert_eq!(list[0].modified, Some(UNIX_EPOCH + Duration::from_secs(60)));
        assert_eq!(list[1].kind, Kind::Dir);
        assert_eq!(calls.lock()[0].1["path"], "library://s1/");
        assert_eq!(p.stat(&list[0].path).unwrap().size, 3);
        let mut got = String::new();
        p.read(&list[0].path)
            .unwrap()
            .read_to_string(&mut got)
            .unwrap();
        assert_eq!(got, "abc");
    }

    #[test]
    fn reads_large_files_in_ranges() {
        let data: Vec<u8> = (0..(CHUNK as usize * 2 + 10)).map(|i| i as u8).collect();
        let (p, calls) = mock(&data);
        let mut got = Vec::new();
        p.read(&VPath::parse("library://s1/big").unwrap())
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        assert_eq!(got, data);
        let offsets: Vec<u64> = calls
            .lock()
            .iter()
            .filter(|(m, _)| m == "read")
            .map(|(_, v)| v["offset"].as_u64().unwrap())
            .collect();
        assert_eq!(offsets, [0, CHUNK, CHUNK * 2]);
    }

    #[test]
    fn removes_through_plan_and_execute_and_waits_for_the_job() {
        let (p, calls) = mock(b"");
        p.remove(&VPath::parse("library://s1/a.txt").unwrap())
            .unwrap();
        let calls = calls.lock();
        let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(methods, ["plan", "execute", "jobs.info", "jobs.info"]);
        assert_eq!(calls[0].1["op"], "delete");
        // Exactly the previewed plan is confirmed.
        assert_eq!(calls[1].1, json!({"plan_id": "p1", "input_hash": "h1"}));
    }

    #[test]
    fn renames_in_place_and_moves_by_name() {
        let (p, calls) = mock(b"");
        let a = VPath::parse("library://s1/d/a.txt").unwrap();
        p.rename(&a, &VPath::parse("library://s1/d/b.txt").unwrap())
            .unwrap();
        p.rename(&a, &VPath::parse("library://s1/e/a.txt").unwrap())
            .unwrap();
        assert!(p
            .rename(&a, &VPath::parse("library://s1/e/b.txt").unwrap())
            .is_err());
        let plans: Vec<Value> = calls
            .lock()
            .iter()
            .filter(|(m, _)| m == "plan")
            .map(|(_, v)| v.clone())
            .collect();
        assert_eq!(plans[0]["op"], "rename");
        assert_eq!(plans[0]["new_name"], "b.txt");
        assert_eq!(plans[1]["op"], "move");
        assert_eq!(plans[1]["to"], "library://s1/e");
    }

    #[test]
    fn cut_listings_and_writes_fail() {
        let (p, _) = mock(b"");
        let huge = VPath::parse("library://s1/huge").unwrap();
        assert_eq!(p.list(&huge).unwrap().len(), 2);
        assert!(p.list_complete(&huge).is_err());
        assert!(p.write(&huge).is_err());
        assert_eq!(p.remove_kind(), RemoveKind::Permanent);
    }
}
