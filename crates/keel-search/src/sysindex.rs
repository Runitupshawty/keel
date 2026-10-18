//! `si:` / `system-index:` (Everything 1.5): the words of the term are looked up in the
//! Windows Search index (OLE DB, provider `Search.CollatorDSO`), which knows file
//! contents and properties for the folders it indexes. What it finds is checked against
//! the rest of the query with Keel's own evaluator (`keel_query`) and merged into the
//! results, so `si:budget ext:xlsx` lists the workbooks whose text says "budget".
//!
//! The query is bounded: at most [`MAX_RESULTS`] rows (or the search's own limit) and
//! [`TIMEOUT`]; the words become `CONTAINS` prefix terms and file name `LIKE` patterns,
//! with every quote, wildcard and bracket escaped ([`build_sql`]). Other platforms answer
//! `si:` with an error.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use keel_query::{DateProp, Props, Timestamp, WithRuns};
use parking_lot::Mutex;

use crate::{Hit, IndexSettings, Query, SearchState, Searcher};

/// Rows asked of the Windows Search index per `si:` text, at most.
pub const MAX_RESULTS: u32 = 10_000;
/// The longest one Windows Search query may take.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// A text searched again within this long reuses the last rows (search as you type).
const REUSE: Duration = Duration::from_secs(5);

/// The words of an `si:` text: split at white space, quotes dropped, empty ones skipped.
pub fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| w.replace('"', ""))
        .filter(|w| w.chars().any(|c| c != '*' && c != '?'))
        .collect()
}

/// `text` inside an SQL string literal: `'` doubled.
fn sql_text(text: &str) -> String {
    text.replace('\'', "''")
}

/// A word as a `CONTAINS` prefix term: `"word*"`. Inner wildcards cannot be expressed
/// there and are dropped (the `LIKE` side keeps them); quotes are already gone.
fn contains_term(word: &str) -> String {
    let w: String = word
        .chars()
        .filter(|c| !matches!(c, '*' | '?' | '"'))
        .collect();
    format!("\"{w}*\"")
}

/// A word as a `LIKE` pattern on the file name: `%word%`, with `%`, `_` and `[` taken
/// literally (`[%]`, `[_]`, `[[]`) and Everything's `*` / `?` as `%` / `_`.
fn like_pattern(word: &str) -> String {
    let mut out = String::from("%");
    for c in word.chars() {
        match c {
            '%' => out.push_str("[%]"),
            '_' => out.push_str("[_]"),
            '[' => out.push_str("[[]"),
            '*' => out.push('%'),
            '?' => out.push('_'),
            c => out.push(c),
        }
    }
    out.push('%');
    out
}

/// A folder as a Windows Search scope: `file:C:/Users/me`.
fn scope_url(dir: &Path) -> String {
    let text = dir.to_string_lossy().replace('\\', "/");
    format!("file:{}", text.trim_end_matches('/'))
}

/// The Windows Search SQL for an `si:` text: every word must be in the item's text or
/// properties (`CONTAINS`) or in its file name (`LIKE`); only files and folders (the
/// `file:` scope, or `within`); the best ranked first; at most `max` rows. `None` when the
/// text has no words.
pub fn build_sql(text: &str, within: Option<&Path>, max: u32) -> Option<String> {
    let words = words(text);
    if words.is_empty() {
        return None;
    }
    let scope = within.map_or_else(|| "file:".to_owned(), scope_url);
    let mut clauses = vec![format!("SCOPE='{}'", sql_text(&scope))];
    for w in &words {
        clauses.push(format!(
            "(CONTAINS(*, '{}') OR System.FileName LIKE '{}')",
            sql_text(&contains_term(w)),
            sql_text(&like_pattern(w))
        ));
    }
    Some(format!(
        "SELECT TOP {} System.ItemPathDisplay FROM SystemIndex WHERE {} ORDER BY System.Search.Rank DESC",
        max.clamp(1, MAX_RESULTS),
        clauses.join(" AND ")
    ))
}

/// Runs `sql` against the Windows Search index on a worker thread, waiting at most
/// [`TIMEOUT`]: the paths it returned.
#[cfg(windows)]
pub fn run_sql(sql: &str) -> anyhow::Result<Vec<PathBuf>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let sql = sql.to_owned();
    std::thread::Builder::new()
        .name("keel-system-index".into())
        .spawn(move || {
            let _ = tx.send(oledb::query(&sql));
        })?;
    match rx.recv_timeout(TIMEOUT) {
        Ok(r) => r,
        Err(_) => anyhow::bail!("the Windows Search index did not answer within {TIMEOUT:?}"),
    }
}

#[cfg(not(windows))]
pub fn run_sql(_sql: &str) -> anyhow::Result<Vec<PathBuf>> {
    anyhow::bail!("si: searches the Windows Search index, which only Windows has")
}

/// Rows of recent queries, for search as you type.
type Recent = Mutex<HashMap<String, (Instant, Arc<Vec<PathBuf>>)>>;

fn recent() -> &'static Recent {
    static RECENT: std::sync::OnceLock<Recent> = std::sync::OnceLock::new();
    RECENT.get_or_init(Recent::default)
}

/// What the Windows Search index finds for an `si:` text (within a folder, at most `max`).
pub fn search(text: &str, within: Option<&Path>, max: u32) -> anyhow::Result<Arc<Vec<PathBuf>>> {
    let sql = build_sql(text, within, max)
        .ok_or_else(|| anyhow::anyhow!("si: needs the words to search for"))?;
    if let Some((at, rows)) = recent().lock().get(&sql) {
        if at.elapsed() < REUSE {
            return Ok(rows.clone());
        }
    }
    let rows = Arc::new(run_sql(&sql)?);
    let mut r = recent().lock();
    r.retain(|_, (at, _)| at.elapsed() < REUSE);
    r.insert(sql, (Instant::now(), rows.clone()));
    Ok(rows)
}

/// The search options of `q` for `keel_query`.
pub fn query_options(q: &Query) -> keel_query::Options {
    keel_query::Options {
        match_case: q.match_case,
        match_diacritics: q.match_diacritics,
        match_whole_words: q.whole_word,
        match_path: q.match_path,
        whole_filename_wildcards: q.whole_filename_wildcards,
        ..keel_query::Options::default()
    }
}

/// A file or folder the Windows Search index returned, as the evaluator sees it.
struct Found {
    path: String,
    name: String,
    is_dir: bool,
    size: u64,
    modified: Option<Timestamp>,
    created: Option<Timestamp>,
    accessed: Option<Timestamp>,
    attrs: Option<u32>,
    /// The `si:` texts that found it.
    found_for: Vec<usize>,
    texts: Arc<Vec<String>>,
}

fn ms(t: std::io::Result<SystemTime>) -> Option<Timestamp> {
    t.ok().map(crate::journal::ms_of)
}

impl Found {
    fn stat(path: &Path, found_for: Vec<usize>, texts: Arc<Vec<String>>) -> Option<Self> {
        let m = std::fs::metadata(path).ok()?;
        #[cfg(windows)]
        let attrs = Some(std::os::windows::fs::MetadataExt::file_attributes(&m));
        #[cfg(not(windows))]
        let attrs = None;
        Some(Self {
            path: crate::journal::path_text(path),
            name: path.file_name().map_or_else(
                || crate::journal::path_text(path),
                |n| n.to_string_lossy().into_owned(),
            ),
            is_dir: m.is_dir(),
            size: if m.is_dir() { 0 } else { m.len() },
            modified: ms(m.modified()),
            created: ms(m.created()),
            accessed: ms(m.accessed()),
            attrs,
            found_for,
            texts,
        })
    }
}

impl Props for Found {
    fn name(&self) -> &str {
        &self.name
    }
    fn path(&self) -> &str {
        &self.path
    }
    fn is_dir(&self) -> bool {
        self.is_dir
    }
    fn size(&self) -> Option<u64> {
        (!self.is_dir).then_some(self.size)
    }
    fn allocated(&self) -> Option<u64> {
        None
    }
    fn date(&self, which: DateProp) -> Option<Timestamp> {
        match which {
            DateProp::Modified => self.modified,
            DateProp::Created => self.created,
            DateProp::Accessed => self.accessed,
            _ => None,
        }
    }
    fn attributes(&self) -> Option<u32> {
        self.attrs
    }
    fn system_index(&self, text: &str) -> Option<bool> {
        Some(self.found_for.iter().any(|&i| self.texts[i] == text))
    }
}

/// The hits of a query with `si:` terms: what the Windows Search index found for each
/// text, kept when the whole query matches it, in the index's rank order.
pub fn hits(
    q: &Query,
    compiled: &keel_query::Query,
    texts: Vec<String>,
) -> anyhow::Result<Vec<Hit>> {
    let texts = Arc::new(texts);
    let mut order: Vec<PathBuf> = Vec::new();
    let mut found: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for (i, text) in texts.iter().enumerate() {
        for p in search(text, q.within.as_deref(), q.max.max(1).saturating_mul(4))?.iter() {
            let slot = found.entry(p.clone()).or_default();
            if slot.is_empty() {
                order.push(p.clone());
            }
            slot.push(i);
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for p in order {
        if out.len() >= q.max as usize {
            break;
        }
        let Some(item) = Found::stat(&p, found.remove(&p).unwrap_or_default(), texts.clone())
        else {
            continue;
        };
        if (q.folders_only && !item.is_dir) || !seen.insert(item.path.clone()) {
            continue;
        }
        let runs = crate::runs::info(&p);
        let props = WithRuns {
            item: &item,
            count: runs.map(|r| r.count),
            last_run: runs.map(|r| r.last_run),
        };
        if !compiled.matches(&props) {
            continue;
        }
        out.push(Hit {
            path: keel_vfs::VPath::local(&p),
            is_dir: item.is_dir,
            size: item.size,
            modified: item.modified.map(|ms| {
                std::time::UNIX_EPOCH + Duration::from_millis(u64::try_from(ms).unwrap_or(0))
            }),
            snippet: None,
        });
    }
    Ok(out)
}

/// A backend with `si:` terms answered through the Windows Search index; every other
/// query (and everything else) goes to the backend as is.
pub struct SystemIndexed {
    inner: Box<dyn Searcher>,
}

impl SystemIndexed {
    pub fn new(inner: Box<dyn Searcher>) -> Self {
        Self { inner }
    }
}

/// The `si:` texts of `q` and the compiled query, or `None` when it has none (or is a
/// regex, or does not parse: the backend reports that).
pub fn system_index_query(q: &Query) -> Option<(keel_query::Query, Vec<String>)> {
    if q.regex || !q.text.to_ascii_lowercase().contains("si") {
        return None;
    }
    let compiled = keel_query::parse(&q.text, &query_options(q)).ok()?;
    let texts = compiled.system_index_texts();
    (!texts.is_empty()).then_some((compiled, texts))
}

impl Searcher for SystemIndexed {
    fn query(&self, q: &Query) -> anyhow::Result<Vec<Hit>> {
        match system_index_query(q) {
            Some((compiled, texts)) => hits(q, &compiled, texts),
            None => self.inner.query(q),
        }
    }
    fn available(&self) -> bool {
        self.inner.available()
    }
    fn probe(&self) -> bool {
        self.inner.probe()
    }
    fn status(&self) -> Option<String> {
        self.inner.status()
    }
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn state(&self) -> SearchState {
        self.inner.state()
    }
    fn take_ready(&self) -> bool {
        self.inner.take_ready()
    }
    fn offline_roots(&self) -> Vec<PathBuf> {
        self.inner.offline_roots()
    }
    fn set_index_settings(&self, settings: &IndexSettings) {
        self.inner.set_index_settings(settings)
    }
    fn index_settings_changed(&self) -> Option<IndexSettings> {
        self.inner.index_settings_changed()
    }
    fn covers(&self, dir: &Path) -> bool {
        self.inner.covers(dir)
    }
}

/// OLE DB over the Windows Search provider.
#[cfg(windows)]
mod oledb {
    use std::path::PathBuf;

    use windows::core::{IUnknown, Interface, GUID, HSTRING, PCWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Search::{
        IAccessor, ICommandText, IDBCreateCommand, IDBCreateSession, IDBInitialize,
        IDataInitialize, IRowset, DBACCESSOR_ROWDATA, DBBINDING, DBMEMOWNER_CLIENTOWNED,
        DBPARAMIO_NOTPARAM, DBPART_LENGTH, DBPART_STATUS, DBPART_VALUE, DBTYPE_WSTR,
        DB_S_ENDOFROWSET, HACCESSOR,
    };

    /// `CLSID_MSDAINITIALIZE`, the OLE DB service component that opens a provider from
    /// a connection string.
    const CLSID_MSDAINITIALIZE: GUID = GUID::from_u128(0x2206cdb0_19c1_11d1_89e0_00c04fd7a829);
    /// `DBGUID_DEFAULT`, the provider's own SQL dialect.
    const DBGUID_DEFAULT: GUID = GUID::from_u128(0xc8b521fb_5cf3_11ce_ade5_00aa0044773d);
    const CONNECTION: &str =
        "Provider=Search.CollatorDSO;Extended Properties='Application=Windows';";
    /// Bytes for one path (UTF-16), with the status and length before it.
    const VALUE_BYTES: usize = 32 * 1024 * 2;
    const STATUS_AT: usize = 0;
    const LENGTH_AT: usize = 8;
    const VALUE_AT: usize = 16;
    const ROW_BYTES: usize = VALUE_AT + VALUE_BYTES + 2;
    /// `DBSTATUS_S_OK` and `DBSTATUS_S_TRUNCATED`.
    const STATUS_OK: u32 = 0;
    const STATUS_TRUNCATED: u32 = 1;
    const ROWS_PER_FETCH: usize = 64;

    struct Com(bool);

    impl Drop for Com {
        fn drop(&mut self) {
            if self.0 {
                // SAFETY: matches the successful CoInitializeEx in `query`.
                unsafe { CoUninitialize() };
            }
        }
    }

    /// The first column (a path) of every row `sql` returns.
    pub fn query(sql: &str) -> anyhow::Result<Vec<PathBuf>> {
        // SAFETY: COM for this worker thread; balanced by `Com`'s drop.
        let _com = Com(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok());
        // SAFETY: plain COM calls with valid arguments; every interface is released on
        // drop, the accessor and rows explicitly below.
        unsafe {
            let init: IDataInitialize =
                CoCreateInstance(&CLSID_MSDAINITIALIZE, None, CLSCTX_INPROC_SERVER)?;
            let mut source: Option<IUnknown> = None;
            let connection = HSTRING::from(CONNECTION);
            init.GetDataSource(
                None,
                CLSCTX_INPROC_SERVER.0,
                PCWSTR(connection.as_ptr()),
                &IDBInitialize::IID,
                &mut source,
            )?;
            let source: IDBInitialize = source
                .ok_or_else(|| anyhow::anyhow!("the Windows Search provider did not open"))?
                .cast()?;
            source.Initialize()?;
            let session: IDBCreateCommand = source
                .cast::<IDBCreateSession>()?
                .CreateSession(None, &IDBCreateCommand::IID)?
                .cast()?;
            let command: ICommandText = session.CreateCommand(None, &ICommandText::IID)?.cast()?;
            let text = HSTRING::from(sql);
            command.SetCommandText(&DBGUID_DEFAULT, PCWSTR(text.as_ptr()))?;
            let mut rows: Option<IUnknown> = None;
            command.Execute(
                None,
                &IRowset::IID,
                None,
                None,
                Some(&mut rows as *mut Option<IUnknown>),
            )?;
            let Some(rows) = rows else {
                return Ok(Vec::new());
            };
            let rowset: IRowset = rows.cast()?;
            read_paths(&rowset)
        }
    }

    /// Reads column 1 of every row as text.
    unsafe fn read_paths(rowset: &IRowset) -> anyhow::Result<Vec<PathBuf>> {
        let accessor: IAccessor = rowset.cast()?;
        let binding = DBBINDING {
            iOrdinal: 1,
            obValue: VALUE_AT,
            obLength: LENGTH_AT,
            obStatus: STATUS_AT,
            dwPart: (DBPART_VALUE.0 | DBPART_LENGTH.0 | DBPART_STATUS.0) as u32,
            dwMemOwner: DBMEMOWNER_CLIENTOWNED.0 as u32,
            eParamIO: DBPARAMIO_NOTPARAM.0 as u32,
            cbMaxLen: VALUE_BYTES,
            wType: DBTYPE_WSTR.0 as u16,
            ..Default::default()
        };
        let mut handle = HACCESSOR::default();
        accessor.CreateAccessor(
            DBACCESSOR_ROWDATA.0 as u32,
            1,
            &binding,
            0,
            &mut handle,
            None,
        )?;
        let mut out = Vec::new();
        let mut buf = vec![0u8; ROW_BYTES];
        let result = (|| -> anyhow::Result<()> {
            loop {
                let mut hrows = [0usize; ROWS_PER_FETCH];
                let mut got = 0usize;
                let mut ptr = hrows.as_mut_ptr();
                // The binding's wrapper takes the row count from a slice of pointers;
                // call the method itself with a buffer of ROWS_PER_FETCH handles.
                let hr = (Interface::vtable(rowset).GetNextRows)(
                    Interface::as_raw(rowset),
                    0,
                    0,
                    ROWS_PER_FETCH as isize,
                    &mut got,
                    &mut ptr,
                );
                if hr.is_err() {
                    return Err(windows::core::Error::from(hr).into());
                }
                for &row in &hrows[..got.min(ROWS_PER_FETCH)] {
                    buf.fill(0);
                    if rowset.GetData(row, handle, buf.as_mut_ptr().cast()).is_ok() {
                        if let Some(p) = path_of(&buf) {
                            out.push(p);
                        }
                    }
                }
                if got > 0 {
                    let _ = rowset.ReleaseRows(
                        got,
                        hrows.as_ptr(),
                        std::ptr::null(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    );
                }
                if hr == DB_S_ENDOFROWSET || got == 0 {
                    return Ok(());
                }
            }
        })();
        let _ = accessor.ReleaseAccessor(handle, None);
        result.map(|()| out)
    }

    /// The path in a row buffer, when its status says it is there.
    fn path_of(buf: &[u8]) -> Option<PathBuf> {
        let status = u32::from_ne_bytes(buf[STATUS_AT..STATUS_AT + 4].try_into().ok()?);
        if status != STATUS_OK && status != STATUS_TRUNCATED {
            return None;
        }
        let len = usize::from_ne_bytes(buf[LENGTH_AT..LENGTH_AT + 8].try_into().ok()?);
        let len = len.min(VALUE_BYTES) / 2;
        let wide: Vec<u16> = buf[VALUE_AT..VALUE_AT + len * 2]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_ne_bytes(*c))
            .collect();
        let text = String::from_utf16_lossy(&wide);
        (!text.is_empty()).then(|| PathBuf::from(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_drop_quotes_and_bare_wildcards() {
        assert_eq!(
            words("  budget  \"q3 report\" * ?? x*"),
            ["budget", "q3", "report", "x*"]
        );
        assert!(words("\"\" *").is_empty());
    }

    #[test]
    fn the_sql_escapes_quotes_wildcards_and_brackets() {
        let sql = build_sql("o'brien 100%_[draft]", None, 50).unwrap();
        assert_eq!(
            sql,
            "SELECT TOP 50 System.ItemPathDisplay FROM SystemIndex WHERE SCOPE='file:' \
             AND (CONTAINS(*, '\"o''brien*\"') OR System.FileName LIKE '%o''brien%') \
             AND (CONTAINS(*, '\"100%_[draft]*\"') OR System.FileName LIKE '%100[%][_][[]draft]%') \
             ORDER BY System.Search.Rank DESC"
        );
        // Everything's wildcards: the LIKE side keeps them, CONTAINS gets a prefix.
        let sql = build_sql("rep*rt?", None, 1).unwrap();
        assert!(sql.contains("CONTAINS(*, '\"report*\"')"), "{sql}");
        assert!(sql.contains("LIKE '%rep%rt_%'"), "{sql}");
        // A quote can never end the literal early: every `'` comes doubled, so what
        // the text says stays inside the literals.
        let sql = build_sql("a') OR 1=1 --", None, 10).unwrap();
        let mut outside = String::new();
        let mut in_literal = false;
        let mut chars = sql.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\'' {
                if in_literal && chars.peek() == Some(&'\'') {
                    chars.next();
                } else {
                    in_literal = !in_literal;
                }
            } else if !in_literal {
                outside.push(c);
            }
        }
        assert!(!in_literal, "{sql}");
        assert!(
            !outside.contains("1=1") && !outside.contains("--"),
            "{outside}"
        );
        assert!(sql.contains("CONTAINS(*, '\"a'')*\"')"), "{sql}");
    }

    #[test]
    fn the_sql_is_bounded_and_scoped() {
        let sql = build_sql("x", Some(Path::new(r"C:\Users\Me's\Docs\")), 0).unwrap();
        assert!(sql.starts_with("SELECT TOP 1 "), "{sql}");
        assert!(sql.contains("SCOPE='file:C:/Users/Me''s/Docs'"), "{sql}");
        let sql = build_sql("x", None, u32::MAX).unwrap();
        assert!(
            sql.starts_with(&format!("SELECT TOP {MAX_RESULTS} ")),
            "{sql}"
        );
        assert_eq!(build_sql("  \"\"  ", None, 10), None);
    }

    #[test]
    fn only_queries_with_si_terms_go_to_the_system_index() {
        let q = |text: &str| Query {
            text: text.into(),
            ..Query::default()
        };
        assert!(system_index_query(&q("report")).is_none());
        assert!(system_index_query(&q("size:>1mb")).is_none());
        let (_, texts) = system_index_query(&q("si:budget ext:xlsx")).unwrap();
        assert_eq!(texts, ["budget"]);
        let mut regex = q("si:budget");
        regex.regex = true;
        assert!(system_index_query(&regex).is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn si_is_windows_only() {
        let q = Query {
            text: "si:budget".into(),
            ..Query::default()
        };
        let err = SystemIndexed::new(Box::new(crate::Unavailable::new("x")))
            .query(&q)
            .unwrap_err();
        assert!(err.to_string().contains("Windows"), "{err}");
    }

    /// The real provider, when this machine runs Windows Search (CI images often do not):
    /// a query either answers or fails cleanly, within the timeout.
    #[cfg(windows)]
    #[test]
    fn the_windows_search_provider_answers_or_fails_cleanly() {
        let started = Instant::now();
        let sql = build_sql("keel-no-such-word-for-tests", None, 5).unwrap();
        match run_sql(&sql) {
            Ok(rows) => assert!(rows.len() <= 5, "{rows:?}"),
            Err(e) => eprintln!("Windows Search unavailable here: {e:#}"),
        }
        assert!(started.elapsed() <= TIMEOUT + Duration::from_secs(2));
    }
}
