//! Audio tags and stream properties without a media library: ID3v2 (2.2, 2.3, 2.4) and ID3v1
//! tags, MPEG audio frames (with Xing/Info/VBRI headers for VBR length), FLAC STREAMINFO and
//! Vorbis comments, and WAV (`fmt `, `data`, `LIST/INFO` and embedded ID3 chunks).
//!
//! Reads only what it needs: the ID3v2 tag (at most [`MAX_TAG_BYTES`]), the first
//! [`FRAME_SCAN_BYTES`] of audio, FLAC metadata blocks and the last 128 bytes.

use std::io::{self, Read, Seek, SeekFrom};
use std::time::Duration;

/// Largest ID3v2 tag or Vorbis comment block read (cover art beyond it is not needed).
pub const MAX_TAG_BYTES: usize = 4 << 20;
/// Bytes searched for the first MPEG frame after the tag.
pub const FRAME_SCAN_BYTES: usize = 64 * 1024;
const MAX_VALUE_CHARS: usize = 4096;
const MAX_BLOCKS: usize = 256;

/// Tags and stream properties of an audio file. Fields are `None` when the file does not say.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioInfo {
    /// `MP3`, `MP2`, `MP1`, `FLAC`, `PCM`, `IEEE float` or `WAV` (other WAV codecs).
    pub format: Option<&'static str>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub comment: Option<String>,
    pub composer: Option<String>,
    pub conductor: Option<String>,
    pub publisher: Option<String>,
    pub encoded_by: Option<String>,
    pub subtitle: Option<String>,
    pub language: Option<String>,
    pub mood: Option<String>,
    pub initial_key: Option<String>,
    pub writer: Option<String>,
    pub part_of_set: Option<String>,
    pub track: Option<u32>,
    pub year: Option<i32>,
    pub bpm: Option<u32>,
    pub compilation: Option<bool>,
    pub length: Option<Duration>,
    /// Average bit rate in kbit/s.
    pub bitrate: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub bits_per_sample: Option<u16>,
}

/// Which tag field a frame or comment fills.
#[derive(Clone, Copy)]
enum Field {
    Title,
    Artist,
    Album,
    AlbumArtist,
    Genre,
    Comment,
    Composer,
    Conductor,
    Publisher,
    EncodedBy,
    Subtitle,
    Language,
    Mood,
    InitialKey,
    Writer,
    PartOfSet,
    Track,
    Year,
    Bpm,
    Compilation,
    LengthMs,
}

impl AudioInfo {
    /// Sets a field unless an earlier (better) source already set it.
    fn set(&mut self, field: Field, value: &str) {
        let value = value.trim_matches(|c: char| c == '\0' || c.is_whitespace());
        if value.is_empty() {
            return;
        }
        let value: String = value.chars().take(MAX_VALUE_CHARS).collect();
        let slot = match field {
            Field::Title => &mut self.title,
            Field::Artist => &mut self.artist,
            Field::Album => &mut self.album,
            Field::AlbumArtist => &mut self.album_artist,
            Field::Genre => {
                if self.genre.is_none() {
                    self.genre = Some(genre_name(&value));
                }
                return;
            }
            Field::Comment => &mut self.comment,
            Field::Composer => &mut self.composer,
            Field::Conductor => &mut self.conductor,
            Field::Publisher => &mut self.publisher,
            Field::EncodedBy => &mut self.encoded_by,
            Field::Subtitle => &mut self.subtitle,
            Field::Language => &mut self.language,
            Field::Mood => &mut self.mood,
            Field::InitialKey => &mut self.initial_key,
            Field::Writer => &mut self.writer,
            Field::PartOfSet => &mut self.part_of_set,
            Field::Track => {
                self.track = self.track.or_else(|| leading_number(&value));
                return;
            }
            Field::Year => {
                self.year = self.year.or_else(|| year(&value));
                return;
            }
            Field::Bpm => {
                self.bpm = self
                    .bpm
                    .or_else(|| value.parse::<f64>().ok().map(|f| f.round() as u32));
                return;
            }
            Field::Compilation => {
                self.compilation = self
                    .compilation
                    .or(Some(matches!(value.as_str(), "1" | "true" | "TRUE" | "yes")));
                return;
            }
            Field::LengthMs => {
                if self.length.is_none() {
                    self.length = value
                        .parse::<u64>()
                        .ok()
                        .filter(|&ms| ms > 0)
                        .map(Duration::from_millis);
                }
                return;
            }
        };
        if slot.is_none() {
            *slot = Some(value);
        }
    }
}

fn leading_number(s: &str) -> Option<u32> {
    let digits: String = s.trim().chars().take_while(char::is_ascii_digit).take(9).collect();
    digits.parse().ok()
}

/// The year in `2004`, `2004-05-01` or `2004-05-01T12:00`.
fn year(s: &str) -> Option<i32> {
    let s = s.trim();
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    (digits.len() == 4).then(|| digits.parse().ok()).flatten()
}

fn read_at<R: Read + Seek>(r: &mut R, len: u64, off: u64, max: usize) -> io::Result<Vec<u8>> {
    if off >= len {
        return Ok(Vec::new());
    }
    let n = (len - off).min(max as u64) as usize;
    r.seek(SeekFrom::Start(off))?;
    let mut buf = Vec::with_capacity(n);
    r.take(n as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Reads tags and stream properties from an audio file `len` bytes long.
pub fn read<R: Read + Seek>(r: &mut R, len: u64) -> io::Result<AudioInfo> {
    let mut info = AudioInfo::default();
    let head = read_at(r, len, 0, 12)?;
    let mut audio_start = 0u64;
    if head.starts_with(b"ID3") && head.len() >= 10 {
        let size = synchsafe(&head[6..10]).unwrap_or(0);
        let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
        audio_start = 10 + u64::from(size) + footer;
        let tag = read_at(r, len, 0, (audio_start as usize).min(MAX_TAG_BYTES))?;
        parse_id3v2(&tag, &mut info);
    }
    let start = read_at(r, len, audio_start, 12)?;
    if start.starts_with(b"fLaC") {
        flac(r, len, audio_start + 4, &mut info)?;
    } else if start.starts_with(b"RIFF") && start.get(8..12) == Some(b"WAVE") {
        wav(r, len, audio_start, &mut info)?;
    } else {
        let has_v1 = len >= 128 && read_at(r, len, len - 128, 3)? == b"TAG";
        let audio_end = if has_v1 { len - 128 } else { len };
        let scan = read_at(r, len, audio_start, FRAME_SCAN_BYTES)?;
        mpeg(&scan, audio_start, audio_end, &mut info);
    }
    if len >= 128 {
        let tail = read_at(r, len, len - 128, 128)?;
        id3v1(&tail, &mut info);
    }
    Ok(info)
}

fn synchsafe(b: &[u8]) -> Option<u32> {
    if b.len() < 4 || b.iter().take(4).any(|&x| x & 0x80 != 0) {
        return None;
    }
    Some(b.iter().take(4).fold(0u32, |n, &x| (n << 7) | u32::from(x)))
}

fn be_u24(b: &[u8]) -> usize {
    (usize::from(b[0]) << 16) | (usize::from(b[1]) << 8) | usize::from(b[2])
}

fn be_u32(b: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(..4)?.try_into().ok()?))
}

fn le_u32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off.checked_add(4)?)?.try_into().ok()?))
}

fn le_u16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off.checked_add(2)?)?.try_into().ok()?))
}

/// Removes ID3 unsynchronisation (`FF 00` becomes `FF`).
fn unsync(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut prev = 0u8;
    for &x in b {
        if !(prev == 0xff && x == 0) {
            out.push(x);
        }
        prev = x;
    }
    out
}

/// Decodes ID3v2 text in `enc` (0 Latin-1, 1 UTF-16 with BOM, 2 UTF-16BE, 3 UTF-8), keeping
/// NULs as value separators.
fn decode_text(enc: u8, b: &[u8]) -> String {
    match enc {
        1 | 2 => {
            let mut le = enc == 1;
            let mut out = String::new();
            let units = b.chunks_exact(2).take(MAX_VALUE_CHARS * 4).map(|c| [c[0], c[1]]);
            let mut buf: Vec<u16> = Vec::new();
            for pair in units {
                // Each value of a UTF-16 frame may start with its own byte order mark.
                if enc == 1 && pair == [0xff, 0xfe] {
                    le = true;
                    continue;
                }
                if enc == 1 && pair == [0xfe, 0xff] {
                    le = false;
                    continue;
                }
                buf.push(if le {
                    u16::from_le_bytes(pair)
                } else {
                    u16::from_be_bytes(pair)
                });
            }
            out.extend(char::decode_utf16(buf).map(|c| c.unwrap_or('\u{fffd}')));
            out
        }
        3 => String::from_utf8_lossy(b).into_owned(),
        _ => b.iter().map(|&c| char::from(c)).collect(),
    }
}

/// Text values of a text frame (several in ID3v2.4, NUL-separated), joined with `; `.
fn text_values(data: &[u8]) -> Option<String> {
    let (&enc, rest) = data.split_first()?;
    let text = decode_text(enc, rest);
    let values: Vec<&str> = text
        .split('\0')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .collect();
    (!values.is_empty()).then(|| values.join("; "))
}

/// A COMM frame's text, unless its description marks it as tool data (`iTunNORM`).
fn comment(data: &[u8]) -> Option<String> {
    let (&enc, rest) = data.split_first()?;
    let text = decode_text(enc, rest.get(3..)?);
    let (desc, body) = text.split_once('\0')?;
    if desc.starts_with("iTun") {
        return None;
    }
    let body = body.trim_matches('\0').trim();
    (!body.is_empty()).then(|| body.to_owned())
}

fn frame_field(id: &[u8]) -> Option<Field> {
    Some(match id {
        b"TIT2" | b"TT2" => Field::Title,
        b"TPE1" | b"TP1" => Field::Artist,
        b"TALB" | b"TAL" => Field::Album,
        b"TPE2" | b"TP2" => Field::AlbumArtist,
        b"TCON" | b"TCO" => Field::Genre,
        b"TCOM" | b"TCM" => Field::Composer,
        b"TPE3" | b"TP3" => Field::Conductor,
        b"TPUB" | b"TPB" => Field::Publisher,
        b"TENC" | b"TEN" => Field::EncodedBy,
        b"TIT3" | b"TT3" => Field::Subtitle,
        b"TLAN" | b"TLA" => Field::Language,
        b"TMOO" => Field::Mood,
        b"TKEY" | b"TKE" => Field::InitialKey,
        b"TEXT" | b"TXT" => Field::Writer,
        b"TPOS" | b"TPA" => Field::PartOfSet,
        b"TRCK" | b"TRK" => Field::Track,
        b"TYER" | b"TYE" | b"TDRC" | b"TORY" | b"TDOR" => Field::Year,
        b"TBPM" | b"TBP" => Field::Bpm,
        b"TCMP" | b"TCP" => Field::Compilation,
        b"TLEN" | b"TLE" => Field::LengthMs,
        _ => return None,
    })
}

/// Parses an ID3v2 tag (the whole tag, header included, possibly cut short).
pub fn parse_id3v2(tag: &[u8], info: &mut AudioInfo) {
    if tag.len() < 10 || !tag.starts_with(b"ID3") {
        return;
    }
    let major = tag[3];
    let flags = tag[5];
    if !(2..=4).contains(&major) {
        return;
    }
    let size = synchsafe(&tag[6..10]).unwrap_or(0) as usize;
    let body = &tag[10..tag.len().min(10 + size)];
    let owned;
    let mut body: &[u8] = if flags & 0x80 != 0 && major < 4 {
        owned = unsync(body);
        &owned
    } else {
        body
    };
    if flags & 0x40 != 0 {
        match major {
            2 => return, // ID3v2.2: the bit means a compressed tag
            3 => {
                let n = be_u32(body).map_or(usize::MAX, |n| n as usize);
                body = body.get(n.saturating_add(4)..).unwrap_or(&[]);
            }
            _ => {
                let n = body.get(..4).and_then(synchsafe).map_or(usize::MAX, |n| n as usize);
                body = body.get(n..).unwrap_or(&[]);
            }
        }
    }
    let header = if major == 2 { 6 } else { 10 };
    let mut pos = 0usize;
    let mut comments: Vec<String> = Vec::new();
    while pos + header <= body.len() {
        let h = &body[pos..pos + header];
        let id_len = if major == 2 { 3 } else { 4 };
        let id = &h[..id_len];
        if !id.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
            break; // padding or garbage
        }
        let size = match major {
            2 => be_u24(&h[3..6]),
            3 => be_u32(&h[4..8]).unwrap_or(0) as usize,
            // ID3v2.4 sizes are synchsafe; some writers store plain integers.
            _ => synchsafe(&h[4..8]).map_or_else(|| be_u32(&h[4..8]).unwrap_or(0), |n| n)
                as usize,
        };
        let start = pos + header;
        let Some(end) = start.checked_add(size).filter(|&e| e <= body.len()) else {
            break;
        };
        pos = end;
        let mut data: &[u8] = &body[start..end];
        let fmt_flags = if major == 2 { 0 } else { h[9] };
        let frame_owned;
        match major {
            3 => {
                if fmt_flags & 0xc0 != 0 {
                    continue; // compressed or encrypted
                }
                if fmt_flags & 0x20 != 0 {
                    data = data.get(1..).unwrap_or(&[]);
                }
            }
            4 => {
                if fmt_flags & 0x0c != 0 {
                    continue; // compressed or encrypted
                }
                if fmt_flags & 0x40 != 0 {
                    data = data.get(1..).unwrap_or(&[]);
                }
                if fmt_flags & 0x01 != 0 {
                    data = data.get(4..).unwrap_or(&[]);
                }
                if fmt_flags & 0x02 != 0 || flags & 0x80 != 0 {
                    frame_owned = unsync(data);
                    data = &frame_owned;
                }
            }
            _ => {}
        }
        if id == b"COMM" || id == b"COM" {
            if let Some(c) = comment(data) {
                comments.push(c);
            }
            continue;
        }
        if let Some(field) = frame_field(id) {
            if let Some(v) = text_values(data) {
                info.set(field, &v);
            }
        }
    }
    if let Some(c) = comments.first() {
        info.set(Field::Comment, c);
    }
}

/// Parses an ID3v1 tag (the last 128 bytes of the file); fills only what is still unset.
pub fn id3v1(tail: &[u8], info: &mut AudioInfo) {
    if tail.len() != 128 || !tail.starts_with(b"TAG") {
        return;
    }
    let s = |r: std::ops::Range<usize>| {
        let raw = &tail[r];
        let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        crate::lnk::decode_ansi(&raw[..end])
    };
    info.set(Field::Title, &s(3..33));
    info.set(Field::Artist, &s(33..63));
    info.set(Field::Album, &s(63..93));
    info.set(Field::Year, &s(93..97));
    if tail[125] == 0 && tail[126] != 0 {
        info.set(Field::Comment, &s(97..125));
        info.track = info.track.or(Some(u32::from(tail[126])));
    } else {
        info.set(Field::Comment, &s(97..127));
    }
    if let Some(name) = GENRES.get(usize::from(tail[127])) {
        info.set(Field::Genre, name);
    }
}

/// `(13)`, `13`, `(13)Pop`, `(RX)` or a name, as a genre name.
fn genre_name(raw: &str) -> String {
    let lookup = |n: &str| -> Option<String> {
        match n {
            "RX" => Some("Remix".into()),
            "CR" => Some("Cover".into()),
            _ => GENRES.get(n.parse::<usize>().ok()?).map(|s| (*s).to_owned()),
        }
    };
    if let Some(rest) = raw.strip_prefix('(') {
        if let Some((n, after)) = rest.split_once(')') {
            let after = after.trim();
            if !after.is_empty() {
                return after.to_owned();
            }
            if let Some(name) = lookup(n) {
                return name;
            }
        }
    }
    lookup(raw).unwrap_or_else(|| raw.to_owned())
}

/// ID3v1 genres (0-79) and the Winamp extensions (80-125).
const GENRES: [&str; 126] = [
    "Blues", "Classic Rock", "Country", "Dance", "Disco", "Funk", "Grunge", "Hip-Hop", "Jazz",
    "Metal", "New Age", "Oldies", "Other", "Pop", "R&B", "Rap", "Reggae", "Rock", "Techno",
    "Industrial", "Alternative", "Ska", "Death Metal", "Pranks", "Soundtrack", "Euro-Techno",
    "Ambient", "Trip-Hop", "Vocal", "Jazz+Funk", "Fusion", "Trance", "Classical", "Instrumental",
    "Acid", "House", "Game", "Sound Clip", "Gospel", "Noise", "AlternRock", "Bass", "Soul",
    "Punk", "Space", "Meditative", "Instrumental Pop", "Instrumental Rock", "Ethnic", "Gothic",
    "Darkwave", "Techno-Industrial", "Electronic", "Pop-Folk", "Eurodance", "Dream",
    "Southern Rock", "Comedy", "Cult", "Gangsta", "Top 40", "Christian Rap", "Pop/Funk",
    "Jungle", "Native American", "Cabaret", "New Wave", "Psychadelic", "Rave", "Showtunes",
    "Trailer", "Lo-Fi", "Tribal", "Acid Punk", "Acid Jazz", "Polka", "Retro", "Musical",
    "Rock & Roll", "Hard Rock", "Folk", "Folk-Rock", "National Folk", "Swing", "Fast Fusion",
    "Bebob", "Latin", "Revival", "Celtic", "Bluegrass", "Avantgarde", "Gothic Rock",
    "Progressive Rock", "Psychedelic Rock", "Symphonic Rock", "Slow Rock", "Big Band", "Chorus",
    "Easy Listening", "Acoustic", "Humour", "Speech", "Chanson", "Opera", "Chamber Music",
    "Sonata", "Symphony", "Booty Bass", "Primus", "Porn Groove", "Satire", "Slow Jam", "Club",
    "Tango", "Samba", "Folklore", "Ballad", "Power Ballad", "Rhythmic Soul", "Freestyle", "Duet",
    "Punk Rock", "Drum Solo", "A capella", "Euro-House", "Dance Hall",
];

/// One MPEG audio frame header.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Frame {
    /// 1 (MPEG-1), 2 (MPEG-2) or 25 (MPEG-2.5).
    version: u8,
    layer: u8,
    bitrate: u32,
    sample_rate: u32,
    channels: u16,
    len: usize,
    samples: u32,
}

fn frame_header(h: &[u8]) -> Option<Frame> {
    if h.len() < 4 || h[0] != 0xff || h[1] & 0xe0 != 0xe0 {
        return None;
    }
    let version = match (h[1] >> 3) & 3 {
        0 => 25,
        2 => 2,
        3 => 1,
        _ => return None,
    };
    let layer = match (h[1] >> 1) & 3 {
        1 => 3,
        2 => 2,
        3 => 1,
        _ => return None,
    };
    let bi = usize::from(h[2] >> 4);
    let si = usize::from((h[2] >> 2) & 3);
    if bi == 0 || bi == 15 || si == 3 {
        return None;
    }
    const V1: [[u32; 15]; 3] = [
        [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448],
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384],
        [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
    ];
    const V2: [[u32; 15]; 2] = [
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    ];
    let bitrate = if version == 1 {
        V1[usize::from(layer - 1)][bi]
    } else {
        V2[usize::from(layer != 1)][bi]
    };
    let sample_rate = [44_100, 48_000, 32_000][si]
        / match version {
            1 => 1,
            2 => 2,
            _ => 4,
        };
    let padding = u32::from((h[2] >> 1) & 1);
    let channels = if h[3] >> 6 == 3 { 1 } else { 2 };
    let (len, samples) = match layer {
        1 => ((12 * bitrate * 1000 / sample_rate + padding) * 4, 384),
        2 => (144 * bitrate * 1000 / sample_rate + padding, 1152),
        _ if version == 1 => (144 * bitrate * 1000 / sample_rate + padding, 1152),
        _ => (72 * bitrate * 1000 / sample_rate + padding, 576),
    };
    Some(Frame {
        version,
        layer,
        bitrate,
        sample_rate,
        channels,
        len: len as usize,
        samples,
    })
}

/// The first MPEG frame in `scan` (confirmed by the next one when it fits) and its stream
/// properties: Xing/Info or VBRI frame counts for VBR files, else a constant bit rate.
fn mpeg(scan: &[u8], scan_start: u64, audio_end: u64, info: &mut AudioInfo) {
    let mut found = None;
    for i in 0..scan.len().saturating_sub(3) {
        let Some(f) = frame_header(&scan[i..]) else {
            continue;
        };
        if f.len < 4 {
            continue;
        }
        match scan.get(i + f.len..).and_then(|rest| {
            (rest.len() >= 4).then(|| frame_header(rest))
        }) {
            Some(Some(next))
                if next.version == f.version
                    && next.layer == f.layer
                    && next.sample_rate == f.sample_rate =>
            {
                found = Some((i, f));
                break;
            }
            Some(_) => continue,
            None => {
                found = Some((i, f));
                break;
            }
        }
    }
    let Some((i, f)) = found else {
        return;
    };
    if info.format.is_none() {
        info.format = Some(match f.layer {
            3 => "MP3",
            2 => "MP2",
            _ => "MP1",
        });
    }
    info.sample_rate = Some(f.sample_rate);
    info.channels = Some(f.channels);
    let audio_bytes = audio_end.saturating_sub(scan_start + i as u64);
    let side = match (f.version, f.channels) {
        (1, 1) => 17,
        (1, _) => 32,
        (_, 1) => 9,
        _ => 17,
    };
    let frame = &scan[i..scan.len().min(i + f.len)];
    let mut frames = None;
    let mut bytes = None;
    if let Some(x) = frame.get(4 + side..) {
        if x.starts_with(b"Xing") || x.starts_with(b"Info") {
            let flags = be_u32(&x[4..]).unwrap_or(0);
            let mut p = 8;
            if flags & 1 != 0 {
                frames = x.get(p..).and_then(be_u32);
                p += 4;
            }
            if flags & 2 != 0 {
                bytes = x.get(p..).and_then(be_u32);
            }
        }
    }
    if let Some(v) = frame.get(36..).filter(|v| v.starts_with(b"VBRI")) {
        bytes = v.get(10..).and_then(be_u32);
        frames = v.get(14..).and_then(be_u32);
    }
    match frames.filter(|&n| n > 0) {
        Some(n) => {
            let secs = f64::from(n) * f64::from(f.samples) / f64::from(f.sample_rate);
            info.length = Some(Duration::from_secs_f64(secs));
            let total = bytes.map_or(audio_bytes, u64::from);
            if secs > 0.0 {
                info.bitrate = Some((total as f64 * 8.0 / secs / 1000.0).round() as u32);
            }
        }
        None => {
            info.bitrate = Some(f.bitrate);
            if info.length.is_none() {
                let ms = audio_bytes.saturating_mul(8) / u64::from(f.bitrate);
                info.length = Some(Duration::from_millis(ms));
            }
        }
    }
}

/// FLAC metadata blocks from `pos` (just after `fLaC`).
fn flac<R: Read + Seek>(r: &mut R, len: u64, mut pos: u64, info: &mut AudioInfo) -> io::Result<()> {
    info.format = Some("FLAC");
    for _ in 0..MAX_BLOCKS {
        let h = read_at(r, len, pos, 4)?;
        if h.len() < 4 {
            break;
        }
        let last = h[0] & 0x80 != 0;
        let kind = h[0] & 0x7f;
        let size = be_u24(&h[1..4]) as u64;
        let body_at = pos + 4;
        match kind {
            0 => {
                let s = read_at(r, len, body_at, 34)?;
                if s.len() == 34 {
                    let rate = (u32::from(s[10]) << 12) | (u32::from(s[11]) << 4) | u32::from(s[12] >> 4);
                    let channels = u16::from((s[12] >> 1) & 7) + 1;
                    let bits = u16::from(((s[12] & 1) << 4) | (s[13] >> 4)) + 1;
                    let total = (u64::from(s[13] & 0x0f) << 32)
                        | u64::from(u32::from_be_bytes([s[14], s[15], s[16], s[17]]));
                    if rate > 0 {
                        info.sample_rate = Some(rate);
                        info.channels = Some(channels);
                        info.bits_per_sample = Some(bits);
                        if total > 0 {
                            let secs = total as f64 / f64::from(rate);
                            info.length = Some(Duration::from_secs_f64(secs));
                            info.bitrate =
                                Some((len.saturating_sub(pos) as f64 * 8.0 / secs / 1000.0).round()
                                    as u32);
                        }
                    }
                }
            }
            4 => {
                let block = read_at(r, len, body_at, (size as usize).min(MAX_TAG_BYTES))?;
                vorbis_comments(&block, info);
            }
            _ => {}
        }
        if last {
            break;
        }
        pos = body_at + size;
    }
    Ok(())
}

/// A Vorbis comment block: vendor string, then `KEY=value` entries (little-endian lengths).
pub fn vorbis_comments(b: &[u8], info: &mut AudioInfo) {
    let Some(vendor) = le_u32(b, 0) else {
        return;
    };
    let mut pos = 4usize.saturating_add(vendor as usize);
    let Some(count) = le_u32(b, pos) else {
        return;
    };
    pos += 4;
    let mut artists: Vec<String> = Vec::new();
    for _ in 0..count.min(10_000) {
        let Some(n) = le_u32(b, pos) else {
            return;
        };
        pos += 4;
        let Some(entry) = pos.checked_add(n as usize).and_then(|e| b.get(pos..e)) else {
            return;
        };
        pos += n as usize;
        let entry = String::from_utf8_lossy(entry);
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        let field = match key.to_ascii_uppercase().as_str() {
            "TITLE" => Field::Title,
            "ARTIST" => {
                if artists.len() < 64 {
                    artists.push(value.trim().to_owned());
                }
                continue;
            }
            "ALBUM" => Field::Album,
            "ALBUMARTIST" | "ALBUM ARTIST" | "ALBUM_ARTIST" => Field::AlbumArtist,
            "GENRE" => Field::Genre,
            "COMMENT" | "DESCRIPTION" => Field::Comment,
            "COMPOSER" => Field::Composer,
            "CONDUCTOR" => Field::Conductor,
            "PUBLISHER" | "ORGANIZATION" | "LABEL" => Field::Publisher,
            "ENCODED-BY" | "ENCODEDBY" | "ENCODED_BY" => Field::EncodedBy,
            "SUBTITLE" => Field::Subtitle,
            "LANGUAGE" => Field::Language,
            "MOOD" => Field::Mood,
            "INITIALKEY" | "KEY" => Field::InitialKey,
            "LYRICIST" | "WRITER" => Field::Writer,
            "DISCNUMBER" => Field::PartOfSet,
            "TRACKNUMBER" => Field::Track,
            "DATE" | "YEAR" | "ORIGINALDATE" => Field::Year,
            "BPM" => Field::Bpm,
            "COMPILATION" => Field::Compilation,
            _ => continue,
        };
        info.set(field, value);
    }
    artists.retain(|a| !a.is_empty());
    if !artists.is_empty() {
        info.set(Field::Artist, &artists.join("; "));
    }
}

/// WAV: `fmt ` (codec, rate, channels, bits), `data` (length), `LIST/INFO` and `id3 ` tags.
fn wav<R: Read + Seek>(r: &mut R, len: u64, start: u64, info: &mut AudioInfo) -> io::Result<()> {
    let mut pos = start + 12;
    let mut byte_rate = 0u32;
    let mut data_len = None;
    for _ in 0..1024 {
        let h = read_at(r, len, pos, 8)?;
        if h.len() < 8 {
            break;
        }
        let size = u64::from(le_u32(&h, 4).unwrap_or(0));
        let body_at = pos + 8;
        match &h[..4] {
            b"fmt " => {
                let f = read_at(r, len, body_at, 16)?;
                if f.len() == 16 {
                    let tag = le_u16(&f, 0).unwrap_or(0);
                    info.format = Some(match tag {
                        1 => "PCM",
                        3 => "IEEE float",
                        0x55 => "MP3",
                        _ => "WAV",
                    });
                    info.channels = le_u16(&f, 2).filter(|&c| c > 0);
                    info.sample_rate = le_u32(&f, 4).filter(|&s| s > 0);
                    byte_rate = le_u32(&f, 8).unwrap_or(0);
                    info.bits_per_sample = le_u16(&f, 14).filter(|&b| b > 0);
                }
            }
            b"data" => data_len = Some(size.min(len.saturating_sub(body_at))),
            b"LIST" => {
                let l = read_at(r, len, body_at, (size as usize).min(MAX_TAG_BYTES))?;
                if l.starts_with(b"INFO") {
                    riff_info(&l[4..], info);
                }
            }
            b"id3 " | b"ID3 " => {
                let t = read_at(r, len, body_at, (size as usize).min(MAX_TAG_BYTES))?;
                parse_id3v2(&t, info);
            }
            _ => {}
        }
        pos = body_at + size + (size & 1);
    }
    if byte_rate > 0 {
        info.bitrate = Some(byte_rate.saturating_mul(8) / 1000);
        if let Some(n) = data_len {
            info.length = Some(Duration::from_secs_f64(n as f64 / f64::from(byte_rate)));
        }
    }
    Ok(())
}

fn riff_info(mut b: &[u8], info: &mut AudioInfo) {
    for _ in 0..256 {
        if b.len() < 8 {
            return;
        }
        let id: [u8; 4] = [b[0], b[1], b[2], b[3]];
        let size = le_u32(b, 4).unwrap_or(0) as usize;
        let Some(raw) = size.checked_add(8).and_then(|e| b.get(8..e)) else {
            return;
        };
        let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        let value = String::from_utf8(raw[..end].to_vec())
            .unwrap_or_else(|e| crate::lnk::decode_ansi(e.as_bytes()));
        let field = match &id {
            b"INAM" => Some(Field::Title),
            b"IART" => Some(Field::Artist),
            b"IPRD" => Some(Field::Album),
            b"ICMT" => Some(Field::Comment),
            b"IGNR" => Some(Field::Genre),
            b"ICRD" => Some(Field::Year),
            b"ITRK" | b"IPRT" => Some(Field::Track),
            b"ICMS" => Some(Field::Composer),
            b"ILNG" => Some(Field::Language),
            b"ISFT" => Some(Field::EncodedBy),
            _ => None,
        };
        if let Some(f) = field {
            info.set(f, &value);
        }
        let next = 8 + size + (size & 1);
        b = b.get(next..).unwrap_or(&[]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use std::io::Cursor;

    fn run(b: &[u8]) -> AudioInfo {
        read(&mut Cursor::new(b), b.len() as u64).unwrap()
    }

    #[test]
    fn id3v2_versions_and_cbr_mp3() {
        for major in [2u8, 3, 4] {
            let mp3 = fixtures::mp3(major, false, 40);
            let a = run(&mp3);
            assert_eq!(a.format, Some("MP3"), "v2.{major}");
            assert_eq!(a.title.as_deref(), Some("Test Title"), "v2.{major}");
            assert_eq!(a.artist.as_deref(), Some("Test Artist"));
            assert_eq!(a.album.as_deref(), Some("Test Album"));
            assert_eq!(a.album_artist.as_deref(), Some("Various"));
            assert_eq!(a.genre.as_deref(), Some("Rock"), "(17) is Rock");
            assert_eq!(a.track, Some(3));
            assert_eq!(a.year, Some(2004));
            assert_eq!(a.comment.as_deref(), Some("A comment"));
            assert_eq!(a.sample_rate, Some(44_100));
            assert_eq!(a.channels, Some(2));
            assert_eq!(a.bitrate, Some(128));
            // 40 frames of 417/418 bytes at 128 kbit/s: about 1.04 s.
            let ms = a.length.unwrap().as_millis();
            assert!((1000..1100).contains(&ms), "{ms}");
        }
    }

    #[test]
    fn xing_vbr_length() {
        let mp3 = fixtures::mp3(3, true, 40);
        let a = run(&mp3);
        // The Xing header says 1,000 frames of 1,152 samples at 44.1 kHz.
        let ms = a.length.unwrap().as_millis();
        assert_eq!(ms, 26_122);
    }

    #[test]
    fn id3v1_only() {
        let mut b = vec![0u8; 64];
        b.extend_from_slice(&fixtures::id3v1("Old Title", "Old Artist", "Old Album", "1999", 7, 13));
        let a = run(&b);
        assert_eq!(a.title.as_deref(), Some("Old Title"));
        assert_eq!(a.artist.as_deref(), Some("Old Artist"));
        assert_eq!(a.year, Some(1999));
        assert_eq!(a.track, Some(7));
        assert_eq!(a.genre.as_deref(), Some("Pop"));
    }

    #[test]
    fn id3v2_wins_over_id3v1() {
        let mut b = fixtures::mp3(3, false, 4);
        b.extend_from_slice(&fixtures::id3v1("Old Title", "", "", "", 0, 255));
        let a = run(&b);
        assert_eq!(a.title.as_deref(), Some("Test Title"));
    }

    #[test]
    fn flac_streaminfo_and_comments() {
        let f = fixtures::flac();
        let a = run(&f);
        assert_eq!(a.format, Some("FLAC"));
        assert_eq!(a.sample_rate, Some(48_000));
        assert_eq!(a.channels, Some(2));
        assert_eq!(a.bits_per_sample, Some(24));
        assert_eq!(a.length, Some(Duration::from_secs(10)));
        assert_eq!(a.title.as_deref(), Some("Flac Title"));
        assert_eq!(a.artist.as_deref(), Some("One; Two"));
        assert_eq!(a.track, Some(5));
        assert_eq!(a.year, Some(2020));
        assert_eq!(a.genre.as_deref(), Some("Ambient"));
    }

    #[test]
    fn wav_fmt_data_and_info() {
        let w = fixtures::wav();
        let a = run(&w);
        assert_eq!(a.format, Some("PCM"));
        assert_eq!(a.sample_rate, Some(8000));
        assert_eq!(a.channels, Some(1));
        assert_eq!(a.bits_per_sample, Some(16));
        assert_eq!(a.bitrate, Some(128));
        assert_eq!(a.length, Some(Duration::from_millis(250)));
        assert_eq!(a.title.as_deref(), Some("Wav Title"));
        assert_eq!(a.artist.as_deref(), Some("Wav Artist"));
    }

    #[test]
    fn genres() {
        assert_eq!(genre_name("(17)"), "Rock");
        assert_eq!(genre_name("17"), "Rock");
        assert_eq!(genre_name("(17)Indie"), "Indie");
        assert_eq!(genre_name("(RX)"), "Remix");
        assert_eq!(genre_name("Shoegaze"), "Shoegaze");
        assert_eq!(genre_name("(999)"), "(999)");
    }

    #[test]
    fn text_encodings() {
        assert_eq!(text_values(b"\x00caf\xe9").as_deref(), Some("caf\u{e9}"));
        assert_eq!(text_values(b"\x03caf\xc3\xa9").as_deref(), Some("caf\u{e9}"));
        assert_eq!(
            text_values(b"\x01\xff\xfeA\0\0\0\xfe\xff\0B").as_deref(),
            Some("A; B")
        );
        assert_eq!(text_values(b"\x02\0A").as_deref(), Some("A"));
        assert_eq!(text_values(b"").as_deref(), None);
    }

    #[test]
    fn truncated_files_never_panic() {
        for f in [fixtures::mp3(4, true, 3), fixtures::flac(), fixtures::wav()] {
            for n in 0..f.len().min(2048) {
                let _ = run(&f[..n]);
            }
        }
    }
}
