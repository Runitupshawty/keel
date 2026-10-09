use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use libloading::Library;
use parking_lot::Mutex;

use crate::{Hit, Query, Searcher};

const EVERYTHING_REQUEST_FULL_PATH_AND_FILE_NAME: u32 = 0x4;
const EVERYTHING_REQUEST_SIZE: u32 = 0x10;
const EVERYTHING_REQUEST_DATE_MODIFIED: u32 = 0x40;
const EVERYTHING_ERROR_IPC: u32 = 2;
const WINDOWS_TO_UNIX_EPOCH_100NS: u64 = 116_444_736_000_000_000;

type SetSearchW = unsafe extern "system" fn(*const u16);
type SetRequestFlags = unsafe extern "system" fn(u32);
type SetMax = unsafe extern "system" fn(u32);
type SetBool = unsafe extern "system" fn(i32);
type QueryW = unsafe extern "system" fn(i32) -> i32;
type GetNumResults = unsafe extern "system" fn() -> u32;
type GetResultFullPathNameW = unsafe extern "system" fn(u32, *mut u16, u32) -> u32;
type IsFolderResult = unsafe extern "system" fn(u32) -> i32;
type GetResultSize = unsafe extern "system" fn(u32, *mut i64) -> i32;
type GetResultDateModified = unsafe extern "system" fn(u32, *mut u64) -> i32;
type GetLastError = unsafe extern "system" fn() -> u32;

#[derive(Clone, Copy)]
struct Functions {
    set_search: SetSearchW,
    set_request_flags: SetRequestFlags,
    set_max: SetMax,
    set_match_case: SetBool,
    set_regex: SetBool,
    query: QueryW,
    get_num_results: GetNumResults,
    get_result_full_path_name: GetResultFullPathNameW,
    is_folder_result: IsFolderResult,
    get_result_size: GetResultSize,
    get_result_date_modified: GetResultDateModified,
    get_last_error: GetLastError,
}

struct State {
    functions: Functions,
    available: bool,
}

/// Searcher backed by the Everything SDK DLL and Everything.exe IPC.
pub struct EverythingSearcher {
    // Function pointers are valid only while the library remains loaded.
    _library: Library,
    state: Mutex<State>,
}

impl EverythingSearcher {
    /// Loads Everything64.dll from beside the executable, then via the normal
    /// Windows DLL search path.
    pub fn load() -> anyhow::Result<Self> {
        if let Ok(executable) = std::env::current_exe() {
            if let Some(directory) = executable.parent() {
                let beside_executable = directory.join("Everything64.dll");
                if beside_executable.is_file() {
                    return Self::load_from(beside_executable);
                }
            }
        }

        Self::load_from("Everything64.dll")
    }

    /// Loads an explicitly located Everything SDK DLL.
    pub fn load_from(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        // SAFETY: the library is retained by `Self`, and every requested symbol
        // has the signature documented by the Everything SDK.
        let library = unsafe { Library::new(path) }
            .with_context(|| format!("failed to load {}", path.display()))?;
        let functions = unsafe { Self::load_functions(&library)? };

        Ok(Self {
            _library: library,
            state: Mutex::new(State {
                functions,
                available: true,
            }),
        })
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
            set_request_flags: symbol!("Everything_SetRequestFlags", SetRequestFlags),
            set_max: symbol!("Everything_SetMax", SetMax),
            set_match_case: symbol!("Everything_SetMatchCase", SetBool),
            set_regex: symbol!("Everything_SetRegex", SetBool),
            query: symbol!("Everything_QueryW", QueryW),
            get_num_results: symbol!("Everything_GetNumResults", GetNumResults),
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
            get_last_error: symbol!("Everything_GetLastError", GetLastError),
        })
    }
}

impl Searcher for EverythingSearcher {
    fn query(&self, query: &Query) -> anyhow::Result<Vec<Hit>> {
        let mut state = self.state.lock();
        let functions = state.functions;
        let text = if query.folders_only {
            format!("folder: {}", query.text)
        } else {
            query.text.clone()
        };
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();

        // SAFETY: all calls are serialized, strings are NUL-terminated, and
        // output buffers remain valid for the duration of each SDK call.
        unsafe {
            (functions.set_search)(wide.as_ptr());
            (functions.set_request_flags)(
                EVERYTHING_REQUEST_FULL_PATH_AND_FILE_NAME
                    | EVERYTHING_REQUEST_SIZE
                    | EVERYTHING_REQUEST_DATE_MODIFIED,
            );
            (functions.set_max)(query.max);
            (functions.set_match_case)(i32::from(query.match_case));
            (functions.set_regex)(i32::from(query.regex));

            if (functions.query)(1) == 0 {
                let error = (functions.get_last_error)();
                if error == EVERYTHING_ERROR_IPC {
                    state.available = false;
                    return Err(anyhow!("Everything is not running"));
                }
                return Err(anyhow!("Everything query failed with error {error}"));
            }

            state.available = true;
            let count = (functions.get_num_results)();
            let mut hits = Vec::with_capacity(count as usize);
            for index in 0..count {
                let path = result_path(functions, index)?;
                let is_dir = (functions.is_folder_result)(index) != 0;

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

    fn available(&self) -> bool {
        let mut state = self.state.lock();
        let functions = state.functions;
        let empty = [0_u16];

        // Probe IPC instead of only reporting the outcome of the last query;
        // the app uses this before it submits its first search.
        unsafe {
            (functions.set_search)(empty.as_ptr());
            (functions.set_max)(1);
            if (functions.query)(1) != 0 {
                state.available = true;
            } else {
                state.available = (functions.get_last_error)() != EVERYTHING_ERROR_IPC;
            }
        }
        state.available
    }
}

unsafe fn result_path(functions: Functions, index: u32) -> anyhow::Result<String> {
    let mut buffer = vec![0_u16; 32_768];
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

fn filetime_to_system_time(filetime: u64) -> Option<std::time::SystemTime> {
    let unix_ticks = filetime.checked_sub(WINDOWS_TO_UNIX_EPOCH_100NS)?;
    UNIX_EPOCH.checked_add(Duration::from_nanos(unix_ticks.checked_mul(100)?))
}

/// A soft-failure search backend used when Everything cannot be loaded.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unavailable;

impl Searcher for Unavailable {
    fn query(&self, _query: &Query) -> anyhow::Result<Vec<Hit>> {
        Err(anyhow!("Everything is not running"))
    }

    fn available(&self) -> bool {
        false
    }
}
