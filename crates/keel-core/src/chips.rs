//! Filter chips for library search (Task 57). The query string stays the truth: a chip
//! stands for one term of the library query syntax (`LibraryQuery::parse`), [`toggle`]
//! adds or removes that term in the text and [`is_on`] reads whether the text has it, so
//! a term typed by hand lights its chip and terms no chip shows are kept as typed.
//!
//! Plain `std`: keel-web shares this file by path (its search goes through the same
//! parser, in keel-daemon).

use std::collections::HashMap;

/// A row of chips.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Group {
    Kind,
    Type,
    Tag,
    Source,
    Size,
    Modified,
    Favorites,
    /// Task 62: the Contents chip (`contents:off` while it is off).
    Contents,
}

impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Group::Kind => "Kind",
            Group::Type => "Type",
            Group::Tag => "Tag",
            Group::Source => "Source",
            Group::Size => "Size",
            Group::Modified => "Modified",
            Group::Favorites => "Favorites",
            Group::Contents => "Contents",
        }
    }

    /// The parser keeps only the last such term: one chip of the group at a time.
    fn exclusive(self) -> bool {
        matches!(
            self,
            Group::Kind | Group::Size | Group::Modified | Group::Contents
        )
    }
}

/// One chip: its group, what it shows and the query term it stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chip {
    pub group: Group,
    pub label: String,
    pub term: String,
}

/// The words `kind:` (and `type:`) take, by the kind they name. `kind:photo` is a media
/// filter of its own, not one of these.
pub const KIND_WORDS: [(&str, &[&str]); 7] = [
    ("file", &["file", "files"]),
    ("folder", &["folder", "folders", "dir"]),
    ("image", &["image", "images", "pic", "picture"]),
    ("video", &["video", "videos"]),
    ("audio", &["audio", "music"]),
    ("document", &["doc", "docs", "document", "documents"]),
    ("archive", &["archive", "archives", "zip"]),
];

/// The kind a `kind:` word names (any case), as its first name in [`KIND_WORDS`].
pub fn kind_word(s: &str) -> Option<&'static str> {
    let s = s.to_ascii_lowercase();
    KIND_WORDS
        .iter()
        .find(|(_, words)| words.contains(&s.as_str()))
        .map(|(kind, _)| *kind)
}

/// The Kind chips: (label, `kind:` word).
pub const KINDS: [(&str, &str); 6] = [
    ("Image", "image"),
    ("Video", "video"),
    ("Audio", "audio"),
    ("Document", "document"),
    ("Archive", "archive"),
    ("Folder", "folder"),
];

/// The Size chips: (label, `size:` value).
pub const SIZES: [(&str, &str); 4] = [
    ("Under 1 MB", "<1mb"),
    ("1\u{2013}100 MB", "1mb..100mb"),
    ("Over 100 MB", ">100mb"),
    ("Over 1 GB", ">1gb"),
];

/// The Modified chips: (label, `dm:` value).
pub const MODIFIED: [(&str, &str); 4] = [
    ("Today", "today"),
    ("This week", "thisweek"),
    ("This month", "thismonth"),
    ("This year", "thisyear"),
];

/// The reserved tag Favorites is.
const FAVORITES: &str = "Favorites";

/// Splits on whitespace outside double quotes (the quotes stay in the tokens).
pub fn tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `ext:` values as the parser reads them: lowercase, without the dot.
fn exts(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split([';', ','])
        .map(|e| e.trim().trim_start_matches('.').to_lowercase())
        .filter(|e| !e.is_empty())
}

/// A term's group and value (unquoted; a Kind's first name), or None for words, phrases
/// and terms no chip shows.
fn classify(tok: &str) -> Option<(Group, String)> {
    if tok.starts_with('"') {
        return None;
    }
    let (key, value) = tok.split_once(':')?;
    let value = value.replace('"', "");
    let key = key.to_ascii_lowercase();
    let group = match key.as_str() {
        "kind" | "type" => return kind_word(&value).map(|k| (Group::Kind, k.to_owned())),
        "file" | "folder" if value.is_empty() => return Some((Group::Kind, key)),
        "ext" => Group::Type,
        "size" => Group::Size,
        "dm" | "datemodified" | "modified" => Group::Modified,
        "tag" if value.eq_ignore_ascii_case(FAVORITES) => Group::Favorites,
        "tag" if !value.is_empty() => Group::Tag,
        "in" | "source" if !value.is_empty() => Group::Source,
        "contents" => {
            let v = match value.to_ascii_lowercase().as_str() {
                "off" | "no" | "false" | "0" => "off",
                "on" | "yes" | "true" | "1" => "on",
                _ => return None,
            };
            return Some((Group::Contents, v.to_owned()));
        }
        _ => return None,
    };
    Some((group, value))
}

/// A value in the query stands for the chip's value.
fn same(group: Group, chip: &str, value: &str) -> bool {
    match group {
        Group::Kind => chip == value,
        Group::Type => exts(value).any(|e| e == chip),
        _ => chip.eq_ignore_ascii_case(value),
    }
}

/// A value the parser reads back as typed: no quote, not empty, quoted when it has
/// spaces.
fn quoted(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.contains('"') {
        return None;
    }
    Some(if value.contains(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value.to_owned()
    })
}

impl Chip {
    fn new(group: Group, label: &str, term: String) -> Chip {
        Chip {
            group,
            label: label.to_owned(),
            term,
        }
    }

    pub fn kinds() -> Vec<Chip> {
        let chip = |(label, word)| Chip::new(Group::Kind, label, format!("kind:{word}"));
        KINDS.into_iter().map(chip).collect()
    }

    pub fn sizes() -> Vec<Chip> {
        let chip = |(label, v)| Chip::new(Group::Size, label, format!("size:{v}"));
        SIZES.into_iter().map(chip).collect()
    }

    pub fn modified() -> Vec<Chip> {
        let chip = |(label, v)| Chip::new(Group::Modified, label, format!("dm:{v}"));
        MODIFIED.into_iter().map(chip).collect()
    }

    pub fn favorites() -> Chip {
        Chip::new(
            Group::Favorites,
            "\u{2605} Favorites",
            format!("tag:{FAVORITES}"),
        )
    }

    /// The Contents chip (Task 62). Its term is `contents:off`, so it reads the other way
    /// round: the row shows it on while the query does not have the term ([`contents_on`]).
    pub fn contents() -> Chip {
        Chip::new(Group::Contents, "Contents", "contents:off".to_owned())
    }

    /// `ext:<e>` (lowercase, without a dot); None when the parser could not read it back.
    pub fn ext(e: &str) -> Option<Chip> {
        let e = e.trim().trim_start_matches('.').to_lowercase();
        let bad = |c: char| c.is_whitespace() || matches!(c, '"' | ';' | ',');
        (!e.is_empty() && !e.contains(bad)).then(|| Chip::new(Group::Type, &e, format!("ext:{e}")))
    }

    /// `tag:<name>`; None when the parser could not read it back (a quote in it).
    pub fn tag(name: &str) -> Option<Chip> {
        if name.trim().eq_ignore_ascii_case(FAVORITES) {
            return Some(Chip::favorites());
        }
        let v = quoted(name)?;
        Some(Chip::new(Group::Tag, name.trim(), format!("tag:{v}")))
    }

    /// `in:<source label>`; None when the parser could not read it back.
    pub fn source(label: &str) -> Option<Chip> {
        let v = quoted(label)?;
        Some(Chip::new(Group::Source, label.trim(), format!("in:{v}")))
    }

    /// The chip's group and value, as a term of the query would have them.
    fn value(&self) -> (Group, String) {
        let (group, value) = classify(&self.term).expect("a chip's term is a chip term");
        let value = match group {
            Group::Type => exts(&value).next().unwrap_or_default(),
            _ => value,
        };
        (group, value)
    }

    /// What a screen reader says: the group and the label.
    pub fn name(&self) -> String {
        match self.group {
            Group::Favorites => "Only favorites".to_owned(),
            Group::Contents => "Search file contents".to_owned(),
            g => format!("{}: {}", g.label(), self.label),
        }
    }
}

/// Whether words in `query` also match the text inside documents: no `contents:off` (or a
/// later `contents:on`).
pub fn contents_on(query: &str) -> bool {
    !is_on(query, &Chip::contents())
}

/// The query has the chip's term (any case, any spelling of its key), and for Kind, Size
/// and Modified it is the last one of its group (the one the parser keeps).
pub fn is_on(query: &str, chip: &Chip) -> bool {
    let (group, want) = chip.value();
    let mut mine = tokens(query)
        .into_iter()
        .filter_map(|t| classify(&t))
        .filter(|(g, _)| *g == group);
    if group.exclusive() {
        mine.next_back()
            .is_some_and(|(_, v)| same(group, &want, &v))
    } else {
        mine.any(|(_, v)| same(group, &want, &v))
    }
}

/// The query with the chip's term taken out (on) or put in (off: replacing the group's
/// term for Kind, Size and Modified). Every other term stays as typed, also an earlier
/// term of the chip's group typed by hand when the chip goes off.
pub fn toggle(query: &str, chip: &Chip) -> String {
    let on = is_on(query, chip);
    let (group, want) = chip.value();
    let mut out: Vec<String> = Vec::new();
    for tok in tokens(query) {
        match classify(&tok) {
            // On: the group's terms make way for the chip's; off: its own value goes.
            Some((g, v)) if g == group && group.exclusive() => {
                if on && !same(group, &want, &v) {
                    out.push(tok);
                }
            }
            Some((Group::Type, v)) if group == Group::Type => {
                let all: Vec<String> = exts(&v).collect();
                let rest: Vec<&str> = (all.iter().map(String::as_str))
                    .filter(|e| *e != want)
                    .collect();
                if rest.len() == all.len() {
                    out.push(tok);
                } else if !rest.is_empty() {
                    let rest = rest.join(";");
                    out.push(match rest.contains(char::is_whitespace) {
                        true => format!("ext:\"{rest}\""),
                        false => format!("ext:{rest}"),
                    });
                }
            }
            Some((g, v)) if g == group && same(group, &want, &v) => {}
            _ => out.push(tok),
        }
    }
    if !on {
        // A quote left open would take the new term in.
        if let Some(last) = out.last_mut().filter(|t| t.matches('"').count() % 2 == 1) {
            last.push('"');
        }
        out.push(chip.term.clone());
    }
    out.join(" ")
}

/// The values of a group's terms in the query, in order, without repeats (for chips the
/// row does not offer on its own: a tag or extension typed by hand).
pub fn values(query: &str, group: Group) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (g, v) in tokens(query).iter().filter_map(|t| classify(t)) {
        if g != group {
            continue;
        }
        let vs: Vec<String> = match g {
            Group::Type => exts(&v).collect(),
            _ => vec![v],
        };
        for v in vs {
            if !out.iter().any(|o| o.eq_ignore_ascii_case(&v)) {
                out.push(v);
            }
        }
    }
    out
}

/// The `n` most common extensions of these file names or paths (lowercase, most first,
/// ties by name), for the Type chips.
pub fn top_extensions<'a>(names: impl IntoIterator<Item = &'a str>, n: usize) -> Vec<String> {
    let mut count: HashMap<String, usize> = HashMap::new();
    for path in names {
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let Some((stem, ext)) = name.rsplit_once('.') else {
            continue;
        };
        let plain = (1..=8).contains(&ext.len()) && ext.chars().all(char::is_alphanumeric);
        if !stem.is_empty() && plain {
            *count.entry(ext.to_lowercase()).or_default() += 1;
        }
    }
    let mut all: Vec<(String, usize)> = count.into_iter().collect();
    all.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    all.into_iter().take(n).map(|(e, _)| e).collect()
}
