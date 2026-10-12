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

/// Match Path and Match Whole Words reach Everything as its search switches and Match
/// Diacritics as its `diacritics:` modifier around the whole query.
#[test]
fn toggles_reach_everything_when_enabled() {
    if !live() {
        return;
    }

    let id = std::process::id();
    let dir = std::env::temp_dir().join(format!("keel-everything-toggles-{id}"));
    std::fs::create_dir_all(&dir).unwrap();
    let tag = format!("keelev{id}");
    std::fs::write(dir.join(format!("caf\u{e9} {tag}.txt")), b"x").unwrap();
    let searcher = EverythingSearcher::load().expect("Everything64.dll should load");
    let find = |text: String, set: fn(&mut Query)| -> usize {
        let mut query = Query {
            text,
            ..Query::default()
        };
        set(&mut query);
        searcher.query(&query).unwrap().len()
    };
    // Everything sees a new file within moments.
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while find(tag.clone(), |_| {}) == 0 {
        assert!(std::time::Instant::now() < end, "Everything did not index {tag}");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let accented = format!("caf\u{e9} {tag}");
    assert_eq!(find(accented.clone(), |q| q.match_diacritics = true), 1);
    assert_eq!(find(format!("cafe {tag}"), |q| q.match_diacritics = true), 0);
    let folder = format!("keel-everything-toggles-{id} {tag}");
    assert_eq!(find(folder.clone(), |_| {}), 0, "the folder is not in the name");
    assert_eq!(find(folder, |q| q.match_path = true), 1);
    assert_eq!(find(tag[..6].to_owned(), |q| q.whole_word = true), 0);
    assert_eq!(find(tag.clone(), |q| q.whole_word = true), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
