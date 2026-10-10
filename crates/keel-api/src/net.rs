//! keel-net glue for hosts of the API. The daemon opens its node with [`NoSources`] until
//! it hosts the library's sources through keel-net's `LibraryHandler` (the app does); it
//! offers no source and refuses every file request, so grants made through `shares.grant`
//! are recorded but give nothing yet.

use anyhow::{bail, Result};
use keel_net::{EntryInfo, Handler, RequestCtx, SourceInfo, Storage, WriteAt};
use tokio::io::AsyncRead;

pub struct NoSources;

const REFUSED: &str = "this device does not serve sources yet";

#[async_trait::async_trait]
impl Handler for NoSources {
    async fn sources(&self, _: &RequestCtx) -> Vec<SourceInfo> {
        Vec::new()
    }
    async fn list(&self, _: &RequestCtx, _: &str, _: &str) -> Result<Vec<EntryInfo>> {
        bail!(REFUSED)
    }
    async fn stat(&self, _: &RequestCtx, _: &str, _: &str) -> Result<EntryInfo> {
        bail!(REFUSED)
    }
    async fn read(
        &self,
        _: &RequestCtx,
        _: &str,
        _: &str,
        _: Option<(u64, u64)>,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>> {
        bail!(REFUSED)
    }
    async fn write(
        &self,
        _: &RequestCtx,
        _: &str,
        _: &str,
        _: Box<dyn AsyncRead + Send + Unpin>,
        _: WriteAt,
    ) -> Result<()> {
        bail!(REFUSED)
    }
    async fn stat_partial(&self, _: &RequestCtx, _: &str, _: &str) -> Result<u64> {
        bail!(REFUSED)
    }
    async fn mkdir(&self, _: &RequestCtx, _: &str, _: &str) -> Result<()> {
        bail!(REFUSED)
    }
    async fn rename(&self, _: &RequestCtx, _: &str, _: &str, _: &str) -> Result<()> {
        bail!(REFUSED)
    }
    async fn remove(&self, _: &RequestCtx, _: &str, _: &str) -> Result<()> {
        bail!(REFUSED)
    }
    async fn storage(&self, _: &RequestCtx) -> Option<Storage> {
        None
    }
}
