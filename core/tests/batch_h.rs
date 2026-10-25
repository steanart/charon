//! End-to-end: names, links and what a drive refuses, through the engine on real folders.

use charon_core::model::SyncMode;
use charon_core::plan::NoteKind;
use charon_core::{ConflictRule, Engine, EngineEvent, Error, JobSetup, SyncStart, Trigger};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Opens the engine once a previous one on `data` has let go of it (its threads may still
/// hold it for a moment after the last handle is dropped).
fn engine(data: &Path) -> Engine {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match Engine::open(data, Arc::new(|_: EngineEvent| {})) {
            Ok(engine) => return engine,
            Err(Error::AlreadyRunning) => {
                assert!(Instant::now() < deadline, "the previous engine never let go of the data folder");
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("the engine doesn't open: {e}"),
        }
    }
}

fn setup(src: &Path, dst: &Path, mode: SyncMode, conflict: ConflictRule) -> JobSetup {
    JobSetup {
        name: String::new(),
        source: src.to_path_buf(),
        destination: dst.to_path_buf(),
        mode,
        conflict,
        trigger: Trigger::Manual,
        excludes: JobSetup::default_excludes(),
        safe_delete: true,
        full_automation: false,
    }
}

/// Waits until no sync of the job runs.
fn wait(engine: &Engine, id: &String) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while engine.is_running(id) {
        assert!(Instant::now() < deadline, "sync took too long");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Looks at the job, confirms the list as it is, and waits for the sync to finish.
fn confirm(engine: &Engine, id: &String) -> charon_core::model::Review {
    let review = engine.prepare_review(id).unwrap();
    engine.approve(id, &review.review_id, true).unwrap();
    wait(engine, id);
    review
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[cfg(unix)]
#[test]
fn a_mirror_keeps_a_file_the_source_replaced_with_a_link() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("clip.mov"), "the only copy");
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, SyncMode::Mirror, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);

    // The film moves to an external drive; a link is left in its place.
    write(&tmp.path().join("ssd/clip.mov"), "the only copy");
    fs::remove_file(src.join("clip.mov")).unwrap();
    std::os::unix::fs::symlink(tmp.path().join("ssd/clip.mov"), src.join("clip.mov")).unwrap();

    let review = confirm(&engine, &id);
    assert_eq!(review.tally.destination.removed, 0, "{:?}", review.steps);
    assert!(review.notes.iter().any(|n| n.kind == NoteKind::Blocked && n.path == "clip.mov"), "{:?}", review.notes);
    let kept = fs::symlink_metadata(dst.join("clip.mov")).unwrap();
    assert!(kept.is_file(), "the backup still holds the file");
    assert_eq!(fs::read_to_string(dst.join("clip.mov")).unwrap(), "the only copy");
}

#[cfg(unix)]
#[test]
fn two_way_asks_before_a_link_takes_a_files_place() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("clip.mov"), "the only copy");
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);

    fs::remove_file(a.join("clip.mov")).unwrap();
    std::os::unix::fs::symlink("/somewhere/else/clip.mov", a.join("clip.mov")).unwrap();
    let review = confirm(&engine, &id);
    assert!(review.steps.iter().any(|s| s.kind == charon_core::model::StepKind::Conflict && s.path == "clip.mov"), "{:?}", review.steps);
    assert_eq!(review.tally.destination.removed, 0);
    assert!(fs::symlink_metadata(b.join("clip.mov")).unwrap().is_file());
    assert_eq!(fs::read_to_string(b.join("clip.mov")).unwrap(), "the only copy");
}

#[test]
fn a_file_named_like_an_old_half_copy_is_listed_and_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("a.txt"), "a");
    write(&b.join("notes.charon-partial"), "my own notes");
    let engine = engine(&data);
    let mut job = setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask);
    job.full_automation = true;
    let id = engine.add_job(job).unwrap().job.id;

    let review = confirm(&engine, &id);
    let note = review.notes.iter().find(|n| n.path == "notes.charon-partial").expect("the list names it");
    assert_eq!(note.kind, NoteKind::LeftOut);
    assert!(note.reason.contains("Charon 0.2"), "{}", note.reason);
    assert!(review.housekeeping.is_empty(), "a file just written is never tidied: {:?}", review.housekeeping);

    write(&a.join("b.txt"), "b");
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait(&engine, &id);
    assert_eq!(fs::read_to_string(b.join("notes.charon-partial")).unwrap(), "my own notes");
    assert!(!a.join("notes.charon-partial").exists());
}

#[cfg(unix)]
#[test]
fn a_drive_found_refusing_links_still_does_after_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("a.txt"), "a");
    fs::create_dir_all(&dst).unwrap();
    let id = {
        let engine = engine(&data);
        let id = engine.add_job(setup(&src, &dst, SyncMode::Backup, ConflictRule::Ask)).unwrap().job.id;
        confirm(&engine, &id);
        id
    };
    // What a sync records when the destination's drive turns out to refuse links (some
    // network shares do).
    let state_path = data.join("charon-state.json");
    let mut state: serde_json::Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    let drive = &mut state["jobs"][0]["drives"]["destination"];
    assert!(drive.is_object(), "the sync recorded its drives");
    drive["refusesLinks"] = serde_json::json!(true);
    fs::write(&state_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
    std::os::unix::fs::symlink("a.txt", src.join("link")).unwrap();

    let engine = engine(&data);
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.total_steps, 0, "{:?}", review.steps);
    assert!(review.notes.iter().any(|n| n.path == "link" && n.reason.contains("can't hold links")), "{:?}", review.notes);
}

/// A folder whose own path is `length` bytes long, inside `parent`.
#[cfg(target_os = "linux")]
fn deep_folder(parent: &Path, length: usize) -> std::path::PathBuf {
    let mut folder = parent.join("dst");
    while folder.as_os_str().len() + 201 < length {
        folder.push("d".repeat(200));
    }
    let rest = length - folder.as_os_str().len() - 1;
    folder.push("e".repeat(rest));
    fs::create_dir_all(&folder).unwrap();
    folder
}

#[cfg(target_os = "linux")]
#[test]
fn a_path_too_long_for_the_destination_is_left_out_and_holds_back_removals() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, data) = (tmp.path().join("a"), tmp.path().join("data"));
    // Paths on Linux end at 4,096 bytes: this folder leaves room for short names only.
    let b = deep_folder(tmp.path(), 3900);
    write(&a.join("keep.txt"), "k");
    write(&a.join("old.txt"), "o");
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);
    assert!(b.join("old.txt").exists());

    let long = format!("long/{}.txt", "x".repeat(200));
    write(&a.join(&long), "fits in a, not in b");
    fs::remove_file(a.join("old.txt")).unwrap();
    let review = confirm(&engine, &id);
    assert_eq!(review.tally.destination.removed, 1);
    let run = engine.job(&id).unwrap().job.last_run.unwrap();
    assert_eq!((run.left_out, run.failed), (1, 0), "{run:?}");
    assert!(b.join("old.txt").exists(), "nothing is removed there in a run that couldn't copy everything");

    // The next list leaves the item out, saying why, and carries the removal.
    let next = engine.prepare_review(&id).unwrap();
    assert!(next.steps.iter().all(|s| s.path != long), "{:?}", next.steps);
    let note = next.notes.iter().find(|n| n.path == long).expect("the list names it");
    assert_eq!(note.kind, NoteKind::LeftOut);
    assert!(note.reason.contains("an earlier sync found") && note.reason.contains("too long"), "{}", note.reason);
    assert_eq!(next.tally.destination.removed, 1);
    engine.approve(&id, &next.review_id, true).unwrap();
    wait(&engine, &id);
    assert!(!b.join("old.txt").exists());

    // Once the item changes, it's worth another try.
    write(&a.join(&long), "edited");
    let again = engine.prepare_review(&id).unwrap();
    assert!(again.steps.iter().any(|s| s.path == long), "{:?}", again.steps);
}
