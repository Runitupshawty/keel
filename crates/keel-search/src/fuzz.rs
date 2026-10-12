//! Fuzz entry points (docs/fuzzing.md), shared by `fuzz/` and the in-tree runs.

use crate::{mft, proto};

/// MFT `FILE` records as the elevated volume read hands them over: the whole input as
/// one record (update sequence, every attribute, the data runs), then the input cut
/// into 1 KiB records merged by a [`mft::Scan`] (attribute lists, extension records,
/// hard links).
pub fn mft_record(data: &[u8]) {
    let mut one = data.to_vec();
    if mft::fixup(&mut one).is_ok() {
        let _ = mft::parse_fixed(&one);
        let _ = mft::data_runs(&one);
    }
    let mut scan = mft::Scan::default();
    for (number, chunk) in data.chunks(1024).enumerate() {
        let mut record = chunk.to_vec();
        if let Ok(Some(entry)) = scan.add(number as u64, &mut record) {
            let _ = (entry.attributes(), entry.links());
        }
    }
    for entry in scan.finish() {
        let _ = (entry.attributes(), entry.links());
    }
}

/// The index service's protocol as both ends read it: the input as a stream of
/// frames, and as one request and one response payload. Whatever decodes must encode
/// back to the same message.
pub fn index_proto(data: &[u8]) {
    let mut stream = data;
    while let Ok(Some(frame)) = proto::read_frame(&mut stream, proto::MAX_RESPONSE) {
        decode_both(&frame);
    }
    decode_both(data);
}

fn decode_both(payload: &[u8]) {
    if let Ok(request) = proto::Request::decode(payload) {
        let again = proto::Request::decode(&request.encode());
        assert_eq!(again.as_ref(), Ok(&request), "request round trip");
    }
    if let Ok(response) = proto::Response::decode(payload) {
        let again = proto::Response::decode(&response.encode());
        assert_eq!(again.as_ref(), Ok(&response), "response round trip");
    }
}

#[cfg(test)]
mod tests {
    use keel_vfs::fuzz_harness::{run, Capped};

    #[global_allocator]
    static ALLOC: Capped = Capped;

    #[test]
    fn fuzz_mft_record() {
        run("mft_record", super::mft_record);
    }

    #[test]
    fn fuzz_index_proto() {
        run("index_proto", super::index_proto);
    }
}
