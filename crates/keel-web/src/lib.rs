//! Keel in a browser, served by `keel-daemon --web` (see README.md here for why this is a
//! purpose-built egui app rather than keel-app compiled to wasm). It talks only to the
//! daemon's `/rpc` WebSocket on its own origin, authenticating with the daemon token as
//! the first message; the token is typed in (never read from the address) and kept in
//! `localStorage` only when "remember on this device" is ticked.
//!
//! The connection state machine, the address guard, the layout breakpoint, the gestures,
//! the share-sheet flow and the display helpers build and are tested on every target; the
//! UI and the browser glue only on wasm32.

pub mod conn;
pub mod gesture;
pub mod guard;
pub mod layout;
pub mod share;
/// keel-api's parameter and result types, shared by path: keel-api itself (keel-core,
/// SQLite, tokio's network stack) does not build for wasm32.
#[path = "../../keel-api/src/types.rs"]
pub mod types;
pub mod util;

#[cfg(target_arch = "wasm32")]
mod app;
#[cfg(target_arch = "wasm32")]
mod web;
