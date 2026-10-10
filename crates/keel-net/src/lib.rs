//! Keel's authenticated device transport. See the crate README for pairing,
//! persistence, wire framing, offline configuration and Handler security contracts.
#![doc = include_str!("../README.md")]
mod host;
mod node;
mod pairing;
mod protocol;
mod provider;
mod scope;
pub mod spacedrop;
mod stage;
mod store;
mod types;
mod wire;
pub use host::LibraryHandler;
pub use node::{Node, NodeOptions, ALPN};
pub use pairing::PairCode;
pub use provider::NodeProvider;
pub use types::*;
#[cfg(test)]
mod library_tests;
#[cfg(test)]
mod spacedrop_tests;
#[cfg(test)]
mod tests;
