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

    /// The committed seeds are the generated fixtures (`KEEL_WRITE_CORPUS=1` rewrites them).
    #[test]
    fn corpus_seeds_match_fixtures() {
        use crate::fixtures as fx;
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
