use crate::{Provider, VPath};
use parking_lot::RwLock;
use std::sync::Arc;

/// Shared with the archive provider (weakly) so it can resolve an archive's OUTER path.
pub(crate) type Registry = RwLock<Vec<Arc<dyn Provider>>>;

pub struct Router {
    providers: Arc<Registry>,
}
impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}
impl Router {
    pub fn new() -> Self {
        Self::with_archive_cache(Arc::default())
    }
    pub fn with_archive_cache(cache: Arc<crate::archive::cache::MaterialiseCache>) -> Self {
        let providers: Arc<Registry> = Arc::new(RwLock::new(vec![Arc::new(crate::LocalProvider)]));
        let archive = crate::archive::ArchiveProvider::new(cache, Arc::downgrade(&providers));
        providers.write().push(Arc::new(archive));
        Self { providers }
    }
    /// Paths with a `!/` archive boundary go to the archive provider, the rest by scheme.
    pub fn provider_for(&self, p: &VPath) -> Option<Arc<dyn Provider>> {
        find(&self.providers, p)
    }
    pub fn register(&mut self, p: Arc<dyn Provider>) {
        let mut providers = self.providers.write();
        providers.retain(|existing| existing.scheme() != p.scheme());
        providers.push(p);
    }
}

pub(crate) fn find(registry: &Registry, p: &VPath) -> Option<Arc<dyn Provider>> {
    let scheme = if p.split_archive().is_some() {
        "archive"
    } else {
        p.scheme.as_str()
    };
    registry
        .read()
        .iter()
        .find(|provider| provider.scheme() == scheme)
        .cloned()
}
