//! Fuzz entry points (docs/fuzzing.md), shared by `fuzz/` and the in-tree runs.

use crate::mft;

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
