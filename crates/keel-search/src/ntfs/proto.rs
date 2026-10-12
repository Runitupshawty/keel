//! The Keel index service's wire protocol (`docs/index-service.md`): what the
//! unelevated Keel asks the `KeelIndex` service over its named pipe, and what comes
//! back. Built on every platform so the parser can be fuzzed (`fuzz::index_proto`).
//!
//! Every message is a frame: a little-endian `u32` byte length, then that many bytes
//! of payload. A payload starts with the protocol [`VERSION`] and a kind byte; the
//! fields follow in a fixed order, little-endian, and must use up the payload exactly
//! (trailing bytes are an error). Both sides decode strictly: an unknown version or
//! kind, a count larger than the bytes left, a drive letter outside `A`..`Z`, a name
//! that is not a single path component: each is an error, never a panic, and nothing
//! is allocated before the bytes for it are known to be there.

use std::io::{self, Read, Write};

/// Protocol version; a service and a Keel with different versions refuse each other.
pub(crate) const VERSION: u8 = 1;
/// Largest request frame (a [`Request::Stat`] of [`MAX_STAT`] ids is about 8 KiB).
pub(crate) const MAX_REQUEST: usize = 16 * 1024;
/// Largest response frame.
pub(crate) const MAX_RESPONSE: usize = 8 << 20;
/// Most file ids in one [`Request::Stat`].
pub(crate) const MAX_STAT: usize = 1024;
/// Largest raw journal buffer in one [`Response::JournalData`].
pub(crate) const MAX_JOURNAL: usize = 1 << 20;
/// Longest file name in UTF-16 units (NTFS's limit).
pub(crate) const MAX_NAME_UNITS: usize = 255;
/// Longest error message in bytes.
pub(crate) const MAX_MESSAGE: usize = 512;
/// Longest version string in a [`Response::Hello`].
const MAX_VERSION: usize = 64;

/// Error codes of [`Response::Error`] that are not Windows error codes.
pub(crate) const E_BAD_REQUEST: u32 = 0xE000_0001;
/// The drive letter is not a fixed local NTFS volume.
pub(crate) const E_BAD_VOLUME: u32 = 0xE000_0002;
/// Anything else that failed in the service (the message says what).
pub(crate) const E_FAILED: u32 = 0xE000_0003;

/// What the unelevated Keel asks for. Every request names at most one volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Request {
    /// The service's process id and Keel version.
    Hello,
    /// The fixed local NTFS volumes the service will read.
    Volumes,
    /// `FSCTL_QUERY_USN_JOURNAL`.
    QueryJournal { letter: char },
    /// One `FSCTL_READ_USN_JOURNAL` call from `start` (raw records, names included).
    ReadJournal {
        letter: char,
        journal_id: u64,
        start: i64,
    },
    /// Size, dates and attributes of files by id (`OpenFileById`).
    Stat { letter: char, frns: Vec<u64> },
    /// The volume's MFT as the elevated side reads it: not a [`Response`] but a run of
    /// `wire` frames (raw records, never parsed there) ending with `End` or `Failed`,
    /// or one [`Response::Error`].
    ReadMft { letter: char },
}

/// Size, dates and attributes of a file looked up by its id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stat {
    pub size: u64,
    pub allocated: u64,
    pub created: i64,
    pub modified: i64,
    pub accessed: i64,
    pub attributes: u32,
    pub links: u16,
}
/// What the service answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Response {
    /// `code`: a Windows error code, or one of the `E_*` codes.
    Error {
        code: u32,
        message: String,
    },
    Hello {
        pid: u32,
        version: String,
    },
    Volumes(Vec<(char, u32)>),
    Journal {
        id: u64,
        next_usn: i64,
        lowest_valid_usn: i64,
    },
    /// The raw output of one `FSCTL_READ_USN_JOURNAL` (the next USN, then records):
    /// parsed by the client, never by the service.
    JournalData(Vec<u8>),
    /// One per requested id, in order; `None` when the file is gone.
    Stats(Vec<Option<Stat>>),
}

/// A message that does not decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Malformed(pub &'static str);

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "malformed index service message: {}", self.0)
    }
}

impl std::error::Error for Malformed {}

impl From<Malformed> for io::Error {
    fn from(e: Malformed) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, e)
    }
}

/// A file name as the index takes it: one path component of 1 to 255 UTF-16 units,
/// with no separator, NUL or other character below U+0020 (which Windows refuses in
/// names), and not `.` or `..`.
pub(crate) fn valid_name(name: &str) -> bool {
    let units = name.encode_utf16().count();
    (1..=MAX_NAME_UNITS).contains(&units)
        && name != "."
        && name != ".."
        && !name.chars().any(|c| c == '\\' || c == '/' || c < ' ')
}

/// A drive letter on the wire: `A` to `Z`.
pub(crate) fn valid_letter(letter: char) -> bool {
    letter.is_ascii_uppercase()
}

// --- frames ---

/// Writes one frame (`payload` must fit the peer's limit).
pub(crate) fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::other("frame too large"))?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(payload);
    w.write_all(&frame)?;
    w.flush()
}

/// Reads one frame of at most `max` bytes. `Ok(None)` when the stream ends cleanly
/// before a frame starts; a frame cut short or over `max` is an error.
pub(crate) fn read_frame(r: &mut impl Read, max: usize) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < len.len() {
        match r.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // A closed pipe at a frame boundary is a clean end.
            Err(e) if got == 0 && e.raw_os_error() == Some(109) => return Ok(None),
            Err(e) => return Err(e),
        }
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > max {
        return Err(Malformed("frame over the size limit").into());
    }
    // Grown as bytes arrive: a length alone does not allocate the frame.
    let mut payload = Vec::with_capacity(len.min(64 * 1024));
    r.by_ref().take(len as u64).read_to_end(&mut payload)?;
    if payload.len() != len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(Some(payload))
}

// --- encoding ---

struct Out(Vec<u8>);

impl Out {
    fn new(kind: u8) -> Self {
        Self(vec![VERSION, kind])
    }
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn i64(&mut self, v: i64) -> &mut Self {
        self.u64(v as u64)
    }
    fn letter(&mut self, c: char) -> &mut Self {
        // Only A-Z decodes; anything else is sent as 0 and refused by the peer.
        self.u8(if valid_letter(c) { c as u8 } else { 0 })
    }
    /// A string cut to `max` bytes (on a character boundary), u16 length first.
    fn str(&mut self, s: &str, max: usize) -> &mut Self {
        let mut end = s.len().min(max);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.u16(end as u16);
        self.0.extend_from_slice(&s.as_bytes()[..end]);
        self
    }
}

const REQ_HELLO: u8 = 1;
const REQ_VOLUMES: u8 = 2;
const REQ_QUERY_JOURNAL: u8 = 3;
const REQ_READ_JOURNAL: u8 = 4;
const REQ_STAT: u8 = 5;
const REQ_READ_MFT: u8 = 6;

const RES_ERROR: u8 = 0x80;
const RES_HELLO: u8 = 0x81;
const RES_VOLUMES: u8 = 0x82;
const RES_JOURNAL: u8 = 0x83;
const RES_JOURNAL_DATA: u8 = 0x84;
const RES_STATS: u8 = 0x85;
const STAT_SIZE: usize = 8 * 5 + 4 + 2;

impl Request {
    pub(crate) fn encode(&self) -> Vec<u8> {
        match self {
            Request::Hello => Out::new(REQ_HELLO).0,
            Request::Volumes => Out::new(REQ_VOLUMES).0,
            Request::QueryJournal { letter } => {
                Out::new(REQ_QUERY_JOURNAL).letter(*letter).0.clone()
            }
            Request::ReadJournal {
                letter,
                journal_id,
                start,
            } => {
                let mut o = Out::new(REQ_READ_JOURNAL);
                o.letter(*letter).u64(*journal_id).i64(*start);
                o.0
            }
            Request::Stat { letter, frns } => {
                let mut o = Out::new(REQ_STAT);
                o.letter(*letter)
                    .u16(frns.len().min(u16::MAX as usize) as u16);
                for &frn in frns {
                    o.u64(frn);
                }
                o.0
            }
            Request::ReadMft { letter } => Out::new(REQ_READ_MFT).letter(*letter).0.clone(),
        }
    }

    /// Strict decode: see the module docs.
    pub(crate) fn decode(buf: &[u8]) -> Result<Self, Malformed> {
        let mut c = In::new(buf)?;
        let request = match c.kind {
            REQ_HELLO => Request::Hello,
            REQ_VOLUMES => Request::Volumes,
            REQ_QUERY_JOURNAL => Request::QueryJournal {
                letter: c.letter()?,
            },
            REQ_READ_JOURNAL => Request::ReadJournal {
                letter: c.letter()?,
                journal_id: c.u64()?,
                start: c.i64()?,
            },
            REQ_STAT => {
                let letter = c.letter()?;
                let n = usize::from(c.u16()?);
                if n == 0 || n > MAX_STAT {
                    return Err(Malformed("stat count out of range"));
                }
                c.need(n, 8)?;
                let frns = (0..n).map(|_| c.u64()).collect::<Result<_, _>>()?;
                Request::Stat { letter, frns }
            }
            REQ_READ_MFT => Request::ReadMft {
                letter: c.letter()?,
            },
            _ => return Err(Malformed("unknown request")),
        };
        c.end()?;
        Ok(request)
    }

    /// The volume a request names.
    pub(crate) fn letter(&self) -> Option<char> {
        match self {
            Request::Hello | Request::Volumes => None,
            Request::QueryJournal { letter }
            | Request::ReadJournal { letter, .. }
            | Request::Stat { letter, .. }
            | Request::ReadMft { letter } => Some(*letter),
        }
    }
}

impl Response {
    pub(crate) fn error(code: u32, message: impl Into<String>) -> Self {
        Response::Error {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        match self {
            Response::Error { code, message } => {
                let mut o = Out::new(RES_ERROR);
                o.u32(*code).str(message, MAX_MESSAGE);
                o.0
            }
            Response::Hello { pid, version } => {
                let mut o = Out::new(RES_HELLO);
                o.u32(*pid).str(version, MAX_VERSION);
                o.0
            }
            Response::Volumes(volumes) => {
                let mut o = Out::new(RES_VOLUMES);
                o.u8(volumes.len().min(26) as u8);
                for &(letter, serial) in volumes.iter().take(26) {
                    o.letter(letter).u32(serial);
                }
                o.0
            }
            Response::Journal {
                id,
                next_usn,
                lowest_valid_usn,
            } => {
                let mut o = Out::new(RES_JOURNAL);
                o.u64(*id).i64(*next_usn).i64(*lowest_valid_usn);
                o.0
            }
            Response::JournalData(data) => {
                let mut o = Out::new(RES_JOURNAL_DATA);
                let data = &data[..data.len().min(MAX_JOURNAL)];
                o.u32(data.len() as u32);
                o.0.extend_from_slice(data);
                o.0
            }
            Response::Stats(stats) => {
                let mut o = Out::new(RES_STATS);
                let stats = &stats[..stats.len().min(MAX_STAT)];
                o.u16(stats.len() as u16);
                for stat in stats {
                    match stat {
                        None => {
                            o.u8(0);
                        }
                        Some(s) => {
                            o.u8(1)
                                .u64(s.size)
                                .u64(s.allocated)
                                .i64(s.created)
                                .i64(s.modified)
                                .i64(s.accessed)
                                .u32(s.attributes)
                                .u16(s.links);
                        }
                    }
                }
                o.0
            }
        }
    }

    /// Strict decode: see the module docs.
    pub(crate) fn decode(buf: &[u8]) -> Result<Self, Malformed> {
        let mut c = In::new(buf)?;
        let response = match c.kind {
            RES_ERROR => Response::Error {
                code: c.u32()?,
                message: c.str(MAX_MESSAGE)?,
            },
            RES_HELLO => Response::Hello {
                pid: c.u32()?,
                version: c.str(MAX_VERSION)?,
            },
            RES_VOLUMES => {
                let n = usize::from(c.u8()?);
                if n > 26 {
                    return Err(Malformed("more than 26 volumes"));
                }
                c.need(n, 5)?;
                let mut volumes: Vec<(char, u32)> = Vec::with_capacity(n);
                for _ in 0..n {
                    let letter = c.letter()?;
                    if volumes.iter().any(|&(l, _)| l == letter) {
                        return Err(Malformed("a volume listed twice"));
                    }
                    volumes.push((letter, c.u32()?));
                }
                Response::Volumes(volumes)
            }
            RES_JOURNAL => Response::Journal {
                id: c.u64()?,
                next_usn: c.i64()?,
                lowest_valid_usn: c.i64()?,
            },
            RES_JOURNAL_DATA => {
                let n = c.u32()? as usize;
                if !(8..=MAX_JOURNAL).contains(&n) {
                    return Err(Malformed("journal buffer size out of range"));
                }
                Response::JournalData(c.bytes(n)?.to_vec())
            }
            RES_STATS => {
                let n = usize::from(c.u16()?);
                if n > MAX_STAT {
                    return Err(Malformed("too many stats"));
                }
                c.need(n, 1)?;
                let mut stats = Vec::with_capacity(n);
                for _ in 0..n {
                    stats.push(match c.u8()? {
                        0 => None,
                        1 => Some(Stat {
                            size: c.u64()?,
                            allocated: c.u64()?,
                            created: c.i64()?,
                            modified: c.i64()?,
                            accessed: c.i64()?,
                            attributes: c.u32()?,
                            links: c.u16()?,
                        }),
                        _ => return Err(Malformed("bad stat flag")),
                    });
                }
                Response::Stats(stats)
            },
            _ => return Err(Malformed("unknown response")),
        };
        c.end()?;
        Ok(response)
    }
}

// --- decoding ---

struct In<'a> {
    buf: &'a [u8],
    at: usize,
    kind: u8,
}

impl<'a> In<'a> {
    fn new(buf: &'a [u8]) -> Result<Self, Malformed> {
        match buf {
            [VERSION, kind, ..] => Ok(Self {
                buf,
                at: 2,
                kind: *kind,
            }),
            [_, _, ..] => Err(Malformed("other protocol version")),
            _ => Err(Malformed("too short")),
        }
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&end| end <= self.buf.len())
            .ok_or(Malformed("cut short"))?;
        let out = &self.buf[self.at..end];
        self.at = end;
        Ok(out)
    }

    /// At least `n` items of `size` bytes are left (checked before allocating).
    fn need(&self, n: usize, size: usize) -> Result<(), Malformed> {
        match n.checked_mul(size) {
            Some(total) if total <= self.buf.len() - self.at => Ok(()),
            _ => Err(Malformed("count larger than the message")),
        }
    }

    fn u8(&mut self) -> Result<u8, Malformed> {
        Ok(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, Malformed> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, Malformed> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, Malformed> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> Result<i64, Malformed> {
        Ok(self.u64()? as i64)
    }
    fn letter(&mut self) -> Result<char, Malformed> {
        let c = char::from(self.u8()?);
        if valid_letter(c) {
            Ok(c)
        } else {
            Err(Malformed("bad drive letter"))
        }
    }
    fn str(&mut self, max: usize) -> Result<String, Malformed> {
        let n = usize::from(self.u16()?);
        if n > max {
            return Err(Malformed("string too long"));
        }
        let bytes = self.bytes(n)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| Malformed("string not UTF-8"))
    }
    fn end(&self) -> Result<(), Malformed> {
        if self.at == self.buf.len() {
            Ok(())
        } else {
            Err(Malformed("trailing bytes"))
        }
    }
}

// The fixed sizes above match what `encode` writes.
const _: () = assert!(STAT_SIZE == 46);

#[cfg(test)]
pub(crate) mod tests {
    use super::*;


    pub(crate) fn all_requests() -> Vec<Request> {
        vec![
            Request::Hello,
            Request::Volumes,
            Request::QueryJournal { letter: 'C' },
            Request::ReadJournal {
                letter: 'D',
                journal_id: 0x01d9_0000_1234,
                start: 4096,
            },
            Request::Stat {
                letter: 'C',
                frns: vec![5, 0x0001_0000_0000_0040, u64::MAX],
            },
            Request::ReadMft { letter: 'Z' },
        ]
    }

    pub(crate) fn all_responses() -> Vec<Response> {
        vec![
            Response::error(5, "Access is denied."),
            Response::Hello {
                pid: 4242,
                version: "0.23.0".into(),
            },
            Response::Volumes(vec![('C', 0x1234_5678), ('D', 7)]),
            Response::Journal {
                id: 99,
                next_usn: 1 << 40,
                lowest_valid_usn: 0,
            },
            Response::JournalData(vec![0u8; 8]),
            Response::Stats(vec![
                None,
                Some(Stat {
                    size: 10,
                    allocated: 4096,
                    created: 1,
                    modified: 2,
                    accessed: 3,
                    attributes: 0x20,
                    links: 2,
                }),
            ]),
        ]
    }

    #[test]
    fn every_message_round_trips() {
        for r in all_requests() {
            assert_eq!(Request::decode(&r.encode()), Ok(r.clone()), "{r:?}");
        }
        for r in all_responses() {
            assert_eq!(Response::decode(&r.encode()), Ok(r.clone()), "{r:?}");
        }
    }

    #[test]
    fn requests_are_decoded_strictly() {
        let stat = Request::Stat {
            letter: 'C',
            frns: vec![1, 2],
        }
        .encode();
        // Wrong version, unknown kind, trailing and missing bytes.
        let mut other = stat.clone();
        other[0] = VERSION + 1;
        assert!(Request::decode(&other).is_err());
        assert!(Request::decode(&[VERSION, 0x7f]).is_err());
        let mut long = stat.clone();
        long.push(0);
        assert!(Request::decode(&long).is_err());
        assert!(Request::decode(&stat[..stat.len() - 1]).is_err());
        assert!(Request::decode(&[]).is_err());
        assert!(Request::decode(&[VERSION]).is_err());
        // Drive letters: A-Z only (lowercase, digits, NUL, '\\' refused).
        for bad in [b'c', b'0', 0, b'\\', b':', 0xC3] {
            assert!(
                Request::decode(&[VERSION, REQ_QUERY_JOURNAL, bad]).is_err(),
                "{bad}"
            );
        }
        // A count that claims more ids than sent, none, or more than MAX_STAT.
        let mut liar = vec![VERSION, REQ_STAT, b'C'];
        liar.extend_from_slice(&u16::MAX.to_le_bytes());
        liar.extend_from_slice(&1u64.to_le_bytes());
        assert!(Request::decode(&liar).is_err());
        assert!(Request::decode(&[VERSION, REQ_STAT, b'C', 0, 0]).is_err());
        let over = Request::Stat {
            letter: 'C',
            frns: vec![1; MAX_STAT + 1],
        };
        assert!(Request::decode(&over.encode()).is_err());
        // An invalid letter given to `encode` goes out as 0 and is refused.
        let lower = Request::QueryJournal { letter: 'c' }.encode();
        assert!(Request::decode(&lower).is_err());
    }

    #[test]
    fn responses_are_decoded_strictly() {
        // A string that is not UTF-8.
        let mut bytes = Response::error(5, "ab").encode();
        let n = bytes.len();
        bytes[n - 1] = 0xFF;
        assert!(Response::decode(&bytes).is_err());
        // A string longer than its limit.
        let mut long = Response::error(5, "").encode();
        let n = long.len();
        long[n - 2..].copy_from_slice(&((MAX_MESSAGE + 1) as u16).to_le_bytes());
        long.extend(vec![b'x'; MAX_MESSAGE + 1]);
        assert!(Response::decode(&long).is_err());
        // Volumes listed twice, or a bad letter.
        let twice = Response::Volumes(vec![('C', 1), ('C', 2)]).encode();
        assert!(Response::decode(&twice).is_err());
        assert!(Response::decode(&[VERSION, RES_VOLUMES, 1, b'c', 0, 0, 0, 0]).is_err());
        // Journal data shorter than its leading USN, or over the limit.
        assert!(Response::decode(&Response::JournalData(vec![0; 7]).encode()).is_err());
        let mut huge = vec![VERSION, RES_JOURNAL_DATA];
        huge.extend_from_slice(&((MAX_JOURNAL + 1) as u32).to_le_bytes());
        assert!(Response::decode(&huge).is_err());
        assert!(Response::decode(&[VERSION, RES_STATS, 1, 0, 7]).is_err());
    }

    #[test]
    fn names_are_one_path_component() {
        for bad in [
            "",
            ".",
            "..",
            r"ab",
            "a/b",
            "nul\0",
            "tab\t",
            &"x".repeat(256),
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert!(valid_name(&"x".repeat(255)));
        assert!(valid_name("naïve é.txt"));
        // Counted in UTF-16 units: 255 characters of 3 UTF-8 bytes fit.
        assert!(valid_name(&"€".repeat(255)));
        assert!(!valid_name(&"😀".repeat(128)), "256 UTF-16 units");
    }

    #[test]
    fn frames_are_bounded() {
        let mut wire = Vec::new();
        write_frame(&mut wire, b"hello").unwrap();
        write_frame(&mut wire, b"").unwrap();
        let mut r = &wire[..];
        assert_eq!(read_frame(&mut r, 16).unwrap().unwrap(), b"hello");
        assert_eq!(read_frame(&mut r, 16).unwrap().unwrap(), b"");
        assert!(read_frame(&mut r, 16).unwrap().is_none(), "clean end");
        // Over the limit: refused before the payload is allocated.
        let mut r = &u32::MAX.to_le_bytes()[..];
        assert!(read_frame(&mut r, MAX_RESPONSE).is_err());
        // Cut inside the length or the payload.
        let mut r = &[5u8, 0][..];
        assert!(read_frame(&mut r, 16).is_err());
        let mut r = &[5u8, 0, 0, 0, b'a'][..];
        assert!(read_frame(&mut r, 16).is_err());
    }

    #[test]
    fn largest_messages_fit_their_frames() {
        let stat = Request::Stat {
            letter: 'C',
            frns: vec![u64::MAX; MAX_STAT],
        };
        assert!(stat.encode().len() <= MAX_REQUEST);
        let journal = Response::JournalData(vec![0; MAX_JOURNAL]);
        assert!(journal.encode().len() <= MAX_RESPONSE);
    }
}
