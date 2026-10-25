//! End-to-end: what a scan leaves alone on one side (a socket, Charon's own leftover links),
//! and what that means for the other side, through the engine on real folders.

#![cfg(unix)]

use charon_core::model::SyncMode;
use charon_core::plan::{NoteKind, Side};
use charon_core::{ConflictRule, Engine, EngineEvent, JobSetup, Trigger};
use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn engine(data: &Path) -> Engine {
    Engine::open(data, Arc::new(|_: EngineEvent| {})).unwrap()
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

/// Every file and link under `root`.
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(items) = fs::read_dir(root) {
        for item in items.flatten() {
            let path = item.path();
            if fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
                found.extend(walk(&path));
            } else {
                found.push(path);
            }
        }
    }
    found
}

#[test]
fn a_socket_keeps_the_other_sides_item_of_its_name_out_of_the_sync() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("keep.txt"), "k");
    write(&a.join("gone.txt"), "g");
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);

    // An app on the source listens on a socket named "x"; the destination gets a file of
    // that name, and loses gone.txt.
    let _socket = std::os::unix::net::UnixListener::bind(a.join("x")).unwrap();
    write(&b.join("x"), "a file on the destination");
    fs::remove_file(b.join("gone.txt")).unwrap();

    let review = confirm(&engine, &id);
    assert!(review.steps.iter().all(|step| step.path != "x"), "nothing is copied onto the socket: {:?}", review.steps);
    // One note, on the side that holds the socket, keeps the name out of the sync on both.
    let notes: Vec<_> = review.notes.iter().filter(|note| note.path == "x").collect();
    assert_eq!(notes.len(), 1, "{:?}", review.notes);
    assert_eq!((notes[0].kind, notes[0].side), (NoteKind::LeftOut, Some(Side::Source)));
    assert!(notes[0].reason.contains("socket"), "{}", notes[0].reason);
    let run = engine.job(&id).unwrap().job.last_run.unwrap();
    assert_eq!((run.failed, run.skipped), (0, 0), "{run:?}");
    assert!(!a.join("gone.txt").exists(), "the run's removals on the source still go ahead");
    assert_eq!(fs::read_to_string(b.join("x")).unwrap(), "a file on the destination");
    assert!(fs::symlink_metadata(a.join("x")).unwrap().file_type().is_socket());
}

#[test]
fn a_link_left_under_a_temporary_name_by_an_interrupted_sync_is_tidied_away() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("photos/cat.jpg"), "meow");
    write(&dst.join("photos/cat.jpg"), "meow");
    // A sync that moved a link aside two hours ago, to replace it, never finished.
    let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() - 2 * 3600;
    let leftover = dst.join(format!("photos/.charon-{started:08x}{:08x}.part", 0x1234u32));
    std::os::unix::fs::symlink("../elsewhere/cat.jpg", &leftover).unwrap();
    let long_ago = filetime::FileTime::from_unix_time(started as i64, 0);
    filetime::set_symlink_file_times(&leftover, long_ago, long_ago).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, SyncMode::Backup, ConflictRule::Ask)).unwrap().job.id;

    let review = confirm(&engine, &id);
    assert!(review.notes.iter().all(|note| !note.path.contains(".charon-")), "Charon's own link isn't told to the person as theirs: {:?}", review.notes);
    assert!(review.housekeeping.iter().any(|line| line.contains("1 item a sync left under a temporary name")), "{:?}", review.housekeeping);
    assert!(fs::symlink_metadata(&leftover).is_err(), "tidied away");
    let kept = walk(&dst.join(".charon-trash"));
    assert!(kept.iter().any(|path| fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())), "into the trash: {kept:?}");
    assert_eq!(fs::read_to_string(dst.join("photos/cat.jpg")).unwrap(), "meow");
}

/// Finder leaves a `.DS_Store` in every folder it shows: in Charon's trash when the person
/// looks there for a removed file, in the hidden `.charon` folder too. Both stay Charon's,
/// and nothing says an item can't be synced.
#[test]
fn finders_clutter_in_charons_folders_leaves_them_charons() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("keep.txt"), "k");
    write(&a.join("gone.txt"), "g");
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);
    fs::remove_file(a.join("gone.txt")).unwrap();
    confirm(&engine, &id);
    assert!(b.join(".charon-trash").is_dir() && b.join(".charon").is_dir());

    write(&b.join(".charon-trash/.DS_Store"), "Finder");
    write(&b.join(".charon/.DS_Store"), "Finder");
    let review = engine.prepare_review(&id).unwrap();
    assert!(review.notes.is_empty(), "{:?}", review.notes);
    assert_eq!(review.total_steps, 0);
    assert!(review.warnings.iter().all(|warning| !warning.text.contains("can't be synced")), "{:?}", review.warnings);
}
