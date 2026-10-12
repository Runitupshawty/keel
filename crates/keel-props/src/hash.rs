//! Content hashes (`md5:`, `sha1:`, `sha256:`, `crc32:`, `crc64:`), computed as a stream in
//! one pass for any set of algorithms, and cancelable between chunks.

use crate::{Cancel, Error};
use md5::Digest as _;
use std::io::Read;

const CHUNK: usize = 256 * 1024;

/// CRC-64/XZ (ECMA-182 polynomial, reflected, as 7-Zip and xz print it).
static CRC64: crc::Crc<u64, crc::Table<16>> = crc::Crc::<u64, crc::Table<16>>::new(&crc::CRC_64_XZ);

/// A hash algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HashAlgo {
    Md5,
    Sha1,
    Sha256,
    /// CRC-32 (ISO-HDLC, as zip and Everything print it).
    Crc32,
    /// CRC-64/XZ.
    Crc64,
}

impl HashAlgo {
    pub const ALL: [HashAlgo; 5] = [
        HashAlgo::Md5,
        HashAlgo::Sha1,
        HashAlgo::Sha256,
        HashAlgo::Crc32,
        HashAlgo::Crc64,
    ];

    /// The function name (`sha256`).
    pub fn name(self) -> &'static str {
        match self {
            HashAlgo::Md5 => "md5",
            HashAlgo::Sha1 => "sha1",
            HashAlgo::Sha256 => "sha256",
            HashAlgo::Crc32 => "crc32",
            HashAlgo::Crc64 => "crc64",
        }
    }
}

enum State {
    Md5(md5::Md5),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Crc32(crc32fast::Hasher),
    Crc64(crc::Digest<'static, u64, crc::Table<16>>),
}

impl State {
    fn new(algo: HashAlgo) -> State {
        match algo {
            HashAlgo::Md5 => State::Md5(md5::Md5::new()),
            HashAlgo::Sha1 => State::Sha1(<sha1::Sha1 as sha1::Digest>::new()),
            HashAlgo::Sha256 => State::Sha256(<sha2::Sha256 as sha2::Digest>::new()),
            HashAlgo::Crc32 => State::Crc32(crc32fast::Hasher::new()),
            HashAlgo::Crc64 => State::Crc64(CRC64.digest()),
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            State::Md5(h) => h.update(data),
            State::Sha1(h) => sha1::Digest::update(h, data),
            State::Sha256(h) => sha2::Digest::update(h, data),
            State::Crc32(h) => h.update(data),
            State::Crc64(h) => h.update(data),
        }
    }

    fn finish(self) -> String {
        match self {
            State::Md5(h) => hex(&h.finalize()),
            State::Sha1(h) => hex(&sha1::Digest::finalize(h)),
            State::Sha256(h) => hex(&sha2::Digest::finalize(h)),
            State::Crc32(h) => format!("{:08x}", h.finalize()),
            State::Crc64(h) => format!("{:016x}", h.finalize()),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Hashes everything `r` yields with each of `algos` in one pass; returns lowercase hex
/// digests in the order asked. Checks `cancel` before every chunk.
pub fn hash_reader<R: Read>(
    mut r: R,
    algos: &[HashAlgo],
    cancel: &Cancel,
) -> Result<Vec<String>, Error> {
    let mut states: Vec<State> = algos.iter().map(|&a| State::new(a)).collect();
    let mut buf = vec![0u8; CHUNK];
    loop {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        for s in &mut states {
            s.update(&buf[..n]);
        }
    }
    Ok(states.into_iter().map(State::finish).collect())
}

/// One algorithm over one byte slice (tests, small inputs).
pub fn hash_bytes(data: &[u8], algo: HashAlgo) -> String {
    let mut s = State::new(algo);
    s.update(data);
    s.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        let d = b"123456789";
        assert_eq!(hash_bytes(d, HashAlgo::Crc32), "cbf43926");
        assert_eq!(hash_bytes(d, HashAlgo::Crc64), "995dc9bbdf1939fa");
        let abc = b"abc";
        assert_eq!(
            hash_bytes(abc, HashAlgo::Md5),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        assert_eq!(
            hash_bytes(abc, HashAlgo::Sha1),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hash_bytes(abc, HashAlgo::Sha256),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hash_bytes(b"", HashAlgo::Md5),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
    }

    #[test]
    fn streaming_matches_one_shot() {
        let data: Vec<u8> = (0..(CHUNK * 2 + 77))
            .map(|i| (i * 31 % 251) as u8)
            .collect();
        let got = hash_reader(&data[..], &HashAlgo::ALL, &Cancel::default()).unwrap();
        for (algo, h) in HashAlgo::ALL.iter().zip(&got) {
            assert_eq!(h, &hash_bytes(&data, *algo), "{}", algo.name());
        }
    }

    #[test]
    fn cancel_stops_it() {
        let c = Cancel::default();
        c.cancel();
        let r = hash_reader(&b"abc"[..], &[HashAlgo::Sha256], &c);
        assert!(matches!(r, Err(Error::Cancelled)));
    }
}
