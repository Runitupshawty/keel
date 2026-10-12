//! The index service's request handling (both ends of [`crate::proto`]), apart from
//! the Win32 plumbing in `svcwin`: [`serve`] answers one connection for a backend
//! that does the elevated reads ([`Elevated`]; [`LocalVolumes`] in the service), and
//! [`Client`] is what the unelevated Keel uses, re-validating every answer.
//!
//! The service only reads: it hands back the journal's raw buffers (parsed by the
//! client), sizes and dates by file id, and the MFT as index rows. It never returns
//! file contents (a small file's data kept inside its MFT record stays in the
//! service: rows carry only the columns the index stores), never writes, and takes
//! requests only for drive letters that are fixed local NTFS volumes in the
//! service's own (global) drive namespace. See `docs/index-service.md`.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::Arc;

use std::cell::RefCell;

use parking_lot::Mutex;

use crate::mft;
use crate::proto::{
    read_frame, valid_letter, valid_name, write_frame, Request, Response, Row, Stat, E_BAD_REQUEST,
    E_BAD_VOLUME, E_FAILED, MAX_JOURNAL, MAX_REQUEST, MAX_RESPONSE, MAX_ROWS, MAX_STAT,
};

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
    /// Every file of the volume as a row, in MFT order; `Ok(true)` when they came
    /// from the MFT itself, `Ok(false)` from the enumeration fallback. `reset` is
    /// called when rows already handed over must be dropped (the MFT read failed half
    /// way and the enumeration starts over).
    fn read_mft(
        &self,
        letter: char,
        row: &mut dyn FnMut(Row),
        reset: &mut dyn FnMut(),
    ) -> anyhow::Result<bool>;
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

/// Bytes of rows per [`Response::Rows`] frame, well under [`MAX_RESPONSE`].
const ROWS_BYTES: usize = MAX_RESPONSE / 2;

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

/// Sends the volume's rows in frames of at most [`MAX_ROWS`] rows and about
/// [`ROWS_BYTES`] bytes, then [`Response::MftEnd`]. A failed write stops sending (the
/// read itself runs to its end: it cannot be interrupted half way).
fn stream_mft(conn: &mut impl Write, backend: &dyn Elevated, letter: char) -> io::Result<()> {
    struct Batch<'a, W: Write> {
        conn: &'a mut W,
        rows: Vec<Row>,
        bytes: usize,
        sent: u64,
        failed: Option<io::Error>,
    }
    impl<W: Write> Batch<'_, W> {
        fn flush(&mut self) {
            if self.rows.is_empty() || self.failed.is_some() {
                self.rows.clear();
                self.bytes = 0;
                return;
            }
            let rows = std::mem::take(&mut self.rows);
            self.sent += rows.len() as u64;
            self.bytes = 0;
            if let Err(e) = send(&mut *self.conn, &Response::Rows(rows)) {
                self.failed = Some(e);
            }
        }
    }
    let mut batch = Batch {
        conn,
        rows: Vec::new(),
        bytes: 0,
        sent: 0,
        failed: None,
    };
    let batch = RefCell::new(&mut batch);
    let result = backend.read_mft(
        letter,
        &mut |row| {
            // Names that would not survive the client's checks are not sent.
            if !valid_name(&row.name) {
                return;
            }
            let mut b = batch.borrow_mut();
            b.bytes += 80 + row.name.len();
            b.rows.push(row);
            if b.rows.len() >= MAX_ROWS || b.bytes >= ROWS_BYTES {
                b.flush();
            }
        },
        &mut || {
            let mut b = batch.borrow_mut();
            b.rows.clear();
            b.bytes = 0;
            if b.failed.is_none() {
                if let Err(e) = send(&mut *b.conn, &Response::MftReset) {
                    b.failed = Some(e);
                }
            }
            b.sent = 0;
        },
    );
    let batch = batch.into_inner();
    batch.flush();
    if let Some(e) = batch.failed.take() {
        return Err(e);
    }
    let end = match result {
        Ok(full) => Response::MftEnd {
            full,
            rows: batch.sent,
        },
        Err(e) => Response::error(E_FAILED, format!("{e:#}")),
    };
    send(&mut *batch.conn, &end)
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
        if let Some((_, v)) = self.handles.lock().get(&letter) {
            return Ok(v.clone());
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
        Ok(volume)
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

    fn read_mft(
        &self,
        letter: char,
        row: &mut dyn FnMut(Row),
        reset: &mut dyn FnMut(),
    ) -> anyhow::Result<bool> {
        let volume = self.volume(letter)?;
        let mut sent = false;
        let direct = super::read_mft(&volume, |entry| {
            if let Some(r) = super::row_of(&entry) {
                sent = true;
                row(r);
            }
        });
        match direct {
            Ok(()) => Ok(true),
            Err(e) => {
                super::warn(&format!(
                    "reading the MFT of {letter}: directly: {e:#}; enumerating instead"
                ));
                if sent {
                    reset();
                }
                volume.enumerate(|records| {
                    for r in records {
                        row(super::row_of_record(&r));
                    }
                })?;
                Ok(false)
            }
        }
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
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("index service: {what}"),
    )
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

    /// Reads one response; an error response becomes an `io::Error` (a Windows code
    /// keeps its raw OS error, so ERROR_JOURNAL_ENTRY_DELETED still reads as such).
    fn receive(&mut self) -> io::Result<Response> {
        let frame = read_frame(&mut self.conn, MAX_RESPONSE);
        let frame = match self.transport(frame)? {
            Some(f) => f,
            None => return Err(self.fail("the service closed the connection")),
        };
        let response = Response::decode(&frame).map_err(io::Error::from);
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

    fn call(&mut self, request: &Request) -> io::Result<Response> {
        let sent = write_frame(&mut self.conn, &request.encode());
        self.transport(sent)?;
        self.receive()
    }

    fn check_letter(&self, letter: char) -> io::Result<()> {
        if valid_letter(letter) {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::InvalidInput, "bad drive letter"))
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

    /// The volume's rows (see [`Request::ReadMft`]); `reset` when the service starts
    /// over. Returns whether they came from the MFT itself.
    pub fn read_mft(
        &mut self,
        letter: char,
        mut row: impl FnMut(Row),
        mut reset: impl FnMut(),
    ) -> io::Result<bool> {
        self.check_letter(letter)?;
        let sent = write_frame(&mut self.conn, &Request::ReadMft { letter }.encode());
        self.transport(sent)?;
        let mut count = 0u64;
        loop {
            match self.receive()? {
                Response::Rows(rows) => {
                    count += rows.len() as u64;
                    for r in rows {
                        if plausible_row(&r) {
                            row(r);
                        }
                    }
                }
                Response::MftReset => {
                    count = 0;
                    reset();
                }
                Response::MftEnd { full, rows } if rows == count => return Ok(full),
                _ => return Err(self.fail("bad MFT stream")),
            }
        }
    }
}

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// A folder has no size of its own.
fn plausible(stat: &Stat) -> bool {
    stat.attributes & FILE_ATTRIBUTE_DIRECTORY == 0 || (stat.size == 0 && stat.allocated == 0)
}

/// What the index takes from a row: not one of NTFS's own files (records 0 to 15,
/// except the root folder 5), not its own parent (but the root, which is), a folder bit that agrees with the
/// attributes, and no size on a folder. The name was checked by the decoder.
fn plausible_row(r: &Row) -> bool {
    let number = r.frn & mft::RECORD_MASK;
    let dir_attr = r.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    let root = number == super::ROOT_RECORD;
    (number >= super::FIRST_USER_RECORD || root)
        && (r.frn != r.parent || root)
        && r.parent != 0
        && dir_attr == r.is_dir
        && (!r.is_dir || (r.size == 0 && r.allocated == 0))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::proto::tests::sample_row;
    use crate::proto::MAX_NAME_UNITS;
    use std::collections::VecDeque;

    /// A fake elevated backend: volume C: (serial 7) with fixed answers.
    #[derive(Default)]
    pub(crate) struct Mock {
        pub rows: Vec<Row>,
        /// Rows sent before the "MFT read" fails and the enumeration starts over.
        pub torn: Vec<Row>,
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
        fn read_mft(
            &self,
            _: char,
            row: &mut dyn FnMut(Row),
            reset: &mut dyn FnMut(),
        ) -> anyhow::Result<bool> {
            if !self.torn.is_empty() {
                self.torn.iter().cloned().for_each(&mut *row);
                reset();
            }
            self.rows.iter().cloned().for_each(row);
            Ok(self.torn.is_empty())
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
        assert!(matches!(answers[0], Response::Error { code: E_BAD_VOLUME, .. }));
        assert!(matches!(answers[1], Response::Error { code: E_BAD_VOLUME, .. }));
        assert!(matches!(answers[2], Response::Journal { id: 42, .. }));
    }

    #[test]
    fn a_malformed_request_ends_the_connection() {
        let mock = Mock::default();
        let answers = exchange(
            &mock,
            &[
                vec![crate::proto::VERSION, 0x7f],
                Request::Hello.encode(),
            ],
        );
        assert_eq!(answers.len(), 1, "nothing after the bad request is answered");
        assert!(matches!(answers[0], Response::Error { code: E_BAD_REQUEST, .. }));
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

    #[test]
    fn mft_rows_stream_and_are_revalidated() {
        let dir = sample_row(0x0002_0000_0000_0020, 0x0005_0000_0000_0005, "Users", true);
        let file = sample_row(0x0001_0000_0000_0021, dir.frn, "a.txt", false);
        // Each of these is dropped by the client: a system record, its own parent,
        // no parent, a folder with a size, a folder bit that disagrees.
        let system = sample_row(3, 5, "$Volume", false);
        let own_parent = sample_row(40, 40, "loop", false);
        let orphan = sample_row(41, 0, "orphan", false);
        let mut sized_dir = sample_row(42, 5, "sized", true);
        sized_dir.size = 1;
        let mut mismatch = sample_row(43, 5, "mismatch", false);
        mismatch.attributes |= 0x10;
        let mock = Mock {
            rows: vec![dir.clone(), file.clone(), system, own_parent, orphan, sized_dir, mismatch],
            ..Mock::default()
        };
        let mut c = client(&mock);
        let got = RefCell::new(Vec::new());
        let full = c
            .read_mft('C', |r| got.borrow_mut().push(r), || got.borrow_mut().clear())
            .unwrap();
        assert!(full);
        assert_eq!(got.into_inner(), vec![dir.clone(), file.clone()]);
        assert!(!c.broken());

        // A torn MFT read: the client drops what came before the reset.
        let torn = Mock {
            torn: vec![sample_row(50, 5, "half", false)],
            rows: vec![dir.clone()],
            ..Mock::default()
        };
        let mut c = client(&torn);
        let got = RefCell::new(Vec::new());
        assert!(!c
            .read_mft('C', |r| got.borrow_mut().push(r), || got.borrow_mut().clear())
            .unwrap());
        assert_eq!(got.into_inner(), vec![dir]);
    }

    #[test]
    fn mft_rows_are_batched_under_the_frame_limit() {
        let rows: Vec<Row> = (0..40_000u64)
            .map(|i| sample_row(100 + i, 5, &format!("{:0>200}", i), false))
            .collect();
        let mock = Mock {
            rows: rows.clone(),
            ..Mock::default()
        };
        let answers = exchange(&mock, &[Request::ReadMft { letter: 'C' }.encode()]);
        let batches = answers.len() - 1;
        assert!(batches >= 3, "{batches} frames");
        let mut got = Vec::new();
        for a in &answers[..batches] {
            let Response::Rows(r) = a else {
                panic!("{a:?}")
            };
            assert!(r.len() <= MAX_ROWS);
            assert!(a.encode().len() <= MAX_RESPONSE);
            got.extend(r.iter().cloned());
        }
        assert_eq!(got, rows);
        assert_eq!(
            answers[batches],
            Response::MftEnd {
                full: true,
                rows: 40_000
            }
        );
    }

    #[test]
    fn the_service_never_sends_a_bad_name() {
        let mut long = sample_row(60, 5, "x", false);
        long.name = "y".repeat(MAX_NAME_UNITS + 1);
        let mock = Mock {
            rows: vec![sample_row(61, 5, "a\\b", false), long, sample_row(62, 5, "ok", false)],
            ..Mock::default()
        };
        let answers = exchange(&mock, &[Request::ReadMft { letter: 'C' }.encode()]);
        assert_eq!(
            answers,
            vec![
                Response::Rows(vec![sample_row(62, 5, "ok", false)]),
                Response::MftEnd {
                    full: true,
                    rows: 1
                }
            ]
        );
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
        assert_eq!(names, ["new.txt", "two.txt"], "a name with a separator dropped");

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
            fn read_mft(
                &self,
                _: char,
                _: &mut dyn FnMut(Row),
                _: &mut dyn FnMut(),
            ) -> anyhow::Result<bool> {
                anyhow::bail!("refused")
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
        assert!(c.read_mft('C', |_| {}, || {}).is_err());
        assert!(!c.broken());
        assert!(c.query_journal('C').is_err());
    }
}
