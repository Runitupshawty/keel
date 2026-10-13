use super::*;
use crate::types::*;
use serde_json::json;
use std::time::Duration;

/// Every op's params schema is an object schema that validates its own example, and the
/// example survives a typed round trip (deserialize into the params type and back).
#[test]
fn schemas_round_trip() {
    for op in OPS {
        let params = (op.params)();
        assert_eq!(
            params["type"], "object",
            "{}: params must be an object",
            op.name
        );
        let validator = jsonschema::validator_for(&params)
            .unwrap_or_else(|e| panic!("{}: bad params schema: {e}", op.name));
        let example = (op.example)();
        let errors: Vec<String> = validator
            .iter_errors(&example)
            .map(|e| e.to_string())
            .collect();
        assert!(
            errors.is_empty(),
            "{}: example invalid: {errors:?}",
            op.name
        );
        let typed = (op.check)(example.clone())
            .unwrap_or_else(|e| panic!("{}: example does not parse: {e}", op.name));
        assert!(
            validator.is_valid(&typed),
            "{}: canonical form invalid",
            op.name
        );
        assert_eq!((op.check)(typed.clone()).unwrap(), typed, "{}", op.name);
        // An unknown field is refused by both the schema and the type.
        let mut extra = example.clone();
        extra["bogus"] = json!(1);
        assert!(
            !validator.is_valid(&extra),
            "{}: schema allows extras",
            op.name
        );
        assert!(
            (op.check)(extra).is_err(),
            "{}: type allows extras",
            op.name
        );
        jsonschema::validator_for(&(op.result)())
            .unwrap_or_else(|e| panic!("{}: bad result schema: {e}", op.name));
        if let Run::Previewed { applied, .. } = op.run {
            jsonschema::validator_for(&applied())
                .unwrap_or_else(|e| panic!("{}: bad applied schema: {e}", op.name));
        }
    }
}

#[test]
fn registry_is_preview_first() {
    let mut names: Vec<_> = OPS.iter().map(|o| o.name).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), OPS.len(), "duplicate operation names");
    for op in OPS {
        let direct = crate::DIRECT.contains(&op.name);
        if op.mutating && !direct {
            assert!(op.previewed(), "{} mutates without a preview", op.name);
        }
        if direct {
            assert!(op.mutating && !op.previewed(), "{}", op.name);
        }
        if op.previewed() {
            assert!(op.mutating, "{}", op.name);
            assert!(!direct, "{}", op.name);
        }
        assert!(
            op.name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_'),
            "{}",
            op.name
        );
    }
    for name in [
        "version",
        "sources.list",
        "sources.add",
        "sources.remove",
        "sources.index",
        "list",
        "stat",
        "read",
        "preview.render",
        "media.thumb",
        "file.get",
        "search",
        "tags.list",
        "tags.add",
        "tags.remove",
        "tags.set",
        "favorites.list",
        "favorites.set",
        "recents",
        "jobs.list",
        "jobs.info",
        "jobs.cancel",
        "duplicates",
        "redundancy",
        "plan",
        "execute",
        "devices.list",
        "devices.pair_code",
        "devices.pair_with",
        "devices.forget",
        "shares.list",
        "shares.grant",
        "shares.revoke",
        "mounts.list",
        "mounts.add",
        "mounts.remove",
        "spacedrop.send",
        "spacedrop.inbox",
        "spacedrop.answer",
        "tags.tagged",
        "views.list",
        "views.create",
        "views.update",
        "views.delete",
        "recents.note",
        "redundancy.folder",
        "library.stats",
        "library.folder_sizes",
        "library.inspect",
        "files.names",
        "protection.summary",
        "volumes.list",
        "volumes.set",
        "integrity.check",
        "hashing.set",
        "media.index",
        "contents.set",
        "activity.note",
        "du.scan",
        "du.result",
        "du.export",
        "devices.settings",
        "devices.settings_set",
        "library.sync",
        "library.backup",
        "library.restore",
        "library.folder_style",
        "library.set_folder_style",
        "system.search",
        "library.comment",
        "library.set_comment",
    ] {
        assert!(find(name).is_some(), "{name} missing");
    }
}

struct Fixture {
    _data: tempfile::TempDir,
    files: tempfile::TempDir,
    /// The configuration folder the ctx guards.
    cfg: tempfile::TempDir,
    ctx: Ctx,
}

fn fixture(plans: Option<PlanStore>) -> Fixture {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    std::fs::create_dir(files.path().join("docs")).unwrap();
    std::fs::write(files.path().join("docs/invoice-2026.pdf"), b"pdf bytes").unwrap();
    std::fs::write(files.path().join("docs/notes.txt"), b"some notes").unwrap();
    let lib = keel_core::Library::open(data.path(), "test").unwrap();
    lib.set_hash_after_walk(false);
    let router = Arc::new(keel_vfs::Router::new());
    lib.set_router(router.clone());
    let mut ctx = Ctx::new(Arc::new(lib), router);
    let cfg = tempfile::tempdir().unwrap();
    ctx.config_dir = Some(cfg.path().to_owned());
    ctx.data_dir = Some(data.path().to_owned());
    if let Some(plans) = plans {
        ctx = ctx.with_plans(plans);
    }
    Fixture {
        _data: data,
        files,
        cfg,
        ctx,
    }
}

fn s(p: &std::path::Path) -> String {
    p.display().to_string()
}

/// Previews then executes `method`, returning the applied result.
fn apply(ctx: &Ctx, method: &str, params: Value) -> Value {
    let preview: PlanPreview = serde_json::from_value(call(ctx, method, params).unwrap()).unwrap();
    assert_eq!(preview.operation, method);
    let done = call(
        ctx,
        "execute",
        json!({"plan_id": preview.plan_id, "input_hash": preview.input_hash}),
    )
    .unwrap();
    done["result"].clone()
}

fn add_and_index(f: &Fixture) -> String {
    let added = apply(&f.ctx, "sources.add", json!({"root": s(f.files.path())}));
    let id = added["id"].as_str().unwrap().to_owned();
    let job = apply(&f.ctx, "sources.index", json!({"id": id}))["job"]
        .as_i64()
        .unwrap();
    let info = f.ctx.lib.jobs().wait(job).unwrap();
    assert_eq!(info.status, keel_core::JobStatus::Done, "{}", info.log);
    id
}

#[test]
fn mutating_calls_only_preview_until_executed() {
    let f = fixture(None);
    let preview = call(&f.ctx, "sources.add", json!({"root": s(f.files.path())})).unwrap();
    assert!(preview["plan_id"].is_string(), "{preview}");
    assert!(f.ctx.lib.sources().is_empty(), "a preview adds nothing");
    let id = add_and_index(&f);
    let sources = call(&f.ctx, "sources.list", Value::Null).unwrap();
    assert_eq!(sources[0]["id"], id);
    assert_eq!(sources[0]["status"], "online");

    let hits = call(&f.ctx, "search", json!({"query": "invoice"})).unwrap();
    assert_eq!(hits.as_array().unwrap().len(), 1, "{hits}");
    let pdf = hits[0]["path"].as_str().unwrap().to_owned();
    assert!(pdf.ends_with("invoice-2026.pdf"), "{pdf}");

    // Tags: preview first, then applied; creating the tag is announced.
    let preview = call(
        &f.ctx,
        "tags.add",
        json!({"tag": "receipts", "paths": [pdf]}),
    )
    .unwrap();
    assert_eq!(preview["warnings"][0]["kind"], "creates_tag");
    assert!(call(&f.ctx, "tags.list", Value::Null)
        .unwrap()
        .as_array()
        .unwrap()
        .is_empty());
    apply(
        &f.ctx,
        "tags.add",
        json!({"tag": "receipts", "paths": [pdf]}),
    );
    let stat = call(&f.ctx, "stat", json!({"path": pdf})).unwrap();
    assert_eq!(stat["indexed"]["tags"], json!(["receipts"]));
    apply(
        &f.ctx,
        "tags.set",
        json!({"paths": [pdf], "tags": ["a", "b"]}),
    );
    let tags = call(&f.ctx, "tags.list", json!({"path": pdf})).unwrap();
    let names: Vec<_> = tags
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].clone())
        .collect();
    assert_eq!(names, vec![json!("a"), json!("b")]);
    apply(&f.ctx, "favorites.set", json!({"paths": [pdf]}));
    assert_eq!(
        call(&f.ctx, "favorites.list", Value::Null).unwrap()[0]["path"],
        pdf
    );

    // Listing through the router and through the library index.
    let listing = call(
        &f.ctx,
        "list",
        json!({"path": s(&f.files.path().join("docs"))}),
    )
    .unwrap();
    assert_eq!(listing["entries"].as_array().unwrap().len(), 2);
    let lib_root = keel_vfs::library::path(&id, "docs").display();
    let listing = call(&f.ctx, "list", json!({"path": lib_root})).unwrap();
    assert_eq!(listing["entries"].as_array().unwrap().len(), 2, "{listing}");

    // Removal previews too, then reports what it did.
    let removed = apply(&f.ctx, "sources.remove", json!({"id": id}));
    assert_eq!(removed["removed"]["id"], id);
    assert!(f.ctx.lib.sources().is_empty());
}

/// Task 67: a Get Info comment, previewed then set, read back and cleared.
#[test]
fn comments_are_previewed_set_read_and_cleared() {
    let f = fixture(None);
    add_and_index(&f);
    let notes = s(&f.files.path().join("docs/notes.txt"));
    let read = || call(&f.ctx, "library.comment", json!({ "path": notes })).unwrap();
    assert_eq!(read().get("comment"), None);
    let params = json!({"path": notes, "comment": "Send to the printer"});
    let preview = call(&f.ctx, "library.set_comment", params.clone()).unwrap();
    assert_eq!(preview["changes"][0]["action"], "comment");
    assert_eq!(read().get("comment"), None, "a preview sets nothing");
    let set = apply(&f.ctx, "library.set_comment", params);
    assert_eq!(set["comment"], "Send to the printer");
    assert_eq!(read(), set);
    apply(
        &f.ctx,
        "library.set_comment",
        json!({"path": notes, "comment": ""}),
    );
    assert_eq!(read().get("comment"), None);
    let outside = tempfile::tempdir().unwrap();
    let err = call(
        &f.ctx,
        "library.set_comment",
        json!({"path": s(outside.path()), "comment": "x"}),
    )
    .unwrap_err();
    assert_eq!(err.code, ApiError::NOT_FOUND);
}

/// Task 56: a folder's color and icon, previewed then set; omitted fields stay, "" clears.
#[test]
fn folder_styles_are_previewed_set_read_and_cleared() {
    let f = fixture(None);
    let id = add_and_index(&f);
    let docs = s(&f.files.path().join("docs"));
    let read = |path: Option<&str>| {
        let params = match path {
            Some(p) => json!({ "path": p }),
            None => json!({}),
        };
        call(&f.ctx, "library.folder_style", params).unwrap()
    };
    assert_eq!(read(Some(&docs)), json!([{ "path": docs }]));
    assert_eq!(read(None), json!([]));
    let preview = call(
        &f.ctx,
        "library.set_folder_style",
        json!({"path": docs, "color": "#3b82f6", "icon": "📷"}),
    )
    .unwrap();
    assert_eq!(preview["changes"][0]["action"], "folder_style");
    assert_eq!(read(None), json!([]), "a preview sets nothing");
    let set = apply(
        &f.ctx,
        "library.set_folder_style",
        json!({"path": docs, "color": "#3b82f6", "icon": "📷"}),
    );
    assert_eq!(set, json!({"path": docs, "color": "#3b82f6", "icon": "📷"}));
    assert_eq!(read(None), json!([set]));
    // The same folder by its library path; only the icon changes.
    let lib_docs = keel_vfs::library::path(&id, "docs").display();
    apply(
        &f.ctx,
        "library.set_folder_style",
        json!({"path": lib_docs, "icon": "theme:docs"}),
    );
    assert_eq!(
        read(Some(&docs)),
        json!([{"path": docs, "color": "#3b82f6", "icon": "theme:docs"}])
    );
    // A source's root takes a style too.
    let root = s(f.files.path());
    apply(
        &f.ctx,
        "library.set_folder_style",
        json!({"path": root, "color": "#e5484d"}),
    );
    assert_eq!(read(None).as_array().unwrap().len(), 2);
    apply(
        &f.ctx,
        "library.set_folder_style",
        json!({"path": root, "color": ""}),
    );
    // A file takes an icon (synced and read like a folder's) but no color.
    let notes = s(&f.files.path().join("docs").join("notes.txt"));
    let set = apply(
        &f.ctx,
        "library.set_folder_style",
        json!({"path": notes, "icon": "theme:.txt"}),
    );
    assert_eq!(set, json!({"path": notes, "icon": "theme:.txt"}));
    assert_eq!(read(Some(&notes)), json!([set]));
    apply(
        &f.ctx,
        "library.set_folder_style",
        json!({"path": notes, "icon": ""}),
    );
    // Refused at the preview: bad values, a file's color, a folder outside the library.
    let refused = |params: Value| call(&f.ctx, "library.set_folder_style", params).unwrap_err();
    for bad in [
        json!({"path": docs, "color": "blue"}),
        json!({"path": docs, "icon": "📷📷"}),
        json!({"path": docs, "icon": "x"}),
        json!({"path": s(&f.files.path().join("docs/notes.txt")), "color": "#000000"}),
    ] {
        assert_eq!(refused(bad.clone()).code, ApiError::INVALID_PARAMS, "{bad}");
    }
    let outside = tempfile::tempdir().unwrap();
    let err = refused(json!({"path": s(outside.path()), "color": "#000000"}));
    assert_eq!(err.code, ApiError::NOT_FOUND);
    assert!(
        err.message
            .contains("add the folder's drive to the library"),
        "{err}"
    );
    // Cleared: both fields "".
    apply(
        &f.ctx,
        "library.set_folder_style",
        json!({"path": docs, "color": "", "icon": ""}),
    );
    assert_eq!(read(None), json!([]));
}

#[test]
fn execute_refuses_a_tampered_input() {
    let f = fixture(None);
    let dst = tempfile::tempdir().unwrap();
    let src = f.files.path().join("docs/notes.txt");
    let preview: PlanPreview = serde_json::from_value(
        call(
            &f.ctx,
            "plan",
            json!({"op": "copy", "paths": [s(&src)], "to": s(dst.path())}),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(preview.changes[0].action, "copy");
    assert!(
        !dst.path().join("notes.txt").exists(),
        "a preview copies nothing"
    );
    // The hash of another input (a different destination) is refused, and the plan stays.
    let other = plans::Input::Call {
        method: "plan".into(),
        params: json!({"to": "elsewhere"}),
    }
    .hash();
    for bad in [other.as_str(), "", "00"] {
        let err = call(
            &f.ctx,
            "execute",
            json!({"plan_id": preview.plan_id, "input_hash": bad}),
        )
        .unwrap_err();
        assert_eq!(err.code, ApiError::PLAN_MISMATCH, "{err}");
    }
    assert!(!dst.path().join("notes.txt").exists());
    let done = call(
        &f.ctx,
        "execute",
        json!({"plan_id": preview.plan_id, "input_hash": preview.input_hash}),
    )
    .unwrap();
    let job = done["job"].as_i64().unwrap();
    assert_eq!(
        f.ctx.lib.jobs().wait(job).unwrap().status,
        keel_core::JobStatus::Done
    );
    assert!(dst.path().join("notes.txt").exists());
    // One shot: the same plan cannot run twice.
    let err = call(
        &f.ctx,
        "execute",
        json!({"plan_id": preview.plan_id, "input_hash": preview.input_hash}),
    )
    .unwrap_err();
    assert_eq!(err.code, ApiError::PLAN_EXPIRED);
}

#[test]
fn execute_refuses_an_expired_plan() {
    let f = fixture(Some(PlanStore::memory(Duration::from_millis(50))));
    let preview = call(&f.ctx, "sources.add", json!({"root": s(f.files.path())})).unwrap();
    std::thread::sleep(Duration::from_millis(120));
    let err = call(
        &f.ctx,
        "execute",
        json!({"plan_id": preview["plan_id"], "input_hash": preview["input_hash"]}),
    )
    .unwrap_err();
    assert_eq!(err.code, ApiError::PLAN_EXPIRED, "{err}");
    assert!(f.ctx.lib.sources().is_empty());
}

#[test]
fn bad_calls_are_typed_errors() {
    let f = fixture(None);
    let err = call(&f.ctx, "nope", Value::Null).unwrap_err();
    assert_eq!(err.code, ApiError::METHOD_NOT_FOUND);
    let err = call(&f.ctx, "search", json!({"q": "x"})).unwrap_err();
    assert_eq!(err.code, ApiError::INVALID_PARAMS);
    let err = call(&f.ctx, "list", json!({"path": "relative/dir"})).unwrap_err();
    assert_eq!(err.code, ApiError::INVALID_PARAMS);
    let err = call(&f.ctx, "devices.list", Value::Null).unwrap_err();
    assert_eq!(err.code, ApiError::NET_DISABLED);
    let err = call(&f.ctx, "jobs.info", json!({"id": 999})).unwrap_err();
    assert_eq!(err.code, ApiError::NOT_FOUND);
}

/// Devices and shares on two offline loopback nodes: pairing codes and grants are
/// preview-first too, and revoke acts at once.
#[test]
fn devices_and_shares() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let open = |dir: &tempfile::TempDir| {
        rt.block_on(keel_net::Node::open_with_options(
            Arc::new(keel_vfs::cloud::MemoryStore::default()),
            dir.path(),
            Arc::new(net::NoSources::default()),
            keel_net::NodeOptions::offline(),
        ))
        .unwrap()
    };
    let (na, nb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = fixture(None);
    let a_node = open(&na);
    let a = Fixture {
        ctx: a.ctx.with_net(a_node.clone(), rt.handle().clone()),
        ..a
    };
    let b = fixture(None);
    let b_node = open(&nb);
    let b = Fixture {
        ctx: b.ctx.with_net(b_node.clone(), rt.handle().clone()),
        ..b
    };
    let devices = call(&a.ctx, "devices.list", Value::Null).unwrap();
    assert_eq!(devices["id"], a_node.id().to_string());
    assert!(devices["peers"].as_array().unwrap().is_empty());

    let code = apply(&a.ctx, "devices.pair_code", json!({}));
    let ticket = code["ticket"].as_str().unwrap().to_owned();
    let preview = call(&b.ctx, "devices.pair_with", json!({"code": ticket})).unwrap();
    assert!(
        !preview.to_string().contains(&ticket),
        "the preview never echoes the code"
    );
    apply(&b.ctx, "devices.pair_with", json!({"code": ticket}));
    let peers = call(&a.ctx, "devices.list", Value::Null).unwrap()["peers"].clone();
    assert_eq!(peers[0]["id"], b_node.id().to_string(), "{peers}");

    let source = apply(&a.ctx, "sources.add", json!({"root": s(a.files.path())}))["id"].clone();
    let grant = json!({"peer": b_node.id().to_string(), "source": source, "subtree": "docs", "access": "read"});
    call(&a.ctx, "shares.grant", grant.clone()).unwrap();
    assert!(a_node.grants().is_empty(), "a preview grants nothing");
    apply(&a.ctx, "shares.grant", grant);
    let shares = call(&a.ctx, "shares.list", Value::Null).unwrap();
    assert_eq!(shares[0]["subtree"], "docs");
    let revoked = call(
        &a.ctx,
        "shares.revoke",
        json!({"peer": b_node.id().to_string(), "source": source, "subtree": "docs"}),
    )
    .unwrap();
    assert_eq!(revoked["existed"], true);
    assert!(a_node.grants().is_empty());
    // Device writes reach a source on another provider too: a read-write share of one
    // whose deletes are permanent says so.
    a.ctx.router.register(Arc::new(Mem {
        data: Vec::new(),
        ranged: false,
        served: Default::default(),
    }));
    let def = keel_core::SourceDef {
        label: "Mem".into(),
        root: keel_vfs::VPath::parse("mem://x/share").unwrap(),
        kind: keel_core::SourceKind::Folder,
        include_hidden: false,
        ignore: Vec::new(),
        poll_secs: None,
        hash_shares: false,
    };
    let remote = a.ctx.lib.add_source(def).unwrap().0;
    let peer = b_node.id().to_string();
    let rw = |source: &Value| json!({"peer": peer, "source": source, "access": "read_write"});
    let preview = call(&a.ctx, "shares.grant", rw(&json!(remote))).unwrap();
    assert!(
        preview.to_string().contains("deletes_permanent"),
        "{preview}"
    );
    let preview = call(&a.ctx, "shares.grant", rw(&source)).unwrap();
    assert!(
        !preview.to_string().contains("deletes_permanent"),
        "{preview}"
    );
    let unknown = json!({"peer": "a".repeat(52), "source": source, "access": "read"});
    assert!(call(&a.ctx, "shares.grant", unknown).is_err());
    apply(
        &a.ctx,
        "devices.forget",
        json!({"peer": b_node.id().to_string()}),
    );
    assert!(a_node.peers().is_empty());
    rt.block_on(async {
        a_node.close().await;
        b_node.close().await;
    });
}

/// Library sync between two hosts: the switch through `devices.settings_set`, the state
/// in `devices.list`, a pull with `library.sync`.
#[test]
fn library_sync_between_two_hosts() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let open = |dir: &tempfile::TempDir, lib: &Arc<keel_core::Library>| {
        rt.block_on(keel_net::Node::open_with_options(
            Arc::new(keel_vfs::cloud::MemoryStore::default()),
            dir.path(),
            Arc::new(keel_net::LibraryHandler::new(lib.clone())),
            keel_net::NodeOptions {
                sync_every: std::time::Duration::ZERO,
                ..keel_net::NodeOptions::offline()
            },
        ))
        .unwrap()
    };
    let (na, nb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = fixture(None);
    let a_node = open(&na, &a.ctx.lib);
    let a = Fixture {
        ctx: a.ctx.with_net(a_node.clone(), rt.handle().clone()),
        ..a
    };
    let b = fixture(None);
    let b_node = open(&nb, &b.ctx.lib);
    let b = Fixture {
        ctx: b.ctx.with_net(b_node.clone(), rt.handle().clone()),
        ..b
    };
    let code = apply(&a.ctx, "devices.pair_code", json!({}));
    apply(&b.ctx, "devices.pair_with", json!({"code": code["ticket"]}));
    let (a_id, b_id) = (a_node.id().to_string(), b_node.id().to_string());
    let peers = call(&b.ctx, "devices.list", Value::Null).unwrap()["peers"].clone();
    assert_eq!(peers[0]["sync"], false, "off by default");
    assert!(peers[0].get("last_sync").is_none());
    let off = call(&b.ctx, "library.sync", json!({"peer": a_id})).unwrap_err();
    assert_eq!(off.code, ApiError::INVALID_PARAMS, "{off:?}");
    assert!(call(&b.ctx, "library.sync", json!({}))
        .unwrap()
        .as_array()
        .unwrap()
        .is_empty());

    // On, on both sides (a preview first, which changes nothing).
    let preview = call(&b.ctx, "devices.settings_set", json!({"sync": [a_id]})).unwrap();
    assert!(preview.to_string().contains("device.sync"), "{preview}");
    assert!(!b_node.syncs_with(&keel_net::PeerId(a_node.id())));
    apply(&b.ctx, "devices.settings_set", json!({"sync": [a_id]}));
    apply(&a.ctx, "devices.settings_set", json!({"sync": [b_id]}));
    let settings = call(&b.ctx, "devices.settings", Value::Null).unwrap();
    assert_eq!(settings["sync"], json!([a_id]));
    assert!(call(&b.ctx, "devices.settings_set", json!({"sync": ["nope"]})).is_err());

    a.ctx
        .lib
        .create_tag("Receipts", Some("#30a46c"), None)
        .unwrap();
    let pulled = call(&b.ctx, "library.sync", json!({"peer": a_id})).unwrap();
    assert_eq!(pulled[0]["peer"], a_id);
    assert_eq!(pulled[0]["applied"], 1, "{pulled}");
    assert!(pulled[0].get("error").is_none());
    let tags = call(&b.ctx, "tags.list", json!({})).unwrap();
    assert!(tags.to_string().contains("Receipts"), "{tags}");
    let peers = call(&b.ctx, "devices.list", Value::Null).unwrap()["peers"].clone();
    assert_eq!(peers[0]["sync"], true);
    assert!(peers[0]["last_sync"].as_i64().is_some(), "{peers}");

    // Off on a: b's pull is refused and says why.
    apply(&a.ctx, "devices.settings_set", json!({"sync": []}));
    let refused = call(&b.ctx, "library.sync", json!({})).unwrap();
    assert!(
        refused[0]["error"].as_str().unwrap().contains("refused"),
        "{refused}"
    );
    rt.block_on(async {
        a_node.close().await;
        b_node.close().await;
    });
}

fn unb64(v: &Value) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(v.as_str().unwrap())
        .unwrap()
}

#[test]
fn reads_ranges_previews_thumbs_and_one_time_links() {
    let f = fixture(None);
    let photo = f.files.path().join("docs/photo.png");
    image::RgbaImage::from_pixel(600, 300, image::Rgba([200, 30, 30, 255]))
        .save(&photo)
        .unwrap();
    let id = add_and_index(&f);
    let notes = format!("library://{id}/docs/notes.txt");

    // Ranges, through the index path as well as the real one.
    let chunk = call(
        &f.ctx,
        "read",
        json!({"path": notes, "offset": 5, "len": 3}),
    )
    .unwrap();
    assert_eq!(unb64(&chunk["data"]), b"not");
    assert_eq!(chunk["eof"], false);
    let rest = call(
        &f.ctx,
        "read",
        json!({"path": s(&f.files.path().join("docs/notes.txt")), "offset": 5}),
    )
    .unwrap();
    assert_eq!(unb64(&rest["data"]), b"notes");
    assert_eq!(rest["eof"], true);
    let folder = call(&f.ctx, "read", json!({"path": s(f.files.path())})).unwrap_err();
    assert_eq!(folder.code, ApiError::INVALID_PARAMS);

    // Previews: text as text, images as a bounded PNG.
    let text = call(&f.ctx, "preview.render", json!({"path": notes})).unwrap();
    assert_eq!(text["kind"], "text");
    assert_eq!(text["text"], "some notes");
    let img = call(
        &f.ctx,
        "preview.render",
        json!({"path": s(&photo), "max_px": 100}),
    )
    .unwrap();
    assert_eq!(img["kind"], "image", "{img}");
    assert_eq!(
        (img["width"].as_u64(), img["height"].as_u64()),
        (Some(100), Some(50))
    );
    let png = image::load_from_memory(&unb64(&img["png"])).unwrap();
    assert_eq!(png.width(), 100);

    // Thumbnails come from (and land in) the sidecar store.
    let thumb = call(&f.ctx, "media.thumb", json!({"path": s(&photo)})).unwrap();
    assert_eq!(thumb["mime"], "image/webp");
    let webp = image::load_from_memory(&unb64(&thumb["data"])).unwrap();
    assert_eq!(webp.width().max(webp.height()), 256);
    let stats = f.ctx.lib.sidecars().unwrap().stats();
    assert!(stats.keys >= 1, "{stats:?}");
    assert!(call(&f.ctx, "media.thumb", json!({"path": notes})).is_err());
    // `make: false` answers only from sidecars that exist; a content id needs 64 hex digits.
    let big = json!({"path": s(&photo), "size": "thumb1024", "make": false});
    let missing = call(&f.ctx, "media.thumb", big).unwrap_err();
    assert_eq!(missing.code, ApiError::NOT_FOUND, "{missing:?}");
    let nothing = call(&f.ctx, "media.thumb", json!({})).unwrap_err();
    assert_eq!(nothing.code, ApiError::INVALID_PARAMS);
    let bad = call(&f.ctx, "media.thumb", json!({"content_id": "zz"})).unwrap_err();
    assert_eq!(bad.code, ApiError::INVALID_PARAMS);
    let unknown = json!({"content_id": "0".repeat(64)});
    let unknown = call(&f.ctx, "media.thumb", unknown).unwrap_err();
    assert_eq!(unknown.code, ApiError::NOT_FOUND);

    // A download link works once.
    let link: FileLink =
        serde_json::from_value(call(&f.ctx, "file.get", json!({"path": notes})).unwrap()).unwrap();
    assert_eq!((link.name.as_str(), link.size), ("notes.txt", 10));
    let token = link.url.strip_prefix("/file/").unwrap();
    assert_eq!(token.len(), 64);
    let (name, size, mut body) = crate::files::open_link(&f.ctx, token).unwrap();
    let mut got = String::new();
    std::io::Read::read_to_string(&mut body, &mut got).unwrap();
    assert_eq!(
        (name.as_str(), size, got.as_str()),
        ("notes.txt", 10, "some notes")
    );
    assert!(
        crate::files::open_link(&f.ctx, token).is_err(),
        "used twice"
    );
    assert!(crate::files::open_link(&f.ctx, "0".repeat(64).as_str()).is_err());
}

#[test]
fn file_plans_take_library_paths() {
    let f = fixture(None);
    let id = add_and_index(&f);
    let preview: PlanPreview = serde_json::from_value(
        call(
            &f.ctx,
            "plan",
            json!({"op": "rename", "paths": [format!("library://{id}/docs/notes.txt")], "new_name": "notes-2026.txt"}),
        )
        .unwrap(),
    )
    .unwrap();
    let done = call(
        &f.ctx,
        "execute",
        json!({"plan_id": preview.plan_id, "input_hash": preview.input_hash}),
    )
    .unwrap();
    let job = done["job"].as_i64().unwrap();
    let info = f.ctx.lib.jobs().wait(job).unwrap();
    assert_eq!(info.status, keel_core::JobStatus::Done, "{}", info.log);
    assert!(f.files.path().join("docs/notes-2026.txt").is_file());
    assert!(!f.files.path().join("docs/notes.txt").exists());
}

/// `sources.remove` over MCP: the call previews (store size, tags, favorites) and deletes
/// nothing; only an `execute` the user confirmed removes the source and its store.
#[test]
fn sources_remove_over_mcp_previews_and_deletes_only_on_execute() {
    let f = fixture(None);
    let id = add_and_index(&f);
    let pdf = s(&f.files.path().join("docs/invoice-2026.pdf"));
    apply(
        &f.ctx,
        "tags.add",
        json!({"tag": "receipts", "paths": [pdf]}),
    );
    apply(&f.ctx, "favorites.set", json!({"paths": [pdf]}));
    let store = f
        .ctx
        .lib
        .source(&keel_core::SourceId(id.clone()))
        .unwrap()
        .store_dir()
        .to_owned();
    assert!(store.is_dir());

    let init = |elicit: bool| {
        let caps = if elicit {
            json!({"elicitation": {}})
        } else {
            json!({})
        };
        json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":caps,"clientInfo":{"name":"t","version":"1"}}})
    };
    let remove = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"sources_remove","arguments":{"id": id, "delete_store": true}}});
    let run = |ctx: &mut Ctx, msgs: &[Value]| -> Vec<Value> {
        let input: String = msgs.iter().map(|m| m.to_string() + "\n").collect();
        let mut out = Vec::new();
        crate::mcp::serve(ctx, input.as_bytes(), &mut out, Default::default()).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    let Fixture {
        _data,
        files,
        cfg,
        mut ctx,
    } = f;
    let _keep = (files, cfg);

    // An agent calls the tool: a preview, nothing removed.
    let out = run(&mut ctx, &[init(false), remove.clone()]);
    let preview = &out[1]["result"]["structuredContent"];
    let summary = preview["summary"].as_str().unwrap().to_owned();
    assert!(summary.contains("1 tag(s), 1 favorite(s)"), "{summary}");
    assert!(summary.contains("bytes"), "{summary}");
    assert_eq!(preview["warnings"][0]["kind"], "deletes_store");
    assert_eq!(ctx.lib.sources().len(), 1, "a preview removes nothing");
    assert!(store.is_dir());

    // Executing without a person (no elicitation) is refused: still nothing removed.
    let plan = preview["plan_id"].clone();
    let hash = preview["input_hash"].clone();
    let exec = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"execute","arguments":{"plan_id": plan, "input_hash": hash, "summary": summary}}});
    let out = run(&mut ctx, &[init(false), remove.clone(), exec]);
    assert_eq!(out[2]["result"]["isError"], true, "{}", out[2]);
    assert_eq!(ctx.lib.sources().len(), 1);
    assert!(store.is_dir());

    // The user confirms (one live session: execute needs that session's preview):
    // removed, store deleted.
    struct Lines(crossbeam_channel::Sender<Value>, Vec<u8>);
    impl std::io::Write for Lines {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.1.extend_from_slice(b);
            while let Some(i) = self.1.iter().position(|&c| c == b'\n') {
                let line: Vec<u8> = self.1.drain(..=i).collect();
                let _ = self.0.send(serde_json::from_slice(&line).unwrap());
            }
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let (reader, mut writer) = std::io::pipe().unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    let server = std::thread::spawn(move || {
        let input = std::io::BufReader::new(reader);
        crate::mcp::serve(&mut ctx, input, Lines(tx, Vec::new()), Default::default()).unwrap();
        ctx
    });
    let mut send = move |v: Value| {
        use std::io::Write;
        writeln!(writer, "{v}").unwrap();
    };
    let recv = |want: Value| loop {
        let v: Value = rx.recv_timeout(Duration::from_secs(30)).unwrap();
        if v["id"] == want {
            return v;
        }
    };
    send(init(true));
    recv(json!(0));
    send(remove);
    let preview = recv(json!(1))["result"]["structuredContent"].clone();
    send(
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"execute","arguments":{"plan_id": preview["plan_id"], "input_hash": preview["input_hash"]}}}),
    );
    let ask = recv(json!("keel-confirm-1"));
    assert!(ask["params"]["message"]
        .as_str()
        .unwrap()
        .contains("1 favorite(s)"));
    send(
        json!({"jsonrpc":"2.0","id":"keel-confirm-1","result":{"action":"accept","content":{"confirm":true}}}),
    );
    let done = recv(json!(2));
    assert_eq!(done["result"]["isError"], false, "{done}");
    drop(send);
    let ctx = server.join().unwrap();
    assert!(ctx.lib.sources().is_empty());
    assert!(!store.exists(), "store deleted");
}

/// The read operations refuse Keel's configuration folder (however the path is written)
/// and, on Windows, UNC and device paths outside the library's sources.
#[test]
fn reads_refuse_the_config_folder_and_network_paths() {
    let f = fixture(None);
    let token = f.cfg.path().join("daemon.token");
    std::fs::write(&token, b"secret").unwrap();
    let sub = f.cfg.path().join("profiles");
    std::fs::create_dir(&sub).unwrap();
    let dotted = s(&sub.join("..").join("daemon.token"));
    for (method, params) in [
        ("read", json!({"path": s(&token)})),
        ("read", json!({"path": dotted})),
        ("stat", json!({"path": s(&token)})),
        ("list", json!({"path": s(f.cfg.path())})),
        ("list", json!({"path": s(&sub)})),
        ("preview.render", json!({"path": s(&token)})),
        ("media.thumb", json!({"path": s(&token)})),
        ("file.get", json!({"path": s(&token)})),
    ] {
        let e = call(&f.ctx, method, params.clone()).unwrap_err();
        assert!(
            e.message.contains("configuration folder"),
            "{method} {params}: {e:?}"
        );
    }
    // A source over the config folder does not open it either.
    let added = apply(&f.ctx, "sources.add", json!({"root": s(f.cfg.path())}));
    let lib = format!("library://{}/daemon.token", added["id"].as_str().unwrap());
    assert!(call(&f.ctx, "read", json!({"path": lib})).is_err());
    // Other files read as before.
    let notes = s(&f.files.path().join("docs/notes.txt"));
    assert!(call(&f.ctx, "read", json!({"path": notes})).is_ok());
    if cfg!(windows) {
        for unc in [
            r"\\example.invalid\share\a.txt",
            r"\\?\UNC\example.invalid\share\a.txt",
            r"\\.\pipe\x",
        ] {
            for method in ["read", "stat", "list", "file.get"] {
                let e = call(&f.ctx, method, json!({"path": unc})).unwrap_err();
                assert!(e.message.contains("network"), "{method} {unc}: {e:?}");
            }
        }
    }
}

/// A provider for `mem://` paths that counts the bytes it hands out.
struct Mem {
    data: Vec<u8>,
    ranged: bool,
    served: Arc<std::sync::atomic::AtomicU64>,
}

struct Counted<R>(R, Arc<std::sync::atomic::AtomicU64>);

impl<R: std::io::Read> std::io::Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.0.read(buf)?;
        self.1
            .fetch_add(n as u64, std::sync::atomic::Ordering::SeqCst);
        Ok(n)
    }
}

impl keel_vfs::Provider for Mem {
    fn scheme(&self) -> &'static str {
        "mem"
    }
    fn caps(&self) -> keel_vfs::Caps {
        keel_vfs::Caps::default()
    }
    fn list(&self, _: &keel_vfs::VPath) -> anyhow::Result<Vec<keel_vfs::Entry>> {
        Ok(Vec::new())
    }
    fn list_complete(&self, d: &keel_vfs::VPath) -> anyhow::Result<Vec<keel_vfs::Entry>> {
        self.list(d)
    }
    fn stat(&self, p: &keel_vfs::VPath) -> anyhow::Result<keel_vfs::Entry> {
        Ok(keel_vfs::Entry {
            path: p.clone(),
            name: p.name().to_owned(),
            kind: keel_vfs::Kind::File,
            size: self.data.len() as u64,
            modified: None,
            hidden: false,
            is_link: false,
            encrypted: false,
            ext: String::new(),
        })
    }
    fn read(&self, _: &keel_vfs::VPath) -> anyhow::Result<Box<dyn std::io::Read + Send>> {
        Ok(Box::new(Counted(
            std::io::Cursor::new(self.data.clone()),
            self.served.clone(),
        )))
    }
    fn read_range(
        &self,
        _: &keel_vfs::VPath,
        offset: u64,
        len: u64,
    ) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        if !self.ranged {
            return Ok(None);
        }
        let start = (offset as usize).min(self.data.len());
        let end = start.saturating_add(len as usize).min(self.data.len());
        Ok(Some(Box::new(Counted(
            std::io::Cursor::new(self.data[start..end].to_vec()),
            self.served.clone(),
        ))))
    }
    fn write(&self, _: &keel_vfs::VPath) -> anyhow::Result<Box<dyn std::io::Write + Send>> {
        anyhow::bail!("read-only")
    }
    fn mkdir(&self, _: &keel_vfs::VPath) -> anyhow::Result<()> {
        anyhow::bail!("read-only")
    }
    fn rename(&self, _: &keel_vfs::VPath, _: &keel_vfs::VPath) -> anyhow::Result<()> {
        anyhow::bail!("read-only")
    }
    fn remove(&self, _: &keel_vfs::VPath) -> anyhow::Result<()> {
        anyhow::bail!("read-only")
    }
    fn remove_kind(&self) -> keel_vfs::RemoveKind {
        keel_vfs::RemoveKind::Permanent
    }
    fn local_copy(&self, _: &keel_vfs::VPath) -> anyhow::Result<std::path::PathBuf> {
        anyhow::bail!("no local copy")
    }
}

/// Remote reads ask the provider for the range (no re-reading from byte 0); a provider
/// without ranges is read up to the offset, which is capped.
#[test]
fn remote_reads_use_ranges_and_cap_skipping() {
    let f = fixture(None);
    let data: Vec<u8> = (0..200u8).collect();
    let served = Arc::new(std::sync::atomic::AtomicU64::new(0));
    f.ctx.router.register(Arc::new(Mem {
        data: data.clone(),
        ranged: true,
        served: served.clone(),
    }));
    let chunk = call(
        &f.ctx,
        "read",
        json!({"path": "mem://x/file.bin", "offset": 150, "len": 10}),
    )
    .unwrap();
    assert_eq!(unb64(&chunk["data"]), &data[150..160]);
    assert_eq!(chunk["eof"], false);
    assert_eq!(
        served.load(std::sync::atomic::Ordering::SeqCst),
        11,
        "only the range"
    );

    served.store(0, std::sync::atomic::Ordering::SeqCst);
    f.ctx.router.register(Arc::new(Mem {
        data: data.clone(),
        ranged: false,
        served: served.clone(),
    }));
    let tail = call(
        &f.ctx,
        "read",
        json!({"path": "mem://x/file.bin", "offset": 195}),
    )
    .unwrap();
    assert_eq!(unb64(&tail["data"]), &data[195..]);
    assert_eq!(tail["eof"], true);
    let far = call(
        &f.ctx,
        "read",
        json!({"path": "mem://x/file.bin", "offset": crate::files::SKIP_MAX + 1, "len": 1}),
    )
    .unwrap_err();
    assert_eq!(far.code, ApiError::INVALID_PARAMS, "{far:?}");
}

/// File-plan summaries (what `--allow-execute` clients echo) name the first three paths.
#[test]
fn file_plan_summaries_name_the_files() {
    let f = fixture(None);
    let docs = f.files.path().join("docs");
    for n in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(docs.join(n), n).unwrap();
    }
    let mut names: Vec<String> = std::fs::read_dir(&docs)
        .unwrap()
        .map(|e| s(&e.unwrap().path()))
        .collect();
    names.sort();
    let preview: PlanPreview = serde_json::from_value(
        call(&f.ctx, "plan", json!({"op": "delete", "paths": names})).unwrap(),
    )
    .unwrap();
    for shown in &names[..3] {
        assert!(
            preview.summary.contains(shown.as_str()),
            "{}",
            preview.summary
        );
    }
    assert!(!preview.summary.contains(names[4].as_str()));
    assert!(preview.summary.contains("2 more"), "{}", preview.summary);
}

#[test]
fn mounts_need_the_daemon_and_a_backend() {
    let f = fixture(None);
    let id = add_and_index(&f);
    let target = if cfg!(windows) {
        "Q:"
    } else {
        "/nonexistent-keel-mount"
    };
    // In-process (the CLI without a daemon): nothing is mounted and nothing can be.
    assert_eq!(call(&f.ctx, "mounts.list", Value::Null).unwrap(), json!([]));
    let e = call(
        &f.ctx,
        "mounts.add",
        json!({"source": id, "target": target}),
    )
    .unwrap_err();
    assert_eq!(e.code, ApiError::MOUNTS_UNAVAILABLE, "{e}");
    assert!(e.message.contains("keel-daemon"), "{e}");
    let e = call(&f.ctx, "mounts.remove", json!({"target": target})).unwrap_err();
    assert_eq!(e.code, ApiError::NOT_FOUND, "{e}");

    // A host that serves mounts: refused without a backend, else checked before the preview.
    let spool = tempfile::tempdir().unwrap();
    let ctx = Ctx::new(f.ctx.lib.clone(), f.ctx.router.clone())
        .with_mounts(Arc::new(keel_mount::Mounts::new(spool.path().to_owned())));
    let r = call(
        &ctx,
        "mounts.add",
        json!({"source": id, "subtree": "docs", "target": target}),
    );
    // With a backend this only previews (keel-mount's ignored round trip mounts for real).
    if keel_mount::backend().is_none() {
        let e = r.unwrap_err();
        assert_eq!(e.code, ApiError::MOUNTS_UNAVAILABLE, "{e}");
        assert!(e.message.contains("--features"), "{e}");
    }
    let e = call(
        &ctx,
        "mounts.add",
        json!({"source": "nope", "target": target}),
    )
    .unwrap_err();
    assert!(
        [ApiError::MOUNTS_UNAVAILABLE, ApiError::NOT_FOUND].contains(&e.code),
        "{e}"
    );
    assert_eq!(call(&ctx, "mounts.list", Value::Null).unwrap(), json!([]));
}

/// Spacedrop between two offline loopback nodes: the preview lists the files and sizes and
/// sends nothing, execute starts a job, the offer waits in the receiver's inbox until
/// `spacedrop.answer` (previewed too) accepts it, and the files land in the inbox.
#[test]
fn spacedrop_send_inbox_and_answer() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let inbox = tempfile::tempdir().unwrap();
    let drops = Arc::new(net::Drops::new(inbox.path().join("in"), Vec::new()));
    let open = |dir: &tempfile::TempDir, handler: net::NoSources| {
        rt.block_on(keel_net::Node::open_with_options(
            Arc::new(keel_vfs::cloud::MemoryStore::default()),
            dir.path(),
            Arc::new(handler),
            keel_net::NodeOptions::offline(),
        ))
        .unwrap()
    };
    let (na, nb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = fixture(None);
    let a_node = open(&na, net::NoSources::default());
    let a = Fixture {
        ctx: a.ctx.with_net(a_node.clone(), rt.handle().clone()),
        ..a
    };
    let b = fixture(None);
    let b_node = open(
        &nb,
        net::NoSources {
            drops: Some(drops.clone()),
        },
    );
    let mut b_ctx = b.ctx.with_net(b_node.clone(), rt.handle().clone());
    b_ctx.drops = Some(drops);
    let b = Fixture { ctx: b_ctx, ..b };
    let code = apply(&a.ctx, "devices.pair_code", json!({}));
    apply(&b.ctx, "devices.pair_with", json!({"code": code["ticket"]}));
    let to_b = b_node.id().to_string();
    // Drops send from library sources.
    a.ctx
        .lib
        .add_source(keel_core::SourceDef {
            label: "Files".into(),
            root: keel_vfs::VPath::local(a.files.path()),
            kind: keel_core::SourceKind::Folder,
            include_hidden: true,
            ignore: Vec::new(),
            poll_secs: None,
            hash_shares: false,
        })
        .unwrap();
    // A link in a sent folder to the daemon token is never followed.
    let token = a.cfg.path().join("daemon.token");
    std::fs::write(&token, b"secret").unwrap();
    let link = a.files.path().join("docs").join("token.txt");
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&token, &link).is_ok();
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(&token, &link).is_ok();
    if !linked {
        eprintln!("no symlink permission: the link case is not exercised");
    }

    let docs = s(&a.files.path().join("docs"));
    let params = json!({"peer": to_b, "paths": [docs]});
    let preview: PlanPreview =
        serde_json::from_value(call(&a.ctx, "spacedrop.send", params.clone()).unwrap()).unwrap();
    assert!(
        preview.summary.contains("2 file(s), 19 bytes"),
        "{preview:?}"
    );
    let mut listed: Vec<_> = preview
        .changes
        .iter()
        .map(|c| (c.path.clone().unwrap(), c.bytes.unwrap()))
        .collect();
    listed.sort();
    assert_eq!(
        listed,
        vec![
            ("docs/invoice-2026.pdf".to_owned(), 9),
            ("docs/notes.txt".to_owned(), 10)
        ]
    );
    assert!(
        a.ctx.lib.jobs().list().unwrap().is_empty(),
        "a preview sends nothing"
    );
    // Never the configuration folder (the daemon token), nor a folder holding it.
    let cfg = json!({"peer": to_b, "paths": [s(a.cfg.path())]});
    assert!(call(&a.ctx, "spacedrop.send", cfg).is_err());
    let holder = a.cfg.path().parent().unwrap();
    let held = json!({"peer": to_b, "paths": [s(holder)]});
    assert!(call(&a.ctx, "spacedrop.send", held).is_err());
    let stranger = json!({"peer": "a".repeat(52), "paths": [docs]});
    assert!(call(&a.ctx, "spacedrop.send", stranger).is_err());
    let send = |path: &std::path::Path| {
        call(
            &a.ctx,
            "spacedrop.send",
            json!({"peer": to_b, "paths": [s(path)]}),
        )
    };
    if linked {
        let e = send(&link).unwrap_err();
        assert!(e.message.contains("configuration"), "{e}");
    }
    // Only library sources, the inbox and opened shares; never the data folder.
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(elsewhere.path().join("x.txt"), b"x").unwrap();
    let e = send(elsewhere.path()).unwrap_err();
    assert!(e.message.contains("library source"), "{e}");
    let data = a.ctx.data_dir.clone().unwrap();
    std::fs::write(data.join("secret.bin"), b"k").unwrap();
    let e = send(&data.join("secret.bin")).unwrap_err();
    assert!(e.message.contains("data folder"), "{e}");
    assert!(send(&data).is_err());
    let share = data.join("shares").join("0123456789abcdef01234567");
    std::fs::create_dir_all(&share).unwrap();
    std::fs::write(share.join("photo.jpg"), b"jpg").unwrap();
    let e = send(&share.join("photo.jpg")).unwrap_err();
    assert!(e.message.contains("data folder"), "not opened yet: {e}");
    std::fs::write(share.join(crate::SHARE_CLAIMED), b"").unwrap();
    let opened = send(&share.join("photo.jpg")).unwrap();
    assert!(
        opened["summary"]
            .as_str()
            .unwrap()
            .contains("1 file(s), 3 bytes"),
        "{opened}"
    );
    // Execute sends what the preview listed, or refuses with a fresh preview.
    let extra = a.files.path().join("extra");
    std::fs::create_dir(&extra).unwrap();
    std::fs::write(extra.join("a.txt"), b"a").unwrap();
    let before: PlanPreview = serde_json::from_value(send(&extra).unwrap()).unwrap();
    std::fs::write(extra.join("b.txt"), b"b").unwrap();
    let e = call(
        &a.ctx,
        "execute",
        json!({"plan_id": before.plan_id, "input_hash": before.input_hash}),
    )
    .unwrap_err();
    assert_eq!(e.code, ApiError::PLAN_CHANGED, "{e}");
    let fresh = e.data.unwrap();
    assert!(
        fresh["summary"].as_str().unwrap().contains("2 file(s)"),
        "{fresh}"
    );
    assert!(a.ctx.lib.jobs().list().unwrap().is_empty(), "nothing sent");

    let done = call(
        &a.ctx,
        "execute",
        json!({"plan_id": preview.plan_id, "input_hash": preview.input_hash}),
    )
    .unwrap();
    let job = done["job"].as_i64().expect("execute names the job");
    assert_eq!(done["result"]["job"], job);

    // The offer waits for an answer on b.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let offer = loop {
        let inbox = call(&b.ctx, "spacedrop.inbox", Value::Null).unwrap();
        if let Some(o) = inbox["pending"].as_array().and_then(|p| p.first()) {
            break o.clone();
        }
        assert!(std::time::Instant::now() < deadline, "no offer arrived");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(offer["peer"], a_node.id().to_string());
    assert_eq!(
        (offer["files"].as_u64(), offer["bytes"].as_u64()),
        (Some(2), Some(19))
    );
    let answer = json!({"peer": offer["peer"], "id": offer["id"], "accept": true});
    let p = call(&b.ctx, "spacedrop.answer", answer.clone()).unwrap();
    assert!(
        p["summary"]
            .as_str()
            .unwrap()
            .starts_with("Accept 2 file(s)"),
        "{p}"
    );
    assert_eq!(
        apply(&b.ctx, "spacedrop.answer", answer.clone())["ok"],
        true
    );
    assert!(
        call(&b.ctx, "spacedrop.answer", answer).is_err(),
        "answered once"
    );

    let info = a.ctx.lib.jobs().wait(job).unwrap();
    assert_eq!(info.status, keel_core::JobStatus::Done, "{}", info.log);
    let got = inbox.path().join("in").join("docs");
    assert_eq!(std::fs::read(got.join("notes.txt")).unwrap(), b"some notes");
    let listing = call(&b.ctx, "spacedrop.inbox", Value::Null).unwrap();
    assert_eq!(listing["entries"][0]["name"], "docs", "{listing}");
    assert_eq!(listing["entries"][0]["is_dir"], true);
    assert!(listing["pending"].as_array().unwrap().is_empty());
    // a's host takes no drops.
    assert!(call(&a.ctx, "spacedrop.inbox", Value::Null).is_err());

    // Device settings change live on b: its label, a new inbox, and a's drops accepted
    // without asking; relays only when it starts again.
    let a_id = a_node.id().to_string();
    let later = inbox.path().join("later");
    let set = json!({"label": "Den", "inbox": s(&later), "auto_accept": [a_id], "relay": false});
    let before = b_node.label();
    let preview = call(&b.ctx, "devices.settings_set", set.clone()).unwrap();
    assert_eq!(preview["warnings"][0]["kind"], "restart", "{preview}");
    assert_eq!(b_node.label(), before, "a preview changes nothing");
    let done = apply(&b.ctx, "devices.settings_set", set);
    assert_eq!(done["restart"], true);
    let got = call(&b.ctx, "devices.settings", Value::Null).unwrap();
    assert_eq!(
        got,
        json!({"label": "Den", "inbox": s(&later), "auto_accept": [a_id], "relay": true, "sync": []})
    );
    assert_eq!(b_node.label(), "Den");
    // Never Keel's own folders; device ids must parse; nothing changes then.
    let cfg_inbox = json!({"inbox": s(b.cfg.path())});
    let e = call(&b.ctx, "devices.settings_set", cfg_inbox).unwrap_err();
    assert_eq!(e.code, ApiError::INVALID_PARAMS, "{e}");
    // Review 44 minor 5: nor a folder holding them (drops would land among them).
    let above = json!({"inbox": s(b.cfg.path().parent().unwrap())});
    let e = call(&b.ctx, "devices.settings_set", above).unwrap_err();
    assert_eq!(e.code, ApiError::INVALID_PARAMS, "{e}");
    // Minor 6: a label the node would refuse fails at the preview.
    let e = call(&b.ctx, "devices.settings_set", json!({"label": "a\nb"})).unwrap_err();
    assert_eq!(e.code, ApiError::INVALID_PARAMS, "{e}");
    let bad = json!({"label": "x", "auto_accept": ["nope"]});
    assert!(call(&b.ctx, "devices.settings_set", bad).is_err());
    assert_eq!(b_node.label(), "Den");
    let sent = send(&a.files.path().join("docs").join("notes.txt")).unwrap();
    let done = call(
        &a.ctx,
        "execute",
        json!({"plan_id": sent["plan_id"], "input_hash": sent["input_hash"]}),
    )
    .unwrap();
    let job = done["job"].as_i64().unwrap();
    let info = a.ctx.lib.jobs().wait(job).unwrap();
    assert_eq!(info.status, keel_core::JobStatus::Done, "{}", info.log);
    assert_eq!(
        std::fs::read(later.join("notes.txt")).unwrap(),
        b"some notes"
    );
    let listing = call(&b.ctx, "spacedrop.inbox", Value::Null).unwrap();
    assert_eq!(listing["dir"], s(&later));
    rt.block_on(async {
        a_node.close().await;
        b_node.close().await;
    });
}

/// What the desktop app reads and changes through the daemon: stats, tagged paths,
/// views, recents, copies per folder, protection, the drive inventory, and the background
/// jobs it drives (index with adopt, hashing, integrity, media).
#[test]
fn the_apps_library_operations() {
    let f = fixture(None);
    let id = add_and_index(&f);
    let stats = call(&f.ctx, "library.stats", Value::Null).unwrap();
    assert_eq!(stats["sources"], 1, "{stats}");
    assert_eq!(stats["files"], 2, "{stats}");
    let one = &stats["per_source"][0];
    assert_eq!(one["id"], id.as_str(), "{stats}");
    assert_eq!(one["files"], 2, "{stats}");
    assert_eq!(one["offline"], false, "{stats}");
    assert!(one["last_walk"].as_i64().is_some_and(|t| t > 0), "{stats}");
    assert!(
        one["folders"].is_u64() && one["hashed_files"].is_u64(),
        "{stats}"
    );

    let invoice = s(&f.files.path().join("docs").join("invoice-2026.pdf"));
    let preview = call(
        &f.ctx,
        "tags.add",
        json!({"tag": "receipts", "paths": [invoice], "color": "#e5484d"}),
    )
    .unwrap();
    assert_eq!(preview["warnings"][0]["kind"], "creates_tag", "{preview}");
    apply(
        &f.ctx,
        "tags.add",
        json!({"tag": "receipts", "paths": [invoice], "color": "#e5484d"}),
    );
    apply(
        &f.ctx,
        "favorites.set",
        json!({"paths": [invoice], "on": true}),
    );
    let tags = call(&f.ctx, "tags.list", Value::Null).unwrap();
    let receipts = tags
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "receipts")
        .unwrap()
        .clone();
    assert_eq!(receipts["color"], "#e5484d");
    let tagged = call(&f.ctx, "tags.tagged", Value::Null).unwrap();
    assert_eq!(tagged.as_array().unwrap().len(), 1, "{tagged}");
    assert_eq!(tagged[0]["path"], invoice);
    let ids: Vec<i64> = serde_json::from_value(tagged[0]["tags"].clone()).unwrap();
    assert!(ids.contains(&receipts["id"].as_i64().unwrap()), "{ids:?}");
    assert!(ids.contains(&keel_core::FAVORITES), "{ids:?}");
    assert_eq!(call(&f.ctx, "views.list", Value::Null).unwrap(), json!([]));

    // Opening a file puts it on top of recents, at once.
    call(&f.ctx, "recents.note", json!({ "path": invoice })).unwrap();
    let recents = call(&f.ctx, "recents", Value::Null).unwrap();
    assert_eq!(recents[0]["path"], invoice, "{recents}");

    let folder = call(
        &f.ctx,
        "redundancy.folder",
        json!({"path": s(&f.files.path().join("docs"))}),
    )
    .unwrap();
    assert_eq!(folder.as_array().unwrap().len(), 2, "{folder}");
    assert_eq!(folder[0]["copies"]["copies"], 1, "{folder}");
    assert_eq!(
        folder[0]["copies"]["locations"][0]["state"], "online",
        "{folder}"
    );

    let p = call(&f.ctx, "protection.summary", Value::Null).unwrap();
    assert!(p["unchecked"].is_u64(), "{p}");
    let volumes = call(&f.ctx, "volumes.list", Value::Null).unwrap();
    let volume = volumes[0]["id"].as_str().unwrap().to_owned();
    assert!(
        call(&f.ctx, "volumes.set", json!({ "volume": volume })).is_err(),
        "nothing to set"
    );
    let set = json!({"volume": volume, "backup": true, "failure_domain": "disk:test", "state": "archived"});
    call(&f.ctx, "volumes.set", set.clone()).unwrap();
    assert_eq!(
        call(&f.ctx, "volumes.list", Value::Null).unwrap()[0]["backup"],
        false,
        "a preview changes nothing"
    );
    let v = apply(&f.ctx, "volumes.set", set);
    assert_eq!(
        (
            &v["backup"],
            &v["failure_domain"],
            &v["state"],
            &v["domain_set"]
        ),
        (
            &json!(true),
            &json!("disk:test"),
            &json!("archived"),
            &json!(true)
        ),
        "{v}"
    );
    let v = apply(
        &f.ctx,
        "volumes.set",
        json!({"volume": volume, "failure_domain": "", "state": "online"}),
    );
    assert_eq!(v["domain_set"], false, "{v}");

    let wait = |job: &Value| {
        let info = f.ctx.lib.jobs().wait(job.as_i64().unwrap()).unwrap();
        assert_eq!(info.status, keel_core::JobStatus::Done, "{}", info.log);
    };
    // Adopting a root re-indexes whatever is there now.
    let preview = call(&f.ctx, "sources.index", json!({"id": id, "adopt": true})).unwrap();
    assert_eq!(preview["warnings"][0]["kind"], "adopts_root", "{preview}");
    wait(&apply(&f.ctx, "sources.index", json!({"id": id, "adopt": true}))["job"]);
    wait(
        &apply(
            &f.ctx,
            "hashing.set",
            json!({"on": true, "idle_only": false}),
        )["job"],
    );
    assert_eq!(
        apply(&f.ctx, "hashing.set", json!({"on": false}))["job"],
        Value::Null
    );
    // The remote policy: stated in the preview, kept with the library, omitted = unchanged.
    let remote = json!({"on": false, "remote": false, "cloud": true, "max_remote_bytes": 4096});
    let preview = call(&f.ctx, "hashing.set", remote.clone()).unwrap();
    assert!(
        preview["summary"].as_str().unwrap().contains(
            "SFTP sources not hashed, cloud sources hashed (downloads can cost egress fees), remote files over 4096 bytes skipped"
        ),
        "{preview}"
    );
    apply(&f.ctx, "hashing.set", remote);
    let kept = keel_core::RemoteHashSettings {
        hash_remote: false,
        hash_cloud: true,
        remote_hash_max_bytes: 4096,
    };
    assert_eq!(f.ctx.lib.remote_hash_settings(), kept);
    apply(&f.ctx, "hashing.set", json!({"on": false}));
    assert_eq!(f.ctx.lib.remote_hash_settings(), kept);
    wait(&apply(&f.ctx, "integrity.check", json!({"sample_pct": 100.0}))["job"]);
    // The first scheduled call only starts the clock.
    let due = json!({"sample_pct": 1.0, "due_days": 7});
    assert_eq!(apply(&f.ctx, "integrity.check", due)["job"], Value::Null);
    assert!(call(&f.ctx, "integrity.check", json!({"sample_pct": 0.0})).is_err());
    assert!(call(
        &f.ctx,
        "integrity.check",
        json!({"source": id, "due_days": 1})
    )
    .is_err());
    wait(&apply(&f.ctx, "media.index", json!({ "id": id }))["job"]);
    let missing = json!({"id": "0123456789abcdef0123456789abcdef"});
    assert_eq!(
        call(&f.ctx, "media.index", missing).unwrap_err().code,
        ApiError::NOT_FOUND
    );
}

/// `activity.note` acts at once: the library's background jobs see the user as busy.
#[test]
fn activity_note_marks_the_user_busy() {
    let f = fixture(None);
    assert!(!f.ctx.lib.user_active());
    let done = call(&f.ctx, "activity.note", Value::Null).unwrap();
    assert_eq!(done["ok"], true);
    assert!(f.ctx.lib.user_active());
}

/// Review 46 minor 5: the `library.stats` example in docs/api.md is a `LibraryStats` and
/// valid under the operation's result schema, and so is a real answer.
#[test]
fn library_stats_example_and_answer_match_the_result_schema() {
    let doc =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/api.md")).unwrap();
    let after = &doc[doc.find("`library.stats` answers").expect("the example")..];
    let start = after.find("```json").unwrap() + "```json".len();
    let end = start + after[start..].find("```").unwrap();
    let example: Value = serde_json::from_str(&after[start..end]).unwrap();
    let typed: LibraryStats = serde_json::from_value(example.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&typed).unwrap(),
        example,
        "every field known"
    );
    let op = OPS.iter().find(|o| o.name == "library.stats").unwrap();
    let validator = jsonschema::validator_for(&(op.result)()).unwrap();
    let errors =
        |v: &Value| -> Vec<String> { validator.iter_errors(v).map(|e| e.to_string()).collect() };
    assert!(errors(&example).is_empty(), "{:?}", errors(&example));
    let f = fixture(None);
    add_and_index(&f);
    let answer = call(&f.ctx, "library.stats", Value::Null).unwrap();
    assert!(errors(&answer).is_empty(), "{:?}", errors(&answer));
    assert_eq!(
        answer["hashing"]["max_remote_bytes"],
        1u64 << 30,
        "{answer}"
    );
}

/// Task 46: folder sizes from the index, by real and by library path; read-only; a path in
/// no source is NOT_FOUND.
/// Ctrl+P's file finder, attached to the daemon: the indexed files' full paths, capped.
#[test]
fn files_names_lists_indexed_files_up_to_max() {
    let f = fixture(None);
    add_and_index(&f);
    let all = call(&f.ctx, "files.names", json!({})).unwrap();
    let mut paths: Vec<String> = serde_json::from_value(all["paths"].clone()).unwrap();
    paths.sort();
    let docs = f.files.path().join("docs");
    assert_eq!(
        paths,
        [
            s(&docs.join("invoice-2026.pdf")),
            s(&docs.join("notes.txt"))
        ],
        "files only: {all}"
    );
    assert_eq!(all["truncated"], false);
    let one = call(&f.ctx, "files.names", json!({"max": 1})).unwrap();
    assert_eq!(one["paths"].as_array().unwrap().len(), 1, "{one}");
    assert_eq!(one["truncated"], true, "{one}");
    assert!(call(&f.ctx, "files.names", json!({"limit": 1})).is_err());
}

#[test]
fn library_folder_sizes_reads_the_index() {
    let f = fixture(None);
    let id = add_and_index(&f);
    let root = call(
        &f.ctx,
        "library.folder_sizes",
        json!({"path": s(f.files.path())}),
    )
    .unwrap();
    // "pdf bytes" + "some notes"
    assert_eq!(
        (root["bytes"].as_u64(), root["files"].as_u64()),
        (Some(19), Some(2)),
        "{root}"
    );
    assert_eq!(root["children"][0]["name"], "docs", "{root}");
    assert_eq!(root["children"][0]["is_dir"], true, "{root}");
    assert_eq!(root["children"][0]["bytes"], 19, "{root}");
    let docs = call(
        &f.ctx,
        "library.folder_sizes",
        json!({"path": format!("library://{id}/docs")}),
    )
    .unwrap();
    let names: Vec<_> = docs["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].clone())
        .collect();
    assert_eq!(
        names,
        ["notes.txt", "invoice-2026.pdf"],
        "biggest first: {docs}"
    );
    assert_eq!(docs["children"][0]["files"], 1, "{docs}");
    assert_eq!(docs["truncated"], false, "{docs}");
    // A cap keeps the biggest and says so; `dirs_only` leaves the files out.
    let one = call(
        &f.ctx,
        "library.folder_sizes",
        json!({"path": format!("library://{id}/docs"), "max": 1}),
    )
    .unwrap();
    assert_eq!(one["children"].as_array().unwrap().len(), 1, "{one}");
    assert_eq!(one["children"][0]["name"], "notes.txt", "{one}");
    assert_eq!(one["truncated"], true, "{one}");
    let dirs = call(
        &f.ctx,
        "library.folder_sizes",
        json!({"path": format!("library://{id}/docs"), "dirs_only": true}),
    )
    .unwrap();
    assert_eq!(dirs["children"], json!([]), "{dirs}");
    assert_eq!(dirs["bytes"], docs["bytes"], "{dirs}");
    let op = OPS
        .iter()
        .find(|o| o.name == "library.folder_sizes")
        .unwrap();
    assert!(!op.mutating);
    let validator = jsonschema::validator_for(&(op.result)()).unwrap();
    assert!(validator.is_valid(&root), "{root}");
    let outside = tempfile::tempdir().unwrap();
    let err = call(
        &f.ctx,
        "library.folder_sizes",
        json!({"path": s(outside.path())}),
    )
    .unwrap_err();
    assert_eq!(err.code, ApiError::NOT_FOUND, "{err:?}");
}

/// Task 59: `library.inspect` on a hashed file with a copy in a second source (tagged), a
/// renamed file (its history), a folder (totals) and a path outside the library.
#[test]
fn library_inspect_shows_copies_history_tags_and_totals() {
    let f = fixture(None);
    let id = add_and_index(&f);
    let second = tempfile::tempdir().unwrap();
    std::fs::write(second.path().join("invoice copy.pdf"), b"pdf bytes").unwrap();
    let added = apply(&f.ctx, "sources.add", json!({"root": s(second.path())}));
    let wait = |job: &Value| {
        let info = f.ctx.lib.jobs().wait(job.as_i64().unwrap()).unwrap();
        assert_eq!(info.status, keel_core::JobStatus::Done, "{}", info.log);
    };
    wait(&apply(&f.ctx, "sources.index", json!({"id": added["id"]}))["job"]);
    let notes = format!("library://{id}/docs/notes.txt");
    let rename = json!({"op": "rename", "paths": [notes], "new_name": "notes-2026.txt"});
    let preview: PlanPreview =
        serde_json::from_value(call(&f.ctx, "plan", rename).unwrap()).unwrap();
    let ids = json!({"plan_id": preview.plan_id, "input_hash": preview.input_hash});
    wait(&call(&f.ctx, "execute", ids).unwrap()["job"]);
    wait(
        &apply(
            &f.ctx,
            "hashing.set",
            json!({"on": true, "idle_only": false}),
        )["job"],
    );
    let pdf = s(&f.files.path().join("docs").join("invoice-2026.pdf"));
    apply(
        &f.ctx,
        "tags.add",
        json!({"tag": "receipts", "paths": [pdf]}),
    );
    let op = OPS.iter().find(|o| o.name == "library.inspect").unwrap();
    assert!(!op.mutating);
    let schema = jsonschema::validator_for(&(op.result)()).unwrap();
    let inspect = |path: &str| {
        let v = call(&f.ctx, "library.inspect", json!({ "path": path })).unwrap();
        assert!(schema.is_valid(&v), "{v}");
        serde_json::from_value::<Inspection>(v).unwrap()
    };

    let i = inspect(&pdf);
    assert_eq!(
        (i.entry.name.as_str(), i.entry.size),
        ("invoice-2026.pdf", 9)
    );
    let lib = i.library.expect("indexed");
    assert_eq!(lib.source, id);
    assert_eq!(lib.content_id.as_ref().map(String::len), Some(64));
    assert!(
        lib.first_seen.is_some() && lib.last_hashed.is_some(),
        "{lib:?}"
    );
    let copies = lib.copies.expect("a file");
    assert_eq!(copies.copies, 2);
    let other = s(&second.path().join("invoice copy.pdf"));
    let paths: Vec<_> = copies.locations.iter().map(|l| l.path.clone()).collect();
    assert_eq!(paths, std::slice::from_ref(&other), "only the other copy");
    assert_eq!(
        lib.tags.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        ["receipts"]
    );
    assert!(!lib.favorite);
    assert_eq!(lib.history.len(), 1, "{:?}", lib.history);
    assert_eq!(
        (lib.history[0].kind.as_str(), lib.history[0].tag.as_deref()),
        ("tag", Some("receipts"))
    );
    // The copy knows the first as its other copy.
    let copy = inspect(&other).library.unwrap().copies.unwrap();
    assert_eq!(copy.locations[0].path, pdf);

    let renamed = inspect(&s(&f.files.path().join("docs").join("notes-2026.txt")));
    let h = &renamed.library.unwrap().history;
    assert_eq!(h.len(), 1, "{h:?}");
    assert_eq!(h[0].kind, "rename");
    assert_eq!(
        h[0].from.as_deref(),
        Some(&*s(&f.files.path().join("docs").join("notes.txt")))
    );

    // A folder by its library path: totals, no copies.
    let docs = inspect(&format!("library://{id}/docs")).library.unwrap();
    assert!(docs.copies.is_none() && docs.content_id.is_none());
    let t = docs.folder.expect("a folder");
    assert_eq!((t.bytes, t.files, t.folders), (Some(19), Some(2), 0));
    assert_eq!(t.biggest.unwrap().name, "notes-2026.txt");
    let root = inspect(&s(f.files.path())).library.unwrap().folder.unwrap();
    assert_eq!((root.files, root.folders), (Some(2), 1));

    // Outside the library: the entry, nothing else.
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("x.txt"), b"x").unwrap();
    let i = inspect(&s(&outside.path().join("x.txt")));
    assert_eq!((i.entry.size, i.library), (1, None));
    let err = call(
        &f.ctx,
        "library.inspect",
        json!({"path": s(&outside.path().join("missing.txt"))}),
    )
    .unwrap_err();
    assert_eq!(err.code, ApiError::NOT_FOUND, "{err:?}");
}

/// Task 53: `library.backup` writes a new `.keelbackup` (never over a file), and
/// `library.restore` previews what the file holds, refuses an existing library and restores
/// under another name.
#[test]
fn backup_and_restore_round_trip_through_the_api() {
    let f = fixture(None);
    let id = add_and_index(&f);
    let pdf = s(&f.files.path().join("docs/invoice-2026.pdf"));
    apply(
        &f.ctx,
        "tags.add",
        json!({"tag": "receipts", "paths": [pdf]}),
    );
    let out = tempfile::tempdir().unwrap();
    // Local absolute paths only, ending in .keelbackup.
    for bad in ["relative.keelbackup", "sftp://host/x.keelbackup"] {
        let e = call(&f.ctx, "library.backup", json!({ "file": bad })).unwrap_err();
        assert_eq!(e.code, ApiError::INVALID_PARAMS, "{bad}: {}", e.message);
    }
    let txt = s(&out.path().join("b.txt"));
    assert!(call(&f.ctx, "library.backup", json!({ "file": txt })).is_err());
    // A folder gets a dated name.
    let preview = call(&f.ctx, "library.backup", json!({"file": s(out.path())})).unwrap();
    assert!(
        preview["summary"]
            .as_str()
            .unwrap()
            .contains("Back up library test"),
        "{preview}"
    );
    assert_eq!(
        std::fs::read_dir(out.path()).unwrap().count(),
        0,
        "a preview writes nothing"
    );
    // The daemon and its clients wait hours, not two minutes, for a backup's `execute`.
    let long = crate::client::LONG_REQUEST_WAIT;
    let plan = json!({"plan_id": preview["plan_id"]});
    assert_eq!(crate::request_wait(&f.ctx, "execute", &plan), long);
    assert_eq!(
        crate::request_wait(&f.ctx, "library.restore", &Value::Null),
        long
    );
    let other = call(
        &f.ctx,
        "tags.add",
        json!({"tag": "x", "paths": [s(&f.files.path().join("docs/notes.txt"))]}),
    )
    .unwrap();
    let other = json!({"plan_id": other["plan_id"]});
    assert_eq!(
        crate::request_wait(&f.ctx, "execute", &other),
        crate::client::REQUEST_WAIT
    );
    let done = apply(&f.ctx, "library.backup", json!({"file": s(out.path())}));
    let file = done["file"].as_str().unwrap().to_owned();
    assert!(
        file.ends_with(".keelbackup") && file.contains("test-"),
        "{file}"
    );
    assert_eq!(done["sources"], 1);
    assert!(done["bytes"].as_u64().unwrap() > 0);
    assert!(call(&f.ctx, "library.stats", Value::Null).unwrap()["last_backup"].is_i64());
    // Never over an existing file.
    let e = call(&f.ctx, "library.backup", json!({ "file": file })).unwrap_err();
    assert!(e.message.contains("exists"), "{}", e.message);

    // The open library's name is refused at the preview.
    let e = call(&f.ctx, "library.restore", json!({ "file": file })).unwrap_err();
    assert!(
        e.message.contains("a library named test exists"),
        "{}",
        e.message
    );
    let preview = call(
        &f.ctx,
        "library.restore",
        json!({"file": file, "name": "restored"}),
    )
    .unwrap();
    assert_eq!(preview["changes"][0]["action"], "library.restore");
    assert_eq!(preview["changes"][1]["action"], "source.restore");
    assert!(
        preview["changes"][1]["detail"]
            .as_str()
            .unwrap()
            .contains("walked again"),
        "{preview}"
    );
    let restored = apply(
        &f.ctx,
        "library.restore",
        json!({"file": file, "name": "restored"}),
    );
    assert_eq!(restored["name"], "restored");
    assert_eq!(restored["sources"][0]["id"], id);
    assert_eq!(restored["sources"][0]["online"], true);
    let back = keel_core::Library::open(f._data.path(), "restored").unwrap();
    let tags: Vec<String> = back.tags().unwrap().into_iter().map(|t| t.name).collect();
    assert_eq!(tags, ["receipts"]);
    assert_eq!(back.stats().files, f.ctx.lib.stats().files);
    // Not a backup: a typed message, not a crash.
    let junk = out.path().join("junk.keelbackup");
    std::fs::write(&junk, b"junk").unwrap();
    let e = call(&f.ctx, "library.restore", json!({"file": s(&junk)})).unwrap_err();
    assert!(e.message.starts_with("not a Keel backup"), "{}", e.message);
}

/// Task 58: the agent skill and its installable copy are the same file, it has the SKILL.md
/// front matter, and every operation it names in backticks exists.
#[test]
fn agent_skill_copy_matches_and_names_real_operations() {
    let doc = include_str!("../../../docs/agent-skill.md");
    let copy = include_str!("../../../skills/keel/SKILL.md");
    assert!(
        doc == copy,
        "skills/keel/SKILL.md differs from docs/agent-skill.md: copy it again"
    );
    let front = doc.strip_prefix("---\n").expect("front matter");
    let (front, _) = front.split_once("\n---\n").expect("front matter end");
    assert!(front.starts_with("name: keel\n"), "{front}");
    assert!(front.contains("\ndescription: "), "{front}");
    let named: Vec<&str> = (doc.split('`').skip(1).step_by(2))
        .filter(|t| t.contains('.') && !t.starts_with('.') && !t.ends_with('.'))
        .filter(|t| {
            t.chars()
                .all(|c| c.is_ascii_lowercase() || c == '_' || c == '.')
        })
        .collect();
    assert!(named.contains(&"tags.set") && named.contains(&"jobs.info"));
    // The operations that act without a preview are all listed, to ask the user first.
    let protocol = doc.split("## The two-step protocol").nth(1).unwrap();
    let listed = protocol.split("\n1. **Preview.**").next().unwrap();
    for name in crate::DIRECT.iter().filter(|n| **n != "execute") {
        assert!(
            listed.contains(&format!("`{name}`")),
            "the skill does not list {name} as acting without a preview"
        );
    }
    assert!(doc.contains("`deletes_permanent`") && doc.contains("| -32007 |"));
    for name in named {
        assert!(
            find(name).is_some(),
            "the skill names {name}, no such operation"
        );
    }
}

/// Runs a previewed file plan to its end.
fn run_file_plan(f: &Fixture, preview: &Value) -> keel_core::JobInfo {
    let done = call(
        &f.ctx,
        "execute",
        json!({"plan_id": preview["plan_id"], "input_hash": preview["input_hash"]}),
    )
    .unwrap();
    let job = done["job"].as_i64().unwrap();
    f.ctx.lib.jobs().wait(job).unwrap()
}

/// Task 60: the folder operations preview every move (and folder made or removed) and
/// run through `execute` like any file plan; a changed folder refuses the preview.
#[test]
fn folder_operations_preview_then_execute() {
    let f = fixture(None);
    let root = f.files.path();
    std::fs::create_dir_all(root.join("docs/old")).unwrap();
    std::fs::write(root.join("docs/old/a.txt"), b"a").unwrap();
    let docs = s(&root.join("docs"));
    let flat = call(&f.ctx, "plan.flatten", json!({"path": docs})).unwrap();
    assert_eq!(flat["operation"], "plan");
    let actions: Vec<&str> = (flat["changes"].as_array().unwrap().iter())
        .map(|c| c["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions, ["move", "remove_folder"]);
    assert!(
        flat["summary"].as_str().unwrap().starts_with("Flatten"),
        "{flat}"
    );
    // A file appears: the confirmed preview no longer matches.
    std::fs::write(root.join("docs/old/b.txt"), b"b").unwrap();
    let err = call(
        &f.ctx,
        "execute",
        json!({"plan_id": flat["plan_id"], "input_hash": flat["input_hash"]}),
    )
    .unwrap_err();
    assert_eq!(err.code, ApiError::PLAN_CHANGED);
    let fresh = err.data.unwrap();
    assert_eq!(fresh["changes"].as_array().unwrap().len(), 3);
    let info = run_file_plan(&f, &fresh);
    assert_eq!(info.status, keel_core::JobStatus::Done, "{}", info.log);
    assert!(root.join("docs/b.txt").is_file() && !root.join("docs/old").exists());

    let org = call(&f.ctx, "plan.organize", json!({"path": docs, "by": "type"})).unwrap();
    assert!(org["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["action"] == "make_folder"));
    let info = run_file_plan(&f, &org);
    assert_eq!(info.status, keel_core::JobStatus::Done, "{}", info.log);
    assert!(root.join("docs/Documents/invoice-2026.pdf").is_file());

    std::fs::create_dir_all(root.join("more")).unwrap();
    std::fs::write(root.join("more/c.txt"), b"c").unwrap();
    let merge = call(
        &f.ctx,
        "plan.merge",
        json!({"paths": [s(&root.join("more")), docs], "into": docs, "on_conflict": "rename_new"}),
    )
    .unwrap();
    let info = run_file_plan(&f, &merge);
    assert_eq!(info.status, keel_core::JobStatus::Done, "{}", info.log);
    assert!(root.join("docs/c.txt").is_file() && !root.join("more").exists());

    let pdf = s(&root.join("docs/Documents/invoice-2026.pdf"));
    let attrs = call(
        &f.ctx,
        "plan.set_attributes",
        json!({"paths": [pdf], "readonly": true}),
    )
    .unwrap();
    assert!(attrs["summary"].as_str().unwrap().contains("read-only on"));
    run_file_plan(&f, &attrs);
    let local = root.join("docs/Documents/invoice-2026.pdf");
    assert_eq!(
        keel_vfs::fsattr::attributes(&local).unwrap().readonly,
        Some(true)
    );
    let bad = call(
        &f.ctx,
        "plan.set_attributes",
        json!({"paths": [pdf], "mode": "9x"}),
    );
    assert_eq!(bad.unwrap_err().code, ApiError::INVALID_PARAMS);
    let link = call(
        &f.ctx,
        "plan.link",
        json!({"target": pdf, "link": s(&root.join("invoice-link.pdf")), "kind": "hardlink"}),
    )
    .unwrap();
    run_file_plan(&f, &link);
    assert_eq!(
        std::fs::read(root.join("invoice-link.pdf")).unwrap(),
        b"pdf bytes"
    );
    keel_vfs::fsattr::set_attributes(
        &local,
        &keel_vfs::Attributes {
            readonly: Some(false),
            ..Default::default()
        },
    )
    .unwrap();
    // Nothing in Keel's configuration folder.
    let token = s(&f.cfg.path().join("token"));
    std::fs::write(f.cfg.path().join("token"), b"t").unwrap();
    let refused = call(
        &f.ctx,
        "plan.link",
        json!({"target": token, "link": s(&root.join("t")), "kind": "hardlink"}),
    );
    assert!(refused.is_err());
}

/// Task 85: `plan.folder_dates` previews each folder's new and old time and sets them.
#[test]
fn folder_dates_preview_then_execute() {
    let f = fixture(None);
    let docs = f.files.path().join("docs");
    let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_400_000_000);
    for name in ["invoice-2026.pdf", "notes.txt"] {
        let file = std::fs::File::options()
            .write(true)
            .open(docs.join(name))
            .unwrap();
        file.set_modified(old).unwrap();
    }
    let preview = call(&f.ctx, "plan.folder_dates", json!({"path": s(&docs)})).unwrap();
    let changes = preview["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "{preview}");
    assert_eq!(changes[0]["action"], "set_folder_date");
    let detail = changes[0]["detail"].as_str().unwrap();
    assert!(detail.starts_with("2014-05-13T16:53:20Z, was "), "{detail}");
    let summary = preview["summary"].as_str().unwrap();
    assert!(
        summary.starts_with("Set the dates of 1 folder in"),
        "{summary}"
    );
    let info = run_file_plan(&f, &preview);
    assert_eq!(info.status, keel_core::JobStatus::Done, "{}", info.log);
    assert_eq!(std::fs::metadata(&docs).unwrap().modified().unwrap(), old);
    let again = call(&f.ctx, "plan.folder_dates", json!({"path": s(&docs)}));
    assert_eq!(again.unwrap_err().code, ApiError::INVALID_PARAMS);
}

/// Task 62: words find the text inside documents (a hit with a snippet), `contents: false`
/// searches names only, `contents.set` turns a source off and back to its default, and
/// `library.stats` counts the text.
#[test]
fn search_finds_the_text_inside_documents() {
    let f = fixture(None);
    f.ctx.lib.set_pause_on_battery(false);
    f.ctx.lib.set_text_extractor(keel_preview::text_extractor());
    std::fs::write(
        f.files.path().join("docs/letter.md"),
        "# Keel\n\nThe harbour pilot boards at dawn.",
    )
    .unwrap();
    let id = add_and_index(&f);
    let documents = || -> u64 {
        let stats = call(&f.ctx, "library.stats", Value::Null).unwrap();
        stats["text"]["documents"].as_u64().unwrap()
    };
    let wait_for = |n: u64| {
        let start = std::time::Instant::now();
        while documents() != n {
            assert!(start.elapsed() < Duration::from_secs(60), "{}", documents());
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    // The walk started the text job: notes.txt and letter.md (no pdfium here: the PDF is
    // skipped).
    wait_for(2);
    let stats = call(&f.ctx, "library.stats", Value::Null).unwrap();
    assert_eq!(stats["text"]["on"], true, "{stats}");
    assert_eq!(stats["per_source"][0]["text"]["documents"], 2, "{stats}");

    let hits = call(&f.ctx, "search", json!({"query": "harbour"})).unwrap();
    assert_eq!(hits.as_array().unwrap().len(), 1, "{hits}");
    assert_eq!(hits[0]["name"], "letter.md");
    let text = hits[0]["snippet"]["text"].as_str().unwrap();
    let [a, b] = [0, 1].map(|i| hits[0]["snippet"]["marks"][0][i].as_u64().unwrap() as usize);
    assert_eq!(&text[a..b], "harbour", "{hits}");
    let names_only = call(
        &f.ctx,
        "search",
        json!({"query": "harbour", "contents": false}),
    )
    .unwrap();
    assert!(names_only.as_array().unwrap().is_empty(), "{names_only}");

    // Off for the source: previewed, then its text is gone.
    let off = json!({"on": false, "id": id});
    let preview = call(&f.ctx, "contents.set", off.clone()).unwrap();
    assert!(
        preview["summary"].as_str().unwrap().contains("deleted"),
        "{preview}"
    );
    assert_eq!(documents(), 2, "a preview changes nothing");
    apply(&f.ctx, "contents.set", off);
    assert_eq!(documents(), 0);
    let stats = call(&f.ctx, "library.stats", Value::Null).unwrap();
    assert_eq!(stats["per_source"][0]["text"]["on"], false, "{stats}");
    // Back to its default (a local folder: on), read again.
    apply(
        &f.ctx,
        "contents.set",
        json!({"on": false, "id": id, "default": true}),
    );
    wait_for(2);
    // The library's setting off.
    apply(&f.ctx, "contents.set", json!({"on": false}));
    let stats = call(&f.ctx, "library.stats", Value::Null).unwrap();
    assert_eq!(stats["text"]["on"], false, "{stats}");
    assert_eq!(documents(), 0);
}

/// Every file under `dir` that holds `needle`.
fn files_holding(dir: &std::path::Path, needle: &[u8]) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            found.extend(files_holding(&p, needle));
        } else if std::fs::read(&p).is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle)) {
            found.push(p);
        }
    }
    found
}

/// Review of 0.22.0, B1, B2, M1, M4: the crafted files of the review (20,000 nested Word
/// tables, an xlsb with a cell at XFD1048576, a docx whose zip understates a 65 MiB part,
/// an xlsx error cell holding a marker) read by the real text reader end failed (the xlsb
/// is read), the process lives on, no reader panics, and the marker is nowhere: not in the
/// job log, not in any file of the library (library.db, the stores), not in a backup.
#[test]
fn crafted_documents_fail_and_leave_no_trace() {
    const MARKER: &str = "SECRETMARKER salary 98765";
    let f = fixture(None);
    f.ctx.lib.set_pause_on_battery(false);
    f.ctx.lib.set_text_extractor(keel_preview::text_extractor());
    keel_preview::probes::review_files(&f.files.path().join("docs"), MARKER);
    add_and_index(&f);
    let text = || call(&f.ctx, "library.stats", Value::Null).unwrap()["text"].clone();
    let start = std::time::Instant::now();
    // notes.txt and the xlsb indexed; the PDF skipped (no pdfium here); three failed.
    while text()["failed"] != 3 || text()["documents"] != 2 {
        assert!(start.elapsed() < Duration::from_secs(120), "{}", text());
        std::thread::sleep(Duration::from_millis(50));
    }
    let jobs = f.ctx.lib.jobs().list().unwrap();
    let log: String = jobs.iter().map(|j| j.log.clone()).collect();
    for (file, why) in [
        ("deep.docx", "nests too deeply"),
        ("error.xlsx", "not a valid spreadsheet"),
        ("liar.docx", ""),
    ] {
        assert!(log.contains(&format!("{file}: {why}")), "{log}");
    }
    assert!(!log.contains("crashed") && !log.contains(MARKER), "{log}");
    let hits = call(&f.ctx, "search", json!({"query": "content:keel"})).unwrap();
    assert_eq!(hits[0]["name"], "sparse.xlsb", "{hits}");

    let out = tempfile::tempdir().unwrap();
    let done = apply(&f.ctx, "library.backup", json!({"file": s(out.path())}));
    let file = std::path::PathBuf::from(done["file"].as_str().unwrap());
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&file).unwrap()).unwrap();
    for i in 0..zip.len() {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut zip.by_index(i).unwrap(), &mut bytes).unwrap();
        let name = zip.by_index(i).unwrap().name().to_owned();
        assert!(
            !bytes.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()),
            "{name}"
        );
    }
    let data = f.ctx.data_dir.clone().unwrap();
    assert!(files_holding(&data, MARKER.as_bytes()).is_empty());
    assert!(files_holding(f.cfg.path(), MARKER.as_bytes()).is_empty());
}

/// Task 78: `system.search` runs the platform search (here a walk of the test folder) with
/// Everything's options, sorted and paged, and refuses a bad pattern as invalid params.
#[test]
fn system_search_runs_the_platform_search() {
    struct Walk(std::path::PathBuf);
    impl keel_search::Searcher for Walk {
        fn query(&self, q: &keel_search::Query) -> anyhow::Result<Vec<keel_search::Hit>> {
            Ok(keel_search::walk(&self.0, q))
        }
        fn available(&self) -> bool {
            true
        }
        fn name(&self) -> &'static str {
            "Walk"
        }
    }
    let f = fixture(None);
    f.ctx.system.set(Arc::new(Walk(f.files.path().to_owned())));
    let r: SystemSearchResult =
        serde_json::from_value(call(&f.ctx, "system.search", json!({"query": "INVOICE"})).unwrap())
            .unwrap();
    assert_eq!((r.backend.as_str(), r.total, r.capped), ("Walk", 1, false));
    let hit = &r.hits[0];
    assert_eq!(
        (hit.name.as_str(), hit.size, hit.is_dir),
        ("invoice-2026.pdf", 9, false)
    );
    assert!(hit.modified.is_some(), "size and date filled in");
    assert_eq!(hit.parent, s(&f.files.path().join("docs")));
    assert_eq!(
        hit.path,
        s(&f.files.path().join("docs").join("invoice-2026.pdf"))
    );
    // Everything: the folder, both files; biggest first, one per page.
    let r: SystemSearchResult = serde_json::from_value(
        call(
            &f.ctx,
            "system.search",
            json!({"query": "", "count": 1, "offset": 0, "sort": "size", "ascending": false}),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!((r.total, r.hits.len()), (3, 1));
    assert_eq!(r.hits[0].name, "notes.txt");
    let none = call(
        &f.ctx,
        "system.search",
        json!({"query": "INVOICE", "match_case": true}),
    )
    .unwrap();
    assert_eq!(none["total"], 0);
    let e = call(
        &f.ctx,
        "system.search",
        json!({"query": "(x", "regex": true}),
    )
    .unwrap_err();
    assert_eq!(e.code, ApiError::INVALID_PARAMS, "{e:?}");
    let e = call(&f.ctx, "system.search", json!({"query": "x", "count": -1})).unwrap_err();
    assert_eq!(e.code, ApiError::INVALID_PARAMS, "{e:?}");
}

/// Task 86: `du.scan` runs in the background and answers with an id; `du.result` waits for
/// it and sums it up; `du.export` returns every list and the report; the configuration
/// folder is never scanned; only the last `du::KEEP` scans are kept.
#[test]
fn disk_usage_scan_result_and_export() {
    let mut f = fixture(None);
    let root = f.files.path().to_owned();
    // A configuration folder inside the scanned folder is left out.
    let cfg = root.join("keel-config");
    std::fs::create_dir(&cfg).unwrap();
    std::fs::write(cfg.join("token"), b"secret").unwrap();
    f.ctx.config_dir = Some(cfg.clone());
    let ctx = &f.ctx;
    let done = |id: &Value| -> Value {
        for _ in 0..30 {
            let r = call(ctx, "du.result", json!({"id": id, "wait_ms": 10_000})).unwrap();
            if r["state"] != "running" {
                return r;
            }
        }
        panic!("the scan did not finish");
    };
    let started = call(ctx, "du.scan", json!({"paths": [s(&root)]})).unwrap();
    let id = started["id"].clone();
    assert_eq!(id.as_str().unwrap().len(), 16, "{started}");
    let r = done(&id);
    assert_eq!(r["state"], "done", "{r}");
    let sum = &r["summary"];
    assert_eq!(
        (sum["bytes"].as_u64(), sum["files"].as_u64()),
        (Some(19), Some(2)),
        "{r}"
    );
    assert_eq!(sum["roots"][0]["children"][0]["name"], "docs", "{r}");
    assert!(r["progress"]["files"].as_u64().unwrap() >= 2, "{r}");
    // Every list and the report.
    let export = |params: Value| call(ctx, "du.export", params).unwrap();
    let types = export(json!({"id": id, "list": "types", "format": "csv", "bom": true}));
    assert_eq!(types["format"], "csv");
    let text = types["content"].as_str().unwrap();
    assert!(text.starts_with("\u{feff}\"extension\""), "{text}");
    assert!(text.contains("\"pdf\",\"1\",\"9\""), "{text}");
    let tree = export(json!({"id": id, "list": "tree"}));
    assert_eq!(tree["format"], "json");
    let loaded = keel_vfs::du::load_from(tree["content"].as_str().unwrap().as_bytes()).unwrap();
    assert_eq!(loaded.node(keel_vfs::du::TOP).size, 19);
    assert!(loaded.nodes().all(|(_, n)| &*n.name != "token"));
    let largest = export(json!({"id": id, "list": "largest", "top": 1}));
    let largest: Value = serde_json::from_str(largest["content"].as_str().unwrap()).unwrap();
    assert_eq!(largest.as_array().unwrap().len(), 1);
    assert_eq!(largest[0]["name"], "notes.txt");
    let report = export(json!({"id": id, "list": "report", "format": "csv"}));
    assert_eq!(report["format"], "html");
    assert!(report["content"].as_str().unwrap().contains("<svg"));

    // Refusals.
    let err = |method: &str, params: Value| call(ctx, method, params).unwrap_err();
    assert_eq!(
        err("du.result", json!({"id": "ffffffffffffffff"})).code,
        ApiError::NOT_FOUND
    );
    assert_eq!(
        err("du.scan", json!({"paths": ["relative/folder"]})).code,
        ApiError::INVALID_PARAMS
    );
    assert_eq!(err("du.scan", json!({})).code, ApiError::INVALID_PARAMS);
    let bad_regex = json!({"paths": [s(&root)], "exclude_regex": "("});
    assert_eq!(err("du.scan", bad_regex).code, ApiError::INVALID_PARAMS);
    let e = err("du.scan", json!({"paths": [s(&cfg)]}));
    assert!(e.message.contains("configuration"), "{}", e.message);

    // The oldest finished scan makes room for a new one.
    let mut ids = vec![id];
    for _ in 0..crate::du::KEEP {
        let started = call(ctx, "du.scan", json!({"paths": [s(&root)], "min_size": 10})).unwrap();
        let r = done(&started["id"]);
        assert_eq!(r["summary"]["files"], 1, "min_size: {r}");
        assert!(r["summary"]["excluded"].as_u64().unwrap() >= 1, "{r}");
        ids.push(started["id"].clone());
    }
    assert_eq!(
        err("du.export", json!({"id": ids[0], "list": "types"})).code,
        ApiError::NOT_FOUND
    );
    assert_eq!(done(&ids[crate::du::KEEP])["state"], "done");
}

/// Task 76: views (bookmarks) are created, updated and deleted through the API, preview
/// first; refused input is a typed error at the preview; MCP lists the new tools.
#[test]
fn views_are_created_updated_and_deleted() {
    let f = fixture(None);
    let preview = call(
        &f.ctx,
        "views.create",
        json!({"name": "Invoices", "query": "invoice ext:pdf", "folder": "Work"}),
    )
    .unwrap();
    assert_eq!(
        preview["summary"],
        "Save the view Work/Invoices: invoice ext:pdf"
    );
    assert_eq!(call(&f.ctx, "views.list", Value::Null).unwrap(), json!([]));
    let created = apply(
        &f.ctx,
        "views.create",
        json!({"name": "Invoices", "query": "invoice ext:pdf", "folder": "Work",
               "sort": "modified:desc", "columns": ["name", "size"],
               "matching": {"case": true}, "filter": "Document", "shortcut": "Ctrl+Shift+1"}),
    );
    let id = created["id"].as_i64().unwrap();
    assert_eq!(
        created,
        json!({"id": id, "name": "Invoices", "query": "invoice ext:pdf", "layout": "details",
               "backend": "library", "sort": "modified:desc", "columns": ["name", "size"],
               "matching": {"case": true}, "filter": "Document", "folder": "Work",
               "position": 1, "shortcut": "Ctrl+Shift+1"})
    );
    assert_eq!(
        call(&f.ctx, "views.list", Value::Null).unwrap(),
        json!([created])
    );
    // A System-backend view keeps Everything syntax the library would refuse.
    let logs = apply(
        &f.ctx,
        "views.create",
        json!({"name": "Logs", "query": "ext:log size:lots", "backend": "system"}),
    );
    assert_eq!(logs["backend"], "system");

    // Refused at the preview: a bad library query, backend, sort, a taken shortcut.
    for bad in [
        json!({"name": "Bad", "query": "size:lots"}),
        json!({"name": "Bad", "query": "x", "backend": "web"}),
        json!({"name": "Bad", "query": "x", "sort": "name:up"}),
        json!({"name": "", "query": "x"}),
        json!({"name": "Bad", "query": "x", "shortcut": "ctrl+shift+1"}),
    ] {
        let e = call(&f.ctx, "views.create", bad.clone()).unwrap_err();
        assert_eq!(e.code, ApiError::INVALID_PARAMS, "{bad}: {e}");
    }

    // Update: omitted fields stay; "" removes the shortcut.
    let preview = call(
        &f.ctx,
        "views.update",
        json!({"id": id, "name": "Invoices 2026", "folder": ""}),
    )
    .unwrap();
    assert_eq!(
        preview["summary"],
        "Change the view Work/Invoices to Invoices 2026"
    );
    let updated = apply(
        &f.ctx,
        "views.update",
        json!({"id": id, "name": "Invoices 2026", "folder": "", "shortcut": "",
               "matching": {"regex": true, "whole_words": true}}),
    );
    assert_eq!(updated["name"], "Invoices 2026");
    assert_eq!(updated["query"], "invoice ext:pdf");
    assert_eq!(updated["sort"], "modified:desc");
    assert_eq!(updated["shortcut"], "");
    assert_eq!(
        updated["matching"],
        json!({"whole_words": true, "regex": true})
    );
    let e = call(&f.ctx, "views.update", json!({"id": 999, "name": "x"})).unwrap_err();
    assert_eq!(e.code, ApiError::NOT_FOUND);
    // The shortcut is free again.
    apply(
        &f.ctx,
        "views.update",
        json!({"id": logs["id"], "shortcut": "Ctrl+Shift+1"}),
    );

    // Delete previews, then answers what it deleted.
    let preview = call(&f.ctx, "views.delete", json!({"id": id})).unwrap();
    assert_eq!(preview["summary"], "Delete the view Invoices 2026");
    assert_eq!(
        call(&f.ctx, "views.list", Value::Null)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let deleted = apply(&f.ctx, "views.delete", json!({"id": id}));
    assert_eq!(deleted["name"], "Invoices 2026");
    let left = call(&f.ctx, "views.list", Value::Null).unwrap();
    assert_eq!(left.as_array().unwrap().len(), 1);
    assert_eq!(left[0]["name"], "Logs");
    let e = call(&f.ctx, "views.delete", json!({"id": id})).unwrap_err();
    assert_eq!(e.code, ApiError::NOT_FOUND);

    // MCP lists them as tools (registry names with `_`).
    let list = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
    let init = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}});
    let input = format!("{init}\n{list}\n");
    let mut out = Vec::new();
    let mut ctx = f.ctx;
    crate::mcp::serve(&mut ctx, input.as_bytes(), &mut out, Default::default()).unwrap();
    let out = String::from_utf8(out).unwrap();
    for tool in ["views_create", "views_update", "views_delete", "views_list"] {
        assert!(
            out.contains(&format!("\"name\":\"{tool}\"")),
            "{tool} missing"
        );
    }
}

/// Review 1: a download signed in with the weaker HTTP login stays out of Keel's own folders
/// (`files::in_keel_folders`: the configuration folder and the data folder).
#[test]
fn keel_folders_are_told_from_the_rest() {
    let f = fixture(None);
    let data = f.ctx.data_dir.clone().unwrap();
    let inside = |p: &std::path::Path| crate::files::in_keel_folders(&f.ctx, &s(p));
    assert!(inside(&f.cfg.path().join("daemon.token")));
    let key = data.join("device.key");
    std::fs::write(&key, b"k").unwrap();
    assert!(inside(&key));
    assert!(inside(&data));
    assert!(!inside(&f.files.path().join("docs/invoice-2026.pdf")));
    assert!(
        inside(std::path::Path::new("relative/path")),
        "unresolvable: counts as inside"
    );
}
