//! The in-tree half of Keel's fuzzing (docs/fuzzing.md): every libFuzzer target in `fuzz/`
//! is also run by `cargo test`, on its committed corpus (`fuzz/corpus/<target>/`) and then on
//! a bounded number of seeded mutations of it. A case that panics, runs past [`CASE_LIMIT`]
//! or (with [`Capped`] as the test binary's allocator) asks for one block over [`ALLOC_CAP`]
//! fails the test with the input saved under `target/fuzz-failures/`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Longest one case may take (debug build, a busy machine).
pub const CASE_LIMIT: Duration = Duration::from_secs(10);
/// Largest single allocation a target may make.
pub const ALLOC_CAP: usize = 512 << 20;
/// Longest generated input.
const MAX_LEN: usize = 64 << 10;

/// Generated cases per target: `KEEL_FUZZ_CASES`, else 500 under CI and 2,000 elsewhere.
pub fn cases() -> usize {
    std::env::var("KEEL_FUZZ_CASES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(if std::env::var_os("CI").is_some() {
            500
        } else {
            2000
        })
}

/// SplitMix64: small, seeded, the same sequence on every platform.
pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// `0..n` (`n > 0`).
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// Values that sit on the edges of size and offset fields.
const INTERESTING: [u64; 12] = [
    0,
    1,
    0x7f,
    0x80,
    0xff,
    0x7fff,
    0xffff,
    0x7fff_ffff,
    0xffff_ffff,
    0x1_0000_0000,
    i64::MAX as u64,
    u64::MAX,
];

/// One input derived from the seeds: a few byte-level mutations of one of them (bit flips,
/// edge values in 1/2/4/8-byte fields of either byte order, edge numbers as text, syntax
/// characters, inserted, deleted, repeated and spliced ranges, truncation).
pub fn mutate(rng: &mut Rng, seeds: &[Vec<u8>]) -> Vec<u8> {
    let mut data = match seeds.len() {
        0 => Vec::new(),
        n => seeds[rng.below(n)].clone(),
    };
    for _ in 0..1 + rng.below(8) {
        let len = data.len();
        match rng.below(10) {
            0 if len > 0 => data[rng.below(len)] ^= 1 << rng.below(8),
            1 if len > 0 => data[rng.below(len)] = rng.next_u64() as u8,
            2 if len > 0 => {
                let width = [1, 2, 4, 8][rng.below(4)];
                let value = INTERESTING[rng.below(INTERESTING.len())];
                let bytes = if rng.below(2) == 0 {
                    value.to_le_bytes()
                } else {
                    value.to_be_bytes()
                };
                let bytes = if bytes[0] == 0 && width < 8 && rng.below(2) == 0 {
                    &bytes[8 - width..]
                } else {
                    &bytes[..width]
                };
                let at = rng.below(len);
                let end = (at + width).min(len);
                data[at..end].copy_from_slice(&bytes[..end - at]);
            }
            3 => {
                let at = rng.below(len + 1);
                let n = 1 + rng.below(16);
                let bytes: Vec<u8> = (0..n).map(|_| rng.next_u64() as u8).collect();
                data.splice(at..at, bytes);
            }
            4 if len > 0 => {
                let at = rng.below(len);
                let n = 1 + rng.below((len - at).min(64));
                data.drain(at..at + n);
            }
            5 if len > 0 => {
                let at = rng.below(len);
                let n = 1 + rng.below((len - at).min(64));
                let chunk = data[at..at + n].to_vec();
                for _ in 0..1 + rng.below(4) {
                    let to = rng.below(data.len() + 1);
                    data.splice(to..to, chunk.iter().copied());
                }
            }
            6 if !seeds.is_empty() => {
                let other = &seeds[rng.below(seeds.len())];
                if !other.is_empty() {
                    let at = rng.below(other.len());
                    let n = 1 + rng.below((other.len() - at).min(256));
                    let to = rng.below(len + 1);
                    data.splice(to..to, other[at..at + n].iter().copied());
                }
            }
            7 if len > 0 => data.truncate(rng.below(len)),
            8 => {
                // Edge numbers as text (sizes, dates, ports and counts typed or in markup).
                const NUMBERS: &[&str] = &[
                    "0",
                    "-1",
                    "65536",
                    "4294967296",
                    "9223372036854775807",
                    "-9223372036854775808",
                    "9223372036854775808",
                    "18446744073709551616",
                    "99999999999999999999",
                    "1e999",
                    "NaN",
                ];
                let at = rng.below(len + 1);
                let n = NUMBERS[rng.below(NUMBERS.len())];
                data.splice(at..at, n.bytes());
            }
            _ => {
                const SYNTAX: &[u8] = b"\0\n\r\"'%/\\:=#*?[]{}<>&;,.~!@ \t";
                let at = rng.below(len + 1);
                data.insert(at, SYNTAX[rng.below(SYNTAX.len())]);
            }
        }
    }
    data.truncate(MAX_LEN);
    data
}

/// `fuzz/corpus/<target>/*`: the libFuzzer seeds and every input that once broke a target.
pub fn corpus(target: &str) -> Vec<Vec<u8>> {
    let dir = workspace().join("fuzz").join("corpus").join(target);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    files.iter().map(|p| std::fs::read(p).unwrap()).collect()
}

fn workspace() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while !dir.join("fuzz").is_dir() {
        assert!(
            dir.pop(),
            "no fuzz/ folder above {}",
            env!("CARGO_MANIFEST_DIR")
        );
    }
    dir
}

/// Runs `target` on its corpus and then on [`cases`] mutations (seeded by the target's
/// name, so a failure reproduces). Panics with the failing input's path on the first case
/// that panics or hangs.
pub fn run(target: &'static str, f: fn(&[u8])) {
    let seeds = corpus(target);
    assert!(!seeds.is_empty(), "fuzz/corpus/{target} is empty");
    let seed = target.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    });
    let mut rng = Rng::new(seed);
    let total = seeds.len() + cases();
    let (job_tx, job_rx) = mpsc::channel::<Vec<u8>>();
    let (done_tx, done_rx) = mpsc::channel::<bool>();
    std::thread::Builder::new()
        .name(format!("fuzz-{target}"))
        .spawn(move || {
            for input in job_rx {
                let ok =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&input))).is_ok();
                if done_tx.send(ok).is_err() {
                    break;
                }
            }
        })
        .unwrap();
    let running = failures().join(format!("{target}-running"));
    let mut slowest = Duration::ZERO;
    for case in 0..total {
        let input = match seeds.get(case) {
            Some(s) => s.clone(),
            None => mutate(&mut rng, &seeds),
        };
        // Kept if the allocator aborts the run (see `Capped`).
        let _ = std::fs::write(&running, &input);
        let started = Instant::now();
        job_tx.send(input.clone()).unwrap();
        let outcome = done_rx.recv_timeout(CASE_LIMIT);
        slowest = slowest.max(started.elapsed());
        let why = match outcome {
            Ok(true) => continue,
            Ok(false) => "panicked",
            Err(_) => "ran past the time limit",
        };
        let _ = std::fs::remove_file(&running);
        let saved = save(target, &input);
        panic!(
            "fuzz target {target}: case {case} {why}; input saved to {}",
            saved.display()
        );
    }
    let _ = std::fs::remove_file(&running);
    eprintln!("fuzz target {target}: {total} cases, slowest {slowest:?}");
}

fn fingerprint(input: &[u8]) -> u64 {
    input.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

fn failures() -> PathBuf {
    let dir = workspace().join("target").join("fuzz-failures");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn save(target: &str, input: &[u8]) -> PathBuf {
    let path = failures().join(format!("{target}-{:016x}", fingerprint(input)));
    let _ = std::fs::write(&path, input);
    path
}

/// A test binary's allocator that refuses (and so aborts on) any single block over
/// [`ALLOC_CAP`]: a size taken from a hostile header instead of from the input. The input
/// being run stays in `target/fuzz-failures/<target>-running`.
pub struct Capped;
unsafe impl GlobalAlloc for Capped {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() > ALLOC_CAP {
            over(layout.size());
            return std::ptr::null_mut();
        }
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() > ALLOC_CAP {
            over(layout.size());
            return std::ptr::null_mut();
        }
        System.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        if new > ALLOC_CAP {
            over(new);
            return std::ptr::null_mut();
        }
        System.realloc(ptr, layout, new)
    }
}

/// Says what happened without allocating.
fn over(size: usize) {
    use std::io::Write;
    let mut hex = [0u8; 16];
    for (i, b) in hex.iter_mut().enumerate() {
        *b = b"0123456789abcdef"[(size >> (60 - 4 * i)) & 15];
    }
    let mut err = std::io::stderr();
    let _ = err.write_all(b"fuzz: allocation of 0x");
    let _ = err.write_all(&hex);
    let _ = err.write_all(b" bytes refused; the input is target/fuzz-failures/*-running\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutations_are_seeded_and_bounded() {
        let seeds = vec![b"PK\x03\x04hello".to_vec(), vec![0; 100]];
        let a: Vec<Vec<u8>> = {
            let mut rng = Rng::new(7);
            (0..200).map(|_| mutate(&mut rng, &seeds)).collect()
        };
        let mut rng = Rng::new(7);
        for x in &a {
            assert_eq!(*x, mutate(&mut rng, &seeds));
            assert!(x.len() <= MAX_LEN);
        }
        assert!(a.iter().any(|x| !seeds.contains(x)));
    }
}
