use crate::{Provider, VPath};
use std::sync::Arc;

pub struct Router {
    providers: Vec<Arc<dyn Provider>>,
    remotes: std::collections::HashMap<String, Arc<dyn Provider>>,
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
        let (remote_events, events) = crossbeam_channel::unbounded();
        Self {
            providers: vec![Arc::new(crate::LocalProvider)],
            remotes: Default::default(),
            remote_events,
            events,
        }
    }
    pub fn provider_for(&self, p: &VPath) -> Option<Arc<dyn Provider>> {
        if p.scheme == "sftp" {
            return self.remotes.get(&p.authority).cloned();
        }
        self.providers
            .iter()
            .find(|provider| provider.scheme() == p.scheme)
            .cloned()
    }
    pub fn register(&mut self, p: Arc<dyn Provider>) {
        self.providers
            .retain(|existing| existing.scheme() != p.scheme());
        self.providers.push(p);
    }
    pub fn register_remote(&mut self, host: crate::RemoteHost) {
        self.register_remote_provider(
            host.id.clone(),
            Arc::new(crate::SftpProvider::new(host, self.remote_events.clone())),
        );
    }
    pub fn register_remote_provider(&mut self, id: String, provider: Arc<dyn Provider>) {
        self.remotes.insert(id, provider);
    }
    pub fn remote_events(&self) -> crossbeam_channel::Receiver<crate::RemoteEvent> {
        self.events.clone()
    }
}
