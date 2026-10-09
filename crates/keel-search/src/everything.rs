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
/// `regex:` scopes the regex to the text so a `folder:` prefix stays a filter.
fn search_text(query: &Query) -> (String, bool) {
    match (query.folders_only, query.regex) {
        (true, true) => (format!("folder: regex:{}", query.text), false),
        (true, false) => (format!("folder: {}", query.text), false),
        (false, regex) => (query.text.clone(), regex),
    }
}

impl Searcher for EverythingSearcher {
    fn name(&self) -> &'static str {
        "Everything"
    }

    fn query(&self, query: &Query) -> anyhow::Result<Vec<Hit>> {
        let functions = self.functions;
        let (text, regex) = search_text(query);
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
            (functions.set_max)(query.max);
            (functions.set_match_case)(i32::from(query.match_case));
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
                });
            }
            Ok(hits)
        }
    }

    /// Last known IPC state from `load`, `probe` or the last query. Never
    /// blocks, so it is safe on the UI thread.
    fn available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
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
}
