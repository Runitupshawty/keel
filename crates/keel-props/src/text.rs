//! Text statistics: character encoding (byte order mark, UTF-8 validity, a UTF-16 heuristic),
//! line count and word count, read as a stream and capped by size.

use crate::{Cancel, Error};
use std::io::Read;

/// Bytes looked at to choose the encoding.
pub const SNIFF_LEN: usize = 64 * 1024;
const CHUNK: usize = 256 * 1024;

/// A text file's encoding as `character-encoding:` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Only bytes below 0x80.
    Ascii,
    /// Valid UTF-8 without a byte order mark.
    Utf8,
    /// UTF-8 with a byte order mark.
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Utf32Le,
    Utf32Be,
    /// 8-bit text that is not UTF-8 (the system code page).
    Ansi,
    /// NUL bytes or many control characters: not text.
    Binary,
}

impl Encoding {
    /// The name `character-encoding:` matches (`UTF-8`, `UTF-16LE`, `ANSI`).
    pub fn name(self) -> &'static str {
        match self {
            Encoding::Ascii => "ASCII",
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf8Bom => "UTF-8 BOM",
            Encoding::Utf16Le => "UTF-16LE",
            Encoding::Utf16Be => "UTF-16BE",
            Encoding::Utf32Le => "UTF-32LE",
            Encoding::Utf32Be => "UTF-32BE",
            Encoding::Ansi => "ANSI",
            Encoding::Binary => "binary",
        }
    }
}

/// What [`stats`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextStats {
    pub encoding: Encoding,
    /// Lines (a last line without a newline counts); `None` for binary files and files over
    /// the cap.
    pub lines: Option<u64>,
    /// Runs of non-space characters; `None` like `lines`.
    pub words: Option<u64>,
    /// True when the file was longer than the cap: the encoding is from its start only.
    pub truncated: bool,
}

/// Picks the encoding from a file's first bytes.
pub fn sniff(head: &[u8]) -> Encoding {
    if head.starts_with(b"\xef\xbb\xbf") {
        return Encoding::Utf8Bom;
    }
    if head.starts_with(b"\xff\xfe\0\0") {
        return Encoding::Utf32Le;
    }
    if head.starts_with(b"\0\0\xfe\xff") {
        return Encoding::Utf32Be;
    }
    if head.starts_with(b"\xff\xfe") {
        return Encoding::Utf16Le;
    }
    if head.starts_with(b"\xfe\xff") {
        return Encoding::Utf16Be;
    }
    if head.contains(&0) {
        // UTF-16 without a byte order mark: mostly ASCII, so NULs fill one byte of each pair.
        let pairs = head.len() / 2;
        if pairs >= 2 {
            let even = head.iter().step_by(2).filter(|&&b| b == 0).count();
            let odd = head.iter().skip(1).step_by(2).filter(|&&b| b == 0).count();
            if odd * 10 >= pairs * 7 && even * 20 <= pairs {
                return Encoding::Utf16Le;
            }
            if even * 10 >= pairs * 7 && odd * 20 <= pairs {
                return Encoding::Utf16Be;
            }
        }
        return Encoding::Binary;
    }
    let controls = head
        .iter()
        .filter(|&&b| b < 0x20 && !matches!(b, b'\n' | b'\r' | b'\t' | 0x0c | 0x1b))
        .count();
    if controls * 20 > head.len() {
        return Encoding::Binary;
    }
    if head.is_ascii() {
        return Encoding::Ascii;
    }
    match std::str::from_utf8(head) {
        Ok(_) => Encoding::Utf8,
        // A multi-byte character cut by the end of the sniffed bytes is still UTF-8.
        Err(e) if e.error_len().is_none() => Encoding::Utf8,
        Err(_) => Encoding::Ansi,
    }
}

/// Counts lines and words of the text read from `r`, reading at most `cap` bytes.
pub fn stats<R: Read>(mut r: R, cap: u64, cancel: &Cancel) -> Result<TextStats, Error> {
    let mut head = Vec::with_capacity(SNIFF_LEN.min(cap as usize));
    (&mut r).take(SNIFF_LEN as u64).read_to_end(&mut head)?;
    let mut enc = sniff(&head);
    if enc == Encoding::Binary {
        return Ok(TextStats {
            encoding: enc,
            lines: None,
            words: None,
            truncated: false,
        });
    }
    let mut counter = Counter::new(enc);
    let mut total = head.len() as u64;
    let mut truncated = total > cap;
    if !truncated {
        counter.feed(&head);
        let mut buf = vec![0u8; CHUNK];
        loop {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let n = match r.read(&mut buf) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > cap {
                truncated = true;
                break;
            }
            counter.feed(&buf[..n]);
        }
    }
    // A character cut by the end of the file is invalid UTF-8 too.
    let cut_at_end = !truncated && !counter.carry.is_empty();
    if matches!(enc, Encoding::Ascii | Encoding::Utf8) && (counter.invalid_utf8 || cut_at_end) {
        enc = Encoding::Ansi;
    } else if enc == Encoding::Ascii && counter.non_ascii {
        enc = Encoding::Utf8;
    }
    let (lines, words) = if truncated {
        (None, None)
    } else {
        let (l, w) = counter.finish();
        (Some(l), Some(w))
    };
    Ok(TextStats {
        encoding: enc,
        lines,
        words,
        truncated,
    })
}

/// Streaming line and word counter over one encoding's code units.
struct Counter {
    enc: Encoding,
    lines: u64,
    words: u64,
    in_word: bool,
    /// Whether the last unit seen ended a line (or nothing was seen).
    at_line_start: bool,
    any: bool,
    carry: Vec<u8>,
    invalid_utf8: bool,
    non_ascii: bool,
    bom_skipped: bool,
}

impl Counter {
    fn new(enc: Encoding) -> Counter {
        Counter {
            enc,
            lines: 0,
            words: 0,
            in_word: false,
            at_line_start: true,
            any: false,
            carry: Vec::new(),
            invalid_utf8: false,
            non_ascii: false,
            bom_skipped: false,
        }
    }

    fn char(&mut self, c: char) {
        if c == '\u{feff}' && !self.any {
            return; // byte order mark
        }
        self.any = true;
        if c == '\n' {
            self.lines += 1;
            self.at_line_start = true;
        } else {
            self.at_line_start = false;
        }
        if c.is_whitespace() {
            self.in_word = false;
        } else if !self.in_word {
            self.in_word = true;
            self.words += 1;
        }
    }

    fn feed(&mut self, data: &[u8]) {
        let mut bytes = std::mem::take(&mut self.carry);
        bytes.extend_from_slice(data);
        let mut rest: &[u8] = &bytes;
        if !self.bom_skipped {
            self.bom_skipped = true;
            let bom: &[u8] = match self.enc {
                Encoding::Utf8Bom => b"\xef\xbb\xbf",
                Encoding::Utf16Le => b"\xff\xfe",
                Encoding::Utf16Be => b"\xfe\xff",
                Encoding::Utf32Le => b"\xff\xfe\0\0",
                Encoding::Utf32Be => b"\0\0\xfe\xff",
                _ => b"",
            };
            rest = rest.strip_prefix(bom).unwrap_or(rest);
        }
        match self.enc {
            Encoding::Utf16Le | Encoding::Utf16Be => {
                let le = self.enc == Encoding::Utf16Le;
                let whole = rest.len() / 2 * 2;
                let units = rest[..whole].as_chunks::<2>().0.iter().map(|c| {
                    if le {
                        u16::from_le_bytes(*c)
                    } else {
                        u16::from_be_bytes(*c)
                    }
                });
                // A surrogate pair split by a chunk boundary decodes as two replacement
                // characters; both count as one word's letters, so the counts hold.
                for c in char::decode_utf16(units) {
                    self.char(c.unwrap_or('\u{fffd}'));
                }
                self.carry = rest[whole..].to_vec();
            }
            Encoding::Utf32Le | Encoding::Utf32Be => {
                let le = self.enc == Encoding::Utf32Le;
                let whole = rest.len() / 4 * 4;
                for c in rest[..whole].as_chunks::<4>().0 {
                    let v = if le {
                        u32::from_le_bytes(*c)
                    } else {
                        u32::from_be_bytes(*c)
                    };
                    self.char(char::from_u32(v).unwrap_or('\u{fffd}'));
                }
                self.carry = rest[whole..].to_vec();
            }
            _ => self.feed_bytes(rest),
        }
    }

    /// UTF-8 (checked as it goes) or 8-bit text.
    fn feed_bytes(&mut self, mut rest: &[u8]) {
        if !rest.is_ascii() {
            self.non_ascii = true;
        }
        while !rest.is_empty() {
            if self.invalid_utf8 || self.enc == Encoding::Ansi {
                for &b in rest {
                    // 8-bit text: ASCII spaces separate words; other bytes are letters.
                    self.char(if b < 0x80 { char::from(b) } else { 'x' });
                }
                return;
            }
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    s.chars().for_each(|c| self.char(c));
                    return;
                }
                Err(e) => {
                    let (good, bad) = rest.split_at(e.valid_up_to());
                    if let Ok(s) = std::str::from_utf8(good) {
                        s.chars().for_each(|c| self.char(c));
                    }
                    match e.error_len() {
                        None => {
                            // A character cut by the chunk's end: keep it for the next one.
                            self.carry = bad.to_vec();
                            return;
                        }
                        Some(_) => {
                            self.invalid_utf8 = true;
                            rest = bad;
                        }
                    }
                }
            }
        }
    }

    fn finish(mut self) -> (u64, u64) {
        if !self.carry.is_empty() {
            // Trailing partial character (or odd byte): it is still part of the last line.
            let carry = std::mem::take(&mut self.carry);
            self.invalid_utf8 = true;
            for _ in carry {
                self.char('x');
            }
        }
        let lines = self.lines + u64::from(self.any && !self.at_line_start);
        (lines, self.words)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(b: &[u8]) -> TextStats {
        stats(b, u64::MAX, &Cancel::default()).unwrap()
    }

    #[test]
    fn counts_lines_and_words() {
        let s = run(b"one two\nthree\n");
        assert_eq!(
            (s.lines, s.words, s.encoding),
            (Some(2), Some(3), Encoding::Ascii)
        );
        let s = run(b"one two\nthree");
        assert_eq!((s.lines, s.words), (Some(2), Some(3)));
        let s = run(b"");
        assert_eq!((s.lines, s.words), (Some(0), Some(0)));
        let s = run(b"\n\n\n");
        assert_eq!((s.lines, s.words), (Some(3), Some(0)));
        let s = run(b"a\r\nb\r\n");
        assert_eq!((s.lines, s.words), (Some(2), Some(2)));
    }

    #[test]
    fn encodings() {
        assert_eq!(
            run("caf\u{e9} cr\u{e8}me\n".as_bytes()).encoding,
            Encoding::Utf8
        );
        let s = run(b"\xef\xbb\xbfhello world");
        assert_eq!(
            (s.encoding, s.words, s.lines),
            (Encoding::Utf8Bom, Some(2), Some(1))
        );
        assert_eq!(run(b"caf\xe9 au lait\n").encoding, Encoding::Ansi);
        // A multi-byte character cut by the end of the file.
        assert_eq!(run(b"caf\xc3").encoding, Encoding::Ansi);
        assert_eq!(run(b"a\0b\0c\x01\x02\0\0").encoding, Encoding::Binary);
        assert_eq!(run(b"a\0b\0c\x01\x02\0\0").lines, None);

        let utf16: Vec<u8> = "\u{feff}two words\nline2\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let s = run(&utf16);
        assert_eq!(
            (s.encoding, s.lines, s.words),
            (Encoding::Utf16Le, Some(2), Some(3))
        );

        let be: Vec<u8> = "no bom here\nok"
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect();
        let s = run(&be);
        assert_eq!(
            (s.encoding, s.lines, s.words),
            (Encoding::Utf16Be, Some(2), Some(4))
        );
        let le: Vec<u8> = "no bom here"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(run(&le).encoding, Encoding::Utf16Le);
    }

    #[test]
    fn utf8_split_across_chunks_and_late_invalid_bytes() {
        // A long ASCII run pushes a multi-byte character across the sniff and chunk edges.
        let mut b = vec![b'a'; SNIFF_LEN - 2];
        b.extend_from_slice(" \u{e9}t\u{e9} fin\n".as_bytes());
        let s = run(&b);
        assert_eq!(
            (s.encoding, s.words, s.lines),
            (Encoding::Utf8, Some(3), Some(1))
        );
        // Invalid UTF-8 after the sniffed part makes it ANSI.
        let mut b = vec![b'a'; SNIFF_LEN + 10];
        b.extend_from_slice(b" \xe9t\xe9\n");
        let s = run(&b);
        assert_eq!((s.encoding, s.words), (Encoding::Ansi, Some(2)));
    }

    #[test]
    fn cap_and_cancel() {
        let b = vec![b'x'; 1000];
        let s = stats(&b[..], 100, &Cancel::default()).unwrap();
        assert!(s.truncated);
        assert_eq!(s.lines, None);
        assert_eq!(s.encoding, Encoding::Ascii);
        let big = vec![b'y'; SNIFF_LEN + CHUNK];
        let c = Cancel::default();
        c.cancel();
        assert!(matches!(
            stats(&big[..], u64::MAX, &c),
            Err(Error::Cancelled)
        ));
    }
}
