//! keel-daemon as a library: [`server::Daemon`] serves a profile's library over JSON-RPC
//! (the binary's `main`, and tests of clients such as the desktop app that start one in
//! their own process).

pub mod everything;
/// Fuzz entry points (`fuzz/` and the in-tree runs; docs/fuzzing.md).
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzz;
pub mod server;
mod share;
mod web;
mod ws;

#[cfg(test)]
mod tests;
