//! Fuzz entry points (docs/fuzzing.md), shared by `fuzz/` and the in-tree runs.

use crate::filelist::{export, read, ExportOptions, FileListSearcher, Format, Limits, ListKind};
use crate::{mft, proto, wire};

/// A file list: the first byte picks how it is read (EFU, CSV, TSV, TXT, M3U), the rest is
/// the file, under small limits. What it reads is then written in every export format, the
/// EFU read back (same number of entries), and searched.
pub fn efu(data: &[u8]) {
    let (kind, body) = match data.split_first() {
        Some((k, rest)) => (
            [
                ListKind::Efu,
                ListKind::Csv,
                ListKind::Tsv,
                ListKind::Txt,
                ListKind::M3u,
            ][usize::from(*k) % 5],
            rest,
        ),
        None => (ListKind::Efu, data),
    };
    let limits = Limits {
        max_rows: 10_000,
        max_line: 4096,
    };
    let Ok(list) = read(body, kind, &limits) else {
        return;
    };
    for format in Format::ALL {
        let mut out = Vec::new();
        export(&list, &ExportOptions::new(format), &mut out).expect("writing to memory");
        if format == Format::Efu {
            let roomy = Limits {
                max_rows: 10_000,
                max_line: 1 << 20,
            };
            let again = read(&out[..], ListKind::Efu, &roomy).expect("our EFU reads back");
            assert_eq!(again.entries.len(), list.entries.len());
            assert_eq!(again.columns, list.columns);
        }
    }
    let searcher = FileListSearcher::new("fuzz", list);
    for text in ["a", "*.txt", "folder:", "regex:^x", "in:c:/"] {
        let q = crate::Query {
            text: text.into(),
            max: 50,
            ..crate::Query::default()
        };
        let _ = crate::Searcher::query(&searcher, &q);
    }
}

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

/// The index service's protocol as both ends read it: the input as a stream of
/// frames, and as one request and one response payload; also each frame as one of
/// the MFT stream's `wire` frames (what Keel reads after a `ReadMft` request).
/// Whatever decodes must encode back to the same message.
pub fn index_proto(data: &[u8]) {
    let mut stream = data;
    while let Ok(Some(frame)) = proto::read_frame(&mut stream, proto::MAX_RESPONSE) {
        decode_both(&frame);
        decode_wire(&frame);
    }
    decode_both(data);
    decode_wire(data);
}

fn decode_wire(payload: &[u8]) {
    // Frames that carry no borrowed bytes must encode to the same payload.
    if let Ok(frame @ (wire::Frame::Volume { .. } | wire::Frame::End { .. } | wire::Frame::Done)) =
        wire::Frame::decode(payload)
    {
        assert_eq!(&frame.encode()[4..], payload, "wire frame round trip");
    }
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
    fn fuzz_efu() {
        run("efu", super::efu);
    }

    #[test]
    fn fuzz_mft_record() {
        run("mft_record", super::mft_record);
    }

    #[test]
    fn fuzz_index_proto() {
        run("index_proto", super::index_proto);
    }
}
