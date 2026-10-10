use super::*;
use crate::index::tests::{eventually, walk};
use crate::library::tests::folder;
use crate::media::tests::{exif_jpeg, png};
use crate::JobStatus;

fn library(data: &Path, root: &Path) -> (Library, SourceId) {
    let lib = Library::open(data, "m").unwrap();
    lib.set_pause_on_battery(false);
    let id = lib.add_source(folder("photos", root)).unwrap();
    walk(&lib.source(&id).unwrap(), &lib.router()).unwrap();
    (lib, id)
}

fn media_rows(lib: &Library, id: &SourceId) -> i64 {
    lib.source(id)
        .unwrap()
        .store
        .get()
        .unwrap()
        .query_row("SELECT count(*) FROM media", [], |r| r.get(0))
        .unwrap()
}

fn run(lib: &Library, id: &SourceId) -> crate::JobInfo {
    let job = lib.media_job(id).unwrap();
    let info = lib.jobs().wait(job).unwrap();
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    info
}

#[test]
fn job_makes_sidecars_and_media_rows_and_survives_a_corrupt_file() {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    exif_jpeg(&files.path().join("a.jpg"));
    std::fs::create_dir_all(files.path().join("sub")).unwrap();
    png(&files.path().join("sub/b.png"), 300, 100);
    std::fs::write(files.path().join("broken.jpg"), b"\xff\xd8 broken").unwrap();
    std::fs::write(files.path().join("notes.txt"), b"not media").unwrap();
    let (lib, id) = library(data.path(), files.path());

    let info = run(&lib, &id);
    assert!(
        info.log
            .contains("3 media files: 2 made or moved, 1 failed"),
        "{}",
        info.log
    );
    assert!(info.log.contains("broken.jpg"), "{}", info.log);
    assert_eq!(media_rows(&lib, &id), 3);
    let sidecars = lib.sidecars().unwrap();
    assert_eq!(sidecars.root(), data.path().join("sidecars"));
    assert_eq!(sidecars.stats().keys, 3);

    let src = lib.source(&id).unwrap();
    let (w, h, o, camera, lat): (u32, u32, u8, String, f64) = src
        .store
        .get()
        .unwrap()
        .query_row(
            "SELECT width, height, orientation, camera, gps_lat FROM media m
             JOIN record r ON r.id = m.record WHERE r.name = 'a.jpg'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!((w, h, o, camera.as_str()), (20, 40, 6, "Keel Cam 1"));
    assert!((lat - 39.5).abs() < 1e-9);
    let hits: i64 = src
        .store
        .get()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM media_fts WHERE media_fts MATCH 'beach'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1, "XMP keywords searchable");

    // A second run does nothing (the corrupt file is not decoded again).
    let info = run(&lib, &id);
    assert!(
        info.log
            .contains("3 media files: 0 made or moved, 0 failed"),
        "{}",
        info.log
    );

    // Deleting the record deletes its media row.
    std::fs::remove_file(files.path().join("sub/b.png")).unwrap();
    walk(&src, &lib.router()).unwrap();
    assert_eq!(media_rows(&lib, &id), 2);
}

#[test]
fn sidecars_follow_a_content_id_that_appears_later() {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    png(&files.path().join("a.png"), 50, 50);
    let (lib, id) = library(data.path(), files.path());
    run(&lib, &id);
    let sidecars = lib.sidecars().unwrap();
    let before: Vec<_> = std::fs::read_dir(sidecars.root())
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(before.len(), 1);
    assert!(before[0].starts_with('p'), "keyed by path before hashing");

    let hash = lib.hash().unwrap();
    assert_eq!(lib.jobs().wait(hash).unwrap().status, JobStatus::Done);
    let info = run(&lib, &id);
    assert!(info.log.contains("1 made or moved"), "{}", info.log);
    let after: Vec<_> = std::fs::read_dir(sidecars.root())
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(after.len(), 1, "moved, not copied: {after:?}");
    assert_eq!(after[0].len(), 64, "keyed by content id: {after:?}");
    assert!(std::path::Path::new(sidecars.root())
        .join(&after[0])
        .join("thumb-256.webp")
        .is_file());
}

#[test]
fn a_stopped_job_resumes_from_its_checkpoint() {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    // Plenty past the first checkpoint, so the job is still running when the test sees it.
    let n = CHECKPOINT_EVERY + 1000;
    for i in 0..n {
        png(&files.path().join(format!("{i:04}.png")), 8, 8);
    }
    let (lib, id) = library(data.path(), files.path());
    let job = lib.media_job(&id).unwrap();
    eventually("first checkpoint", || {
        media_rows(&lib, &id) >= CHECKPOINT_EVERY as i64
    });
    // Pause (user activity), then close: the job stops at its pause.
    lib.shared.busy_until.store(u64::MAX, Ordering::SeqCst);
    drop(lib);

    let lib = Library::open(data.path(), "m").unwrap();
    lib.set_pause_on_battery(false);
    assert_eq!(lib.jobs().info(job).unwrap().status, JobStatus::Running);
    // `Library::open` registered the kind.
    assert_eq!(lib.jobs().resume_all().unwrap(), [job]);
    let info = lib.jobs().wait(job).unwrap();
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    assert!(info.log.contains("resumed"));
    // Counted once each: continued after the checkpoint, not from the start.
    assert!(
        info.log.contains(&format!("{n} media files")),
        "{}",
        info.log
    );
    assert_eq!(media_rows(&lib, &id), n as i64);
    assert_eq!(lib.sidecars().unwrap().stats().keys, n as u64);
}

#[test]
fn activity_pauses_the_job() {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    for i in 0..5 {
        png(&files.path().join(format!("{i}.png")), 8, 8);
    }
    let (lib, id) = library(data.path(), files.path());
    lib.shared.busy_until.store(u64::MAX, Ordering::SeqCst);
    let job = lib.media_job(&id).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(media_rows(&lib, &id), 0, "paused while the user is busy");
    lib.shared.busy_until.store(0, Ordering::SeqCst);
    assert_eq!(lib.jobs().wait(job).unwrap().status, JobStatus::Done);
    assert_eq!(media_rows(&lib, &id), 5);
}

/// `cargo test -p keel-core --release -- --ignored sidecar_perf --nocapture`
#[test]
#[ignore]
fn sidecar_perf_1000_images() {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    for i in 0..1000u32 {
        image::RgbImage::from_fn(64, 64, |x, y| {
            image::Rgb([(x * 4) as u8, (y * 4) as u8, i as u8])
        })
        .save(files.path().join(format!("{i:04}.png")))
        .unwrap();
    }
    let (lib, id) = library(data.path(), files.path());
    let start = Instant::now();
    run(&lib, &id);
    let took = start.elapsed();
    println!("1000 64x64 images -> Thumb256 + Meta: {took:?}");
    assert_eq!(media_rows(&lib, &id), 1000);
    assert!(took < Duration::from_secs(10), "{took:?}");
}
