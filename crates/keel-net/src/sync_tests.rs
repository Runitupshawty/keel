//! Library sync between two in-process nodes, each serving its own library (offline,
//! loopback).
use super::*;
use keel_core::{JobStatus, Library, RecordRef, FAVORITES};
use keel_vfs::cloud::MemoryStore;
use std::{path::Path, sync::Arc, time::Duration};

struct Side {
    lib: Arc<Library>,
    node: Arc<Node>,
    src: Arc<keel_core::Source>,
    _dirs: [tempfile::TempDir; 3],
}

impl Side {
    fn id(&self) -> PeerId {
        PeerId(self.node.id())
    }

    fn tag(&self, name: &str) -> Option<keel_core::Tag> {
        self.lib
            .tags()
            .unwrap()
            .into_iter()
            .find(|t| t.name == name)
    }

    fn rec(&self, rel: &str) -> RecordRef {
        let hit = self
            .lib
            .list_children(&self.src.id, "")
            .unwrap()
            .into_iter()
            .find(|h| h.name == rel)
            .unwrap_or_else(|| panic!("no record {rel}"));
        hit.record
    }
}

struct Two {
    rt: tokio::runtime::Runtime,
    a: Side,
    b: Side,
}

/// A library with one folder source holding `files`, indexed and hashed.
fn library(
    dir: &Path,
    files: &Path,
    contents: &[(&str, &str)],
) -> (Arc<Library>, Arc<keel_core::Source>) {
    for (name, data) in contents {
        std::fs::write(files.join(name), data).unwrap();
    }
    let lib = Arc::new(Library::open(dir, "test").unwrap());
    let id = lib.add_source(crate::library_tests::folder(files)).unwrap();
    let job = lib.index(&id).unwrap();
    assert_eq!(lib.jobs().wait(job).unwrap().status, JobStatus::Done);
    let job = lib.hash().unwrap();
    assert_eq!(lib.jobs().wait(job).unwrap().status, JobStatus::Done);
    let src = lib.source(&id).unwrap();
    (lib, src)
}

/// Two paired devices with their own libraries; `b` uses `b_options`.
fn two(a_files: &[(&str, &str)], b_files: &[(&str, &str)], b_options: NodeOptions) -> Two {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let side = |files: &[(&str, &str)], options: NodeOptions| -> Side {
        let dirs = [(); 3].map(|_| tempfile::tempdir().unwrap());
        let (lib, src) = library(dirs[0].path(), dirs[1].path(), files);
        let node = rt
            .block_on(Node::open_with_options(
                Arc::new(MemoryStore::default()),
                dirs[2].path(),
                Arc::new(LibraryHandler::new(lib.clone())),
                options,
            ))
            .unwrap();
        Side {
            lib,
            node,
            src,
            _dirs: dirs,
        }
    };
    let a = side(a_files, manual());
    let b = side(b_files, b_options);
    rt.block_on(async {
        let code = a.node.pair_code().await.unwrap();
        b.node
            .pair_with(&code.ticket().parse().unwrap())
            .await
            .unwrap();
    });
    Two { rt, a, b }
}

impl Two {
    /// Sync on, both ways.
    fn on(&self) {
        self.a.node.set_sync_peers([self.b.id()]);
        self.b.node.set_sync_peers([self.a.id()]);
    }

    /// `to` pulls from `from` now.
    fn pull(&self, to: &Side, from: &Side) -> anyhow::Result<usize> {
        self.rt.block_on(to.node.sync_now(&from.id()))
    }

    fn close(self) {
        self.rt.block_on(async {
            self.a.node.close().await;
            self.b.node.close().await;
        });
    }
}

/// Library sync does not run on its own in these tests (each pulls explicitly).
fn manual() -> NodeOptions {
    NodeOptions {
        sync_every: Duration::ZERO,
        ..NodeOptions::offline()
    }
}

#[test]
fn a_tag_added_on_one_device_appears_on_the_other_within_one_pull() {
    let t = two(&[], &[], manual());
    t.on();
    let work = t.a.lib.create_tag("Work", Some("#e5484d"), None).unwrap();
    t.a.lib.create_tag("Client", None, Some(work)).unwrap();
    let events = t.b.node.events();
    assert_eq!(t.pull(&t.b, &t.a).unwrap(), 2);
    let on_b = t.b.tag("Work").expect("arrived");
    assert_eq!(on_b.color.as_deref(), Some("#e5484d"));
    assert_eq!(t.b.tag("Client").unwrap().parent, Some(on_b.id));
    assert!(events
        .try_iter()
        .any(|e| matches!(e, NetEvent::LibrarySynced { applied: 2, .. })));
    // Nothing new: nothing applied, but the time of the pull is kept.
    assert_eq!(t.pull(&t.b, &t.a).unwrap(), 0);
    let peers = t.b.lib.sync_peers().unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].device, t.a.node.id().to_string());
    assert!(peers[0].last_sync.is_some());
    t.close();
}

#[test]
fn both_devices_recolor_one_tag_and_converge() {
    let t = two(&[], &[], manual());
    t.on();
    let on_a = t.a.lib.create_tag("Shared", None, None).unwrap();
    t.pull(&t.b, &t.a).unwrap();
    let on_b = t.b.tag("Shared").unwrap().id;
    t.a.lib.recolor_tag(on_a, Some("#ff0000")).unwrap();
    t.b.lib.recolor_tag(on_b, Some("#0000ff")).unwrap();
    t.pull(&t.b, &t.a).unwrap();
    t.pull(&t.a, &t.b).unwrap();
    let (ca, cb) = (
        t.a.tag("Shared").unwrap().color,
        t.b.tag("Shared").unwrap().color,
    );
    assert_eq!(ca, cb, "converged");
    assert!(ca.is_some());
    t.close();
}

#[test]
fn a_favorite_on_content_reaches_the_copy_on_the_other_device() {
    let t = two(
        &[("report.pdf", "the same bytes"), ("mine.txt", "a")],
        &[("copy.pdf", "the same bytes"), ("mine.txt", "b")],
        manual(),
    );
    t.on();
    let tag = t.a.lib.create_tag("Taxes", None, None).unwrap();
    t.a.lib
        .set_tag(tag, &[t.a.rec("report.pdf")], true)
        .unwrap();
    t.a.lib
        .set_favorite(&[t.a.rec("report.pdf")], true)
        .unwrap();
    t.pull(&t.b, &t.a).unwrap();
    let copy = t.b.rec("copy.pdf");
    let on_copy = t.b.lib.tags_of(&copy).unwrap();
    assert!(on_copy.contains(&FAVORITES), "{on_copy:?}");
    assert!(on_copy.contains(&t.b.tag("Taxes").unwrap().id));
    assert_eq!(t.b.lib.favorites().unwrap().len(), 1);
    assert!(t.b.lib.tags_of(&t.b.rec("mine.txt")).unwrap().is_empty());
    t.close();
}

#[test]
fn with_the_switch_off_a_device_receives_nothing_and_answers_nothing() {
    let t = two(&[], &[], manual());
    // a syncs with b; b does not sync with a.
    t.a.node.set_sync_peers([t.b.id()]);
    t.a.lib.create_tag("FromA", None, None).unwrap();
    t.b.lib.create_tag("FromB", None, None).unwrap();
    let off = t.pull(&t.b, &t.a).unwrap_err();
    assert!(off.to_string().contains("off"), "{off:#}");
    let refused = t.pull(&t.a, &t.b).unwrap_err();
    assert!(refused.to_string().contains("refused"), "{refused:#}");
    assert!(t.b.tag("FromA").is_none());
    assert!(t.a.tag("FromB").is_none());
    assert!(t.a.lib.sync_peers().unwrap().is_empty());
    // Turned on, it works; turned off again, it stops.
    t.b.node.set_sync_peers([t.a.id()]);
    assert_eq!(t.pull(&t.b, &t.a).unwrap(), 1);
    t.b.node.set_sync_peers([]);
    t.a.lib.create_tag("Later", None, None).unwrap();
    assert!(t.pull(&t.b, &t.a).is_err());
    assert!(t.b.tag("Later").is_none());
    t.close();
}

#[test]
fn a_flood_is_rate_limited() {
    let t = two(
        &[],
        &[],
        NodeOptions {
            sync_rate: 150,
            ..manual()
        },
    );
    t.on();
    for i in 0..400 {
        t.a.lib.create_tag(&format!("t{i:03}"), None, None).unwrap();
    }
    assert_eq!(t.pull(&t.b, &t.a).unwrap(), 150);
    assert_eq!(t.pull(&t.b, &t.a).unwrap(), 0, "the rest waits a minute");
    assert_eq!(t.b.lib.tags().unwrap().len(), 150);
    // Nothing was skipped: the next pull starts where the limit stopped.
    assert_eq!(t.b.lib.sync_since(&t.a.node.id().to_string()).unwrap(), 150);
    t.close();
}

#[test]
fn forgetting_a_device_stops_sync_and_keeps_what_arrived() {
    let t = two(&[], &[], manual());
    t.on();
    t.a.lib.create_tag("Kept", None, None).unwrap();
    t.pull(&t.b, &t.a).unwrap();
    t.b.node.forget_peer(&t.a.id()).unwrap();
    assert!(t.b.node.sync_peers().is_empty());
    t.a.lib.create_tag("After", None, None).unwrap();
    assert!(t.pull(&t.b, &t.a).is_err());
    // b no longer answers a either (a is not paired there any more).
    assert!(t.pull(&t.a, &t.b).is_err());
    assert!(t.b.tag("Kept").is_some(), "what arrived stays");
    assert!(t.b.tag("After").is_none());
    t.close();
}

#[test]
fn devices_pull_on_their_own_once_sync_is_on() {
    let quick = NodeOptions {
        sync_every: Duration::from_millis(200),
        ..NodeOptions::offline()
    };
    let t = two(&[], &[], quick);
    t.a.lib.create_tag("Auto", None, None).unwrap();
    t.on();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while t.b.tag("Auto").is_none() {
        assert!(std::time::Instant::now() < deadline, "never pulled");
        std::thread::sleep(Duration::from_millis(50));
    }
    t.close();
}
