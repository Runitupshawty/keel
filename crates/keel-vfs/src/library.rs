//! `library://<source id>/<path>`: a library source's indexed tree. Listing comes from the
//! index (so it works while the source is offline); reads go to the source's real location
//! when it is online and fail with "offline (last seen …)" otherwise. Writes are refused:
//! the app plans them on the real paths (keel-core's validate → preview → execute).
//! keel-core depends on this crate, so the index is reached through [`LibraryIndex`].

use crate::{Caps, Entry, Progress, Provider, Router, VPath};
use anyhow::Result;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{atomic::AtomicBool, Arc, Weak};

pub const SCHEME: &str = "library";

/// What the provider needs from the library.
pub trait LibraryIndex: Send + Sync {
    /// The indexed children of `rel` ("" = the root) in `source`, from its last generation.
    /// Their `path` is ignored (the provider sets the `library://` one).
    fn children(&self, source: &str, rel: &str) -> Result<Vec<Entry>>;
    /// The real path of `rel` in `source`; an error such as "offline (last seen …)" when the
    /// source cannot be reached now.
    fn resolve(&self, source: &str, rel: &str) -> Result<VPath>;
}

/// `library://<source>/<rel>`.
pub fn path(source: &str, rel: &str) -> VPath {
    VPath {
        scheme: SCHEME.into(),
        authority: source.into(),
        path: format!("/{}", rel.trim_matches('/')),
    }
}

/// The source id and relative path of a `library://` path.
pub fn split(p: &VPath) -> Option<(&str, &str)> {
    (p.scheme == SCHEME && p.split_archive().is_none())
        .then(|| (p.authority.as_str(), p.path.trim_matches('/')))
}

pub struct LibraryProvider {
    index: Arc<dyn LibraryIndex>,
    /// Weak: the router holds this provider.
    router: Weak<Router>,
}

impl LibraryProvider {
    pub fn new(index: Arc<dyn LibraryIndex>, router: Weak<Router>) -> Self {
        Self { index, router }
    }

    fn parts(p: &VPath) -> Result<(&str, &str)> {
        split(p).ok_or_else(|| anyhow::anyhow!("not a library path: {}", p.display()))
    }

    /// The real path and its provider.
    fn real(&self, p: &VPath) -> Result<(Arc<dyn Provider>, VPath)> {
        let (source, rel) = Self::parts(p)?;
        let real = self.index.resolve(source, rel)?;
        let router = self
            .router
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("closing"))?;
        let provider = router
            .provider_for(&real)
            .ok_or_else(|| anyhow::anyhow!("no provider for {}", real.display()))?;
        Ok((provider, real))
    }

    fn read_only(p: &VPath) -> anyhow::Error {
        anyhow::anyhow!(
            "{}: library views are read-only here; open the real folder",
            p.display()
        )
    }
}

impl Provider for LibraryProvider {
    fn scheme(&self) -> &'static str {
        SCHEME
    }

    fn caps(&self) -> Caps {
        Caps::default()
    }

    fn list(&self, dir: &VPath) -> Result<Vec<Entry>> {
        let (source, rel) = Self::parts(dir)?;
        let mut entries = self.index.children(source, rel)?;
        for e in &mut entries {
            e.path = path(source, &format!("{rel}/{}", e.name));
            if e.ext.is_empty() && e.kind != crate::Kind::Dir {
                e.ext = match e.name.rsplit_once('.') {
                    Some((stem, ext)) if !stem.is_empty() => ext.to_lowercase(),
                    _ => String::new(),
                };
            }
        }
        Ok(entries)
    }

    fn stat(&self, p: &VPath) -> Result<Entry> {
        let parent = p
            .parent()
            .ok_or_else(|| anyhow::anyhow!("{} is a source root", p.display()))?;
        self.list(&parent)?
            .into_iter()
            .find(|e| e.name == p.name())
            .ok_or_else(|| anyhow::anyhow!("{} is not in the index", p.display()))
    }

    fn read(&self, p: &VPath) -> Result<Box<dyn Read + Send>> {
        let (provider, real) = self.real(p)?;
        provider.read(&real)
    }

    fn write(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        Err(Self::read_only(p))
    }

    fn mkdir(&self, p: &VPath) -> Result<()> {
        Err(Self::read_only(p))
    }

    fn rename(&self, from: &VPath, _to: &VPath) -> Result<()> {
        Err(Self::read_only(from))
    }

    fn remove(&self, p: &VPath) -> Result<()> {
        Err(Self::read_only(p))
    }

    fn local_copy(&self, p: &VPath) -> Result<PathBuf> {
        let (provider, real) = self.real(p)?;
        provider.local_copy(&real)
    }

    fn local_copy_cancellable(
        &self,
        p: &VPath,
        progress: &dyn Fn(Progress),
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        let (provider, real) = self.real(p)?;
        provider.local_copy_cancellable(&real, progress, cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Kind;

    struct Fake {
        root: PathBuf,
        online: bool,
    }

    impl LibraryIndex for Fake {
        fn children(&self, source: &str, rel: &str) -> Result<Vec<Entry>> {
            anyhow::ensure!(source == "src1" && rel.is_empty(), "no such folder");
            Ok(vec![Entry {
                path: VPath::local("ignored"),
                name: "a.TXT".into(),
                kind: Kind::File,
                size: 2,
                modified: None,
                hidden: false,
                is_link: false,
                encrypted: false,
                ext: String::new(),
            }])
        }
        fn resolve(&self, _: &str, rel: &str) -> Result<VPath> {
            anyhow::ensure!(self.online, "offline (last seen 2026-10-01)");
            Ok(VPath::local(self.root.join(rel)))
        }
    }

    #[test]
    fn lists_from_the_index_and_reads_through_the_real_path() {
        let root = std::env::temp_dir().join(format!("keel-libvfs-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.TXT"), "hi").unwrap();
        let router = Arc::new(Router::new());
        let fake = |online| {
            Arc::new(Fake {
                root: root.clone(),
                online,
            })
        };
        router.register(Arc::new(LibraryProvider::new(
            fake(true),
            Arc::downgrade(&router),
        )));
        let dir = path("src1", "");
        let p = router.provider_for(&dir).unwrap();
        let entries = p.list(&dir).unwrap();
        assert_eq!(entries[0].path, path("src1", "a.TXT"));
        assert_eq!(entries[0].ext, "txt");
        assert_eq!(split(&entries[0].path), Some(("src1", "a.TXT")));
        assert_eq!(p.stat(&entries[0].path).unwrap().size, 2);
        let mut text = String::new();
        p.read(&entries[0].path)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "hi");
        assert!(p.remove(&entries[0].path).is_err(), "read-only");

        router.register(Arc::new(LibraryProvider::new(
            fake(false),
            Arc::downgrade(&router),
        )));
        let p = router.provider_for(&dir).unwrap();
        assert_eq!(p.list(&dir).unwrap().len(), 1, "lists while offline");
        let err = p.local_copy(&path("src1", "a.TXT")).unwrap_err();
        assert!(format!("{err:#}").contains("offline (last seen"), "{err:#}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
