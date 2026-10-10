use super::*;
use crate::{ops, Conflict, Router};
use serde_json::{json, Value};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    fs,
    sync::atomic::AtomicUsize,
};

fn account(id: &str, kind: CloudKind) -> CloudAccount {
    CloudAccount {
        id: id.into(),
        label: format!("{id} label"),
        kind,
        root: None,
        client_id_override: None,
        s3: None,
        webdav: None,
    }
}
/// A cloud account of `kind` backed by opendal's in-memory service (no native rename or
/// copy, one-request writes).
fn memory_cloud_of(id: &str, kind: CloudKind) -> CloudProvider {
    let op = opendal::Operator::new(opendal::services::Memory::default()).unwrap();
    CloudProvider::with_operator(account(id, kind), op, crossbeam_channel::unbounded().0).unwrap()
}
/// S3 semantics (permanent delete, recursive folder delete).
fn memory_cloud(id: &str) -> CloudProvider {
    memory_cloud_of(id, CloudKind::S3)
}
fn vp(s: &str) -> VPath {
    VPath::parse(s).unwrap()
}
fn put(p: &CloudProvider, path: &str, data: &[u8]) {
    let mut w = p.write(&vp(path)).unwrap();
    w.write_all(data).unwrap();
    w.flush().unwrap();
}
fn get(p: &CloudProvider, path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    p.read(&vp(path)).unwrap().read_to_end(&mut out).unwrap();
    out
}
fn names(entries: &[Entry]) -> Vec<String> {
    let mut n: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
    n.sort();
    n
}
fn kind_of(e: &anyhow::Error) -> Option<io::ErrorKind> {
    io_kind(e)
}
/// An opendal error as a service answering `status` produces it.
fn http_error(status: u16) -> opendal::Error {
    opendal::Error::new(ErrorKind::Unexpected, "server says no").with_context(
        "response",
        format!("Parts {{ status: {status}, version: HTTP/1.1, headers: {{}} }}"),
    )
}

// --- Local HTTP fakes ---------------------------------------------------------------

/// One request to a fake service.
struct Req {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}
type Reply = (u16, Vec<(&'static str, String)>, Vec<u8>);
/// Serves `handle` on a loopback port; returns `http://127.0.0.1:<port>`.
fn serve(handle: impl Fn(Req) -> Reply + Send + 'static) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for mut request in server.incoming_requests() {
            let url = url::Url::parse(&format!("http://x{}", request.url())).unwrap();
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body).unwrap();
            let req = Req {
                method: request.method().as_str().to_owned(),
                path: url.path().to_owned(),
                query: url.query_pairs().into_owned().collect(),
                headers: request
                    .headers()
                    .iter()
                    .map(|h| {
                        (
                            h.field.as_str().as_str().to_ascii_lowercase(),
                            h.value.as_str().to_owned(),
                        )
                    })
                    .collect(),
                body,
            };
            let (status, headers, body) = handle(req);
            let mut response = tiny_http::Response::from_data(body).with_status_code(status);
            for (name, value) in headers {
                response.add_header(tiny_http::Header::from_bytes(name, value).unwrap());
            }
            let _ = request.respond(response);
        }
    });
    format!("http://127.0.0.1:{port}")
}
fn json_reply(status: u16, body: Value) -> Reply {
    (
        status,
        vec![("Content-Type", "application/json".into())],
        body.to_string().into_bytes(),
    )
}

/// A minimal S3 bucket `b` (path-style): ListObjectsV2, HEAD, GET, PUT (and copy), DELETE.
#[derive(Default)]
struct S3Fake {
    objects: BTreeMap<String, Vec<u8>>,
    /// (method, key, status): the next matching request fails once.
    fail: Vec<(&'static str, String, u16)>,
    /// Once the first key is deleted, the second appears (another client's upload).
    on_delete: Option<(String, String)>,
}
const HTTP_DATE: &str = "Fri, 09 Oct 2026 12:00:00 GMT";
const ISO_DATE: &str = "2026-10-09T12:00:00.000Z";
fn s3_fake() -> (String, Arc<Mutex<S3Fake>>) {
    let state = Arc::new(Mutex::new(S3Fake::default()));
    let shared = state.clone();
    let base = serve(move |req| s3_reply(&mut shared.lock(), req));
    (base, state)
}
fn s3_reply(s: &mut S3Fake, req: Req) -> Reply {
    let key = req
        .path
        .strip_prefix("/b")
        .unwrap_or(&req.path)
        .trim_start_matches('/')
        .to_owned();
    if let Some(i) = s
        .fail
        .iter()
        .position(|(m, k, _)| *m == req.method && *k == key)
    {
        let (_, _, status) = s.fail.remove(i);
        return (
            status,
            vec![],
            b"<Error><Code>AccessDenied</Code><Message>no</Message></Error>".to_vec(),
        );
    }
    let found = |data: &[u8]| -> Reply {
        (
            200,
            vec![
                ("Last-Modified", HTTP_DATE.into()),
                ("ETag", "\"e\"".into()),
            ],
            data.to_vec(),
        )
    };
    let missing = || -> Reply {
        (
            404,
            vec![],
            b"<Error><Code>NoSuchKey</Code></Error>".to_vec(),
        )
    };
    match req.method.as_str() {
        "GET" if key.is_empty() => {
            assert_eq!(req.query.get("list-type").map(String::as_str), Some("2"));
            let prefix = req.query.get("prefix").cloned().unwrap_or_default();
            let delimiter = req.query.get("delimiter").cloned().unwrap_or_default();
            let (mut contents, mut prefixes) = (String::new(), BTreeSet::new());
            for (k, data) in s.objects.range(prefix.clone()..) {
                let Some(rest) = k.strip_prefix(&prefix) else {
                    break;
                };
                match rest.find(&delimiter).filter(|_| !delimiter.is_empty()) {
                    Some(i) => {
                        prefixes.insert(format!("{prefix}{}", &rest[..=i]));
                    }
                    None => contents.push_str(&format!(
                        "<Contents><Key>{k}</Key><Size>{}</Size><LastModified>{ISO_DATE}\
                         </LastModified><ETag>\"e\"</ETag></Contents>",
                        data.len()
                    )),
                }
            }
            let prefixes: String = prefixes
                .iter()
                .map(|p| format!("<CommonPrefixes><Prefix>{p}</Prefix></CommonPrefixes>"))
                .collect();
            let xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult><Name>b</Name>\
                 <Prefix>{prefix}</Prefix><IsTruncated>false</IsTruncated>{contents}{prefixes}\
                 </ListBucketResult>"
            );
            (200, vec![], xml.into_bytes())
        }
        "HEAD" | "GET" => match s.objects.get(&key) {
            Some(data) => found(data),
            None if key.ends_with('/') && s.objects.keys().any(|k| k.starts_with(&key)) => {
                found(b"")
            }
            None => missing(),
        },
        "PUT" => {
            if let Some(source) = req.headers.get("x-amz-copy-source") {
                let source = source.replace("%2F", "/");
                let source = source.trim_start_matches('/').trim_start_matches("b/");
                let Some(data) = s.objects.get(source).cloned() else {
                    return missing();
                };
                s.objects.insert(key, data);
                let xml = format!(
                    "<CopyObjectResult><ETag>\"e\"</ETag><LastModified>{ISO_DATE}\
                     </LastModified></CopyObjectResult>"
                );
                return (200, vec![], xml.into_bytes());
            }
            if req.headers.get("if-none-match").is_some_and(|v| v == "*")
                && s.objects.contains_key(&key)
            {
                return (
                    412,
                    vec![],
                    b"<Error><Code>PreconditionFailed</Code></Error>".to_vec(),
                );
            }
            s.objects.insert(key, req.body);
            (200, vec![("ETag", "\"e\"".into())], vec![])
        }
        "DELETE" => {
            s.objects.remove(&key);
            if let Some((_, appears)) = s.on_delete.take_if(|(after, _)| *after == key) {
                s.objects.insert(appears, b"late".to_vec());
            }
            (204, vec![], vec![])
        }
        "POST" if req.query.contains_key("delete") => {
            let body = String::from_utf8(req.body).unwrap();
            let mut deleted = String::new();
            for part in body.split("<Key>").skip(1) {
                let k = part.split("</Key>").next().unwrap();
                s.objects.remove(k);
                deleted.push_str(&format!("<Deleted><Key>{k}</Key></Deleted>"));
            }
            let xml = format!("<DeleteResult>{deleted}</DeleteResult>");
            (200, vec![], xml.into_bytes())
        }
        _ => (501, vec![], vec![]),
    }
}
fn s3_cloud(base: &str) -> CloudProvider {
    let store = Arc::new(MemoryStore::default());
    store.set("fake/access_key_id", "AKIDFAKE").unwrap();
    store.set("fake/secret_access_key", "fake-secret").unwrap();
    let mut fake = account("fake", CloudKind::S3);
    fake.s3 = Some(S3Config {
        endpoint: base.into(),
        region: "us-east-1".into(),
        bucket: "b".into(),
    });
    CloudProvider::connect(&fake, store, crossbeam_channel::unbounded().0).unwrap()
}

/// Sends an operator's requests to a fake service instead of the real host.
#[derive(Debug)]
struct ToFake(String);
impl opendal::raw::Layer for ToFake {
    fn apply_context(
        &self,
        _: opendal::raw::Servicer,
        inner: opendal::OperationContext,
    ) -> opendal::OperationContext {
        let redirect = Redirect {
            base: self.0.clone(),
            inner: inner.http_transport().clone(),
        };
        inner.with_http_transport(opendal::HttpTransporter::new(redirect))
    }
}
struct Redirect {
    base: String,
    inner: opendal::HttpTransporter,
}
impl opendal::HttpTransport for Redirect {
    async fn fetch(
        &self,
        mut req: http::Request<Buffer>,
    ) -> opendal::Result<http::Response<opendal::HttpBody>> {
        let path = req
            .uri()
            .path_and_query()
            .map_or("/".to_owned(), |p| p.as_str().to_owned());
        *req.uri_mut() = format!("{}{path}", self.base).parse().unwrap();
        self.inner.fetch(req).await
    }
}

/// A minimal Dropbox: get_metadata, upload, move_v2, delete_v2, create_folder_v2,
/// list_folder. Paths as opendal sends them (`/dir/name`, the root is ``).
#[derive(Default)]
struct DropboxFake {
    files: BTreeMap<String, Vec<u8>>,
    folders: BTreeSet<String>,
    /// (endpoint, path, status, error summary): the next matching request fails once.
    fail: Vec<(&'static str, String, u16, &'static str)>,
    /// (endpoint, path) of every request.
    log: Vec<(String, String)>,
}
impl DropboxFake {
    fn meta(&self, path: &str) -> Option<Value> {
        let name = path.rsplit('/').next().unwrap_or_default();
        if let Some(data) = self.files.get(path) {
            return Some(json!({
                ".tag": "file", "name": name, "path_display": path, "id": "id:f",
                "size": data.len(), "rev": "1",
                "client_modified": "2026-10-09T12:00:00Z",
                "server_modified": "2026-10-09T12:00:00Z",
            }));
        }
        self.folders
            .contains(path)
            .then(|| json!({".tag": "folder", "name": name, "path_display": path, "id": "id:d"}))
    }
    fn calls(&self, endpoint: &str) -> Vec<String> {
        self.log
            .iter()
            .filter(|(e, _)| e == endpoint)
            .map(|(_, p)| p.clone())
            .collect()
    }
}
fn dropbox_fake() -> (String, Arc<Mutex<DropboxFake>>) {
    let state = Arc::new(Mutex::new(DropboxFake::default()));
    let shared = state.clone();
    let base = serve(move |req| dropbox_reply(&mut shared.lock(), req));
    (base, state)
}
fn dropbox_reply(s: &mut DropboxFake, req: Req) -> Reply {
    let endpoint = req.path.trim_start_matches("/2/files/").to_owned();
    let arg: Value = match req.headers.get("dropbox-api-arg") {
        Some(arg) => serde_json::from_str(arg).unwrap(),
        None => serde_json::from_slice(&req.body).unwrap_or_default(),
    };
    let path = arg["path"]
        .as_str()
        .or(arg["from_path"].as_str())
        .unwrap_or_default()
        .to_owned();
    s.log.push((endpoint.clone(), path.clone()));
    if let Some(i) = s
        .fail
        .iter()
        .position(|(e, p, ..)| *e == endpoint && *p == path)
    {
        let (_, _, status, summary) = s.fail.remove(i);
        return json_reply(status, json!({ "error_summary": summary }));
    }
    let not_found = || json_reply(409, json!({"error_summary": "path/not_found/.."}));
    match endpoint.as_str() {
        "get_metadata" => s.meta(&path).map_or_else(not_found, |m| json_reply(200, m)),
        "upload" => {
            s.files.insert(path.clone(), req.body);
            json_reply(200, s.meta(&path).unwrap())
        }
        "create_folder_v2" => {
            s.folders.insert(path.clone());
            json_reply(200, json!({ "metadata": s.meta(&path) }))
        }
        "move_v2" | "delete_v2" => {
            if s.meta(&path).is_none() {
                return not_found();
            }
            let to = arg["to_path"].as_str().map(str::to_owned);
            if to.as_deref().is_some_and(|to| s.meta(to).is_some()) {
                return json_reply(409, json!({"error_summary": "to/conflict/file/.."}));
            }
            let under = format!("{path}/");
            let moved = |p: &String| *p == path || p.starts_with(&under);
            let rebase = |p: &str| to.as_ref().map(|to| format!("{to}{}", &p[path.len()..]));
            let files: Vec<_> = s.files.keys().filter(|p| moved(p)).cloned().collect();
            for p in files {
                let data = s.files.remove(&p).unwrap();
                if let Some(p) = rebase(&p) {
                    s.files.insert(p, data);
                }
            }
            let folders: Vec<_> = s.folders.iter().filter(|p| moved(p)).cloned().collect();
            for p in folders {
                s.folders.remove(&p);
                if let Some(p) = rebase(&p) {
                    s.folders.insert(p);
                }
            }
            json_reply(200, json!({ "metadata": {"name": "x"} }))
        }
        "list_folder" => {
            let parent = |p: &str| p.rsplit_once('/').map_or("", |(d, _)| d).to_owned();
            let entries: Vec<_> = s
                .files
                .keys()
                .chain(&s.folders)
                .filter(|p| parent(p) == path)
                .map(|p| s.meta(p).unwrap())
                .collect();
            json_reply(
                200,
                json!({"entries": entries, "cursor": "c", "has_more": false}),
            )
        }
        _ => (501, vec![], vec![]),
    }
}
fn dropbox_op(base: &str, access: &str) -> opendal::Operator {
    opendal::Operator::new(
        opendal::services::Dropbox::default()
            .root("/")
            .access_token(access),
    )
    .unwrap()
    .layer(ToFake(base.to_owned()))
}

/// A token endpoint answering every refresh with `reply` (status, JSON body); also returns
/// the number of requests it got.
fn token_server(status: u16, reply: &'static str) -> (String, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    let base = serve(move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        (status, vec![], reply.as_bytes().to_vec())
    });
    (format!("{base}/token"), count)
}
/// A Dropbox account "dbx" with refresh token "rt" (in the per-field form older builds
/// wrote), whose operator for an access token comes from `op_for`.
fn oauth_cloud_over(
    token_url: String,
    expires_at: SystemTime,
    op_for: impl Fn(&str) -> opendal::Operator + Send + Sync + 'static,
) -> (
    CloudProvider,
    Arc<MemoryStore>,
    crossbeam_channel::Receiver<RemoteEvent>,
) {
    let store = Arc::new(MemoryStore::default());
    store.set("dbx/refresh_token", "rt").unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut endpoints = oauth::Endpoints::for_kind(CloudKind::Dropbox).unwrap();
    endpoints.token = token_url;
    let client = OAuthClient {
        id: "app".into(),
        secret: None,
    };
    init().unwrap();
    let op = op_for("at-1");
    let oauth = OAuth::new(
        client,
        endpoints,
        "at-1",
        expires_at,
        Box::new(move |access| Ok(op_for(access))),
    );
    let p = CloudProvider::build(
        account("dbx", CloudKind::Dropbox),
        op,
        Some(oauth),
        store.clone(),
        tx,
    )
    .unwrap();
    (p, store, rx)
}
fn oauth_cloud(
    token_url: String,
    expires_at: SystemTime,
) -> (
    CloudProvider,
    Arc<MemoryStore>,
    crossbeam_channel::Receiver<RemoteEvent>,
) {
    let op = opendal::Operator::new(opendal::services::Memory::default()).unwrap();
    oauth_cloud_over(token_url, expires_at, move |_| op.clone())
}
fn stored(store: &MemoryStore, id: &str) -> OAuthTokens {
    load_tokens(store, id).unwrap().unwrap()
}

// --- Configuration, secrets, CPU ---------------------------------------------------

#[test]
fn config_round_trip_keeps_secrets_out_of_toml() {
    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Config {
        clouds: Vec<CloudAccount>,
    }
    let mut s3 = account("b2", CloudKind::S3);
    s3.s3 = Some(S3Config {
        endpoint: "https://s3.example.invalid".into(),
        region: "us-west-002".into(),
        bucket: "photos".into(),
    });
    s3.root = Some("/backup".into());
    let mut drive = account("drive", CloudKind::GoogleDrive);
    drive.client_id_override = Some("my-app.apps.googleusercontent.com".into());
    let config = Config {
        clouds: vec![drive, s3, account("dbx", CloudKind::Dropbox)],
    };
    let store = MemoryStore::default();
    store_tokens(
        &store,
        "drive",
        &OAuthTokens {
            access: "ya29.access-secret".into(),
            refresh: Some("1//refresh-secret".into()),
            expires_at: UNIX_EPOCH + Duration::from_secs(2_000_000_000),
        },
    )
    .unwrap();
    store.set("b2/access_key_id", "KEYID-secret").unwrap();
    store.set("b2/secret_access_key", "KEY-secret").unwrap();
    let text = toml::to_string(&config).unwrap();
    assert!(
        text.contains("[[clouds]]") && text.contains("GoogleDrive"),
        "{text}"
    );
    assert!(!text.contains("secret"), "no secret in config: {text}");
    assert_eq!(toml::from_str::<Config>(&text).unwrap(), config);
    // One keychain entry holds all tokens (saved atomically).
    assert_eq!(
        store.keys(),
        ["b2/access_key_id", "b2/secret_access_key", "drive/tokens"]
    );
    let tokens = stored(&store, "drive");
    assert_eq!(tokens.access, "ya29.access-secret");
    assert_eq!(
        tokens.expires_at,
        UNIX_EPOCH + Duration::from_secs(2_000_000_000)
    );
    // A refresh reply without a new refresh token keeps the stored one.
    store_tokens(
        &store,
        "drive",
        &OAuthTokens {
            access: "new".into(),
            refresh: None,
            expires_at: UNIX_EPOCH,
        },
    )
    .unwrap();
    assert_eq!(
        stored(&store, "drive").refresh.as_deref(),
        Some("1//refresh-secret")
    );
    forget_account(&store, "drive");
    assert_eq!(store.keys(), ["b2/access_key_id", "b2/secret_access_key"]);
}

#[test]
fn tokens_of_earlier_builds_are_read_and_replaced_by_one_entry() {
    let store = MemoryStore::default();
    store.set("old/access_token", "at").unwrap();
    store.set("old/refresh_token", "rt").unwrap();
    store.set("old/expires_at", "2000000000").unwrap();
    let t = stored(&store, "old");
    assert_eq!(
        (t.access.as_str(), t.refresh.as_deref()),
        ("at", Some("rt"))
    );
    assert_eq!(
        t.expires_at,
        UNIX_EPOCH + Duration::from_secs(2_000_000_000)
    );
    store_tokens(
        &store,
        "old",
        &OAuthTokens {
            access: "at-2".into(),
            refresh: None,
            expires_at: UNIX_EPOCH,
        },
    )
    .unwrap();
    assert_eq!(store.keys(), ["old/tokens"]);
    assert_eq!(stored(&store, "old").refresh.as_deref(), Some("rt"));
    // An access token too long for Windows Credential Manager is not kept.
    store_tokens(
        &store,
        "old",
        &OAuthTokens {
            access: "x".repeat(2000),
            refresh: Some("rt-3".into()),
            expires_at: UNIX_EPOCH + Duration::from_secs(2_000_000_000),
        },
    )
    .unwrap();
    let json = store.get("old/tokens").unwrap().unwrap();
    assert!(json.len() < KEYCHAIN_MAX_CHARS, "{}", json.len());
    let t = stored(&store, "old");
    assert_eq!(
        (t.access.as_str(), t.refresh.as_deref(), t.expires_at),
        ("", Some("rt-3"), UNIX_EPOCH)
    );
    assert_eq!(load_tokens(&store, "none").unwrap(), None);
}

#[test]
fn client_ids_come_from_the_account_or_the_shipped_file() {
    // The shipped file holds placeholders only: users bring their own ids.
    assert_eq!(CloudKind::GoogleDrive.default_client(), None);
    assert_eq!(CloudKind::Dropbox.default_client(), None);
    let store = MemoryStore::default();
    let mut drive = account("drive", CloudKind::GoogleDrive);
    let err = resolve_client(&drive, &store).unwrap_err();
    assert!(format!("{err:#}").contains("client id"), "{err:#}");
    drive.client_id_override = Some("mine".into());
    store.set("drive/client_secret", "installed-app").unwrap();
    let client = resolve_client(&drive, &store).unwrap();
    assert_eq!(client.id, "mine");
    assert_eq!(client.secret.as_deref(), Some("installed-app"));
    assert!(!format!("{client:?}").contains("installed-app"));
}

#[test]
fn connect_reads_keys_from_the_store_without_network() {
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore::default());
    let events = crossbeam_channel::unbounded().0;
    let mut b2 = account("b2", CloudKind::S3);
    assert!(CloudProvider::connect(&b2, store.clone(), events.clone()).is_err());
    b2.s3 = Some(S3Config {
        endpoint: "https://s3.example.invalid".into(),
        region: "auto".into(),
        bucket: "b".into(),
    });
    let err = CloudProvider::connect(&b2, store.clone(), events.clone())
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("keychain"), "{err:#}");
    store.set("b2/access_key_id", "id").unwrap();
    store.set("b2/secret_access_key", "secret").unwrap();
    let p = CloudProvider::connect(&b2, store.clone(), events.clone()).unwrap();
    assert_eq!(p.remove_kind(), RemoveKind::Permanent);
    let mut drive = account("drive", CloudKind::GoogleDrive);
    drive.client_id_override = Some("mine".into());
    let p = CloudProvider::connect(&drive, store, events).unwrap();
    assert_eq!(p.remove_kind(), RemoveKind::Trash);
    assert!(!p.needs_reauth());
    // Ids are lowercase slugs: Windows Credential Manager would mix up `Work` and `work`.
    for bad in ["bad id", "Work", "", "a/b"] {
        assert!(
            CloudProvider::connect(
                &account(bad, CloudKind::S3),
                Arc::new(MemoryStore::default()),
                crossbeam_channel::unbounded().0
            )
            .is_err(),
            "{bad:?}"
        );
    }
    assert!(valid_id("work-2_b"));
}

#[test]
fn an_unsupported_cpu_is_an_error_not_a_panic() {
    assert_eq!(check_cpu(), Ok(()), "this machine runs the cloud tests");
    assert_eq!(cpu_error(|_| true), Ok(()));
    let missing = *CPU_FEATURES.last().unwrap();
    let err = cpu_error(|f| f != missing).unwrap_err();
    assert_eq!(err, CloudError::UnsupportedCpu(missing.to_uppercase()));
    let text = anyhow::Error::from(err.clone()).to_string();
    assert!(
        text.contains("need a CPU with") && text.contains(&missing.to_uppercase()),
        "{text}"
    );
    assert!(cpu_error(|_| false)
        .unwrap_err()
        .to_string()
        .contains(&CPU_FEATURES.join(", ").to_uppercase()));
}

#[test]
fn s3_keys_never_reach_debug_output() {
    let keys = S3Keys(reqsign_aws_v4::StaticCredentialProvider::new(
        "AKIDLEAKCHECK",
        "wJalr-leak-check-secret",
    ));
    let mut fake = account("leak", CloudKind::S3);
    fake.s3 = Some(S3Config {
        endpoint: "https://s3.example.invalid".into(),
        region: "auto".into(),
        bucket: "b".into(),
    });
    let op = s3_op(&fake, "AKIDLEAKCHECK", "wJalr-leak-check-secret").unwrap();
    for text in [format!("{keys:?}"), format!("{op:?}")] {
        assert!(
            !text.contains("leak-check") && !text.contains("AKIDLEAKCHECK"),
            "{text}"
        );
    }
    assert!(LOG_FILTER_HINT.contains("reqsign_core=warn"));
}

// --- Retries, tokens -----------------------------------------------------------------

#[test]
fn backoff_doubles_with_jitter_and_stops_after_five_tries() {
    let schedule: Vec<_> = (0..6).map(|a| backoff(a, 0.0)).collect();
    assert_eq!(
        schedule,
        [
            Some(Duration::from_millis(250)),
            Some(Duration::from_millis(500)),
            Some(Duration::from_millis(1000)),
            Some(Duration::from_millis(2000)),
            None,
            None
        ]
    );
    for attempt in 0..4 {
        let low = backoff(attempt, 0.0).unwrap();
        let high = backoff(attempt, 0.999).unwrap();
        assert!(high > low && high < low * 2, "{attempt}: {low:?}..{high:?}");
    }
    let unit = jitter();
    assert!((0.0..1.0).contains(&unit));
}

#[test]
fn http_errors_are_classified_and_sanitised() {
    let err = |kind, status: u16| {
        opendal::Error::new(kind, "server says: token ya29.secret is bad").with_context(
            "response",
            format!("Parts {{ status: {status}, version: HTTP/1.1, headers: {{}} }}"),
        )
    };
    let p = vp("cloud://x/a.txt");
    assert_eq!(http_status(&err(ErrorKind::Unexpected, 429)), Some(429));
    assert!(retryable(&err(ErrorKind::Unexpected, 429)));
    assert!(retryable(&err(ErrorKind::Unexpected, 503)));
    assert!(!retryable(&err(ErrorKind::Unexpected, 400)));
    assert!(!retryable(&err(ErrorKind::NotFound, 404)));
    let wired = wire(&err(ErrorKind::Unexpected, 401), &p);
    let text = format!("{wired:#}");
    assert!(
        text.contains("HTTP 401") && !text.contains("ya29"),
        "{text}"
    );
    assert_eq!(kind_of(&wired), Some(io::ErrorKind::PermissionDenied));
    assert!(is_not_found(&wire(&err(ErrorKind::NotFound, 404), &p)));
}

#[test]
fn drive_rate_limit_403s_are_retried_other_403s_are_not() {
    // As opendal's Drive service words them (it keeps only the message).
    let drive = |message: &str| {
        opendal::Error::new(
            ErrorKind::PermissionDenied,
            format!("GdriveError {{ error: GdriveInnerError {{ message: \"{message}\" }} }}"),
        )
        .with_context(
            "response",
            "Parts { status: 403, version: HTTP/1.1, headers: {} }",
        )
    };
    assert!(retryable(&drive("Rate Limit Exceeded")));
    assert!(retryable(&drive("User Rate Limit Exceeded")));
    assert!(retryable(&drive("reason: userRateLimitExceeded")));
    assert!(!retryable(&drive(
        "The user does not have sufficient permissions"
    )));
}

#[test]
fn backoff_waits_end_when_cancelled() {
    let cloud = memory_cloud("mem");
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let tries = AtomicUsize::new(0);
    let err = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(150));
            cancel.store(true, Ordering::SeqCst);
        });
        cloud
            .core
            .call_cancellable(&vp("cloud://mem/a"), &cancel, |_| {
                tries.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(http_error(503))
            })
            .unwrap_err()
    });
    // Uncancelled, the four waits take at least 3.75 s.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(kind_of(&err), Some(io::ErrorKind::Interrupted));
    assert!(format!("{err:#}").contains("cancelled"), "{err:#}");
    assert!(tries.load(Ordering::SeqCst) < MAX_TRIES as usize);
}

#[test]
fn rejected_or_expired_tokens_refresh_once_and_are_stored() {
    let ok = r#"{"access_token":"at-2","expires_in":3600}"#;
    let far = SystemTime::now() + Duration::from_secs(3600);
    let (url, requests) = token_server(200, ok);
    let (cloud, store, events) = oauth_cloud(url, far);
    let p = vp("cloud://dbx/a");
    let calls = std::sync::atomic::AtomicU32::new(0);
    cloud
        .core
        .call(&p, |_| match calls.fetch_add(1, Ordering::SeqCst) {
            0 => Err(http_error(401)),
            _ => Ok(()),
        })
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    let tokens = stored(&store, "dbx");
    assert_eq!(tokens.access, "at-2");
    assert_eq!(tokens.refresh.as_deref(), Some("rt"));
    assert!(matches!(
        events.try_recv(),
        Ok(RemoteEvent::Status { status: ConnStatus::Connected, ref host_id, .. }) if host_id == "cloud:dbx"
    ));
    // A token the service keeps rejecting: one refresh, then the error (no loop).
    calls.store(0, Ordering::SeqCst);
    let err = cloud
        .core
        .call(&p, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(http_error(401))
        })
        .unwrap_err();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(kind_of(&err), Some(io::ErrorKind::PermissionDenied));
    // Expired before the call: refreshed first, the operation runs once.
    let (url, _) = token_server(200, ok);
    let (cloud, store, _) = oauth_cloud(url, UNIX_EPOCH);
    calls.store(0, Ordering::SeqCst);
    cloud
        .core
        .call(&p, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(stored(&store, "dbx").access, "at-2");
}

#[test]
fn concurrent_401s_refresh_the_token_once() {
    let (url, requests) = token_server(200, r#"{"access_token":"at-2","expires_in":3600}"#);
    let far = SystemTime::now() + Duration::from_secs(3600);
    let (cloud, _, _) = oauth_cloud(url, far);
    let threads = 6;
    let barrier = std::sync::Barrier::new(threads);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let first = Cell::new(true);
                cloud
                    .core
                    .call(&vp("cloud://dbx/a"), |_| {
                        let generation = cloud.core.generation.load(Ordering::SeqCst);
                        // Every thread's first request fails with the old token.
                        if first.replace(false) {
                            barrier.wait();
                        }
                        match generation {
                            0 => Err(http_error(401)),
                            _ => Ok(()),
                        }
                    })
                    .unwrap();
            });
        }
    });
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[test]
fn a_revoked_grant_asks_to_sign_in_again_and_fails_fast() {
    let (url, requests) = token_server(400, r#"{"error":"invalid_grant"}"#);
    let (cloud, store, events) = oauth_cloud(url, UNIX_EPOCH);
    let err = cloud.list(&vp("cloud://dbx/")).unwrap_err();
    assert!(format!("{err:#}").contains("sign in again"), "{err:#}");
    assert!(matches!(
        events.try_recv(),
        Ok(RemoteEvent::Status {
            status: ConnStatus::Failed,
            ..
        })
    ));
    assert!(cloud.needs_reauth());
    assert!(load_tokens(&*store, "dbx")
        .unwrap()
        .unwrap()
        .access
        .is_empty());
    // From now on: no token request and no further status event, until a new sign-in.
    for _ in 0..3 {
        let err = cloud.stat(&vp("cloud://dbx/x")).unwrap_err();
        assert!(format!("{err:#}").contains("sign in again"), "{err:#}");
    }
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(events.try_recv().is_err());
    // Signed in again (new tokens stored): works without re-registering.
    store_tokens(
        &*store,
        "dbx",
        &OAuthTokens {
            access: "at-new".into(),
            refresh: Some("rt-new".into()),
            expires_at: SystemTime::now() + Duration::from_secs(3600),
        },
    )
    .unwrap();
    assert!(cloud.list(&vp("cloud://dbx/")).unwrap().is_empty());
    assert!(!cloud.needs_reauth());
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    // Signed out entirely (no refresh token): the same message, no request.
    let (url, requests) = token_server(400, r#"{"error":"invalid_grant"}"#);
    let (cloud, store, _) = oauth_cloud(url, UNIX_EPOCH);
    store.delete("dbx/refresh_token").unwrap();
    let err = cloud.stat(&vp("cloud://dbx/x")).unwrap_err();
    assert!(format!("{err:#}").contains("sign in again"), "{err:#}");
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    assert!(cloud.needs_reauth());
}

// --- Listing, reading, writing (memory service) ---------------------------------------

#[test]
fn listings_are_cached_until_a_write_or_the_ttl() {
    let cloud = memory_cloud("mem");
    put(&cloud, "cloud://mem/a.txt", b"a");
    cloud.mkdir(&vp("cloud://mem/docs")).unwrap();
    let root = vp("cloud://mem/");
    let first = cloud.list(&root).unwrap();
    assert_eq!(names(&first), ["a.txt", "docs"]);
    let a = first.iter().find(|e| e.name == "a.txt").unwrap();
    assert_eq!(
        (a.kind.clone(), a.size, a.ext.as_str()),
        (Kind::File, 1, "txt")
    );
    // A change made elsewhere is not seen while the listing is fresh...
    cloud
        .core
        .op
        .read()
        .write("elsewhere.txt", b"x".to_vec())
        .unwrap();
    assert_eq!(names(&cloud.list(&root).unwrap()), ["a.txt", "docs"]);
    assert!(is_not_found(
        &cloud.stat(&vp("cloud://mem/elsewhere.txt")).unwrap_err()
    ));
    // ...our own write invalidates the folder.
    put(&cloud, "cloud://mem/b.txt", b"bb");
    assert_eq!(
        names(&cloud.list(&root).unwrap()),
        ["a.txt", "b.txt", "docs", "elsewhere.txt"]
    );
    assert_eq!(cloud.stat(&vp("cloud://mem/b.txt")).unwrap().size, 2);
    assert_eq!(cloud.stat(&vp("cloud://mem/docs")).unwrap().kind, Kind::Dir);
    // Expiry.
    let mut short = memory_cloud("mem");
    short.tune(|core| core.ttl = Duration::from_millis(30));
    put(&short, "cloud://mem/a.txt", b"a");
    let root = vp("cloud://mem/");
    assert_eq!(names(&short.list(&root).unwrap()), ["a.txt"]);
    short.core.op.read().write("c.txt", b"c".to_vec()).unwrap();
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(names(&short.list(&root).unwrap()), ["a.txt", "c.txt"]);
}

#[test]
fn big_folders_are_cut_off_at_the_cap() {
    let mut cloud = memory_cloud("mem");
    cloud.tune(|core| core.list_cap = 3);
    for i in 0..5 {
        cloud
            .core
            .op
            .read()
            .write(&format!("f{i}"), b"x".to_vec())
            .unwrap();
    }
    assert_eq!(cloud.list(&vp("cloud://mem/")).unwrap().len(), 3);
    // For an index the cut-off listing is an error, never a partial answer.
    let err = cloud.list_complete(&vp("cloud://mem/")).unwrap_err();
    assert!(err.to_string().contains("more than 3 entries"), "{err:#}");
    cloud.tune(|core| core.list_cap = 10);
    // Fresh, not the cached 3.
    assert_eq!(cloud.list_complete(&vp("cloud://mem/")).unwrap().len(), 5);
}

#[test]
fn google_docs_and_shortcuts_are_not_listed_as_files() {
    let with_type = |t: &str| {
        let mut meta = opendal::MetadataBuilder::file(0);
        meta.content_type(t);
        meta.build()
    };
    assert!(google_native(&with_type(
        "application/vnd.google-apps.document"
    )));
    assert!(google_native(&with_type(
        "application/vnd.google-apps.shortcut"
    )));
    assert!(!google_native(&with_type(
        "application/vnd.google-apps.folder"
    )));
    assert!(!google_native(&with_type("application/pdf")));
    assert!(!google_native(&opendal::MetadataBuilder::file(1).build()));
}

#[test]
fn read_write_create_new_and_dropped_uploads() {
    let cloud = memory_cloud("mem");
    put(&cloud, "cloud://mem/a.txt", b"hello");
    assert_eq!(get(&cloud, "cloud://mem/a.txt"), b"hello");
    put(&cloud, "cloud://mem/a.txt", b"replaced");
    assert_eq!(get(&cloud, "cloud://mem/a.txt"), b"replaced");
    let err = cloud.create_new(&vp("cloud://mem/a.txt")).err().unwrap();
    assert_eq!(kind_of(&err), Some(io::ErrorKind::AlreadyExists));
    {
        let mut w = cloud.create_new(&vp("cloud://mem/never.txt")).unwrap();
        w.write_all(b"half").unwrap();
        // dropped without flush(): discarded
    }
    assert!(is_not_found(
        &cloud.stat(&vp("cloud://mem/never.txt")).unwrap_err()
    ));
    assert!(cloud.list(&vp("cloud://mem/missing")).unwrap().is_empty());
    assert!(cloud.read(&vp("cloud://mem/")).is_err());
    for bad in [
        "cloud://other/a.txt",
        "cloud://mem/../a",
        "cloud://mem/a//b",
        "sftp://mem/a",
    ] {
        assert!(cloud.stat(&vp(bad)).is_err(), "{bad}");
    }
    let copy = cloud.local_copy(&vp("cloud://mem/a.txt")).unwrap();
    assert_eq!(fs::read(&copy).unwrap(), b"replaced");
}

#[test]
fn ranged_reads_start_at_the_offset() {
    let cloud = memory_cloud("mem");
    put(&cloud, "cloud://mem/r.txt", b"hello world");
    let mut got = String::new();
    cloud
        .read_range(&vp("cloud://mem/r.txt"), 6, 3)
        .unwrap()
        .expect("cloud reads ranges")
        .read_to_string(&mut got)
        .unwrap();
    assert_eq!(got, "wor");
}

#[test]
fn local_copy_checks_the_service_not_the_listing_cache() {
    let cloud = memory_cloud("mem");
    put(&cloud, "cloud://mem/doc.txt", b"old");
    cloud.list(&vp("cloud://mem/")).unwrap();
    // Changed elsewhere while the listing (size 3) is cached.
    cloud
        .core
        .op
        .read()
        .write("doc.txt", b"changed elsewhere".to_vec())
        .unwrap();
    let copy = cloud.local_copy(&vp("cloud://mem/doc.txt")).unwrap();
    assert_eq!(fs::read(&copy).unwrap(), b"changed elsewhere");
}

#[test]
fn single_request_uploads_are_capped_with_a_clear_message() {
    assert_eq!(
        CloudKind::GoogleDrive.upload_limit(),
        Some(256 * 1024 * 1024)
    );
    assert_eq!(CloudKind::Dropbox.upload_limit(), Some(150 * 1024 * 1024));
    let mut drive = memory_cloud_of("drive", CloudKind::GoogleDrive);
    drive.tune(|core| core.upload_limit = Some(1 << 20));
    let mut w = drive.write(&vp("cloud://drive/big.bin")).unwrap();
    w.write_all(&vec![1; 1 << 20]).unwrap();
    let err = w.write_all(b"one more").unwrap_err();
    assert!(
        err.to_string()
            .contains("files over 1 MB cannot be uploaded to Google Drive"),
        "{err}"
    );
    assert!(w.flush().is_err());
    drop(w);
    assert!(drive.stat(&vp("cloud://drive/big.bin")).is_err());
}

#[test]
fn rename_and_remove_follow_each_kind() {
    assert_eq!(CloudKind::GoogleDrive.remove_kind(), RemoveKind::Trash);
    assert_eq!(
        CloudKind::Dropbox.remove_kind(),
        RemoveKind::RecoverableDelete
    );
    assert_eq!(CloudKind::S3.remove_kind(), RemoveKind::Permanent);
    let cloud = memory_cloud("mem");
    put(&cloud, "cloud://mem/a.txt", b"a");
    put(&cloud, "cloud://mem/b.txt", b"b");
    let (a, b, c) = (
        vp("cloud://mem/a.txt"),
        vp("cloud://mem/b.txt"),
        vp("cloud://mem/c.txt"),
    );
    let err = cloud.rename(&a, &b).unwrap_err();
    assert_eq!(kind_of(&err), Some(io::ErrorKind::AlreadyExists));
    cloud.rename_noreplace(&a, &c).unwrap();
    assert!(is_not_found(&cloud.stat(&a).unwrap_err()));
    cloud.rename_replace(&c, &b).unwrap();
    assert_eq!(get(&cloud, "cloud://mem/b.txt"), b"a");
    // Never onto itself (a replace would delete it) or into itself.
    let err = cloud.rename_replace(&b, &b).unwrap_err();
    assert!(format!("{err:#}").contains("into itself"), "{err:#}");
    assert_eq!(get(&cloud, "cloud://mem/b.txt"), b"a");
    // Folders move with their contents (file by file).
    cloud.mkdir(&vp("cloud://mem/d")).unwrap();
    cloud.mkdir(&vp("cloud://mem/d/sub")).unwrap();
    put(&cloud, "cloud://mem/d/sub/x.txt", b"x");
    put(&cloud, "cloud://mem/d/y.txt", b"y");
    assert!(cloud.mkdir(&vp("cloud://mem/d")).is_err());
    let err = cloud
        .rename(&vp("cloud://mem/d"), &vp("cloud://mem/d/sub/d"))
        .unwrap_err();
    assert!(format!("{err:#}").contains("into itself"), "{err:#}");
    assert_eq!(get(&cloud, "cloud://mem/d/sub/x.txt"), b"x");
    cloud
        .rename(&vp("cloud://mem/d"), &vp("cloud://mem/e"))
        .unwrap();
    assert_eq!(
        names(&cloud.list(&vp("cloud://mem/")).unwrap()),
        ["b.txt", "e"]
    );
    assert_eq!(get(&cloud, "cloud://mem/e/sub/x.txt"), b"x");
    assert!(cloud
        .rename_replace(&vp("cloud://mem/e"), &vp("cloud://mem/b.txt"))
        .is_err());
    // S3 semantics: a folder delete removes every key under it.
    assert!(cloud.remove_empty_dir(&vp("cloud://mem/e")).is_err());
    cloud.remove(&vp("cloud://mem/e")).unwrap();
    assert_eq!(names(&cloud.list(&vp("cloud://mem/")).unwrap()), ["b.txt"]);
    assert!(cloud
        .core
        .op
        .read()
        .list_options(
            "/",
            options::ListOptions {
                recursive: true,
                ..Default::default()
            }
        )
        .unwrap()
        .iter()
        .all(|e| !e.path().starts_with("e/")));
    assert!(cloud.remove(&vp("cloud://mem/")).is_err());
}

#[test]
fn transfer_between_local_and_cloud_with_conflicts_and_cancel() {
    let router = Router::new();
    router.register_cloud_provider("mem".into(), Arc::new(memory_cloud("mem")));
    let cloud = router.provider_for(&vp("cloud://mem/")).unwrap();
    let local = tempfile::tempdir().unwrap();
    fs::write(local.path().join("a.txt"), b"local a").unwrap();
    fs::create_dir(local.path().join("tree")).unwrap();
    fs::write(
        local.path().join("tree").join("n.bin"),
        vec![7u8; 3_000_000],
    )
    .unwrap();
    cloud.mkdir(&vp("cloud://mem/up")).unwrap();
    let up = vp("cloud://mem/up");
    let no = AtomicBool::new(false);
    let run = |src: &[VPath], dst: &VPath, conflict, cancel: &AtomicBool| {
        ops::transfer(src, dst, false, conflict, &|_| {}, cancel, &router)
    };
    let sources = [
        VPath::local(local.path().join("a.txt")),
        VPath::local(local.path().join("tree")),
    ];
    run(&sources, &up, Conflict::Skip, &no).unwrap();
    let read = |p: &str| {
        let mut v = Vec::new();
        cloud.read(&vp(p)).unwrap().read_to_end(&mut v).unwrap();
        v
    };
    assert_eq!(read("cloud://mem/up/a.txt"), b"local a");
    assert_eq!(read("cloud://mem/up/tree/n.bin").len(), 3_000_000);
    // Conflicts.
    fs::write(local.path().join("a.txt"), b"newer").unwrap();
    run(&sources[..1], &up, Conflict::Skip, &no).unwrap();
    assert_eq!(read("cloud://mem/up/a.txt"), b"local a");
    run(&sources[..1], &up, Conflict::RenameNew, &no).unwrap();
    assert_eq!(read("cloud://mem/up/a (2).txt"), b"newer");
    run(&sources[..1], &up, Conflict::Overwrite, &no).unwrap();
    assert_eq!(read("cloud://mem/up/a.txt"), b"newer");
    // Cloud -> local.
    let down = tempfile::tempdir().unwrap();
    let down_dir = VPath::local(down.path());
    run(&[vp("cloud://mem/up/tree")], &down_dir, Conflict::Skip, &no).unwrap();
    assert_eq!(
        fs::read(down.path().join("tree").join("n.bin"))
            .unwrap()
            .len(),
        3_000_000
    );
    run(
        &[vp("cloud://mem/up/a.txt")],
        &down_dir,
        Conflict::Skip,
        &no,
    )
    .unwrap();
    assert_eq!(fs::read(down.path().join("a.txt")).unwrap(), b"newer");
    // Cancel mid-file, both directions: nothing half-written is left behind.
    let cancel = AtomicBool::new(false);
    let stop = |_: Progress| cancel.store(true, Ordering::Relaxed);
    let big = [VPath::local(local.path().join("tree").join("n.bin"))];
    let other = vp("cloud://mem/other");
    cloud.mkdir(&other).unwrap();
    let err =
        ops::transfer(&big, &other, false, Conflict::Skip, &stop, &cancel, &router).unwrap_err();
    assert!(format!("{err:#}").contains("cancelled"), "{err:#}");
    assert!(cloud.list(&other).unwrap().is_empty());
    cancel.store(false, Ordering::Relaxed);
    let empty = tempfile::tempdir().unwrap();
    let err = ops::transfer(
        &[vp("cloud://mem/up/tree/n.bin")],
        &VPath::local(empty.path()),
        false,
        Conflict::Skip,
        &stop,
        &cancel,
        &router,
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("cancelled"), "{err:#}");
    assert_eq!(fs::read_dir(empty.path()).unwrap().count(), 0);
    // A move out of the cloud deletes the source only after the copy.
    ops::transfer(
        &[vp("cloud://mem/up/a (2).txt")],
        &down_dir,
        true,
        Conflict::Skip,
        &|_| {},
        &no,
        &router,
    )
    .unwrap();
    assert!(down.path().join("a (2).txt").exists());
    assert!(cloud.stat(&vp("cloud://mem/up/a (2).txt")).is_err());
    router.unregister_cloud("mem");
    assert!(router.provider_for(&vp("cloud://mem/")).is_none());
}

/// Drive / Dropbox take a file in one request on `flush()`: the job says "Uploading to …"
/// instead of standing at 100 % while that runs, and a cancelled job uploads nothing.
#[test]
fn single_request_uploads_say_so_and_stop_on_cancel() {
    let router = Router::new();
    let dbx = Arc::new(memory_cloud_of("dbx", CloudKind::Dropbox));
    router.register_cloud_provider("dbx".into(), dbx);
    let cloud = router.provider_for(&vp("cloud://dbx/")).unwrap();
    assert_eq!(cloud.uploads_on_flush(), Some("Dropbox"));
    let local = tempfile::tempdir().unwrap();
    fs::write(local.path().join("a.bin"), vec![1u8; 3_000_000]).unwrap();
    let src = [VPath::local(local.path().join("a.bin"))];
    let seen = Mutex::new(Vec::<Progress>::new());
    let report = |p: Progress| seen.lock().push(p);
    let no = AtomicBool::new(false);
    ops::transfer(
        &src,
        &vp("cloud://dbx/"),
        false,
        Conflict::Skip,
        &report,
        &no,
        &router,
    )
    .unwrap();
    let seen = seen.into_inner();
    let uploading: Vec<_> = seen
        .iter()
        .filter(|p| p.current.starts_with("Uploading to Dropbox…"))
        .collect();
    assert!(!uploading.is_empty());
    assert!(uploading.iter().all(|p| p.done_bytes == 0));
    assert_eq!(seen.last().unwrap().done_bytes, 3_000_000);
    // A commit after the job was cancelled sends nothing.
    let cancel = AtomicBool::new(false);
    let target = vp("cloud://dbx/b.txt");
    let mut w = cloud.create_new_cancellable(&target, &cancel).unwrap();
    w.write_all(b"data").unwrap();
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(w.flush().unwrap_err().kind(), io::ErrorKind::Interrupted);
    drop(w);
    assert!(cloud.stat(&target).is_err());
}

// --- Real opendal services against local fakes ----------------------------------------

/// opendal's real S3 service over the HTTP transport `init` installs.
#[test]
fn s3_over_http_lists_reads_and_writes() {
    let (base, fake) = s3_fake();
    let cloud = s3_cloud(&base);
    put(&cloud, "cloud://fake/a.txt", b"hello");
    cloud.mkdir(&vp("cloud://fake/docs")).unwrap();
    put(&cloud, "cloud://fake/docs/b.txt", b"bee");
    assert_eq!(fake.lock().objects["a.txt"], b"hello");
    assert_eq!(
        names(&cloud.list(&vp("cloud://fake/")).unwrap()),
        ["a.txt", "docs"]
    );
    assert_eq!(
        names(&cloud.list(&vp("cloud://fake/docs")).unwrap()),
        ["b.txt"]
    );
    assert_eq!(get(&cloud, "cloud://fake/docs/b.txt"), b"bee");
    let a = cloud.stat(&vp("cloud://fake/a.txt")).unwrap();
    assert_eq!((a.kind, a.size), (Kind::File, 5));
    assert!(a.modified.is_some());
    let err = cloud.create_new(&vp("cloud://fake/a.txt")).err().unwrap();
    assert_eq!(kind_of(&err), Some(io::ErrorKind::AlreadyExists));
    cloud
        .rename(&vp("cloud://fake/a.txt"), &vp("cloud://fake/c.txt"))
        .unwrap();
    assert_eq!(get(&cloud, "cloud://fake/c.txt"), b"hello");
    assert!(!fake.lock().objects.contains_key("a.txt"));
}

#[test]
fn s3_folder_moves_keep_whatever_was_not_moved() {
    let (base, fake) = s3_fake();
    let cloud = s3_cloud(&base);
    cloud.mkdir(&vp("cloud://fake/d")).unwrap();
    put(&cloud, "cloud://fake/d/a.txt", b"a");
    put(&cloud, "cloud://fake/d/b.txt", b"b");
    // Someone uploads into the folder while it is being moved.
    fake.lock().on_delete = Some(("d/a.txt".into(), "d/late.txt".into()));
    let err = cloud
        .rename(&vp("cloud://fake/d"), &vp("cloud://fake/e"))
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(
        text.contains("moved 2 of 3 items") && text.contains("still holds the rest: late.txt"),
        "{text}"
    );
    {
        let objects = &fake.lock().objects;
        assert_eq!(objects["d/late.txt"], b"late");
        assert_eq!(objects["e/a.txt"], b"a");
        assert_eq!(objects["e/b.txt"], b"b");
    }
    // A child that cannot be moved: the move stops there and the source keeps the rest.
    cloud.mkdir(&vp("cloud://fake/f")).unwrap();
    put(&cloud, "cloud://fake/f/a.txt", b"a");
    put(&cloud, "cloud://fake/f/b.txt", b"b");
    fake.lock().fail.push(("PUT", "g/b.txt".into(), 403));
    let err = cloud
        .rename(&vp("cloud://fake/f"), &vp("cloud://fake/g"))
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("moved 1 of 2 items"), "{text}");
    let objects = &fake.lock().objects;
    assert_eq!(objects["f/b.txt"], b"b");
    assert!(objects.contains_key("f/"));
}

#[test]
fn dropbox_uploads_go_in_one_request_straight_to_the_target() {
    let (base, fake) = dropbox_fake();
    let op = dropbox_op(&base, "at");
    let dbx = CloudProvider::with_operator(
        account("dbx", CloudKind::Dropbox),
        op,
        crossbeam_channel::unbounded().0,
    )
    .unwrap();
    // opendal's Dropbox writer takes one write only; uploads come in 1 MiB pieces.
    let mut w = dbx.write(&vp("cloud://dbx/direct.bin")).unwrap();
    for _ in 0..3 {
        w.write_all(&vec![5; 1 << 20]).unwrap();
    }
    w.flush().unwrap();
    {
        let fake = fake.lock();
        assert_eq!(fake.files["/direct.bin"].len(), 3 << 20);
        assert_eq!(fake.calls("upload"), ["/direct.bin"]);
        assert!(fake.calls("move_v2").is_empty(), "no staging name");
    }
    // Through a transfer (which stages under its own partial name).
    let router = Router::new();
    router.register_cloud_provider("dbx".into(), Arc::new(dbx));
    let local = tempfile::tempdir().unwrap();
    fs::write(local.path().join("n.bin"), vec![7u8; 3 << 20]).unwrap();
    fake.lock().folders.insert("/up".into());
    ops::transfer(
        &[VPath::local(local.path().join("n.bin"))],
        &vp("cloud://dbx/up"),
        false,
        Conflict::Skip,
        &|_| {},
        &AtomicBool::new(false),
        &router,
    )
    .unwrap();
    let fake = fake.lock();
    assert_eq!(fake.files["/up/n.bin"].len(), 3 << 20);
    assert_eq!(fake.calls("upload").len(), 2, "one request per file");
}

#[test]
fn dropbox_upload_requests_are_retried_with_the_data_still_in_memory() {
    let (base, fake) = dropbox_fake();
    let (url, requests) = token_server(200, r#"{"access_token":"at-2","expires_in":3600}"#);
    let far = SystemTime::now() + Duration::from_secs(3600);
    let fake_url = base.clone();
    let (dbx, store, _) = oauth_cloud_over(url, far, move |access| dropbox_op(&fake_url, access));
    {
        let mut fake = fake.lock();
        fake.fail
            .push(("upload", "/r.bin".into(), 503, "too_many_write_operations/"));
        fake.fail
            .push(("upload", "/r.bin".into(), 401, "expired_access_token/"));
    }
    put(&dbx, "cloud://dbx/r.bin", &[9; 3000]);
    assert_eq!(fake.lock().files["/r.bin"], [9; 3000]);
    assert_eq!(fake.lock().calls("upload").len(), 3);
    assert_eq!(requests.load(Ordering::SeqCst), 1, "401: one token refresh");
    assert_eq!(stored(&store, "dbx").access, "at-2");
}

#[test]
fn dropbox_replace_never_loses_the_old_or_the_new_file() {
    let (base, fake) = dropbox_fake();
    let dbx = CloudProvider::with_operator(
        account("dbx", CloudKind::Dropbox),
        dropbox_op(&base, "at"),
        crossbeam_channel::unbounded().0,
    )
    .unwrap();
    {
        let mut fake = fake.lock();
        fake.files.insert("/t.txt".into(), b"old".to_vec());
        fake.files.insert("/staged".into(), b"new".to_vec());
        fake.fail
            .push(("move_v2", "/staged".into(), 409, "to/no_write_permission/"));
    }
    let err = dbx
        .rename_replace(&vp("cloud://dbx/staged"), &vp("cloud://dbx/t.txt"))
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(
        text.contains("the old file is unchanged") && text.contains("still at cloud://dbx/staged"),
        "{text}"
    );
    {
        let fake = fake.lock();
        assert_eq!(fake.files["/t.txt"], b"old");
        assert_eq!(fake.files["/staged"], b"new");
        assert_eq!(fake.files.len(), 2, "{:?}", fake.files.keys());
        assert!(fake.calls("delete_v2").is_empty());
    }
    dbx.rename_replace(&vp("cloud://dbx/staged"), &vp("cloud://dbx/t.txt"))
        .unwrap();
    let fake = fake.lock();
    assert_eq!(fake.files["/t.txt"], b"new");
    assert_eq!(fake.files.len(), 1, "{:?}", fake.files.keys());
}

#[test]
fn dropbox_folder_moves_are_one_request() {
    let (base, fake) = dropbox_fake();
    let dbx = CloudProvider::with_operator(
        account("dbx", CloudKind::Dropbox),
        dropbox_op(&base, "at"),
        crossbeam_channel::unbounded().0,
    )
    .unwrap();
    {
        let mut fake = fake.lock();
        fake.folders.insert("/d".into());
        fake.files.insert("/d/a.txt".into(), b"a".to_vec());
        fake.files.insert("/d/b.txt".into(), b"b".to_vec());
    }
    dbx.rename(&vp("cloud://dbx/d"), &vp("cloud://dbx/e"))
        .unwrap();
    let fake = fake.lock();
    assert_eq!(fake.calls("move_v2"), ["/d"]);
    assert!(fake.calls("delete_v2").is_empty());
    assert_eq!(
        fake.files.keys().collect::<Vec<_>>(),
        ["/e/a.txt", "/e/b.txt"]
    );
}

/// `KEEL_CLOUD_TEST_S3=endpoint,region,bucket` plus `KEEL_CLOUD_TEST_S3_KEY_ID` and
/// `KEEL_CLOUD_TEST_S3_SECRET`: round trip against a real bucket under `keel-test/`.
#[test]
fn live_s3_round_trip() {
    let (Ok(spec), Ok(id), Ok(secret)) = (
        std::env::var("KEEL_CLOUD_TEST_S3"),
        std::env::var("KEEL_CLOUD_TEST_S3_KEY_ID"),
        std::env::var("KEEL_CLOUD_TEST_S3_SECRET"),
    ) else {
        println!(
            "skipped: set KEEL_CLOUD_TEST_S3=endpoint,region,bucket, \
             KEEL_CLOUD_TEST_S3_KEY_ID and KEEL_CLOUD_TEST_S3_SECRET to run"
        );
        return;
    };
    let parts: Vec<_> = spec.split(',').map(str::trim).collect();
    let [endpoint, region, bucket] = parts[..] else {
        panic!("KEEL_CLOUD_TEST_S3 must be endpoint,region,bucket");
    };
    let store = Arc::new(MemoryStore::default());
    store.set("live/access_key_id", &id).unwrap();
    store.set("live/secret_access_key", &secret).unwrap();
    let mut live = account("live", CloudKind::S3);
    live.root = Some("/keel-test".into());
    live.s3 = Some(S3Config {
        endpoint: endpoint.into(),
        region: region.into(),
        bucket: bucket.into(),
    });
    let cloud = CloudProvider::connect(&live, store, crossbeam_channel::unbounded().0).unwrap();
    let name = format!("cloud://live/t{}.txt", std::process::id());
    put(&cloud, &name, b"live");
    assert_eq!(get(&cloud, &name), b"live");
    assert!(
        names(&cloud.list(&vp("cloud://live/")).unwrap()).contains(&vp(&name).name().to_owned())
    );
    let moved = format!("{name}.moved");
    cloud.rename(&vp(&name), &vp(&moved)).unwrap();
    cloud.remove(&vp(&moved)).unwrap();
    assert!(cloud.stat(&vp(&moved)).is_err());
}

/// Writes, reads and deletes a throwaway entry in the real OS keychain.
#[test]
#[ignore = "touches the OS keychain"]
fn keyring_store_round_trip() {
    let store = KeyringStore;
    let key = format!("keel-test-{}/access_token", std::process::id());
    store.set(&key, "value").unwrap();
    assert_eq!(store.get(&key).unwrap().as_deref(), Some("value"));
    store.delete(&key).unwrap();
    assert_eq!(store.get(&key).unwrap(), None);
    store.delete(&key).unwrap();
}

/// A revoke endpoint logging the token of each call (Drive: the form field; Dropbox: the
/// bearer).
fn revoke_server() -> (String, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let base = serve(move |req| {
        let token = match req.headers.get("authorization") {
            Some(bearer) => bearer.trim_start_matches("Bearer ").to_owned(),
            None => String::from_utf8_lossy(&req.body).replace("token=", ""),
        };
        log.lock().push(token);
        (200, vec![], vec![])
    });
    (format!("{base}/revoke"), seen)
}

#[test]
fn sign_out_forgets_the_keys_before_revoking_with_the_right_token() {
    let store = MemoryStore::default();
    let tokens = OAuthTokens {
        access: "at".into(),
        refresh: Some("rt".into()),
        expires_at: SystemTime::now() + Duration::from_secs(3600),
    };
    store_tokens(&store, "drive", &tokens).unwrap();
    store.set("drive/client_secret", "cs").unwrap();
    let mut drive = account("drive", CloudKind::GoogleDrive);
    drive.client_id_override = Some("app".into());
    // The entries are gone before the (maybe slow, offline) revoke starts: a new account
    // reusing the id meanwhile keeps its secrets. The revoke gets what was read before.
    let err = sign_out_with(&drive, &store, |t, c| {
        assert!(store.keys().is_empty(), "{:?}", store.keys());
        let got = (t.access.as_str(), t.refresh.as_deref(), c.secret.as_deref());
        assert_eq!(got, ("at", Some("rt"), Some("cs")));
        anyhow::bail!("offline")
    })
    .unwrap_err();
    assert_eq!(err.to_string(), "offline");
    assert!(store.keys().is_empty());
    // Drive revokes the refresh token, Dropbox a live access token.
    let (url, seen) = revoke_server();
    let client = OAuthClient {
        id: "app".into(),
        secret: None,
    };
    let endpoints = |kind| oauth::Endpoints::for_kind(kind).unwrap();
    let drive_kind = CloudKind::GoogleDrive;
    revoke_at(drive_kind, &tokens, &client, &endpoints(drive_kind), &url).unwrap();
    let dbx = CloudKind::Dropbox;
    revoke_at(dbx, &tokens, &client, &endpoints(dbx), &url).unwrap();
    // An expired Dropbox access token (4 h) would get a 401: it is refreshed first.
    let (token_url, refreshes) =
        token_server(200, r#"{"access_token":"at-new","expires_in":14400}"#);
    let mut dbx_endpoints = endpoints(dbx);
    dbx_endpoints.token = token_url;
    let expired = OAuthTokens {
        expires_at: UNIX_EPOCH,
        ..tokens.clone()
    };
    revoke_at(dbx, &expired, &client, &dbx_endpoints, &url).unwrap();
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(*seen.lock(), ["rt", "at", "at-new"]);
    // A grant already revoked has nothing left to revoke.
    let (token_url, _) = token_server(400, r#"{"error":"invalid_grant"}"#);
    dbx_endpoints.token = token_url;
    revoke_at(dbx, &expired, &client, &dbx_endpoints, &url).unwrap();
    assert_eq!(seen.lock().len(), 3);
    // S3 has nothing to revoke (no request): its keys just go. Nothing stored: no request.
    store.set("b2/access_key_id", "AKID").unwrap();
    store.set("b2/secret_access_key", "SECRET").unwrap();
    sign_out(&account("b2", CloudKind::S3), &store).unwrap();
    assert!(store.keys().is_empty());
    sign_out(&drive, &store).unwrap();
}

// --- WebDAV ---------------------------------------------------------------------------

#[test]
fn webdav_url_is_normalised_and_checked() {
    let n = |u: &str, insecure| WebDavConfig::normalize_url(u, insecure);
    assert_eq!(
        n(" https://dav.test/remote.php/dav/files/u ", false).unwrap(),
        "https://dav.test/remote.php/dav/files/u/"
    );
    assert_eq!(n("https://dav.test", false).unwrap(), "https://dav.test/");
    assert_eq!(
        n("https://dav.test/a/", false).unwrap(),
        "https://dav.test/a/"
    );
    let e = n("webdav://dav.test/a/", false).unwrap_err();
    assert!(format!("{e}").contains("webdav://"), "{e}");
    assert!(n("webdavs://dav.test/a/", true).is_err());
    assert!(n("http://dav.test/a/", false).is_err());
    assert_eq!(n("http://dav.test/a", true).unwrap(), "http://dav.test/a/");
    assert!(n("ftp://dav.test/", true).is_err());
    assert!(n("https://u:p@dav.test/", false).is_err());
    assert!(n("https://dav.test/a?x=1", false).is_err());
    assert!(n("not a url", false).is_err());
}

#[test]
fn webdav_config_needs_a_user_and_roundtrips_without_secrets() {
    let cfg = WebDavConfig {
        url: "https://dav.test/dav/".into(),
        username: "u".into(),
        insecure: false,
    };
    cfg.validate().unwrap();
    assert!(WebDavConfig {
        username: " ".into(),
        ..cfg.clone()
    }
    .validate()
    .is_err());
    let mut a = account("dav", CloudKind::WebDav);
    a.webdav = Some(cfg);
    let text = toml::to_string(&a).unwrap();
    assert!(
        !text.contains("insecure") && !text.contains("password"),
        "{text}"
    );
    assert_eq!(toml::from_str::<CloudAccount>(&text).unwrap(), a);
    // forget_account removes the password entry.
    let store = MemoryStore::default();
    store.set("dav/password", "pw").unwrap();
    forget_account(&store, "dav");
    assert!(store.keys().is_empty());
    // A missing password fails at connect, without a network request.
    let err = CloudProvider::connect(
        &a,
        Arc::new(MemoryStore::default()),
        crossbeam_channel::unbounded().0,
    )
    .err()
    .expect("no password");
    assert!(format!("{err:#}").contains("password"), "{err:#}");
}

#[test]
fn webdav_account_lists_uploads_moves_and_deletes_permanently() {
    assert_eq!(CloudKind::WebDav.remove_kind(), RemoveKind::Permanent);
    let dav = memory_cloud_of("dav", CloudKind::WebDav);
    assert_eq!(dav.remove_kind(), RemoveKind::Permanent);
    dav.mkdir(&vp("cloud://dav/docs")).unwrap();
    put(&dav, "cloud://dav/docs/a.txt", b"a");
    put(&dav, "cloud://dav/top.txt", b"t");
    let complete = dav.list_complete(&vp("cloud://dav/")).unwrap();
    assert_eq!(names(&complete), ["docs", "top.txt"]);
    dav.rename(&vp("cloud://dav/docs"), &vp("cloud://dav/papers"))
        .unwrap();
    assert_eq!(get(&dav, "cloud://dav/papers/a.txt"), b"a");
    dav.remove(&vp("cloud://dav/top.txt")).unwrap();
    assert!(is_not_found(
        &dav.stat(&vp("cloud://dav/top.txt")).unwrap_err()
    ));
}

/// Against a real server: set `KEEL_WEBDAV_TEST_URL`, `KEEL_WEBDAV_TEST_USER` and
/// `KEEL_WEBDAV_TEST_PASS`, then `cargo test -p keel-vfs -- --ignored webdav_live`.
/// Works in a scratch folder it creates and removes.
#[test]
#[ignore = "needs KEEL_WEBDAV_TEST_URL / _USER / _PASS"]
fn webdav_live_roundtrip() {
    let var = |n: &str| std::env::var(n).unwrap_or_else(|_| panic!("{n} not set"));
    let url = var("KEEL_WEBDAV_TEST_URL");
    let mut a = account("live", CloudKind::WebDav);
    a.webdav = Some(WebDavConfig {
        insecure: url.starts_with("http://"),
        url,
        username: var("KEEL_WEBDAV_TEST_USER"),
    });
    let store = Arc::new(MemoryStore::default());
    store
        .set("live/password", &var("KEEL_WEBDAV_TEST_PASS"))
        .unwrap();
    let dav = CloudProvider::connect(&a, store, crossbeam_channel::unbounded().0).unwrap();
    dav.list_complete(&vp("cloud://live/")).unwrap();
    let dir = format!("cloud://live/keel-test-{}", std::process::id());
    dav.mkdir(&vp(&dir)).unwrap();
    put(&dav, &format!("{dir}/a.txt"), b"hello");
    dav.rename(&vp(&format!("{dir}/a.txt")), &vp(&format!("{dir}/b.txt")))
        .unwrap();
    assert_eq!(get(&dav, &format!("{dir}/b.txt")), b"hello");
    dav.remove(&vp(&dir)).unwrap();
    assert!(dav.stat(&vp(&dir)).is_err());
}

// --- Quota and share links (the services' own APIs) -----------------------------------

/// A signed-in Drive or Dropbox account (fresh token "at-1") whose API calls go to `api`.
fn api_cloud(a: CloudAccount, api: &str) -> CloudProvider {
    init().unwrap();
    let op = opendal::Operator::new(opendal::services::Memory::default()).unwrap();
    let mut endpoints = oauth::Endpoints::for_kind(a.kind).unwrap();
    // Never asked: the token is fresh.
    endpoints.token = "http://127.0.0.1:9/token".into();
    let client = OAuthClient {
        id: "app".into(),
        secret: None,
    };
    let far = SystemTime::now() + Duration::from_secs(3600);
    let make = op.clone();
    let oauth = OAuth::new(
        client,
        endpoints,
        "at-1",
        far,
        Box::new(move |_| Ok(make.clone())),
    );
    let store = Arc::new(MemoryStore::default());
    let mut cloud =
        CloudProvider::build(a, op, Some(oauth), store, crossbeam_channel::unbounded().0).unwrap();
    let api = api.to_owned();
    cloud.tune(|c| c.api = api);
    cloud
}
/// (method, path, query `q` or `fields`, JSON body) of each request to a fake API.
type ApiLog = Arc<Mutex<Vec<(String, String, String, Value)>>>;
/// A fake API answering `reply(path, query, body)`; every request must carry "Bearer at-1".
fn api_fake(
    reply: impl Fn(&str, &HashMap<String, String>, &Value) -> Reply + Send + 'static,
) -> (String, ApiLog) {
    let log = ApiLog::default();
    let seen = log.clone();
    let base = serve(move |req| {
        assert_eq!(
            req.headers.get("authorization").map(String::as_str),
            Some("Bearer at-1")
        );
        let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
        let q = req.query.get("q").or(req.query.get("fields"));
        seen.lock().push((
            req.method.clone(),
            req.path.clone(),
            q.cloned().unwrap_or_default(),
            body.clone(),
        ));
        reply(&req.path, &req.query, &body)
    });
    (base, log)
}

#[test]
fn drive_quota_with_and_without_a_limit() {
    for (quota, total) in [
        (
            json!({"usage": "1230000000", "limit": "15000000000"}),
            Some(15_000_000_000),
        ),
        (json!({"usage": "1230000000"}), None),
    ] {
        let (api, log) = api_fake(move |path, _, _| {
            assert_eq!(path, "/drive/v3/about");
            json_reply(200, json!({ "storageQuota": quota }))
        });
        let drive = api_cloud(account("drive", CloudKind::GoogleDrive), &api);
        assert_eq!(
            drive.quota(),
            Some(crate::Quota {
                used: 1_230_000_000,
                total
            })
        );
        assert_eq!(log.lock()[0].2, "storageQuota");
    }
}

#[test]
fn dropbox_quota_reads_individual_and_team_allocations() {
    for (allocation, total) in [
        (json!({".tag": "individual", "allocated": 2000}), Some(2000)),
        (
            json!({".tag": "team", "used": 900, "allocated": 5000,
                   "user_within_team_space_allocated": 0}),
            Some(5000),
        ),
        (
            json!({".tag": "team", "used": 900, "allocated": 5000,
                   "user_within_team_space_allocated": 1500}),
            Some(1500),
        ),
    ] {
        let (api, log) = api_fake(move |path, _, _| {
            assert_eq!(path, "/2/users/get_space_usage");
            json_reply(200, json!({ "used": 120, "allocation": allocation }))
        });
        let dbx = api_cloud(account("dbx", CloudKind::Dropbox), &api);
        assert_eq!(dbx.quota(), Some(crate::Quota { used: 120, total }));
        assert_eq!(log.lock()[0].0, "POST");
    }
}

#[test]
fn a_failed_quota_request_leaves_the_quota_unknown() {
    let (api, log) = api_fake(|_, _, _| json_reply(403, json!({"error": "no"})));
    let drive = api_cloud(account("drive", CloudKind::GoogleDrive), &api);
    assert_eq!(drive.quota(), None);
    assert_eq!(log.lock().len(), 1, "a refusal is not retried");
    let (api, _) = api_fake(|_, _, _| json_reply(200, json!({"unexpected": true})));
    assert_eq!(
        api_cloud(account("dbx", CloudKind::Dropbox), &api).quota(),
        None
    );
    // S3 and WebDAV report none (and ask nothing).
    assert_eq!(memory_cloud("bucket").quota(), None);
}

#[test]
fn drive_links_are_the_files_own_and_change_no_sharing() {
    let (api, log) = api_fake(|path, query, _| match path {
        "/drive/v3/files" => {
            let id = match query["q"].as_str() {
                q if q.starts_with("'root' in parents and name = 'Work'") => "f1",
                q if q.starts_with("'f1' in parents and name = 'it\\'s.txt'") => "f2",
                _ => return json_reply(200, json!({ "files": [] })),
            };
            json_reply(200, json!({ "files": [{ "id": id }] }))
        }
        "/drive/v3/files/f2" => {
            assert_eq!(query["fields"], "webViewLink");
            json_reply(
                200,
                json!({"webViewLink": "https://drive.example/file/d/f2/view"}),
            )
        }
        _ => (404, vec![], vec![]),
    });
    let mut a = account("drive", CloudKind::GoogleDrive);
    a.root = Some("/Work".into());
    let drive = api_cloud(a, &api);
    let link = drive
        .share_link(&vp("cloud://drive/it's.txt"), false)
        .unwrap();
    assert_eq!(
        link,
        crate::ShareLink::Ready {
            url: "https://drive.example/file/d/f2/view".into(),
            note: "Link copied. It opens only for people who already have access.".into(),
        }
    );
    assert!(log.lock().iter().all(|(m, ..)| m == "GET"), "read-only");
    let err = drive
        .share_link(&vp("cloud://drive/missing.txt"), false)
        .unwrap_err();
    assert_eq!(kind_of(&err), Some(io::ErrorKind::NotFound));
}

/// A Dropbox sharing API holding `links` (path -> url); a create adds one.
fn dropbox_links(links: &[(&str, &str)]) -> (String, ApiLog) {
    let links: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(
        (links.iter())
            .map(|(p, u)| (p.to_string(), u.to_string()))
            .collect(),
    ));
    api_fake(move |path, _, body| {
        let at = body["path"].as_str().unwrap().to_owned();
        match path {
            "/2/sharing/list_shared_links" => {
                assert_eq!(body["direct_only"], true);
                let found: Vec<Value> = (links.lock().get(&at).into_iter())
                    .map(|u| json!({".tag": "file", "url": u, "path_lower": at}))
                    .collect();
                json_reply(200, json!({ "links": found, "has_more": false }))
            }
            "/2/sharing/create_shared_link_with_settings" => {
                let url = format!("https://dropbox.example/s/new{at}");
                links.lock().insert(at, url.clone());
                json_reply(200, json!({ "url": url }))
            }
            _ => (404, vec![], vec![]),
        }
    })
}
fn creates(log: &ApiLog) -> usize {
    (log.lock().iter())
        .filter(|(_, p, ..)| p.ends_with("create_shared_link_with_settings"))
        .count()
}

#[test]
fn dropbox_reuses_an_existing_link_without_creating_one() {
    let (api, log) = dropbox_links(&[("/a.txt", "https://dropbox.example/s/old")]);
    let dbx = api_cloud(account("dbx", CloudKind::Dropbox), &api);
    for create in [false, true] {
        assert_eq!(
            dbx.share_link(&vp("cloud://dbx/a.txt"), create).unwrap(),
            crate::ShareLink::Ready {
                url: "https://dropbox.example/s/old".into(),
                note: "Link copied.".into()
            }
        );
    }
    assert_eq!(creates(&log), 0);
}

#[test]
fn dropbox_creates_a_link_only_after_a_yes() {
    let (api, log) = dropbox_links(&[]);
    let mut a = account("dbx", CloudKind::Dropbox);
    a.root = Some("/Team".into());
    let dbx = api_cloud(a, &api);
    let p = vp("cloud://dbx/docs/b.pdf");
    assert_eq!(
        dbx.share_link(&p, false).unwrap(),
        crate::ShareLink::Confirm {
            question: "Create a link anyone can open?".into()
        }
    );
    assert_eq!(creates(&log), 0);
    let crate::ShareLink::Ready { url, note } = dbx.share_link(&p, true).unwrap() else {
        panic!("no link");
    };
    assert_eq!(url, "https://dropbox.example/s/new/Team/docs/b.pdf");
    assert!(note.contains("Anyone with it"), "{note}");
    assert_eq!(creates(&log), 1);
    // The next time it is the existing one.
    assert!(matches!(
        dbx.share_link(&p, false).unwrap(),
        crate::ShareLink::Ready { .. }
    ));
    assert_eq!(creates(&log), 1);
}

#[test]
fn s3_links_are_presigned_for_an_hour_after_a_yes() {
    let (base, fake) = s3_fake();
    let cloud = s3_cloud(&base);
    let p = vp("cloud://fake/docs/a b.txt");
    let crate::ShareLink::Confirm { question } = cloud.share_link(&p, false).unwrap() else {
        panic!("S3 asks first");
    };
    assert!(question.contains("next hour"), "{question}");
    let crate::ShareLink::Ready { url, note } = cloud.share_link(&p, true).unwrap() else {
        panic!("no link");
    };
    assert!(
        url.starts_with(&format!("{base}/b/docs/a%20b.txt?")),
        "{url}"
    );
    assert!(url.contains("X-Amz-Expires=3600"), "{url}");
    assert!(url.contains("X-Amz-Signature="), "{url}");
    assert!(url.contains("AKIDFAKE"), "{url}");
    assert!(!url.contains("fake-secret"), "the secret key never leaves");
    assert!(note.contains("1 hour"), "{note}");
    assert!(fake.lock().objects.is_empty(), "signing asks nothing");
    let err = memory_cloud_of("dav", CloudKind::WebDav)
        .share_link(&vp("cloud://dav/a"), true)
        .unwrap_err();
    assert!(format!("{err:#}").contains("no share links"), "{err:#}");
}

/// Review 45 minor 9: a link request retrying a failing service (here in the middle of a
/// Drive path walk) stops once the provider is replaced.
#[test]
fn cancel_requests_ends_a_retrying_link_request() {
    let (api, log) = api_fake(|_, _, _| json_reply(503, json!({})));
    let drive = Arc::new(api_cloud(account("drive", CloudKind::GoogleDrive), &api));
    let asking = drive.clone();
    let started = Instant::now();
    let worker = std::thread::spawn(move || asking.share_link(&vp("cloud://drive/a/b.txt"), false));
    while log.lock().is_empty() {
        std::thread::sleep(Duration::from_millis(10));
    }
    drive.cancel_requests();
    assert!(worker.join().unwrap().is_err());
    if std::env::var_os("CI").is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }
    let asked = log.lock().len();
    assert!(asked <= 2, "{asked} requests");
}

// --- Change feeds -----------------------------------------------------------------------

fn feed(changes: &[crate::ChangedPath]) -> Vec<(String, crate::ChangeKind)> {
    changes
        .iter()
        .map(|c| (c.path.path.clone(), c.kind))
        .collect()
}
fn cursor(s: &str) -> Option<crate::ChangeCursor> {
    Some(crate::ChangeCursor(s.into()))
}
fn feed_error(e: &anyhow::Error) -> Option<crate::FeedError> {
    e.downcast_ref::<crate::FeedError>().copied()
}

/// Drive: the start token, every item remembered (two pages), then `changes.list` pages:
/// a trashed file, a file moved and renamed, a new file in a folder not seen yet (asked
/// for by id), a file deleted for good, a Google Doc and an item outside the drive's tree.
#[test]
fn drive_changes_follow_the_page_tokens_through_removes_and_moves() {
    use crate::ChangeKind::*;
    let item = |id: &str, name: &str, parent: &str| json!({"id": id, "name": name, "parents": [parent], "mimeType": "text/plain"});
    let (api, log) = api_fake(move |path, query, _| match path {
        "/drive/v3/files/root" => json_reply(200, json!({"id": "R"})),
        "/drive/v3/changes/startPageToken" => json_reply(200, json!({"startPageToken": "10"})),
        "/drive/v3/files" => {
            assert_eq!(query["q"], "trashed = false");
            match query.get("pageToken").map(String::as_str) {
                None => json_reply(
                    200,
                    json!({"nextPageToken": "p2", "files": [
                        {"id": "D1", "name": "docs", "parents": ["R"],
                         "mimeType": "application/vnd.google-apps.folder"},
                        item("F1", "a.txt", "D1"),
                    ]}),
                ),
                Some("p2") => json_reply(
                    200,
                    json!({"files": [
                        item("F2", "b.txt", "R"),
                        item("F3", "old.txt", "D1"),
                        item("X", "theirs.txt", "S"),
                    ]}),
                ),
                other => panic!("page {other:?}"),
            }
        }
        "/drive/v3/files/D2" => json_reply(200, json!({"name": "sub", "parents": ["D1"]})),
        "/drive/v3/files/S" => json_reply(404, json!({})),
        "/drive/v3/changes" => match query["pageToken"].as_str() {
            "10" => {
                assert_eq!(query["includeRemoved"], "true");
                json_reply(
                    200,
                    json!({"nextPageToken": "11", "changes": [
                        {"fileId": "F1", "file": {"name": "a.txt", "parents": ["D1"], "trashed": true}},
                        {"fileId": "F3", "file": {"name": "new.txt", "parents": ["R"]}},
                        {"fileId": "F4", "file": {"name": "c.txt", "parents": ["D2"]}},
                        {"fileId": "F2", "removed": true},
                        {"fileId": "G1", "file": {"name": "Notes", "parents": ["R"],
                            "mimeType": "application/vnd.google-apps.document"}},
                        {"fileId": "X", "file": {"name": "theirs.txt", "parents": ["S"]}},
                    ]}),
                )
            }
            "11" => json_reply(
                200,
                json!({"newStartPageToken": "12", "changes": [
                    {"fileId": "F5", "file": {"name": "d.txt", "parents": ["R"]}},
                    {"fileId": "F4", "file": {"name": "c.txt", "parents": ["D2"]}},
                ]}),
            ),
            _ => json_reply(404, json!({"error": "notFound"})),
        },
        other => panic!("unexpected request {other}"),
    });
    let drive = api_cloud(account("drive", CloudKind::GoogleDrive), &api);
    // A cursor from before this provider existed: where items were is not known.
    let err = drive.changes(cursor("9")).unwrap_err();
    assert_eq!(feed_error(&err), Some(crate::FeedError::CursorRejected));
    let start = drive.changes(None).unwrap();
    assert_eq!((start.cursor.0.as_str(), start.changes.len()), ("10", 0));
    let page = drive.changes(Some(start.cursor)).unwrap();
    assert_eq!(
        feed(&page.changes),
        [
            ("/docs/a.txt".into(), Removed),
            ("/docs/old.txt".into(), Removed),
            ("/new.txt".into(), Created),
            ("/docs/sub/c.txt".into(), Created),
            ("/b.txt".into(), Removed),
        ]
    );
    assert_eq!((page.cursor.0.as_str(), page.more), ("11", true));
    assert!(page.changes.iter().all(|c| c.path.authority == "drive"));
    let page = drive.changes(Some(page.cursor)).unwrap();
    assert_eq!(
        feed(&page.changes),
        [
            ("/d.txt".into(), Created),
            ("/docs/sub/c.txt".into(), Modified)
        ]
    );
    assert_eq!((page.cursor.0.as_str(), page.more), ("12", false));
    let asked = |p: &str| log.lock().iter().filter(|(_, path, ..)| path == p).count();
    assert_eq!(asked("/drive/v3/files/D2"), 1, "a folder is asked for once");
    // An old token is refused: the caller walks and starts again.
    let err = drive.changes(cursor("3")).unwrap_err();
    assert_eq!(feed_error(&err), Some(crate::FeedError::CursorRejected));
    assert!(log.lock().iter().all(|(m, ..)| m == "GET"), "read-only");
}

/// Dropbox: `get_latest_cursor` on the account's folder, `continue` pages (paths below
/// it, the folder itself left out), and `reset` refuses the cursor.
#[test]
fn dropbox_changes_continue_from_the_cursor_until_a_reset() {
    use crate::ChangeKind::*;
    let (api, log) = api_fake(|path, _, body| match path {
        "/2/files/list_folder/get_latest_cursor" => {
            assert_eq!(body["path"], "/Work");
            assert_eq!(body["recursive"], true);
            assert_eq!(body["include_deleted"], true);
            json_reply(200, json!({"cursor": "c1"}))
        }
        "/2/files/list_folder/continue" => match body["cursor"].as_str().unwrap() {
            "c1" => json_reply(
                200,
                json!({"cursor": "c2", "has_more": true, "entries": [
                    {".tag": "folder", "path_display": "/Work"},
                    {".tag": "file", "path_display": "/Work/a.txt"},
                    {".tag": "deleted", "path_display": "/Work/Old"},
                    {".tag": "folder", "path_display": "/Work/New"},
                ]}),
            ),
            "c2" => json_reply(
                200,
                json!({"cursor": "c3", "has_more": false, "entries": [
                    {".tag": "file", "path_display": "/Work/New/x.txt"},
                ]}),
            ),
            _ => json_reply(409, json!({"error": {".tag": "reset"}})),
        },
        other => panic!("unexpected request {other}"),
    });
    let mut a = account("dbx", CloudKind::Dropbox);
    a.root = Some("/Work".into());
    let dbx = api_cloud(a, &api);
    let start = dbx.changes(None).unwrap();
    assert_eq!((start.cursor.0.as_str(), start.changes.len()), ("c1", 0));
    let page = dbx.changes(Some(start.cursor)).unwrap();
    assert_eq!(
        feed(&page.changes),
        [
            ("/a.txt".into(), Modified),
            ("/Old".into(), Removed),
            ("/New".into(), Modified),
        ]
    );
    assert_eq!((page.cursor.0.as_str(), page.more), ("c2", true));
    let page = dbx.changes(Some(page.cursor)).unwrap();
    assert_eq!(feed(&page.changes), [("/New/x.txt".into(), Modified)]);
    assert_eq!((page.cursor.0.as_str(), page.more), ("c3", false));
    let err = dbx.changes(cursor("c0")).unwrap_err();
    assert_eq!(feed_error(&err), Some(crate::FeedError::CursorRejected));
    assert!(log.lock().iter().all(|(m, ..)| m == "POST"));
}

/// S3 has no feed: a bucket of one list page is fingerprinted (unchanged: nothing to
/// walk; changed: its root is `Unknown`), a bigger one is `Unsupported`, as is WebDAV.
#[test]
fn s3_fingerprints_a_small_bucket_and_gives_up_on_a_big_one() {
    let (base, fake) = s3_fake();
    let cloud = s3_cloud(&base);
    put(&cloud, "cloud://fake/a.txt", b"hello");
    let start = cloud.changes(None).unwrap();
    assert!(start.changes.is_empty());
    let same = cloud.changes(Some(start.cursor.clone())).unwrap();
    assert!(same.changes.is_empty());
    assert_eq!(same.cursor, start.cursor);
    put(&cloud, "cloud://fake/docs/b.txt", b"bee");
    let changed = cloud.changes(Some(same.cursor)).unwrap();
    assert_eq!(
        feed(&changed.changes),
        [("/".into(), crate::ChangeKind::Unknown)]
    );
    for i in 0..1000 {
        fake.lock().objects.insert(format!("many/{i}"), vec![1]);
    }
    let err = cloud.changes(Some(changed.cursor)).unwrap_err();
    assert_eq!(feed_error(&err), Some(crate::FeedError::Unsupported));
    let err = memory_cloud_of("dav", CloudKind::WebDav)
        .changes(None)
        .unwrap_err();
    assert_eq!(feed_error(&err), Some(crate::FeedError::Unsupported));
}
