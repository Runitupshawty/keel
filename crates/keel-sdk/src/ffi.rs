//! The C ABI (`include/keel_sdk.h`) over one process-wide [`Sdk`] talking to the profile's
//! keel-daemon. Booleans are `int` (0 false, anything else true); strings are NUL-ended
//! UTF-8 (`char*`) or UTF-16 (`uint16_t*`, a `wchar_t*` on Windows).
//!
//! String getters copy into the caller's buffer, cut at a character boundary to fit with
//! the NUL, and return the length the whole string needs without the NUL (call with a NULL
//! buffer to learn it); 0 means an error (see `keel_get_last_error`).

use crate::*;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::OnceLock;

/// The process-wide state the C functions share.
pub fn sdk() -> &'static Sdk {
    static SDK: OnceLock<Sdk> = OnceLock::new();
    SDK.get_or_init(|| Sdk::new(daemon_connector()))
}

/// `void (*)(void *user_data, uint32_t error)`: a background query ended with `error`
/// (`KEEL_OK` when its results are ready to read).
pub type KeelQueryCallback = Option<unsafe extern "C" fn(user_data: *mut c_void, error: u32)>;

/// The `INVALID_FILE_ATTRIBUTES` `keel_get_result_attributes` returns on error.
pub const KEEL_INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;

/// A NUL-ended UTF-8 string (NULL: empty); None (and the error set) when not UTF-8.
unsafe fn utf8<'a>(s: *const c_char) -> Option<&'a str> {
    if s.is_null() {
        return Some("");
    }
    match CStr::from_ptr(s).to_str() {
        Ok(s) => Some(s),
        Err(_) => {
            sdk().fail(KEEL_ERROR_INVALIDPARAMETER, "the text is not UTF-8");
            None
        }
    }
}

/// `out` as a buffer of `len` items (None for NULL or 0).
unsafe fn buffer<'a, T>(out: *mut T, len: u32) -> Option<&'a mut [T]> {
    match out.is_null() || len == 0 {
        true => None,
        false => Some(std::slice::from_raw_parts_mut(out, len as usize)),
    }
}

/// Sets the search: words, "phrases", wildcards, or with `keel_set_regex` one regular
/// expression (as Keel's search pane). NULL is the empty search (everything). Returns 0 for
/// text that is not UTF-8 or longer than 4096 bytes.
///
/// # Safety
/// `search` is NULL or a NUL-ended string.
#[no_mangle]
pub unsafe extern "C" fn keel_set_search(search: *const c_char) -> c_int {
    match utf8(search) {
        Some(s) => sdk().set_search(s) as c_int,
        None => 0,
    }
}

/// As `keel_set_search`, in UTF-16.
///
/// # Safety
/// `search` is NULL or a NUL-ended UTF-16 string.
#[no_mangle]
pub unsafe extern "C" fn keel_set_search_w(search: *const u16) -> c_int {
    if search.is_null() {
        return sdk().set_search("") as c_int;
    }
    // Never read past the NUL, nor further than the longest search allowed.
    let mut len = 0;
    while *search.add(len) != 0 {
        len += 1;
        if len > MAX_SEARCH {
            sdk().fail(
                KEEL_ERROR_INVALIDPARAMETER,
                format!("the search is longer than {MAX_SEARCH} bytes"),
            );
            return 0;
        }
    }
    match String::from_utf16(std::slice::from_raw_parts(search, len)) {
        Ok(s) => sdk().set_search(&s) as c_int,
        Err(_) => {
            sdk().fail(KEEL_ERROR_INVALIDPARAMETER, "the text is not UTF-16");
            0
        }
    }
}

/// Upper and lower case differ.
#[no_mangle]
pub extern "C" fn keel_set_match_case(on: c_int) {
    sdk().set_match_case(on != 0);
}

/// Words match whole words of the name only.
#[no_mangle]
pub extern "C" fn keel_set_match_whole_word(on: c_int) {
    sdk().set_whole_word(on != 0);
}

/// Words match the full path, not only the name.
#[no_mangle]
pub extern "C" fn keel_set_match_path(on: c_int) {
    sdk().set_match_path(on != 0);
}

/// Accents count (`é` does not match `e`).
#[no_mangle]
pub extern "C" fn keel_set_match_diacritics(on: c_int) {
    sdk().set_match_diacritics(on != 0);
}

/// The search is one regular expression.
#[no_mangle]
pub extern "C" fn keel_set_regex(on: c_int) {
    sdk().set_regex(on != 0);
}

/// Results kept at most (default `0xFFFFFFFF`: all; 0 counts only, see
/// `keel_get_total_results`).
#[no_mangle]
pub extern "C" fn keel_set_max(max: u32) {
    sdk().set_max(max);
}

/// Results skipped first (default 0).
#[no_mangle]
pub extern "C" fn keel_set_offset(offset: u32) {
    sdk().set_offset(offset);
}

/// One of the `KEEL_SORT_*` values; returns 0 (sort unchanged) for another.
#[no_mangle]
pub extern "C" fn keel_set_sort(sort: u32) -> c_int {
    sdk().set_sort(sort) as c_int
}

/// The `KEEL_REQUEST_*` properties the next query's results can be read for (default
/// file name and path).
#[no_mangle]
pub extern "C" fn keel_set_request_flags(flags: u32) {
    sdk().set_request_flags(flags);
}

/// The keel-daemon profile to search through (default `default`); 0 for an invalid name.
///
/// # Safety
/// `profile` is NULL (the default profile) or a NUL-ended string.
#[no_mangle]
pub unsafe extern "C" fn keel_set_profile(profile: *const c_char) -> c_int {
    match utf8(profile) {
        Some("") => sdk().set_profile(keel_api::config::DEFAULT_PROFILE) as c_int,
        Some(p) => sdk().set_profile(p) as c_int,
        None => 0,
    }
}

struct UserData(*mut c_void);
// The caller hands the pointer to the callback's thread.
unsafe impl Send for UserData {}
impl UserData {
    fn get(&self) -> *mut c_void {
        self.0
    }
}

/// Runs the search. `wait` non-zero: returns when it is done, 1 when it succeeded (then
/// `callback`, when given, is also called before returning). `wait` 0: runs on a new thread
/// and returns 1 once it started; `callback` (required) is called on that thread with the
/// result code when it ends. The results replace the previous ones only when the query
/// succeeded and no newer query, `keel_reset` or `keel_cleanup` came in the meantime
/// (`KEEL_ERROR_CANCELLED`).
///
/// # Safety
/// `callback` is NULL or a function safe to call from another thread with `user_data`.
#[no_mangle]
pub unsafe extern "C" fn keel_query(
    wait: c_int,
    callback: KeelQueryCallback,
    user_data: *mut c_void,
) -> c_int {
    let done = callback.map(|cb| {
        let data = UserData(user_data);
        Box::new(move |code: u32| unsafe { cb(data.get(), code) }) as Done
    });
    sdk().query(wait != 0, done) as c_int
}

/// Results of the last query (what `keel_set_max` and `keel_set_offset` let through).
#[no_mangle]
pub extern "C" fn keel_get_num_results() -> u32 {
    sdk().num_results()
}

/// Matches in all, regardless of offset and max (counted over the first 10000 the backend
/// finds).
#[no_mangle]
pub extern "C" fn keel_get_total_results() -> u32 {
    sdk().total_results()
}

/// 1 when result `index` is a folder.
#[no_mangle]
pub extern "C" fn keel_is_folder_result(index: u32) -> c_int {
    sdk().is_folder(index).unwrap_or(false) as c_int
}

unsafe fn text8(value: Option<String>, out: *mut c_char, len: u32) -> u32 {
    match value {
        Some(s) => fill_utf8(&s, buffer(out.cast::<u8>(), len)),
        None => 0,
    }
}

unsafe fn text16(value: Option<String>, out: *mut u16, len: u32) -> u32 {
    match value {
        Some(s) => fill_utf16(&s, buffer(out, len)),
        None => 0,
    }
}

/// Result `index`'s full path in UTF-8 (needs `KEEL_REQUEST_FULL_PATH_AND_FILE_NAME`, or
/// both `KEEL_REQUEST_FILE_NAME` and `KEEL_REQUEST_PATH`). Returns the bytes the path
/// needs without the NUL.
///
/// # Safety
/// `out` is NULL or has room for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn keel_get_result_full_path(index: u32, out: *mut c_char, len: u32) -> u32 {
    text8(sdk().full_path(index), out, len)
}

/// As `keel_get_result_full_path`, in UTF-16 code units.
///
/// # Safety
/// `out` is NULL or has room for `len` UTF-16 units.
#[no_mangle]
pub unsafe extern "C" fn keel_get_result_full_path_w(index: u32, out: *mut u16, len: u32) -> u32 {
    text16(sdk().full_path(index), out, len)
}

/// Result `index`'s name (needs `KEEL_REQUEST_FILE_NAME`).
///
/// # Safety
/// `out` is NULL or has room for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn keel_get_result_file_name(index: u32, out: *mut c_char, len: u32) -> u32 {
    text8(sdk().file_name(index), out, len)
}

/// As `keel_get_result_file_name`, in UTF-16 code units.
///
/// # Safety
/// `out` is NULL or has room for `len` UTF-16 units.
#[no_mangle]
pub unsafe extern "C" fn keel_get_result_file_name_w(index: u32, out: *mut u16, len: u32) -> u32 {
    text16(sdk().file_name(index), out, len)
}

/// The folder result `index` is in (needs `KEEL_REQUEST_PATH`).
///
/// # Safety
/// `out` is NULL or has room for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn keel_get_result_path(index: u32, out: *mut c_char, len: u32) -> u32 {
    text8(sdk().path(index), out, len)
}

/// As `keel_get_result_path`, in UTF-16 code units.
///
/// # Safety
/// `out` is NULL or has room for `len` UTF-16 units.
#[no_mangle]
pub unsafe extern "C" fn keel_get_result_path_w(index: u32, out: *mut u16, len: u32) -> u32 {
    text16(sdk().path(index), out, len)
}

/// Result `index`'s size in bytes into `*size` (0 for a folder; needs
/// `KEEL_REQUEST_SIZE`); 1 on success.
///
/// # Safety
/// `size` points to a writable `uint64_t` (any alignment).
#[no_mangle]
pub unsafe extern "C" fn keel_get_result_size(index: u32, size: *mut u64) -> c_int {
    if size.is_null() {
        sdk().fail(KEEL_ERROR_INVALIDPARAMETER, "size is NULL");
        return 0;
    }
    match sdk().size(index) {
        Some(v) => {
            size.write_unaligned(v);
            1
        }
        None => 0,
    }
}

/// Result `index`'s modified date as a FILETIME (100 ns units since 1601-01-01 UTC; a
/// Windows `FILETIME*` may be passed) into `*filetime`, `0xFFFFFFFFFFFFFFFF` when unknown
/// (needs `KEEL_REQUEST_DATE_MODIFIED`); 1 on success.
///
/// # Safety
/// `filetime` points to 8 writable bytes (any alignment).
#[no_mangle]
pub unsafe extern "C" fn keel_get_result_date_modified(index: u32, filetime: *mut u64) -> c_int {
    if filetime.is_null() {
        sdk().fail(KEEL_ERROR_INVALIDPARAMETER, "filetime is NULL");
        return 0;
    }
    match sdk().date_modified(index) {
        Some(v) => {
            filetime.write_unaligned(v);
            1
        }
        None => 0,
    }
}

/// Result `index`'s `FILE_ATTRIBUTE_*` bits (needs `KEEL_REQUEST_ATTRIBUTES`), or
/// `KEEL_INVALID_FILE_ATTRIBUTES`.
#[no_mangle]
pub extern "C" fn keel_get_result_attributes(index: u32) -> u32 {
    sdk()
        .attributes(index)
        .unwrap_or(KEEL_INVALID_FILE_ATTRIBUTES)
}

/// Unsupported: Keel keeps no run counts. Returns 0 with `KEEL_ERROR_UNSUPPORTED`.
#[no_mangle]
pub extern "C" fn keel_get_result_run_count(_index: u32) -> u32 {
    sdk().run_count_unsupported();
    0
}

/// Unsupported: Keel keeps no run counts. Returns 0 with `KEEL_ERROR_UNSUPPORTED`.
#[no_mangle]
pub extern "C" fn keel_get_run_count(_path: *const c_char) -> u32 {
    sdk().run_count_unsupported();
    0
}

/// Unsupported: Keel keeps no run counts (and the SDK never changes anything). Returns 0
/// with `KEEL_ERROR_UNSUPPORTED`.
#[no_mangle]
pub extern "C" fn keel_inc_run_count(_path: *const c_char) -> u32 {
    sdk().run_count_unsupported();
    0
}

/// The last `KEEL_ERROR_*` code (process-wide, as Everything's); `KEEL_OK` after a query
/// that succeeded.
#[no_mangle]
pub extern "C" fn keel_get_last_error() -> u32 {
    sdk().last_error()
}

/// The last error's message in UTF-8 (empty when there is none); returns its length
/// without the NUL.
///
/// # Safety
/// `out` is NULL or has room for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn keel_get_last_error_message(out: *mut c_char, len: u32) -> u32 {
    fill_utf8(&sdk().last_error_message(), buffer(out.cast::<u8>(), len))
}

/// The default settings (the profile stays), no results, no error.
#[no_mangle]
pub extern "C" fn keel_reset() {
    sdk().reset();
}

/// As `keel_reset`, and the profile back to `default`. A background query still running
/// ends with `KEEL_ERROR_CANCELLED`; wait for its callback before unloading the library.
#[no_mangle]
pub extern "C" fn keel_cleanup() {
    sdk().cleanup();
}
