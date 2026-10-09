use crate::{Provider, VPath};
use std::sync::Arc;

pub struct Router {
    providers: Vec<Arc<dyn Provider>>,
}
impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}
impl Router {
    pub fn new() -> Self {
        Self {
            providers: vec![Arc::new(crate::LocalProvider)],
        }
    }
    pub fn provider_for(&self, p: &VPath) -> Option<Arc<dyn Provider>> {
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
}
