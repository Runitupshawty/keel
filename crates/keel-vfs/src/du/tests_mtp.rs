//! Portable device scans against a mock device (no phone needed), and one live scan of the
//! first connected device (`--ignored`).

use super::mtp::{self, Backend, ContentKind, RawObject, Storage, WpdTime};
use super::walk::{refresh_with, scan_with};
use super::*;
use anyhow::Context;
use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;

type Hook = Box<dyn Fn(&str) + Send + Sync>;

#[derive(Clone, Default)]
struct MockDevice {
    id: String,
    name: String,
    storages: Vec<Storage>,
    /// Children by parent object id.
    objects: HashMap<String, Vec<RawObject>>,
    /// Parents whose listing fails.
    failing: Vec<String>,
}

#[derive(Default)]
struct Mock {
    devices: Mutex<Vec<MockDevice>>,
    /// Called with each parent before it is listed.
    hook: Option<Hook>,
    opens: AtomicUsize,
}

struct MockSession<'a> {
    mock: &'a Mock,
    dev: MockDevice,
}

impl Backend for Mock {
    fn devices(&self) -> anyhow::Result<Vec<mtp::Device>> {
        Ok((self.devices.lock().iter())
            .map(|d| mtp::Device {
                id: d.id.clone(),
                name: d.name.clone(),
                storages: d.storages.clone(),
            })
            .collect())
    }

    fn open<'a>(&'a self, device_id: &str) -> anyhow::Result<Box<dyn mtp::Session + 'a>> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        let dev = (self.devices.lock().iter())
            .find(|d| d.id == device_id)
            .cloned()
            .context("no such device")?;
        Ok(Box::new(MockSession { mock: self, dev }))
    }
}

impl mtp::Session for MockSession<'_> {
    fn name(&mut self) -> String {
        self.dev.name.clone()
    }

    fn storage(&mut self, storage_id: &str) -> anyhow::Result<Storage> {
        (self.dev.storages.iter())
            .find(|s| s.id == storage_id)
            .cloned()
            .context("no such storage")
    }

    fn children(
        &mut self,
        parent: &str,
        stop: &dyn Fn() -> bool,
    ) -> anyhow::Result<Vec<RawObject>> {
        if let Some(h) = &self.mock.hook {
            h(parent);
        }
        anyhow::ensure!(
            !self.dev.failing.iter().any(|f| f == parent),
            "the device did not answer"
        );
        let mut out = Vec::new();
        for o in self.dev.objects.get(parent).into_iter().flatten() {
            if stop() {
                break;
            }
            out.push(o.clone());
        }
        Ok(out)
    }
}

fn obj(id: &str, name: &str, kind: ContentKind, size: Option<u64>) -> RawObject {
    RawObject {
        id: id.into(),
        name: Some(name.into()),
        original_name: None,
        kind,
        size,
        modified: None,
        created: None,
        hidden: false,
        system: false,
    }
}

fn folder(id: &str, name: &str) -> RawObject {
    obj(id, name, ContentKind::Folder, None)
}

fn file(id: &str, name: &str, size: u64) -> RawObject {
    obj(id, name, ContentKind::File, Some(size))
}

/// "Phone" with one storage "Internal storage" (1000 bytes, 100 free):
/// `DCIM/IMG_1.JPG` (300), `DCIM/clip.mp4` (50), `notes.txt` (7), an empty `Music`.
fn phone() -> MockDevice {
    let mut img = file("o1", "IMG_1", 300);
    img.original_name = Some("IMG_1.JPG".into());
    img.modified = Some(WpdTime::Ole(25_570.5));
    let mut clip = file("o2", "clip.mp4", 50);
    clip.modified = Some(WpdTime::FileTime(116_444_736_000_000_000 + 10_000_000));
    let mut notes = file("o3", "notes.txt", 7);
    notes.hidden = true;
    notes.system = true;
    // No content type and no size: a folder.
    let music = obj("f2", "Music", ContentKind::Unknown, None);
    MockDevice {
        id: "dev-1".into(),
        name: "Phone".into(),
        storages: vec![Storage {
            id: "s1".into(),
            name: "Internal storage".into(),
            space: Some(Space {
                total: 1000,
                free: 100,
            }),
        }],
        objects: HashMap::from([
            ("s1".into(), vec![folder("f1", "DCIM"), notes, music]),
            ("f1".into(), vec![img, clip]),
        ]),
        failing: Vec::new(),
    }
}

fn mock(devices: Vec<MockDevice>) -> Mock {
    Mock {
        devices: Mutex::new(devices),
        ..Mock::default()
    }
}

fn phone_root() -> Root {
    Root::Mtp {
        device_id: "dev-1".into(),
        storage_id: "s1".into(),
    }
}

fn scan_mock(m: &Mock, opts: &ScanOptions) -> anyhow::Result<Tree> {
    scan_with(&[phone_root()], opts, &ScanHandle::new(), m)
}

fn child(t: &Tree, parent: NodeId, name: &str) -> NodeId {
    t.children(parent)
        .find(|&c| &*t.node(c).name == name)
        .unwrap_or_else(|| panic!("{name} not below {}", t.node(parent).name))
}

#[test]
fn device_objects_become_tree_items() {
    let m = mock(vec![phone()]);
    let t = scan_mock(&m, &ScanOptions::default()).unwrap();
    assert!(t.info.complete && !t.info.timed_out);
    assert_eq!(t.roots().len(), 1);
    let r = &t.roots()[0];
    assert_eq!(r.path, PathBuf::from("mtp://Phone/Internal storage"));
    assert_eq!(
        r.device,
        Some(DeviceRoot {
            device_id: "dev-1".into(),
            storage_id: "s1".into()
        })
    );
    let root = t.node(r.node);
    assert_eq!(
        (root.size, root.allocated, root.files, root.folders),
        (357, 357, 3, 2)
    );
    assert!(t.on_device(r.node));
    let dcim = child(&t, r.node, "DCIM");
    assert_eq!(t.node(dcim).kind, NodeKind::Dir);
    assert_eq!((t.node(dcim).size, t.node(dcim).files), (350, 2));
    // The original file name (with its extension) wins over the object name.
    let img = child(&t, dcim, "IMG_1.JPG");
    assert_eq!(t.ext(img), "jpg");
    assert_eq!(t.node(img).modified, 86_400_000_000_000 * 3 / 2);
    let clip = child(&t, dcim, "clip.mp4");
    assert_eq!(t.node(clip).modified, 1_000_000_000);
    assert_eq!(t.node(dcim).newest, 86_400_000_000_000 * 3 / 2);
    let notes = child(&t, r.node, "notes.txt");
    assert_eq!(t.node(notes).attributes, attr::HIDDEN | attr::SYSTEM);
    let music = child(&t, r.node, "Music");
    assert_eq!(t.node(music).kind, NodeKind::Dir);
    assert_eq!(t.node(music).attributes, attr::DIRECTORY);
    // Free and unknown space from the storage's capacity.
    let pseudo = |k: NodeKind| {
        let id = t.children(r.node).find(|&c| t.node(c).kind == k).unwrap();
        t.node(id).size
    };
    assert_eq!(pseudo(NodeKind::Free), 100);
    assert_eq!(pseudo(NodeKind::Unknown), 900 - 357);
    // Paths are display paths below the root, and find them again.
    let p = t.path(img).unwrap();
    assert_eq!(t.find(&p), Some(img));
    assert!(p.starts_with("mtp://Phone/Internal storage"));
}

#[test]
fn names_and_dates_are_mapped() {
    assert_eq!(mtp::clean_name("a/b\\c"), "a_b_c");
    assert_eq!(mtp::clean_name("tab\there"), "tab_here");
    assert_eq!(mtp::clean_name(".."), "__");
    assert_eq!(mtp::clean_name(""), "_");
    assert_eq!(mtp::clean_name(&"x".repeat(300)).len(), 255);
    assert_eq!(WpdTime::Ole(25_569.0).unix_ns(), 0);
    assert_eq!(WpdTime::Ole(0.0).unix_ns(), UNKNOWN_TIME);
    assert_eq!(WpdTime::Ole(f64::NAN).unix_ns(), UNKNOWN_TIME);
    assert_eq!(WpdTime::Ole(1e9).unix_ns(), UNKNOWN_TIME);
    assert_eq!(WpdTime::FileTime(0).unix_ns(), UNKNOWN_TIME);
    assert_eq!(WpdTime::FileTime(116_444_736_000_000_000).unix_ns(), 0);
    // Before 1970.
    assert_eq!(
        WpdTime::FileTime(116_444_735_000_000_000).unix_ns(),
        -100_000_000_000
    );

    // No usable name: the object id; the creation date when there is no change date.
    let mut o = obj("o9", "  ", ContentKind::File, Some(5));
    o.created = Some(WpdTime::Ole(25_570.0));
    let e = o.entry();
    assert_eq!(e.name, "o9");
    assert!(!e.folder);
    assert_eq!((e.size, e.modified), (5, 86_400_000_000_000));
    // A folder's stated size is not its size; an unknown type with a size is a file.
    assert_eq!(
        obj("f", "F", ContentKind::Folder, Some(4096)).entry().size,
        0
    );
    assert!(!obj("x", "X", ContentKind::Unknown, Some(1)).entry().folder);
    assert!(obj("y", "Y", ContentKind::Functional, None).entry().folder);
}

#[test]
fn devices_and_folders_scan_together() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.bin"), [0u8; 10]).unwrap();
    let mut twin = phone();
    twin.id = "dev-2".into();
    let m = mock(vec![phone(), twin]);
    let roots = [
        Root::Path(tmp.path().to_owned()),
        phone_root(),
        Root::Mtp {
            device_id: "dev-2".into(),
            storage_id: "s1".into(),
        },
        // Named twice: scanned once.
        phone_root(),
    ];
    let t = scan_with(&roots, &ScanOptions::default(), &ScanHandle::new(), &m).unwrap();
    assert_eq!(t.roots().len(), 3);
    assert!(t.roots()[0].device.is_none());
    // Two alike devices keep paths of their own.
    assert_eq!(
        t.roots()[2].path,
        PathBuf::from("mtp://Phone/Internal storage (2)")
    );
    assert_eq!(t.node(TOP).size, 10 + 357 * 2);
    assert!(!t.on_device(t.roots()[0].node));
    assert_eq!(
        t.scan_roots()[1..],
        [
            phone_root(),
            Root::Mtp {
                device_id: "dev-2".into(),
                storage_id: "s1".into()
            }
        ]
    );
}

#[test]
fn a_device_scan_can_be_cancelled() {
    let handle = ScanHandle::new();
    let h = handle.clone();
    let m = Mock {
        hook: Some(Box::new(move |parent| {
            if parent == "f1" {
                h.cancel();
            }
        })),
        ..mock(vec![phone()])
    };
    let t = scan_with(&[phone_root()], &ScanOptions::default(), &handle, &m).unwrap();
    assert!(!t.info.complete);
    assert!(!t.info.timed_out);
    let r = t.roots()[0].node;
    let dcim = child(&t, r, "DCIM");
    assert!(t.node(dcim).has(flag::INCOMPLETE));
    assert_eq!(t.children(dcim).count(), 0);
    assert!(t.node(child(&t, r, "Music")).has(flag::INCOMPLETE));
    assert_eq!(t.node(r).size, 7);
}

#[test]
fn a_device_scan_stops_at_its_time_limit() {
    let m = Mock {
        hook: Some(Box::new(|parent| {
            if parent == "s1" {
                std::thread::sleep(Duration::from_millis(1100));
            }
        })),
        ..mock(vec![phone()])
    };
    let opts = ScanOptions {
        device_time_limit_secs: 1,
        ..ScanOptions::default()
    };
    let t = scan_mock(&m, &opts).unwrap();
    assert!(t.info.timed_out);
    assert!(!t.info.complete);
    let r = t.roots()[0].node;
    assert!(t.node(r).has(flag::INCOMPLETE));
    assert_eq!(t.node(r).items, 0);
}

#[test]
fn device_errors() {
    // Not connected, or the storage is gone: the scan fails before walking.
    let m = mock(vec![phone()]);
    let gone = Root::Mtp {
        device_id: "dev-9".into(),
        storage_id: "s1".into(),
    };
    let e = scan_with(&[gone], &ScanOptions::default(), &ScanHandle::new(), &m).unwrap_err();
    assert!(format!("{e:#}").contains("not connected"), "{e:#}");
    let no_storage = Root::Mtp {
        device_id: "dev-1".into(),
        storage_id: "s7".into(),
    };
    let e = scan_with(
        &[no_storage],
        &ScanOptions::default(),
        &ScanHandle::new(),
        &m,
    )
    .unwrap_err();
    assert!(format!("{e:#}").contains("no longer"), "{e:#}");

    // A folder that cannot be listed is flagged, the rest is scanned.
    let mut dev = phone();
    dev.failing.push("f1".into());
    let t = scan_mock(&mock(vec![dev]), &ScanOptions::default()).unwrap();
    assert!(t.info.complete);
    assert_eq!(t.info.unreadable, 1);
    let r = t.roots()[0].node;
    assert!(t.node(child(&t, r, "DCIM")).has(flag::UNREADABLE));
    assert_eq!(t.node(r).size, 7);

    // A device that lists a folder inside itself does not loop.
    let mut dev = phone();
    dev.objects
        .get_mut("f1")
        .unwrap()
        .push(folder("f1", "DCIM"));
    dev.objects
        .get_mut("f1")
        .unwrap()
        .push(folder("s1", "Loop"));
    let t = scan_mock(&mock(vec![dev]), &ScanOptions::default()).unwrap();
    assert!(t.info.complete);
    let dcim = child(&t, t.roots()[0].node, "DCIM");
    assert_eq!(t.children(dcim).count(), 2);

    // The system's devices: a device that is not there cannot be opened (and none
    // anywhere but Windows).
    let e = scan(
        &[Root::Mtp {
            device_id: "no-such-device".into(),
            storage_id: "s1".into(),
        }],
        &ScanOptions::default(),
        &ScanHandle::new(),
    );
    assert!(e.is_err());
    #[cfg(not(windows))]
    assert!(mtp::devices().unwrap().is_empty());
}

#[test]
fn filters_apply_on_devices() {
    let opts = ScanOptions {
        filters: Filters {
            exclude_names: vec!["*.mp4".into()],
            exclude_hidden: true,
            ..Filters::default()
        },
        ..ScanOptions::default()
    };
    let t = scan_mock(&mock(vec![phone()]), &opts).unwrap();
    assert_eq!(t.info.excluded, 2);
    let r = t.roots()[0].node;
    assert_eq!((t.node(r).size, t.node(r).files), (300, 1));
}

#[test]
fn device_branches_refresh() {
    let m = mock(vec![phone()]);
    let mut t = scan_mock(&m, &ScanOptions::default()).unwrap();
    let r = t.roots()[0].node;
    let dcim = child(&t, r, "DCIM");
    // A new photo, and less free space.
    {
        let mut devs = m.devices.lock();
        devs[0]
            .objects
            .get_mut("f1")
            .unwrap()
            .push(file("o4", "IMG_2.JPG", 1000));
        devs[0].storages[0].space = Some(Space {
            total: 1000,
            free: 50,
        });
    }
    let h = ScanHandle::new();
    refresh_with(&mut t, dcim, &h, &m).unwrap();
    assert_eq!(t.node(dcim).files, 3);
    assert_eq!(t.node(r).size, 1357);
    assert!(h.progress().done);
    // A file refreshes its folder.
    let clip = child(&t, dcim, "clip.mp4");
    m.devices.lock()[0]
        .objects
        .get_mut("f1")
        .unwrap()
        .retain(|o| o.id != "o2");
    refresh_with(&mut t, clip, &ScanHandle::new(), &m).unwrap();
    assert_eq!(t.node(dcim).files, 2);
    assert_eq!(t.node(r).size, 1307);
    // Free space asks the device again.
    let free = (t.children(r)).find(|&c| t.node(c).kind == NodeKind::Free);
    refresh_with(&mut t, free.unwrap(), &ScanHandle::new(), &m).unwrap();
    assert_eq!(t.roots()[0].space.unwrap().free, 50);
    // A folder gone from the device is removed.
    m.devices.lock()[0]
        .objects
        .get_mut("s1")
        .unwrap()
        .retain(|o| o.id != "f1");
    refresh_with(&mut t, dcim, &ScanHandle::new(), &m).unwrap();
    assert_eq!(t.node(dcim).kind, NodeKind::Dead);
    assert_eq!(t.node(r).size, 7);
    // The top item scans the device again.
    refresh_with(&mut t, TOP, &ScanHandle::new(), &m).unwrap();
    assert_eq!(t.roots()[0].device.as_ref().unwrap().device_id, "dev-1");
    assert_eq!(t.node(TOP).size, 7);
    // The device unplugged: refreshing says so and keeps the tree.
    m.devices.lock().clear();
    let r = t.roots()[0].node;
    assert!(refresh_with(&mut t, r, &ScanHandle::new(), &m).is_err());
    assert_eq!(t.node(TOP).size, 7);
}

#[test]
fn device_roots_survive_save_and_load() {
    let t = scan_mock(&mock(vec![phone()]), &ScanOptions::default()).unwrap();
    let mut buf = Vec::new();
    save_to(&t, &mut buf, false).unwrap();
    let back = load_from(&buf[..]).unwrap();
    assert_eq!(back.roots()[0].device, t.roots()[0].device);
    assert_eq!(back.roots()[0].space, t.roots()[0].space);
    assert_eq!(back.node(TOP).size, 357);
    assert_eq!(back.scan_roots(), vec![phone_root()]);
    let json = String::from_utf8(buf).unwrap();
    assert!(!json.contains("timed_out"));
}

#[test]
fn the_root_picker_lists_drives_and_device_storages() {
    let mut cam = phone();
    cam.id = "dev-2".into();
    cam.name = "Camera".into();
    cam.storages.push(Storage {
        id: "s2".into(),
        name: "SD card".into(),
        space: None,
    });
    let m = mock(vec![phone(), cam]);
    let roots = scannable_roots_with(&m);
    let drives = roots
        .iter()
        .filter(|r| r.kind == ScannableKind::Drive)
        .count();
    assert_eq!(drives, local_drives().len());
    let devices: Vec<(&str, &Root)> = (roots.iter())
        .filter(|r| r.kind == ScannableKind::Device)
        .map(|r| (r.label.as_str(), &r.root))
        .collect();
    assert_eq!(devices.len(), 3);
    assert_eq!(devices[0], ("Phone: Internal storage", &phone_root()));
    assert_eq!(devices[2].0, "Camera: SD card");
    assert_eq!(
        *devices[2].1,
        Root::Mtp {
            device_id: "dev-2".into(),
            storage_id: "s2".into()
        }
    );
}

/// Lists the first connected device's first storage (Windows, a phone set to file transfer
/// and unlocked, or a camera). `cargo test -p keel-vfs live_device_scan -- --ignored
/// --nocapture`.
#[test]
#[ignore = "needs a connected phone or camera"]
fn live_device_scan() {
    let devices = mtp::devices().unwrap();
    for d in &devices {
        eprintln!("device {:?}: {} storage(s)", d.name, d.storages.len());
    }
    let Some(d) = devices.iter().find(|d| !d.storages.is_empty()) else {
        eprintln!("no portable device with a storage is connected");
        return;
    };
    let opts = ScanOptions {
        device_time_limit_secs: 120,
        ..ScanOptions::default()
    };
    let root = Root::Mtp {
        device_id: d.id.clone(),
        storage_id: d.storages[0].id.clone(),
    };
    let t = scan(&[root], &opts, &ScanHandle::new()).unwrap();
    let s = t.summary(10);
    eprintln!(
        "{}: {} bytes, {} files, {} folders in {} ms (complete: {}, timed out: {})",
        s.roots[0].path, s.bytes, s.files, s.folders, s.elapsed_ms, s.complete, t.info.timed_out
    );
    for c in &s.roots[0].children {
        eprintln!("  {:>14}  {}", c.bytes, c.name);
    }
    assert_eq!(t.roots().len(), 1);
    assert!(t.on_device(t.roots()[0].node));
}
