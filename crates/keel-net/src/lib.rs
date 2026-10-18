//! Keel's authenticated device transport. See the crate README for pairing,
//! persistence, wire framing, offline configuration and Handler security contracts.
#![doc = include_str!("../README.md")]
/// Fuzz entry points (`fuzz/` and the in-tree runs; docs/fuzzing.md).
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzz;
mod host;
mod node;
mod pairing;
mod protocol;
mod provider;
mod scope;
mod search;
pub mod spacedrop;
mod stage;
mod store;
mod sync;
mod types;
mod wire;
pub use host::{LibraryHandler, SearchFn};
pub use node::{Node, NodeOptions, ALPN};
pub use pairing::PairCode;
pub use provider::NodeProvider;
pub use scope::valid_label;
pub use search::{NOT_SHARED, SEARCH_RATE, SEARCH_TIMEOUT};
pub use types::*;
#[cfg(test)]
mod library_tests;
#[cfg(test)]
mod search_tests;
#[cfg(test)]
mod spacedrop_tests;
#[cfg(test)]
mod sync_tests;
#[cfg(test)]
mod tests;
