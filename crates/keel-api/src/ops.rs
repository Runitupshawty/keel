//! The registry and its handlers.

use crate::error::{ApiError, Result};
use crate::plans::Input;
use crate::types::*;
use crate::{check, parse, schema, to_value, Ctx, Operation, Preview, Run};
use keel_core::{LibraryHit, LibraryQuery, RecordRef, SourceId, SourceStatus, FAVORITES};
use keel_vfs::VPath;
use serde_json::json;
use std::sync::Arc;

/// A read (or a direct removal when `mutating`).
macro_rules! now {
    ($name:literal, $summary:literal, $P:ty => $R:ty, $f:path, $ex:expr) => {
        now!($name, $summary, false, $P => $R, $f, $ex)
    };
    ($name:literal, $summary:literal, $mutating:expr, $P:ty => $R:ty, $f:path, $ex:expr) => {
        Operation {
            name: $name,
            summary: $summary,
            mutating: $mutating,
            params: schema::<$P>,
            result: schema::<$R>,
            example: || $ex,
            check: check::<$P>,
            run: Run::Now(|ctx, v| to_value::<$R>($f(ctx, parse::<$P>(v)?)?)),
        }
    };
}

/// Preview-first: a direct call previews, `execute` applies.
macro_rules! previewed {
    ($name:literal, $summary:literal, $P:ty => $R:ty, $preview:path, $apply:path, $ex:expr) => {
        previewed!($name, $summary, false, $P => $R, $preview, $apply, $ex)
    };
    ($name:literal, $summary:literal, $secret:expr, $P:ty => $R:ty, $preview:path, $apply:path, $ex:expr) => {
        Operation {
            name: $name,
            summary: $summary,
            mutating: true,
            params: schema::<$P>,
            result: schema::<PlanPreview>,
            example: || $ex,
            check: check::<$P>,
            run: Run::Previewed {
                preview: |ctx, v| $preview(ctx, &parse::<$P>(v.clone())?),
                apply: |ctx, v| to_value::<$R>($apply(ctx, parse::<$P>(v)?)?),
                applied: schema::<$R>,
                secret: $secret,
            },
        }
    };
}

/// Every operation, in the order `tools/list` and `docs/api.md` show them.
pub static OPS: &[Operation] = &[
    now!("version", "Keel version, API revision and the open library.",
        NoParams => VersionInfo, version, json!({})),
    now!("sources.list", "The library's sources with their status.",
        NoParams => Vec<SourceInfo>, sources_list, json!({})),
    previewed!("sources.add", "Adds a folder, drive or share as a library source (index it with sources.index).",
        AddSourceParams => AddedSource, sources_add_preview, sources_add, json!({"root": example_dir(), "label": "Photos"})),
    now!("sources.remove", "Forgets a source (its files are never touched; delete_store also deletes its index). Acts at once and returns what it removed.",
        true, RemoveSourceParams => RemovedSource, sources_remove, json!({"id": "0123456789abcdef0123456789abcdef"})),
    previewed!("sources.index", "Indexes a source as a background job.",
        SourceIdParams => JobStarted, sources_index_preview, sources_index, json!({"id": "0123456789abcdef0123456789abcdef"})),
    now!("list", "Lists a folder: a library path (library://<source>/<rel>, from the index, works offline) or any path Keel can reach.",
        ListParams => Listing, list, json!({"path": example_dir(), "max": 100})),
    now!("stat", "One file or folder, with its library record, tags and favorite state when indexed.",
        PathParams => StatInfo, stat, json!({"path": example_dir()})),
    now!("search", "Searches the library index across all sources (words, \"phrases\", kind:, ext:, size:, dm:, source:, tag:).",
        SearchParams => Vec<Hit>, search, json!({"query": "invoice ext:pdf", "max": 20})),
    now!("tags.list", "All tags, or the tags on one indexed path.",
        TagsListParams => Vec<TagInfo>, tags_list, json!({})),
    previewed!("tags.add", "Tags indexed paths (creating the tag when missing).",
        TagParams => Tagged, tags_add_preview, tags_add, json!({"tag": "receipts", "paths": [example_file()]})),
    previewed!("tags.remove", "Removes a tag from indexed paths.",
        TagParams => Tagged, tags_remove_preview, tags_remove, json!({"tag": "receipts", "paths": [example_file()]})),
    previewed!("tags.set", "Sets exactly these tags on indexed paths (Favorites is kept).",
        TagsSetParams => Tagged, tags_set_preview, tags_set, json!({"paths": [example_file()], "tags": ["receipts", "2026"]})),
    now!("favorites.list", "Favorite files and folders.",
        NoParams => Vec<Hit>, favorites_list, json!({})),
    previewed!("favorites.set", "Adds indexed paths to Favorites (on=false removes them).",
        FavoritesSetParams => Tagged, favorites_set_preview, favorites_set, json!({"paths": [example_file()], "on": true})),
    now!("recents", "Recently opened files, newest first.",
        LimitParams => Vec<Hit>, recents, json!({"limit": 20})),
    now!("jobs.list", "Library jobs (indexing, hashing, file operations), newest first.",
        NoParams => Vec<JobInfo>, jobs_list, json!({})),
    now!("jobs.info", "One job with its log.",
        JobParams => JobInfo, jobs_info, json!({"id": 1})),
    previewed!("jobs.cancel", "Cancels a queued or running job.",
        JobParams => JobInfo, jobs_cancel_preview, jobs_cancel, json!({"id": 1})),
    now!("duplicates", "Groups of indexed files with the same content, most wasted bytes first.",
        DuplicatesParams => Vec<DupGroup>, duplicates, json!({"min_size": 1048576, "max": 50})),
    now!("redundancy", "How many copies of a file's content the library knows, and where.",
        PathParams => Copies, redundancy, json!({"path": example_file()})),
    now!("plan", "Previews a file operation (copy, move, delete, rename) from the index without touching anything. Returns a preview; call execute with the plan id and input hash to apply.",
        PlanParams => PlanPreview, plan, json!({"op": "copy", "paths": [example_file()], "to": example_dir(), "on_conflict": "skip"})),
    now!("execute", "Applies a previewed plan: pass the plan_id and input_hash of the preview being confirmed. Refuses expired, tampered or changed plans.",
        true, ExecuteParams => Executed, execute, json!({"plan_id": "0123456789abcdef0123456789abcdef", "input_hash": "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"})),
    now!("devices.list", "This device and its paired devices with their link state.",
        NoParams => Devices, devices_list, json!({})),
    previewed!("devices.pair_code", "Creates a one-time pairing code (valid 10 minutes) for another device to enter.",
        NoParams => PairCodeInfo, pair_code_preview, pair_code, json!({})),
    previewed!("devices.pair_with", "Pairs with the device that showed this code. Pairing grants nothing by itself.",
        true, PairWithParams => PeerInfo, pair_with_preview, pair_with, json!({"code": "abcdefghijklmnopqrstuvwxyz"})),
    previewed!("devices.forget", "Forgets a paired device (its sessions close and its grants are removed).",
        PeerParams => Done, forget_preview, forget, json!({"peer": "a".repeat(52)})),
    now!("shares.list", "Grants this device gives its paired devices.",
        NoParams => Vec<GrantInfo>, shares_list, json!({})),
    previewed!("shares.grant", "Grants a paired device read or read-write access to a source or a subtree of it.",
        GrantParams => GrantInfo, grant_preview, grant, json!({"peer": "a".repeat(52), "source": "0123456789abcdef0123456789abcdef", "subtree": "Photos/2026", "access": "read"})),
    now!("shares.revoke", "Revokes a grant at once (the device's next request is refused) and returns what it revoked.",
        true, RevokeParams => Revoked, revoke, json!({"peer": "a".repeat(52), "source": "0123456789abcdef0123456789abcdef", "subtree": "Photos/2026"})),
];

fn example_dir() -> &'static str {
    if cfg!(windows) {
        r"C:\Users\me\Pictures"
    } else {
        "/home/me/Pictures"
    }
}

fn example_file() -> &'static str {
    if cfg!(windows) {
        r"C:\Users\me\Documents\invoice.pdf"
    } else {
        "/home/me/Documents/invoice.pdf"
    }
}

// --- helpers ---

/// An absolute local path (archive paths included) or a VPath URI.
pub(crate) fn vpath(s: &str) -> Result<VPath> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ApiError::invalid_params("empty path"));
    }
    if s.contains("://") {
        return VPath::parse(s).map_err(|e| ApiError::invalid_params(format!("{e:#}")));
    }
    let outer = s.split("!/").next().unwrap_or(s);
    if !std::path::Path::new(outer).is_absolute() {
        return Err(ApiError::invalid_params(format!(
            "not an absolute path: {s} (the host's working folder is not the caller's)"
        )));
    }
    Ok(VPath::local(s))
}

fn unix(t: Option<std::time::SystemTime>) -> Option<i64> {
    t.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

fn hit(h: &LibraryHit) -> Hit {
    Hit {
        path: h.path.display(),
        name: h.name.clone(),
        is_dir: h.is_dir,
        size: h.size,
        modified: h.modified,
        source: h.record.source.0.clone(),
        source_label: h.source_label.clone(),
        record: h.record.id,
        score: h.score,
    }
}

fn hits(v: Vec<LibraryHit>) -> Vec<Hit> {
    v.iter().map(hit).collect()
}

fn kind_name(k: keel_core::SourceKind) -> SourceKindName {
    use keel_core::SourceKind as K;
    match k {
        K::Folder => SourceKindName::Folder,
        K::Drive => SourceKindName::Drive,
        K::Share => SourceKindName::Share,
        K::Cloud => SourceKindName::Cloud,
        K::Device => SourceKindName::Device,
    }
}

fn source_info(s: &keel_core::SourceSummary) -> SourceInfo {
    let mut info = SourceInfo {
        id: s.id.0.clone(),
        label: s.label.clone(),
        root: s.root.display(),
        kind: kind_name(s.kind),
        status: String::new(),
        indexed_at: None,
        last_seen: None,
        detail: None,
        generation: s.generation,
    };
    match &s.status {
        SourceStatus::Online { indexed_at } => {
            info.status = "online".into();
            info.indexed_at = *indexed_at;
        }
        SourceStatus::Indexing { done, total } => {
            info.status = "indexing".into();
            info.detail = Some(format!("{done} of about {total} records"));
        }
        SourceStatus::Offline { last_seen, reason } => {
            info.status = "offline".into();
            info.last_seen = *last_seen;
            info.detail = Some(format!("{reason:?}").to_lowercase());
        }
        SourceStatus::Error(e) => {
            info.status = "error".into();
            info.detail = Some(e.clone());
        }
    }
    info
}

fn find_source(ctx: &Ctx, id: &str) -> Result<keel_core::SourceSummary> {
    ctx.lib
        .sources()
        .into_iter()
        .find(|s| s.id.0 == id)
        .ok_or_else(|| ApiError::not_found(format!("no source {id}")))
}

/// `(source, path relative to it)` of a library path or a path inside a source.
fn locate(ctx: &Ctx, p: &VPath) -> Result<(SourceId, String)> {
    if let Some((id, rel)) = keel_vfs::library::split(p) {
        return Ok((SourceId(id.to_owned()), rel.to_owned()));
    }
    let (src, rel) = ctx
        .lib
        .source_for(p)
        .ok_or_else(|| ApiError::not_found(format!("{} is in no library source", p.display())))?;
    Ok((src.id.clone(), rel))
}

/// The indexed record at `path`.
fn record_at(ctx: &Ctx, path: &str) -> Result<LibraryHit> {
    let p = vpath(path)?;
    let (source, rel) = locate(ctx, &p)?;
    if rel.is_empty() {
        return Err(ApiError::invalid_params(format!(
            "{path} is a source root, not a record"
        )));
    }
    let (parent, name) = rel.rsplit_once('/').unwrap_or(("", &rel));
    let children = ctx
        .lib
        .list_children(&source, parent)
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?;
    children
        .iter()
        .find(|h| h.name == name)
        .or_else(|| children.iter().find(|h| h.name.eq_ignore_ascii_case(name)))
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("{path} is not in the library index yet")))
}

fn records(ctx: &Ctx, paths: &[String]) -> Result<Vec<LibraryHit>> {
    if paths.is_empty() {
        return Err(ApiError::invalid_params("no paths"));
    }
    paths.iter().map(|p| record_at(ctx, p)).collect()
}

fn refs(hits: &[LibraryHit]) -> Vec<RecordRef> {
    hits.iter().map(|h| h.record.clone()).collect()
}

fn change(action: &str, path: Option<String>, detail: Option<String>) -> Change {
    Change {
        action: action.into(),
        path,
        to: None,
        detail,
        files: None,
        bytes: None,
    }
}

fn warning(kind: &str, path: Option<String>, message: String) -> Warning {
    Warning {
        kind: kind.into(),
        path,
        files: None,
        message,
    }
}

// --- library ---

fn version(ctx: &Ctx, _: NoParams) -> Result<VersionInfo> {
    Ok(VersionInfo {
        name: "keel".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        api: crate::API_VERSION,
        library: ctx.lib.name.clone(),
        net: ctx.node.is_some(),
        pid: std::process::id(),
    })
}

fn sources_list(ctx: &Ctx, _: NoParams) -> Result<Vec<SourceInfo>> {
    Ok(ctx.lib.sources().iter().map(source_info).collect())
}

fn source_def(p: &AddSourceParams) -> Result<keel_core::SourceDef> {
    let root = vpath(&p.root)?;
    if let Some(local) = root.to_local_path() {
        if !local.is_dir() {
            return Err(ApiError::invalid_params(format!(
                "{} is not a folder",
                root.display()
            )));
        }
    }
    let label = match &p.label {
        Some(l) if !l.trim().is_empty() => l.trim().to_owned(),
        _ => match root.name() {
            "" => root.display(),
            n => n.to_owned(),
        },
    };
    use keel_core::SourceKind as K;
    Ok(keel_core::SourceDef {
        label,
        root,
        kind: match p.kind.unwrap_or(SourceKindName::Folder) {
            SourceKindName::Folder => K::Folder,
            SourceKindName::Drive => K::Drive,
            SourceKindName::Share => K::Share,
            SourceKindName::Cloud => K::Cloud,
            SourceKindName::Device => K::Device,
        },
        include_hidden: p.include_hidden,
        ignore: p.ignore.clone(),
        poll_secs: None,
        hash_shares: false,
    })
}

fn sources_add_preview(ctx: &Ctx, p: &AddSourceParams) -> Result<Preview> {
    let def = source_def(p)?;
    if let Some((s, _)) = ctx.lib.source_for(&def.root) {
        return Err(ApiError::invalid_params(format!(
            "{} is inside source {}",
            def.root.display(),
            s.def.label
        )));
    }
    Ok(Preview {
        summary: format!("Add source {} at {}", def.label, def.root.display()),
        changes: vec![change(
            "source.add",
            Some(def.root.display()),
            Some(def.label.clone()),
        )],
        warnings: Vec::new(),
    })
}

fn sources_add(ctx: &Ctx, p: AddSourceParams) -> Result<AddedSource> {
    let id = ctx.lib.add_source(source_def(&p)?)?;
    Ok(AddedSource { id: id.0 })
}

fn sources_remove(ctx: &Ctx, p: RemoveSourceParams) -> Result<RemovedSource> {
    let removed = source_info(&find_source(ctx, &p.id)?);
    ctx.lib
        .remove_source(&SourceId(p.id.clone()), p.delete_store)?;
    Ok(RemovedSource {
        removed,
        store_deleted: p.delete_store,
    })
}

fn sources_index_preview(ctx: &Ctx, p: &SourceIdParams) -> Result<Preview> {
    let s = find_source(ctx, &p.id)?;
    Ok(Preview {
        summary: format!("Index {} ({})", s.label, s.root.display()),
        changes: vec![change(
            "source.index",
            Some(s.root.display()),
            Some(s.label.clone()),
        )],
        warnings: Vec::new(),
    })
}

fn sources_index(ctx: &Ctx, p: SourceIdParams) -> Result<JobStarted> {
    find_source(ctx, &p.id)?;
    Ok(JobStarted {
        job: ctx.lib.index(&SourceId(p.id))?,
    })
}

fn list(ctx: &Ctx, p: ListParams) -> Result<Listing> {
    let max = p.max.unwrap_or(1000).max(1);
    let dir = vpath(&p.path)?;
    let mut entries: Vec<EntryInfo> = if let Some((id, rel)) = keel_vfs::library::split(&dir) {
        let children = ctx
            .lib
            .list_children(&SourceId(id.to_owned()), rel)
            .map_err(|e| ApiError::not_found(format!("{e:#}")))?;
        children
            .iter()
            .map(|h| EntryInfo {
                name: h.name.clone(),
                path: keel_vfs::library::path(id, &format!("{rel}/{}", h.name)).display(),
                is_dir: h.is_dir,
                size: h.size,
                modified: h.modified,
                hidden: false,
            })
            .collect()
    } else {
        let provider = ctx
            .router
            .provider_for(&dir)
            .ok_or_else(|| ApiError::not_found(format!("nothing serves {}", dir.display())))?;
        let mut v: Vec<EntryInfo> = provider
            .list(&dir)?
            .into_iter()
            .map(|e| EntryInfo {
                name: e.name.clone(),
                path: e.path.display(),
                is_dir: e.kind == keel_vfs::Kind::Dir,
                size: e.size,
                modified: unix(e.modified),
                hidden: e.hidden,
            })
            .collect();
        v.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        v
    };
    let truncated = entries.len() > max;
    entries.truncate(max);
    Ok(Listing { entries, truncated })
}

fn indexed(ctx: &Ctx, h: &LibraryHit) -> Result<IndexedInfo> {
    let ids = ctx.lib.tags_of(&h.record)?;
    let tags = ctx.lib.tags()?;
    Ok(IndexedInfo {
        source: h.record.source.0.clone(),
        source_label: h.source_label.clone(),
        record: h.record.id,
        tags: tags
            .iter()
            .filter(|t| ids.contains(&t.id))
            .map(|t| t.name.clone())
            .collect(),
        favorite: ids.contains(&FAVORITES),
    })
}

fn stat(ctx: &Ctx, p: PathParams) -> Result<StatInfo> {
    let path = vpath(&p.path)?;
    if keel_vfs::library::split(&path).is_some() {
        let h = record_at(ctx, &p.path)?;
        return Ok(StatInfo {
            entry: EntryInfo {
                name: h.name.clone(),
                path: path.display(),
                is_dir: h.is_dir,
                size: h.size,
                modified: h.modified,
                hidden: false,
            },
            indexed: Some(indexed(ctx, &h)?),
        });
    }
    let provider = ctx
        .router
        .provider_for(&path)
        .ok_or_else(|| ApiError::not_found(format!("nothing serves {}", path.display())))?;
    let e = provider
        .stat(&path)
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?;
    let indexed = match record_at(ctx, &p.path) {
        Ok(h) => Some(indexed(ctx, &h)?),
        Err(_) => None,
    };
    Ok(StatInfo {
        entry: EntryInfo {
            name: e.name.clone(),
            path: e.path.display(),
            is_dir: e.kind == keel_vfs::Kind::Dir,
            size: e.size,
            modified: unix(e.modified),
            hidden: e.hidden,
        },
        indexed,
    })
}

fn search(ctx: &Ctx, p: SearchParams) -> Result<Vec<Hit>> {
    let mut q = LibraryQuery::parse(&p.query, ctx.utc_offset)
        .map_err(|e| ApiError::invalid_params(format!("{e:#}")))?;
    q.max = p.max.unwrap_or(100).clamp(1, 10_000);
    Ok(hits(ctx.lib.search(&q)?))
}

// --- tags ---

fn tags_list(ctx: &Ctx, p: TagsListParams) -> Result<Vec<TagInfo>> {
    let mut tags = ctx.lib.tags()?;
    if let Some(path) = &p.path {
        let ids = ctx.lib.tags_of(&record_at(ctx, path)?.record)?;
        tags.retain(|t| ids.contains(&t.id));
    }
    Ok(tags
        .into_iter()
        .map(|t| TagInfo {
            id: t.id,
            name: t.name,
            color: t.color,
            parent: t.parent,
        })
        .collect())
}

fn tag_named(ctx: &Ctx, name: &str) -> Result<Option<keel_core::Tag>> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::invalid_params("empty tag name"));
    }
    let tags = ctx.lib.tags()?;
    Ok(tags
        .iter()
        .find(|t| t.name == name && t.parent.is_none())
        .or_else(|| tags.iter().find(|t| t.name.eq_ignore_ascii_case(name)))
        .cloned())
}

fn tag_or_create(ctx: &Ctx, name: &str) -> Result<i64> {
    match tag_named(ctx, name)? {
        Some(t) => Ok(t.id),
        None => Ok(ctx.lib.create_tag(name.trim(), None, None)?),
    }
}

fn tag_changes(action: &str, hits: &[LibraryHit], detail: &str) -> Vec<Change> {
    hits.iter()
        .map(|h| change(action, Some(h.path.display()), Some(detail.to_owned())))
        .collect()
}

fn tags_add_preview(ctx: &Ctx, p: &TagParams) -> Result<Preview> {
    let hits = records(ctx, &p.paths)?;
    let mut warnings = Vec::new();
    if tag_named(ctx, &p.tag)?.is_none() {
        warnings.push(warning(
            "creates_tag",
            None,
            format!(
                "tag {:?} does not exist yet and will be created",
                p.tag.trim()
            ),
        ));
    }
    Ok(Preview {
        summary: format!("Tag {} item(s) with {}", hits.len(), p.tag.trim()),
        changes: tag_changes("tag.add", &hits, p.tag.trim()),
        warnings,
    })
}

fn tags_add(ctx: &Ctx, p: TagParams) -> Result<Tagged> {
    let hits = records(ctx, &p.paths)?;
    let tag = tag_or_create(ctx, &p.tag)?;
    ctx.lib.set_tag(tag, &refs(&hits), true)?;
    Ok(Tagged {
        records: hits.len(),
    })
}

fn existing_tag(ctx: &Ctx, name: &str) -> Result<keel_core::Tag> {
    tag_named(ctx, name)?.ok_or_else(|| ApiError::not_found(format!("no tag {name:?}")))
}

fn tags_remove_preview(ctx: &Ctx, p: &TagParams) -> Result<Preview> {
    let tag = existing_tag(ctx, &p.tag)?;
    let hits = records(ctx, &p.paths)?;
    Ok(Preview {
        summary: format!("Remove tag {} from {} item(s)", tag.name, hits.len()),
        changes: tag_changes("tag.remove", &hits, &tag.name),
        warnings: Vec::new(),
    })
}

fn tags_remove(ctx: &Ctx, p: TagParams) -> Result<Tagged> {
    let tag = existing_tag(ctx, &p.tag)?;
    let hits = records(ctx, &p.paths)?;
    ctx.lib.set_tag(tag.id, &refs(&hits), false)?;
    Ok(Tagged {
        records: hits.len(),
    })
}

fn tags_set_preview(ctx: &Ctx, p: &TagsSetParams) -> Result<Preview> {
    let hits = records(ctx, &p.paths)?;
    let mut warnings = Vec::new();
    for name in &p.tags {
        if tag_named(ctx, name)?.is_none() {
            warnings.push(warning(
                "creates_tag",
                None,
                format!(
                    "tag {:?} does not exist yet and will be created",
                    name.trim()
                ),
            ));
        }
    }
    let names = p
        .tags
        .iter()
        .map(|t| t.trim())
        .collect::<Vec<_>>()
        .join(", ");
    Ok(Preview {
        summary: format!("Set the tags of {} item(s) to [{names}]", hits.len()),
        changes: tag_changes("tags.set", &hits, &names),
        warnings,
    })
}

fn tags_set(ctx: &Ctx, p: TagsSetParams) -> Result<Tagged> {
    let hits = records(ctx, &p.paths)?;
    let want = p
        .tags
        .iter()
        .map(|t| tag_or_create(ctx, t))
        .collect::<Result<Vec<_>>>()?;
    for h in &hits {
        for id in ctx.lib.tags_of(&h.record)? {
            if id != FAVORITES && !want.contains(&id) {
                ctx.lib
                    .set_tag(id, std::slice::from_ref(&h.record), false)?;
            }
        }
    }
    for id in want {
        ctx.lib.set_tag(id, &refs(&hits), true)?;
    }
    Ok(Tagged {
        records: hits.len(),
    })
}

fn favorites_list(ctx: &Ctx, _: NoParams) -> Result<Vec<Hit>> {
    Ok(hits(ctx.lib.favorites()?))
}

fn favorites_set_preview(ctx: &Ctx, p: &FavoritesSetParams) -> Result<Preview> {
    let hits = records(ctx, &p.paths)?;
    let (action, verb) = match p.on {
        true => ("favorite.add", "Add"),
        false => ("favorite.remove", "Remove"),
    };
    Ok(Preview {
        summary: format!(
            "{verb} {} item(s) {} Favorites",
            hits.len(),
            if p.on { "to" } else { "from" }
        ),
        changes: tag_changes(action, &hits, "Favorites"),
        warnings: Vec::new(),
    })
}

fn favorites_set(ctx: &Ctx, p: FavoritesSetParams) -> Result<Tagged> {
    let hits = records(ctx, &p.paths)?;
    ctx.lib.set_favorite(&refs(&hits), p.on)?;
    Ok(Tagged {
        records: hits.len(),
    })
}

fn recents(ctx: &Ctx, p: LimitParams) -> Result<Vec<Hit>> {
    Ok(hits(ctx.lib.recents(p.limit.unwrap_or(50).clamp(1, 1000))?))
}

// --- jobs ---

fn job_info(j: keel_core::JobInfo, with_log: bool) -> JobInfo {
    JobInfo {
        id: j.id,
        kind: j.kind,
        status: format!("{:?}", j.status).to_lowercase(),
        progress: j.progress,
        created: j.created,
        updated: j.updated,
        result: j.result,
        log: with_log.then_some(j.log),
    }
}

fn jobs_list(ctx: &Ctx, _: NoParams) -> Result<Vec<JobInfo>> {
    Ok(ctx
        .lib
        .jobs()
        .list()?
        .into_iter()
        .map(|j| job_info(j, false))
        .collect())
}

fn job(ctx: &Ctx, id: i64) -> Result<keel_core::JobInfo> {
    ctx.lib
        .jobs()
        .info(id)
        .map_err(|_| ApiError::not_found(format!("no job {id}")))
}

fn jobs_info(ctx: &Ctx, p: JobParams) -> Result<JobInfo> {
    Ok(job_info(job(ctx, p.id)?, true))
}

fn jobs_cancel_preview(ctx: &Ctx, p: &JobParams) -> Result<Preview> {
    let j = job_info(job(ctx, p.id)?, false);
    let mut warnings = Vec::new();
    if j.finished() {
        warnings.push(warning(
            "finished",
            None,
            format!("job {} already ended ({})", j.id, j.status),
        ));
    }
    Ok(Preview {
        summary: format!("Cancel job {} ({}, {})", j.id, j.kind, j.status),
        changes: vec![change("job.cancel", None, Some(j.kind.clone()))],
        warnings,
    })
}

fn jobs_cancel(ctx: &Ctx, p: JobParams) -> Result<JobInfo> {
    job(ctx, p.id)?;
    ctx.lib.jobs().cancel(p.id)?;
    Ok(job_info(job(ctx, p.id)?, false))
}

// --- content ---

fn duplicates(ctx: &Ctx, p: DuplicatesParams) -> Result<Vec<DupGroup>> {
    let mut groups = ctx.lib.duplicates(p.min_size.unwrap_or(1))?;
    groups.sort_by_key(|g| std::cmp::Reverse(g.size * (g.records.len() as u64 - 1)));
    groups.truncate(p.max.unwrap_or(100).clamp(1, 10_000));
    groups
        .into_iter()
        .map(|g| {
            let mut paths = Vec::new();
            for r in &g.records {
                if let Some(h) = ctx.lib.record(r)? {
                    paths.push(h.path.display());
                }
            }
            Ok(DupGroup {
                content_id: g.cas_id.iter().map(|b| format!("{b:02x}")).collect(),
                size: g.size,
                paths,
            })
        })
        .collect()
}

fn redundancy(ctx: &Ctx, p: PathParams) -> Result<Copies> {
    let h = record_at(ctx, &p.path)?;
    let c = ctx.lib.redundancy(&h.record)?;
    Ok(Copies {
        copies: c.copies,
        failure_domains: c.failure_domains,
        backed_up: c.backed_up,
        offline_copies: c.offline_copies,
        locations: c
            .locations
            .into_iter()
            .map(|l| Location {
                path: l.path.display(),
                source_label: l.source_label,
                volume: l.volume.label.clone(),
                failure_domain: l.volume.failure_domain.clone(),
            })
            .collect(),
    })
}

// --- file plans ---

fn plan(ctx: &Ctx, p: PlanParams) -> Result<PlanPreview> {
    use keel_core::OnConflict as C;
    let on_conflict = match p.on_conflict.unwrap_or(OnConflict::Skip) {
        OnConflict::Skip => C::Skip,
        OnConflict::Overwrite => C::Overwrite,
        OnConflict::RenameNew => C::RenameNew,
    };
    if p.paths.is_empty() {
        return Err(ApiError::invalid_params("no paths"));
    }
    let paths = p
        .paths
        .iter()
        .map(|s| vpath(s))
        .collect::<Result<Vec<_>>>()?;
    let dst = || -> Result<VPath> {
        vpath(
            p.to.as_deref()
                .ok_or_else(|| ApiError::invalid_params("copy and move need `to`"))?,
        )
    };
    let op = match p.op {
        FileOpKind::Copy => keel_core::Op::Copy {
            src: paths,
            dst_dir: dst()?,
            on_conflict,
        },
        FileOpKind::Move => keel_core::Op::Move {
            src: paths,
            dst_dir: dst()?,
            on_conflict,
        },
        FileOpKind::Delete => keel_core::Op::Delete { paths },
        FileOpKind::Rename => {
            let [path] = <[VPath; 1]>::try_from(paths)
                .map_err(|_| ApiError::invalid_params("rename takes exactly one path"))?;
            let new_name = p
                .new_name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .ok_or_else(|| ApiError::invalid_params("rename needs `new_name`"))?;
            keel_core::Op::Rename { path, new_name }
        }
    };
    let plan = keel_core::validate_preview_execute(&ctx.lib, op)
        .map_err(|e| ApiError::invalid_params(format!("{e:#}")))?;
    store_file_plan(ctx, plan)
}

fn store_file_plan(ctx: &Ctx, plan: keel_core::Plan) -> Result<PlanPreview> {
    let summary = file_summary(&plan);
    let changes = plan.changes.iter().map(file_change).collect();
    let warnings = plan.warnings.iter().map(file_warning).collect();
    let (plan_id, input_hash, expires_at) = ctx.plans.insert(Input::Files(plan), false)?;
    Ok(PlanPreview {
        plan_id,
        input_hash,
        operation: "plan".into(),
        summary,
        changes,
        warnings,
        expires_at,
    })
}

fn file_summary(plan: &keel_core::Plan) -> String {
    let files: u64 = plan.changes.iter().map(|c| c.files).sum();
    let bytes: u64 = plan.changes.iter().map(|c| c.bytes).sum();
    let what = format!(
        "{} item(s), {files} file(s), {bytes} bytes",
        plan.changes.len()
    );
    match &plan.op {
        keel_core::Op::Copy { dst_dir, .. } => format!("Copy {what} to {}", dst_dir.display()),
        keel_core::Op::Move { dst_dir, .. } => format!("Move {what} to {}", dst_dir.display()),
        keel_core::Op::Delete { .. } => format!("Delete {what}"),
        keel_core::Op::Rename { path, new_name } => {
            format!("Rename {} to {new_name}", path.display())
        }
    }
}

fn file_change(c: &keel_core::Change) -> Change {
    Change {
        action: format!("{:?}", c.action).to_lowercase(),
        path: Some(c.from.display()),
        to: c.to.as_ref().map(VPath::display),
        detail: None,
        files: Some(c.files),
        bytes: Some(c.bytes),
    }
}

fn file_warning(w: &keel_core::Warning) -> Warning {
    use keel_core::Warning as W;
    let (kind, path, files, message) = match w {
        W::LastCopy { path, files } => (
            "last_copy",
            Some(path),
            Some(*files),
            format!(
                "{files} file(s) in {} are the last indexed copy of their content",
                path.display()
            ),
        ),
        W::SingleDomain { path, files } => (
            "single_domain",
            Some(path),
            Some(*files),
            format!(
                "{files} file(s) in {} would leave every remaining copy on one disk or account",
                path.display()
            ),
        ),
        W::OfflineSource { label, .. } => (
            "offline_source",
            None,
            None,
            format!("source {label} is offline: previewed from its last index; it must be online to run"),
        ),
        W::NotIndexed { path } => (
            "not_indexed",
            Some(path),
            None,
            format!("{} is not indexed: previewed from the folder itself", path.display()),
        ),
        W::Exists { path, on_conflict } => (
            "exists",
            Some(path),
            None,
            format!("{} exists already ({on_conflict:?})", path.display()),
        ),
        W::Permanent { path } => (
            "permanent",
            Some(path),
            None,
            format!("{} is deleted for good (no trash)", path.display()),
        ),
        W::ContentUnverified { path, files } => (
            "content_unverified",
            Some(path),
            Some(*files),
            format!(
                "{files} file(s) in {} have no content id yet: whether other copies exist is unknown",
                path.display()
            ),
        ),
    };
    Warning {
        kind: kind.into(),
        path: path.map(VPath::display),
        files,
        message,
    }
}

fn execute(ctx: &Ctx, p: ExecuteParams) -> Result<Executed> {
    let input = ctx.plans.take(&p.plan_id, &p.input_hash)?;
    let operation = input.method().to_owned();
    match input {
        Input::Files(plan) => match plan.execute(&ctx.lib, true) {
            Ok(job) => Ok(Executed {
                plan_id: p.plan_id,
                operation,
                job: Some(job),
                result: None,
            }),
            Err(e) => match e.downcast::<keel_core::PlanChanged>() {
                Ok(keel_core::PlanChanged(fresh)) => {
                    let preview = store_file_plan(ctx, *fresh)?;
                    Err(ApiError::new(
                        ApiError::PLAN_CHANGED,
                        "the sources changed since the preview: confirm the new preview",
                    )
                    .with_data(to_value(preview)?))
                }
                Err(e) => Err(e.into()),
            },
        },
        Input::Call { method, params } => {
            let Some(Run::Previewed { apply, .. }) = crate::find(&method).map(|op| &op.run) else {
                return Err(ApiError::new(
                    ApiError::INTERNAL,
                    format!("plan for unknown operation {method}"),
                ));
            };
            Ok(Executed {
                plan_id: p.plan_id,
                operation,
                job: None,
                result: Some(apply(ctx, params)?),
            })
        }
    }
}

// --- devices and shares ---

fn node(ctx: &Ctx) -> Result<(&Arc<keel_net::Node>, &tokio::runtime::Handle)> {
    match (&ctx.node, &ctx.rt) {
        (Some(n), Some(rt)) => Ok((n, rt)),
        _ => Err(ApiError::new(
            ApiError::NET_DISABLED,
            "devices are off: set [net] enabled = true in the profile's config.toml and restart keel-daemon",
        )),
    }
}

fn peer_info(p: &keel_net::Peer) -> PeerInfo {
    PeerInfo {
        id: p.id.0.to_string(),
        label: p.label.clone(),
        link: format!("{:?}", p.link).to_lowercase(),
        last_seen: p.last_seen,
        storage_used: p.storage.as_ref().map(|s| s.used),
        storage_total: p.storage.as_ref().map(|s| s.total),
    }
}

fn peer_id(s: &str) -> Result<keel_net::PeerId> {
    s.trim()
        .parse::<keel_net::NodeId>()
        .map(keel_net::PeerId)
        .map_err(|e| ApiError::invalid_params(format!("bad peer id: {e:#}")))
}

fn paired(ctx: &Ctx, s: &str) -> Result<keel_net::Peer> {
    let (node, _) = node(ctx)?;
    let id = peer_id(s)?;
    node.peers()
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| ApiError::not_found(format!("no paired device {s}")))
}

fn devices_list(ctx: &Ctx, _: NoParams) -> Result<Devices> {
    let (node, _) = node(ctx)?;
    Ok(Devices {
        id: node.id().to_string(),
        label: node.label(),
        peers: node.peers().iter().map(peer_info).collect(),
    })
}

fn pair_code_preview(ctx: &Ctx, _: &NoParams) -> Result<Preview> {
    node(ctx)?;
    Ok(Preview {
        summary: "Create a one-time pairing code, valid 10 minutes".into(),
        changes: vec![change("device.pair_code", None, None)],
        warnings: vec![warning(
            "secret",
            None,
            "whoever has the code can pair with this device: share it only with the other device"
                .into(),
        )],
    })
}

fn pair_code(ctx: &Ctx, _: NoParams) -> Result<PairCodeInfo> {
    let (node, rt) = node(ctx)?;
    let code = rt.block_on(node.pair_code())?;
    Ok(PairCodeInfo {
        code: code.to_string(),
        ticket: code.ticket(),
        expires_at: keel_now() + 600,
    })
}

fn keel_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn pair_with_preview(ctx: &Ctx, p: &PairWithParams) -> Result<Preview> {
    node(ctx)?;
    p.code
        .trim()
        .parse::<keel_net::PairCode>()
        .map_err(|e| ApiError::invalid_params(format!("bad pairing code: {e:#}")))?;
    Ok(Preview {
        summary: "Pair with the device that showed this code (no access is granted)".into(),
        changes: vec![change("device.pair", None, None)],
        warnings: Vec::new(),
    })
}

fn pair_with(ctx: &Ctx, p: PairWithParams) -> Result<PeerInfo> {
    let (node, rt) = node(ctx)?;
    let code = p
        .code
        .trim()
        .parse::<keel_net::PairCode>()
        .map_err(|e| ApiError::invalid_params(format!("bad pairing code: {e:#}")))?;
    Ok(peer_info(&rt.block_on(node.pair_with(&code))?))
}

fn forget_preview(ctx: &Ctx, p: &PeerParams) -> Result<Preview> {
    let peer = paired(ctx, &p.peer)?;
    let (node, _) = node(ctx)?;
    let grants = node.grants().iter().filter(|g| g.peer == peer.id).count();
    Ok(Preview {
        summary: format!("Forget device {} and its {grants} grant(s)", peer.label),
        changes: vec![change(
            "device.forget",
            None,
            Some(format!("{} ({})", peer.label, p.peer.trim())),
        )],
        warnings: Vec::new(),
    })
}

fn forget(ctx: &Ctx, p: PeerParams) -> Result<Done> {
    let peer = paired(ctx, &p.peer)?;
    node(ctx)?.0.forget_peer(&peer.id)?;
    Ok(Done { ok: true })
}

fn grant_info(g: &keel_net::Grant) -> GrantInfo {
    GrantInfo {
        peer: g.peer.0.to_string(),
        source: g.source.clone(),
        subtree: g.subtree.clone(),
        access: match g.access {
            keel_net::Access::Read => Access::Read,
            keel_net::Access::ReadWrite => Access::ReadWrite,
        },
        created: g.created,
    }
}

fn shares_list(ctx: &Ctx, _: NoParams) -> Result<Vec<GrantInfo>> {
    Ok(node(ctx)?.0.grants().iter().map(grant_info).collect())
}

fn grant_preview(ctx: &Ctx, p: &GrantParams) -> Result<Preview> {
    let peer = paired(ctx, &p.peer)?;
    let source = find_source(ctx, &p.source)?;
    let what = match p.subtree.trim_matches('/') {
        "" => source.label.clone(),
        sub => format!("{}/{sub}", source.label),
    };
    let access = match p.access {
        Access::Read => "read",
        Access::ReadWrite => "read-write",
    };
    let mut warnings = Vec::new();
    if p.access == Access::ReadWrite {
        warnings.push(warning(
            "read_write",
            None,
            format!(
                "{} will be able to change and delete files in {what}",
                peer.label
            ),
        ));
    }
    Ok(Preview {
        summary: format!("Give {} {access} access to {what}", peer.label),
        changes: vec![Change {
            action: "share.grant".into(),
            path: Some(source.root.display()),
            to: Some(peer.label.clone()),
            detail: Some(format!("{access}: {what}")),
            files: None,
            bytes: None,
        }],
        warnings,
    })
}

fn grant(ctx: &Ctx, p: GrantParams) -> Result<GrantInfo> {
    let peer = paired(ctx, &p.peer)?;
    find_source(ctx, &p.source)?;
    let g = keel_net::Grant {
        peer: peer.id,
        source: p.source,
        subtree: p.subtree.trim_matches('/').to_owned(),
        access: match p.access {
            Access::Read => keel_net::Access::Read,
            Access::ReadWrite => keel_net::Access::ReadWrite,
        },
        created: keel_now(),
    };
    node(ctx)?.0.grant(g.clone())?;
    Ok(grant_info(&g))
}

fn revoke(ctx: &Ctx, p: RevokeParams) -> Result<Revoked> {
    let (node, _) = node(ctx)?;
    let peer = peer_id(&p.peer)?;
    let subtree = p.subtree.trim_matches('/').to_owned();
    let existed = node
        .grants()
        .iter()
        .any(|g| g.peer == peer && g.source == p.source && g.subtree == subtree);
    node.revoke(&peer, &p.source, &subtree)?;
    Ok(Revoked {
        peer: p.peer.trim().to_owned(),
        source: p.source,
        subtree,
        existed,
    })
}
