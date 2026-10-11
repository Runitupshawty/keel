//! Fuzz entry points (docs/fuzzing.md), shared by `fuzz/` and the in-tree runs.

use crate::types::{Request, Response};

/// One message as a peer sends it (CBOR, `wire::decode`): requests (a Spacedrop offer is
/// checked as the receiver checks it), responses (a sync page is applied to a library, as
/// a pull applies it), pairing tickets.
pub fn wire(data: &[u8]) {
    if let Ok(Request::DropOffer { id, files }) = crate::wire::decode::<Request>(data) {
        let _ = crate::spacedrop::offer_valid(&id, &files);
    }
    if let Ok(Response::SyncEntries {
        entries,
        more,
        upto,
        epoch,
    }) = crate::wire::decode::<Response>(data)
    {
        let page = keel_core::SyncPage {
            entries,
            more,
            upto,
            epoch,
        };
        let _ = library().sync_apply("fuzz-peer", &page);
    }
    let _ = crate::wire::decode::<crate::pairing::Ticket>(data);
}

/// One library for every case (opening one per input would dominate the run).
fn library() -> &'static keel_core::Library {
    static LIB: std::sync::OnceLock<(tempfile::TempDir, keel_core::Library)> =
        std::sync::OnceLock::new();
    &LIB.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let lib = keel_core::Library::open(dir.path(), "fuzz").unwrap();
        (dir, lib)
    })
    .1
}

/// An invitation as typed or scanned (`PairCode::from_str`) and shown again, and a device
/// id as typed.
pub fn pair_code(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    if let Ok(code) = text.parse::<crate::PairCode>() {
        let _ = code.to_string();
    }
    let _ = text.parse::<crate::NodeId>();
}

#[cfg(test)]
mod tests {
    use keel_vfs::fuzz_harness::{run, Capped};

    #[global_allocator]
    static ALLOC: Capped = Capped;

    #[test]
    fn fuzz_wire() {
        run("wire", super::wire);
    }
    #[test]
    fn fuzz_pair_code() {
        run("pair_code", super::pair_code);
    }
}
