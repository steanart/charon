//! End-to-end: an empty file an interrupted sync left holding a name (see
//! `scan::is_claim`) is never taken for the person's file: it isn't copied, kept twice or
//! counted as a conflict, a confirmed sync moves it into the trash, and the real file
//! lands at the name afterwards.
//!
//! The folders are on tmpfs, which keeps no change times Charon trusts, as on the network
//! shares where such files are left: a leftover is then tidied at the first sync.

#![cfg(target_os = "linux")]

use charon_core::model::SyncMode;
use charon_core::scan::CLAIM_MODIFIED_MS;
use charon_core::{ConflictRule, Engine, EngineEvent, JobSetup, Trigger};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

fn engine(data: &Path) -> Engine {
    Engine::open(data, Arc::new(|_: EngineEvent| {})).unwrap()
}

fn setup(src: &Path, dst: &Path) -> JobSetup {
    JobSetup {
        name: String::new(),
        source: src.to_path_buf(),
        destination: dst.to_path_buf(),
        mode: SyncMode::TwoWay,
        conflict: ConflictRule::Ask,
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

/// An empty file holding `path`'s name, as an interrupted sync leaves it.
fn leave_claim(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let claim = fs::File::create(path).unwrap();
    claim.set_modified(UNIX_EPOCH + Duration::from_millis(CLAIM_MODIFIED_MS as u64)).unwrap();
}

/// Every file under `root`.
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(items) = fs::read_dir(root) {
        for item in items.flatten() {
            let path = item.path();
            if path.is_dir() {
                found.extend(walk(&path));
            } else {
                found.push(path);
            }
        }
    }
    found
}

/// Three folders on tmpfs: the source, the destination and Charon's data.
fn folders() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let shared_memory = Path::new("/dev/shm");
    assert!(shared_memory.is_dir(), "these tests need a folder on tmpfs");
    let tmp = tempfile::tempdir_in(shared_memory).unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    for folder in [&src, &dst] {
        fs::create_dir_all(folder).unwrap();
    }
    (tmp, src, dst, data)
}

#[test]
fn a_name_held_for_a_copy_gets_the_real_file_once_the_holder_is_tidied() {
    let (_tmp, src, dst, data) = folders();
    write(&src.join("docs/report.txt"), "the real report");
    write(&src.join("seed.txt"), "seed");
    leave_claim(&dst.join("docs/report.txt"));
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst)).unwrap().job.id;

    let review = confirm(&engine, &id);
    assert_eq!(review.tally.conflicts + review.tally.keep_both, 0, "an empty holder is no version of the report");
    assert!(review.housekeeping.iter().any(|line| line.contains("“docs/report.txt”") && line.contains("to hold the name for a copy")), "{:?}", review.housekeeping);
    assert!(dst.join(".charon-trash").is_dir(), "the holder went into the trash");

    confirm(&engine, &id);
    assert_eq!(fs::read_to_string(dst.join("docs/report.txt")).unwrap(), "the real report");
    assert_eq!(fs::read_to_string(src.join("docs/report.txt")).unwrap(), "the real report");
    let everywhere: Vec<PathBuf> = walk(&src).into_iter().chain(walk(&dst)).collect();
    assert!(!everywhere.iter().any(|path| path.to_string_lossy().contains("conflict")), "{everywhere:?}");
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.total_steps, 0, "in sync");
}

#[test]
fn a_name_held_on_one_side_only_is_never_copied_to_the_other() {
    let (_tmp, src, dst, data) = folders();
    write(&src.join("seed.txt"), "seed");
    leave_claim(&dst.join("notes.txt"));
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst)).unwrap().job.id;

    let review = confirm(&engine, &id);
    assert_eq!(review.tally.source.new_files, 0, "{:?}", review.tally);
    assert!(!src.join("notes.txt").exists(), "the empty holder isn't the person's file");
    assert!(!dst.join("notes.txt").exists(), "it went into the trash");
}
