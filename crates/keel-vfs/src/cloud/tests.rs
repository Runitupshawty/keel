use super::*;
use crate::{ops, Conflict, Router};
use std::{fs, sync::atomic::Ordering};

fn account(id: &str, kind: CloudKind) -> CloudAccount {
    CloudAccount {
        id: id.into(),
        label: format!("{id} label"),
        kind,
        root: None,
        client_id_override: None,
        s3: None,
    }
}
/// A cloud account backed by opendal's in-memory service (S3 semantics: no native
/// rename, permanent delete).
fn memory_cloud(id: &str) -> CloudProvider {
    let op = opendal::Operator::new(opendal::services::Memory::default()).unwrap();
    CloudProvider::with_operator(
        account(id, CloudKind::S3),
        op,
        crossbeam_channel::unbounded().0,
    )
    .unwrap()
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
    assert_eq!(
        store.get("drive/expires_at").unwrap().as_deref(),
        Some("2000000000")
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
        store.get("drive/refresh_token").unwrap().as_deref(),
        Some("1//refresh-secret")
    );
    forget_account(&store, "drive");
    assert_eq!(store.keys(), ["b2/access_key_id", "b2/secret_access_key"]);
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
    assert!(CloudProvider::connect(
        &account("bad id", CloudKind::S3),
        Arc::new(MemoryStore::default()),
        crossbeam_channel::unbounded().0
    )
    .is_err());
}

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
    assert_eq!(
        wired.downcast_ref::<io::Error>().map(io::Error::kind),
        Some(io::ErrorKind::PermissionDenied)
    );
    assert!(is_not_found(&wire(&err(ErrorKind::NotFound, 404), &p)));
}

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
    short.set_ttl(Duration::from_millis(30));
    put(&short, "cloud://mem/a.txt", b"a");
    let root = vp("cloud://mem/");
    assert_eq!(names(&short.list(&root).unwrap()), ["a.txt"]);
    short.core.op.read().write("c.txt", b"c".to_vec()).unwrap();
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(names(&short.list(&root).unwrap()), ["a.txt", "c.txt"]);
}

#[test]
fn read_write_create_new_and_dropped_uploads() {
    let cloud = memory_cloud("mem");
    put(&cloud, "cloud://mem/a.txt", b"hello");
    assert_eq!(get(&cloud, "cloud://mem/a.txt"), b"hello");
    put(&cloud, "cloud://mem/a.txt", b"replaced");
    assert_eq!(get(&cloud, "cloud://mem/a.txt"), b"replaced");
    let err = cloud.create_new(&vp("cloud://mem/a.txt")).err().unwrap();
    assert_eq!(
        err.downcast_ref::<io::Error>().map(io::Error::kind),
        Some(io::ErrorKind::AlreadyExists)
    );
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
    assert_eq!(
        err.downcast_ref::<io::Error>().map(io::Error::kind),
        Some(io::ErrorKind::AlreadyExists)
    );
    cloud.rename_noreplace(&a, &c).unwrap();
    assert!(is_not_found(&cloud.stat(&a).unwrap_err()));
    cloud.rename_replace(&c, &b).unwrap();
    assert_eq!(get(&cloud, "cloud://mem/b.txt"), b"a");
    // Folders move with their contents (file by file).
    cloud.mkdir(&vp("cloud://mem/d")).unwrap();
    cloud.mkdir(&vp("cloud://mem/d/sub")).unwrap();
    put(&cloud, "cloud://mem/d/sub/x.txt", b"x");
    put(&cloud, "cloud://mem/d/y.txt", b"y");
    assert!(cloud.mkdir(&vp("cloud://mem/d")).is_err());
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

/// A token endpoint answering every refresh with `reply` (status, JSON body).
fn token_server(status: u16, reply: &'static str) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let _ =
                request.respond(tiny_http::Response::from_string(reply).with_status_code(status));
        }
    });
    format!("http://127.0.0.1:{port}/token")
}
fn oauth_cloud(
    token_url: String,
    expires_at: SystemTime,
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
    let oauth = OAuth {
        client: OAuthClient {
            id: "app".into(),
            secret: None,
        },
        endpoints,
        expires_at: Mutex::new(expires_at),
        refreshing: Mutex::new(()),
    };
    let op = opendal::Operator::new(opendal::services::Memory::default()).unwrap();
    init();
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
fn unauthorized() -> opendal::Error {
    opendal::Error::new(ErrorKind::Unexpected, "expired").with_context(
        "response",
        "Parts { status: 401, version: HTTP/1.1, headers: {} }",
    )
}

#[test]
fn rejected_or_expired_tokens_refresh_once_and_are_stored() {
    let ok = r#"{"access_token":"at-2","expires_in":3600}"#;
    let far = SystemTime::now() + Duration::from_secs(3600);
    let (cloud, store, events) = oauth_cloud(token_server(200, ok), far);
    let p = vp("cloud://dbx/a");
    let calls = std::sync::atomic::AtomicU32::new(0);
    cloud
        .core
        .call(&p, |_| match calls.fetch_add(1, Ordering::SeqCst) {
            0 => Err(unauthorized()),
            _ => Ok(()),
        })
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        store.get("dbx/access_token").unwrap().as_deref(),
        Some("at-2")
    );
    assert_eq!(
        store.get("dbx/refresh_token").unwrap().as_deref(),
        Some("rt")
    );
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
            Err::<(), _>(unauthorized())
        })
        .unwrap_err();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        err.downcast_ref::<io::Error>().map(io::Error::kind),
        Some(io::ErrorKind::PermissionDenied)
    );
    // Expired before the call: refreshed first, the operation runs once.
    let (cloud, store, _) = oauth_cloud(token_server(200, ok), UNIX_EPOCH);
    calls.store(0, Ordering::SeqCst);
    cloud
        .core
        .call(&p, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        store.get("dbx/access_token").unwrap().as_deref(),
        Some("at-2")
    );
}

#[test]
fn a_revoked_grant_asks_to_sign_in_again() {
    let (cloud, store, events) = oauth_cloud(
        token_server(400, r#"{"error":"invalid_grant"}"#),
        UNIX_EPOCH,
    );
    let err = cloud.list(&vp("cloud://dbx/")).unwrap_err();
    assert!(format!("{err:#}").contains("sign in again"), "{err:#}");
    assert!(matches!(
        events.try_recv(),
        Ok(RemoteEvent::Status {
            status: ConnStatus::Failed,
            ..
        })
    ));
    assert_eq!(store.get("dbx/access_token").unwrap(), None);
    // Signed out entirely (no refresh token): same message, no request.
    store.delete("dbx/refresh_token").unwrap();
    let err = cloud.stat(&vp("cloud://dbx/x")).unwrap_err();
    assert!(format!("{err:#}").contains("sign in again"), "{err:#}");
}

/// opendal's `install_default` installs no HTTP transport with the rustls-no-provider
/// feature; without Keel's own every request failed as "ConfigInvalid".
#[test]
fn s3_requests_reach_the_network() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let _ = request.respond(tiny_http::Response::empty(403));
        }
    });
    let mut a = account("s3net", CloudKind::S3);
    a.s3 = Some(S3Config {
        endpoint: format!("http://127.0.0.1:{port}"),
        region: "us-west-002".into(),
        bucket: "bucket".into(),
    });
    let store = Arc::new(MemoryStore::default());
    store.set("s3net/access_key_id", "AKID").unwrap();
    store.set("s3net/secret_access_key", "SECRET").unwrap();
    let p = CloudProvider::connect(&a, store, crossbeam_channel::unbounded().0).unwrap();
    let err = p.list(&vp("cloud://s3net/")).unwrap_err();
    assert!(format!("{err:#}").contains("HTTP 403"), "{err:#}");
    assert!(!format!("{err:#}").contains("SECRET"));
}
