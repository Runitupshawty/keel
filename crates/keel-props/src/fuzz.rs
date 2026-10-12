//! Fuzz entry points (docs/fuzzing.md), shared by `fuzz/` and the in-tree runs.

use std::io::Cursor;

/// File signatures, the text check, text statistics and `.url` files over any bytes.
pub fn file_signature(data: &[u8]) {
    let _ = crate::signature::detect(data);
    let _ = crate::signature::looks_like_text(data);
    let _ = crate::text::sniff(data);
    let _ = crate::text::stats(data, 1 << 20, &crate::Cancel::default());
    let _ = crate::parse_url_file(data);
}

/// A `.lnk` file and its target.
pub fn lnk(data: &[u8]) {
    if let Ok(s) = crate::lnk::parse(data) {
        let _ = s.target();
    }
}

/// A PE image's version resource, and the bytes as a bare `VS_VERSIONINFO`.
pub fn pe_version(data: &[u8]) {
    if let Ok(v) = crate::pe::version_info(&mut Cursor::new(data), data.len() as u64) {
        let _ = (
            v.file_version(),
            v.product_version(),
            v.string("CompanyName"),
        );
    }
    let _ = crate::pe::parse_version_resource(data);
}

/// An audio file (ID3v2, ID3v1, MPEG frames, FLAC, WAV), and the bytes as a bare ID3v2 tag
/// and Vorbis comment block.
pub fn audio_tags(data: &[u8]) {
    let _ = crate::audio::read(&mut Cursor::new(data), data.len() as u64);
    let mut info = crate::audio::AudioInfo::default();
    crate::audio::parse_id3v2(data, &mut info);
    crate::audio::vorbis_comments(data, &mut info);
}

/// Content search over any bytes: the first byte picks the encoding (bits 0-2), `case:`
/// (3), `whole-words:` (4), `regex:` (5) and the line mode (6-7); the bytes up to the next
/// NUL are the pattern (at most 64), the rest is the file.
pub fn content_match(data: &[u8]) {
    use crate::content::{parse_hex, Budget, Encoding as E, Limits, Lines, Matcher, Spec};
    let Some((&flags, rest)) = data.split_first() else {
        return;
    };
    let (pattern, body) = match rest.iter().position(|&b| b == 0) {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, &[][..]),
    };
    let pattern = String::from_utf8_lossy(&pattern[..pattern.len().min(64)]);
    let encodings = [E::Auto, E::Utf8, E::Utf16Le, E::Utf16Be, E::Ansi, E::Binary, E::Hex];
    let mut spec = Spec::new(pattern.as_ref(), encodings[usize::from(flags & 7) % 7]);
    spec.case = flags & 8 != 0;
    spec.whole_words = flags & 16 != 0;
    spec.regex = flags & 32 != 0;
    spec.lines = match flags >> 6 {
        0 => Lines::Whole,
        1 => Lines::MultiLine,
        _ => Lines::EachLine,
    };
    let _ = parse_hex(&pattern);
    let _ = Spec::parse(&pattern, spec.encoding);
    if let Ok(m) = Matcher::new(spec) {
        let _ = m.match_reader(body, &Budget::default());
        let _ = m.match_text(&String::from_utf8_lossy(body), &Budget::default());
        let small = Limits {
            max_file_bytes: (body.len() / 2) as u64,
            ..Limits::default()
        };
        let _ = m.match_reader(body, &Budget::new(small, crate::Cancel::default(), false));
    }
}

/// A Windows Recycle Bin `$I` record.
pub fn recycle_info(data: &[u8]) {
    let _ = crate::trash::parse_recycle_info(data);
}

/// A freedesktop.org `.trashinfo` file.
pub fn trash_info(data: &[u8]) {
    let _ = crate::trash::parse_trash_info(data);
}

#[cfg(test)]
mod tests {
    use keel_vfs::fuzz_harness::{run, Capped};

    #[global_allocator]
    static ALLOC: Capped = Capped;

    #[test]
    fn fuzz_file_signature() {
        run("file_signature", super::file_signature);
    }
    #[test]
    fn fuzz_lnk() {
        run("lnk", super::lnk);
    }
    #[test]
    fn fuzz_pe_version() {
        run("pe_version", super::pe_version);
    }
    #[test]
    fn fuzz_audio_tags() {
        run("audio_tags", super::audio_tags);
    }
    #[test]
    fn fuzz_content_match() {
        run("content_match", super::content_match);
    }
    #[test]
    fn fuzz_recycle_info() {
        run("recycle_info", super::recycle_info);
    }
    #[test]
    fn fuzz_trash_info() {
        run("trash_info", super::trash_info);
    }

    /// The committed seeds are the generated fixtures (`KEEL_WRITE_CORPUS=1` rewrites them).
    #[test]
    fn corpus_seeds_match_fixtures() {
        use crate::fixtures as fx;
        /// 2021-06-15T10:20:30Z.
        const FILETIME: u64 = 132_682_260_300_000_000;
        let lnk_spec = fx::LnkSpec {
            local_base_path: Some(r"C:\Tools\"),
            suffix: "app.exe",
            unicode_paths: true,
            arguments: Some("--flag"),
            working_dir: Some(r"C:\Tools"),
            name: Some("App"),
            id_list: true,
            env_target: Some(r"%windir%\notepad.exe"),
        };
        let seeds: Vec<(&str, &str, Vec<u8>)> = vec![
            ("lnk", "local-unicode-env.lnk", fx::lnk(&lnk_spec)),
            (
                "pe_version",
                "pe32.exe",
                fx::pe_with_version(false, &fx::VERSION_STRINGS),
            ),
            (
                "pe_version",
                "pe32plus.exe",
                fx::pe_with_version(true, &fx::VERSION_STRINGS),
            ),
            (
                "pe_version",
                "versioninfo.bin",
                fx::version_resource(&fx::VERSION_STRINGS),
            ),
            ("audio_tags", "id3v23.mp3", fx::mp3(3, false, 3)),
            ("audio_tags", "id3v24-xing.mp3", fx::mp3(4, true, 2)),
            ("audio_tags", "id3v22.mp3", fx::mp3(2, false, 2)),
            ("audio_tags", "tones.flac", fx::flac()),
            ("audio_tags", "silence.wav", fx::wav()),
            (
                "audio_tags",
                "id3v1.mp3",
                fx::id3v1("Title", "Artist", "Album", "1999", 7, 13),
            ),
            (
                "content_match",
                "utf16-auto.bin",
                [
                    b"\0caf\xc3\xa9\0".as_slice(),
                    &fx::encode("Caf\u{e9} au lait\n", fx::FxEncoding::Utf16LeBom),
                ]
                .concat(),
            ),
            (
                "content_match",
                "regex-each-line.bin",
                b"\xa0^b.t\0alpha\r\nbeta\n".to_vec(),
            ),
            ("content_match", "hex.bin", b"\x0650 4b\0xxPKyy".to_vec()),
            (
                "recycle_info",
                "v1.bin",
                fx::recycle_info(1, r"C:\Data\a.txt", 10, FILETIME),
            ),
            (
                "recycle_info",
                "v2.bin",
                fx::recycle_info(2, r"C:\Data\report.docx", 12_345, FILETIME),
            ),
            (
                "trash_info",
                "basic.trashinfo",
                fx::trash_info("/tmp/a%20b.txt", "2021-06-15T10:20:30"),
            ),
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus");
        let write = std::env::var_os("KEEL_WRITE_CORPUS").is_some();
        for (target, name, bytes) in seeds {
            let path = root.join(target).join(name);
            if write {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, &bytes).unwrap();
            }
            let on_disk =
                std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert!(
                on_disk == bytes,
                "{} differs from its fixture",
                path.display()
            );
        }
    }
}
