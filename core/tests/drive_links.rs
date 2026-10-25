//! End-to-end: when a drive counts as holding no symbolic links. Only the drive's own answer
//! at the top of the job's folder decides that, never one folder that can't be written to.

#![cfg(unix)]

use charon_core::model::SyncMode;
use charon_core::{ConflictRule, Engine, EngineEvent, JobSetup, Trigger};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
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

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// A folder whose permissions are changed for a while, and given back at the end.
struct Locked(PathBuf);

impl Locked {
    fn new(path: &Path, mode: u32) -> Locked {
        chmod(path, mode);
        Locked(path.to_path_buf())
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

fn i_am_unprivileged() -> bool {
    // Root with DAC override writes into a 555 folder: then the test proves nothing.
    let tmp = tempfile::tempdir().unwrap();
    let probe = tmp.path().join("x");
    fs::create_dir(&probe).unwrap();
    chmod(&probe, 0o000);
    let blocked = fs::read_dir(&probe).is_err();
    chmod(&probe, 0o755);
    blocked
}

/// Whether file permissions bite in this process. If not, the test can't make a folder
/// that can't be written to: it says so where everyone sees it and skips, or fails when
/// CHARON_REQUIRE_PERMISSION_TESTS=1.
fn permissions_bite(test: &str) -> bool {
    if i_am_unprivileged() {
        return true;
    }
    let how = "capsh --drop=cap_dac_override,cap_dac_read_search -- -c \"cargo test -p charon-core --test drive_links\"";
    let why = "this process can write into folders whose permissions forbid it (root with CAP_DAC_OVERRIDE), so the test would prove nothing";
    if std::env::var("CHARON_REQUIRE_PERMISSION_TESTS").is_ok_and(|v| v == "1") {
        panic!("{test} can't run: {why}. CHARON_REQUIRE_PERMISSION_TESTS=1 forbids skipping it. Run it with: {how}");
    }
    // Straight to stderr: the test harness swallows output of a test that passes, and a
    // skipped test must never look like one that ran.
    let _ = writeln!(
        std::io::stderr(),
        "SKIPPED {test}: {why}. It did NOT run. Run it with: {how} (or set CHARON_REQUIRE_PERMISSION_TESTS=1 to fail instead of skipping)"
    );
    false
}

#[test]
fn a_folder_that_cant_be_written_to_never_makes_the_drive_count_as_holding_no_links() {
    if !permissions_bite("a_folder_that_cant_be_written_to_never_makes_the_drive_count_as_holding_no_links") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (src, data) = (tmp.path().join("src"), tmp.path().join("data"));
    // tmpfs is a kind of drive Charon doesn't know, so it may answer "permission denied"
    // when it can't hold links.
    let shared_memory = Path::new("/dev/shm");
    let drive = if shared_memory.is_dir() { tempfile::tempdir_in(shared_memory) } else { tempfile::tempdir() }.unwrap();
    let dst = drive.path().join("dst");
    write(&src.join("locked/a.txt"), "a");
    write(&src.join("open/b.txt"), "b");
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, SyncMode::Backup, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);

    // One folder on the destination can't be written to; a link appears in each folder.
    let locked = Locked::new(&dst.join("locked"), 0o555);
    std::os::unix::fs::symlink("a.txt", src.join("locked/link")).unwrap();
    std::os::unix::fs::symlink("b.txt", src.join("open/link")).unwrap();
    let review = confirm(&engine, &id);
    assert_eq!(review.total_steps, 2, "{:?}", review.steps);
    let run = engine.job(&id).unwrap().job.last_run.unwrap();
    assert_eq!((run.copied, run.failed, run.left_out), (1, 1, 0), "only the locked folder's link failed: {run:?}");
    assert_eq!(fs::read_link(dst.join("open/link")).unwrap(), Path::new("b.txt"));
    let state = fs::read_to_string(data.join("charon-state.json")).unwrap();
    assert!(!state.contains("refusesLinks"), "the drive isn't recorded as one without links");

    // Once the folder can be written to again, its link is copied like any other.
    drop(locked);
    let next = confirm(&engine, &id);
    assert!(next.notes.iter().all(|note| !note.reason.contains("can't hold links")), "{:?}", next.notes);
    assert_eq!(fs::read_link(dst.join("locked/link")).unwrap(), Path::new("a.txt"));
}
