//! The C ABI end to end: a keel-daemon on a temp profile (settings, library and the folder
//! searched all in temp folders) answers `system.search` from a walk of a temp folder, and
//! every call goes through the exported `keel_*` functions as a C caller makes them.

use keel_api::config::HostConfig;
use keel_daemon::server::{Daemon, Options};
use keel_sdk::ffi::*;
use keel_sdk::*;
use std::ffi::{c_char, c_void, CString};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// The platform search in this test: a walk of one folder.
struct Walk(PathBuf);

impl keel_api::system::Searcher for Walk {
    fn query(&self, q: &keel_api::system::Query) -> anyhow::Result<Vec<keel_api::system::Hit>> {
        Ok(keel_search::walk(&self.0, q))
    }
    fn available(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "Walk"
    }
}

fn rand() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).unwrap();
    u64::from_le_bytes(b)
}

fn last_message() -> String {
    let mut buf = vec![0u8; 1024];
    let n = unsafe { keel_get_last_error_message(buf.as_mut_ptr().cast(), buf.len() as u32) };
    buf.truncate((n as usize).min(1023));
    String::from_utf8_lossy(&buf).into_owned()
}

fn full_path(index: u32) -> String {
    unsafe {
        let n = keel_get_result_full_path(index, std::ptr::null_mut(), 0);
        assert!(n > 0, "error {}", keel_get_last_error());
        let mut buf = vec![0u8; n as usize + 1];
        assert_eq!(
            keel_get_result_full_path(index, buf.as_mut_ptr().cast(), n + 1),
            n
        );
        assert_eq!(buf.pop(), Some(0), "NUL-ended");
        String::from_utf8(buf).unwrap()
    }
}

fn full_path_w(index: u32) -> Vec<u16> {
    unsafe {
        let n = keel_get_result_full_path_w(index, std::ptr::null_mut(), 0);
        let mut buf = vec![0u16; n as usize + 1];
        assert_eq!(
            keel_get_result_full_path_w(index, buf.as_mut_ptr(), n + 1),
            n
        );
        assert_eq!(buf.pop(), Some(0));
        buf
    }
}

fn query() -> bool {
    unsafe { keel_query(1, None, std::ptr::null_mut()) == 1 }
}

fn set_search(s: &str) {
    let s = CString::new(s).unwrap();
    assert_eq!(unsafe { keel_set_search(s.as_ptr()) }, 1);
}

unsafe extern "C" fn on_done(user_data: *mut c_void, error: u32) {
    let tx = &*(user_data as *const mpsc::Sender<u32>);
    tx.send(error).unwrap();
}

/// Every file and folder under `root` with its bytes.
fn snapshot(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.push((p.clone(), None));
                stack.push(p);
            } else {
                out.push((p.clone(), Some(std::fs::read(&p).unwrap())));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn searches_a_temp_folder_through_the_c_abi() {
    let (config, data, files) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    // The SDK finds the daemon from these, as a real caller's process would.
    std::env::set_var("KEEL_CONFIG_DIR", config.path());
    std::env::set_var("KEEL_DATA_DIR", data.path());
    let docs = files.path().join("docs");
    std::fs::create_dir(&docs).unwrap();
    std::fs::write(docs.join("invoice-2026.pdf"), b"pdf bytes").unwrap();
    std::fs::write(docs.join("invoice-\u{fc}.txt"), b"umlaut").unwrap();
    std::fs::write(files.path().join("notes.txt"), b"the biggest file here").unwrap();
    let before = snapshot(files.path());

    let profile = format!("sdk-test-{}-{}", std::process::id(), rand());
    let daemon = Daemon::start(Options {
        cfg: HostConfig::load(&profile).unwrap(),
        ws: None,
        web: None,
        ws_allow_remote: false,
        web_hosts: Vec::new(),
        net: None,
    })
    .unwrap();
    daemon
        .ctx()
        .system
        .set(Arc::new(Walk(files.path().to_owned())));
    let c_profile = CString::new(profile.clone()).unwrap();

    unsafe {
        // The default profile has no daemon here.
        set_search("invoice");
        assert!(!query());
        assert_eq!(keel_get_last_error(), KEEL_ERROR_IPC, "{}", last_message());
        assert_eq!(keel_get_num_results(), 0);

        assert_eq!(keel_set_profile(c_profile.as_ptr()), 1);
        keel_set_request_flags(
            KEEL_REQUEST_FULL_PATH_AND_FILE_NAME
                | KEEL_REQUEST_FILE_NAME
                | KEEL_REQUEST_PATH
                | KEEL_REQUEST_SIZE
                | KEEL_REQUEST_DATE_MODIFIED
                | KEEL_REQUEST_ATTRIBUTES,
        );
        assert_eq!(keel_set_sort(KEEL_SORT_NAME_ASCENDING), 1);
        assert!(query(), "{}", last_message());
        assert_eq!(keel_get_last_error(), KEEL_OK);
        assert_eq!((keel_get_num_results(), keel_get_total_results()), (2, 2));
        let pdf = docs.join("invoice-2026.pdf");
        let umlaut = docs.join("invoice-\u{fc}.txt");
        assert_eq!(full_path(0), pdf.display().to_string());
        assert_eq!(full_path(1), umlaut.display().to_string());
        assert_eq!(
            full_path_w(1),
            umlaut.display().to_string().encode_utf16().collect::<Vec<_>>()
        );
        let mut name = [0u8; 64];
        let n = keel_get_result_file_name(0, name.as_mut_ptr().cast::<c_char>(), 64);
        assert_eq!(&name[..n as usize], b"invoice-2026.pdf");
        let mut parent = [0u16; 512];
        let n = keel_get_result_path_w(0, parent.as_mut_ptr(), 512);
        assert_eq!(
            String::from_utf16(&parent[..n as usize]).unwrap(),
            docs.display().to_string()
        );
        assert_eq!(keel_is_folder_result(0), 0);
        // A buffer too small: cut, NUL-ended, the full length returned.
        let mut small = [0xffu8; 5];
        let n = keel_get_result_full_path(0, small.as_mut_ptr().cast(), 5);
        assert_eq!(n as usize, pdf.display().to_string().len());
        assert_eq!(small[4], 0);

        let mut size = 0u64;
        assert_eq!(keel_get_result_size(0, &mut size), 1);
        assert_eq!(size, 9);
        let mut ft = 0u64;
        assert_eq!(keel_get_result_date_modified(0, &mut ft), 1);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let expected = filetime(now).unwrap();
        let day = 86_400 * 10_000_000;
        assert!(
            ft > expected - day && ft < expected + day,
            "{ft} vs {expected}"
        );
        let attrs = keel_get_result_attributes(0);
        assert_ne!(attrs, KEEL_INVALID_FILE_ATTRIBUTES);
        assert_eq!(attrs & FILE_ATTRIBUTE_DIRECTORY, 0);

        // Bad index and the run counts Keel does not keep.
        assert_eq!(keel_get_result_full_path(2, std::ptr::null_mut(), 0), 0);
        assert_eq!(keel_get_last_error(), KEEL_ERROR_INVALIDINDEX);
        assert_eq!(keel_get_result_size(0, std::ptr::null_mut()), 0);
        assert_eq!(keel_get_last_error(), KEEL_ERROR_INVALIDPARAMETER);
        let p = CString::new(pdf.display().to_string()).unwrap();
        assert_eq!(keel_inc_run_count(p.as_ptr()), 0);
        assert_eq!(keel_get_last_error(), KEEL_ERROR_UNSUPPORTED);
        assert_eq!(keel_get_run_count(p.as_ptr()), 0);
        assert_eq!(keel_get_result_run_count(0), 0);
        assert_eq!(keel_get_last_error(), KEEL_ERROR_UNSUPPORTED);

        // Everything, biggest first, one result; the folder found by name.
        set_search("");
        keel_set_sort(KEEL_SORT_SIZE_DESCENDING);
        keel_set_max(1);
        assert!(query(), "{}", last_message());
        assert_eq!((keel_get_num_results(), keel_get_total_results()), (1, 4));
        assert_eq!(
            full_path(0),
            files.path().join("notes.txt").display().to_string()
        );
        keel_set_max(u32::MAX);
        set_search("docs");
        assert!(query());
        assert_eq!(keel_get_num_results(), 1);
        assert_eq!(keel_is_folder_result(0), 1);
        assert_ne!(keel_get_result_attributes(0) & FILE_ATTRIBUTE_DIRECTORY, 0);

        // The match switches and the search in UTF-16.
        let upper: Vec<u16> = "INVOICE".encode_utf16().chain([0]).collect();
        assert_eq!(keel_set_search_w(upper.as_ptr()), 1);
        assert!(query());
        assert_eq!(keel_get_num_results(), 2);
        keel_set_match_case(1);
        assert!(query());
        assert_eq!(keel_get_num_results(), 0);
        keel_set_match_case(0);
        // A bad pattern is the caller's error; the previous (empty) results stay.
        set_search("(x");
        keel_set_regex(1);
        assert!(!query());
        assert_eq!(
            keel_get_last_error(),
            KEEL_ERROR_INVALIDPARAMETER,
            "{}",
            last_message()
        );
        assert_eq!(keel_get_num_results(), 0);
        keel_set_regex(0);

        // In the background, with a callback.
        set_search("invoice");
        keel_set_offset(1);
        let (tx, rx) = mpsc::channel::<u32>();
        let ud = &tx as *const mpsc::Sender<u32> as *mut c_void;
        assert_eq!(keel_query(0, Some(on_done), ud), 1);
        assert_eq!(rx.recv_timeout(Duration::from_secs(60)).unwrap(), KEEL_OK);
        assert_eq!((keel_get_num_results(), keel_get_total_results()), (1, 2));
        assert_eq!(full_path(0), umlaut.display().to_string());
        // Without a callback a background query is refused.
        assert_eq!(keel_query(0, None, std::ptr::null_mut()), 0);
        assert_eq!(keel_get_last_error(), KEEL_ERROR_INVALIDPARAMETER);

        // Reset keeps the profile; cleanup forgets it.
        keel_reset();
        assert_eq!(keel_get_last_error(), KEEL_OK);
        assert_eq!(keel_get_num_results(), 0);
        assert!(query());
        assert_eq!(keel_get_num_results(), 4, "the empty search, the same daemon");
        keel_cleanup();

        // The daemon gone: an IPC error.
        let name = daemon.name().to_owned();
        drop(daemon);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while keel_api::client::Client::connect(&name).is_ok() {
            assert!(std::time::Instant::now() < deadline, "the socket is still served");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(keel_set_profile(c_profile.as_ptr()), 1);
        assert!(!query());
        assert_eq!(keel_get_last_error(), KEEL_ERROR_IPC, "{}", last_message());
        keel_cleanup();
    }
    // Searching changed nothing.
    assert_eq!(snapshot(files.path()), before);
}
