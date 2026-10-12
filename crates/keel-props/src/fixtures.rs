//! Tiny generated files for tests and fuzz seeds: shell links, PE images with a version
//! resource, MP3 (ID3v2.2/2.3/2.4, Xing), ID3v1, FLAC and WAV. No real media.

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn ansi_z(s: &str) -> Vec<u8> {
    let mut v: Vec<u8> = s
        .chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect();
    v.push(0);
    v
}

fn pad4(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
}

/// What [`lnk`] puts in a shell link.
pub struct LnkSpec<'a> {
    pub local_base_path: Option<&'a str>,
    pub suffix: &'a str,
    /// Also write the Unicode copies of the LinkInfo paths (header size 0x24).
    pub unicode_paths: bool,
    pub arguments: Option<&'a str>,
    pub working_dir: Option<&'a str>,
    pub name: Option<&'a str>,
    pub id_list: bool,
    pub env_target: Option<&'a str>,
}

/// A shell link (`.lnk`) as [MS-SHLLINK] lays it out.
pub fn lnk(spec: &LnkSpec<'_>) -> Vec<u8> {
    let mut flags = 0x80u32; // IsUnicode
    if spec.id_list {
        flags |= 0x1;
    }
    if spec.local_base_path.is_some() {
        flags |= 0x2;
    }
    if spec.name.is_some() {
        flags |= 0x4;
    }
    if spec.working_dir.is_some() {
        flags |= 0x10;
    }
    if spec.arguments.is_some() {
        flags |= 0x20;
    }
    let mut b = Vec::new();
    b.extend_from_slice(&0x4cu32.to_le_bytes());
    b.extend_from_slice(&[
        0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x46,
    ]);
    b.extend_from_slice(&flags.to_le_bytes());
    b.extend_from_slice(&0x20u32.to_le_bytes()); // FILE_ATTRIBUTE_ARCHIVE
    b.extend_from_slice(&[0; 24]); // times
    b.extend_from_slice(&1234u32.to_le_bytes()); // target size
    b.extend_from_slice(&[0; 4]); // icon index
    b.extend_from_slice(&1u32.to_le_bytes()); // SW_SHOWNORMAL
    b.extend_from_slice(&[0; 12]); // hotkey, reserved
    assert_eq!(b.len(), 0x4c);
    if spec.id_list {
        // One 20-byte item (a root-folder entry) and the terminator.
        let mut item = vec![0x14, 0x00, 0x1f, 0x50];
        item.extend_from_slice(&[0xe0, 0x4f, 0xd0, 0x20, 0xea, 0x3a, 0x69, 0x10]);
        item.extend_from_slice(&[0xa2, 0xd8, 0x08, 0x00, 0x2b, 0x30, 0x30, 0x9d]);
        b.extend_from_slice(&((item.len() + 2) as u16).to_le_bytes());
        b.extend_from_slice(&item);
        b.extend_from_slice(&[0, 0]);
    }
    if let Some(base) = spec.local_base_path {
        let header = if spec.unicode_paths { 0x24u32 } else { 0x1c };
        let volume: Vec<u8> = [0x11u32, 3, 0x1234_5678, 0x10]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .chain([0])
            .collect();
        let base_a = ansi_z(base);
        let suffix_a = ansi_z(spec.suffix);
        let volume_off = header;
        let base_off = volume_off + volume.len() as u32;
        let suffix_off = base_off + base_a.len() as u32;
        let mut tail = Vec::new();
        tail.extend_from_slice(&volume);
        tail.extend_from_slice(&base_a);
        tail.extend_from_slice(&suffix_a);
        let mut uni = Vec::new();
        let (base_u_off, suffix_u_off);
        if spec.unicode_paths {
            let start = suffix_off + suffix_a.len() as u32;
            let bu = utf16z(base);
            base_u_off = start;
            suffix_u_off = start + bu.len() as u32;
            uni.extend_from_slice(&bu);
            uni.extend_from_slice(&utf16z(spec.suffix));
        } else {
            (base_u_off, suffix_u_off) = (0, 0);
        }
        let size = header as usize + tail.len() + uni.len();
        let mut li = Vec::new();
        for v in [size as u32, header, 1, volume_off, base_off, 0, suffix_off] {
            li.extend_from_slice(&v.to_le_bytes());
        }
        if spec.unicode_paths {
            li.extend_from_slice(&base_u_off.to_le_bytes());
            li.extend_from_slice(&suffix_u_off.to_le_bytes());
        }
        li.extend_from_slice(&tail);
        li.extend_from_slice(&uni);
        assert_eq!(li.len(), size);
        b.extend_from_slice(&li);
    }
    for s in [spec.name, spec.working_dir, spec.arguments]
        .into_iter()
        .flatten()
    {
        let units: Vec<u16> = s.encode_utf16().collect();
        b.extend_from_slice(&(units.len() as u16).to_le_bytes());
        b.extend(units.iter().flat_map(|u| u.to_le_bytes()));
    }
    if let Some(t) = spec.env_target {
        let mut block = Vec::new();
        block.extend_from_slice(&0x314u32.to_le_bytes());
        block.extend_from_slice(&0xa000_0001u32.to_le_bytes());
        let mut a = ansi_z(t);
        a.resize(260, 0);
        let mut u = utf16z(t);
        u.resize(520, 0);
        block.extend_from_slice(&a);
        block.extend_from_slice(&u);
        assert_eq!(block.len(), 0x314);
        b.extend_from_slice(&block);
    }
    b.extend_from_slice(&[0; 4]); // TerminalBlock
    b
}

/// The StringFileInfo entries [`pe_with_version`] tests use.
pub const VERSION_STRINGS: [(&str, &str); 7] = [
    ("CompanyName", "Example Corp"),
    ("FileDescription", "A test tool"),
    ("FileVersion", "1.2.3.4"),
    ("LegalCopyright", "(c) Example Corp"),
    ("LegalTrademarks", "Example is a mark"),
    ("ProductName", "Keel Test Tool"),
    ("ProductVersion", "1.2 beta"),
];

/// A version block: header, key, value, children (each child already 4-byte padded).
fn ver_block(key: &str, value: &[u8], value_len: u16, text: bool, children: &[u8]) -> Vec<u8> {
    let mut b = vec![0, 0];
    b.extend_from_slice(&value_len.to_le_bytes());
    b.extend_from_slice(&u16::from(text).to_le_bytes());
    b.extend_from_slice(&utf16z(key));
    pad4(&mut b);
    b.extend_from_slice(value);
    if !children.is_empty() {
        pad4(&mut b);
        b.extend_from_slice(children);
    }
    let len = b.len() as u16;
    b[..2].copy_from_slice(&len.to_le_bytes());
    pad4(&mut b);
    b
}

/// A `VS_VERSIONINFO` resource with version 1.2.3.4 and `strings`.
pub fn version_resource(strings: &[(&str, &str)]) -> Vec<u8> {
    let mut fixed = Vec::new();
    for v in [
        0xfeef_04bdu32,
        0x0001_0000,
        (1 << 16) | 2,
        (3 << 16) | 4,
        (1 << 16) | 2,
        (3 << 16) | 4,
        0x3f,
        0,
        0x0004_0004,
        1,
        0,
        0,
        0,
    ] {
        fixed.extend_from_slice(&v.to_le_bytes());
    }
    let mut children = Vec::new();
    if !strings.is_empty() {
        let entries: Vec<u8> = strings
            .iter()
            .flat_map(|(k, v)| {
                let value = utf16z(v);
                ver_block(k, &value, (value.len() / 2) as u16, true, &[])
            })
            .collect();
        let table = ver_block("040904b0", &[], 0, true, &entries);
        children.extend(ver_block("StringFileInfo", &[], 0, true, &table));
    }
    let translation = ver_block("Translation", &[0x09, 0x04, 0xb0, 0x04], 4, false, &[]);
    children.extend(ver_block("VarFileInfo", &[], 0, true, &translation));
    ver_block("VS_VERSION_INFO", &fixed, 52, false, &children)
}

fn pe_image(pe32_plus: bool, resource: Option<&[u8]>) -> Vec<u8> {
    let mut b = vec![0u8; 0x40];
    b[..2].copy_from_slice(b"MZ");
    b[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
    let dirs_at = if pe32_plus { 112 } else { 96 };
    let opt_size = dirs_at + 16 * 8;
    b.extend_from_slice(b"PE\0\0");
    b.extend_from_slice(&(if pe32_plus { 0x8664u16 } else { 0x14c }).to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // sections
    b.extend_from_slice(&[0; 12]);
    b.extend_from_slice(&(opt_size as u16).to_le_bytes());
    b.extend_from_slice(&0x0102u16.to_le_bytes());
    let mut opt = vec![0u8; opt_size];
    opt[..2].copy_from_slice(&(if pe32_plus { 0x20bu16 } else { 0x10b }).to_le_bytes());
    opt[dirs_at - 4..dirs_at].copy_from_slice(&16u32.to_le_bytes());
    // The resource section: three directory levels, a data entry, then the resource.
    let mut rsrc = Vec::new();
    if let Some(res) = resource {
        let dir = |id: u32, off: u32| -> Vec<u8> {
            let mut d = vec![0u8; 12];
            d.extend_from_slice(&0u16.to_le_bytes());
            d.extend_from_slice(&1u16.to_le_bytes());
            d.extend_from_slice(&id.to_le_bytes());
            d.extend_from_slice(&off.to_le_bytes());
            d
        };
        rsrc.extend(dir(16, 0x8000_0018));
        rsrc.extend(dir(1, 0x8000_0030));
        rsrc.extend(dir(0x409, 0x48));
        rsrc.extend_from_slice(&(0x1000u32 + 0x58).to_le_bytes());
        rsrc.extend_from_slice(&(res.len() as u32).to_le_bytes());
        rsrc.extend_from_slice(&[0; 8]);
        assert_eq!(rsrc.len(), 0x58);
        rsrc.extend_from_slice(res);
        opt[dirs_at + 16..dirs_at + 20].copy_from_slice(&0x1000u32.to_le_bytes());
        opt[dirs_at + 20..dirs_at + 24].copy_from_slice(&(rsrc.len() as u32).to_le_bytes());
    }
    b.extend_from_slice(&opt);
    let raw_size = (rsrc.len().max(1) + 0x1ff) & !0x1ff;
    let mut section = Vec::new();
    section.extend_from_slice(b".rsrc\0\0\0");
    for v in [rsrc.len() as u32, 0x1000, raw_size as u32, 0x200, 0, 0, 0] {
        section.extend_from_slice(&v.to_le_bytes());
    }
    section.extend_from_slice(&0x4000_0040u32.to_le_bytes());
    assert_eq!(section.len(), 40);
    b.extend_from_slice(&section);
    b.resize(0x200, 0);
    rsrc.resize(raw_size, 0);
    b.extend_from_slice(&rsrc);
    b
}

/// A PE32 (or PE32+) image whose only section holds a version resource.
pub fn pe_with_version(pe32_plus: bool, strings: &[(&str, &str)]) -> Vec<u8> {
    pe_image(pe32_plus, Some(&version_resource(strings)))
}

/// A PE32 image without resources.
pub fn pe_without_resources() -> Vec<u8> {
    pe_image(false, None)
}

fn synchsafe(n: usize) -> [u8; 4] {
    [
        ((n >> 21) & 0x7f) as u8,
        ((n >> 14) & 0x7f) as u8,
        ((n >> 7) & 0x7f) as u8,
        (n & 0x7f) as u8,
    ]
}

/// One ID3v2 frame for version `major`.
fn id3_frame(major: u8, id: &str, data: &[u8]) -> Vec<u8> {
    let mut f = Vec::new();
    if major == 2 {
        f.extend_from_slice(&id.as_bytes()[..3]);
        f.extend_from_slice(&(data.len() as u32).to_be_bytes()[1..]);
    } else {
        f.extend_from_slice(id.as_bytes());
        if major == 4 {
            f.extend_from_slice(&synchsafe(data.len()));
        } else {
            f.extend_from_slice(&(data.len() as u32).to_be_bytes());
        }
        f.extend_from_slice(&[0, 0]);
    }
    f.extend_from_slice(data);
    f
}

fn latin1(s: &str) -> Vec<u8> {
    let mut v = vec![0u8];
    v.extend(s.bytes());
    v
}

/// An ID3v2.`major` tag (title, artist, album, album artist, genre `(17)`, track `3/12`,
/// year 2004, a comment) followed by `frames` MPEG-1 Layer III frames at 128 kbit/s,
/// 44.1 kHz, stereo; with `xing`, the first frame is a Xing header saying 1,000 frames.
pub fn mp3(major: u8, xing: bool, frames: usize) -> Vec<u8> {
    let ids: [&str; 8] = if major == 2 {
        ["TT2", "TP1", "TAL", "TP2", "TCO", "TRK", "TYE", "COM"]
    } else {
        let year = if major == 4 { "TDRC" } else { "TYER" };
        ["TIT2", "TPE1", "TALB", "TPE2", "TCON", "TRCK", year, "COMM"]
    };
    let mut body = Vec::new();
    // Title in UTF-8 (2.4) or Latin-1; artist in UTF-16 with a byte order mark.
    let title = if major == 4 {
        let mut v = vec![3u8];
        v.extend_from_slice("Test Title".as_bytes());
        v
    } else {
        latin1("Test Title")
    };
    body.extend(id3_frame(major, ids[0], &title));
    let mut artist = vec![1u8, 0xff, 0xfe];
    artist.extend("Test Artist".encode_utf16().flat_map(u16::to_le_bytes));
    body.extend(id3_frame(major, ids[1], &artist));
    body.extend(id3_frame(major, ids[2], &latin1("Test Album")));
    body.extend(id3_frame(major, ids[3], &latin1("Various")));
    body.extend(id3_frame(major, ids[4], &latin1("(17)")));
    body.extend(id3_frame(major, ids[5], &latin1("3/12")));
    body.extend(id3_frame(major, ids[6], &latin1("2004")));
    if major == 3 {
        body.extend(id3_frame(major, "COMM", b"\0engiTunNORM\0 0000 0000"));
    }
    body.extend(id3_frame(major, ids[7], b"\0eng\0A comment"));
    body.extend_from_slice(&[0; 32]); // padding
    let mut b = b"ID3".to_vec();
    b.extend_from_slice(&[major, 0, 0]);
    b.extend_from_slice(&synchsafe(body.len()));
    b.extend_from_slice(&body);
    for i in 0..frames {
        let mut frame = vec![0u8; 417];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x00]);
        if xing && i == 0 {
            frame[36..40].copy_from_slice(b"Xing");
            frame[40..44].copy_from_slice(&3u32.to_be_bytes());
            frame[44..48].copy_from_slice(&1000u32.to_be_bytes());
            frame[48..52].copy_from_slice(&(1000u32 * 417).to_be_bytes());
        }
        b.extend_from_slice(&frame);
    }
    b
}

/// An ID3v1.1 tag (128 bytes).
pub fn id3v1(title: &str, artist: &str, album: &str, year: &str, track: u8, genre: u8) -> Vec<u8> {
    let field = |s: &str, n: usize| {
        let mut v = s.as_bytes().to_vec();
        v.resize(n, 0);
        v
    };
    let mut b = b"TAG".to_vec();
    b.extend(field(title, 30));
    b.extend(field(artist, 30));
    b.extend(field(album, 30));
    b.extend(field(year, 4));
    b.extend(field("", 28));
    b.push(0);
    b.push(track);
    b.push(genre);
    assert_eq!(b.len(), 128);
    b
}

/// A FLAC stream: 48 kHz, stereo, 24-bit, 10 s; Vorbis comments with a title, two artists,
/// track 5, date 2020-01-01 and genre Ambient.
pub fn flac() -> Vec<u8> {
    let mut b = b"fLaC".to_vec();
    let rate = 48_000u32;
    let (channels, bits, total) = (2u32, 24u32, 480_000u64);
    let mut si = vec![0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0];
    si.push((rate >> 12) as u8);
    si.push((rate >> 4) as u8);
    si.push((((rate & 0xf) << 4) | ((channels - 1) << 1) | ((bits - 1) >> 4)) as u8);
    si.push(((((bits - 1) & 0xf) << 4) as u8) | ((total >> 32) & 0xf) as u8);
    si.extend_from_slice(&(total as u32).to_be_bytes());
    si.extend_from_slice(&[0; 16]);
    assert_eq!(si.len(), 34);
    b.push(0);
    b.extend_from_slice(&(si.len() as u32).to_be_bytes()[1..]);
    b.extend_from_slice(&si);
    let mut vc = Vec::new();
    vc.extend_from_slice(&4u32.to_le_bytes());
    vc.extend_from_slice(b"keel");
    let comments = [
        "TITLE=Flac Title",
        "ARTIST=One",
        "ARTIST=Two",
        "TRACKNUMBER=5",
        "DATE=2020-01-01",
        "GENRE=Ambient",
    ];
    vc.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        vc.extend_from_slice(&(c.len() as u32).to_le_bytes());
        vc.extend_from_slice(c.as_bytes());
    }
    b.push(0x84);
    b.extend_from_slice(&(vc.len() as u32).to_be_bytes()[1..]);
    b.extend_from_slice(&vc);
    b.extend_from_slice(&[0xff, 0xf8, 0, 0]);
    b.resize(b.len() + 96, 0);
    b
}

/// A WAV file: PCM, 8 kHz, mono, 16-bit, 0.25 s of silence, LIST/INFO title and artist.
pub fn wav() -> Vec<u8> {
    let mut chunks = Vec::new();
    chunks.extend_from_slice(b"fmt ");
    chunks.extend_from_slice(&16u32.to_le_bytes());
    chunks.extend_from_slice(&1u16.to_le_bytes());
    chunks.extend_from_slice(&1u16.to_le_bytes());
    chunks.extend_from_slice(&8000u32.to_le_bytes());
    chunks.extend_from_slice(&16_000u32.to_le_bytes());
    chunks.extend_from_slice(&2u16.to_le_bytes());
    chunks.extend_from_slice(&16u16.to_le_bytes());
    let mut info = b"INFO".to_vec();
    for (id, v) in [(b"INAM", "Wav Title"), (b"IART", "Wav Artist")] {
        let mut value = v.as_bytes().to_vec();
        value.push(0);
        info.extend_from_slice(id);
        info.extend_from_slice(&(value.len() as u32).to_le_bytes());
        info.extend_from_slice(&value);
        if value.len() % 2 == 1 {
            info.push(0);
        }
    }
    chunks.extend_from_slice(b"LIST");
    chunks.extend_from_slice(&(info.len() as u32).to_le_bytes());
    chunks.extend_from_slice(&info);
    chunks.extend_from_slice(b"data");
    chunks.extend_from_slice(&4000u32.to_le_bytes());
    chunks.resize(chunks.len() + 4000, 0);
    let mut b = b"RIFF".to_vec();
    b.extend_from_slice(&((chunks.len() + 4) as u32).to_le_bytes());
    b.extend_from_slice(b"WAVE");
    b.extend_from_slice(&chunks);
    b
}
