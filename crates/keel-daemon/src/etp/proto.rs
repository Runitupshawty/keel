//! ETP command lines and replies: the FTP commands the server answers and Everything's
//! `SITE EVERYTHING` extension (also accepted as a bare `EVERYTHING` command, as
//! Everything does). Pure functions: the session in `super` does the I/O.

use keel_api::types::{SystemSearchParams, SystemSort};

/// Longest command line accepted (without its line break).
pub const MAX_LINE: usize = 4096;
/// Most results one `QUERY` returns (`keel_api::system::MAX_COUNT`).
pub const MAX_COUNT: u32 = keel_api::system::MAX_COUNT as u32;

/// One command line.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    User(String),
    Pass(String),
    Auth(String),
    Pbsz(String),
    Prot(String),
    Syst,
    Feat,
    Help,
    Noop,
    Quit,
    Pwd,
    Cwd(String),
    Cdup,
    Type(String),
    Mode(String),
    Stru(String),
    Opts(String),
    Pasv,
    Epsv(String),
    /// `PORT` / `EPRT`: active mode, refused (no connections out of the server).
    Active,
    List(String),
    Nlst(String),
    Mlsd(String),
    Mlst(String),
    Size(String),
    Mdtm(String),
    Retr(String),
    Rest(String),
    Abor,
    Everything(Everything),
    /// A `SITE` command other than `EVERYTHING` (its first word, uppercase).
    Site(String),
    /// A command that would change something (its verb, uppercase): refused.
    Write(String),
    /// Anything else (its verb, uppercase).
    Unknown(String),
}

/// A `SITE EVERYTHING` command.
#[derive(Clone, Debug, PartialEq)]
pub enum Everything {
    Search(String),
    FilterSearch(String),
    Flag(Flag, bool),
    FilterFlag(Flag, bool),
    Sort(Sort),
    Offset(u32),
    Count(u32),
    Column(Column, bool),
    Query,
    /// An unknown sort name.
    BadSort(String),
    /// An unknown `SITE EVERYTHING` command (uppercase).
    Unknown(String),
}

/// A match option.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flag {
    Case,
    WholeWord,
    Path,
    Diacritics,
    Prefix,
    Suffix,
    IgnorePunctuation,
    IgnoreWhitespace,
    Regex,
    HideEmptySearchResults,
}

impl Flag {
    const ALL: [(&'static str, Flag, &'static str); 10] = [
        ("CASE", Flag::Case, "Case"),
        ("WHOLE_WORD", Flag::WholeWord, "Whole word"),
        ("PATH", Flag::Path, "Path"),
        ("DIACRITICS", Flag::Diacritics, "Diacritics"),
        ("PREFIX", Flag::Prefix, "Prefix"),
        ("SUFFIX", Flag::Suffix, "Suffix"),
        (
            "IGNORE_PUNCTUATION",
            Flag::IgnorePunctuation,
            "Ignore punctuation",
        ),
        (
            "IGNORE_WHITESPACE",
            Flag::IgnoreWhitespace,
            "Ignore whitespace",
        ),
        ("REGEX", Flag::Regex, "Regex"),
        (
            "HIDE_EMPTY_SEARCH_RESULTS",
            Flag::HideEmptySearchResults,
            "Hide empty search results",
        ),
    ];

    fn label(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(_, f, _)| *f == self)
            .map_or("", |(_, _, l)| l)
    }
}

/// A result column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Column {
    Size,
    DateCreated,
    DateModified,
    Attributes,
    Path,
    FileListFilename,
    DateRecentlyChanged,
}

impl Column {
    const ALL: [(&'static str, Column, &'static str); 7] = [
        ("SIZE_COLUMN", Column::Size, "Size column"),
        (
            "DATE_CREATED_COLUMN",
            Column::DateCreated,
            "Date created column",
        ),
        (
            "DATE_MODIFIED_COLUMN",
            Column::DateModified,
            "Date modified column",
        ),
        ("ATTRIBUTES_COLUMN", Column::Attributes, "Attributes column"),
        ("PATH_COLUMN", Column::Path, "Path column"),
        (
            "FILE_LIST_FILENAME_COLUMN",
            Column::FileListFilename,
            "File list filename column",
        ),
        (
            "DATE_RECENTLY_CHANGED_COLUMN",
            Column::DateRecentlyChanged,
            "Date recently changed column",
        ),
    ];
}

/// What results are sorted by: Everything's sort names (`NAME_ASCENDING`, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub ascending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Sort {
            key: SortKey::Name,
            ascending: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Path,
    Size,
    Extension,
    DateCreated,
    DateModified,
    Attributes,
    FileListFilename,
    DateRecentlyChanged,
}

impl Sort {
    /// Everything's sort names, any case (`INVERSE_SIZE_*` is size the other way round).
    pub fn parse(name: &str) -> Option<Sort> {
        let name = name.trim().to_ascii_uppercase();
        let (key, ascending) = if let Some(k) = name.strip_suffix("_ASCENDING") {
            (k, true)
        } else {
            (name.strip_suffix("_DESCENDING")?, false)
        };
        let (key, ascending) = match key {
            "NAME" => (SortKey::Name, ascending),
            "PATH" => (SortKey::Path, ascending),
            "SIZE" => (SortKey::Size, ascending),
            "INVERSE_SIZE" => (SortKey::Size, !ascending),
            "EXTENSION" => (SortKey::Extension, ascending),
            "DATE_CREATED" => (SortKey::DateCreated, ascending),
            "DATE_MODIFIED" => (SortKey::DateModified, ascending),
            "ATTRIBUTES" => (SortKey::Attributes, ascending),
            "FILE_LIST_FILENAME" => (SortKey::FileListFilename, ascending),
            "DATE_RECENTLY_CHANGED" => (SortKey::DateRecentlyChanged, ascending),
            _ => return None,
        };
        Some(Sort { key, ascending })
    }

    /// The system search's sort: name, path, size and date modified are its own; the
    /// others (not indexed) sort by name, date created by date modified.
    pub fn system(self) -> SystemSort {
        match self.key {
            SortKey::Path => SystemSort::Path,
            SortKey::Size => SystemSort::Size,
            SortKey::DateModified | SortKey::DateCreated | SortKey::DateRecentlyChanged => {
                SystemSort::DateModified
            }
            _ => SystemSort::Name,
        }
    }
}

/// Everything's `string_to_int`: optional spaces and sign, then the leading digits (none:
/// 0), saturating.
pub fn int(s: &str) -> i64 {
    let s = s.trim_start();
    let (negative, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut n: i64 = 0;
    for b in digits.bytes().take_while(u8::is_ascii_digit) {
        n = n.saturating_mul(10).saturating_add(i64::from(b - b'0'));
    }
    if negative {
        -n
    } else {
        n
    }
}

/// Everything's `string_to_dword`: [`int`] kept within `u32` (negative: 0).
pub fn dword(s: &str) -> u32 {
    u32::try_from(int(s).max(0)).unwrap_or(u32::MAX)
}

/// The verb's argument: everything after the first space (more spaces are kept: names may
/// start with one).
fn split(line: &str) -> (String, String) {
    let (verb, arg) = line.split_once(' ').unwrap_or((line, ""));
    (verb.to_ascii_uppercase(), arg.to_owned())
}

/// Parses `SITE EVERYTHING <cmd> <arg>`'s `<cmd> <arg>`.
pub fn everything(rest: &str) -> Everything {
    let (cmd, arg) = split(rest.trim_start_matches(' '));
    let on = || int(&arg) != 0;
    if let Some((_, flag, _)) = Flag::ALL.iter().find(|(n, _, _)| *n == cmd) {
        return Everything::Flag(*flag, on());
    }
    if let Some(name) = cmd.strip_prefix("FILTER_") {
        if name == "SEARCH" {
            return Everything::FilterSearch(arg);
        }
        if let Some((_, flag, _)) = Flag::ALL.iter().find(|(n, _, _)| *n == name) {
            return Everything::FilterFlag(*flag, on());
        }
    }
    if let Some((_, col, _)) = Column::ALL.iter().find(|(n, _, _)| *n == cmd) {
        return Everything::Column(*col, on());
    }
    match cmd.as_str() {
        "SEARCH" => Everything::Search(arg),
        "SORT" => match Sort::parse(&arg) {
            Some(s) => Everything::Sort(s),
            None => Everything::BadSort(arg),
        },
        "OFFSET" => Everything::Offset(dword(&arg)),
        "COUNT" => Everything::Count(dword(&arg)),
        "QUERY" => Everything::Query,
        _ => Everything::Unknown(cmd),
    }
}

/// Why a line is not a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bad {
    TooLong,
    Empty,
    Control,
}

/// Parses one command line (its line break already gone).
pub fn parse(line: &str) -> Result<Command, Bad> {
    if line.len() > MAX_LINE {
        return Err(Bad::TooLong);
    }
    // NUL and other control characters never belong in a command (a file name with a line
    // break cannot be named over FTP either).
    if line.chars().any(|c| c.is_control() && c != '\t') {
        return Err(Bad::Control);
    }
    let line = line.trim_start_matches(' ');
    if line.is_empty() {
        return Err(Bad::Empty);
    }
    let (verb, arg) = split(line);
    Ok(match verb.as_str() {
        "USER" => Command::User(arg),
        "PASS" => Command::Pass(arg),
        "AUTH" => Command::Auth(arg),
        "PBSZ" => Command::Pbsz(arg),
        "PROT" => Command::Prot(arg),
        "SYST" => Command::Syst,
        "FEAT" => Command::Feat,
        "HELP" => Command::Help,
        "NOOP" => Command::Noop,
        "QUIT" => Command::Quit,
        "PWD" | "XPWD" => Command::Pwd,
        "CWD" | "XCWD" => Command::Cwd(arg),
        "CDUP" | "XCUP" => Command::Cdup,
        "TYPE" => Command::Type(arg),
        "MODE" => Command::Mode(arg),
        "STRU" => Command::Stru(arg),
        "OPTS" => Command::Opts(arg),
        "PASV" => Command::Pasv,
        "EPSV" => Command::Epsv(arg),
        "PORT" | "EPRT" | "LPRT" => Command::Active,
        "LIST" => Command::List(arg),
        "NLST" => Command::Nlst(arg),
        "MLSD" => Command::Mlsd(arg),
        "MLST" => Command::Mlst(arg),
        "SIZE" => Command::Size(arg),
        "MDTM" => Command::Mdtm(arg),
        "RETR" => Command::Retr(arg),
        "REST" => Command::Rest(arg),
        "ABOR" => Command::Abor,
        "EVERYTHING" => Command::Everything(everything(&arg)),
        "SITE" => {
            let (sub, rest) = split(arg.trim_start_matches(' '));
            if sub == "EVERYTHING" {
                Command::Everything(everything(&rest))
            } else {
                Command::Site(sub)
            }
        }
        "STOR" | "STOU" | "APPE" | "DELE" | "RMD" | "XRMD" | "MKD" | "XMKD" | "RNFR" | "RNTO"
        | "MFMT" | "MFCT" | "MFF" | "ALLO" | "SMNT" => Command::Write(verb),
        _ => Command::Unknown(verb),
    })
}

/// A connection's `SITE EVERYTHING` settings (Everything's defaults: every switch off, no
/// limit on the count, by name ascending).
#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    pub search: String,
    pub filter_search: String,
    pub flags: Vec<Flag>,
    pub filter_flags: Vec<Flag>,
    pub sort: Sort,
    pub offset: u32,
    pub count: u32,
    pub columns: Vec<Column>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            search: String::new(),
            filter_search: String::new(),
            flags: Vec::new(),
            filter_flags: Vec::new(),
            sort: Sort::default(),
            offset: 0,
            count: u32::MAX,
            columns: Vec::new(),
        }
    }
}

fn set<T: PartialEq + Copy>(list: &mut Vec<T>, item: T, on: bool) {
    list.retain(|x| *x != item);
    if on {
        list.push(item);
    }
}

impl Options {
    pub fn has(&self, flag: Flag) -> bool {
        self.flags.contains(&flag)
    }

    pub fn column(&self, c: Column) -> bool {
        self.columns.contains(&c)
    }

    /// Applies a setting and returns the reply line (without its line break); `Query` is
    /// the session's (None).
    pub fn apply(&mut self, cmd: &Everything) -> Option<String> {
        let shown = |s: &str| s.chars().take(200).collect::<String>();
        Some(match cmd {
            Everything::Search(s) => {
                self.search = s.clone();
                format!("200 Search set to ({}).", shown(s))
            }
            Everything::FilterSearch(s) => {
                self.filter_search = s.clone();
                format!("200 Filter search set to ({}).", shown(s))
            }
            Everything::Flag(f, on) => {
                set(&mut self.flags, *f, *on);
                format!("200 {} set to ({}).", f.label(), u8::from(*on))
            }
            Everything::FilterFlag(f, on) => {
                set(&mut self.filter_flags, *f, *on);
                format!(
                    "200 Filter {} set to ({}).",
                    f.label().to_ascii_lowercase(),
                    u8::from(*on)
                )
            }
            Everything::Sort(s) => {
                self.sort = *s;
                "200 Sort set.".to_owned()
            }
            Everything::Offset(n) => {
                self.offset = *n;
                format!("200 Offset set to ({n}).")
            }
            Everything::Count(n) => {
                self.count = *n;
                format!("200 Count set to ({n}).")
            }
            Everything::Column(c, on) => {
                set(&mut self.columns, *c, *on);
                let label = Column::ALL
                    .iter()
                    .find(|(_, x, _)| x == c)
                    .map_or("", |(_, _, l)| l);
                format!("200 {label} set to ({}).", u8::from(*on))
            }
            Everything::BadSort(_) => "500 Unknown sort type.".to_owned(),
            Everything::Unknown(_) => "500 Unknown Everything command.".to_owned(),
            Everything::Query => return None,
        })
    }

    /// The system search of these settings (the count bounded by [`MAX_COUNT`]).
    pub fn params(&self) -> SystemSearchParams {
        SystemSearchParams {
            query: self.search.clone(),
            offset: Some(self.offset as usize),
            count: Some(self.count.min(MAX_COUNT) as usize),
            match_case: self.has(Flag::Case),
            whole_word: self.has(Flag::WholeWord),
            match_path: self.has(Flag::Path),
            match_diacritics: self.has(Flag::Diacritics),
            regex: self.has(Flag::Regex),
            prefix: self.has(Flag::Prefix),
            suffix: self.has(Flag::Suffix),
            ignore_punctuation: self.has(Flag::IgnorePunctuation),
            ignore_whitespace: self.has(Flag::IgnoreWhitespace),
            sort: Some(self.sort.system()),
            ascending: Some(self.sort.ascending),
        }
    }

    /// The filter search's settings (the results must match it too), None when empty.
    pub fn filter_params(&self) -> Option<SystemSearchParams> {
        if self.filter_search.trim().is_empty() {
            return None;
        }
        let has = |f| self.filter_flags.contains(&f);
        Some(SystemSearchParams {
            query: self.filter_search.clone(),
            match_case: has(Flag::Case),
            whole_word: has(Flag::WholeWord),
            match_path: has(Flag::Path),
            match_diacritics: has(Flag::Diacritics),
            regex: has(Flag::Regex),
            prefix: has(Flag::Prefix),
            suffix: has(Flag::Suffix),
            ignore_punctuation: has(Flag::IgnorePunctuation),
            ignore_whitespace: has(Flag::IgnoreWhitespace),
            ..SystemSearchParams::default()
        })
    }
}

/// One result as `QUERY` sends it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Item {
    pub name: String,
    pub is_dir: bool,
    /// Its folder in the server's names (`Media\Music`; empty for a top folder).
    pub path: String,
    pub size: Option<u64>,
    pub modified: Option<u64>,
    pub created: Option<u64>,
    pub attributes: u32,
}

/// The `QUERY` reply (with its line breaks): `200-Query results`, ` RESULT_COUNT n`, per
/// result its columns then ` FILE name` or ` FOLDER name`, `200 End.`. Unknown sizes and
/// dates are `u64::MAX`, as Everything sends them.
pub fn results(total: usize, items: &[Item], o: &Options) -> String {
    use std::fmt::Write;
    let mut out = String::from("200-Query results\r\n");
    let _ = write!(out, " RESULT_COUNT {total}\r\n");
    let unknown = |v: Option<u64>| v.unwrap_or(u64::MAX);
    for i in items {
        if o.column(Column::Path) {
            let _ = write!(out, " PATH {}\r\n", i.path);
        }
        if o.column(Column::Attributes) {
            let _ = write!(out, " ATTRIBUTES {}\r\n", i.attributes);
        }
        if o.column(Column::Size) {
            let size = if i.is_dir { None } else { i.size };
            let _ = write!(out, " SIZE {}\r\n", unknown(size));
        }
        if o.column(Column::DateModified) {
            let _ = write!(out, " DATE_MODIFIED {}\r\n", unknown(i.modified));
        }
        if o.column(Column::DateCreated) {
            let _ = write!(out, " DATE_CREATED {}\r\n", unknown(i.created));
        }
        if o.column(Column::FileListFilename) {
            out.push_str(" FILE_LIST_FILENAME \r\n");
        }
        if o.column(Column::DateRecentlyChanged) {
            let _ = write!(out, " DATE_RECENTLY_CHANGED {}\r\n", unknown(i.modified));
        }
        let kind = if i.is_dir { "FOLDER" } else { "FILE" };
        let _ = write!(out, " {kind} {}\r\n", i.name);
    }
    out.push_str("200 End.\r\n");
    out
}

/// A path in a `257` reply: double quotes doubled (RFC 959).
pub fn quoted(path: &str) -> String {
    format!("\"{}\"", path.replace('"', "\"\""))
}

/// `LIST`'s and `NLST`'s argument without the `ls` options clients add (`-a`, `-la`).
pub fn list_target(arg: &str) -> &str {
    let mut rest = arg.trim_start_matches(' ');
    while rest.starts_with('-') {
        rest = rest
            .split_once(' ')
            .map_or("", |(_, r)| r)
            .trim_start_matches(' ');
    }
    rest
}

/// The welcome message as `220-` lines and a last `220 ` line (control characters dropped).
pub fn welcome(text: &str) -> String {
    let lines: Vec<String> = text
        .lines()
        .map(|l| l.chars().filter(|c| !c.is_control()).collect())
        .collect();
    let lines = if lines.iter().all(|l: &String| l.trim().is_empty()) {
        vec![super::DEFAULT_WELCOME.to_owned()]
    } else {
        lines
    };
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        let sep = if i + 1 == lines.len() { ' ' } else { '-' };
        out.push_str(&format!("220{sep}{l}\r\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse() {
        assert_eq!(parse("user me"), Ok(Command::User("me".into())));
        assert_eq!(
            parse("PASS  two spaces"),
            Ok(Command::Pass(" two spaces".into()))
        );
        assert_eq!(
            parse("CWD /Media/a b"),
            Ok(Command::Cwd("/Media/a b".into()))
        );
        assert_eq!(parse("RETR  x"), Ok(Command::Retr(" x".into())));
        assert_eq!(parse("PWD"), Ok(Command::Pwd));
        assert_eq!(parse("eprt |1|1.2.3.4|5|"), Ok(Command::Active));
        assert_eq!(parse("STOR x"), Ok(Command::Write("STOR".into())));
        assert_eq!(parse("DELE x"), Ok(Command::Write("DELE".into())));
        assert_eq!(parse("XYZ"), Ok(Command::Unknown("XYZ".into())));
        assert_eq!(parse("SITE CHMOD 777 x"), Ok(Command::Site("CHMOD".into())));
        assert_eq!(parse(""), Err(Bad::Empty));
        assert_eq!(parse("RETR a\0b"), Err(Bad::Control));
        assert_eq!(parse("RETR a\rb"), Err(Bad::Control));
        assert_eq!(parse(&"A".repeat(MAX_LINE + 1)), Err(Bad::TooLong));
        // Everything's extension, through SITE and bare.
        let ev = |l: &str| match parse(l) {
            Ok(Command::Everything(e)) => e,
            other => panic!("{l}: {other:?}"),
        };
        assert_eq!(
            ev("SITE EVERYTHING SEARCH  abc def"),
            Everything::Search(" abc def".into())
        );
        assert_eq!(
            ev("site everything case 1"),
            Everything::Flag(Flag::Case, true)
        );
        assert_eq!(
            ev("EVERYTHING REGEX 0"),
            Everything::Flag(Flag::Regex, false)
        );
        assert_eq!(
            ev("SITE EVERYTHING WHOLE_WORD 7"),
            Everything::Flag(Flag::WholeWord, true)
        );
        assert_eq!(
            ev("SITE EVERYTHING FILTER_PATH 1"),
            Everything::FilterFlag(Flag::Path, true)
        );
        assert_eq!(
            ev("SITE EVERYTHING FILTER_SEARCH ext:mp3"),
            Everything::FilterSearch("ext:mp3".into())
        );
        assert_eq!(
            ev("SITE EVERYTHING PATH_COLUMN 1"),
            Everything::Column(Column::Path, true)
        );
        assert_eq!(ev("SITE EVERYTHING OFFSET 100"), Everything::Offset(100));
        assert_eq!(ev("SITE EVERYTHING COUNT -5"), Everything::Count(0));
        assert_eq!(
            ev("SITE EVERYTHING COUNT 99999999999"),
            Everything::Count(u32::MAX)
        );
        assert_eq!(ev("SITE EVERYTHING COUNT x"), Everything::Count(0));
        assert_eq!(ev("SITE EVERYTHING QUERY"), Everything::Query);
        assert_eq!(
            ev("SITE EVERYTHING SORT size_descending"),
            Everything::Sort(Sort {
                key: SortKey::Size,
                ascending: false
            })
        );
        assert_eq!(
            ev("SITE EVERYTHING SORT INVERSE_SIZE_ASCENDING"),
            Everything::Sort(Sort {
                key: SortKey::Size,
                ascending: false
            })
        );
        assert_eq!(
            ev("SITE EVERYTHING SORT NAME"),
            Everything::BadSort("NAME".into())
        );
        assert_eq!(
            ev("SITE EVERYTHING FOO 1"),
            Everything::Unknown("FOO".into())
        );
    }

    #[test]
    fn options_apply_and_answer() {
        let mut o = Options::default();
        assert_eq!(o.count, u32::MAX);
        let r = o.apply(&Everything::Flag(Flag::Case, true)).unwrap();
        assert_eq!(r, "200 Case set to (1).");
        o.apply(&Everything::Search("abc".into()));
        o.apply(&Everything::Count(5000));
        o.apply(&Everything::Sort(Sort {
            key: SortKey::DateModified,
            ascending: false,
        }));
        let p = o.params();
        assert_eq!(p.query, "abc");
        assert!(p.match_case && !p.regex);
        assert_eq!(p.count, Some(MAX_COUNT as usize));
        assert_eq!(
            (p.sort, p.ascending),
            (Some(SystemSort::DateModified), Some(false))
        );
        o.apply(&Everything::Flag(Flag::Case, false));
        assert!(!o.params().match_case);
        assert!(o.filter_params().is_none());
        o.apply(&Everything::FilterSearch("x".into()));
        o.apply(&Everything::FilterFlag(Flag::Regex, true));
        assert!(o.filter_params().unwrap().regex);
        assert!(o.apply(&Everything::Query).is_none());
        assert!(o
            .apply(&Everything::BadSort("x".into()))
            .unwrap()
            .starts_with("500"));
    }

    #[test]
    fn results_have_everythings_shape() {
        let mut o = Options::default();
        let items = vec![
            Item {
                name: "a.txt".into(),
                path: "Media\\Docs".into(),
                size: Some(12),
                modified: Some(133_444_736_000_000_000),
                attributes: 32,
                ..Item::default()
            },
            Item {
                name: "Media".into(),
                is_dir: true,
                size: Some(0),
                attributes: 16,
                ..Item::default()
            },
        ];
        assert_eq!(
            results(7, &items, &o),
            "200-Query results\r\n RESULT_COUNT 7\r\n FILE a.txt\r\n FOLDER Media\r\n200 End.\r\n"
        );
        for c in [
            Column::Path,
            Column::Size,
            Column::DateModified,
            Column::Attributes,
        ] {
            o.apply(&Everything::Column(c, true));
        }
        assert_eq!(
            results(2, &items, &o),
            "200-Query results\r\n RESULT_COUNT 2\r\n PATH Media\\Docs\r\n ATTRIBUTES 32\r\n SIZE 12\r\n DATE_MODIFIED 133444736000000000\r\n FILE a.txt\r\n PATH \r\n ATTRIBUTES 16\r\n SIZE 18446744073709551615\r\n DATE_MODIFIED 18446744073709551615\r\n FOLDER Media\r\n200 End.\r\n"
        );
    }

    #[test]
    fn small_helpers() {
        assert_eq!(quoted("/a \"b\""), "\"/a \"\"b\"\"\"");
        assert_eq!(list_target("-la /x y"), "/x y");
        assert_eq!(list_target("-a"), "");
        assert_eq!(list_target(" /x"), "/x");
        assert_eq!(welcome("one\ntwo\x07"), "220-one\r\n220 two\r\n");
        assert!(welcome("").starts_with("220 "));
        assert_eq!(
            (int(" 12x"), int("-3"), int("+"), dword("-1")),
            (12, -3, 0, 0)
        );
    }
}
