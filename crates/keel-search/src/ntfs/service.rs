//! The index service's request handling (both ends of [`crate::proto`]), apart from
//! the Win32 plumbing in `svcwin`: [`serve`] answers one connection for a backend
//! that does the elevated reads ([`Elevated`]; [`LocalVolumes`] in the service), and
//! [`Client`] is what the unelevated Keel uses, re-validating every answer.
//!
//! The service only reads, and parses nothing a disk holds: it hands back the
//! journal's raw buffers (parsed by the client), sizes and dates by file id, and the
//! MFT as raw records in the one-shot helper's frames ([`crate::wire`]) with the
//! update sequence applied and file contents kept inside records blanked. Keel
//! decodes, parses and indexes those (`receive`), so a hostile volume cannot reach
//! parsing code running as SYSTEM. It never returns file contents, never writes, and
//! takes requests only for drive letters that are fixed local NTFS volumes in the
//! service's own (global) drive namespace. See `docs/index-service.md`.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::proto::{
    read_frame, valid_letter, valid_name, write_frame, Request, Response, Stat, E_BAD_REQUEST,
    E_BAD_VOLUME, E_FAILED, MAX_JOURNAL, MAX_REQUEST, MAX_RESPONSE, MAX_STAT,
};
use crate::wire;

use super::db::Meta;
use super::index::Index;
use super::receive::{Receiver, Start};
use super::usn::{parse_buffer, Record};
use super::win::{self, Journal, Volume, VolumeId};

/// The elevated reads the service performs. [`LocalVolumes`] in the service; a mock
/// in tests.
pub(crate) trait Elevated: Send + Sync {
    /// Fixed local NTFS volumes: (letter, serial).
    fn volumes(&self) -> Vec<(char, u32)>;
    fn query_journal(&self, letter: char) -> io::Result<Journal>;
    /// One raw `FSCTL_READ_USN_JOURNAL` buffer from `start`.
    fn read_journal(&self, letter: char, journal_id: u64, start: i64) -> io::Result<Vec<u8>>;
    fn stat(&self, letter: char, frns: &[u64]) -> Vec<Option<Stat>>;
    /// The volume's MFT as `wire` frames, one `out` call per whole frame (length
    /// prefix included): [`wire::send_volume`]. Errors when `out` fails, or, before
    /// any frame was sent, when the volume cannot be read at all.
    fn send_mft(
        &self,
        letter: char,
        out: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()>;
}

/// Answers requests on one connection until the client closes it. A request that
/// does not decode gets an error and ends the connection; one that names a volume
/// the service does not read gets an error.
pub(crate) fn serve(conn: &mut (impl Read + Write), backend: &dyn Elevated) -> io::Result<()> {
    while let Some(frame) = read_frame(conn, MAX_REQUEST)? {
        let request = match Request::decode(&frame) {
            Ok(r) => r,
            Err(e) => {
                send(conn, &Response::error(E_BAD_REQUEST, e.0))?;
                return Ok(());
            }
        };
        if let Some(letter) = request.letter() {
            if !backend.volumes().iter().any(|&(l, _)| l == letter) {
                send(
                    conn,
                    &Response::error(
                        E_BAD_VOLUME,
                        format!("{letter}: is not a fixed local NTFS volume"),
                    ),
                )?;
                continue;
            }
        }
        answer(conn, backend, request)?;
    }
    Ok(())
}

fn send(conn: &mut impl Write, response: &Response) -> io::Result<()> {
    write_frame(conn, &response.encode())
}

fn os_error(e: &io::Error) -> Response {
    match e.raw_os_error() {
        Some(code) if code > 0 => Response::error(code as u32, e.to_string()),
        _ => Response::error(E_FAILED, e.to_string()),
    }
}


fn answer(
    conn: &mut (impl Read + Write),
    backend: &dyn Elevated,
    request: Request,
) -> io::Result<()> {
    let response = match request {
        Request::Hello => Response::Hello {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        Request::Volumes => Response::Volumes(backend.volumes()),
        Request::QueryJournal { letter } => match backend.query_journal(letter) {
            Ok(j) => Response::Journal {
                id: j.id,
                next_usn: j.next_usn,
                lowest_valid_usn: j.lowest_valid_usn,
            },
            Err(e) => os_error(&e),
        },
        Request::ReadJournal {
            letter,
            journal_id,
            start,
        } => match backend.read_journal(letter, journal_id, start) {
            Ok(mut data) => {
                data.truncate(MAX_JOURNAL);
                if data.len() < 8 {
                    Response::error(E_FAILED, "short journal read")
                } else {
                    Response::JournalData(data)
                }
            }
            Err(e) => os_error(&e),
        },
        Request::Stat { letter, frns } => {
            let mut stats = backend.stat(letter, &frns);
            stats.resize(frns.len(), None);
            Response::Stats(stats)
        }
        Request::ReadMft { letter } => return stream_mft(conn, backend, letter),
    };
    send(conn, &response)
}

/// Streams the volume's raw MFT records to the client as the elevated helper's frames
/// ([`wire`]): the service locates the MFT, applies each record's update sequence and
/// blanks file contents kept inside records, and parses nothing; Keel does that.
/// A backend that fails before the first frame gets an error answer; a failed write
/// ends the connection.
fn stream_mft(conn: &mut impl Write, backend: &dyn Elevated, letter: char) -> io::Result<()> {
    let mut started = false;
    let sent = backend.send_mft(letter, &mut |frame| {
        started = true;
        conn.write_all(frame)
    });
    match sent {
        Ok(()) => conn.flush(),
        Err(e) if !started => send(conn, &os_error(&e)),
        Err(e) => Err(e),
    }
}

/// The service's backend: the volumes of this machine, read as SYSTEM.
#[derive(Default)]
pub(crate) struct LocalVolumes {
    /// Open volume handles by letter, with the serial the letter had when opened.
    handles: Mutex<HashMap<char, (u32, Arc<Volume>)>>,
}

impl LocalVolumes {
    /// A volume handle (cached), only for a letter that is a fixed NTFS volume now.
    fn volume(&self, letter: char) -> io::Result<Arc<Volume>> {
        self.volume_and_serial(letter).map(|(_, v)| v)
    }

    /// [`Self::volume`] with the volume serial the letter has.
    fn volume_and_serial(&self, letter: char) -> io::Result<(u32, Arc<Volume>)> {
        if let Some((serial, v)) = self.handles.lock().get(&letter) {
            return Ok((*serial, v.clone()));
        }
        let serial = self
            .volumes()
            .into_iter()
            .find_map(|(l, serial)| (l == letter).then_some(serial))
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let volume = Arc::new(Volume::open(letter)?);
        if !volume.privileged() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the service cannot open the volume",
            ));
        }
        self.handles.lock().insert(letter, (serial, volume.clone()));
        Ok((serial, volume))
    }
}

impl Elevated for LocalVolumes {
    fn volumes(&self) -> Vec<(char, u32)> {
        let ids = win::fixed_ntfs_volumes();
        // A letter that now names another volume (or none) loses its cached handle.
        self.handles.lock().retain(|letter, (serial, _)| {
            ids.iter()
                .any(|id| id.letter == *letter && id.serial == *serial)
        });
        ids.into_iter().map(|id| (id.letter, id.serial)).collect()
    }

    fn query_journal(&self, letter: char) -> io::Result<Journal> {
        self.volume(letter)?.query_journal()
    }

    fn read_journal(&self, letter: char, journal_id: u64, start: i64) -> io::Result<Vec<u8>> {
        self.volume(letter)?.read_journal_raw(journal_id, start)
    }

    fn stat(&self, letter: char, frns: &[u64]) -> Vec<Option<Stat>> {
        match self.volume(letter) {
            Ok(v) => frns.iter().map(|&frn| v.stat(frn)).collect(),
            Err(_) => vec![None; frns.len()],
        }
    }

    fn send_mft(
        &self,
        letter: char,
        out: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        let (serial, volume) = self.volume_and_serial(letter)?;
        wire::send_volume(&*volume, letter, serial, out)
    }
}

// --- client ---

/// Rounds of [`Request::ReadJournal`] one [`Client::read_journal`] makes at most
/// (1 MiB each): a service that never reaches the end of the journal cannot keep the
/// client reading forever.
const MAX_JOURNAL_ROUNDS: usize = 4096;

/// The unelevated side of the protocol. Every answer is checked again here: the
/// decoder refuses malformed frames, and the values are checked against the request
/// (counts, kinds, a journal that moves forward, names that are one component). A
/// transport or protocol error marks the client [`broken`](Client::broken); an error
/// the service reports does not.
pub(crate) struct Client<S: Read + Write> {
    conn: S,
    /// The service's process id and Keel version (from the handshake).
    pub pid: u32,
    pub version: String,
    /// Volumes the service reads: (letter, serial).
    volumes: Vec<(char, u32)>,
    broken: bool,
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("index service: {what}"))
}

impl<S: Read + Write> Client<S> {
    /// Hello and the volume list.
    pub fn handshake(conn: S) -> io::Result<Self> {
        let mut client = Self {
            conn,
            pid: 0,
            version: String::new(),
            volumes: Vec::new(),
            broken: false,
        };
        match client.call(&Request::Hello)? {
            Response::Hello { pid, version } => {
                client.pid = pid;
                client.version = version;
            }
            _ => return Err(client.fail("unexpected answer to hello")),
        }
        client.refresh_volumes()?;
        Ok(client)
    }

    /// Asks the service for its volume list again.
    pub fn refresh_volumes(&mut self) -> io::Result<()> {
        match self.call(&Request::Volumes)? {
            Response::Volumes(v) => {
                self.volumes = v;
                Ok(())
            }
            _ => Err(self.fail("unexpected answer to volumes")),
        }
    }

    /// The connection failed or the service answered out of protocol: drop it.
    pub fn broken(&self) -> bool {
        self.broken
    }

    /// Whether the service reads `id`: the same letter and the same volume serial as
    /// this process sees (drive letters are per logon session; a letter the user
    /// mapped elsewhere is not taken).
    pub fn serves(&self, id: VolumeId) -> bool {
        self.volumes
            .iter()
            .any(|&(l, s)| l == id.letter && s == id.serial)
    }

    fn fail(&mut self, what: &str) -> io::Error {
        self.broken = true;
        invalid(what)
    }

    fn transport<T>(&mut self, r: io::Result<T>) -> io::Result<T> {
        if r.is_err() {
            self.broken = true;
        }
        r
    }

    /// The next frame's payload.
    fn next_frame(&mut self) -> io::Result<Vec<u8>> {
        let frame = read_frame(&mut self.conn, MAX_RESPONSE);
        match self.transport(frame)? {
            Some(f) => Ok(f),
            None => Err(self.fail("the service closed the connection")),
        }
    }

    /// Decodes a response; an error response becomes an `io::Error` (a Windows code
    /// keeps its raw OS error, so ERROR_JOURNAL_ENTRY_DELETED still reads as such).
    fn answer(&mut self, frame: &[u8]) -> io::Result<Response> {
        let response = Response::decode(frame).map_err(io::Error::from);
        match self.transport(response)? {
            // A Windows error code keeps its raw value, so ERROR_JOURNAL_ENTRY_DELETED
            // still reads as such.
            Response::Error { code, .. } if code < E_BAD_REQUEST => {
                Err(io::Error::from_raw_os_error(code as i32))
            }
            Response::Error { code, message } => {
                if code == E_BAD_REQUEST {
                    self.broken = true;
                }
                Err(io::Error::other(format!("index service: {message}")))
            }
            r => Ok(r),
        }
    }

    /// Reads one response.
    fn receive(&mut self) -> io::Result<Response> {
        let frame = self.next_frame()?;
        self.answer(&frame)
    }

    fn call(&mut self, request: &Request) -> io::Result<Response> {
        let sent = write_frame(&mut self.conn, &request.encode());
        self.transport(sent)?;
        self.receive()
    }

    fn check_letter(&self, letter: char) -> io::Result<()> {
        if valid_letter(letter) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bad drive letter",
            ))
        }
    }

    pub fn query_journal(&mut self, letter: char) -> io::Result<Journal> {
        self.check_letter(letter)?;
        match self.call(&Request::QueryJournal { letter })? {
            Response::Journal {
                id,
                next_usn,
                lowest_valid_usn,
            } if lowest_valid_usn <= next_usn && lowest_valid_usn >= 0 => Ok(Journal {
                id,
                next_usn,
                lowest_valid_usn,
            }),
            _ => Err(self.fail("bad journal answer")),
        }
    }

    /// Journal records from `start` to the current end, names included, and the USN
    /// to continue from (as [`Volume::read_journal`]).
    pub fn read_journal(
        &mut self,
        letter: char,
        journal_id: u64,
        start: i64,
    ) -> io::Result<(Vec<Record>, i64)> {
        self.check_letter(letter)?;
        let mut all = Vec::new();
        let mut at = start;
        for _ in 0..MAX_JOURNAL_ROUNDS {
            let data = match self.call(&Request::ReadJournal {
                letter,
                journal_id,
                start: at,
            })? {
                Response::JournalData(data) => data,
                _ => return Err(self.fail("bad journal data")),
            };
            let Some((next, mut records)) = parse_buffer(&data) else {
                return Err(self.fail("journal buffer without a next USN"));
            };
            let next = next as i64;
            if next < at {
                return Err(self.fail("the journal went backwards"));
            }
            // Re-validated like the MFT rows: a name must be one path component.
            records.retain(|r| r.frn != 0 && (r.name.is_empty() || valid_name(&r.name)));
            if records.is_empty() {
                return Ok((all, next));
            }
            if next == at {
                return Err(self.fail("the journal does not move"));
            }
            all.extend(records);
            at = next;
        }
        Ok((all, at))
    }

    /// Size and dates of files by id; files that are gone are left out.
    pub fn stat(&mut self, letter: char, frns: &[u64]) -> io::Result<HashMap<u64, Stat>> {
        self.check_letter(letter)?;
        let mut out = HashMap::new();
        for chunk in frns.chunks(MAX_STAT) {
            let stats = match self.call(&Request::Stat {
                letter,
                frns: chunk.to_vec(),
            })? {
                Response::Stats(s) if s.len() == chunk.len() => s,
                _ => return Err(self.fail("bad stat answer")),
            };
            for (&frn, stat) in chunk.iter().zip(stats) {
                if let Some(stat) = stat.filter(plausible) {
                    out.insert(frn, stat);
                }
            }
        }
        Ok(out)
    }

    /// Builds the volume's index from the service's raw MFT records. The service
    /// only locates the MFT and blanks what the index never reads; everything from
    /// there (decoding the frames, parsing each record, merging files, the index) is
    /// done here by the same code and checks as for the one-shot helper
    /// ([`Receiver`]), treating every byte as untrusted. `start` is the volume as
    /// this process sees it (letter, serial, root, journal position taken before the
    /// read); a stream for another volume builds nothing. The service reporting that
    /// it could not read the volume is an error that leaves the client usable; a
    /// stream out of protocol breaks it.
    pub fn read_mft(&mut self, start: Start) -> io::Result<(Index, Meta)> {
        let letter = start.id.letter;
        self.check_letter(letter)?;
        let sent = write_frame(&mut self.conn, &Request::ReadMft { letter }.encode());
        self.transport(sent)?;
        let mut built = None;
        let mut rx = Receiver::new(vec![start], |_, index, meta| built = Some((index, meta)));
        loop {
            let frame = self.next_frame()?;
            // An answer instead of the stream: the service could not start it.
            if frame.get(1).is_some_and(|&kind| kind >= 0x80) {
                self.answer(&frame)?;
                return Err(self.fail("an answer in the MFT stream"));
            }
            let last = match wire::Frame::decode(&frame) {
                Ok(wire::Frame::End { .. } | wire::Frame::Failed(_)) => true,
                // Only the one-shot helper ends its stream with Done.
                Ok(wire::Frame::Done) => return Err(self.fail("an unexpected end of the stream")),
                Ok(_) => false,
                Err(e) => return Err(self.fail(&format!("bad MFT stream: {e:#}"))),
            };
            if let Err(e) = rx.frame(&frame) {
                return Err(self.fail(&format!("bad MFT stream: {e:#}")));
            }
            if last {
                break;
            }
        }
        let failed = std::mem::take(&mut rx.failed);
        drop(rx);
        built.ok_or_else(|| {
            io::Error::other(format!(
                "index service: {}",
                failed.first().map_or("nothing was read", String::as_str)
            ))
        })
    }
}

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// A folder has no size of its own.
fn plausible(stat: &Stat) -> bool {
    stat.attributes & FILE_ATTRIBUTE_DIRECTORY == 0 || (stat.size == 0 && stat.allocated == 0)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mft::tests::{record, A};
    use crate::mft::Standard;
    use crate::raw::tests::{file, mft_zero, runs, Image, RECORD};
    use std::collections::VecDeque;

    const ROOT: u64 = 5 | 5 << 48;

    /// A small NTFS volume image: the root folder and files in records 30 and 31.
    pub(crate) fn sample_image() -> Image {
        let size = 64 * RECORD;
        let all = [(10, 16)];
        let mut image = Image::new(48, size);
        image.put(&all, 0, &mft_zero(size, runs(&[(Some(10), 16)]), None));
        let root = record(
            5,
            0,
            3,
            &[A::Standard(Standard::default()), A::Name(ROOT, ".", 3)],
        );
        image.put(&all, 5, &root);
        image.put(&all, 30, &file("near.txt"));
        image.put(&all, 31, &file("other.txt"));
        image
    }

    /// What Keel knows about volume `C:` before it asks for the MFT.
    pub(crate) fn start(serial: u32) -> Start {
        Start {
            id: VolumeId {
                letter: 'C',
                serial,
            },
            root: ROOT,
            journal: Journal {
                id: 7,
                next_usn: 1_000,
                lowest_valid_usn: 0,
            },
        }
    }

    /// A fake elevated backend: volume C: (serial 7) with fixed answers.
    #[derive(Default)]
    pub(crate) struct Mock {
        /// The volume the MFT frames are made from ([`wire::send_volume`]).
        pub image: Option<Image>,
        /// Frames sent as they are instead (a service that misbehaves).
        pub frames: Vec<Vec<u8>>,
        /// Raw journal buffers, one per call; then an empty one at `end`.
        pub journal: Mutex<VecDeque<Vec<u8>>>,
        pub end: i64,
        pub stats: HashMap<u64, Stat>,
    }

    impl Elevated for Mock {
        fn volumes(&self) -> Vec<(char, u32)> {
            vec![('C', 7)]
        }
        fn query_journal(&self, _: char) -> io::Result<Journal> {
            Ok(Journal {
                id: 42,
                next_usn: self.end,
                lowest_valid_usn: 0,
            })
        }
        fn read_journal(&self, _: char, journal_id: u64, start: i64) -> io::Result<Vec<u8>> {
            if journal_id != 42 {
                return Err(io::Error::from_raw_os_error(1181));
            }
            Ok(self
                .journal
                .lock()
                .pop_front()
                .unwrap_or_else(|| self.end.max(start).to_le_bytes().to_vec()))
        }
        fn stat(&self, _: char, frns: &[u64]) -> Vec<Option<Stat>> {
            frns.iter().map(|f| self.stats.get(f).copied()).collect()
        }
        fn send_mft(
            &self,
            letter: char,
            out: &mut dyn FnMut(&[u8]) -> io::Result<()>,
        ) -> io::Result<()> {
            if let Some(image) = &self.image {
                return wire::send_volume(image, letter, 7, out);
            }
            if self.frames.is_empty() {
                // ERROR_ACCESS_DENIED before anything was sent.
                return Err(io::Error::from_raw_os_error(5));
            }
            self.frames.iter().try_for_each(|f| out(f))
        }
    }

    /// Both ends of a connection in memory: the client's writes are served by `serve`
    /// on each read.
    struct Loop<'a> {
        backend: &'a dyn Elevated,
        to_server: Vec<u8>,
        from_server: VecDeque<u8>,
    }

    impl Write for Loop<'_> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.to_server.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Read for Loop<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.from_server.is_empty() && !self.to_server.is_empty() {
                let input = std::mem::take(&mut self.to_server);
                let mut conn = Duplex {
                    input: &input[..],
                    output: Vec::new(),
                };
                serve(&mut conn, self.backend)?;
                self.from_server.extend(conn.output);
            }
            self.from_server.read(buf)
        }
    }

    pub(crate) struct Duplex<'a> {
        pub input: &'a [u8],
        pub output: Vec<u8>,
    }

    impl Read for Duplex<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Duplex<'_> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn client(backend: &dyn Elevated) -> Client<Loop<'_>> {
        Client::handshake(Loop {
            backend,
            to_server: Vec::new(),
            from_server: VecDeque::new(),
        })
        .unwrap()
    }

    /// The responses `serve` writes for `requests` (raw frames in, decoded out).
    pub(crate) fn exchange(backend: &dyn Elevated, requests: &[Vec<u8>]) -> Vec<Response> {
        let mut input = Vec::new();
        for r in requests {
            write_frame(&mut input, r).unwrap();
        }
        let mut conn = Duplex {
            input: &input,
            output: Vec::new(),
        };
        serve(&mut conn, backend).unwrap();
        let mut out = &conn.output[..];
        let mut responses = Vec::new();
        while let Some(frame) = read_frame(&mut out, MAX_RESPONSE).unwrap() {
            responses.push(Response::decode(&frame).unwrap());
        }
        responses
    }

    pub(crate) fn journal_buffer(next: i64, records: &[Vec<u8>]) -> Vec<u8> {
        let mut buf = next.to_le_bytes().to_vec();
        for r in records {
            buf.extend_from_slice(r);
        }
        buf
    }

    #[test]
    fn hello_and_volumes() {
        let mock = Mock::default();
        let c = client(&mock);
        assert_eq!(c.pid, std::process::id());
        assert_eq!(c.version, env!("CARGO_PKG_VERSION"));
        assert!(c.serves(VolumeId {
            letter: 'C',
            serial: 7
        }));
        // Same letter, another volume (a letter mapped differently for this user).
        assert!(!c.serves(VolumeId {
            letter: 'C',
            serial: 8
        }));
        assert!(!c.serves(VolumeId {
            letter: 'D',
            serial: 7
        }));
    }

    #[test]
    fn requests_for_other_volumes_are_refused() {
        let mock = Mock::default();
        let answers = exchange(
            &mock,
            &[
                Request::QueryJournal { letter: 'D' }.encode(),
                Request::ReadMft { letter: 'Z' }.encode(),
                Request::QueryJournal { letter: 'C' }.encode(),
            ],
        );
        assert!(matches!(
            answers[0],
            Response::Error {
                code: E_BAD_VOLUME,
                ..
            }
        ));
        assert!(matches!(
            answers[1],
            Response::Error {
                code: E_BAD_VOLUME,
                ..
            }
        ));
        assert!(matches!(answers[2], Response::Journal { id: 42, .. }));
    }

    #[test]
    fn a_malformed_request_ends_the_connection() {
        let mock = Mock::default();
        let answers = exchange(
            &mock,
            &[vec![crate::proto::VERSION, 0x7f], Request::Hello.encode()],
        );
        assert_eq!(
            answers.len(),
            1,
            "nothing after the bad request is answered"
        );
        assert!(matches!(
            answers[0],
            Response::Error {
                code: E_BAD_REQUEST,
                ..
            }
        ));
        // An oversized frame is refused before it is read.
        let mut input = Vec::new();
        input.extend_from_slice(&((MAX_REQUEST + 1) as u32).to_le_bytes());
        let mut conn = Duplex {
            input: &input,
            output: Vec::new(),
        };
        assert!(serve(&mut conn, &mock).is_err());
        assert!(conn.output.is_empty());
    }

    /// The wire frames `serve` writes for a `ReadMft` request, decoded.
    fn mft_frames(backend: &dyn Elevated) -> Vec<Vec<u8>> {
        let mut input = Vec::new();
        write_frame(&mut input, &Request::ReadMft { letter: 'C' }.encode()).unwrap();
        let mut conn = Duplex {
            input: &input,
            output: Vec::new(),
        };
        serve(&mut conn, backend).unwrap();
        let mut out = &conn.output[..];
        let mut frames = Vec::new();
        while let Some(frame) = read_frame(&mut out, MAX_RESPONSE).unwrap() {
            frames.push(frame);
        }
        frames
    }

    #[test]
    fn the_service_streams_raw_records_and_keel_builds_the_index() {
        let mock = Mock {
            image: Some(sample_image()),
            ..Mock::default()
        };
        // What leaves the service: the volume header, raw `FILE` records, the end.
        let frames = mft_frames(&mock);
        let decoded: Vec<_> = frames
            .iter()
            .map(|f| wire::Frame::decode(f).unwrap())
            .collect();
        assert_eq!(
            decoded[0],
            wire::Frame::Volume {
                letter: 'C',
                serial: 7
            }
        );
        assert_eq!(decoded.last(), Some(&wire::Frame::End { full: true }));
        let mut numbers = Vec::new();
        for frame in &decoded[1..decoded.len() - 1] {
            let wire::Frame::Records(records) = frame else {
                panic!("{frame:?}")
            };
            for (number, bytes) in records.iter() {
                assert!(bytes.starts_with(b"FILE"));
                numbers.push(number);
            }
        }
        assert_eq!(numbers, [5, 30, 31], "system records are not sent");

        // Keel parses them, as it does for the one-shot helper.
        let mut c = client(&mock);
        let (index, meta) = c.read_mft(start(7)).unwrap();
        assert!(meta.full);
        assert_eq!((meta.journal_id, meta.next_usn), (7, 1_000));
        assert_eq!(index.path(30 | 1 << 48).unwrap(), r"C:\near.txt");
        assert_eq!(index.path(31 | 1 << 48).unwrap(), r"C:\other.txt");
        assert_eq!(index.info(30 | 1 << 48).unwrap().size, 5);
        assert!(!c.broken());
        // The connection is still good for the next request.
        assert!(c.query_journal('C').is_ok());
    }

    #[test]
    fn a_stream_for_another_volume_builds_nothing() {
        let mock = Mock {
            image: Some(sample_image()),
            ..Mock::default()
        };
        let mut c = client(&mock);
        // This process sees drive C: as another volume (serial 8): not taken.
        let err = c.read_mft(start(8)).err().unwrap();
        assert!(err.to_string().contains("nothing was read"), "{err}");
        assert!(!c.broken(), "a well-formed stream is not a protocol error");
    }

    #[test]
    fn a_service_that_cannot_read_the_volume_is_not_a_protocol_error() {
        // It refuses before any frame: an error answer with the Windows code.
        let refusing = Mock::default();
        let mut c = client(&refusing);
        let err = c.read_mft(start(7)).err().unwrap();
        assert_eq!(err.raw_os_error(), Some(5));
        assert!(!c.broken());
        // It starts the volume and reports that the read failed.
        let failed = Mock {
            frames: vec![
                wire::Frame::Volume {
                    letter: 'C',
                    serial: 7,
                }
                .encode(),
                wire::Frame::Failed("cannot open the volume".into()).encode(),
            ],
            ..Mock::default()
        };
        let mut c = client(&failed);
        let err = c.read_mft(start(7)).err().unwrap();
        assert!(err.to_string().contains("cannot open the volume"), "{err}");
        assert!(!c.broken());
        assert!(c.query_journal('C').is_ok());
    }

    #[test]
    fn a_stream_out_of_protocol_breaks_the_client() {
        let volume = wire::Frame::Volume {
            letter: 'C',
            serial: 7,
        }
        .encode();
        let end = wire::Frame::End { full: true }.encode();
        let cases: [(&str, Vec<Vec<u8>>); 6] = [
            // Records before any volume.
            (
                "records first",
                vec![wire::Frame::Enum(&[0; 8]).encode(), end.clone()],
            ),
            // The helper's final frame: a service never sends it.
            ("done", vec![volume.clone(), wire::Frame::Done.encode()]),
            // An unknown frame kind, a short frame, another protocol version.
            ("unknown kind", vec![vec![2, 0, 0, 0, 1, 0x77]]),
            ("short", vec![vec![1, 0, 0, 0, 1]]),
            ("version", vec![vec![2, 0, 0, 0, 9, 1]]),
            // An enumeration buffer nobody asked for.
            (
                "enum unasked",
                vec![volume.clone(), wire::Frame::Enum(&[0; 8]).encode(), end],
            ),
        ];
        for (what, frames) in cases {
            let mock = Mock {
                frames,
                ..Mock::default()
            };
            let mut c = client(&mock);
            assert!(c.read_mft(start(7)).is_err(), "{what}");
            assert!(c.broken(), "{what}");
        }
        // A response of another kind in the middle of the stream.
        let answer = Mock {
            frames: vec![write_response(&Response::Volumes(vec![]))],
            ..Mock::default()
        };
        let mut c = client(&answer);
        assert!(c.read_mft(start(7)).is_err());
        assert!(c.broken());
    }

    fn write_response(r: &Response) -> Vec<u8> {
        let mut out = Vec::new();
        write_frame(&mut out, &r.encode()).unwrap();
        out
    }

    #[test]
    fn mft_frames_fit_the_response_limit() {
        // 5000 records are about 5 MiB: more than one batch.
        let all = [(10, 1280)];
        let size = 5120 * RECORD;
        let mut image = Image::new(1300, size);
        image.put(&all, 0, &mft_zero(size, runs(&[(Some(10), 1280)]), None));
        for number in 16..5100 {
            image.put(&all, number, &file("f.txt"));
        }
        let mock = Mock {
            image: Some(image),
            ..Mock::default()
        };
        let frames = mft_frames(&mock);
        let batches = frames.len() - 2;
        assert!(batches >= 2, "{batches} frames");
        let mut records = 0;
        for f in &frames[1..frames.len() - 1] {
            assert!(f.len() <= MAX_RESPONSE);
            let Ok(wire::Frame::Records(r)) = wire::Frame::decode(f) else {
                panic!("not records")
            };
            records += r.len();
        }
        assert_eq!(records, 5100 - 16);
    }

    #[test]
    fn journal_reads_loop_to_the_end_and_check_progress() {
        use crate::ntfs::usn::tests::record_bytes;
        let a = record_bytes(2, 100, 5, 0x100, 0x20, "new.txt");
        let b = record_bytes(2, 101, 5, 0x100, 0x20, "two.txt");
        let bad = record_bytes(2, 102, 5, 0x100, 0x20, "a\\b");
        let mock = Mock {
            journal: Mutex::new(VecDeque::from([
                journal_buffer(200, &[a]),
                journal_buffer(300, &[b, bad]),
            ])),
            end: 300,
            ..Mock::default()
        };
        let mut c = client(&mock);
        let (records, next) = c.read_journal('C', 42, 100).unwrap();
        assert_eq!(next, 300);
        let names: Vec<&str> = records.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["new.txt", "two.txt"],
            "a name with a separator dropped"
        );

        // The service's own error code survives (the journal was truncated).
        let err = c.read_journal('C', 41, 100).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(1181));
        assert!(!c.broken(), "a reported error keeps the connection");

        // A journal that goes backwards is a protocol error.
        let back = Mock {
            journal: Mutex::new(VecDeque::from([journal_buffer(50, &[])])),
            ..Mock::default()
        };
        let mut c = client(&back);
        assert!(c.read_journal('C', 42, 100).is_err());
        assert!(c.broken());

        // One that never moves while sending records too.
        let stuck = Mock {
            journal: Mutex::new(VecDeque::from([journal_buffer(
                100,
                &[record_bytes(2, 100, 5, 0x100, 0x20, "x")],
            )])),
            ..Mock::default()
        };
        let mut c = client(&stuck);
        assert!(c.read_journal('C', 42, 100).is_err());
    }

    #[test]
    fn stats_come_back_per_id_and_folders_have_no_size() {
        let file = Stat {
            size: 10,
            allocated: 4096,
            attributes: 0x20,
            links: 1,
            ..Stat::default()
        };
        let bad_dir = Stat {
            size: 10,
            attributes: 0x10,
            ..Stat::default()
        };
        let mock = Mock {
            stats: HashMap::from([(1, file), (2, bad_dir)]),
            ..Mock::default()
        };
        let mut c = client(&mock);
        let frns: Vec<u64> = (0..2500).collect();
        let stats = c.stat('C', &frns).unwrap();
        assert_eq!(stats, HashMap::from([(1, file)]));
        // A letter that is not A-Z never leaves the client.
        assert_eq!(
            c.stat('c', &[1]).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn a_stat_answer_of_the_wrong_length_breaks_the_client() {
        struct Short;
        impl Elevated for Short {
            fn volumes(&self) -> Vec<(char, u32)> {
                vec![('C', 7)]
            }
            fn query_journal(&self, _: char) -> io::Result<Journal> {
                Err(io::Error::from_raw_os_error(1179))
            }
            fn read_journal(&self, _: char, _: u64, _: i64) -> io::Result<Vec<u8>> {
                Ok(Vec::new())
            }
            fn stat(&self, _: char, _: &[u64]) -> Vec<Option<Stat>> {
                Vec::new()
            }
            fn send_mft(
                &self,
                _: char,
                _: &mut dyn FnMut(&[u8]) -> io::Result<()>,
            ) -> io::Result<()> {
                Err(io::Error::other("refused"))
            }
        }
        let mut c = client(&Short);
        // `serve` pads a short answer to one per id: all gone.
        assert!(c.stat('C', &[1, 2]).unwrap().is_empty());
        // ERROR_JOURNAL_NOT_ACTIVE comes through as itself.
        assert_eq!(c.query_journal('C').unwrap_err().raw_os_error(), Some(1179));
        // A journal read shorter than its USN is an error answer, not a crash.
        assert!(c.read_journal('C', 1, 0).is_err());
        // A failed MFT read is reported, and the connection stays usable.
        assert!(c.read_mft(start(7)).is_err());
        assert!(!c.broken());
        assert!(c.query_journal('C').is_err());
    }
}
