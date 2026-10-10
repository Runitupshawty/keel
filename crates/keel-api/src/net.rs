//! keel-net glue for hosts of the API. Serving library sources to peers (the remote
//! `node://` provider and its host side) is Task 36; until then a host opens its node with
//! [`NoSources`], which offers no source and refuses every file request, so grants made
//! through `shares.grant` are recorded but give nothing yet.

use anyhow::{bail, Result};
use keel_net::{EntryInfo, Handler, SourceInfo, Storage};
use tokio::io::AsyncRead;

pub struct NoSources;

const REFUSED: &str = "this device does not serve sources yet";

#[async_trait::async_trait]
impl Handler for NoSources {
    async fn sources(&self) -> Vec<SourceInfo> {
        Vec::new()
    }
    async fn list(&self, _: &str, _: &str) -> Result<Vec<EntryInfo>> {
        bail!(REFUSED)
    }
    async fn stat(&self, _: &str, _: &str) -> Result<EntryInfo> {
        bail!(REFUSED)
    }
    async fn read(
        &self,
        _: &str,
        _: &str,
        _: Option<(u64, u64)>,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>> {
        bail!(REFUSED)
    }
    async fn write(
        &self,
        _: &str,
        _: &str,
        _: Box<dyn AsyncRead + Send + Unpin>,
        _: u64,
    ) -> Result<()> {
        bail!(REFUSED)
    }
    async fn mkdir(&self, _: &str, _: &str) -> Result<()> {
        bail!(REFUSED)
    }
    async fn rename(&self, _: &str, _: &str, _: &str) -> Result<()> {
        bail!(REFUSED)
    }
    async fn remove(&self, _: &str, _: &str) -> Result<()> {
        bail!(REFUSED)
    }
    async fn storage(&self) -> Option<Storage> {
        None
    }
}
