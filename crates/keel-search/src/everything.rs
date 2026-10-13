use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use libloading::os::windows::{
    Library as WinLibrary, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use libloading::Library;
use parking_lot::Mutex;

use crate::{Hit, Query, Searcher};

const DLL_NAME: &str = "Everything64.dll";
const EVERYTHING_REQUEST_FULL_PATH_AND_FILE_NAME: u32 = 0x4;
const EVERYTHING_REQUEST_SIZE: u32 = 0x10;
const EVERYTHING_REQUEST_DATE_MODIFIED: u32 = 0x40;
const EVERYTHING_ERROR_IPC: u32 = 2;
const WINDOWS_TO_UNIX_EPOCH_100NS: u64 = 116_444_736_000_000_000;

/// The SDK keeps its search state in process-wide globals, so every instance
/// shares this one lock for a whole query (set search, query, read results).
static EVERYTHING_LOCK: Mutex<()> = Mutex::new(());

type SetSearchW = unsafe extern "system" fn(*const u16);
type SetU32 = unsafe extern "system" fn(u32);
type SetBool = unsafe extern "system" fn(i32);
type QueryW = unsafe extern "system" fn(i32) -> i32;
type GetU32 = unsafe extern "system" fn() -> u32;
type GetResultFullPathNameW = unsafe extern "system" fn(u32, *mut u16, u32) -> u32;
type IsFolderResult = unsafe extern "system" fn(u32) -> i32;
type GetResultSize = unsafe extern "system" fn(u32, *mut i64) -> i32;
type GetResultDateModified = unsafe extern "system" fn(u32, *mut u64) -> i32;

#[derive(Clone, Copy)]
struct Functions {
    set_search: SetSearchW,
    set_request_flags: SetU32,
    set_max: SetU32,
    set_match_case: SetBool,
    set_match_path: SetBool,
    set_match_whole_word: SetBool,
    set_regex: SetBool,
    query: QueryW,
    get_num_results: GetU32,
    get_result_full_path_name: GetResultFullPathNameW,
    is_folder_result: IsFolderResult,
    get_result_size: GetResultSize,
    get_result_date_modified: GetResultDateModified,
    get_last_error: GetU32,
    get_major_version: GetU32,
}

/// Searcher backed by the Everything SDK DLL and Everything.exe IPC.
pub struct EverythingSearcher {
    // Function pointers are valid only while the library remains loaded.
    _library: Library,
    functions: Functions,
    available: AtomicBool,
}

impl EverythingSearcher {
    /// Loads Everything64.dll from beside the executable, then from the first
    /// absolute `PATH` entry that has it; the current directory is never
    /// searched. Probes Everything.exe once over IPC, so call it off the UI
    /// thread.
    pub fn load() -> anyhow::Result<Self> {
        let beside_exe = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        let path_dirs = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .unwrap_or_default();
        let dll = beside_exe
            .into_iter()
            .chain(path_dirs)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(DLL_NAME))
            .find(|candidate| candidate.is_file())
            .ok_or_else(|| anyhow!("{DLL_NAME} not found beside the executable or on PATH"))?;
        Self::load_from(dll)
    }

    /// Loads an explicitly located Everything SDK DLL. A relative path is made
    /// absolute against the current directory, so pass an absolute one.
    pub fn load_from(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = std::path::absolute(path.as_ref())
            .with_context(|| format!("invalid DLL path {}", path.as_ref().display()))?;
        // SAFETY: the library is retained by `Self`, and every requested symbol
        // has the signature documented by the Everything SDK. The DLL's own
        // imports resolve only from its directory and System32.
        let library: Library = unsafe {
            WinLibrary::load_with_flags(
                &path,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        }
        .with_context(|| format!("failed to load {}", path.display()))?
        .into();
        let functions = unsafe { Self::load_functions(&library)? };

        let searcher = Self {
            _library: library,
            functions,
            available: AtomicBool::new(false),
        };
        searcher.probe();
        Ok(searcher)
    }

    unsafe fn load_functions(library: &Library) -> anyhow::Result<Functions> {
        macro_rules! symbol {
            ($name:literal, $ty:ty) => {
                *library
                    .get::<$ty>(concat!($name, "\0").as_bytes())
                    .with_context(|| concat!("missing Everything SDK symbol ", $name))?
            };
        }

        Ok(Functions {
            set_search: symbol!("Everything_SetSearchW", SetSearchW),
            set_request_flags: symbol!("Everything_SetRequestFlags", SetU32),
            set_max: symbol!("Everything_SetMax", SetU32),
            set_match_case: symbol!("Everything_SetMatchCase", SetBool),
            set_match_path: symbol!("Everything_SetMatchPath", SetBool),
            set_match_whole_word: symbol!("Everything_SetMatchWholeWord", SetBool),
            set_regex: symbol!("Everything_SetRegex", SetBool),
            query: symbol!("Everything_QueryW", QueryW),
            get_num_results: symbol!("Everything_GetNumResults", GetU32),
            get_result_full_path_name: symbol!(
                "Everything_GetResultFullPathNameW",
                GetResultFullPathNameW
            ),
            is_folder_result: symbol!("Everything_IsFolderResult", IsFolderResult),
            get_result_size: symbol!("Everything_GetResultSize", GetResultSize),
            get_result_date_modified: symbol!(
                "Everything_GetResultDateModified",
                GetResultDateModified
            ),
            get_last_error: symbol!("Everything_GetLastError", GetU32),
            get_major_version: symbol!("Everything_GetMajorVersion", GetU32),
        })
    }

    /// Asks Everything.exe over IPC whether it is running and refreshes
    /// [`Searcher::available`]. Blocks on IPC: call from worker threads only.
    pub fn probe(&self) -> bool {
        let _guard = EVERYTHING_LOCK.lock();
        // SAFETY: serialized by EVERYTHING_LOCK; takes no arguments.
        let alive = unsafe { (self.functions.get_major_version)() } != 0;
        self.available.store(alive, Ordering::Relaxed);
        alive
    }
}

/// Everything search string and whether whole-string regex mode is on.
/// `regex:` scopes the regex to the text so a `folder:` prefix stays a filter. Match
/// Case, Match Path, Match Whole Words and Regex are Everything's own search flags (set
/// with the SDK, the same switches as its Search menu); Match Diacritics has no SDK
/// flag, so it is Everything's `diacritics:` modifier over the whole search as a group.
/// Keel's "Match whole filename when using wildcards" is Everything's own setting there.
/// Keel's `in:<folder>` (outside regex mode) or a pane's folder (`Query::within`) becomes
/// Everything's path term `nocase:"<folder>\"` (a term with a backslash matches the full
/// path; Match Case is for the query, never the folder); a regex then follows it quoted,
/// as `regex:"…"`, so its `|`, `<`, `!` and spaces are not Everything's operators. A regex
/// holding a `"` cannot be quoted: it stays whole-string and the hits are kept to the
/// folder afterwards ([`kept_to`]).
fn search_text(query: &Query) -> (String, bool) {
    if kept_to(query).is_some() {
        return (query.text.clone(), true);
    }
    let mut regex = query.regex;
    let text = match crate::scope(query) {
        (Some(folder), rest) => {
            let folder = folder.display().to_string();
            let sep = if folder.ends_with(['\\', '/']) {
                ""
            } else {
                "\\"
            };
            let rest = match std::mem::take(&mut regex) {
                true if !rest.is_empty() => format!("regex:\"{rest}\""),
                _ => rest,
            };
            format!("nocase:\"{folder}{sep}\" {rest}")
                .trim_end()
                .to_owned()
        }
        _ => query.text.clone(),
    };
    let text = if query.match_diacritics && !query.regex && !text.trim().is_empty() {
        format!("diacritics:<{text}>")
    } else {
        text
    };
    match (query.folders_only, regex) {
        (true, true) => (format!("folder: regex:{text}"), false),
        (true, false) => (format!("folder: {text}"), false),
        (false, regex) => (text, regex),
    }
}

/// How many more hits to ask Everything for when it cannot keep to the folder itself.
const OVERFETCH: u32 = 10;

/// The pane's folder when the query is a regex holding a `"` (no `regex:"…"` form): the
/// hits are filtered to it after the query, which asks for [`OVERFETCH`] times as many.
// ponytail: a folder with fewer than 1 in OVERFETCH of the regex's matches shows fewer
// hits than it has; a page loop would fix that if it matters.
fn kept_to(query: &Query) -> Option<std::path::PathBuf> {
    match crate::scope(query) {
        (Some(folder), rest) if query.regex && rest.contains('"') => Some(folder),
        _ => None,
    }
}

impl Searcher for EverythingSearcher {
    fn name(&self) -> &'static str {
        "Everything"
    }

    fn query(&self, query: &Query) -> anyhow::Result<Vec<Hit>> {
        let functions = self.functions;
        let (text, regex) = search_text(query);
        let keep = kept_to(query);
        let max = match keep {
            Some(_) => query.max.saturating_mul(OVERFETCH),
            None => query.max,
        };
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();

        let _guard = EVERYTHING_LOCK.lock();
        // SAFETY: all calls are serialized by EVERYTHING_LOCK, strings are
        // NUL-terminated, and output buffers outlive each SDK call.
        unsafe {
            (functions.set_search)(wide.as_ptr());
            (functions.set_request_flags)(
                EVERYTHING_REQUEST_FULL_PATH_AND_FILE_NAME
                    | EVERYTHING_REQUEST_SIZE
                    | EVERYTHING_REQUEST_DATE_MODIFIED,
            );
            (functions.set_max)(max);
            (functions.set_match_case)(i32::from(query.match_case));
            (functions.set_match_path)(i32::from(query.match_path));
            (functions.set_match_whole_word)(i32::from(query.whole_word));
            (functions.set_regex)(i32::from(regex));

            if (functions.query)(1) == 0 {
                let error = (functions.get_last_error)();
                if error == EVERYTHING_ERROR_IPC {
                    self.available.store(false, Ordering::Relaxed);
                    return Err(anyhow!("Everything is not running"));
                }
                return Err(anyhow!("Everything query failed with error {error}"));
            }
            self.available.store(true, Ordering::Relaxed);

            let count = (functions.get_num_results)();
            let mut hits = Vec::with_capacity(count as usize);
            let mut buffer = vec![0_u16; 32_768];
            for index in 0..count {
                let path = result_path(functions, index, &mut buffer)?;
                let is_dir = (functions.is_folder_result)(index) != 0;
                if let Some(dir) = &keep {
                    if !crate::is_under(Path::new(&path), dir) || (query.folders_only && !is_dir) {
                        continue;
                    }
                }

                // Folders report -1.
                let mut raw_size = 0_i64;
                let size = if (functions.get_result_size)(index, &mut raw_size) != 0 {
                    raw_size.max(0) as u64
                } else {
                    0
                };

                let mut filetime = 0_u64;
                let modified = if (functions.get_result_date_modified)(index, &mut filetime) != 0 {
                    filetime_to_system_time(filetime)
                } else {
                    None
                };

                hits.push(Hit {
                    path: keel_vfs::VPath::local(path),
                    is_dir,
                    size,
                    modified,
                    snippet: None,
                });
            }
            hits.truncate(query.max as usize);
            Ok(hits)
        }
    }

    /// Last known IPC state from `load`, `probe` or the last query. Never
    /// blocks, so it is safe on the UI thread.
    fn available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }

    fn probe(&self) -> bool {
        EverythingSearcher::probe(self)
    }
}

unsafe fn result_path(
    functions: Functions,
    index: u32,
    buffer: &mut Vec<u16>,
) -> anyhow::Result<String> {
    let mut length =
        (functions.get_result_full_path_name)(index, buffer.as_mut_ptr(), buffer.len() as u32)
            as usize;
    if length == 0 {
        return Err(anyhow!(
            "Everything returned an empty path for result {index}"
        ));
    }
    if length >= buffer.len() {
        buffer.resize(length + 1, 0);
        length =
            (functions.get_result_full_path_name)(index, buffer.as_mut_ptr(), buffer.len() as u32)
                as usize;
    }
    Ok(String::from_utf16_lossy(
        &buffer[..length.min(buffer.len())],
    ))
}

pub(crate) fn filetime_to_system_time(filetime: u64) -> Option<std::time::SystemTime> {
    let unix_ticks = filetime.checked_sub(WINDOWS_TO_UNIX_EPOCH_100NS)?;
    UNIX_EPOCH.checked_add(Duration::from_nanos(unix_ticks.checked_mul(100)?))
}

#[cfg(test)]
mod tests {
    use super::search_text;
    use crate::Query;

    fn text(folders_only: bool, regex: bool) -> (String, bool) {
        search_text(&Query {
            text: "^ab+c$".into(),
            folders_only,
            regex,
            ..Query::default()
        })
    }

    #[test]
    fn regex_and_folder_filters_compose() {
        assert_eq!(text(true, true), ("folder: regex:^ab+c$".into(), false));
        assert_eq!(text(true, false), ("folder: ^ab+c$".into(), false));
        assert_eq!(text(false, true), ("^ab+c$".into(), true));
        assert_eq!(text(false, false), ("^ab+c$".into(), false));
    }

    #[test]
    fn match_diacritics_is_a_modifier_over_the_whole_search() {
        let q = |text: &str, folders_only, regex| {
            search_text(&Query {
                text: text.into(),
                folders_only,
                regex,
                match_diacritics: true,
                ..Query::default()
            })
        };
        assert_eq!(
            q("café ext:pdf", false, false),
            ("diacritics:<café ext:pdf>".into(), false)
        );
        assert_eq!(
            q("café", true, false),
            ("folder: diacritics:<café>".into(), false)
        );
        // A regex has no accents to fold; an empty search stays empty.
        assert_eq!(q("^caf", false, true), ("^caf".into(), true));
        assert_eq!(q("", false, false), ("".into(), false));
    }

    #[test]
    fn in_becomes_a_path_term() {
        let text = |text: &str, regex: bool| {
            search_text(&Query {
                text: text.into(),
                regex,
                ..Query::default()
            })
            .0
        };
        let spaced = r#"nocase:"C:\a b\" report"#;
        assert_eq!(text(r#"in:"C:\a b" report"#, false), spaced);
        assert_eq!(text(r"report in:D:\", false), r#"nocase:"D:\" report"#);
        assert_eq!(text(r"in:C:\x", false), r#"nocase:"C:\x\""#);
        // A regex is the whole text.
        assert_eq!(text(r"in:C:\x", true), r"in:C:\x");
    }

    #[test]
    fn a_pane_folder_is_a_path_term() {
        let within = |text: &str, regex, folders_only| {
            search_text(&Query {
                text: text.into(),
                regex,
                folders_only,
                within: Some(r"C:\Users\ann".into()),
                ..Query::default()
            })
        };
        let plain = (r#"nocase:"C:\Users\ann\" ^ab+c$"#.to_owned(), false);
        assert_eq!(within("^ab+c$", false, false), plain);
        // A regex keeps to the text, after the folder, quoted: `|`, `<` and spaces in it
        // are not Everything's operators.
        let regex = r#"nocase:"C:\Users\ann\" regex:"^(foo|bar) <x>\.rs$""#;
        assert_eq!(
            within(r"^(foo|bar) <x>\.rs$", true, false),
            (regex.to_owned(), false)
        );
        let folders = (
            r#"folder: nocase:"C:\Users\ann\" regex:"^a""#.to_owned(),
            false,
        );
        assert_eq!(within("^a", true, true), folders);
        // A typed in: takes the pane's folder's place.
        let typed = (r#"nocase:"D:\x\" report"#.to_owned(), false);
        assert_eq!(within(r"report in:D:\x", false, false), typed);
        // A regex with a `"` cannot be quoted: whole-string regex, kept to the folder after.
        let quote = r#"^say "hi"$"#;
        assert_eq!(within(quote, true, false), (quote.to_owned(), true));
        let q = Query {
            text: quote.into(),
            regex: true,
            within: Some(r"C:\Users\ann".into()),
            ..Query::default()
        };
        assert_eq!(super::kept_to(&q), Some(r"C:\Users\ann".into()));
        assert_eq!(super::kept_to(&Query { regex: false, ..q }), None);
    }
}
