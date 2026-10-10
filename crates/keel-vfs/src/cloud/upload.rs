//! Uploads: one request on `flush()` for a file of at most one chunk, else chunks of
//! `CHUNK` bytes through the service's own upload sessions (Drive resumable uploads,
//! Dropbox upload sessions, S3 multipart uploads), each chunk with the usual retries and
//! token refresh. Errors carry the HTTP status only, never server text, URLs or tokens.
use super::*;
use serde_json::{json, Value};

/// Bytes per chunk: a multiple of 256 KiB (Drive) and at least 5 MiB (S3 parts but the
/// last). At most one chunk (plus one `write`) is held in memory.
pub(super) const CHUNK: usize = 8 << 20;
/// How long one chunk request may take (8 MiB at about 14 KB/s).
const CHUNK_TIMEOUT: Duration = Duration::from_secs(600);
/// Service API requests: lookups, quota, links, starting and finishing sessions.
pub(super) const API_TIMEOUT: Duration = Duration::from_secs(30);
/// S3 takes at most this many parts per object (so 8 MiB parts cap an object at 78 GiB).
const S3_MAX_PARTS: usize = 10_000;

/// A service's answer: status, headers and the whole body.
pub(super) struct Answer {
    pub status: u16,
    headers: http::HeaderMap,
    pub body: Vec<u8>,
}
impl Answer {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }
    fn success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

pub(super) fn http_error(p: &VPath, status: u16) -> anyhow::Error {
    let kind = match status {
        401 | 403 => io::ErrorKind::PermissionDenied,
        404 => io::ErrorKind::NotFound,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("{}: cloud HTTP {status}", p.display())).into()
}
pub(super) fn cancelled() -> anyhow::Error {
    io::Error::new(io::ErrorKind::Interrupted, "cancelled").into()
}
fn unreachable(p: &VPath) -> anyhow::Error {
    io::Error::new(
        io::ErrorKind::ConnectionRefused,
        format!("{}: cannot reach the cloud service", p.display()),
    )
    .into()
}
fn exists(p: &VPath) -> anyhow::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{}: destination exists", p.display()),
    )
    .into()
}

/// Runs `work` on the shared runtime until it ends, or until `cancel` or `stop` is set:
/// then its task is aborted, which drops a request in flight (closing its connection), and
/// None is returned. The flags are checked every 20 ms.
pub(super) fn until_cancelled<T: Send + 'static>(
    cancel: &AtomicBool,
    stop: &AtomicBool,
    work: impl std::future::Future<Output = T> + Send + 'static,
) -> Option<T> {
    let runtime = crate::sftp::conn::runtime();
    let mut task = runtime.spawn(work);
    runtime.block_on(async {
        loop {
            tokio::select! {
                done = &mut task => return match done {
                    Ok(value) => Some(value),
                    Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                    Err(_) => None,
                },
                () = tokio::time::sleep(Duration::from_millis(20)) => {
                    if cancel.load(Ordering::SeqCst) || stop.load(Ordering::SeqCst) {
                        task.abort();
                        return None;
                    }
                }
            }
        }
    })
}

pub(super) fn request(
    method: http::Method,
    url: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> Result<http::Request<Vec<u8>>> {
    let mut req = http::Request::builder().method(method).uri(url);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    Ok(req.body(body)?)
}

/// SigV4 signing and object URLs for the account's bucket: the multipart requests opendal's
/// S3 service makes only inside its own writer.
pub(super) struct S3Api {
    signer: reqsign_core::Signer<reqsign_aws_v4::Credential>,
    /// `<endpoint>/<bucket>` (path style, like opendal's S3 service).
    base: String,
    /// The account's root as a key prefix: `a/b/`, or empty.
    root: String,
}
impl S3Api {
    pub(super) fn new(account: &CloudAccount, key_id: &str, secret: &str) -> Result<Self> {
        let cfg = account
            .s3
            .as_ref()
            .context("S3 account without endpoint, region and bucket")?;
        let keys = S3Keys(reqsign_aws_v4::StaticCredentialProvider::new(
            key_id, secret,
        ));
        let signer = reqsign_core::Signer::new(
            reqsign_core::Context::new(),
            keys,
            reqsign_aws_v4::RequestSigner::new("s3", &cfg.region),
        );
        let root = (account.root.as_deref().unwrap_or("").split('/'))
            .filter(|s| !s.is_empty())
            .map(|s| format!("{s}/"))
            .collect();
        Ok(Self {
            signer,
            base: s3_base(cfg),
            root,
        })
    }
}
/// The bucket's URL as opendal's S3 service builds it (path style).
fn s3_base(cfg: &S3Config) -> String {
    let mut endpoint = cfg.endpoint.trim().to_owned();
    if !endpoint.starts_with("http") {
        endpoint = format!("https://{endpoint}");
    }
    endpoint = endpoint.replace(&format!("//{}.", cfg.bucket), "//");
    if let Ok(url) = url::Url::parse(&endpoint) {
        endpoint = url.to_string();
    }
    let mut endpoint = endpoint.trim_end_matches('/').to_owned();
    if endpoint == "https://s3.amazonaws.com" {
        endpoint = format!("https://s3.{}.amazonaws.com", cfg.region);
    }
    format!("{endpoint}/{}", cfg.bucket)
}
/// Percent-encodes all but the unreserved characters (and `/` when `slash`).
fn encode(s: &str, slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) || (slash && b == b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
/// The text of the first `<tag>` element.
fn xml_text(body: &[u8], tag: &str) -> Option<String> {
    let s = std::str::from_utf8(body).ok()?;
    let start = s.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = start + s[start..].find(&format!("</{tag}>"))?;
    Some(
        s[start..end]
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
/// JSON for Dropbox's `Dropbox-API-Arg` header: anything past ASCII as `\uXXXX`.
fn header_json(v: &Value) -> String {
    let mut out = String::new();
    for c in v.to_string().chars() {
        if c.is_ascii() && c != '\x7f' {
            out.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

impl Core {
    /// Whether uploads of more than one chunk go through the service's upload sessions
    /// (else the whole file is held in memory and sent on `flush()`).
    pub(super) fn chunked(&self) -> bool {
        match self.account.kind {
            CloudKind::GoogleDrive | CloudKind::Dropbox => self.oauth.is_some(),
            CloudKind::S3 => self.s3.is_some(),
            CloudKind::WebDav => false,
        }
    }

    /// The account's credentials on `req`: its bearer token, or the S3 signature.
    fn authorize(&self, req: &mut http::Request<Vec<u8>>) -> Result<()> {
        if let Some(oauth) = &self.oauth {
            let mut token = http::HeaderValue::from_str(&format!("Bearer {}", oauth.access.lock()))
                .map_err(|_| sign_in_again())?;
            token.set_sensitive(true);
            req.headers_mut().insert(http::header::AUTHORIZATION, token);
        } else if let Some(s3) = &self.s3 {
            let (mut parts, body) = std::mem::take(req).into_parts();
            crate::sftp::conn::runtime()
                .block_on(s3.signer.sign(&mut parts, None))
                .map_err(|_| anyhow::anyhow!("the S3 request could not be signed"))?;
            *req = http::Request::from_parts(parts, body);
        }
        Ok(())
    }

    /// One request with the account's credentials, made by `make` (again for each try):
    /// refreshes an expiring token first and once on HTTP 401, and retries 429 / 5xx /
    /// unanswered requests with `backoff`, `tries` times in all. Returns any other answer;
    /// ends, with `cancelled()`, once `cancel` or the account's `stop` is set.
    pub(super) fn exchange(
        &self,
        p: &VPath,
        cancel: &AtomicBool,
        tries: u32,
        timeout: Duration,
        make: &dyn Fn() -> Result<http::Request<Vec<u8>>>,
    ) -> Result<Answer> {
        let halted = || cancel.load(Ordering::SeqCst) || self.stop.load(Ordering::SeqCst);
        if let Some(oauth) = &self.oauth {
            self.ensure_signed_in(oauth)?;
            if Self::expiring(oauth) {
                self.refresh(None)?;
            }
        }
        let client = http_client()?;
        let (mut attempt, mut refreshed) = (0, false);
        loop {
            if halted() {
                return Err(cancelled());
            }
            let generation = self.generation.load(Ordering::SeqCst);
            let mut req = make()?;
            self.authorize(&mut req)?;
            let mut req = reqwest::Request::try_from(req).context("invalid cloud request")?;
            *req.timeout_mut() = Some(timeout);
            let client = client.clone();
            // A cancel ends the request on the wire, not after it.
            let answer = until_cancelled(cancel, &self.stop, async move {
                let reply = client.execute(req).await?;
                let (status, headers) = (reply.status().as_u16(), reply.headers().clone());
                let body = reply.bytes().await?.to_vec();
                Ok::<_, reqwest::Error>(Answer {
                    status,
                    headers,
                    body,
                })
            })
            .ok_or_else(cancelled)?;
            let status = match answer {
                Ok(a) if a.status == 401 && self.oauth.is_some() && !refreshed => {
                    refreshed = true;
                    self.refresh(Some(generation))?;
                    continue;
                }
                Ok(a) if a.status != 429 && !(500..600).contains(&a.status) => return Ok(a),
                Ok(a) => Some(a.status),
                Err(_) => None,
            };
            attempt += 1;
            match backoff(attempt - 1, jitter()).filter(|_| attempt < tries) {
                Some(delay) => {
                    tracing::debug!(attempt, ?delay, "cloud request retry");
                    sleep_unless_cancelled(delay, &[cancel, &self.stop])?;
                }
                None => return Err(status.map_or_else(|| unreachable(p), |s| http_error(p, s))),
            }
        }
    }

    // --- Google Drive resumable uploads -----------------------------------------------

    /// Starts a resumable upload of `p`: a new file in its folder, or (unless `exclusive`)
    /// a new version of the file already there. Returns the session URL.
    fn drive_session(&self, p: &VPath, cancel: &AtomicBool, exclusive: bool) -> Result<String> {
        let existing = match exclusive {
            true => None,
            false => match self.drive_id(p, cancel) {
                Ok(id) => Some(id),
                Err(e) if is_not_found(&e) => None,
                Err(e) => return Err(e),
            },
        };
        let (method, path, meta) = match existing {
            Some(id) => (
                http::Method::PATCH,
                format!("/upload/drive/v3/files/{id}?uploadType=resumable"),
                json!({}),
            ),
            None => {
                let folder = self.drive_id(&p.parent().context("no parent folder")?, cancel)?;
                (
                    http::Method::POST,
                    "/upload/drive/v3/files?uploadType=resumable".to_owned(),
                    json!({ "name": p.name(), "parents": [folder] }),
                )
            }
        };
        let url = format!("{}{path}", self.content);
        let meta = meta.to_string().into_bytes();
        let json = [("content-type", "application/json; charset=UTF-8")];
        let a = self.exchange(p, cancel, MAX_TRIES, API_TIMEOUT, &|| {
            request(method.clone(), &url, &json, meta.clone())
        })?;
        if !a.success() {
            return Err(http_error(p, a.status));
        }
        // The session URL gets the file's bytes (and the token): the service's own host only.
        a.header("location")
            .filter(|l| l.starts_with(&format!("{}/", self.content)))
            .map(str::to_owned)
            .context("the service answered without an upload session")
    }
    /// Sends `data`, bytes `offset..` of the file, to a Drive session (`total`: this is the
    /// last chunk). After a failed request the session is asked how much it holds (308 with
    /// `Range`) and the rest is sent from there.
    fn drive_chunk(
        &self,
        p: &VPath,
        cancel: &AtomicBool,
        url: &str,
        offset: u64,
        data: &[u8],
        total: Option<u64>,
    ) -> Result<()> {
        let end = offset + data.len() as u64;
        let size = total.map_or("*".to_owned(), |t| t.to_string());
        let (mut at, mut attempt) = (offset, 0);
        loop {
            let piece = data[(at - offset) as usize..].to_vec();
            let range = format!("bytes {at}-{}/{size}", end - 1);
            let answer = match self.exchange(p, cancel, 1, CHUNK_TIMEOUT, &|| {
                request(
                    http::Method::PUT,
                    url,
                    &[("content-range", &range)],
                    piece.clone(),
                )
            }) {
                Ok(a) => a,
                Err(e) if io_kind(&e) == Some(io::ErrorKind::Interrupted) => return Err(e),
                Err(e) => {
                    let Some(delay) = backoff(attempt, jitter()) else {
                        return Err(e);
                    };
                    tracing::debug!(attempt, ?delay, "drive chunk failed; asking the session");
                    sleep_unless_cancelled(delay, &[cancel, &self.stop])?;
                    attempt += 1;
                    let query = format!("bytes */{size}");
                    self.exchange(p, cancel, MAX_TRIES, API_TIMEOUT, &|| {
                        request(
                            http::Method::PUT,
                            url,
                            &[("content-range", &query)],
                            Vec::new(),
                        )
                    })?
                }
            };
            match answer.status {
                200 | 201 if total.is_some() => return Ok(()),
                308 => {
                    // `Range: bytes=0-<last byte held>`; none: nothing held yet.
                    let held = (answer.header("range"))
                        .and_then(|r| r.strip_prefix("bytes=0-")?.parse::<u64>().ok())
                        .map_or(0, |last| last + 1);
                    anyhow::ensure!(
                        (offset..=end).contains(&held),
                        "{}: the upload session lost data; copy the file again",
                        p.display()
                    );
                    if held == end && total.is_none() {
                        return Ok(());
                    }
                    if held <= at {
                        // Taken but not kept: give up after as many tries as a failure.
                        attempt += 1;
                        anyhow::ensure!(
                            attempt < MAX_TRIES && held < end,
                            "{}: the upload session does not take the data",
                            p.display()
                        );
                    }
                    at = held;
                }
                404 | 410 => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("{}: the upload session expired", p.display()),
                    )
                    .into())
                }
                s => return Err(http_error(p, s)),
            }
        }
    }

    // --- Dropbox upload sessions ------------------------------------------------------

    fn dropbox_call(
        &self,
        p: &VPath,
        cancel: &AtomicBool,
        endpoint: &str,
        arg: &Value,
        data: &[u8],
    ) -> Result<Answer> {
        let url = format!("{}/2/files/upload_session/{endpoint}", self.content);
        let arg = header_json(arg);
        let body = data.to_vec();
        self.exchange(p, cancel, MAX_TRIES, CHUNK_TIMEOUT, &|| {
            request(
                http::Method::POST,
                &url,
                &[
                    ("dropbox-api-arg", &arg),
                    ("content-type", "application/octet-stream"),
                ],
                body.clone(),
            )
        })
    }
    /// Starts a session with the file's first chunk; returns its id.
    fn dropbox_start(&self, p: &VPath, cancel: &AtomicBool, data: &[u8]) -> Result<String> {
        let a = self.dropbox_call(p, cancel, "start", &json!({ "close": false }), data)?;
        if !a.success() {
            return Err(dropbox_error(p, &a));
        }
        (a.json()["session_id"].as_str())
            .map(str::to_owned)
            .context("the service answered without an upload session")
    }
    /// How the last chunk is committed: Dropbox adds the file, or (not `exclusive`)
    /// replaces the one there, never renaming it.
    fn dropbox_commit(&self, p: &VPath, exclusive: bool) -> Value {
        json!({
            "path": self.service_path(p),
            "mode": if exclusive { "add" } else { "overwrite" },
            "autorename": false,
            "mute": true,
            "strict_conflict": false,
        })
    }
    /// Appends `data`, bytes `offset..` of the file, to session `id`, or finishes it with
    /// them when `commit` is given. A chunk that reached Dropbox although its answer was
    /// lost is answered `incorrect_offset` on the retry, with how much the session holds:
    /// the rest is sent from there.
    fn dropbox_send(
        &self,
        p: &VPath,
        cancel: &AtomicBool,
        id: &str,
        offset: u64,
        data: &[u8],
        commit: Option<Value>,
    ) -> Result<()> {
        let end = offset + data.len() as u64;
        let mut at = offset;
        loop {
            let cursor = json!({ "session_id": id, "offset": at });
            let (endpoint, arg) = match &commit {
                Some(commit) => ("finish", json!({ "cursor": cursor, "commit": commit })),
                None => ("append_v2", json!({ "cursor": cursor, "close": false })),
            };
            let rest = &data[(at - offset) as usize..];
            let a = self.dropbox_call(p, cancel, endpoint, &arg, rest)?;
            if a.success() {
                return Ok(());
            }
            let v = a.json();
            let error = &v["error"];
            let held = (error["correct_offset"].as_u64())
                .or(error["lookup_failed"]["correct_offset"].as_u64());
            match held {
                Some(held) if held > at && held <= end => {
                    if held == end && commit.is_none() {
                        return Ok(());
                    }
                    at = held;
                }
                _ => return Err(dropbox_error(p, &a)),
            }
        }
    }

    // --- S3 multipart uploads ---------------------------------------------------------

    fn s3_url(&self, p: &VPath) -> Result<String> {
        let s3 = self.s3.as_ref().context("no S3 bucket for this account")?;
        let object = format!("{}{}", s3.root, key(p, false));
        Ok(format!("{}/{}", s3.base, encode(&object, true)))
    }
    fn s3_create(&self, p: &VPath, cancel: &AtomicBool) -> Result<String> {
        let url = format!("{}?uploads", self.s3_url(p)?);
        let a = self.exchange(p, cancel, MAX_TRIES, API_TIMEOUT, &|| {
            request(http::Method::POST, &url, &[], Vec::new())
        })?;
        s3_ok(p, &a)?;
        xml_text(&a.body, "UploadId").context("the service answered without an upload id")
    }
    /// Uploads part `number` (from 1); returns its ETag.
    fn s3_part(
        &self,
        p: &VPath,
        cancel: &AtomicBool,
        id: &str,
        number: usize,
        data: &[u8],
    ) -> Result<String> {
        let url = format!(
            "{}?partNumber={number}&uploadId={}",
            self.s3_url(p)?,
            encode(id, false)
        );
        let body = data.to_vec();
        let a = self.exchange(p, cancel, MAX_TRIES, CHUNK_TIMEOUT, &|| {
            request(http::Method::PUT, &url, &[], body.clone())
        })?;
        s3_ok(p, &a)?;
        (a.header("etag").map(str::to_owned)).context("the service answered without an ETag")
    }
    /// Joins the parts into the object (refused when `exclusive` and it exists).
    fn s3_complete(
        &self,
        p: &VPath,
        cancel: &AtomicBool,
        id: &str,
        etags: &[String],
        exclusive: bool,
    ) -> Result<()> {
        let url = format!("{}?uploadId={}", self.s3_url(p)?, encode(id, false));
        let parts: String = (etags.iter().enumerate())
            .map(|(i, etag)| {
                format!(
                    "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                    i + 1,
                    xml_escape(etag)
                )
            })
            .collect();
        let body = format!("<CompleteMultipartUpload>{parts}</CompleteMultipartUpload>");
        let mut headers = vec![("content-type", "application/xml")];
        if exclusive {
            headers.push(("if-none-match", "*"));
        }
        // Joining big objects can take a while.
        let a = self.exchange(p, cancel, MAX_TRIES, CHUNK_TIMEOUT, &|| {
            request(
                http::Method::POST,
                &url,
                &headers,
                body.clone().into_bytes(),
            )
        })?;
        if a.status == 412 {
            return Err(exists(p));
        }
        s3_ok(p, &a)
    }
    /// Deletes an unfinished upload's parts (one try: this runs after a failure or a
    /// cancel). False when that failed: the bucket's lifecycle rules remove them then.
    fn s3_abort(&self, p: &VPath, id: &str) -> bool {
        let Ok(url) = self.s3_url(p) else {
            return false;
        };
        let url = format!("{url}?uploadId={}", encode(id, false));
        let a = self.exchange(p, &NEVER, 1, API_TIMEOUT, &|| {
            request(http::Method::DELETE, &url, &[], Vec::new())
        });
        match a {
            Ok(a) if a.success() || a.status == 404 => true,
            Ok(a) => {
                tracing::warn!(file = %p.display(), status = a.status, "S3 upload not aborted");
                false
            }
            Err(e) => {
                tracing::warn!(file = %p.display(), "S3 upload not aborted: {e:#}");
                false
            }
        }
    }
}
/// S3 answers some failures with 200 and an `<Error>` body.
fn s3_ok(p: &VPath, a: &Answer) -> Result<()> {
    match a.success() && xml_text(&a.body, "Code").is_none() {
        true => Ok(()),
        false if a.success() => Err(io::Error::other(format!(
            "{}: cloud HTTP {} with an error",
            p.display(),
            a.status
        ))
        .into()),
        false => Err(http_error(p, a.status)),
    }
}
fn dropbox_error(p: &VPath, a: &Answer) -> anyhow::Error {
    let summary = a.json()["error_summary"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    match a.status {
        409 if summary.contains("conflict") => exists(p),
        409 if summary.contains("insufficient_space") => {
            io::Error::other(format!("{}: the Dropbox account is full", p.display())).into()
        }
        s => http_error(p, s),
    }
}

/// An upload in progress on the service.
enum Session {
    /// The session URL.
    Drive(String),
    /// The session id.
    Dropbox(String),
    S3 {
        id: String,
        etags: Vec<String>,
    },
}

/// One upload, written straight to the target: the file (or object) only changes once the
/// upload completes, so no staging name is needed. Up to one chunk is buffered; a file of
/// at most one chunk goes in one request on `flush()`, a bigger one in chunks as it is
/// written (the last on `flush()`), through an upload session. `cancel` ends a request on
/// the wire as well as retry waits. Accounts without sessions (WebDAV) hold the whole file until `flush()`.
/// Dropped without `flush()`: nothing is written (an S3 upload is aborted, Drive and
/// Dropbox sessions expire on their own).
pub(super) struct CloudUpload<'c> {
    core: Arc<Core>,
    target: VPath,
    exclusive: bool,
    cancel: &'c AtomicBool,
    /// Bytes not sent yet.
    buf: Vec<u8>,
    /// Bytes sent in chunks.
    sent: u64,
    session: Option<Session>,
    chunked: bool,
    done: bool,
    failed: bool,
}
impl<'c> CloudUpload<'c> {
    pub(super) fn start(
        core: Arc<Core>,
        target: &VPath,
        exclusive: bool,
        cancel: &'c AtomicBool,
    ) -> Result<Self> {
        core.validate(target)?;
        anyhow::ensure!(target.parent().is_some(), "cannot write the account root");
        if exclusive && core.maybe_stat(target)?.is_some() {
            return Err(exists(target));
        }
        Ok(Self {
            chunked: core.chunked(),
            core,
            target: target.clone(),
            exclusive,
            cancel,
            buf: Vec::new(),
            sent: 0,
            session: None,
            done: false,
            failed: false,
        })
    }
    /// Sends `data`, the next bytes of the file, through the upload session (started with
    /// the first chunk); `last` finishes it.
    fn chunk(&mut self, data: Vec<u8>, last: bool) -> Result<()> {
        let (core, p, cancel) = (&*self.core, &self.target, self.cancel);
        let offset = self.sent;
        let total = last.then_some(offset + data.len() as u64);
        if self.session.is_none() {
            self.session = Some(match core.account.kind {
                CloudKind::GoogleDrive => {
                    Session::Drive(core.drive_session(p, cancel, self.exclusive)?)
                }
                CloudKind::S3 => Session::S3 {
                    id: core.s3_create(p, cancel)?,
                    etags: Vec::new(),
                },
                // The first chunk starts the session (it is never the last one).
                CloudKind::Dropbox => {
                    let id = core.dropbox_start(p, cancel, &data)?;
                    self.session = Some(Session::Dropbox(id));
                    self.sent += data.len() as u64;
                    return Ok(());
                }
                CloudKind::WebDav => anyhow::bail!("WebDAV has no upload sessions"),
            });
        }
        match self.session.as_mut().context("no upload session")? {
            Session::Drive(url) => core.drive_chunk(p, cancel, url, offset, &data, total)?,
            Session::Dropbox(id) => {
                let commit = last.then(|| core.dropbox_commit(p, self.exclusive));
                core.dropbox_send(p, cancel, id, offset, &data, commit)?
            }
            Session::S3 { id, etags } => {
                anyhow::ensure!(
                    etags.len() < S3_MAX_PARTS,
                    "{}: S3 objects can be at most {} GiB here ({S3_MAX_PARTS} parts of {} MiB)",
                    p.display(),
                    (S3_MAX_PARTS * core.chunk) >> 30,
                    core.chunk >> 20
                );
                etags.push(core.s3_part(p, cancel, id, etags.len() + 1, &data)?);
                if last {
                    core.s3_complete(p, cancel, id, etags, self.exclusive)?;
                }
            }
        }
        self.sent += data.len() as u64;
        Ok(())
    }
    /// Ends a session that will not be finished: an S3 upload is aborted (its parts would
    /// be kept, and billed, until the bucket's lifecycle rules remove them); Drive and
    /// Dropbox sessions expire on their own after about a week. Says what became of it.
    fn abandon(&mut self) -> &'static str {
        match self.session.take() {
            Some(Session::S3 { id, .. }) if self.core.s3_abort(&self.target, &id) => {
                "the unfinished S3 upload was aborted"
            }
            Some(Session::S3 { .. }) => {
                "the unfinished S3 upload could not be aborted; the bucket's lifecycle rules \
                 remove it"
            }
            Some(_) => "the unfinished upload session expires on its own",
            None => "nothing was stored",
        }
    }
    /// The upload failed (or was cancelled) with `e`: it ends here, and the error says how
    /// far it got.
    fn fail(&mut self, e: anyhow::Error) -> anyhow::Error {
        self.failed = true;
        let chunked = self.session.is_some();
        let left = self.abandon();
        let cancelled = io_kind(&e) == Some(io::ErrorKind::Interrupted);
        let what = format!(
            "upload to {} {} after {} had been sent ({left})",
            self.core.account.kind.name(),
            if cancelled { "cancelled" } else { "failed" },
            amount(self.sent)
        );
        let target = self.target.display();
        if cancelled {
            tracing::info!("{target}: {what}");
            return io::Error::new(io::ErrorKind::Interrupted, format!("{target}: {what}")).into();
        }
        tracing::warn!("{target}: {what}: {e:#}");
        match chunked {
            true => e.context(what),
            false => e,
        }
    }
    fn commit(&mut self) -> Result<()> {
        if self.done {
            return Ok(());
        }
        anyhow::ensure!(!self.failed, "upload failed: {}", self.target.display());
        if self.cancel.load(Ordering::Relaxed) {
            return Err(self.fail(cancelled()));
        }
        let result = match self.session {
            Some(_) => {
                let last = std::mem::take(&mut self.buf);
                self.chunk(last, true)
            }
            None => {
                let data = Buffer::from(std::mem::take(&mut self.buf));
                let k = key(&self.target, false);
                let opts = options::WriteOptions {
                    if_not_exists: self.exclusive && self.core.caps().write_with_if_not_exists,
                    ..Default::default()
                };
                let cancel = self.cancel;
                self.core
                    .call_cancellable(&self.target, cancel, |op| {
                        // Async, so that a cancel ends the request on the wire.
                        let op = opendal::Operator::from(op.clone());
                        let (k, data, opts) = (k.clone(), data.clone(), opts.clone());
                        until_cancelled(cancel, &NEVER, async move {
                            op.write_options(&k, data, opts).await
                        })
                        .unwrap_or_else(|| {
                            Err(opendal::Error::new(ErrorKind::Unexpected, "cancelled"))
                        })
                    })
                    .map(drop)
            }
        };
        self.core.invalidate(&self.target.path);
        match result {
            Ok(()) => {
                self.done = true;
                Ok(())
            }
            Err(e) => Err(self.fail(e)),
        }
    }
}
fn amount(bytes: u64) -> String {
    match bytes {
        b if b < 1 << 10 => format!("{b} bytes"),
        b if b < 1 << 20 => format!("{} KiB", b >> 10),
        b => format!("{} MiB", b >> 20),
    }
}
fn to_io(e: anyhow::Error) -> io::Error {
    io::Error::new(
        io_kind(&e).unwrap_or(io::ErrorKind::Other),
        format!("{e:#}"),
    )
}
impl Write for CloudUpload<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.done || self.failed {
            return Err(io::Error::other("upload finished or failed"));
        }
        let limit = self.core.upload_limit.filter(|_| !self.chunked);
        if limit.is_some_and(|limit| (self.buf.len() + bytes.len()) as u64 > limit) {
            self.failed = true;
            return Err(io::Error::other(format!(
                "{}: files over {} MB cannot be uploaded to {} yet",
                self.target.display(),
                limit.unwrap_or_default() >> 20,
                self.core.account.kind.name()
            )));
        }
        self.buf.extend_from_slice(bytes);
        while self.chunked && self.buf.len() > self.core.chunk {
            let rest = self.buf.split_off(self.core.chunk);
            let piece = std::mem::replace(&mut self.buf, rest);
            if let Err(e) = self.chunk(piece, false) {
                let e = to_io(self.fail(e));
                // `write_all` and `io::copy` retry `Interrupted`: a cancel must end them.
                return Err(match e.kind() {
                    io::ErrorKind::Interrupted => io::Error::other(e.to_string()),
                    _ => e,
                });
            }
        }
        Ok(bytes.len())
    }
    /// Commits the upload (see `Provider::write`).
    fn flush(&mut self) -> io::Result<()> {
        self.commit().map_err(to_io)
    }
}
impl Drop for CloudUpload<'_> {
    fn drop(&mut self) {
        if !self.done && !self.failed {
            let left = self.abandon();
            tracing::error!(
                target = %self.target.display(),
                "cloud upload dropped without flush(); discarding it ({left})"
            );
        }
    }
}
