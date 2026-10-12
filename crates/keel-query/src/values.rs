//! Values of function terms: comparison operators, numbers, sizes with units and
//! constants, ranges (`a..b`, `a-b`) and the tests built from them.

/// A comparison operator in front of a value (`size:>=1mb`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CmpOp {
    /// `=`, `==` (or none): within the value's granularity.
    Eq,
    /// `!=`, `!`.
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    /// Reads an operator at the start of `s`: `<= < >= > == = != !`.
    pub fn split(s: &str) -> (Option<CmpOp>, &str) {
        for (p, op) in [
            ("<=", CmpOp::Le),
            (">=", CmpOp::Ge),
            ("==", CmpOp::Eq),
            ("!=", CmpOp::Ne),
            ("<", CmpOp::Lt),
            (">", CmpOp::Gt),
            ("=", CmpOp::Eq),
            ("!", CmpOp::Ne),
        ] {
            if let Some(rest) = s.strip_prefix(p) {
                return (Some(op), rest);
            }
        }
        (None, s)
    }
}

/// A value as two spans: `eq` for `=`/`!=` (the value's granularity: `1kb` is
/// 1024..2047 bytes, `2024` is the whole year) and `cmp` for `<`/`>` (`size:>1mb` is more
/// than 1,048,576 bytes; `dm:>2024` is after 2024).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct NumVal {
    pub eq: (f64, f64),
    pub cmp: (f64, f64),
}

impl NumVal {
    pub fn point(v: f64) -> Self {
        Self::span(v, v)
    }
    pub fn span(lo: f64, hi: f64) -> Self {
        Self {
            eq: (lo, hi),
            cmp: (lo, hi),
        }
    }
}

/// A test on a number (or a date as milliseconds).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum NumTest {
    /// `unknown`.
    Unknown,
    /// `!unknown`.
    Known,
    /// No value: any non-zero number.
    NonZero,
    Cmp {
        op: CmpOp,
        lo: f64,
        hi: f64,
    },
}

impl NumTest {
    pub fn single(op: Option<CmpOp>, v: NumVal) -> Self {
        let op = op.unwrap_or(CmpOp::Eq);
        let (lo, hi) = match op {
            CmpOp::Eq | CmpOp::Ne => v.eq,
            _ => v.cmp,
        };
        NumTest::Cmp { op, lo, hi }
    }

    pub fn range(
        op: Option<CmpOp>,
        lo: Option<NumVal>,
        hi: Option<NumVal>,
    ) -> Result<Self, String> {
        let op = match op {
            None | Some(CmpOp::Eq) => CmpOp::Eq,
            Some(CmpOp::Ne) => CmpOp::Ne,
            Some(_) => return Err("a range takes no `<` or `>`".into()),
        };
        Ok(NumTest::Cmp {
            op,
            lo: lo.map_or(f64::NEG_INFINITY, |v| v.eq.0),
            hi: hi.map_or(f64::INFINITY, |v| v.eq.1),
        })
    }

    pub fn eval(&self, v: Option<f64>) -> bool {
        match *self {
            NumTest::Unknown => v.is_none(),
            NumTest::Known => v.is_some(),
            NumTest::NonZero => v.is_some_and(|x| x != 0.0),
            NumTest::Cmp { op, lo, hi } => {
                let Some(x) = v else { return false };
                match op {
                    CmpOp::Eq => (lo..=hi).contains(&x),
                    CmpOp::Ne => !(lo..=hi).contains(&x),
                    CmpOp::Lt => x < lo,
                    CmpOp::Le => x <= hi,
                    CmpOp::Gt => x > hi,
                    CmpOp::Ge => x >= lo,
                }
            }
        }
    }
}

/// One value, or a range with optional ends.
#[derive(Debug)]
pub(crate) enum Parsed<T> {
    One(T),
    Range(Option<T>, Option<T>),
}

/// `a..b`, `..b`, `a..`; else the whole text as one value; else `a-b` split at the first
/// `-` that leaves two values (so `2024-05` stays a month and `2020-2021` is a range).
pub(crate) fn split_range<T>(
    s: &str,
    parse: &dyn Fn(&str) -> Result<T, String>,
) -> Result<Parsed<T>, String> {
    if let Some((a, b)) = s.split_once("..") {
        let a = a.trim();
        let b = b.trim();
        if a.is_empty() && b.is_empty() {
            return Err("a range needs a start or an end".into());
        }
        let lo = if a.is_empty() { None } else { Some(parse(a)?) };
        let hi = if b.is_empty() { None } else { Some(parse(b)?) };
        return Ok(Parsed::Range(lo, hi));
    }
    let whole = match parse(s) {
        Ok(v) => return Ok(Parsed::One(v)),
        Err(e) => e,
    };
    for (i, _) in s.match_indices('-').filter(|(i, _)| *i > 0) {
        if let (Ok(a), Ok(b)) = (parse(&s[..i]), parse(&s[i + 1..])) {
            return Ok(Parsed::Range(Some(a), Some(b)));
        }
    }
    Err(whole)
}

/// A plain number: digits with an optional fraction and sign, or `0x` hex.
pub(crate) fn parse_plain(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u64::from_str_radix(h, 16).ok().map(|v| v as f64);
    }
    let digits = s.strip_prefix('-').unwrap_or(s);
    let ok = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && digits.bytes().filter(|&b| b == b'.').count() <= 1
        && digits.bytes().any(|b| b.is_ascii_digit());
    if ok {
        s.parse().ok()
    } else {
        None
    }
}

/// A count or length value.
pub(crate) fn parse_number(s: &str) -> Result<NumVal, String> {
    parse_plain(s)
        .map(NumVal::point)
        .ok_or_else(|| format!("`{s}` is not a number"))
}

/// Size constants: Everything's bounds in KB (`tiny` is 1 byte..10 KB).
pub(crate) fn size_constant(s: &str, k: f64) -> Option<NumVal> {
    let span = |lo: f64, hi: f64| Some(NumVal::span(lo, hi));
    match s {
        "empty" => span(0.0, 0.0),
        "tiny" => span(1.0, 10.0 * k),
        "small" => span(10.0 * k + 1.0, 100.0 * k),
        "medium" => span(100.0 * k + 1.0, k * k),
        "large" => span(k * k + 1.0, 16.0 * k * k),
        "huge" => span(16.0 * k * k + 1.0, 128.0 * k * k),
        "gigantic" => span(128.0 * k * k + 1.0, f64::INFINITY),
        _ => None,
    }
}

/// A size: bytes, a number with a unit (`b kb mb gb tb pb`, `k m g t p`, `kib`..`pib`), or
/// a constant. `metric` makes `kb` 1,000 bytes (`kib` is always 1,024).
pub(crate) fn parse_size(s: &str, metric: bool) -> Result<NumVal, String> {
    let k = if metric { 1000.0 } else { 1024.0 };
    let t = s.trim().to_ascii_lowercase();
    if let Some(v) = size_constant(&t, k) {
        return Ok(v);
    }
    if t.starts_with("0x") {
        return parse_number(&t);
    }
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let unit = unit.trim();
    let bad = || format!("`{s}` is not a size (try 10mb, 1.5gb, >100kb or small)");
    let n = parse_plain(num).ok_or_else(bad)?;
    let mult = match unit {
        "" | "b" | "byte" | "bytes" => 1.0,
        "k" | "kb" => k,
        "m" | "mb" => k * k,
        "g" | "gb" => k * k * k,
        "t" | "tb" => k * k * k * k,
        "p" | "pb" => k * k * k * k * k,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        "pib" => 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return Err(bad()),
    };
    let v = n * mult;
    if mult > 1.0 && !num.contains('.') {
        Ok(NumVal {
            eq: (v, v + mult - 1.0),
            cmp: (v, v),
        })
    } else {
        Ok(NumVal::point(v))
    }
}

/// How `to-number:` and friends read text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NumParse {
    Integer,
    Real,
    Hex,
}

/// The number in `s` (the whole text, else the part before the last `.`), or `None`.
pub(crate) fn number_from_text(s: &str, how: NumParse) -> Option<f64> {
    let one = |t: &str| -> Option<f64> {
        let t = t.trim();
        match how {
            NumParse::Hex => {
                let h = t
                    .strip_prefix("0x")
                    .or_else(|| t.strip_prefix("0X"))
                    .unwrap_or(t);
                u64::from_str_radix(h, 16).ok().map(|v| v as f64)
            }
            NumParse::Integer => {
                let d = t.strip_prefix('-').unwrap_or(t);
                if !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()) {
                    t.parse::<f64>().ok()
                } else {
                    None
                }
            }
            NumParse::Real => parse_plain(t).filter(|_| !t.starts_with("0x")),
        }
    };
    one(s).or_else(|| s.rsplit_once('.').and_then(|(stem, _)| one(stem)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1kb", false).unwrap().eq, (1024.0, 2047.0));
        assert_eq!(parse_size("1kb", false).unwrap().cmp, (1024.0, 1024.0));
        assert_eq!(parse_size("1.5 KB", false).unwrap().eq, (1536.0, 1536.0));
        assert_eq!(parse_size("1kb", true).unwrap().cmp.0, 1000.0);
        assert_eq!(parse_size("1kib", true).unwrap().cmp.0, 1024.0);
        assert_eq!(parse_size("tiny", false).unwrap().eq, (1.0, 10240.0));
        assert!(parse_size("1zb", false).is_err());
        assert!(parse_size("1e999", false).is_err());
        assert!(parse_size("", false).is_err());
    }

    #[test]
    fn ranges() {
        let p = |s: &str| parse_number(s);
        assert!(matches!(
            split_range("1..5", &p),
            Ok(Parsed::Range(Some(_), Some(_)))
        ));
        assert!(matches!(
            split_range("..5", &p),
            Ok(Parsed::Range(None, Some(_)))
        ));
        assert!(matches!(
            split_range("1-5", &p),
            Ok(Parsed::Range(Some(_), Some(_)))
        ));
        assert!(matches!(split_range("-5", &p), Ok(Parsed::One(_))));
        assert!(split_range("..", &p).is_err());
    }

    #[test]
    fn tests() {
        let t = NumTest::single(Some(CmpOp::Gt), parse_size("1mb", false).unwrap());
        assert!(t.eval(Some(1_048_577.0)) && !t.eval(Some(1_048_576.0)) && !t.eval(None));
        let t = NumTest::single(None, parse_size("1kb", false).unwrap());
        assert!(t.eval(Some(2047.0)) && !t.eval(Some(2048.0)));
        assert!(NumTest::Unknown.eval(None));
    }

    #[test]
    fn text_numbers() {
        assert_eq!(number_from_text("123.txt", NumParse::Integer), Some(123.0));
        assert_eq!(number_from_text("1.5", NumParse::Real), Some(1.5));
        assert_eq!(number_from_text("ff", NumParse::Hex), Some(255.0));
        assert_eq!(number_from_text("abc", NumParse::Integer), None);
    }
}
