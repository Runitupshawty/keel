//! The state machine against a mock daemon: settings to `system.search` params, paging,
//! errors, getters, background queries and their cancellation, thread safety.

use super::*;
use serde_json::{json, Value};
use std::sync::mpsc;
use std::time::Duration;

/// What the mock daemon saw.
#[derive(Default)]
struct Seen {
    profiles: Vec<String>,
    calls: Vec<Value>,
}

/// A daemon serving `n` files `file-0000.txt`... (sizes = index, folders every 10th), with
/// an optional gate each call waits on.
struct Mock {
    n: usize,
    seen: Arc<Mutex<Seen>>,
    gate: Option<Arc<Mutex<mpsc::Receiver<()>>>>,
    fail: Option<ApiError>,
}

impl Backend for Mock {
    fn call(&mut self, method: &str, params: Value) -> keel_api::Result<Value> {
        assert_eq!(method, "system.search");
        self.seen.lock().calls.push(params.clone());
        if let Some(gate) = &self.gate {
            gate.lock().recv_timeout(Duration::from_secs(10)).unwrap();
        }
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        let p: SystemSearchParams = serde_json::from_value(params).unwrap();
        let mut hits: Vec<SystemHit> = (0..self.n)
            .map(|i| SystemHit {
                path: format!("/nowhere-keel-sdk/file-{i:04}.txt"),
                name: format!("file-{i:04}.txt"),
                parent: "/nowhere-keel-sdk".into(),
                is_dir: i % 10 == 9,
                size: i as u64,
                modified: (i % 2 == 0).then_some(1_700_000_000 + i as i64),
            })
            .filter(|h| h.name.contains(&p.query))
            .collect();
        if p.ascending == Some(false) {
            hits.reverse();
        }
        let total = hits.len();
        let hits = hits
            .into_iter()
            .skip(p.offset.unwrap_or(0))
            .take(p.count.unwrap_or(32).min(PAGE))
            .collect();
        Ok(serde_json::to_value(SystemSearchResult {
            backend: "Mock".into(),
            total,
            capped: false,
            offset: p.offset.unwrap_or(0),
            hits,
        })
        .unwrap())
    }
}

struct Fixture {
    sdk: Sdk,
    seen: Arc<Mutex<Seen>>,
}

fn mock(n: usize) -> Fixture {
    mock_with(n, None, None)
}

fn mock_with(n: usize, gate: Option<mpsc::Receiver<()>>, fail: Option<ApiError>) -> Fixture {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let gate = gate.map(|g| Arc::new(Mutex::new(g)));
    let s = seen.clone();
    let sdk = Sdk::new(Arc::new(move |profile: &str| {
        s.lock().profiles.push(profile.to_owned());
        Ok(Box::new(Mock {
            n,
            seen: s.clone(),
            gate: gate.clone(),
            fail: fail.clone(),
        }) as Box<dyn Backend + Send>)
    }));
    Fixture { sdk, seen }
}

#[test]
fn defaults_match_everything_sdk() {
    let s = Settings::default();
    assert_eq!(s.max, u32::MAX);
    assert_eq!(s.offset, 0);
    assert_eq!(s.sort, KEEL_SORT_NAME_ASCENDING);
    assert_eq!(s.flags, KEEL_REQUEST_FILE_NAME | KEEL_REQUEST_PATH);
    assert_eq!(s.profile, "default");
    let f = mock(0);
    assert_eq!(f.sdk.last_error(), KEEL_OK);
    // Nothing to read before a query.
    assert_eq!(f.sdk.num_results(), 0);
    assert_eq!(f.sdk.last_error(), KEEL_ERROR_INVALIDCALL);
    assert_eq!(f.sdk.full_path(0), None);
}

#[test]
fn settings_become_system_search_params() {
    let f = mock(5);
    f.sdk.set_search("file");
    f.sdk.set_match_case(true);
    f.sdk.set_whole_word(true);
    f.sdk.set_match_path(true);
    f.sdk.set_match_diacritics(true);
    f.sdk.set_regex(true);
    f.sdk.set_max(3);
    f.sdk.set_offset(1);
    assert!(f.sdk.set_sort(KEEL_SORT_DATE_MODIFIED_DESCENDING));
    assert!(f.sdk.set_profile("work"));
    assert!(f.sdk.query(true, None));
    let seen = f.seen.lock();
    assert_eq!(seen.profiles, ["work"]);
    assert_eq!(
        seen.calls,
        [json!({
            "query": "file", "offset": 1, "count": 3, "match_case": true, "whole_word": true,
            "match_path": true, "match_diacritics": true, "regex": true, "prefix": false,
            "suffix": false, "ignore_punctuation": false, "ignore_whitespace": false,
            "sort": "date_modified", "ascending": false
        })]
    );
    drop(seen);
    assert_eq!(f.sdk.num_results(), 3);
    assert_eq!(f.sdk.total_results(), 5);
    assert_eq!(f.sdk.backend().as_deref(), Some("Mock"));
}

#[test]
fn every_sort_maps_and_others_are_refused() {
    let f = mock(0);
    for (sort, field, ascending) in [
        (KEEL_SORT_NAME_ASCENDING, SystemSort::Name, true),
        (KEEL_SORT_NAME_DESCENDING, SystemSort::Name, false),
        (KEEL_SORT_PATH_ASCENDING, SystemSort::Path, true),
        (KEEL_SORT_PATH_DESCENDING, SystemSort::Path, false),
        (KEEL_SORT_SIZE_ASCENDING, SystemSort::Size, true),
        (KEEL_SORT_SIZE_DESCENDING, SystemSort::Size, false),
        (
            KEEL_SORT_DATE_MODIFIED_ASCENDING,
            SystemSort::DateModified,
            true,
        ),
        (
            KEEL_SORT_DATE_MODIFIED_DESCENDING,
            SystemSort::DateModified,
            false,
        ),
    ] {
        assert_eq!(sort_of(sort), Some((field, ascending)));
        assert!(f.sdk.set_sort(sort));
    }
    // Everything's extension, type and date created sorts are not Keel's.
    for bad in [0, 7, 9, 11, 26, u32::MAX] {
        assert!(!f.sdk.set_sort(bad));
        assert_eq!(f.sdk.last_error(), KEEL_ERROR_INVALIDPARAMETER);
    }
    assert_eq!(f.sdk.settings().sort, KEEL_SORT_DATE_MODIFIED_DESCENDING);
}

#[test]
fn large_max_is_fetched_in_pages() {
    let f = mock(2_600);
    f.sdk.set_max(2_500);
    assert!(f.sdk.query(true, None));
    assert_eq!(f.sdk.num_results(), 2_500);
    assert_eq!(f.sdk.total_results(), 2_600);
    let pages: Vec<(u64, u64)> = f
        .seen
        .lock()
        .calls
        .iter()
        .map(|c| (c["offset"].as_u64().unwrap(), c["count"].as_u64().unwrap()))
        .collect();
    assert_eq!(pages, [(0, 1000), (1000, 1000), (2000, 500)]);
    assert_eq!(f.sdk.file_name(2_499).as_deref(), Some("file-2499.txt"));

    // Default max: everything, stopping at the end.
    let f = mock(1_200);
    f.sdk.set_offset(100);
    assert!(f.sdk.query(true, None));
    assert_eq!(f.sdk.num_results(), 1_100);
    assert_eq!(f.seen.lock().calls.len(), 2);
    assert_eq!(f.sdk.file_name(0).as_deref(), Some("file-0100.txt"));

    // Max 0: the count only.
    let f = mock(42);
    f.sdk.set_max(0);
    assert!(f.sdk.query(true, None));
    assert_eq!((f.sdk.num_results(), f.sdk.total_results()), (0, 42));
    assert_eq!(f.seen.lock().calls[0]["count"], 0);
}

#[test]
fn getters_check_index_and_request_flags() {
    let f = mock(12);
    assert!(f.sdk.query(true, None));
    // Default flags: name and path, so the full path too, but not size or date.
    assert_eq!(
        f.sdk.full_path(0).as_deref(),
        Some("/nowhere-keel-sdk/file-0000.txt")
    );
    assert_eq!(f.sdk.file_name(1).as_deref(), Some("file-0001.txt"));
    assert_eq!(f.sdk.path(1).as_deref(), Some("/nowhere-keel-sdk"));
    assert_eq!(f.sdk.size(0), None);
    assert_eq!(f.sdk.last_error(), KEEL_ERROR_INVALIDREQUEST);
    assert_eq!(f.sdk.date_modified(0), None);
    assert_eq!(f.sdk.attributes(0), None);
    assert_eq!(f.sdk.full_path(12), None);
    assert_eq!(f.sdk.last_error(), KEEL_ERROR_INVALIDINDEX);
    assert_eq!(f.sdk.is_folder(9), Some(true));
    assert_eq!(f.sdk.is_folder(8), Some(false));

    // Flags are taken at query time.
    f.sdk.set_request_flags(
        KEEL_REQUEST_FULL_PATH_AND_FILE_NAME
            | KEEL_REQUEST_SIZE
            | KEEL_REQUEST_DATE_MODIFIED
            | KEEL_REQUEST_ATTRIBUTES
            | 0x8000_0000,
    );
    assert_eq!(f.sdk.size(3), None, "not yet queried with them");
    assert!(f.sdk.query(true, None));
    assert_eq!(
        f.sdk.settings().flags & 0x8000_0000,
        0,
        "unknown bits dropped"
    );
    assert_eq!(f.sdk.size(3), Some(3));
    assert_eq!(f.sdk.size(9), Some(0), "a folder");
    assert_eq!(
        f.sdk.date_modified(0),
        Some((1_700_000_000 + 11_644_473_600) * 10_000_000)
    );
    assert_eq!(f.sdk.date_modified(1), Some(UNKNOWN_FILETIME));
    // Not on disk: folder or normal.
    assert_eq!(f.sdk.attributes(9), Some(FILE_ATTRIBUTE_DIRECTORY));
    assert_eq!(f.sdk.attributes(0), Some(FILE_ATTRIBUTE_NORMAL));
    assert_eq!(f.sdk.file_name(0).as_deref(), Some("file-0000.txt"));
    assert_eq!(f.sdk.path(0).as_deref(), Some("/nowhere-keel-sdk"));

    // Name only: no full path.
    f.sdk.set_request_flags(KEEL_REQUEST_FILE_NAME);
    assert!(f.sdk.query(true, None));
    assert_eq!(f.sdk.full_path(0), None);
    assert_eq!(f.sdk.last_error(), KEEL_ERROR_INVALIDREQUEST);
    assert_eq!(f.sdk.path(0), None);
}

#[test]
fn errors_map_to_keel_codes() {
    let cases = [
        (
            ApiError::invalid_params("bad regex"),
            KEEL_ERROR_INVALIDPARAMETER,
        ),
        (
            ApiError::new(ApiError::TIMEOUT, "keel-daemon: late"),
            KEEL_ERROR_TIMEOUT,
        ),
        (
            ApiError::new(ApiError::METHOD_NOT_FOUND, "no such method"),
            KEEL_ERROR_UNSUPPORTED,
        ),
        (ApiError::failed("keel-daemon: pipe broken"), KEEL_ERROR_IPC),
        (
            ApiError::failed("Everything: not running"),
            KEEL_ERROR_SEARCH,
        ),
    ];
    for (e, code) in cases {
        let f = mock_with(3, None, Some(e.clone()));
        assert!(!f.sdk.query(true, None));
        assert_eq!(f.sdk.last_error(), code, "{e:?}");
        assert_eq!(f.sdk.last_error_message(), e.message);
    }
    // No daemon.
    let sdk = Sdk::new(Arc::new(|_: &str| {
        Err(ApiError::failed("keel-daemon: not running"))
    }));
    assert!(!sdk.query(true, None));
    assert_eq!(sdk.last_error(), KEEL_ERROR_IPC);
    // A failed query keeps the previous results.
    let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let d = down.clone();
    let sdk = Sdk::new(Arc::new(move |_: &str| {
        match d.load(std::sync::atomic::Ordering::SeqCst) {
            true => Err(ApiError::failed("keel-daemon: gone")),
            false => Ok(Box::new(Mock {
                n: 3,
                seen: Default::default(),
                gate: None,
                fail: None,
            }) as Box<dyn Backend + Send>),
        }
    }));
    assert!(sdk.query(true, None));
    down.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(!sdk.query(true, None));
    assert_eq!(sdk.last_error(), KEEL_ERROR_IPC);
    assert_eq!(sdk.num_results(), 3);

    // A success clears the error.
    let f = mock(3);
    assert!(!f.sdk.set_sort(99));
    assert!(f.sdk.query(true, None));
    assert_eq!(f.sdk.last_error(), KEEL_OK);
    assert_eq!(f.sdk.last_error_message(), "");
}

#[test]
fn search_and_profile_are_checked() {
    let f = mock(0);
    assert!(f.sdk.set_search(&"a".repeat(MAX_SEARCH)));
    assert!(!f.sdk.set_search(&"a".repeat(MAX_SEARCH + 1)));
    assert_eq!(f.sdk.last_error(), KEEL_ERROR_INVALIDPARAMETER);
    assert_eq!(f.sdk.settings().search.len(), MAX_SEARCH, "unchanged");
    for bad in ["", "../x", "a b", "a\\b", "con:"] {
        assert!(!f.sdk.set_profile(bad), "{bad}");
    }
    assert_eq!(f.sdk.settings().profile, "default");
    // A background query needs a callback.
    assert!(!f.sdk.query(false, None));
    assert_eq!(f.sdk.last_error(), KEEL_ERROR_INVALIDPARAMETER);
}

#[test]
fn reset_and_cleanup() {
    let f = mock(4);
    f.sdk.set_search("file");
    f.sdk.set_max(2);
    f.sdk.set_profile("work");
    assert!(f.sdk.query(true, None));
    f.sdk.reset();
    let s = f.sdk.settings();
    assert_eq!(
        s,
        Settings {
            profile: "work".into(),
            ..Settings::default()
        }
    );
    assert_eq!(f.sdk.num_results(), 0);
    assert_eq!(f.sdk.last_error(), KEEL_ERROR_INVALIDCALL);
    f.sdk.cleanup();
    assert_eq!(f.sdk.settings(), Settings::default());
    assert_eq!(f.sdk.last_error(), KEEL_OK);
}

#[test]
fn background_query_calls_back_when_done() {
    let (open, gate) = mpsc::channel();
    let f = mock_with(5, Some(gate), None);
    let (tx, rx) = mpsc::channel();
    let done: Done = Box::new(move |code| tx.send(code).unwrap());
    assert!(f.sdk.query(false, Some(done)));
    // Still running: settings may change meanwhile (for the next query).
    f.sdk.set_search("nothing");
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    open.send(()).unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), KEEL_OK);
    assert_eq!(f.sdk.num_results(), 5);
    assert_eq!(f.seen.lock().calls[0]["query"], "");
}

#[test]
fn a_newer_query_or_reset_cancels_a_running_one() {
    let (open, gate) = mpsc::channel();
    let f = mock_with(5, Some(gate), None);
    let (tx, rx) = mpsc::channel();
    let tx2 = tx.clone();
    assert!(f.sdk.query(
        false,
        Some(Box::new(move |c| tx.send(("first", c)).unwrap()))
    ));
    // Wait until the first one is in the daemon.
    while f.seen.lock().calls.is_empty() {
        std::thread::sleep(Duration::from_millis(5));
    }
    f.sdk.set_search("file-0001");
    assert!(f.sdk.query(
        false,
        Some(Box::new(move |c| tx2.send(("second", c)).unwrap()))
    ));
    open.send(()).unwrap();
    open.send(()).unwrap();
    let mut got = [
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
    ];
    got.sort();
    assert_eq!(got, [("first", KEEL_ERROR_CANCELLED), ("second", KEEL_OK)]);
    assert_eq!(f.sdk.num_results(), 1, "the newer query's results");

    let (open, gate) = mpsc::channel();
    let f = mock_with(5, Some(gate), None);
    let (tx, rx) = mpsc::channel();
    assert!(f
        .sdk
        .query(false, Some(Box::new(move |c| tx.send(c).unwrap()))));
    f.sdk.reset();
    open.send(()).unwrap();
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        KEEL_ERROR_CANCELLED
    );
    assert_eq!(f.sdk.num_results(), 0, "dropped");
}

#[test]
fn callbacks_may_query_again() {
    let f = mock(3);
    let (tx, rx) = mpsc::channel();
    let again = f.sdk.clone();
    let done: Done = Box::new(move |code| {
        assert_eq!(code, KEEL_OK);
        again.set_max(1);
        tx.send(again.query(true, None)).unwrap();
    });
    assert!(f.sdk.query(false, Some(done)));
    assert!(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(f.sdk.num_results(), 1);
}

#[test]
fn threads_share_one_state_safely() {
    let f = mock(50);
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let sdk = f.sdk.clone();
            std::thread::spawn(move || {
                for i in 0..20 {
                    sdk.set_max(t * 3 + i % 4);
                    sdk.set_match_case(i % 2 == 0);
                    let _ = sdk.query(true, None);
                    let n = sdk.num_results();
                    for k in 0..n {
                        // Results may be replaced between calls: either a value or an
                        // index error, never a crash.
                        let _ = sdk.full_path(k);
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(f.seen.lock().calls.len(), 8 * 20);
}

#[test]
fn filetime_conversion() {
    assert_eq!(filetime(0), Some(116_444_736_000_000_000));
    assert_eq!(filetime(-11_644_473_600), Some(0));
    assert_eq!(filetime(-11_644_473_601), None, "before 1601");
    assert_eq!(filetime(i64::MAX), None);
}

#[test]
fn string_buffers_are_cut_at_character_boundaries() {
    let s = "a\u{e9}\u{1F600}"; // a, é (2 bytes), an emoji (4 bytes, a UTF-16 pair)
    assert_eq!(fill_utf8(s, None), 7);
    let mut buf = [0xffu8; 8];
    assert_eq!(fill_utf8(s, Some(&mut buf)), 7);
    assert_eq!(&buf, b"a\xc3\xa9\xf0\x9f\x98\x80\0");
    let mut buf = [0xffu8; 5];
    assert_eq!(fill_utf8(s, Some(&mut buf)), 7);
    assert_eq!(&buf[..4], b"a\xc3\xa9\0", "the emoji does not fit whole");
    let mut buf = [0xffu8; 1];
    fill_utf8(s, Some(&mut buf));
    assert_eq!(buf, [0]);

    assert_eq!(fill_utf16(s, None), 4);
    let mut buf = [0xffffu16; 5];
    assert_eq!(fill_utf16(s, Some(&mut buf)), 4);
    assert_eq!(buf, [0x61, 0xe9, 0xd83d, 0xde00, 0]);
    let mut buf = [0xffffu16; 4];
    fill_utf16(s, Some(&mut buf));
    assert_eq!(buf[..3], [0x61, 0xe9, 0], "a pair is not split");
}

/// `include/keel_sdk.h` declares every exported function and the constants' values.
#[test]
fn header_matches_the_exports() {
    let header = include_str!("../include/keel_sdk.h");
    let ffi = include_str!("ffi.rs");
    let exported: Vec<&str> = ffi
        .split("extern \"C\" fn ")
        .skip(1)
        .filter_map(|rest| rest.split('(').next())
        .collect();
    assert!(exported.len() >= 30, "{exported:?}");
    for name in &exported {
        assert!(
            header.contains(&format!(" {name}(")),
            "{name} is not declared in keel_sdk.h"
        );
    }
    for (name, value) in [
        ("KEEL_OK", KEEL_OK),
        ("KEEL_ERROR_MEMORY", KEEL_ERROR_MEMORY),
        ("KEEL_ERROR_IPC", KEEL_ERROR_IPC),
        ("KEEL_ERROR_CREATETHREAD", KEEL_ERROR_CREATETHREAD),
        ("KEEL_ERROR_INVALIDINDEX", KEEL_ERROR_INVALIDINDEX),
        ("KEEL_ERROR_INVALIDCALL", KEEL_ERROR_INVALIDCALL),
        ("KEEL_ERROR_INVALIDREQUEST", KEEL_ERROR_INVALIDREQUEST),
        ("KEEL_ERROR_INVALIDPARAMETER", KEEL_ERROR_INVALIDPARAMETER),
        ("KEEL_ERROR_UNSUPPORTED", KEEL_ERROR_UNSUPPORTED),
        ("KEEL_ERROR_SEARCH", KEEL_ERROR_SEARCH),
        ("KEEL_ERROR_TIMEOUT", KEEL_ERROR_TIMEOUT),
        ("KEEL_ERROR_CANCELLED", KEEL_ERROR_CANCELLED),
        ("KEEL_SORT_NAME_ASCENDING", KEEL_SORT_NAME_ASCENDING),
        ("KEEL_SORT_NAME_DESCENDING", KEEL_SORT_NAME_DESCENDING),
        ("KEEL_SORT_PATH_ASCENDING", KEEL_SORT_PATH_ASCENDING),
        ("KEEL_SORT_PATH_DESCENDING", KEEL_SORT_PATH_DESCENDING),
        ("KEEL_SORT_SIZE_ASCENDING", KEEL_SORT_SIZE_ASCENDING),
        ("KEEL_SORT_SIZE_DESCENDING", KEEL_SORT_SIZE_DESCENDING),
        (
            "KEEL_SORT_DATE_MODIFIED_ASCENDING",
            KEEL_SORT_DATE_MODIFIED_ASCENDING,
        ),
        (
            "KEEL_SORT_DATE_MODIFIED_DESCENDING",
            KEEL_SORT_DATE_MODIFIED_DESCENDING,
        ),
        ("KEEL_REQUEST_FILE_NAME", KEEL_REQUEST_FILE_NAME),
        ("KEEL_REQUEST_PATH", KEEL_REQUEST_PATH),
        (
            "KEEL_REQUEST_FULL_PATH_AND_FILE_NAME",
            KEEL_REQUEST_FULL_PATH_AND_FILE_NAME,
        ),
        ("KEEL_REQUEST_SIZE", KEEL_REQUEST_SIZE),
        ("KEEL_REQUEST_DATE_MODIFIED", KEEL_REQUEST_DATE_MODIFIED),
        ("KEEL_REQUEST_ATTRIBUTES", KEEL_REQUEST_ATTRIBUTES),
        ("KEEL_REQUEST_RUN_COUNT", KEEL_REQUEST_RUN_COUNT),
        (
            "KEEL_INVALID_FILE_ATTRIBUTES",
            ffi::KEEL_INVALID_FILE_ATTRIBUTES,
        ),
    ] {
        let line = header
            .lines()
            .find(|l| l.starts_with(&format!("#define {name} ")))
            .unwrap_or_else(|| panic!("{name} missing from keel_sdk.h"));
        let text = line.split_whitespace().nth(2).unwrap();
        let text = text.trim_end_matches(['u', 'U']);
        let parsed = match text.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16).unwrap(),
            None => text.parse().unwrap(),
        };
        assert_eq!(parsed, value, "{name}");
    }
}
