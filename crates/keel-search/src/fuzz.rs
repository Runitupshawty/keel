//! Fuzz entry points (docs/fuzzing.md), shared by `fuzz/` and the in-tree runs.

use crate::{mft, wire};

/// MFT `FILE` records as the elevated volume read hands them over: the whole input as
/// one record (update sequence, every attribute, the data runs), then the input cut
/// into 1 KiB records merged by a [`mft::Scan`] (attribute lists, extension records,
/// hard links). Each 1 KiB record is also blanked as the helper does before sending
/// it (asserting the parse stays the same), framed, and decoded again as the
/// unelevated side does; and the whole input is decoded as a helper frame.
pub fn mft_record(data: &[u8]) {
    let _ = wire::Frame::decode(data);
    let mut batch = wire::Batch::new(1024);
    for (number, chunk) in data.as_chunks::<1024>().0.iter().enumerate() {
        let mut fixed = chunk.to_vec();
        if mft::fixup(&mut fixed).is_ok() {
            let before = mft::parse_fixed(&fixed);
            let mut blanked = fixed.clone();
            if mft::blank_fixed(&mut blanked) == Ok(true) {
                assert_eq!(
                    mft::parse_fixed(&blanked),
                    before,
                    "blanking changed the parse"
                );
                batch.push(number as u64, &blanked);
            }
        }
    }
    let mut stream = Vec::new();
    let _ = batch.send(&mut |frame| {
        stream.extend(frame);
        Ok(())
    });
    let mut buf = Vec::new();
    if let Ok(true) = wire::read_frame(&mut &stream[..], &mut buf) {
        if let Ok(wire::Frame::Records(records)) = wire::Frame::decode(&buf) {
            let mut scan = mft::Scan::default();
            for (number, record) in records.iter() {
                if let Ok(rec) = mft::parse_fixed(record) {
                    let _ = scan.add_parsed(number, rec);
                }
            }
            let _ = scan.finish();
        }
    }
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

#[cfg(test)]
mod tests {
    use keel_vfs::fuzz_harness::{run, Capped};

    #[global_allocator]
    static ALLOC: Capped = Capped;

    #[test]
    fn fuzz_mft_record() {
        run("mft_record", super::mft_record);
    }
}
