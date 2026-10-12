//! `--web`'s Everything-compatible search (`/everything`, or `/` with Everything's
//! parameters): the system search (`keel_api::system`) answered as Everything's HTTP
//! server answers, so tools written for it work. `json=1` gives Everything's JSON
//! (`{"totalResults": n, "results": [{"type", "name", "path", "size", "date_modified"}]}`,
//! size and date as strings, the date a Windows FILETIME), otherwise a plain HTML page
//! (no script) with a search form, the results and links to more.
//!
//! Parameters (Everything's names, short forms too): `search`/`s`/`q`, `offset`/`o`,
//! `count`/`c`, `json`/`j`, `case`/`i`, `diacritics`/`m`, `wholeword`/`w`, `path`/`p`,
//! `regex`/`r`, `prefix`, `suffix`, `ignore_punctuation`, `ignore_whitespace`, `sort`
//! (`name`, `path`, `date_modified`, `size`; others mean `name`), `ascending`,
//! `path_column`, `size_column`, `date_modified_column`. Switches take `1`/`0` (also
//! `true`/`false`, `on`/`off`, `yes`/`no`; no value means on); other parameters are
//! ignored. The last of a repeated parameter counts.
//!
//! Every request needs the daemon token (`Authorization: Bearer <token>`, or Basic with
//! the token as the password and any user name), or the HTTP login set with
//! `keel-daemon --set-http-login USER` (user and password in the OS keychain) as Basic
//! auth, for tools that only know Basic. Basic auth is refused on a non-loopback bind
//! (plain http there; behind a TLS proxy the bind is loopback). Repeated wrong
//! credentials from one address lock it out for a growing time (`web::AuthGuard`).
//! Downloads (`/everything/file?path=...`) work only while `[web] allow_downloads = true`
//! is in the profile's config.toml; signed in with the HTTP login, only a file a recent
//! search listed ([`Recent`]) and nothing in Keel's own folders.

use keel_api::system::{Found, MAX_COUNT};
use keel_api::types::{SystemSearchParams, SystemSort};
use keel_vfs::cloud::SecretStore;
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

/// Said when `--web` answers on a non-loopback address (`[web] allow_plaintext = true`): it
/// has no TLS.
pub const UNENCRYPTED: &str = "--web is bound to a network address without TLS: the token, \
search results and files cross the network unencrypted, and Basic sign-in (the HTTP login) is \
refused there. Use a private network (a tailnet), or bind a loopback address behind a TLS proxy.";

/// Longest query string accepted.
pub const MAX_QUERY_STRING: usize = 8 * 1024;
/// Shortest HTTP login password `--set-http-login` takes.
pub const MIN_PASSWORD: usize = 10;

/// A parsed Everything request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Request {
    pub params: SystemSearchParams,
    pub json: bool,
    pub path_column: bool,
    pub size_column: bool,
    pub date_modified_column: bool,
}

/// The parameters that make `/?...` an Everything search rather than the web client.
const SEARCH_KEYS: &[&str] = &[
    "search", "s", "q", "json", "j", "offset", "o", "count", "c", "sort",
];

/// `%XX` and `+` decoded (invalid escapes stay as they are; invalid UTF-8 is replaced).
pub fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                        continue;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encodes all but the unreserved characters.
pub fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// The query string's pairs, decoded, keys lowercase.
fn pairs(query: &str) -> impl Iterator<Item = (String, String)> + '_ {
    query.split('&').filter(|p| !p.is_empty()).map(|p| {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        (decode(k).to_ascii_lowercase(), decode(v))
    })
}

/// The last value of `key` in the query string.
pub fn param(query: &str, key: &str) -> Option<String> {
    pairs(query)
        .filter(|(k, _)| k == key)
        .last()
        .map(|(_, v)| v)
}

/// Whether `/?<query>` asks for an Everything search.
pub fn is_search(query: &str) -> bool {
    pairs(query).any(|(k, _)| SEARCH_KEYS.contains(&k.as_str()))
}

fn switch(key: &str, v: &str) -> Result<bool, String> {
    match v.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "true" | "on" | "yes" => Ok(true),
        "0" | "false" | "off" | "no" => Ok(false),
        _ => Err(format!("{key}: expected 1 or 0, not {v:?}")),
    }
}

fn number(key: &str, v: &str) -> Result<usize, String> {
    let v = v.trim();
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{key}: expected a number, not {v:?}"));
    }
    // Larger than any count or offset can use: as large as it gets.
    Ok(v.parse::<usize>().unwrap_or(usize::MAX))
}

/// Parses Everything's query string. A bad number or switch is an error (400).
pub fn parse(query: &str) -> Result<Request, String> {
    if query.len() > MAX_QUERY_STRING {
        return Err(format!(
            "the query string is longer than {MAX_QUERY_STRING} bytes"
        ));
    }
    let mut r = Request::default();
    for (k, v) in pairs(query) {
        let p = &mut r.params;
        match k.as_str() {
            "search" | "s" | "q" => p.query = v,
            "offset" | "o" => p.offset = Some(number(&k, &v)?),
            "count" | "c" => p.count = Some(number(&k, &v)?),
            "json" | "j" => r.json = switch(&k, &v)?,
            "case" | "i" => p.match_case = switch(&k, &v)?,
            "diacritics" | "m" => p.match_diacritics = switch(&k, &v)?,
            "wholeword" | "w" => p.whole_word = switch(&k, &v)?,
            "path" | "p" => p.match_path = switch(&k, &v)?,
            "regex" | "r" => p.regex = switch(&k, &v)?,
            "prefix" => p.prefix = switch(&k, &v)?,
            "suffix" => p.suffix = switch(&k, &v)?,
            "ignore_punctuation" => p.ignore_punctuation = switch(&k, &v)?,
            "ignore_whitespace" => p.ignore_whitespace = switch(&k, &v)?,
            "sort" => {
                p.sort = Some(match v.trim().to_ascii_lowercase().as_str() {
                    "path" => SystemSort::Path,
                    "date_modified" => SystemSort::DateModified,
                    "size" => SystemSort::Size,
                    _ => SystemSort::Name,
                })
            }
            "ascending" => p.ascending = Some(switch(&k, &v)?),
            "path_column" => r.path_column = switch(&k, &v)?,
            "size_column" => r.size_column = switch(&k, &v)?,
            "date_modified_column" => r.date_modified_column = switch(&k, &v)?,
            _ => {}
        }
    }
    if let Some(c) = &mut r.params.count {
        *c = (*c).min(MAX_COUNT);
    }
    Ok(r)
}

/// The query string of `r` (non-default parameters only), for links.
pub fn query_string(r: &Request) -> String {
    let p = &r.params;
    let mut out = vec![format!("search={}", encode(&p.query))];
    let mut add = |k: &str, on: bool| {
        if on {
            out.push(format!("{k}=1"));
        }
    };
    add("case", p.match_case);
    add("diacritics", p.match_diacritics);
    add("wholeword", p.whole_word);
    add("path", p.match_path);
    add("regex", p.regex);
    add("prefix", p.prefix);
    add("suffix", p.suffix);
    add("ignore_punctuation", p.ignore_punctuation);
    add("ignore_whitespace", p.ignore_whitespace);
    add("json", r.json);
    add("path_column", r.path_column);
    add("size_column", r.size_column);
    add("date_modified_column", r.date_modified_column);
    if let Some(s) = p.sort {
        out.push(format!("sort={}", sort_name(s)));
    }
    if let Some(a) = p.ascending {
        out.push(format!("ascending={}", u8::from(a)));
    }
    if let Some(o) = p.offset.filter(|o| *o > 0) {
        out.push(format!("offset={o}"));
    }
    if let Some(c) = p.count {
        out.push(format!("count={c}"));
    }
    out.join("&")
}

fn sort_name(s: SystemSort) -> &'static str {
    match s {
        SystemSort::Name => "name",
        SystemSort::Path => "path",
        SystemSort::DateModified => "date_modified",
        SystemSort::Size => "size",
    }
}

/// 100 ns intervals since 1601 (a Windows FILETIME), as Everything reports dates.
pub fn filetime(t: SystemTime) -> u64 {
    const EPOCH: u64 = 116_444_736_000_000_000;
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => EPOCH.saturating_add((d.as_nanos() / 100).min(u128::from(u64::MAX)) as u64),
        Err(e) => {
            EPOCH.saturating_sub((e.duration().as_nanos() / 100).min(u128::from(EPOCH)) as u64)
        }
    }
}

/// Everything's JSON answer.
pub fn to_json(found: &Found, r: &Request) -> Value {
    let results: Vec<Value> = found
        .hits
        .iter()
        .map(|h| {
            let mut o = serde_json::Map::new();
            o.insert(
                "type".into(),
                json!(if h.is_dir { "folder" } else { "file" }),
            );
            o.insert("name".into(), json!(h.path.name()));
            if r.path_column {
                let parent = h.path.parent().map(|p| p.display()).unwrap_or_default();
                o.insert("path".into(), json!(parent));
            }
            if r.size_column {
                // Folder sizes are not indexed: empty, as Everything without them.
                let size = if h.is_dir {
                    String::new()
                } else {
                    h.size.to_string()
                };
                o.insert("size".into(), json!(size));
            }
            if r.date_modified_column {
                let date = h.modified.map(|t| filetime(t).to_string());
                o.insert("date_modified".into(), json!(date.unwrap_or_default()));
            }
            Value::Object(o)
        })
        .collect();
    json!({"totalResults": found.total, "results": results})
}

/// Text for HTML.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

const STYLE: &str = "body{font-family:system-ui,sans-serif;margin:16px;color:#222;background:#fff}\
input[type=search]{width:min(36em,90%);font-size:1.1em;padding:4px}\
table{border-collapse:collapse;width:100%;margin-top:8px}\
th,td{text-align:left;padding:3px 8px;border-bottom:1px solid #ddd;word-break:break-all}\
td.n{text-align:right;white-space:nowrap}.warn{background:#fff3cd;color:#5c4400;padding:8px;margin:8px 0}\
.err{color:#a00}@media (prefers-color-scheme:dark){body{background:#1e1e1e;color:#ddd}\
th,td{border-color:#444}a{color:#8ab4f8}.warn{background:#4a3b00;color:#ffe08a}}";

/// What an HTML page shows besides the results.
pub struct Page<'a> {
    /// The path the form and links go to.
    pub base: &'a str,
    /// Links to download files.
    pub downloads: bool,
    /// Served on a non-loopback address without TLS.
    pub unencrypted: bool,
}

/// The HTML page: the form, `found` (or `error`) and links to more.
pub fn to_html(r: &Request, found: Result<&Found, &str>, page: &Page) -> String {
    let p = &r.params;
    let mut h = String::new();
    h.push_str("<!doctype html><html><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    h.push_str("<title>Search all files - Keel</title><style>");
    h.push_str(STYLE);
    h.push_str("</style></head><body><h1>Search all files</h1>");
    if page.unencrypted {
        h.push_str(
            "<p class=\"warn\">Not encrypted: this server answers on the network over plain \
             http, so passwords, the token, names and files cross it readable. Use a TLS proxy \
             or a private network (docs/daemon.md).</p>",
        );
    }
    h.push_str(&format!(
        "<form method=\"get\" action=\"{}\"><input type=\"search\" name=\"search\" value=\"{}\" \
         autofocus> <button>Search</button>",
        escape(page.base),
        escape(&p.query)
    ));
    let boxes = [
        ("case", "Match case", p.match_case),
        ("wholeword", "Whole words", p.whole_word),
        ("path", "Match path", p.match_path),
        ("diacritics", "Match diacritics", p.match_diacritics),
        ("regex", "Regex", p.regex),
    ];
    for (name, label, on) in boxes {
        h.push_str(&format!(
            " <label><input type=\"checkbox\" name=\"{name}\" value=\"1\"{}> {label}</label>",
            if on { " checked" } else { "" }
        ));
    }
    h.push_str("</form>");
    let found = match found {
        Ok(f) => f,
        Err(e) => {
            h.push_str(&format!("<p class=\"err\">{}</p></body></html>", escape(e)));
            return h;
        }
    };
    let offset = found.offset;
    h.push_str(&format!(
        "<p>{} result{}{} ({})</p>",
        found.total,
        if found.total == 1 { "" } else { "s" },
        if found.capped {
            " in the first 10000 found"
        } else {
            ""
        },
        escape(&found.backend)
    ));
    let sort = p.sort.unwrap_or_default();
    let ascending = p.ascending.unwrap_or(true);
    let header = |col: SystemSort, label: &str| {
        let mut next = r.clone();
        next.params.sort = Some(col);
        next.params.ascending = Some(!(sort == col && ascending));
        next.params.offset = None;
        let mark = match (sort == col, ascending) {
            (true, true) => " \u{25B2}",
            (true, false) => " \u{25BC}",
            _ => "",
        };
        format!(
            "<th><a href=\"{}?{}\">{label}</a>{mark}</th>",
            escape(page.base),
            escape(&query_string(&next))
        )
    };
    h.push_str("<table><tr>");
    h.push_str(&header(SystemSort::Name, "Name"));
    h.push_str(&header(SystemSort::Path, "Path"));
    h.push_str(&header(SystemSort::Size, "Size"));
    h.push_str(&header(SystemSort::DateModified, "Date Modified"));
    h.push_str("</tr>");
    for hit in &found.hits {
        let name = escape(hit.path.name());
        let full = hit.path.display();
        let name = if page.downloads && !hit.is_dir {
            format!(
                "<a href=\"/everything/file?path={}\" download>{name}</a>",
                escape(&encode(&full))
            )
        } else if hit.is_dir {
            format!("{name}/")
        } else {
            name
        };
        let parent = hit.path.parent().map(|p| p.display()).unwrap_or_default();
        let size = if hit.is_dir {
            String::new()
        } else {
            human(hit.size)
        };
        let date = hit
            .modified
            .map(|t| {
                chrono::DateTime::<chrono::Local>::from(t)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        h.push_str(&format!(
            "<tr><td>{name}</td><td>{}</td><td class=\"n\">{size}</td><td class=\"n\">{date}</td></tr>",
            escape(&parent)
        ));
    }
    h.push_str("</table><p>");
    let count = p.count.unwrap_or(keel_api::system::DEFAULT_COUNT);
    let mut nav = |label: &str, to: usize| {
        let mut next = r.clone();
        next.params.offset = Some(to);
        h.push_str(&format!(
            "<a href=\"{}?{}\">{label}</a> ",
            escape(page.base),
            escape(&query_string(&next))
        ));
    };
    if offset > 0 && count > 0 {
        nav("\u{2190} Previous", offset.saturating_sub(count));
    }
    if count > 0 && offset.saturating_add(count) < found.total {
        nav("Next \u{2192}", offset + count);
    }
    h.push_str("</p></body></html>");
    h
}

/// How a request signed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Who {
    /// The daemon token (bearer, or the Basic password): the same power as the API.
    Token,
    /// The HTTP login, a password for third-party tools.
    Login,
}

/// Who may search: the daemon token as a bearer token or as a Basic password, or the HTTP
/// login (`login`: user and password) as Basic auth.
pub fn authorized(header: Option<&str>, token: &str, login: Option<&(String, String)>) -> bool {
    signed_in(header, token, login).is_some()
}

/// [`authorized`], and by which credential.
pub fn signed_in(header: Option<&str>, token: &str, login: Option<&(String, String)>) -> Option<Who> {
    let header = header.map(str::trim)?;
    let (scheme, rest) = header.split_once(' ').unwrap_or((header, ""));
    let rest = rest.trim();
    if scheme.eq_ignore_ascii_case("bearer") {
        return crate::ws::same(rest.as_bytes(), token.as_bytes()).then_some(Who::Token);
    }
    if !scheme.eq_ignore_ascii_case("basic") || rest.len() > 4096 {
        return None;
    }
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD.decode(rest).ok()?;
    let text = String::from_utf8(raw).ok()?;
    let (user, password) = text.split_once(':')?;
    let by_token = crate::ws::same(password.as_bytes(), token.as_bytes());
    let by_login = login.is_some_and(|(u, p)| {
        // Both compared in full, whatever the first gives.
        let user_ok = crate::ws::same(user.as_bytes(), u.as_bytes());
        let pass_ok = crate::ws::same(password.as_bytes(), p.as_bytes());
        user_ok & pass_ok
    });
    if by_token {
        Some(Who::Token)
    } else if by_login {
        Some(Who::Login)
    } else {
        None
    }
}

/// Whether the header asks to sign in with Basic auth.
pub fn is_basic(header: Option<&str>) -> bool {
    header.is_some_and(|h| {
        h.trim_start()
            .get(..6)
            .is_some_and(|s| s.eq_ignore_ascii_case("basic "))
    })
}

/// The most paths [`Recent`] keeps.
pub const RECENT_MAX: usize = 20_000;

/// The files recent searches listed. A download signed in with the HTTP login (a password
/// meant for third-party search tools) may only name one of these, not any file the
/// daemon's user can read; the daemon token, which is the whole API anyway, is not held
/// to it.
#[derive(Default)]
pub struct Recent(parking_lot::Mutex<(std::collections::VecDeque<String>, std::collections::HashSet<String>)>);

impl Recent {
    /// How paths are compared: Windows ignores case and takes `/` for `\`.
    fn key(path: &str) -> String {
        if cfg!(windows) {
            path.replace('/', "\\").to_lowercase()
        } else {
            path.to_owned()
        }
    }

    /// Remembers `path`, forgetting the oldest beyond [`RECENT_MAX`].
    pub fn remember(&self, path: &str) {
        let key = Self::key(path);
        let mut g = self.0.lock();
        if g.1.insert(key.clone()) {
            g.0.push_back(key);
            while g.0.len() > RECENT_MAX {
                if let Some(old) = g.0.pop_front() {
                    g.1.remove(&old);
                }
            }
        }
    }

    pub fn contains(&self, path: &str) -> bool {
        self.0.lock().1.contains(&Self::key(path))
    }
}

/// The keychain entry of a profile's HTTP login.
fn login_key(profile: &str) -> String {
    format!("keel-daemon/http-login/{profile}")
}

/// The profile's HTTP login (user, password), if one is set.
pub fn login(store: &dyn SecretStore, profile: &str) -> anyhow::Result<Option<(String, String)>> {
    Ok(store
        .get(&login_key(profile))?
        .and_then(|v| v.split_once(':').map(|(u, p)| (u.to_owned(), p.to_owned()))))
}

/// Stores the profile's HTTP login in the keychain. The user may not hold `:` or control
/// characters; the password needs `MIN_PASSWORD` characters and no line breaks.
pub fn set_login(
    store: &dyn SecretStore,
    profile: &str,
    user: &str,
    password: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !user.is_empty()
            && user.len() <= 128
            && !user.contains(':')
            && !user.chars().any(char::is_control),
        "the user name must be 1 to 128 characters, without ':'"
    );
    anyhow::ensure!(
        password.chars().count() >= MIN_PASSWORD
            && password.len() <= 1024
            && !password.chars().any(char::is_control),
        "the password must be at least {MIN_PASSWORD} characters, on one line"
    );
    store.set(&login_key(profile), &format!("{user}:{password}"))
}

/// Removes the profile's HTTP login.
pub fn clear_login(store: &dyn SecretStore, profile: &str) -> anyhow::Result<()> {
    store.delete(&login_key(profile))
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_vfs::cloud::MemoryStore;
    use keel_vfs::VPath;
    use std::time::Duration;

    #[test]
    fn everything_parameters_parse() {
        let r = parse("search=foo+bar&o=10&c=5&j=1&i=1&m=1&w=1&p=1&r=0&prefix=1&suffix&ignore_punctuation=true&ignore_whitespace=on&sort=date_modified&ascending=0&path_column=1&size_column=1&date_modified_column=1").unwrap();
        let p = &r.params;
        assert_eq!(p.query, "foo bar");
        assert_eq!((p.offset, p.count), (Some(10), Some(5)));
        assert!(r.json && p.match_case && p.match_diacritics && p.whole_word && p.match_path);
        assert!(!p.regex && p.prefix && p.suffix && p.ignore_punctuation && p.ignore_whitespace);
        assert_eq!(
            (p.sort, p.ascending),
            (Some(SystemSort::DateModified), Some(false))
        );
        assert!(r.path_column && r.size_column && r.date_modified_column);
        // Long names, the last repeat wins, keys in any case, `q` and `s` too.
        let r = parse(
            "SEARCH=a&q=%C3%A9t%C3%A9%20x&json=0&wholeword=1&diacritics=0&regex=1&count=3&offset=1",
        )
        .unwrap();
        assert_eq!(r.params.query, "été x");
        assert!(!r.json && r.params.whole_word && r.params.regex && !r.params.match_diacritics);
        assert_eq!(parse("s=x").unwrap().params.query, "x");
        // Unknown sort is by name; unknown parameters are ignored.
        let r = parse("search=x&sort=extension&foo=bar").unwrap();
        assert_eq!(r.params.sort, Some(SystemSort::Name));
        assert_eq!(parse("").unwrap(), Request::default());
        // Bad numbers and switches are refused; huge counts are bounded.
        for bad in [
            "count=-1",
            "offset=x",
            "o=",
            "json=2",
            "case=maybe",
            "c=1e3",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert_eq!(
            parse("count=99999999999999999999999").unwrap().params.count,
            Some(MAX_COUNT)
        );
        assert_eq!(parse("c=1001").unwrap().params.count, Some(MAX_COUNT));
        assert_eq!(
            parse("offset=99999999999999999999999")
                .unwrap()
                .params
                .offset,
            Some(usize::MAX)
        );
        assert!(parse(&"a".repeat(MAX_QUERY_STRING + 1)).is_err());
        // Bad escapes stay; bad UTF-8 is replaced.
        assert_eq!(decode("%zz%4"), "%zz%4");
        assert_eq!(decode("%ff"), "\u{fffd}");
        assert_eq!(decode("a%2"), "a%2");
        // What makes `/?...` a search.
        assert!(is_search("search=x") && is_search("j=1") && is_search("q="));
        assert!(!is_search("share=abc&files=1&bytes=2") && !is_search(""));
        // A round trip through the links.
        let r = parse("search=a b&case=1&sort=size&ascending=0&offset=4&count=2&json=1").unwrap();
        assert_eq!(parse(&query_string(&r)).unwrap(), r);
    }

    fn found() -> Found {
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let sep = if cfg!(windows) { r"C:\d\" } else { "/d/" };
        Found {
            backend: "Test".into(),
            total: 7,
            capped: false,
            offset: 2,
            hits: vec![
                keel_api::system::Hit {
                    path: VPath::local(format!("{sep}a <b>.txt")),
                    is_dir: false,
                    size: 1234,
                    modified: Some(t),
                    snippet: None,
                },
                keel_api::system::Hit {
                    path: VPath::local(format!("{sep}sub")),
                    is_dir: true,
                    size: 0,
                    modified: None,
                    snippet: None,
                },
            ],
        }
    }

    #[test]
    fn json_has_everythings_shape() {
        let r = parse("search=a&json=1").unwrap();
        let v = to_json(&found(), &r);
        assert_eq!(v["totalResults"], 7);
        assert_eq!(
            v["results"][0],
            json!({"type": "file", "name": "a <b>.txt"})
        );
        assert_eq!(v["results"][1], json!({"type": "folder", "name": "sub"}));
        let r =
            parse("search=a&json=1&path_column=1&size_column=1&date_modified_column=1").unwrap();
        let v = to_json(&found(), &r);
        let parent = if cfg!(windows) { r"C:\d" } else { "/d" };
        assert_eq!(
            v["results"][0],
            json!({"type": "file", "name": "a <b>.txt", "path": parent, "size": "1234", "date_modified": "133444736000000000"})
        );
        assert_eq!(v["results"][1]["size"], "");
        assert_eq!(v["results"][1]["date_modified"], "");
        assert_eq!(filetime(UNIX_EPOCH), 116_444_736_000_000_000);
        assert_eq!(
            filetime(UNIX_EPOCH - Duration::from_secs(1)),
            116_444_735_990_000_000
        );
    }

    #[test]
    fn html_escapes_and_links() {
        let r = parse("search=<x>&count=2").unwrap();
        let page = Page {
            base: "/everything",
            downloads: false,
            unencrypted: true,
        };
        let html = to_html(&r, Ok(&found()), &page);
        assert!(html.contains("a &lt;b&gt;.txt") && !html.contains("<b>.txt"));
        assert!(html.contains("value=\"&lt;x&gt;\""));
        assert!(html.contains("Not encrypted"));
        assert!(!html.contains("/everything/file"), "downloads are off");
        assert!(
            html.contains("offset=4") && html.contains("Previous"),
            "{html}"
        );
        assert!(!html.contains("<script"));
        let page = Page {
            downloads: true,
            unencrypted: false,
            ..page
        };
        let html = to_html(&r, Ok(&found()), &page);
        assert!(html.contains("/everything/file?path=") && !html.contains("Not encrypted"));
        let html = to_html(&r, Err("bad <thing>"), &page);
        assert!(html.contains("bad &lt;thing&gt;"));
    }

    #[test]
    fn auth_takes_the_token_or_the_login() {
        use base64::Engine;
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        let token = "t".repeat(64);
        let creds = ("me".to_owned(), "correct horse".to_owned());
        let ok = |h: &str, l| authorized(Some(h), &token, l);
        assert!(ok(&format!("Bearer {token}"), None));
        assert!(ok(&format!("bearer  {token} "), None));
        assert!(!ok("Bearer nope", None));
        assert!(ok(&format!("Basic {}", b64(&format!("any:{token}"))), None));
        assert!(ok(
            &format!("Basic {}", b64("me:correct horse")),
            Some(&creds)
        ));
        assert!(!ok(&format!("Basic {}", b64("me:correct horse")), None));
        assert!(!ok(
            &format!("Basic {}", b64("you:correct horse")),
            Some(&creds)
        ));
        assert!(!ok(&format!("Basic {}", b64("me:wrong")), Some(&creds)));
        assert!(!ok("Basic !!!", Some(&creds)));
        assert!(!ok("Digest x", Some(&creds)));
        assert!(!authorized(None, &token, Some(&creds)));
        // The login lives in the keychain, checked when set.
        let store = MemoryStore::default();
        assert!(login(&store, "p").unwrap().is_none());
        assert!(set_login(&store, "p", "a:b", "long enough password").is_err());
        assert!(set_login(&store, "p", "", "long enough password").is_err());
        assert!(set_login(&store, "p", "me", "short").is_err());
        assert!(set_login(&store, "p", "me", "line\nbreak password").is_err());
        set_login(&store, "p", "me", "pass:with:colons").unwrap();
        assert_eq!(
            login(&store, "p").unwrap(),
            Some(("me".to_owned(), "pass:with:colons".to_owned()))
        );
        assert!(login(&store, "other").unwrap().is_none(), "per profile");
        clear_login(&store, "p").unwrap();
        assert!(login(&store, "p").unwrap().is_none());
    }

    #[test]
    fn the_credential_is_told_apart() {
        use base64::Engine;
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        let token = "t".repeat(64);
        let creds = ("me".to_owned(), "correct horse".to_owned());
        let who = |h: &str| signed_in(Some(h), &token, Some(&creds));
        assert_eq!(who(&format!("Bearer {token}")), Some(Who::Token));
        assert_eq!(
            who(&format!("Basic {}", b64(&format!("x:{token}")))),
            Some(Who::Token)
        );
        assert_eq!(
            who(&format!("Basic {}", b64("me:correct horse"))),
            Some(Who::Login)
        );
        assert_eq!(who("Bearer correct horse"), None);
        assert_eq!(signed_in(None, &token, Some(&creds)), None);
        assert!(is_basic(Some(" basic abc")) && !is_basic(Some("Bearer x")) && !is_basic(None));
    }

    #[test]
    fn recent_remembers_a_bounded_number_of_paths() {
        let r = Recent::default();
        assert!(!r.contains("/a/b.txt"));
        r.remember("/a/b.txt");
        r.remember("/a/b.txt");
        assert!(r.contains("/a/b.txt") && !r.contains("/a/b.txt2") && !r.contains("/a"));
        if cfg!(windows) {
            assert!(r.contains(r"\A\B.TXT"), "case and separators do not matter");
        }
        for i in 0..RECENT_MAX {
            r.remember(&format!("/f/{i}"));
        }
        assert!(!r.contains("/a/b.txt"), "the oldest went");
        assert!(r.contains("/f/0") && r.contains(&format!("/f/{}", RECENT_MAX - 1)));
        let g = r.0.lock();
        assert_eq!((g.0.len(), g.1.len()), (RECENT_MAX, RECENT_MAX));
    }
}
