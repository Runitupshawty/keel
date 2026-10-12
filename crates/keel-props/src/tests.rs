//! `get` over generated files in a temp folder.

use super::*;
use crate::fixtures as fx;
use std::time::Duration;

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

fn s(v: &str) -> Prop {
    Prop::Str(v.to_owned())
}

#[test]
fn file_info_from_signatures_and_text() {
    let dir = tempfile::tempdir().unwrap();
    let png = write(dir.path(), "picture.bin", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
    assert_eq!(get(&png, "file-signature").unwrap(), s("png"));
    assert_eq!(get(&png, "content-type:").unwrap(), s("image/png"));
    assert_eq!(get(&png, "perceived-type").unwrap(), s("image"));
    let txt = write(dir.path(), "notes.txt", b"two words\nand three more\n");
    assert_eq!(get(&txt, "file-signature").unwrap(), Prop::Unknown);
    assert_eq!(get(&txt, "content-type").unwrap(), s("text/plain"));
    assert_eq!(get(&txt, "perceived-type").unwrap(), s("text"));
    assert_eq!(get(&txt, "line-count").unwrap(), Prop::Int(2));
    assert_eq!(get(&txt, "word-count").unwrap(), Prop::Int(5));
    assert_eq!(get(&txt, "character-encoding").unwrap(), s("ASCII"));
}

#[test]
fn text_cap_makes_counts_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let txt = write(dir.path(), "big.txt", &b"word ".repeat(100));
    let opts = Options {
        max_text_bytes: 100,
        ..Options::default()
    };
    assert_eq!(get_with(&txt, "line-count", &opts).unwrap(), Prop::Unknown);
    assert_eq!(
        get_with(&txt, "character-encoding", &opts).unwrap(),
        s("ASCII")
    );
}

#[test]
fn hashes_one_pass_cache_and_limits() {
    let dir = tempfile::tempdir().unwrap();
    let p = write(dir.path(), "abc.txt", b"abc");
    let opts = Options::default();
    let mut props = FileProps::new(&p, &opts);
    let all = props.hashes(&HashAlgo::ALL).unwrap();
    assert_eq!(all[0], s("900150983cd24fb0d6963f7d28e17f72"));
    assert_eq!(props.get("sha256").unwrap(), all[2]);
    assert_eq!(props.get("crc").unwrap(), all[4]);
    assert_eq!(get(&p, "crc32").unwrap(), s("352441c2"));
    let small = Options {
        max_hash_bytes: 2,
        ..Options::default()
    };
    assert_eq!(get_with(&p, "md5", &small).unwrap(), Prop::Unknown);
    let cancelled = Options::default();
    cancelled.cancel.cancel();
    assert!(matches!(
        get_with(&p, "sha1", &cancelled),
        Err(Error::Cancelled)
    ));
    // Folders have no content hash.
    assert_eq!(get(dir.path(), "md5").unwrap(), Prop::Unknown);
}

#[test]
fn shortcut_targets() {
    let dir = tempfile::tempdir().unwrap();
    let link = write(
        dir.path(),
        "tool.lnk",
        &fx::lnk(&fx::LnkSpec {
            local_base_path: Some(r"C:\Tools\"),
            suffix: "app.exe",
            unicode_paths: true,
            arguments: None,
            working_dir: None,
            name: None,
            id_list: true,
            env_target: None,
        }),
    );
    assert_eq!(
        get(&link, "shortcut-target").unwrap(),
        s(r"C:\Tools\app.exe")
    );
    assert_eq!(get(&link, "file-signature").unwrap(), s("lnk"));
    let target = write(dir.path(), "real.txt", b"x");
    let sym = dir.path().join("sym.txt");
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(&target, &sym).is_ok();
    // Creating symlinks on Windows needs Developer Mode or the privilege.
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(&target, &sym).is_ok();
    if made {
        match get(&sym, "shortcut-target").unwrap() {
            Prop::Str(t) => assert!(t.ends_with("real.txt"), "{t}"),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(get(&target, "shortcut-target").unwrap(), Prop::Unknown);
}

#[test]
fn internet_shortcuts() {
    let dir = tempfile::tempdir().unwrap();
    let p = write(
        dir.path(),
        "site.url",
        b"[Other]\r\nURL=wrong\r\n[InternetShortcut]\r\nIconIndex=0\r\nURL=https://example.com/\r\n",
    );
    assert_eq!(get(&p, "url").unwrap(), s("https://example.com/"));
    assert_eq!(parse_url_file(b"URL=no-section"), None);
}

#[test]
fn pe_version_properties() {
    let dir = tempfile::tempdir().unwrap();
    let exe = write(
        dir.path(),
        "tool.exe",
        &fx::pe_with_version(true, &fx::VERSION_STRINGS),
    );
    assert_eq!(get(&exe, "version").unwrap(), s("1.2.3.4"));
    assert_eq!(get(&exe, "product-version").unwrap(), s("1.2 beta"));
    assert_eq!(get(&exe, "product-name").unwrap(), s("Keel Test Tool"));
    assert_eq!(get(&exe, "company").unwrap(), s("Example Corp"));
    assert_eq!(get(&exe, "copyright").unwrap(), s("(c) Example Corp"));
    assert_eq!(get(&exe, "trademarks").unwrap(), s("Example is a mark"));
    assert_eq!(get(&exe, "description").unwrap(), s("A test tool"));
    assert_eq!(get(&exe, "file-signature").unwrap(), s("exe"));
    let txt = write(dir.path(), "readme.txt", b"hello");
    assert_eq!(get(&txt, "version").unwrap(), Prop::Unknown);
}

#[test]
fn audio_properties() {
    let dir = tempfile::tempdir().unwrap();
    let mp3 = write(dir.path(), "song.mp3", &fx::mp3(3, false, 40));
    let opts = Options::default();
    let mut p = FileProps::new(&mp3, &opts);
    assert_eq!(p.get("title").unwrap(), s("Test Title"));
    assert_eq!(p.get("artist").unwrap(), s("Test Artist"));
    assert_eq!(p.get("album").unwrap(), s("Test Album"));
    assert_eq!(p.get("album-artist").unwrap(), s("Various"));
    assert_eq!(p.get("genre").unwrap(), s("Rock"));
    assert_eq!(p.get("track").unwrap(), Prop::Int(3));
    assert_eq!(p.get("year").unwrap(), Prop::Int(2004));
    assert_eq!(p.get("comment").unwrap(), s("A comment"));
    assert_eq!(p.get("audio-bitrate").unwrap(), Prop::Int(128));
    assert_eq!(p.get("audio-sample-rate").unwrap(), Prop::Int(44_100));
    assert_eq!(p.get("audio-channels").unwrap(), Prop::Int(2));
    assert_eq!(p.get("audio-format").unwrap(), s("MP3"));
    assert!(matches!(p.get("duration").unwrap(), Prop::Duration(_)));

    let flac = write(dir.path(), "tones.flac", &fx::flac());
    assert_eq!(
        get(&flac, "length").unwrap(),
        Prop::Duration(Duration::from_secs(10))
    );
    assert_eq!(get(&flac, "audio-bits-per-sample").unwrap(), Prop::Int(24));
    assert_eq!(get(&flac, "artist").unwrap(), s("One; Two"));
    let wav = write(dir.path(), "beep.wav", &fx::wav());
    assert_eq!(get(&wav, "title").unwrap(), s("Wav Title"));
    assert_eq!(
        get(&wav, "length").unwrap(),
        Prop::Duration(Duration::from_millis(250))
    );
}

#[test]
fn unknown_names_missing_files_and_unknown_values() {
    let dir = tempfile::tempdir().unwrap();
    let txt = write(dir.path(), "a.txt", b"hello");
    assert!(matches!(
        get(&txt, "no-such-function"),
        Err(Error::UnknownFunction(_))
    ));
    assert!(matches!(
        get(&dir.path().join("missing.txt"), "md5"),
        Err(Error::Io(_))
    ));
    // Functions with no reader on any platform, and image properties of a text file.
    assert_eq!(get(&txt, "aspect-ratio").unwrap(), Prop::Unknown);
    assert_eq!(get(&txt, "width").unwrap(), Prop::Unknown);
    assert_eq!(get(&txt, "artist").unwrap(), Prop::Unknown);
}

#[test]
fn canonical_names() {
    let dir = tempfile::tempdir().unwrap();
    let txt = write(dir.path(), "a.txt", b"hello");
    let size = get(&txt, "System.Size").unwrap();
    if cfg!(windows) {
        assert_eq!(size, Prop::Int(5));
    } else {
        assert_eq!(size, Prop::Unknown);
    }
    let opts = Options {
        system: false,
        ..Options::default()
    };
    assert_eq!(get_with(&txt, "System.Size", &opts).unwrap(), Prop::Unknown);
}

#[test]
fn windows_value_conversions() {
    assert_eq!(convert(Prop::Int(128_000), Conv::Kbps), Prop::Int(128));
    assert_eq!(
        convert(Prop::Int(30_000_000), Conv::Ticks),
        Prop::Duration(Duration::from_secs(3))
    );
    assert_eq!(
        convert(Prop::Int(29_970), Conv::PerThousand),
        Prop::Float(29.97)
    );
    assert_eq!(convert(Prop::Int(3), Conv::Perceived), s("audio"));
    assert_eq!(convert(s("x"), Conv::Kbps), s("x"));
}

#[test]
fn every_function_reads_without_error() {
    let dir = tempfile::tempdir().unwrap();
    let files = [
        write(dir.path(), "a.txt", b"hello world\n"),
        write(dir.path(), "a.mp3", &fx::mp3(4, true, 3)),
        write(
            dir.path(),
            "a.exe",
            &fx::pe_with_version(false, &fx::VERSION_STRINGS),
        ),
    ];
    for f in &files {
        for func in FUNCTIONS {
            let v = get(f, func.name);
            assert!(v.is_ok(), "{}: {} -> {v:?}", f.display(), func.name);
            if !func.supported() {
                assert_eq!(v.unwrap(), Prop::Unknown, "{}", func.name);
            }
        }
    }
    assert!(get(dir.path(), "perceived-type").is_ok());
}

#[test]
fn placeholders_are_not_regular_files_here() {
    let dir = tempfile::tempdir().unwrap();
    let p = write(dir.path(), "a.txt", b"x");
    assert!(!is_placeholder(&std::fs::metadata(&p).unwrap()));
}

#[test]
fn display_paths() {
    assert_eq!(display_path(Path::new(r"\\?\C:\x\y")), r"C:\x\y");
    assert_eq!(
        display_path(Path::new(r"\\?\UNC\srv\share\f")),
        r"\\srv\share\f"
    );
    assert_eq!(display_path(Path::new("relative/t")), "relative/t");
}
