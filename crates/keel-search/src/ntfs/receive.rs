//! The unelevated end of the index helper's stream ([`crate::wire`]): builds each
//! volume's index from the raw MFT records the helper sends. Nothing from the helper
//! is trusted: frames are decoded strictly, only volumes this process asked for are
//! taken, record numbers must increase, every record goes through the MFT parser
//! (made for hostile bytes, and fuzzed), and names that are not a single path
//! component are dropped.

use anyhow::{anyhow, bail, ensure};

use super::db::Meta;
use super::index::{Index, Info};
use super::usn::parse_buffer;
use super::win::{Journal, VolumeId};
use super::{add_entry, warn};
use crate::mft;
use crate::wire::Frame;

/// What this process knows about a volume before the helper reads it: the journal
/// position is taken first, so changes made during the read are replayed afterwards.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Start {
    pub id: VolumeId,
    pub root: u64,
    pub journal: Journal,
}

struct Building {
    start: Start,
    index: Index,
    scan: mft::Scan,
    /// Bytes per record, fixed by the first batch.
    size: Option<usize>,
    /// The least record number the next record may have.
    next: u64,
    /// The raw read failed and the enumeration is coming instead.
    enumerating: bool,
}

impl Building {
    fn new(start: Start) -> Self {
        let mut index = Index::new(start.id.drive(), start.root);
        index.begin_bulk();
        Building {
            start,
            index,
            scan: mft::Scan::default(),
            size: None,
            next: 0,
            enumerating: false,
        }
    }
}

/// Where the stream is.
enum State {
    /// Between volumes.
    Idle,
    /// Inside a volume this process did not ask for: its frames are dropped.
    Skipping,
    Building(Box<Building>),
}

/// Feeds frames in; calls `done` with each volume's finished index.
pub(crate) struct Receiver<F: FnMut(VolumeId, Index, Meta)> {
    starts: Vec<Start>,
    seen: Vec<(char, u32)>,
    state: State,
    done: F,
    /// Every volume was sent.
    pub complete: bool,
    /// Volumes the helper could not read, with why.
    pub failed: Vec<String>,
}

/// A name the index can show: one path component (the root folder's own name aside).
fn plausible(frn: u64, name: &str) -> bool {
    frn & mft::RECORD_MASK == super::ROOT_RECORD
        || !(name.is_empty() || name == "." || name == ".." || name.contains(['\\', '/', '\0']))
}

impl<F: FnMut(VolumeId, Index, Meta)> Receiver<F> {
    pub fn new(starts: Vec<Start>, done: F) -> Self {
        Receiver {
            starts,
            seen: Vec::new(),
            state: State::Idle,
            done,
            complete: false,
            failed: Vec::new(),
        }
    }

    /// The volume being built; None while skipping one.
    fn current(&mut self) -> anyhow::Result<Option<&mut Building>> {
        match &mut self.state {
            State::Idle => bail!("index helper sent records outside a volume"),
            State::Skipping => Ok(None),
            State::Building(b) => Ok(Some(b.as_mut())),
        }
    }

    /// Ends the current volume: the one built, or None when it was skipped.
    fn end(&mut self, what: &str) -> anyhow::Result<Option<Box<Building>>> {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Idle => bail!("index helper {what} a volume it never started"),
            State::Skipping => Ok(None),
            State::Building(b) => Ok(Some(b)),
        }
    }

    /// Takes one frame's payload. An error means the stream cannot be trusted: stop
    /// reading it.
    pub fn frame(&mut self, payload: &[u8]) -> anyhow::Result<()> {
        ensure!(
            !self.complete,
            "index helper sent more after its last frame"
        );
        match Frame::decode(payload)? {
            Frame::Volume { letter, serial } => {
                ensure!(
                    matches!(self.state, State::Idle),
                    "index helper started a volume inside another"
                );
                ensure!(
                    !self.seen.contains(&(letter, serial)),
                    "index helper sent {letter}: twice"
                );
                self.seen.push((letter, serial));
                let start = self
                    .starts
                    .iter()
                    .find(|s| s.id.letter == letter && s.id.serial == serial);
                self.state = match start {
                    Some(start) => State::Building(Box::new(Building::new(*start))),
                    None => {
                        warn(&format!(
                            "the index helper sent {letter}:, which Keel did not ask for: skipped"
                        ));
                        State::Skipping
                    }
                };
            }
            Frame::Records(records) => {
                let Some(b) = self.current()? else {
                    return Ok(());
                };
                ensure!(!b.enumerating, "index helper sent records after a restart");
                ensure!(
                    *b.size.get_or_insert(records.size) == records.size,
                    "index helper changed the record size"
                );
                for (number, bytes) in records.iter() {
                    ensure!(
                        number >= b.next && number <= mft::RECORD_MASK,
                        "index helper sent record {number} out of order"
                    );
                    b.next = number + 1;
                    if !bytes.starts_with(b"FILE") {
                        continue;
                    }
                    // A record the parser refuses is skipped, as a torn one would be.
                    let Ok(rec) = mft::parse_fixed(bytes) else {
                        continue;
                    };
                    if let Some(entry) = b.scan.add_parsed(number, rec) {
                        add_checked(&mut b.index, &entry);
                    }
                }
            }
            Frame::Restart(why) => {
                let Some(b) = self.current()? else {
                    return Ok(());
                };
                warn(&format!(
                    "reading the MFT of {} directly: {why}; enumerating instead",
                    b.start.id.drive()
                ));
                *b = Building {
                    enumerating: true,
                    ..Building::new(b.start)
                };
            }
            Frame::Enum(buf) => {
                let Some(b) = self.current()? else {
                    return Ok(());
                };
                ensure!(b.enumerating, "index helper sent an enumeration unasked");
                let (_, records) =
                    parse_buffer(buf).ok_or_else(|| anyhow!("bad enumeration buffer"))?;
                for r in records {
                    if !plausible(r.frn, &r.name) {
                        continue;
                    }
                    b.index.upsert(r.frn, r.parent, &r.name, r.is_dir);
                    let info = Info {
                        attributes: r.attributes,
                        ..Info::default()
                    };
                    b.index.set_info(r.frn, info);
                }
            }
            Frame::End { full } => {
                let Some(mut b) = self.end("ended")? else {
                    return Ok(());
                };
                ensure!(
                    full != b.enumerating,
                    "index helper's end frame contradicts its stream"
                );
                for entry in std::mem::take(&mut b.scan).finish() {
                    add_checked(&mut b.index, &entry);
                }
                b.index.settle();
                let meta = Meta {
                    journal_id: b.start.journal.id,
                    next_usn: b.start.journal.next_usn,
                    root: b.start.root,
                    volume: b.start.id.drive(),
                    full,
                };
                let Building { start, index, .. } = *b;
                (self.done)(start.id, index, meta);
            }
            Frame::Failed(why) => {
                if let Some(b) = self.end("failed")? {
                    self.failed.push(format!("{}: {why}", b.start.id.drive()));
                }
            }
            Frame::Done => {
                ensure!(
                    matches!(self.state, State::Idle),
                    "index helper stopped inside a volume"
                );
                self.complete = true;
            }
        }
        Ok(())
    }
}

fn add_checked(index: &mut Index, entry: &mft::Entry) {
    if entry
        .names
        .first()
        .is_some_and(|n| plausible(entry.frn, &n.name))
    {
        add_entry(index, entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mft::tests::{record, A};
    use crate::mft::Standard;
    use crate::raw::tests::{file, mft_zero, runs, Image, RECORD};
    use crate::raw::RawVolume;
    use crate::wire::tests::payloads;
    use crate::wire::{send_volume, Batch};

    const ROOT: u64 = 5 | 5 << 48;

    fn start(letter: char, serial: u32) -> Start {
        Start {
            id: VolumeId { letter, serial },
            root: ROOT,
            journal: Journal {
                id: 7,
                next_usn: 1_000,
                lowest_valid_usn: 0,
            },
        }
    }

    fn stream_of(volume: &dyn RawVolume, letter: char, serial: u32) -> Vec<u8> {
        let mut stream = Vec::new();
        send_volume(volume, letter, serial, &mut |f| {
            stream.extend(f);
            Ok(())
        })
        .unwrap();
        stream.extend(Frame::Done.encode());
        stream
    }

    fn receive(starts: Vec<Start>, stream: &[u8]) -> anyhow::Result<Vec<(VolumeId, Index, Meta)>> {
        let mut built = Vec::new();
        let mut rx = Receiver::new(starts, |id, index, meta| built.push((id, index, meta)));
        for p in payloads(stream) {
            rx.frame(&p)?;
        }
        ensure!(rx.complete, "incomplete");
        drop(rx);
        Ok(built)
    }

    /// A fragmented MFT read by the helper and indexed here, end to end: files past
    /// the first extent, a hard-linked file's extension record, the journal position
    /// taken before the read.
    #[test]
    fn a_volume_is_indexed_from_the_helpers_records() {
        let size = 64 * RECORD;
        let all = [(10, 8), (40, 8)];
        let mut image = Image::new(48, size);
        let list = vec![mft::ListEntry {
            kind: mft::ATTR_DATA,
            start_vcn: 8,
            record: 20 | 1 << 48,
            named: false,
        }];
        image.put(&all, 0, &mft_zero(size, runs(&[(Some(10), 8)]), Some(list)));
        let ext = record(
            1,
            1 << 48,
            1,
            &[A::NonResident(
                "",
                8,
                size,
                size,
                None,
                0,
                runs(&[(Some(40), 8)]),
            )],
        );
        image.put(&all, 20, &ext);
        let root = record(
            5,
            0,
            3,
            &[A::Standard(Standard::default()), A::Name(ROOT, ".", 3)],
        );
        image.put(&all, 5, &root);
        image.put(&all, 30, &file("near.txt"));
        image.put(&all, 50, &file("far.txt"));
        let stream = stream_of(&image, 'C', 9);
        let built = receive(vec![start('C', 9)], &stream).unwrap();
        assert_eq!(built.len(), 1);
        let (id, index, meta) = &built[0];
        assert_eq!(id.serial, 9);
        assert!(meta.full);
        assert_eq!((meta.journal_id, meta.next_usn), (7, 1_000));
        assert_eq!(index.path(30 | 1 << 48).unwrap(), r"C:\near.txt");
        assert_eq!(index.path(50 | 1 << 48).unwrap(), r"C:\far.txt");
        assert_eq!(index.info(50 | 1 << 48).unwrap().size, 5);
    }

    /// The enumeration fallback builds a names-only index, marked not full.
    #[test]
    fn a_restart_builds_from_the_enumeration() {
        use super::super::usn::tests::record_bytes;
        let mut buf = 99_u64.to_le_bytes().to_vec();
        buf.extend(record_bytes(3, 30 | 1 << 48, ROOT, 0, 0x20, "listed.txt"));
        buf.extend(record_bytes(3, 31 | 1 << 48, ROOT, 0, 0x20, r"..\escape"));
        let mut stream = Frame::Volume {
            letter: 'D',
            serial: 4,
        }
        .encode();
        stream.extend(Frame::Restart("sparse MFT run".into()).encode());
        stream.extend(Frame::Enum(&buf).encode());
        stream.extend(Frame::End { full: false }.encode());
        stream.extend(Frame::Done.encode());
        let built = receive(vec![start('D', 4)], &stream).unwrap();
        let (_, index, meta) = &built[0];
        assert!(!meta.full);
        assert_eq!(index.path(30 | 1 << 48).unwrap(), r"D:\listed.txt");
        assert!(index.get(31 | 1 << 48).is_none(), "a name with a separator");
    }

    /// Streams the unelevated side refuses: a volume it did not ask for, records out
    /// of order, an end that contradicts the stream, a stream cut short, a hostile
    /// record (skipped, not fatal).
    #[test]
    fn the_receiver_refuses_what_it_did_not_ask_for() {
        let vol = |l, s| {
            Frame::Volume {
                letter: l,
                serial: s,
            }
            .encode()
        };
        let batch = |numbers: &[u64], rec: &[u8]| {
            let mut b = Batch::new(1024);
            for &n in numbers {
                b.push(n, rec);
            }
            let mut out = Vec::new();
            b.send(&mut |f| {
                out.extend(f);
                Ok(())
            })
            .unwrap();
            out
        };
        let mut good = file("a.txt");
        mft::fixup(&mut good).unwrap();
        let end = Frame::End { full: true }.encode();
        let done = Frame::Done.encode();
        let cases: Vec<(Vec<u8>, &str)> = vec![
            ([vol('E', 1), done.clone()].concat(), "inside a volume"),
            (
                [vol('C', 1), batch(&[20, 19], &good), end.clone()].concat(),
                "out of order",
            ),
            (
                [vol('C', 1), batch(&[20, 20], &good), end.clone()].concat(),
                "out of order",
            ),
            (
                [vol('C', 1), Frame::End { full: false }.encode()].concat(),
                "contradicts",
            ),
            ([vol('C', 1), done.clone()].concat(), "inside a volume"),
            ([vol('C', 1), end.clone(), vol('C', 1)].concat(), "twice"),
            (batch(&[20], &good), "outside a volume"),
            ([vol('C', 1), end.clone()].concat(), "incomplete"),
        ];
        for (stream, says) in cases {
            let err = receive(vec![start('C', 1)], &stream)
                .unwrap_err()
                .to_string();
            assert!(err.contains(says), "{says}: {err}");
        }
        // A volume not asked for is skipped (here with the same serial on another
        // letter); the next one is still built.
        let stream = [
            vol('E', 1),
            batch(&[20], &good),
            end.clone(),
            vol('C', 1),
            batch(&[22], &good),
            end.clone(),
            done.clone(),
        ]
        .concat();
        let built = receive(vec![start('C', 1)], &stream).unwrap();
        assert_eq!(built.len(), 1);
        assert_eq!(built[0].0.letter, 'C');

        // A record the parser refuses is dropped; the rest of the volume stands.
        let mut bad = good.clone();
        bad[0x18..0x1C].copy_from_slice(&u32::MAX.to_le_bytes());
        let stream = [
            vol('C', 1),
            batch(&[20, 21], &bad),
            batch(&[22], &good),
            end,
            done,
        ]
        .concat();
        let built = receive(vec![start('C', 1)], &stream).unwrap();
        assert_eq!(built[0].1.path(22 | 1 << 48).unwrap(), r"C:\a.txt");
        assert!(built[0].1.get(20 | 1 << 48).is_none());
    }
}
