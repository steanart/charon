//! End-to-end: how a drive treats upper and lower case is asked of the drive itself, also
//! when several looks at an empty folder happen at once, so what Charon remembers of the
//! last sync stays trusted.

#![cfg(target_os = "linux")]

use charon_core::model::SyncMode;
use charon_core::{ConflictRule, Engine, EngineEvent, JobSetup, Trigger};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

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

/// A folder on tmpfs: a kind of drive whose case setting Charon can only find out by asking.
fn folder_on_tmpfs() -> tempfile::TempDir {
    let shared_memory = Path::new("/dev/shm");
    if shared_memory.is_dir() { tempfile::tempdir_in(shared_memory) } else { tempfile::tempdir() }.unwrap()
}

#[test]
fn opening_the_list_of_a_new_job_while_it_is_looked_at_keeps_the_next_sync_trusted() {
    // The two looks meet in the folder only now and then, so it takes many rounds.
    for round in 0..200 {
        let tmp = folder_on_tmpfs();
        let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("Notes.txt"), "keep").unwrap();
        fs::write(src.join("Draft.txt"), "remove later").unwrap();
        let engine = engine(&data);

        // Adding the job starts a look of its own; the person opens the list right away.
        let id = engine.add_job(setup(&src, &dst)).unwrap().job.id;
        let review = engine.prepare_review(&id).unwrap();
        engine.approve(&id, &review.review_id, true).unwrap();
        wait(&engine, &id);
        assert_eq!(fs::read_to_string(dst.join("Draft.txt")).unwrap(), "remove later", "round {round}");

        fs::remove_file(src.join("Draft.txt")).unwrap();
        let review = engine.prepare_review(&id).unwrap();
        let warnings: Vec<&str> = review.warnings.iter().map(|warning| warning.text.as_str()).collect();
        assert!(!warnings.iter().any(|text| text.contains("upper and lower case")), "round {round}: {warnings:?}");
        engine.approve(&id, &review.review_id, true).unwrap();
        wait(&engine, &id);
        assert!(!dst.join("Draft.txt").exists(), "round {round}: the removal reached the destination, so the memory was trusted");
        assert_eq!(fs::read_to_string(dst.join("Notes.txt")).unwrap(), "keep");
    }
}
