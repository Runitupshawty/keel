use keel_search::{EverythingSearcher, Query, Searcher, Unavailable};

#[test]
fn loading_a_missing_dll_returns_an_error() {
    let missing = std::env::temp_dir().join(format!(
        "keel-missing-everything-{}-{}.dll",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));

    assert!(EverythingSearcher::load_from(&missing).is_err());

    let unavailable = Unavailable;
    assert!(!unavailable.available());
}

#[test]
fn everything_finds_its_configuration_when_enabled() {
    if std::env::var("KEEL_EVERYTHING_TEST").as_deref() != Ok("1") {
        eprintln!("skipping: set KEEL_EVERYTHING_TEST=1 to run this test");
        return;
    }

    let searcher = EverythingSearcher::load().expect("Everything64.dll should load");
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
