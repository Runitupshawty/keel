//! A bounded CSV record reader (RFC 4180 quoting, lenient about stray quotes) and the
//! field writer, for EFU, CSV and TSV file lists.

use std::io::{BufRead, Read};

use anyhow::bail;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// At the start of a field.
    Start,
    Unquoted,
    Quoted,
    /// A `"` inside a quoted field: the end of the field, or the first of `""`.
    QuoteSeen,
    /// Text after a quoted field's closing quote (kept, as spreadsheets do).
    After,
}

/// Reads records one at a time from a byte stream. A record is at most `max_line` bytes
/// (quoted line breaks included); a longer one is an error, so a hostile file cannot make
/// it buffer without end.
pub struct Records<R> {
    r: R,
    delim: u8,
    max_line: usize,
    /// Physical line number of the last line read (1-based), for error messages.
    pub line: usize,
    buf: Vec<u8>,
}

/// One physical line, without its `\n` / `\r\n`; None at the end. Errors when the line is
/// longer than `max` bytes. `line` counts lines; the first one loses a UTF-8 BOM.
pub fn read_line(
    r: &mut impl BufRead,
    buf: &mut Vec<u8>,
    max: usize,
    line: &mut usize,
) -> anyhow::Result<Option<usize>> {
    buf.clear();
    let n = Read::take(&mut *r, max as u64 + 2).read_until(b'\n', buf)?;
    if n == 0 {
        return Ok(None);
    }
    *line += 1;
    let mut end = buf.len();
    if buf[..end].ends_with(b"\n") {
        end -= 1;
        if buf[..end].ends_with(b"\r") {
            end -= 1;
        }
    } else if buf.len() > max {
        bail!("line {} is longer than {max} bytes", *line);
    }
    if end > max {
        bail!("line {} is longer than {max} bytes", *line);
    }
    if *line == 1 && buf.starts_with(b"\xEF\xBB\xBF") {
        buf.drain(..3);
        end -= 3;
    }
    Ok(Some(end))
}

impl<R: BufRead> Records<R> {
    pub fn new(r: R, delim: u8, max_line: usize) -> Self {
        Self {
            r,
            delim,
            max_line,
            line: 0,
            buf: Vec::new(),
        }
    }

    /// The next record's fields (lossy UTF-8), None at the end of the input.
    pub fn next_record(&mut self) -> anyhow::Result<Option<Vec<String>>> {
        let mut fields = Vec::new();
        let mut cur = Vec::new();
        let mut state = State::Start;
        let mut total = 0usize;
        let first_line = self.line + 1;
        loop {
            let budget = self.max_line.saturating_sub(total);
            let read = read_line(&mut self.r, &mut self.buf, budget, &mut self.line);
            let read = read.map_err(|e| match total {
                0 => e,
                _ => anyhow::anyhow!(
                    "the record from line {first_line} is longer than {} bytes",
                    self.max_line
                ),
            })?;
            let Some(end) = read else {
                if total == 0 {
                    return Ok(None);
                }
                // The input ended inside a quoted field: take what there is.
                fields.push(String::from_utf8_lossy(&cur).into_owned());
                return Ok(Some(fields));
            };
            total += self.buf.len();
            for &b in &self.buf[..end] {
                state = match (state, b) {
                    (State::Start, b'"') => State::Quoted,
                    (State::Quoted, b'"') => State::QuoteSeen,
                    (State::QuoteSeen, b'"') => {
                        cur.push(b'"');
                        State::Quoted
                    }
                    (State::Quoted, b) => {
                        cur.push(b);
                        State::Quoted
                    }
                    (_, b) if b == self.delim => {
                        fields.push(String::from_utf8_lossy(&cur).into_owned());
                        cur.clear();
                        State::Start
                    }
                    (State::Start | State::Unquoted, b) => {
                        cur.push(b);
                        State::Unquoted
                    }
                    (State::QuoteSeen | State::After, b) => {
                        cur.push(b);
                        State::After
                    }
                };
            }
            if state == State::Quoted {
                // A line break inside quotes belongs to the field.
                cur.extend_from_slice(&self.buf[end..]);
                continue;
            }
            fields.push(String::from_utf8_lossy(&cur).into_owned());
            return Ok(Some(fields));
        }
    }
}

/// Appends `field` to `out`, quoted (`"` doubled) when `always` or when it holds the
/// delimiter, a quote, a line break, or starts or ends with a space.
pub fn push_field(out: &mut String, field: &str, delim: char, always: bool) {
    let needs = always
        || field.contains([delim, '"', '\r', '\n'])
        || field.starts_with(' ')
        || field.ends_with(' ');
    if needs {
        out.push('"');
        out.push_str(&field.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(field);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(text: &str, max: usize) -> anyhow::Result<Vec<Vec<String>>> {
        let mut r = Records::new(text.as_bytes(), b',', max);
        let mut out = Vec::new();
        while let Some(rec) = r.next_record()? {
            out.push(rec);
        }
        Ok(out)
    }

    #[test]
    fn quoting_rules() {
        let got = records(
            "\u{feff}Filename,Size\r\n\"C:\\a, b\\\"\"q\"\".txt\",5\r\nplain,\r\n\"two\r\nlines\",1\n,\n\"open",
            1024,
        )
        .unwrap();
        assert_eq!(
            got,
            [
                vec!["Filename", "Size"],
                vec!["C:\\a, b\\\"q\".txt", "5"],
                vec!["plain", ""],
                vec!["two\r\nlines", "1"],
                vec!["", ""],
                vec!["open"],
            ]
        );
        // Stray quotes are text, not the start of a field.
        assert_eq!(
            records("ab\"c,\"d\"e\n", 64).unwrap(),
            [vec!["ab\"c", "de"]]
        );
        assert_eq!(records("", 64).unwrap(), Vec::<Vec<String>>::new());
        assert_eq!(records("\n", 64).unwrap(), [vec![""]]);
    }

    #[test]
    fn long_lines_and_records_are_refused() {
        assert!(records("short\nthis line is too long\n", 10).is_err());
        assert!(records("0123456789\n", 10).is_ok());
        assert!(records("0123456789", 10).is_ok());
        assert!(records("01234567890", 10).is_err());
        let err = records("\"a\nb\nc\nd\ne\nf\ng\n", 8)
            .unwrap_err()
            .to_string();
        assert!(err.contains("record from line 1"), "{err}");
    }

    #[test]
    fn fields_are_quoted_when_needed() {
        let mut out = String::new();
        for (f, always) in [
            ("a", false),
            ("a,b", false),
            ("q\"", false),
            (" x", false),
            ("p", true),
        ] {
            push_field(&mut out, f, ',', always);
            out.push('|');
        }
        assert_eq!(out, "a|\"a,b\"|\"q\"\"\"|\" x\"|\"p\"|");
        let mut tsv = String::new();
        push_field(&mut tsv, "a,b", '\t', false);
        push_field(&mut tsv, "a\tb", '\t', false);
        assert_eq!(tsv, "a,b\"a\tb\"");
    }
}
