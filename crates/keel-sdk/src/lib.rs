//! keel-sdk: a C ABI (`keel_sdk.dll`, `libkeel_sdk.so`, `libkeel_sdk.dylib`) for searching
//! every file on this computer with Keel, shaped like Everything's SDK: set the search and
//! its switches, query (blocking, or in the background with a callback), then read the
//! results by index. It talks JSON-RPC to the user's running keel-daemon over the per-user
//! local socket ([`keel_api::client::Client`]) and runs `system.search` only: it never opens,
//! runs, changes or deletes a file.
//!
//! [`Sdk`] holds the state and is what the tests drive (with a mock connection); [`ffi`] is
//! the C ABI over one process-wide [`Sdk`]. The header is `include/keel_sdk.h`.

use keel_api::types::{SystemHit, SystemSearchParams, SystemSearchResult, SystemSort};
use keel_api::{ApiError, Backend};
use parking_lot::Mutex;
use std::sync::Arc;

pub mod ffi;

// Error codes (`keel_get_last_error`). Everything's numbers where the meaning is the same.
/// No error.
pub const KEEL_OK: u32 = 0;
/// Out of memory.
pub const KEEL_ERROR_MEMORY: u32 = 1;
/// keel-daemon is not running for the profile, or the connection to it failed.
pub const KEEL_ERROR_IPC: u32 = 2;
/// The background query's thread could not be started.
pub const KEEL_ERROR_CREATETHREAD: u32 = 5;
/// The result index is out of range.
pub const KEEL_ERROR_INVALIDINDEX: u32 = 6;
/// No query has finished since the last reset (no results to read).
pub const KEEL_ERROR_INVALIDCALL: u32 = 7;
/// The property was not requested with `keel_set_request_flags` before the query.
pub const KEEL_ERROR_INVALIDREQUEST: u32 = 8;
/// A bad argument: a search that is not UTF-8 or UTF-16, too long (over 4096 bytes), an
/// invalid regular expression, an unknown sort, an unknown profile name.
pub const KEEL_ERROR_INVALIDPARAMETER: u32 = 9;
/// Keel has no such data (run counts), or the daemon is too old for `system.search`.
pub const KEEL_ERROR_UNSUPPORTED: u32 = 10;
/// The daemon's search failed (the message says why).
pub const KEEL_ERROR_SEARCH: u32 = 11;
/// The daemon did not answer in time.
pub const KEEL_ERROR_TIMEOUT: u32 = 12;
/// A newer query, `keel_reset` or `keel_cleanup` replaced this query: its results were
/// dropped.
pub const KEEL_ERROR_CANCELLED: u32 = 13;

// Sorts (`keel_set_sort`): Everything's numbers for the four orders Keel has.
pub const KEEL_SORT_NAME_ASCENDING: u32 = 1;
pub const KEEL_SORT_NAME_DESCENDING: u32 = 2;
pub const KEEL_SORT_PATH_ASCENDING: u32 = 3;
pub const KEEL_SORT_PATH_DESCENDING: u32 = 4;
pub const KEEL_SORT_SIZE_ASCENDING: u32 = 5;
pub const KEEL_SORT_SIZE_DESCENDING: u32 = 6;
pub const KEEL_SORT_DATE_MODIFIED_ASCENDING: u32 = 13;
pub const KEEL_SORT_DATE_MODIFIED_DESCENDING: u32 = 14;

// Request flags (`keel_set_request_flags`): Everything's bits.
pub const KEEL_REQUEST_FILE_NAME: u32 = 0x1;
pub const KEEL_REQUEST_PATH: u32 = 0x2;
pub const KEEL_REQUEST_FULL_PATH_AND_FILE_NAME: u32 = 0x4;
pub const KEEL_REQUEST_SIZE: u32 = 0x10;
pub const KEEL_REQUEST_DATE_MODIFIED: u32 = 0x40;
pub const KEEL_REQUEST_ATTRIBUTES: u32 = 0x100;
/// Accepted; Keel keeps no run counts, so the run count getters fail with
/// [`KEEL_ERROR_UNSUPPORTED`].
pub const KEEL_REQUEST_RUN_COUNT: u32 = 0x400;
/// The flags a query starts with (Everything's default).
pub const KEEL_REQUEST_DEFAULT: u32 = KEEL_REQUEST_FILE_NAME | KEEL_REQUEST_PATH;
const KNOWN_FLAGS: u32 = KEEL_REQUEST_FILE_NAME
    | KEEL_REQUEST_PATH
    | KEEL_REQUEST_FULL_PATH_AND_FILE_NAME
    | KEEL_REQUEST_SIZE
    | KEEL_REQUEST_DATE_MODIFIED
    | KEEL_REQUEST_ATTRIBUTES
    | KEEL_REQUEST_RUN_COUNT;

/// Windows file attributes the results carry (`FILE_ATTRIBUTE_*`).
pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// A FILETIME (100 ns since 1601) for a date Keel does not know.
pub const UNKNOWN_FILETIME: u64 = u64::MAX;

/// Longest search (bytes of UTF-8), as `system.search`.
pub const MAX_SEARCH: usize = keel_api::system::MAX_QUERY;
/// Results one `system.search` call returns at most: larger `max` values are fetched in
/// pages of this many.
pub const PAGE: usize = keel_api::system::MAX_COUNT;
/// Calls one query makes at most (the daemon counts at most 10000 matches).
const MAX_PAGES: usize = 64;

/// Opens a JSON-RPC connection to the daemon of a profile.
pub type Connector =
    Arc<dyn Fn(&str) -> Result<Box<dyn Backend + Send>, ApiError> + Send + Sync + 'static>;

/// The real connection: the profile's keel-daemon socket (`HostConfig::socket_name`, from
/// `KEEL_CONFIG_DIR` / `KEEL_DATA_DIR` or the usual folders).
pub fn daemon_connector() -> Connector {
    Arc::new(|profile: &str| {
        let cfg = keel_api::config::HostConfig::load(profile)
            .map_err(|e| ApiError::new(ApiError::INVALID_PARAMS, format!("{e:#}")))?;
        let client = keel_api::client::Client::connect(&cfg.socket_name()).map_err(|e| {
            ApiError::failed(format!(
                "keel-daemon: not running for profile \"{profile}\" ({e})"
            ))
        })?;
        Ok(Box::new(client) as Box<dyn Backend + Send>)
    })
}

/// What the next query asks.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub search: String,
    pub match_case: bool,
    pub whole_word: bool,
    pub match_path: bool,
    pub match_diacritics: bool,
    pub regex: bool,
    /// Results kept at most (default all: `u32::MAX`).
    pub max: u32,
    /// Results skipped first.
    pub offset: u32,
    /// A `KEEL_SORT_*` value.
    pub sort: u32,
    /// `KEEL_REQUEST_*` bits.
    pub flags: u32,
    /// The keel-daemon profile asked (default `default`); kept by `reset`.
    pub profile: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            search: String::new(),
            match_case: false,
            whole_word: false,
            match_path: false,
            match_diacritics: false,
            regex: false,
            max: u32::MAX,
            offset: 0,
            sort: KEEL_SORT_NAME_ASCENDING,
            flags: KEEL_REQUEST_DEFAULT,
            profile: keel_api::config::DEFAULT_PROFILE.to_owned(),
        }
    }
}

/// One result.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub full_path: String,
    pub name: String,
    /// The folder it is in.
    pub parent: String,
    pub is_dir: bool,
    /// Bytes (0 for a folder).
    pub size: u64,
    /// Unix seconds.
    pub modified: Option<i64>,
    /// `FILE_ATTRIBUTE_*` bits (read only when requested).
    pub attributes: u32,
}

/// A finished query's results.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Results {
    /// The request flags at query time: what may be read.
    pub flags: u32,
    pub items: Vec<Item>,
    /// Matches in all (counted over the first 10000 the backend finds).
    pub total: u32,
    /// The backend found 10000 or more.
    pub capped: bool,
    /// The backend that answered ("Everything", "Keel index", ...).
    pub backend: String,
}

#[derive(Default)]
struct State {
    settings: Settings,
    results: Option<Arc<Results>>,
    /// Bumped by every query, reset and cleanup: a query's answer is kept only while it is
    /// still the latest.
    generation: u64,
    error: u32,
    message: String,
}

struct Inner {
    state: Mutex<State>,
    connect: Connector,
}

/// The SDK's state: settings, the last results and the last error. Cheap to clone (one
/// shared state); every method is thread-safe.
#[derive(Clone)]
pub struct Sdk(Arc<Inner>);

/// Called once when a background query ends, with its `KEEL_*` code.
pub type Done = Box<dyn FnOnce(u32) + Send + 'static>;

impl Sdk {
    pub fn new(connect: Connector) -> Sdk {
        Sdk(Arc::new(Inner {
            state: Mutex::new(State::default()),
            connect,
        }))
    }

    fn fail(&self, code: u32, message: impl Into<String>) {
        let mut s = self.0.state.lock();
        s.error = code;
        s.message = message.into();
    }

    pub fn last_error(&self) -> u32 {
        self.0.state.lock().error
    }

    pub fn last_error_message(&self) -> String {
        self.0.state.lock().message.clone()
    }

    pub fn settings(&self) -> Settings {
        self.0.state.lock().settings.clone()
    }

    fn edit(&self, f: impl FnOnce(&mut Settings)) {
        f(&mut self.0.state.lock().settings);
    }

    /// False (and [`KEEL_ERROR_INVALIDPARAMETER`]) when longer than [`MAX_SEARCH`] bytes.
    pub fn set_search(&self, search: &str) -> bool {
        if search.len() > MAX_SEARCH {
            self.fail(
                KEEL_ERROR_INVALIDPARAMETER,
                format!("the search is longer than {MAX_SEARCH} bytes"),
            );
            return false;
        }
        self.edit(|s| s.search = search.to_owned());
        true
    }

    pub fn set_match_case(&self, on: bool) {
        self.edit(|s| s.match_case = on);
    }

    pub fn set_whole_word(&self, on: bool) {
        self.edit(|s| s.whole_word = on);
    }

    pub fn set_match_path(&self, on: bool) {
        self.edit(|s| s.match_path = on);
    }

    pub fn set_match_diacritics(&self, on: bool) {
        self.edit(|s| s.match_diacritics = on);
    }

    pub fn set_regex(&self, on: bool) {
        self.edit(|s| s.regex = on);
    }

    pub fn set_max(&self, max: u32) {
        self.edit(|s| s.max = max);
    }

    pub fn set_offset(&self, offset: u32) {
        self.edit(|s| s.offset = offset);
    }

    /// False (and [`KEEL_ERROR_INVALIDPARAMETER`]) for a sort Keel does not have; the sort
    /// is then unchanged.
    pub fn set_sort(&self, sort: u32) -> bool {
        if sort_of(sort).is_none() {
            self.fail(
                KEEL_ERROR_INVALIDPARAMETER,
                format!("sort {sort} is not one of Keel's (name, path, size, date modified)"),
            );
            return false;
        }
        self.edit(|s| s.sort = sort);
        true
    }

    /// Bits Keel does not know are dropped.
    pub fn set_request_flags(&self, flags: u32) {
        self.edit(|s| s.flags = flags & KNOWN_FLAGS);
    }

    /// False (and [`KEEL_ERROR_INVALIDPARAMETER`]) for a name keel-daemon would refuse.
    pub fn set_profile(&self, profile: &str) -> bool {
        if !keel_api::config::valid_profile(profile) {
            self.fail(
                KEEL_ERROR_INVALIDPARAMETER,
                format!("profile \"{profile}\": {}", keel_api::config::PROFILE_RULE),
            );
            return false;
        }
        self.edit(|s| s.profile = profile.to_owned());
        true
    }

    /// The default settings (the profile stays), no results, no error. A query still
    /// running is dropped when it ends ([`KEEL_ERROR_CANCELLED`]).
    pub fn reset(&self) {
        let mut s = self.0.state.lock();
        let profile = std::mem::take(&mut s.settings.profile);
        *s = State {
            settings: Settings {
                profile,
                ..Settings::default()
            },
            generation: s.generation + 1,
            ..State::default()
        };
    }

    /// As `reset`, and the profile back to `default`.
    pub fn cleanup(&self) {
        let mut s = self.0.state.lock();
        *s = State {
            generation: s.generation + 1,
            ..State::default()
        };
    }

    /// Runs the search with the current settings. `wait`: returns when it is done, true when
    /// it succeeded. Otherwise it runs on a new thread, `done` is called there with the
    /// result code when it ends, and this returns true once the thread started. The results
    /// replace the previous ones only when the query succeeded and is still the latest.
    pub fn query(&self, wait: bool, done: Option<Done>) -> bool {
        if !wait && done.is_none() {
            self.fail(
                KEEL_ERROR_INVALIDPARAMETER,
                "a query that does not wait needs a callback",
            );
            return false;
        }
        let (settings, generation) = {
            let mut s = self.0.state.lock();
            s.generation += 1;
            (s.settings.clone(), s.generation)
        };
        if wait {
            let code = self.finish(generation, self.run(&settings));
            if let Some(done) = done {
                done(code);
            }
            return code == KEEL_OK;
        }
        let sdk = self.clone();
        let spawned = std::thread::Builder::new()
            .name("keel-sdk-query".into())
            .spawn(move || {
                let code = sdk.finish(generation, sdk.run(&settings));
                if let Some(done) = done {
                    done(code);
                }
            });
        match spawned {
            Ok(_) => true,
            Err(e) => {
                self.fail(KEEL_ERROR_CREATETHREAD, e.to_string());
                false
            }
        }
    }

    /// Keeps `result` when `generation` is still the latest; the code for the caller.
    fn finish(&self, generation: u64, result: Result<Results, (u32, String)>) -> u32 {
        let mut s = self.0.state.lock();
        if s.generation != generation {
            return KEEL_ERROR_CANCELLED;
        }
        match result {
            Ok(results) => {
                s.results = Some(Arc::new(results));
                s.error = KEEL_OK;
                s.message.clear();
                KEEL_OK
            }
            Err((code, message)) => {
                s.error = code;
                s.message = message;
                code
            }
        }
    }

    /// The daemon's `system.search`, page by page up to `max` results.
    fn run(&self, s: &Settings) -> Result<Results, (u32, String)> {
        let (sort, ascending) =
            sort_of(s.sort).ok_or((KEEL_ERROR_INVALIDPARAMETER, "unknown sort".to_owned()))?;
        let mut backend = (self.0.connect)(&s.profile).map_err(|e| code_of(&e))?;
        let max = s.max as usize;
        let mut out = Results {
            flags: s.flags,
            ..Results::default()
        };
        let mut offset = s.offset as usize;
        for _ in 0..MAX_PAGES {
            let count = (max - out.items.len()).min(PAGE);
            let params = SystemSearchParams {
                query: s.search.clone(),
                offset: Some(offset),
                count: Some(count),
                match_case: s.match_case,
                whole_word: s.whole_word,
                match_path: s.match_path,
                match_diacritics: s.match_diacritics,
                regex: s.regex,
                sort: Some(sort),
                ascending: Some(ascending),
                ..SystemSearchParams::default()
            };
            let params =
                serde_json::to_value(&params).map_err(|e| (KEEL_ERROR_MEMORY, e.to_string()))?;
            let answer = backend
                .call("system.search", params)
                .map_err(|e| code_of(&e))?;
            let page: SystemSearchResult = serde_json::from_value(answer).map_err(|e| {
                (
                    KEEL_ERROR_SEARCH,
                    format!("keel-daemon answered system.search unexpectedly: {e}"),
                )
            })?;
            out.total = u32::try_from(page.total).unwrap_or(u32::MAX);
            out.capped = page.capped;
            out.backend = page.backend;
            let got = page.hits.len().min(count);
            out.items
                .extend(page.hits.into_iter().take(count).map(|h| item(h, s.flags)));
            offset += got;
            if got == 0 || got < count || out.items.len() >= max || offset >= page.total {
                break;
            }
        }
        Ok(out)
    }

    /// The last results, or [`KEEL_ERROR_INVALIDCALL`].
    fn results(&self) -> Option<Arc<Results>> {
        let r = self.0.state.lock().results.clone();
        if r.is_none() {
            self.fail(
                KEEL_ERROR_INVALIDCALL,
                "no query has finished since the last reset",
            );
        }
        r
    }

    /// Results of the last query (0 without one).
    pub fn num_results(&self) -> u32 {
        self.results().map_or(0, |r| r.items.len() as u32)
    }

    /// Matches in all, beyond `offset` and `max` (0 without a query).
    pub fn total_results(&self) -> u32 {
        self.results().map_or(0, |r| r.total)
    }

    /// The backend that answered the last query.
    pub fn backend(&self) -> Option<String> {
        self.results().map(|r| r.backend.clone())
    }

    /// `f` of result `index` when the query requested what `need` accepts; else None with
    /// the error set.
    pub fn with_item<T>(
        &self,
        index: u32,
        need: impl FnOnce(u32) -> bool,
        f: impl FnOnce(&Item) -> T,
    ) -> Option<T> {
        let r = self.results()?;
        let Some(item) = r.items.get(index as usize) else {
            self.fail(
                KEEL_ERROR_INVALIDINDEX,
                format!("result {index} of {}", r.items.len()),
            );
            return None;
        };
        if !need(r.flags) {
            self.fail(
                KEEL_ERROR_INVALIDREQUEST,
                "that property was not requested before the query (keel_set_request_flags)",
            );
            return None;
        }
        Some(f(item))
    }

    pub fn full_path(&self, index: u32) -> Option<String> {
        self.with_item(index, wants_full_path, |i| i.full_path.clone())
    }

    pub fn file_name(&self, index: u32) -> Option<String> {
        let need = |f| f & (KEEL_REQUEST_FILE_NAME | KEEL_REQUEST_FULL_PATH_AND_FILE_NAME) != 0;
        self.with_item(index, need, |i| i.name.clone())
    }

    pub fn path(&self, index: u32) -> Option<String> {
        let need = |f| f & (KEEL_REQUEST_PATH | KEEL_REQUEST_FULL_PATH_AND_FILE_NAME) != 0;
        self.with_item(index, need, |i| i.parent.clone())
    }

    pub fn is_folder(&self, index: u32) -> Option<bool> {
        self.with_item(index, |_| true, |i| i.is_dir)
    }

    pub fn size(&self, index: u32) -> Option<u64> {
        self.with_item(index, |f| f & KEEL_REQUEST_SIZE != 0, |i| i.size)
    }

    /// A FILETIME; [`UNKNOWN_FILETIME`] when the backend did not know the date.
    pub fn date_modified(&self, index: u32) -> Option<u64> {
        self.with_item(
            index,
            |f| f & KEEL_REQUEST_DATE_MODIFIED != 0,
            |i| i.modified.and_then(filetime).unwrap_or(UNKNOWN_FILETIME),
        )
    }

    pub fn attributes(&self, index: u32) -> Option<u32> {
        self.with_item(
            index,
            |f| f & KEEL_REQUEST_ATTRIBUTES != 0,
            |i| i.attributes,
        )
    }

    /// Keel keeps no run counts: always [`KEEL_ERROR_UNSUPPORTED`].
    pub fn run_count_unsupported(&self) {
        self.fail(KEEL_ERROR_UNSUPPORTED, "Keel keeps no run counts");
    }
}

fn wants_full_path(flags: u32) -> bool {
    flags & KEEL_REQUEST_FULL_PATH_AND_FILE_NAME != 0
        || flags & (KEEL_REQUEST_FILE_NAME | KEEL_REQUEST_PATH)
            == KEEL_REQUEST_FILE_NAME | KEEL_REQUEST_PATH
}

/// `system.search`'s order for a `KEEL_SORT_*` value.
pub fn sort_of(sort: u32) -> Option<(SystemSort, bool)> {
    Some(match sort {
        KEEL_SORT_NAME_ASCENDING => (SystemSort::Name, true),
        KEEL_SORT_NAME_DESCENDING => (SystemSort::Name, false),
        KEEL_SORT_PATH_ASCENDING => (SystemSort::Path, true),
        KEEL_SORT_PATH_DESCENDING => (SystemSort::Path, false),
        KEEL_SORT_SIZE_ASCENDING => (SystemSort::Size, true),
        KEEL_SORT_SIZE_DESCENDING => (SystemSort::Size, false),
        KEEL_SORT_DATE_MODIFIED_ASCENDING => (SystemSort::DateModified, true),
        KEEL_SORT_DATE_MODIFIED_DESCENDING => (SystemSort::DateModified, false),
        _ => return None,
    })
}

/// The `KEEL_ERROR_*` code and message for an API error.
fn code_of(e: &ApiError) -> (u32, String) {
    let code = match e.code {
        ApiError::INVALID_PARAMS => KEEL_ERROR_INVALIDPARAMETER,
        ApiError::TIMEOUT => KEEL_ERROR_TIMEOUT,
        ApiError::METHOD_NOT_FOUND => KEEL_ERROR_UNSUPPORTED,
        // The client's own transport errors.
        _ if e.message.starts_with("keel-daemon: ") => KEEL_ERROR_IPC,
        _ => KEEL_ERROR_SEARCH,
    };
    (code, e.message.clone())
}

/// Seconds between 1601-01-01 and 1970-01-01.
const EPOCH_1601: i64 = 11_644_473_600;

/// Unix seconds as a FILETIME (100 ns since 1601); None before 1601 or past FILETIME's
/// range.
pub fn filetime(unix: i64) -> Option<u64> {
    let secs = unix.checked_add(EPOCH_1601)?;
    u64::try_from(secs).ok()?.checked_mul(10_000_000)
}

fn item(h: SystemHit, flags: u32) -> Item {
    let attributes = match flags & KEEL_REQUEST_ATTRIBUTES != 0 {
        true => attributes_of(&h.path, &h.name, h.is_dir),
        false => 0,
    };
    Item {
        full_path: h.path,
        name: h.name,
        parent: h.parent,
        is_dir: h.is_dir,
        size: if h.is_dir { 0 } else { h.size },
        modified: h.modified,
        attributes,
    }
}

/// The file's attributes: on Windows read (never changed) from the file system; else, or
/// when that fails, folder or normal, plus hidden for a dot name outside Windows.
#[cfg_attr(not(windows), allow(unused_variables))]
fn attributes_of(path: &str, name: &str, is_dir: bool) -> u32 {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            return meta.file_attributes();
        }
    }
    let base = match is_dir {
        true => FILE_ATTRIBUTE_DIRECTORY,
        false => FILE_ATTRIBUTE_NORMAL,
    };
    match cfg!(not(windows)) && name.starts_with('.') {
        true => (base & !FILE_ATTRIBUTE_NORMAL) | FILE_ATTRIBUTE_HIDDEN,
        false => base,
    }
}

/// Copies `s` as UTF-8 into `out` (when given, cut at a character boundary to leave room
/// for the NUL, which is always written); returns the bytes `s` needs without the NUL.
pub fn fill_utf8(s: &str, out: Option<&mut [u8]>) -> u32 {
    if let Some(out) = out.filter(|o| !o.is_empty()) {
        let mut n = s.len().min(out.len() - 1);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        out[..n].copy_from_slice(&s.as_bytes()[..n]);
        out[n] = 0;
    }
    u32::try_from(s.len()).unwrap_or(u32::MAX)
}

/// As [`fill_utf8`] in UTF-16 code units (a surrogate pair is never split).
pub fn fill_utf16(s: &str, out: Option<&mut [u16]>) -> u32 {
    let units: Vec<u16> = s.encode_utf16().collect();
    if let Some(out) = out.filter(|o| !o.is_empty()) {
        let mut n = units.len().min(out.len() - 1);
        // Not between the halves of a pair.
        if n > 0 && n < units.len() && (0xD800..0xDC00).contains(&units[n - 1]) {
            n -= 1;
        }
        out[..n].copy_from_slice(&units[..n]);
        out[n] = 0;
    }
    u32::try_from(units.len()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests;
