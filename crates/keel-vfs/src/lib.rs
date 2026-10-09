//! Virtual filesystem providers and shared file operation types for Keel.

mod entry;
mod path;

pub use entry::{Entry, Kind};
pub use path::VPath;
