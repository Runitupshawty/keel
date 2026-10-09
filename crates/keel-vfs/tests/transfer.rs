//! `ops::transfer` through the generic streaming path, with the local filesystem standing in
//! for remote hosts: `Mirror` serves `sftp://<id>/...` from a temp folder.
use keel_vfs::{
    ops::transfer, Caps, Conflict, Entry, LocalProvider, Progress, Provider, Router, VPath,
};
use std::{
    cell::RefCell,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

struct Mirror {
    id: String,
    root: PathBuf,
    /// Simulates a dropped connection: reads fail after this many bytes.
    fail_after: Option<u64>,
}
impl Mirror {
    fn local(&self, p: &VPath) -> VPath {
        assert_eq!(p.authority, self.id);
        VPath::local(self.root.join(p.path.trim_start_matches('/')))
    }
}
struct Flaky<R> {
    inner: R,
    left: u64,
}
impl<R: Read> Read for Flaky<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "dropped"));
        }
        let max = buf.len().min(self.left as usize);
        let n = self.inner.read(&mut buf[..max])?;
        self.left -= n as u64;
        Ok(n)
    }
}
impl Provider for Mirror {
    fn scheme(&self) -> &'static str {
        "sftp"
    }
    fn caps(&self) -> Caps {
        LocalProvider.caps()
    }
    fn list(&self, dir: &VPath) -> anyhow::Result<Vec<Entry>> {
        Ok(LocalProvider
            .list(&self.local(dir))?
            .into_iter()
            .map(|e| Entry {
                path: dir.join(&e.name),
                ..e
            })
            .collect())
    }
    fn stat(&self, p: &VPath) -> anyhow::Result<Entry> {
        Ok(Entry {
            path: p.clone(),
            ..LocalProvider.stat(&self.local(p))?
        })
    }
    fn read(&self, p: &VPath) -> anyhow::Result<Box<dyn Read + Send>> {
        let inner = LocalProvider.read(&self.local(p))?;
        Ok(match self.fail_after {
            Some(left) => Box::new(Flaky { inner, left }),
            None => inner,
        })
    }
    fn write(&self, p: &VPath) -> anyhow::Result<Box<dyn Write + Send>> {
        LocalProvider.write(&self.local(p))
    }
    fn create_new(&self, p: &VPath) -> anyhow::Result<Box<dyn Write + Send>> {
        LocalProvider.create_new(&self.local(p))
    }
    fn mkdir(&self, p: &VPath) -> anyhow::Result<()> {
        LocalProvider.mkdir(&self.local(p))
    }
    fn rename(&self, from: &VPath, to: &VPath) -> anyhow::Result<()> {
        LocalProvider.rename(&self.local(from), &self.local(to))
    }
    fn rename_noreplace(&self, from: &VPath, to: &VPath) -> anyhow::Result<()> {
        LocalProvider.rename_noreplace(&self.local(from), &self.local(to))
    }
    fn rename_replace(&self, from: &VPath, to: &VPath) -> anyhow::Result<()> {
        LocalProvider.rename_replace(&self.local(from), &self.local(to))
    }
    fn remove_empty_dir(&self, p: &VPath) -> anyhow::Result<()> {
        LocalProvider.remove_empty_dir(&self.local(p))
    }
    /// Remote delete is permanent (no trash on a remote host).
    fn remove(&self, p: &VPath) -> anyhow::Result<()> {
        let local = self.local(p).to_local_path().unwrap();
        if local.is_dir() {
            fs::remove_dir_all(local)?;
        } else {
            fs::remove_file(local)?;
        }
        Ok(())
    }
    fn local_copy(&self, p: &VPath) -> anyhow::Result<PathBuf> {
        Ok(self.local(p).to_local_path().unwrap())
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    local: PathBuf,
    a: PathBuf,
    b: PathBuf,
    router: Router,
}
fn fixture(fail_after: Option<u64>) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let [local, a, b] = ["local", "a", "b"].map(|n| tmp.path().join(n));
    let router = Router::new();
    for (id, root) in [("a", &a), ("b", &b)] {
        fs::create_dir(root).unwrap();
        router.register_remote_provider(
            id.into(),
            Arc::new(Mirror {
                id: id.into(),
                root: root.clone(),
                fail_after,
            }),
        );
    }
    fs::create_dir(&local).unwrap();
    Fixture {
        _tmp: tmp,
        local,
        a,
        b,
        router,
    }
}
fn remote(id: &str, path: &str) -> VPath {
    VPath::parse(&format!("sftp://{id}/{path}")).unwrap()
}
fn run(f: &Fixture, src: &[VPath], dst: &VPath, mv: bool, c: Conflict) -> anyhow::Result<()> {
    transfer(src, dst, mv, c, &|_| {}, &AtomicBool::new(false), &f.router)
}
/// Every file under `dir`, relative, sorted; fails the test on any staged leftovers.
fn tree(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = walk(dir)
        .into_iter()
        .map(|p| {
            let rel = p
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            assert!(!rel.contains(".keel-partial"), "leftover partial: {rel}");
            rel
        })
        .collect();
    out.sort();
    out
}
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        }
        out.push(p);
    }
    out
}

#[test]
fn copies_trees_local_to_remote_remote_to_local_and_remote_to_remote() {
    let f = fixture(None);
    let src = f.local.join("tree");
    fs::create_dir_all(src.join("sub dir ü/empty")).unwrap();
    fs::write(src.join("a.txt"), b"alpha").unwrap();
    fs::write(
        src.join("sub dir ü/big.bin"),
        vec![7u8; 3 * 1024 * 1024 + 5],
    )
    .unwrap();
    let updates = RefCell::new(Vec::<Progress>::new());
    transfer(
        &[VPath::local(&src)],
        &remote("a", ""),
        false,
        Conflict::Skip,
        &|p| updates.borrow_mut().push(p),
        &AtomicBool::new(false),
        &f.router,
    )
    .unwrap();
    let expected = [
        "tree",
        "tree/a.txt",
        "tree/sub dir ü",
        "tree/sub dir ü/big.bin",
        "tree/sub dir ü/empty",
    ];
    assert_eq!(tree(&f.a), expected);
    let last = updates.borrow().last().cloned().unwrap();
    let total = 5 + 3 * 1024 * 1024 + 5;
    assert_eq!(
        (
            last.done_bytes,
            last.total_bytes,
            last.done_items,
            last.total_items
        ),
        (total, total, 5, 5)
    );
    // remote -> remote (another host), then remote -> local.
    run(
        &f,
        &[remote("a", "tree")],
        &remote("b", ""),
        false,
        Conflict::Skip,
    )
    .unwrap();
    assert_eq!(tree(&f.b), expected);
    let back = f.local.join("back");
    fs::create_dir(&back).unwrap();
    run(
        &f,
        &[remote("b", "tree")],
        &VPath::local(&back),
        false,
        Conflict::Skip,
    )
    .unwrap();
    assert_eq!(
        fs::read(back.join("tree/sub dir ü/big.bin")).unwrap(),
        fs::read(src.join("sub dir ü/big.bin")).unwrap()
    );
    assert_eq!(tree(&f.local).len(), 11);
}

#[test]
fn conflict_modes_skip_overwrite_and_rename() {
    let f = fixture(None);
    let src = f.local.join("report.txt");
    fs::write(&src, b"new").unwrap();
    fs::write(f.a.join("report.txt"), b"old").unwrap();
    let dst = remote("a", "");
    let s = [VPath::local(&src)];
    run(&f, &s, &dst, false, Conflict::Skip).unwrap();
    assert_eq!(fs::read(f.a.join("report.txt")).unwrap(), b"old");
    run(&f, &s, &dst, false, Conflict::RenameNew).unwrap();
    assert_eq!(fs::read(f.a.join("report (2).txt")).unwrap(), b"new");
    run(&f, &s, &dst, false, Conflict::Overwrite).unwrap();
    assert_eq!(fs::read(f.a.join("report.txt")).unwrap(), b"new");
    assert_eq!(tree(&f.a), ["report (2).txt", "report.txt"]);
    // A directory never overwrites a file of the same name (or the reverse).
    fs::create_dir(f.local.join("folder")).unwrap();
    fs::write(f.a.join("folder"), b"file").unwrap();
    assert!(run(
        &f,
        &[VPath::local(f.local.join("folder"))],
        &dst,
        false,
        Conflict::Overwrite
    )
    .is_err());
    assert_eq!(fs::read(f.a.join("folder")).unwrap(), b"file");
}

#[test]
fn cancel_mid_file_keeps_destination_and_source_and_removes_partial() {
    let f = fixture(None);
    let src = f.local.join("big.bin");
    fs::write(&src, vec![1u8; 4 * 1024 * 1024]).unwrap();
    fs::write(f.a.join("big.bin"), b"original").unwrap();
    let cancel = AtomicBool::new(false);
    let err = transfer(
        &[VPath::local(&src)],
        &remote("a", ""),
        true,
        Conflict::Overwrite,
        &|p| {
            if p.done_bytes > 0 {
                cancel.store(true, Ordering::SeqCst);
            }
        },
        &cancel,
        &f.router,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").to_lowercase().contains("cancel"),
        "{err:#}"
    );
    assert_eq!(fs::read(f.a.join("big.bin")).unwrap(), b"original");
    assert_eq!(tree(&f.a), ["big.bin"]);
    assert_eq!(fs::metadata(&src).unwrap().len(), 4 * 1024 * 1024);
}

#[test]
fn dropped_connection_mid_copy_leaves_no_partial_and_keeps_move_source() {
    let f = fixture(Some(1024 * 1024 + 3));
    fs::write(f.a.join("big.bin"), vec![9u8; 3 * 1024 * 1024]).unwrap();
    assert!(run(
        &f,
        &[remote("a", "big.bin")],
        &VPath::local(&f.local),
        true,
        Conflict::Skip
    )
    .is_err());
    assert!(tree(&f.local).is_empty());
    assert_eq!(tree(&f.a), ["big.bin"]);
    assert_eq!(
        fs::metadata(f.a.join("big.bin")).unwrap().len(),
        3 * 1024 * 1024
    );
}

#[test]
fn move_deletes_sources_only_after_success_and_keeps_skipped() {
    let f = fixture(None);
    fs::create_dir_all(f.a.join("dir/inner")).unwrap();
    fs::write(f.a.join("dir/keep.txt"), b"remote").unwrap();
    fs::write(f.a.join("dir/inner/go.txt"), b"go").unwrap();
    fs::create_dir(f.local.join("dir")).unwrap();
    fs::write(f.local.join("dir/keep.txt"), b"local").unwrap();
    run(
        &f,
        &[remote("a", "dir")],
        &VPath::local(&f.local),
        true,
        Conflict::Skip,
    )
    .unwrap();
    assert_eq!(fs::read(f.local.join("dir/inner/go.txt")).unwrap(), b"go");
    assert_eq!(fs::read(f.local.join("dir/keep.txt")).unwrap(), b"local");
    // The skipped file and its folder stay on the source; the moved subtree is gone.
    assert_eq!(tree(&f.a), ["dir", "dir/keep.txt"]);
}

#[test]
fn refuses_folder_into_itself_and_onto_itself() {
    let f = fixture(None);
    fs::create_dir_all(f.a.join("x/y")).unwrap();
    fs::write(f.a.join("x/f.txt"), b"f").unwrap();
    assert!(run(
        &f,
        &[remote("a", "x")],
        &remote("a", "x/y"),
        false,
        Conflict::Skip
    )
    .is_err());
    assert!(run(
        &f,
        &[remote("a", "x/f.txt")],
        &remote("a", "x"),
        false,
        Conflict::Overwrite
    )
    .is_err());
    assert_eq!(fs::read(f.a.join("x/f.txt")).unwrap(), b"f");
    assert_eq!(tree(&f.a), ["x", "x/f.txt", "x/y"]);
    assert!(f.b.read_dir().unwrap().next().is_none());
}

#[test]
fn local_to_local_delegates_to_local_ops() {
    let f = fixture(None);
    fs::write(f.local.join("one.txt"), b"1").unwrap();
    let dst = f.local.join("dst");
    fs::create_dir(&dst).unwrap();
    run(
        &f,
        &[VPath::local(f.local.join("one.txt"))],
        &VPath::local(&dst),
        false,
        Conflict::Skip,
    )
    .unwrap();
    assert_eq!(fs::read(dst.join("one.txt")).unwrap(), b"1");
}
