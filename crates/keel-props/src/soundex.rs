//! `soundex:` matching: American Soundex (the U.S. National Archives rules), so `robert`
//! finds `Rupert` and `ashcraft` finds `Ashcroft`.
//!
//! A word's code is its first letter and three digits: `b f p v` = 1, `c g j k q s x z` = 2,
//! `d t` = 3, `l` = 4, `m n` = 5, `r` = 6; vowels (and `y`) are dropped but separate equal
//! digits, `h` and `w` are dropped and do not; a digit equal to the first letter's is
//! dropped; the code is cut or padded with zeros to four characters. Latin letters with
//! diacritics count as their base letter (`é` as `e`); other characters separate words.
//!
//! Matching ([`matches`]) compares words: every word of the pattern must have the code of
//! some word of the text (a file name: `report_robert-2024.txt` has the words `report`,
//! `robert` and `txt`).

/// The American Soundex code of `word` (`R163`), or `None` when it has no Latin letter.
pub fn code(word: &str) -> Option<String> {
    let mut letters = word.chars().filter_map(fold);
    let first = letters.next()?;
    let mut out = String::with_capacity(4);
    out.push(first.to_ascii_uppercase());
    let mut last = digit(first);
    for c in letters {
        let d = digit(c);
        if d != 0 && d != last {
            out.push(char::from(b'0' + d));
            if out.len() == 4 {
                break;
            }
        }
        if !matches!(c, 'h' | 'w') {
            last = d;
        }
    }
    while out.len() < 4 {
        out.push('0');
    }
    Some(out)
}

/// True when every word of `pattern` sounds like some word of `text` (and the pattern has
/// a word).
pub fn matches(text: &str, pattern: &str) -> bool {
    let codes: Vec<String> = words(text).filter_map(code).collect();
    let mut any = false;
    for w in words(pattern) {
        let Some(c) = code(w) else { continue };
        any = true;
        if !codes.contains(&c) {
            return false;
        }
    }
    any
}

/// Runs of letters (digits and punctuation separate words).
fn words(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| fold(c).is_none())
        .filter(|w| !w.is_empty())
}

fn digit(c: char) -> u8 {
    match c {
        'b' | 'f' | 'p' | 'v' => 1,
        'c' | 'g' | 'j' | 'k' | 'q' | 's' | 'x' | 'z' => 2,
        'd' | 't' => 3,
        'l' => 4,
        'm' | 'n' => 5,
        'r' => 6,
        _ => 0,
    }
}

/// A Latin letter as a lowercase ASCII letter (diacritics dropped); `None` for anything
/// else.
fn fold(c: char) -> Option<char> {
    if c.is_ascii_alphabetic() {
        return Some(c.to_ascii_lowercase());
    }
    let base = match c.to_lowercase().next()? {
        '\u{e0}'..='\u{e5}' | '\u{101}' | '\u{103}' | '\u{105}' => 'a',
        '\u{e7}' | '\u{107}' | '\u{109}' | '\u{10b}' | '\u{10d}' => 'c',
        '\u{10f}' | '\u{111}' => 'd',
        '\u{e8}'..='\u{eb}' | '\u{113}' | '\u{115}' | '\u{117}' | '\u{119}' | '\u{11b}' => 'e',
        '\u{11d}' | '\u{11f}' | '\u{121}' | '\u{123}' => 'g',
        '\u{125}' | '\u{127}' => 'h',
        '\u{ec}'..='\u{ef}' | '\u{129}' | '\u{12b}' | '\u{12d}' | '\u{12f}' | '\u{131}' => 'i',
        '\u{135}' => 'j',
        '\u{137}' => 'k',
        '\u{13a}' | '\u{13c}' | '\u{13e}' | '\u{140}' | '\u{142}' => 'l',
        '\u{f1}' | '\u{144}' | '\u{146}' | '\u{148}' => 'n',
        '\u{f2}'..='\u{f6}' | '\u{f8}' | '\u{14d}' | '\u{14f}' | '\u{151}' => 'o',
        '\u{155}' | '\u{157}' | '\u{159}' => 'r',
        '\u{df}' | '\u{15b}' | '\u{15d}' | '\u{15f}' | '\u{161}' => 's',
        '\u{163}' | '\u{165}' | '\u{167}' => 't',
        '\u{f9}'..='\u{fc}'
        | '\u{169}'
        | '\u{16b}'
        | '\u{16d}'
        | '\u{16f}'
        | '\u{171}'
        | '\u{173}' => 'u',
        '\u{175}' => 'w',
        '\u{fd}' | '\u{ff}' | '\u{177}' => 'y',
        '\u{17a}' | '\u{17c}' | '\u{17e}' => 'z',
        _ => return None,
    };
    Some(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn national_archives_examples() {
        for (w, c) in [
            ("Robert", "R163"),
            ("Rupert", "R163"),
            ("Rubin", "R150"),
            ("Ashcraft", "A261"),
            ("Ashcroft", "A261"),
            ("Tymczak", "T522"),
            ("Pfister", "P236"),
            ("Honeyman", "H555"),
            ("Lee", "L000"),
            ("Gutierrez", "G362"),
            ("Jackson", "J250"),
            ("Washington", "W252"),
            ("A", "A000"),
        ] {
            assert_eq!(code(w).as_deref(), Some(c), "{w}");
        }
        assert_eq!(code("123"), None);
        assert_eq!(code(""), None);
        // Diacritics count as their base letter.
        assert_eq!(code("M\u{fc}ller"), code("Muller"));
        assert_eq!(code("\u{c9}mile"), code("Emile"));
    }

    #[test]
    fn word_matching() {
        assert!(matches("report_robert-2024.txt", "rupert"));
        assert!(matches("Ashcroft family.jpg", "ashcraft famly"));
        assert!(!matches("Ashcroft family.jpg", "ashcraft smith"));
        assert!(!matches("anything", ""));
        assert!(!matches("anything", "123"));
        assert!(matches("x", "x"));
    }
}
