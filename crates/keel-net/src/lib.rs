//! Keel's authenticated device transport. See the crate README for pairing,
//! persistence, wire framing, offline configuration and Handler security contracts.
#![doc = include_str!("../README.md")]
mod node;
mod pairing;
mod protocol;
mod scope;
mod store;
mod types;
mod wire;
pub use node::{Node, NodeOptions, ALPN};
pub use pairing::PairCode;
pub use types::*;
#[cfg(test)]
mod tests;
