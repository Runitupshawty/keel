//! What the elevated index helper (`keel --index-service`) hands the unelevated Keel:
//! each volume's MFT records as they are on disk, never anything built from them. The
//! helper only locates the MFT ([`crate::raw::layout`]: record 0 and the `$MFT`'s own
//! extension records), applies each record's update sequence and blanks what the
//! index never reads ([`mft::blank_fixed`]: file contents kept inside records, the
//! slack), so no file contents leave it. Parsing the records, merging them into files
//! and building the index all happen in the unelevated process, which treats every
//! byte of this stream as untrusted (`ntfs::receive`).
//!
//! The stream is a run of frames: a little-endian `u32` payload length, then the
//! payload: [`VERSION`], a kind byte and the kind's fields, which must use the payload
//! up exactly. Per volume: [`Frame::Volume`], then [`Frame::Records`] (or, when the
//! raw read fails, [`Frame::Restart`] and [`Frame::Enum`] buffers of
//! `FSCTL_ENUM_USN_DATA` output), then [`Frame::End`] or [`Frame::Failed`]; after the
//! last volume, [`Frame::Done`].

// Only Windows reads volumes; the tests and the fuzz build use the rest everywhere.
#![allow(dead_code)]

use std::io::{self, Read};

use anyhow::{anyhow, bail, ensure};

use crate::mft;
use crate::raw::{layout, read_records, RawVolume};

/// Protocol version: a helper and a Keel of different versions refuse each other.
pub(crate) const VERSION: u8 = 1;
/// Largest payload: a batch of records, or one enumeration buffer (1 MiB).
pub(crate) const MAX_FRAME: usize = 8 << 20;
/// A record batch is sent once it holds this many bytes.
const BATCH: usize = 4 << 20;
/// Longest error message carried.
const MAX_MESSAGE: usize = 1024;
/// NTFS's system files (records 0 to 15) are never indexed, except the root folder;
/// the helper does not send them.
const FIRST_USER_RECORD: u64 = 16;
const ROOT_RECORD: u64 = 5;

const VOLUME: u8 = 1;
const RECORDS: u8 = 2;
const RESTART: u8 = 3;
const ENUM: u8 = 4;
const END: u8 = 5;
const FAILED: u8 = 6;
const DONE: u8 = 7;

/// One decoded frame, borrowing from its payload.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Frame<'a> {
    /// What follows (up to [`Frame::End`] or [`Frame::Failed`]) is this volume's.
    Volume { letter: char, serial: u32 },
    /// MFT records in increasing record number: update sequence applied, contents
    /// blanked.
    Records(Records<'a>),
    /// The raw read failed (why): drop this volume's records so far; enumeration
    /// buffers follow.
    Restart(String),
    /// One raw `FSCTL_ENUM_USN_DATA` output buffer.
    Enum(&'a [u8]),
    /// The volume is complete; `full` when it came from the MFT itself.
    End { full: bool },
    /// The volume could not be read at all (why).
    Failed(String),
    /// Every volume was sent.
    Done,
}

/// The records of a [`Frame::Records`]: (record number, `size` bytes) each.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Records<'a> {
    pub size: usize,
    bytes: &'a [u8],
}

impl<'a> Records<'a> {
    pub fn iter(&self) -> impl Iterator<Item = (u64, &'a [u8])> + 'a {
        let bytes: &'a [u8] = self.bytes;
        bytes.chunks_exact(8 + self.size).map(|c| {
            let (number, record) = c.split_at(8);
            (
                u64::from_le_bytes(number.try_into().unwrap_or_default()),
                record,
            )
        })
    }

    pub fn len(&self) -> usize {
        self.bytes.len() / (8 + self.size)
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

fn message(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_MESSAGE)]).into_owned()
}

impl<'a> Frame<'a> {
    /// Decodes one payload (without its length prefix). Strict: an unknown version or
    /// kind, a short or long payload, a drive letter outside `A`..`Z`, an impossible
    /// record size: each an error, never a panic.
    pub fn decode(payload: &'a [u8]) -> anyhow::Result<Frame<'a>> {
        let [version, kind, rest @ ..] = payload else {
            bail!("index helper frame too short");
        };
        ensure!(
            *version == VERSION,
            "index helper protocol {version}, this Keel speaks {VERSION}"
        );
        let exact = |n: usize| {
            ensure!(
                rest.len() == n,
                "index helper frame of kind {kind} has a bad length"
            );
            Ok(())
        };
        Ok(match *kind {
            VOLUME => {
                exact(5)?;
                let letter = char::from(rest[0]);
                ensure!(
                    letter.is_ascii_uppercase(),
                    "bad drive letter in index helper frame"
                );
                let serial = u32::from_le_bytes([rest[1], rest[2], rest[3], rest[4]]);
                Frame::Volume { letter, serial }
            }
            RECORDS => {
                let Some((size, bytes)) = rest.split_first_chunk::<4>() else {
                    bail!("index helper frame of kind {kind} has a bad length");
                };
                let size = u32::from_le_bytes(*size) as usize;
                ensure!(
                    (512..=mft::MAX_RECORD).contains(&size) && size.is_power_of_two(),
                    "index helper sent {size}-byte MFT records"
                );
                ensure!(
                    bytes.len().is_multiple_of(8 + size),
                    "index helper frame of kind {kind} has a bad length"
                );
                Frame::Records(Records { size, bytes })
            }
            RESTART => Frame::Restart(message(rest)),
            ENUM => Frame::Enum(rest),
            END => {
                exact(1)?;
                ensure!(rest[0] <= 1, "bad index helper end frame");
                Frame::End { full: rest[0] == 1 }
            }
            FAILED => Frame::Failed(message(rest)),
            DONE => {
                exact(0)?;
                Frame::Done
            }
            _ => bail!("unknown index helper frame kind {kind}"),
        })
    }

    /// The frame with its length prefix. Records are built with a [`Batch`].
    pub fn encode(&self) -> Vec<u8> {
        let mut p = vec![0, 0, 0, 0, VERSION];
        match self {
            Frame::Volume { letter, serial } => {
                p.push(VOLUME);
                p.push(*letter as u8);
                p.extend(serial.to_le_bytes());
            }
            Frame::Records(r) => {
                p.push(RECORDS);
                p.extend((r.size as u32).to_le_bytes());
                p.extend(r.bytes);
            }
            Frame::Restart(why) => {
                p.push(RESTART);
                p.extend(&why.as_bytes()[..why.len().min(MAX_MESSAGE)]);
            }
            Frame::Enum(buf) => {
                p.push(ENUM);
                p.extend(*buf);
            }
            Frame::End { full } => p.extend([END, u8::from(*full)]),
            Frame::Failed(why) => {
                p.push(FAILED);
                p.extend(&why.as_bytes()[..why.len().min(MAX_MESSAGE)]);
            }
            Frame::Done => p.push(DONE),
        }
        let len = (p.len() - 4) as u32;
        p[..4].copy_from_slice(&len.to_le_bytes());
        p
    }
}

/// Builds [`Frame::Records`] frames in place.
pub(crate) struct Batch {
    buf: Vec<u8>,
    size: usize,
}

impl Batch {
    pub fn new(size: usize) -> Self {
        let mut batch = Batch {
            buf: Vec::new(),
            size,
        };
        batch.clear();
        batch
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.buf.extend([0, 0, 0, 0, VERSION, RECORDS]);
        self.buf.extend((self.size as u32).to_le_bytes());
    }

    pub fn push(&mut self, number: u64, record: &[u8]) {
        debug_assert_eq!(record.len(), self.size);
        self.buf.extend(number.to_le_bytes());
        self.buf.extend(record);
    }

    pub fn is_empty(&self) -> bool {
        self.buf.len() == 10
    }

    pub fn is_full(&self) -> bool {
        self.buf.len() >= BATCH
    }

    /// Hands the finished frame to `out` and starts the next.
    pub fn send(&mut self, out: &mut dyn FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
        let len = (self.buf.len() - 4) as u32;
        self.buf[..4].copy_from_slice(&len.to_le_bytes());
        let sent = out(&self.buf);
        self.clear();
        sent
    }
}

/// Reads the next frame's payload into `buf`. False at the end of the stream (between
/// frames); an error for a frame cut short or larger than [`MAX_FRAME`].
pub(crate) fn read_frame(r: &mut impl Read, buf: &mut Vec<u8>) -> io::Result<bool> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len[got..])? {
            0 if got == 0 => return Ok(false),
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => got += n,
        }
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("index helper frame of {len} bytes"),
        ));
    }
    buf.clear();
    buf.resize(len, 0);
    r.read_exact(buf)?;
    Ok(true)
}

/// Readies one raw record to leave the helper: applies its update sequence and blanks
/// what the index never reads. False for a record not worth sending (free, torn,
/// damaged, not a `FILE` record).
pub(crate) fn blank(record: &mut [u8]) -> bool {
    mft::fixup(record).is_ok() && mft::blank_fixed(record) == Ok(true)
}

/// Why sending a volume stopped.
enum Stop {
    /// The output is gone (the reader closed the pipe): stop everything.
    Out(io::Error),
    /// The raw read failed: enumerate instead.
    Read(anyhow::Error),
}

/// The helper's side for one volume: the [`Frame::Volume`] header, the raw MFT
/// records, and [`Frame::End`]; when the raw read fails, a [`Frame::Restart`] and the
/// enumeration, or [`Frame::Failed`]. Errors only when `out` fails.
pub(crate) fn send_volume(
    volume: &dyn RawVolume,
    letter: char,
    serial: u32,
    out: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    out(&Frame::Volume { letter, serial }.encode())?;
    let why = match send_mft(volume, out) {
        Ok(()) => return out(&Frame::End { full: true }.encode()),
        Err(Stop::Out(e)) => return Err(e),
        Err(Stop::Read(e)) => format!("{e:#}"),
    };
    out(&Frame::Restart(why).encode())?;
    let mut lost = None;
    let listed = volume.enumerate_raw(&mut |buf| {
        out(&Frame::Enum(buf).encode()).inspect_err(|e| lost = Some(e.kind()))
    });
    if let Some(kind) = lost {
        return Err(kind.into());
    }
    match listed {
        Ok(()) => out(&Frame::End { full: false }.encode()),
        Err(e) => out(&Frame::Failed(format!("enumerating the MFT: {e}")).encode()),
    }
}

fn send_mft(
    volume: &dyn RawVolume,
    out: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<(), Stop> {
    let layout = layout(volume).map_err(Stop::Read)?;
    let mut batch = Batch::new(layout.record as usize);
    let mut lost = None;
    let read = read_records(volume, &layout, |number, record| {
        if (number < FIRST_USER_RECORD && number != ROOT_RECORD) || !blank(record) {
            return Ok(());
        }
        batch.push(number, record);
        if batch.is_full() {
            if let Err(e) = batch.send(out) {
                lost = Some(e);
                return Err(anyhow!("the index output closed"));
            }
        }
        Ok(())
    });
    if let Some(e) = lost {
        return Err(Stop::Out(e));
    }
    read.map_err(Stop::Read)?;
    if !batch.is_empty() {
        batch.send(out).map_err(Stop::Out)?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mft::tests::{record, A};
    use crate::mft::Standard;
    use crate::raw::tests::{file, mft_zero, runs, Image, RECORD};

    /// Splits a stream into its payloads.
    pub(crate) fn payloads(stream: &[u8]) -> Vec<Vec<u8>> {
        let mut r = stream;
        let mut out = Vec::new();
        let mut buf = Vec::new();
        while read_frame(&mut r, &mut buf).unwrap() {
            out.push(buf.clone());
        }
        out
    }

    #[test]
    fn frames_round_trip_and_bad_ones_are_refused() {
        let record = vec![7u8; 1024];
        let mut batch = Batch::new(1024);
        batch.push(16, &record);
        batch.push(17, &record);
        let mut stream = Vec::new();
        batch
            .send(&mut |f| {
                stream.extend(f);
                Ok(())
            })
            .unwrap();
        assert!(batch.is_empty());
        for f in [
            Frame::Volume {
                letter: 'C',
                serial: 0xABCD,
            },
            Frame::Restart("why".into()),
            Frame::Enum(&[1, 2, 3]),
            Frame::End { full: true },
            Frame::Failed("no".into()),
            Frame::Done,
        ] {
            stream.extend(f.encode());
        }
        let frames = payloads(&stream);
        let Frame::Records(r) = Frame::decode(&frames[0]).unwrap() else {
            panic!("not records");
        };
        let got: Vec<u64> = r
            .iter()
            .map(|(n, b)| {
                assert_eq!(b, &record[..]);
                n
            })
            .collect();
        assert_eq!(got, [16, 17]);
        assert_eq!(
            Frame::decode(&frames[1]).unwrap(),
            Frame::Volume {
                letter: 'C',
                serial: 0xABCD
            }
        );
        assert_eq!(Frame::decode(&frames[3]).unwrap(), Frame::Enum(&[1, 2, 3]));
        assert_eq!(Frame::decode(&frames[6]).unwrap(), Frame::Done);

        for bad in [
            &[][..],
            &[VERSION],
            &[9, DONE],
            &[VERSION, 99],
            &[VERSION, DONE, 0],
            &[VERSION, VOLUME, b'c', 0, 0, 0, 0],
            &[VERSION, VOLUME, b'C', 0, 0, 0],
            &[VERSION, END, 2],
            &[VERSION, RECORDS, 0, 4, 0],
            &[VERSION, RECORDS, 0, 3, 0, 0],
            &[VERSION, RECORDS, 0, 4, 0, 0, 1],
        ] {
            assert!(Frame::decode(bad).is_err(), "{bad:?}");
        }
        // Cut short, or claiming more than a frame may hold.
        let done = Frame::Volume {
            letter: 'C',
            serial: 1,
        }
        .encode();
        let mut short = &done[..7];
        assert!(read_frame(&mut short, &mut Vec::new()).is_err());
        let huge = ((MAX_FRAME + 1) as u32).to_le_bytes();
        assert!(read_frame(&mut &huge[..], &mut Vec::new()).is_err());
    }

    /// The helper's stream for a volume: system records left out, a small file's
    /// contents and the slack blanked, and the receiving parse sees the same file.
    #[test]
    fn the_helper_sends_blanked_raw_records() {
        let size = 32 * RECORD;
        let all = [(10, 8)];
        let mut image = Image::new(24, size);
        image.put(&all, 0, &mft_zero(size, runs(&[(Some(10), 8)]), None));
        let secret = record(
            1,
            0,
            1,
            &[
                A::Standard(Standard::default()),
                A::Name(5 | 5 << 48, "secret.txt", 1),
                A::Resident(40),
                A::Stream("Zone.Identifier", 26),
            ],
        );
        image.put(&all, 20, &secret);
        let mut free = file("free.txt");
        free[0x16] = 0;
        image.put(&all, 21, &free);
        let mut stream = Vec::new();
        send_volume(&image, 'C', 1, &mut |f| {
            stream.extend(f);
            Ok(())
        })
        .unwrap();
        let frames = payloads(&stream);
        assert_eq!(frames.len(), 3, "volume, one batch, end");
        let Frame::Records(r) = Frame::decode(&frames[1]).unwrap() else {
            panic!("not records");
        };
        let sent: Vec<(u64, &[u8])> = r.iter().collect();
        assert_eq!(
            sent.len(),
            1,
            "record 0 and the free record stay in the helper"
        );
        let (number, bytes) = sent[0];
        assert_eq!(number, 20);
        // The resident contents ('x' and 'y' bytes) did not leave.
        assert!(!bytes.windows(4).any(|w| w == b"xxxx" || w == b"yyyy"));
        let mut original = secret.clone();
        let want = mft::parse(&mut original).unwrap();
        let got = mft::parse_fixed(bytes).unwrap();
        assert_eq!(got, want);
        assert_eq!(got.data.unwrap().size, 40);
        assert_eq!(got.streams, 1);
        assert_eq!(
            Frame::decode(&frames[2]).unwrap(),
            Frame::End { full: true }
        );
    }

    /// A raw read that fails says why and switches to the enumeration; a closed
    /// output stops the helper.
    #[test]
    fn a_failed_read_restarts_with_the_enumeration() {
        struct Listing(Image);
        impl RawVolume for Listing {
            fn ntfs_data(&self) -> io::Result<crate::raw::NtfsData> {
                self.0.ntfs_data()
            }
            fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
                self.0.read_at(offset, buf)
            }
            fn enumerate_raw(
                &self,
                each: &mut dyn FnMut(&[u8]) -> io::Result<()>,
            ) -> io::Result<()> {
                each(&[1, 0, 0, 0, 0, 0, 0, 0, 0xAA])?;
                each(&[2, 0, 0, 0, 0, 0, 0, 0, 0xBB])
            }
        }
        // Sparse MFT: the layout is refused.
        let size = 32 * RECORD;
        let mut image = Image::new(24, size);
        image.put(
            &[(10, 4)],
            0,
            &mft_zero(size, runs(&[(Some(10), 4), (None, 4)]), None),
        );
        let listing = Listing(image);
        let mut stream = Vec::new();
        send_volume(&listing, 'D', 2, &mut |f| {
            stream.extend(f);
            Ok(())
        })
        .unwrap();
        let frames = payloads(&stream);
        let kinds: Vec<Frame> = frames.iter().map(|p| Frame::decode(p).unwrap()).collect();
        assert!(matches!(&kinds[1], Frame::Restart(why) if why.contains("sparse")));
        assert_eq!(kinds[2], Frame::Enum(&[1, 0, 0, 0, 0, 0, 0, 0, 0xAA]));
        assert_eq!(kinds[4], Frame::End { full: false });

        // No enumeration either: the volume fails.
        let mut stream = Vec::new();
        send_volume(&listing.0, 'D', 2, &mut |f| {
            stream.extend(f);
            Ok(())
        })
        .unwrap();
        let frames = payloads(&stream);
        assert!(matches!(
            Frame::decode(&frames[2]).unwrap(),
            Frame::Failed(_)
        ));

        // The reader went away.
        let mut calls = 0;
        let err = send_volume(&listing, 'D', 2, &mut |_| {
            calls += 1;
            if calls > 2 {
                Err(io::ErrorKind::BrokenPipe.into())
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    /// Blanking never changes what the parser reads, on any single-byte change of a
    /// full-featured record.
    #[test]
    fn blanking_keeps_the_parse() {
        let base = record(
            9,
            0,
            1,
            &[
                A::Standard(Standard::default()),
                A::Name(5 | 5 << 48, "a.txt", 1),
                A::Stream("s", 3),
                A::Resident(17),
                A::Reparse(0xA000_000C),
            ],
        );
        for at in 0..0x200 {
            for value in [0x00, 0x01, 0x7F, 0xFF] {
                let mut r = base.clone();
                r[at] = value;
                check_blank(&r);
            }
        }
    }

    /// The invariant [`blank`] keeps (also asserted by the fuzz target).
    pub(crate) fn check_blank(raw: &[u8]) {
        let mut fixed = raw.to_vec();
        if mft::fixup(&mut fixed).is_err() {
            return;
        }
        let before = mft::parse_fixed(&fixed);
        let mut blanked = fixed.clone();
        if mft::blank_fixed(&mut blanked) == Ok(true) {
            assert_eq!(mft::parse_fixed(&blanked), before);
        } else {
            assert_eq!(blanked, fixed, "a refused record is left as it was");
        }
    }
}
