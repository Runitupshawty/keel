use super::*;
use crate::index::tests::{id_of, walk, write};
use crate::library::tests::folder;
use crate::{
    validate_preview_execute, IntegrityResult, JobStatus, Op, SourceDef, Warning, INTEGRITY_EVERY,
};
use std::path::Path;

/// A library over `roots` (one folder source each), walked and hashed.
fn library(data: &Path, roots: &[&Path]) -> (Library, Vec<Arc<Source>>) {
    let lib = Library::open(data, "p").unwrap();
    lib.set_pause_on_battery(false);
    lib.set_hash_after_walk(false);
    let sources: Vec<Arc<Source>> = roots
        .iter()
        .enumerate()
        .map(|(i, root)| {
            let id = lib.add_source(folder(&format!("s{i}"), root)).unwrap();
            let src = lib.source(&id).unwrap();
            walk(&src, &lib.router()).unwrap();
            src
        })
        .collect();
    let id = lib.hash().unwrap();
    assert_eq!(lib.jobs().wait(id).unwrap().status, JobStatus::Done);
    (lib, sources)
}

fn rec(src: &Source, rel: &str) -> RecordRef {
    RecordRef {
        source: src.id.clone(),
        id: id_of(src, rel).unwrap(),
    }
}

/// Puts `src` on volume `vol` in failure domain `domain` (as if seen there).
fn put_on(lib: &Library, src: &Source, vol: &str, domain: &str) {
    let c = lib.shared.db.get().unwrap();
    c.execute(
        "INSERT INTO volume(id, label, kind, domain, last_seen) VALUES (?1, ?1, 'fixed', ?2, 1)
         ON CONFLICT(id) DO UPDATE SET domain = excluded.domain",
        params![vol, domain],
    )
    .unwrap();
    c.execute(
        "UPDATE source SET volume_id = ?2 WHERE id = ?1",
        params![src.id.0, vol],
    )
    .unwrap();
    *src.volume_id.write() = Some(vol.into());
}

fn vol(id: &str, domain: &str, state: VolumeState, backup: bool) -> Volume {
    Volume {
        id: id.into(),
        label: id.into(),
        kind: VolumeKind::Fixed,
        failure_domain: domain.into(),
        domain_set: false,
        state,
        last_seen: 0,
        backup,
        capacity: None,
    }
}

#[test]
fn tally_counts_files_and_domains_once() {
    use VolumeState::*;
    let (c, d1, d1b, e) = (
        vol("C:", "disk:1", Online, false),
        vol("D:", "disk:1", Online, false),
        vol("E:", "disk:1", Online, true),
        vol("F:", "disk:2", Offline, false),
    );
    // Two partitions of one disk: two files, one domain.
    assert_eq!(tally([("a", &c), ("b", &d1)]), (2, 1, false, 0));
    // A hard link (same file key) is one file.
    assert_eq!(tally([("a", &c), ("a", &c)]), (1, 1, false, 0));
    // A backup volume on the same disk is not a backup.
    assert_eq!(tally([("a", &c), ("b", &d1b)]), (2, 1, false, 0));
    // Another disk, offline: counted, flagged; now backed up.
    assert_eq!(tally([("a", &c), ("b", &d1b), ("c", &e)]), (3, 2, true, 1));
    // Lost and retired volumes do not count; archived ones count, offline.
    let lost = vol("G:", "disk:3", Lost, true);
    let archived = vol("H:", "disk:4", Archived, false);
    assert_eq!(tally([("a", &c), ("x", &lost)]), (1, 1, false, 0));
    assert_eq!(tally([("a", &c), ("y", &archived)]), (2, 2, false, 1));
}

#[test]
fn redundancy_by_failure_domain_offline_and_backup() {
    let files = tempfile::tempdir().unwrap();
    let (a, b) = (files.path().join("a"), files.path().join("b"));
    write(&a.join("x.txt"), "same");
    write(&a.join("x2.txt"), "same");
    write(&b.join("x.txt"), "same");
    write(&a.join("solo.txt"), "solo");
    let data = tempfile::tempdir().unwrap();
    let (lib, s) = library(data.path(), &[&a, &b]);

    // Both folders really are on one volume: three files, one domain.
    let r = lib.redundancy(&rec(&s[0], "x.txt")).unwrap();
    assert_eq!((r.copies, r.failure_domains, r.offline_copies), (3, 1, 0));
    assert_eq!(r.locations.len(), 3);
    assert_eq!(lib.volumes().unwrap().len(), 1);
    assert_eq!(r.locations[0].volume.state, VolumeState::Online);
    assert!(!r.locations[0].volume.failure_domain.is_empty());

    // b on another disk: two domains.
    put_on(&lib, &s[1], "vol-b", "disk:B");
    let r = lib.redundancy(&rec(&s[0], "x.txt")).unwrap();
    assert_eq!((r.copies, r.failure_domains, r.backed_up), (3, 2, false));
    let copies = lib.record_copies(&cas_of(&s[0], "x.txt")).unwrap();
    assert_eq!(copies.len(), 3);
    assert_eq!(copies[2].1.id, "vol-b");

    // b unplugged: its copy still counts, flagged offline.
    std::fs::rename(&b, files.path().join("away")).unwrap();
    lib.refresh_status().join().unwrap();
    let r = lib.redundancy(&rec(&s[0], "x.txt")).unwrap();
    assert_eq!((r.copies, r.failure_domains, r.offline_copies), (3, 2, 1));
    let vb = lib
        .volumes()
        .unwrap()
        .into_iter()
        .find(|v| v.id == "vol-b")
        .unwrap();
    assert_eq!(vb.state, VolumeState::Offline);

    // Backup: a copy on a backup volume in another domain.
    lib.set_backup("vol-b", true).unwrap();
    assert!(lib.redundancy(&rec(&s[0], "x.txt")).unwrap().backed_up);
    assert!(!lib.redundancy(&rec(&s[0], "solo.txt")).unwrap().backed_up);
    // Archived: still counted and offline; lost: gone.
    lib.set_volume_state("vol-b", VolumeState::Archived)
        .unwrap();
    let r = lib.redundancy(&rec(&s[0], "x.txt")).unwrap();
    assert_eq!((r.copies, r.offline_copies, r.backed_up), (3, 1, true));
    lib.set_volume_state("vol-b", VolumeState::Lost).unwrap();
    let r = lib.redundancy(&rec(&s[0], "x.txt")).unwrap();
    assert_eq!((r.copies, r.failure_domains, r.backed_up), (2, 1, false));
    assert_eq!(r.locations.len(), 3, "listed, not counted");
    // Online hands it back to its sources' status.
    lib.set_volume_state("vol-b", VolumeState::Online).unwrap();
    let vb = lib
        .volumes()
        .unwrap()
        .into_iter()
        .find(|v| v.id == "vol-b")
        .unwrap();
    assert_eq!(vb.state, VolumeState::Offline);
    assert!(lib.set_backup("nope", true).is_err());
}

fn cas_of(src: &Source, rel: &str) -> Vec<u8> {
    src.store
        .get()
        .unwrap()
        .query_row(
            "SELECT cas_id FROM record WHERE id = ?1",
            [id_of(src, rel).unwrap()],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn summary_counters_after_walk_and_hash() {
    let files = tempfile::tempdir().unwrap();
    let (a, b) = (files.path().join("a"), files.path().join("b"));
    write(&a.join("alpha.txt"), "alpha");
    write(&a.join("same.txt"), "same");
    write(&b.join("same.txt"), "same");
    let data = tempfile::tempdir().unwrap();
    let lib = Library::open(data.path(), "p").unwrap();
    lib.set_pause_on_battery(false);
    assert_eq!(
        lib.protection_summary().unwrap(),
        ProtectionSummary::default()
    );
    for (i, root) in [&a, &b].into_iter().enumerate() {
        let id = lib.add_source(folder(&format!("s{i}"), root)).unwrap();
        // The index job sees the volume and schedules hashing.
        let job = lib.index(&id).unwrap();
        assert_eq!(lib.jobs().wait(job).unwrap().status, JobStatus::Done);
    }
    let id = lib.hash().unwrap();
    assert_eq!(lib.jobs().wait(id).unwrap().status, JobStatus::Done);
    let s = lib.protection_summary().unwrap();
    assert_eq!(
        (
            s.single_copy,
            s.single_domain,
            s.unbacked,
            s.drifted,
            s.offline_volumes
        ),
        (1, 1, 2, 0, 0)
    );
    // One temp volume, its capacity known.
    assert_eq!(s.capacity.len(), 1);
    let (v, used, total) = &s.capacity[0];
    assert!(*total > 0 && used <= total);
    assert_eq!(v.kind, VolumeKind::Fixed);
    assert!(v.last_seen > 0);
    // A backup on the same disk changes nothing; b on its own backup disk backs up "same".
    lib.set_backup(&v.id, true).unwrap();
    assert_eq!(lib.protection_summary().unwrap().unbacked, 2);
    let sb = lib.source(&lib.sources()[1].id).unwrap();
    put_on(&lib, &sb, "vol-b", "disk:B");
    lib.set_backup("vol-b", true).unwrap();
    let s = lib.protection_summary().unwrap();
    assert_eq!((s.single_copy, s.single_domain, s.unbacked), (1, 0, 1));
}

#[test]
fn integrity_marks_drift_when_bytes_change_under_the_same_metadata() {
    let files = tempfile::tempdir().unwrap();
    let a = files.path();
    write(&a.join("rot.bin"), "0123456789");
    write(&a.join("fine.bin"), "abcdefghij");
    write(&a.join("edited.bin"), "ABCDEFGHIJ");
    let data = tempfile::tempdir().unwrap();
    let (lib, s) = library(data.path(), &[a]);
    let src = &s[0];
    // Bit rot: same size, and the stored mtime and change time made to match again.
    std::fs::write(a.join("rot.bin"), "0123456780").unwrap();
    let now = crate::fsid::stat(&a.join("rot.bin")).unwrap();
    src.store
        .get()
        .unwrap()
        .execute(
            "UPDATE record SET mtime = ?2, ctime = ?3 WHERE id = ?1",
            params![id_of(src, "rot.bin").unwrap(), now.mtime, now.ctime],
        )
        .unwrap();
    // An ordinary edit: metadata moved on, left to the indexer.
    std::fs::write(a.join("edited.bin"), "ABCDEFGHIK").unwrap();

    let job = lib.integrity(None, 100.0).unwrap();
    let info = lib.jobs().wait(job).unwrap();
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    let result: IntegrityResult = serde_json::from_value(info.result.unwrap()).unwrap();
    assert_eq!(
        (result.checked, result.drifted, result.changed),
        (2, 1, 1),
        "{result:?}"
    );
    let drift = |rel: &str| -> Option<i64> {
        src.store
            .get()
            .unwrap()
            .query_row(
                "SELECT drift FROM record WHERE id = ?1",
                [id_of(src, rel).unwrap()],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert!(drift("rot.bin").is_some());
    assert_eq!((drift("fine.bin"), drift("edited.bin")), (None, None));
    assert_eq!(lib.protection_summary().unwrap().drifted, 1);
    // A drifted file no longer holds its content.
    assert!(lib
        .record_copies(&cas_of(src, "rot.bin"))
        .unwrap()
        .is_empty());
    // The next walk sees the edit; the drift mark stays (metadata unchanged).
    walk(src, &lib.router()).unwrap();
    assert!(drift("rot.bin").is_some());
    // Scheduling: nothing runs within the interval after a check.
    assert_eq!(lib.schedule_integrity(1.0, INTEGRITY_EVERY).unwrap(), None);
    assert!(lib
        .schedule_integrity(1.0, std::time::Duration::ZERO)
        .unwrap()
        .is_some());
}

#[test]
fn delete_warnings_follow_failure_domains() {
    let files = tempfile::tempdir().unwrap();
    let (a, b) = (files.path().join("a"), files.path().join("b"));
    write(&a.join("x.txt"), "same");
    write(&a.join("x2.txt"), "same");
    write(&b.join("x.txt"), "same");
    let data = tempfile::tempdir().unwrap();
    let (lib, s) = library(data.path(), &[&a, &b]);
    put_on(&lib, &s[0], "vol-a", "disk:A");
    put_on(&lib, &s[1], "vol-b", "disk:B");
    let warnings = |p: &Path| {
        validate_preview_execute(
            &lib,
            Op::Delete {
                paths: vec![VPath::local(p)],
            },
        )
        .unwrap()
        .warnings
    };
    // b's copy is the only one outside disk A.
    assert_eq!(
        warnings(&b.join("x.txt")),
        [Warning::SingleDomain {
            path: VPath::local(b.join("x.txt")),
            files: 1
        }]
    );
    // One of a's two copies: disk A and disk B keep it.
    assert!(warnings(&a.join("x.txt")).is_empty());
    // b lost: its copy no longer counts, so deleting a's two is deleting the last copies.
    lib.set_volume_state("vol-b", VolumeState::Lost).unwrap();
    assert_eq!(
        warnings(&a),
        [Warning::LastCopy {
            path: VPath::local(&a),
            files: 2
        }]
    );
}

/// A provider that forwards to a cloud account and counts listings.
struct Counting {
    inner: Arc<dyn keel_vfs::Provider>,
    list: std::sync::atomic::AtomicUsize,
    complete: std::sync::atomic::AtomicUsize,
}

impl keel_vfs::Provider for Counting {
    fn scheme(&self) -> &'static str {
        self.inner.scheme()
    }
    fn caps(&self) -> keel_vfs::Caps {
        self.inner.caps()
    }
    fn list(&self, dir: &VPath) -> Result<Vec<keel_vfs::Entry>> {
        self.list.fetch_add(1, Ordering::SeqCst);
        self.inner.list(dir)
    }
    fn list_complete(&self, dir: &VPath) -> Result<Vec<keel_vfs::Entry>> {
        self.complete.fetch_add(1, Ordering::SeqCst);
        self.inner.list_complete(dir)
    }
    fn stat(&self, p: &VPath) -> Result<keel_vfs::Entry> {
        self.inner.stat(p)
    }
    fn read(&self, p: &VPath) -> Result<Box<dyn std::io::Read + Send>> {
        self.inner.read(p)
    }
    fn write(&self, p: &VPath) -> Result<Box<dyn std::io::Write + Send>> {
        self.inner.write(p)
    }
    fn mkdir(&self, p: &VPath) -> Result<()> {
        self.inner.mkdir(p)
    }
    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.inner.rename(from, to)
    }
    fn remove(&self, p: &VPath) -> Result<()> {
        self.inner.remove(p)
    }
    fn remove_kind(&self) -> keel_vfs::RemoveKind {
        self.inner.remove_kind()
    }
    fn local_copy(&self, p: &VPath) -> Result<std::path::PathBuf> {
        self.inner.local_copy(p)
    }
}

#[test]
fn cloud_sources_walk_the_provider_listing_as_their_own_domain() {
    use std::io::Write;
    let op = opendal::Operator::new(opendal::services::Memory::default()).unwrap();
    let account = keel_vfs::CloudAccount {
        id: "mem".into(),
        label: "Memory".into(),
        kind: keel_vfs::CloudKind::S3,
        root: None,
        client_id_override: None,
        s3: None,
        webdav: None,
    };
    let cloud =
        keel_vfs::CloudProvider::with_operator(account, op, crossbeam_channel::unbounded().0)
            .unwrap();
    let counting = Arc::new(Counting {
        inner: Arc::new(cloud),
        list: Default::default(),
        complete: Default::default(),
    });
    let router = Arc::new(keel_vfs::Router::new());
    router.register_cloud_provider("mem".into(), counting.clone());
    let p = |s: &str| VPath::parse(s).unwrap();
    let cloud = router.provider_for(&p("cloud://mem/")).unwrap();
    cloud.mkdir(&p("cloud://mem/docs")).unwrap();
    for (path, data) in [("cloud://mem/a.txt", "a"), ("cloud://mem/docs/b.txt", "bb")] {
        let mut w = cloud.write(&p(path)).unwrap();
        w.write_all(data.as_bytes()).unwrap();
        w.flush().unwrap();
    }

    let data = tempfile::tempdir().unwrap();
    let lib = Library::open(data.path(), "c").unwrap();
    lib.set_router(router);
    let id = lib
        .add_source(SourceDef {
            label: "Mem".into(),
            root: p("cloud://mem/"),
            kind: SourceKind::Cloud,
            include_hidden: false,
            ignore: Vec::new(),
            poll_secs: None,
            hash_shares: false,
        })
        .unwrap();
    let job = lib.index(&id).unwrap();
    let info = lib.jobs().wait(job).unwrap();
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    let names: Vec<String> = lib
        .list_children(&id, "")
        .unwrap()
        .into_iter()
        .map(|h| h.name)
        .collect();
    assert_eq!(names, ["docs", "a.txt"]);
    assert_eq!(lib.list_children(&id, "docs").unwrap()[0].size, 2);
    // Fresh, complete listings only (never a capped, cached `list`).
    assert!(counting.complete.load(Ordering::SeqCst) >= 2);
    assert_eq!(counting.list.load(Ordering::SeqCst), 0);
    // The account is the volume and its own failure domain.
    let v = lib.volumes().unwrap();
    assert_eq!(v.len(), 1);
    assert_eq!(
        (v[0].id.as_str(), v[0].kind, v[0].failure_domain.as_str()),
        ("cloud:mem", VolumeKind::Cloud, "cloud:mem")
    );
    assert_eq!(v[0].state, VolumeState::Online);
}

fn delete_warnings(lib: &Library, p: &Path) -> Vec<Warning> {
    validate_preview_execute(
        lib,
        Op::Delete {
            paths: vec![VPath::local(p)],
        },
    )
    .unwrap()
    .warnings
}

#[test]
fn add_source_refuses_a_root_aliased_through_a_junction() {
    let files = tempfile::tempdir().unwrap();
    let (real, alias) = (files.path().join("real"), files.path().join("alias"));
    write(&real.join("sub/only.txt"), "the only copy of these bytes");
    if !crate::index::tests::dir_link(&real, &alias) {
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let lib = Library::open(data.path(), "p").unwrap();
    lib.add_source(folder("real", &real.join("sub"))).unwrap();
    // Same folder, other path text: refused; so is the folder around it.
    assert!(lib.add_source(folder("same", &alias.join("sub"))).is_err());
    assert!(lib.add_source(folder("around", &alias)).is_err());
    assert_eq!(lib.sources().len(), 1);
}

#[test]
fn walk_refuses_a_root_another_source_holds() {
    let files = tempfile::tempdir().unwrap();
    let (real, alias) = (files.path().join("real"), files.path().join("alias"));
    write(&real.join("sub/only.txt"), "the only copy of these bytes");
    let data = tempfile::tempdir().unwrap();
    let lib = Library::open(data.path(), "p").unwrap();
    lib.set_hash_after_walk(false);
    let a = lib.add_source(folder("a", &real.join("sub"))).unwrap();
    // Added while the alias does not exist yet, so add_source cannot see through it.
    let b = lib.add_source(folder("b", &alias.join("sub"))).unwrap();
    if !crate::index::tests::dir_link(&real, &alias) {
        return;
    }
    let (a, b) = (lib.source(&a).unwrap(), lib.source(&b).unwrap());
    walk(&a, &lib.router()).unwrap();
    let err = walk(&b, &lib.router()).unwrap_err();
    assert!(format!("{err:#}").contains("already indexes"), "{err:#}");
    assert!(matches!(*b.status.read(), crate::SourceStatus::Error(_)));
    assert_eq!(id_of(&b, "only.txt"), None);
}

#[test]
fn deleting_the_only_copy_through_an_alias_warns() {
    let files = tempfile::tempdir().unwrap();
    let (real, alias) = (files.path().join("real"), files.path().join("alias"));
    write(&real.join("sub/only.txt"), "the only copy of these bytes");
    write(&real.join("sub/linked.txt"), "bytes with a hard link");
    let data = tempfile::tempdir().unwrap();
    let lib = Library::open(data.path(), "p").unwrap();
    lib.set_pause_on_battery(false);
    lib.set_hash_after_walk(false);
    // Both added before the alias exists; b walks first, so a's root is no folder of b's
    // and a indexes real/sub again: one physical file, two records with one file key.
    let b = lib.add_source(folder("b", &alias.join("sub"))).unwrap();
    let a = lib.add_source(folder("a", &real)).unwrap();
    if !crate::index::tests::dir_link(&real, &alias) {
        return;
    }
    std::fs::hard_link(real.join("sub/linked.txt"), real.join("link2.txt")).unwrap();
    for id in [&b, &a] {
        walk(&lib.source(id).unwrap(), &lib.router()).unwrap();
    }
    let id = lib.hash().unwrap();
    assert_eq!(lib.jobs().wait(id).unwrap().status, JobStatus::Done);
    for p in [alias.join("sub/only.txt"), real.join("sub/only.txt")] {
        assert_eq!(
            delete_warnings(&lib, &p),
            [Warning::LastCopy {
                path: VPath::local(&p),
                files: 1
            }],
            "{}",
            p.display()
        );
    }
    // A real hard link outside the deletion (another path) still keeps the content.
    assert!(delete_warnings(&lib, &alias.join("sub/linked.txt")).is_empty());
}

#[test]
fn drifted_files_warn_on_delete_and_are_no_duplicates() {
    let files = tempfile::tempdir().unwrap();
    let a = files.path();
    write(&a.join("x.txt"), "same bytes");
    write(&a.join("y.txt"), "same bytes");
    let data = tempfile::tempdir().unwrap();
    let (lib, s) = library(data.path(), &[a]);
    assert_eq!(lib.duplicates(0).unwrap().len(), 1);
    // As `IntegrityJob` marks it: y's bytes are no longer its content id's.
    s[0].store
        .get()
        .unwrap()
        .execute("UPDATE record SET drift = 1 WHERE path = 'y.txt'", [])
        .unwrap();
    assert!(lib.duplicates(0).unwrap().is_empty());
    assert_eq!(
        delete_warnings(&lib, &a.join("y.txt")),
        [Warning::ContentUnverified {
            path: VPath::local(a.join("y.txt")),
            files: 1
        }]
    );
    // y no longer holds x's content: x is its last copy.
    assert_eq!(
        delete_warnings(&lib, &a.join("x.txt")),
        [Warning::LastCopy {
            path: VPath::local(a.join("x.txt")),
            files: 1
        }]
    );
}

#[test]
fn deleting_warns_when_the_copies_left_are_all_offline() {
    let files = tempfile::tempdir().unwrap();
    let (a, b) = (files.path().join("a"), files.path().join("b"));
    write(&a.join("x.txt"), "same");
    write(&b.join("x.txt"), "same");
    let data = tempfile::tempdir().unwrap();
    let (lib, s) = library(data.path(), &[&a, &b]);
    put_on(&lib, &s[0], "vol-a", "disk:A");
    put_on(&lib, &s[1], "vol-b", "disk:A");
    assert!(delete_warnings(&lib, &a.join("x.txt")).is_empty());
    lib.set_volume_state("vol-b", VolumeState::Archived)
        .unwrap();
    assert_eq!(
        delete_warnings(&lib, &a.join("x.txt")),
        [Warning::CopiesOffline {
            path: VPath::local(a.join("x.txt")),
            files: 1
        }]
    );
}

#[test]
fn summary_says_how_many_files_are_not_checked_yet() {
    let files = tempfile::tempdir().unwrap();
    let a = files.path();
    for i in 0..5 {
        write(&a.join(format!("f{i}.txt")), &format!("unique {i}"));
    }
    let data = tempfile::tempdir().unwrap();
    let lib = Library::open(data.path(), "p").unwrap();
    lib.set_hash_after_walk(false);
    let id = lib.add_source(folder("a", a)).unwrap();
    walk(&lib.source(&id).unwrap(), &lib.router()).unwrap();
    lib.recount_protection().unwrap();
    let p = lib.protection_summary().unwrap();
    // Nothing hashed: no file is known to have one copy, and all five are unknown.
    assert_eq!((p.single_copy, p.unchecked), (0, 5));
    lib.set_pause_on_battery(false);
    let job = lib.hash().unwrap();
    assert_eq!(lib.jobs().wait(job).unwrap().status, JobStatus::Done);
    let p = lib.protection_summary().unwrap();
    assert_eq!((p.single_copy, p.unchecked), (5, 0));
}

#[test]
fn failure_domain_set_by_hand_survives_detection_and_resets() {
    let files = tempfile::tempdir().unwrap();
    let (a, b) = (files.path().join("a"), files.path().join("b"));
    write(&a.join("x.txt"), "same");
    write(&b.join("x.txt"), "same");
    let data = tempfile::tempdir().unwrap();
    let (lib, s) = library(data.path(), &[&a, &b]);
    put_on(&lib, &s[0], "vol-a", "disk:A");
    put_on(&lib, &s[1], "vol-b", "disk:B");
    lib.recount_protection().unwrap();
    assert_eq!(lib.protection_summary().unwrap().single_domain, 0);
    // Two names of one server: the user says so.
    lib.set_failure_domain("vol-b", Some("  disk:A ")).unwrap();
    let vb = |lib: &Library| {
        lib.volumes()
            .unwrap()
            .into_iter()
            .find(|v| v.id == "vol-b")
            .unwrap()
    };
    assert_eq!(
        (vb(&lib).failure_domain.as_str(), vb(&lib).domain_set),
        ("disk:A", true)
    );
    assert_eq!(lib.protection_summary().unwrap().single_domain, 1);
    // Detection writes its own column; the one set by hand still wins.
    lib.shared
        .db
        .get()
        .unwrap()
        .execute(
            "UPDATE volume SET domain = 'disk:B2' WHERE id = 'vol-b'",
            [],
        )
        .unwrap();
    assert_eq!(vb(&lib).failure_domain, "disk:A");
    // Reopened: kept.
    drop(s);
    let dir = data.path().to_path_buf();
    drop(lib);
    let lib = Library::open(&dir, "p").unwrap();
    assert_eq!(vb(&lib).failure_domain, "disk:A");
    // Blank: back to the detected one.
    lib.set_failure_domain("vol-b", Some(" ")).unwrap();
    assert_eq!(
        (vb(&lib).failure_domain.as_str(), vb(&lib).domain_set),
        ("disk:B2", false)
    );
    assert!(lib.set_failure_domain("nope", None).is_err());
}

#[test]
fn a_copy_on_an_sftp_host_is_its_own_failure_domain() {
    let files = tempfile::tempdir().unwrap();
    write(&files.path().join("x.txt"), "on both");
    write(&files.path().join("solo.txt"), "here only");
    let data = tempfile::tempdir().unwrap();
    let (lib, s) = library(data.path(), &[files.path()]);
    let mem = Arc::new(keel_vfs::memory::MemoryProvider::new());
    mem.put("/srv/x.txt", "on both");
    mem.put("/srv/y.txt", "server only");
    mem.put("/backup/y.txt", "server only");
    lib.router()
        .register_remote_provider("box".into(), mem.clone());
    let remote = |name: &str, root: &str| {
        let id = lib
            .add_source(crate::library::tests::remote(name, root))
            .unwrap();
        let src = lib.source(&id).unwrap();
        walk(&src, &lib.router()).unwrap();
        src
    };
    let (srv, backup) = (
        remote("srv", "sftp://box/srv"),
        remote("backup", "sftp://box/backup"),
    );
    let id = lib.hash().unwrap();
    assert_eq!(lib.jobs().wait(id).unwrap().status, JobStatus::Done);

    // This PC and the server: two copies in two failure domains.
    let r = lib.redundancy(&rec(&s[0], "x.txt")).unwrap();
    assert_eq!((r.copies, r.failure_domains), (2, 2), "{r:?}");
    let server = &r.locations[1].volume;
    assert_eq!(
        (
            server.id.as_str(),
            server.kind,
            server.failure_domain.as_str()
        ),
        ("sftp:box", VolumeKind::Network, "sftp:box")
    );
    assert_ne!(r.locations[0].volume.failure_domain, "sftp:box");
    // Deleting the PC's copy leaves the server's: not the last copy, one domain left.
    assert!(matches!(
        delete_warnings(&lib, &files.path().join("x.txt"))[..],
        [Warning::SingleDomain { files: 1, .. }]
    ));
    assert!(matches!(
        delete_warnings(&lib, &files.path().join("solo.txt"))[..],
        [Warning::LastCopy { files: 1, .. }]
    ));
    // Two sources on one host: one failure domain.
    let r = lib.redundancy(&rec(&srv, "y.txt")).unwrap();
    assert_eq!((r.copies, r.failure_domains), (2, 1), "{r:?}");
    assert_eq!(lib.redundancy(&rec(&backup, "y.txt")).unwrap().copies, 2);
    // Counters: solo.txt has one copy, y.txt one domain; nothing is left unchecked.
    let p = lib.protection_summary().unwrap();
    assert_eq!(
        (p.single_copy, p.single_domain, p.unchecked),
        (1, 1, 0),
        "{p:?}"
    );
}
