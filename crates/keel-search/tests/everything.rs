#![cfg(windows)]

use keel_search::{EverythingSearcher, Query, Searcher, Unavailable};

fn live() -> bool {
    let enabled = std::env::var("KEEL_EVERYTHING_TEST").as_deref() == Ok("1");
    if !enabled {
        eprintln!("skipping: set KEEL_EVERYTHING_TEST=1 to run this test");
    }
    enabled
}

#[test]
fn loading_a_missing_dll_returns_an_error() {
    let missing = std::env::temp_dir().join(format!(
        "keel-missing-everything-{}.dll",
        std::process::id()
    ));

    assert!(EverythingSearcher::load_from(&missing).is_err());

    let unavailable = Unavailable::new("Everything is not running");
    assert!(!unavailable.available());
}

#[test]
fn everything_finds_its_configuration_when_enabled() {
    if !live() {
        return;
    }

    let searcher = EverythingSearcher::load().expect("Everything64.dll should load");
    assert!(searcher.available());
    let hits = searcher
        .query(&Query {
            text: "Everything.ini".into(),
            ..Query::default()
        })
        .expect("Everything.exe should answer IPC queries");

    assert!(
        hits.iter()
            .any(|hit| hit.path.display().ends_with("Everything.ini")),
        "expected at least one Everything.ini hit, got {hits:#?}"
    );
}

#[test]
fn folders_only_regex_returns_folders() {
    if !live() {
        return;
    }

    let searcher = EverythingSearcher::load().expect("Everything64.dll should load");
    let hits = searcher
        .query(&Query {
            text: "^Windows$".into(),
            folders_only: true,
            regex: true,
            max: 50,
            ..Query::default()
        })
        .unwrap();

    assert!(!hits.is_empty(), "expected C:\\Windows to match");
    assert!(hits.iter().all(|hit| hit.is_dir && hit.size == 0));
}

#[test]
fn two_searchers_on_two_threads_get_their_own_results() {
    if !live() {
        return;
    }

    let run = |needle: &'static str| {
        std::thread::spawn(move || {
            let searcher = EverythingSearcher::load().expect("Everything64.dll should load");
            for _ in 0..50 {
                let hits = searcher
                    .query(&Query {
                        text: needle.into(),
                        max: 50,
                        ..Query::default()
                    })
                    .unwrap();
                assert!(!hits.is_empty(), "no hits for {needle}");
                for hit in &hits {
                    let path = hit.path.display().to_lowercase();
                    let name = path.rsplit('\\').next().unwrap();
                    assert!(name.contains(needle), "{needle} query returned {path}");
                }
            }
        })
    };

    let a = run("everything.ini");
    let b = run("notepad.exe");
    a.join().unwrap();
    b.join().unwrap();
}
