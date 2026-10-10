//! keel-daemon as a library: [`server::Daemon`] serves a profile's library over JSON-RPC
//! (the binary's `main`, and tests of clients such as the desktop app that start one in
//! their own process).

pub mod server;
mod share;
mod web;
mod ws;

#[cfg(test)]
mod tests;
