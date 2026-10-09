use crate::{Provider, VPath};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// Providers by scheme plus remote providers by host id. Shared weakly with the archive
/// provider so it can resolve an archive's OUTER path, including a remote one.
pub(crate) struct Table {
    providers: Vec<Arc<dyn Provider>>,
    remotes: HashMap<String, Arc<dyn Provider>>,
}
pub(crate) type Registry = RwLock<Table>;

pub struct Router {
    registry: Arc<Registry>,
    remote_events: crossbeam_channel::Sender<crate::RemoteEvent>,
    events: crossbeam_channel::Receiver<crate::RemoteEvent>,
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
        let (remote_events, events) = crossbeam_channel::unbounded();
        let registry: Arc<Registry> = Arc::new(RwLock::new(Table {
            providers: vec![Arc::new(crate::LocalProvider)],
            remotes: HashMap::new(),
        }));
        let archive = crate::archive::ArchiveProvider::new(cache, Arc::downgrade(&registry));
        registry.write().providers.push(Arc::new(archive));
        Self {
            registry,
            remote_events,
            events,
        }
    }
    /// Paths with a `!/` archive boundary go to the archive provider, `sftp://<id>/...` to that
    /// host's provider, the rest by scheme.
    pub fn provider_for(&self, p: &VPath) -> Option<Arc<dyn Provider>> {
        find(&self.registry, p)
    }
    pub fn register(&mut self, p: Arc<dyn Provider>) {
        let mut table = self.registry.write();
        table
            .providers
            .retain(|existing| existing.scheme() != p.scheme());
        table.providers.push(p);
    }
    pub fn register_remote(&self, host: crate::RemoteHost) {
        self.register_remote_provider(
            host.id.clone(),
            Arc::new(crate::SftpProvider::new(host, self.remote_events.clone())),
        );
    }
    /// Adds or replaces the provider for `sftp://<id>/...` (callable through an `Arc<Router>`).
    pub fn register_remote_provider(&self, id: String, provider: Arc<dyn Provider>) {
        self.registry.write().remotes.insert(id, provider);
    }
    pub fn unregister_remote(&self, id: &str) {
        self.registry.write().remotes.remove(id);
    }
    pub fn remote_events(&self) -> crossbeam_channel::Receiver<crate::RemoteEvent> {
        self.events.clone()
    }
}

pub(crate) fn find(registry: &Registry, p: &VPath) -> Option<Arc<dyn Provider>> {
    let table = registry.read();
    if p.split_archive().is_some() {
        return table
            .providers
            .iter()
            .find(|provider| provider.scheme() == "archive")
            .cloned();
    }
    if p.scheme == "sftp" {
        return table.remotes.get(&p.authority).cloned();
    }
    table
        .providers
        .iter()
        .find(|provider| provider.scheme() == p.scheme)
        .cloned()
}
