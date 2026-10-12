//! Fuzz entry points (docs/fuzzing.md), shared by `fuzz/` and the in-tree runs.

use crate::filelist::{export, read, ExportOptions, FileListSearcher, Format, Limits, ListKind};

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

#[cfg(test)]
mod tests {
    use keel_vfs::fuzz_harness::{run, Capped};

    #[global_allocator]
    static ALLOC: Capped = Capped;

    #[test]
    fn fuzz_efu() {
        run("efu", super::efu);
    }
}
