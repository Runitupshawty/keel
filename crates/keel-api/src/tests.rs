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
        let direct = ["sources.remove", "shares.revoke", "execute"].contains(&op.name);
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
    ] {
        assert!(find(name).is_some(), "{name} missing");
    }
}

struct Fixture {
    _data: tempfile::TempDir,
    files: tempfile::TempDir,
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
    if let Some(plans) = plans {
        ctx = ctx.with_plans(plans);
    }
    Fixture {
        _data: data,
        files,
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

    // Removal acts at once and reports what it did.
    let removed = call(&f.ctx, "sources.remove", json!({"id": id})).unwrap();
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
            Arc::new(net::NoSources),
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
