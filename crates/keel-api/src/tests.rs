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
        let direct = ["shares.revoke", "execute"].contains(&op.name);
        if op.mutating && !direct {
            assert!(op.previewed(), "{} mutates without a preview", op.name);
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
        "spacedrop.send",
        "spacedrop.inbox",
        "spacedrop.answer",
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
    rt.block_on(async {
        a_node.close().await;
        b_node.close().await;
    });
}
