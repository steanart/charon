//! End-to-end: files a scan can list but a copy can't read (no permission, or another
//! program holds them), through the engine on real folders.
//!
//! The tests make files unreadable with their permissions, so they must run without
//! CAP_DAC_OVERRIDE when the process is root. Otherwise they skip, and say so on stderr;
//! with CHARON_REQUIRE_PERMISSION_TESTS=1 they fail instead of skipping (for CI):
//!
//! ```text
//! capsh --drop=cap_dac_override,cap_dac_read_search -- -c \
//!     "cargo test -p charon-core --test unreadable_files"
//! ```

#![cfg(unix)]

use charon_core::model::{StepKind, SyncMode, PLACES_SHOWN};
use charon_core::plan::{NoteKind, Side};
use charon_core::{ConflictRule, Engine, EngineEvent, Error, JobSetup, JobStatus, Review, SyncStart, Trigger};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
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

fn setup(src: &Path, dst: &Path, mode: SyncMode) -> JobSetup {
    JobSetup {
        name: String::new(),
        source: src.to_path_buf(),
        destination: dst.to_path_buf(),
        mode,
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
fn confirm(engine: &Engine, id: &String) -> Review {
    let review = engine.prepare_review(id).unwrap();
    engine.approve(id, &review.review_id, true).unwrap();
    wait(engine, id);
    review
}

/// A background look at the job, waited for.
fn look(engine: &Engine, id: &String) {
    engine.check(id);
    let deadline = Instant::now() + Duration::from_secs(30);
    while engine.job(id).unwrap().looking {
        assert!(Instant::now() < deadline, "the look took too long");
        thread::sleep(Duration::from_millis(10));
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// Writes `text` with a modification time `seconds` after 2020 began, so an edit always
/// shows as one.
fn write_at(path: &Path, text: &str, seconds: i64) {
    write(path, text);
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(1_577_836_800 + seconds, 0)).unwrap();
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Whether file permissions bite in this process. If not (root with CAP_DAC_OVERRIDE), the
/// test can't make a file unreadable: it says so where everyone sees it and skips, or fails
/// when CHARON_REQUIRE_PERMISSION_TESTS=1.
fn permissions_bite(test: &str) -> bool {
    let tmp = tempfile::tempdir().unwrap();
    let probe = tmp.path().join("x");
    fs::write(&probe, "x").unwrap();
    chmod(&probe, 0o000);
    if fs::read(&probe).is_err() {
        return true;
    }
    let how = "capsh --drop=cap_dac_override,cap_dac_read_search -- -c \"cargo test -p charon-core --test unreadable_files\"";
    let why = "this process can read files whose permissions are 000 (root with CAP_DAC_OVERRIDE), so it can't make a file \
               unreadable and the test would prove nothing";
    if std::env::var("CHARON_REQUIRE_PERMISSION_TESTS").is_ok_and(|v| v == "1") {
        panic!("{test} can't run: {why}. CHARON_REQUIRE_PERMISSION_TESTS=1 forbids skipping it. Run it with: {how}");
    }
    // Straight to stderr: the test harness swallows eprintln! of a test that passes, and a
    // skipped safety test must never look like one that ran.
    let _ = writeln!(
        std::io::stderr(),
        "SKIPPED {test}: {why}. It did NOT run. Run it with: {how} (or set CHARON_REQUIRE_PERMISSION_TESTS=1 to fail instead of skipping)"
    );
    false
}

fn steps_for<'a>(review: &'a Review, path: &str) -> Vec<&'a charon_core::model::ReviewStep> {
    review.steps.iter().filter(|step| step.path == path).collect()
}

#[test]
fn a_backup_leaves_out_a_file_it_cannot_read_and_never_turns_red() {
    if !permissions_bite("a_backup_leaves_out_a_file_it_cannot_read_and_never_turns_red") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("a.txt"), "a");
    write(&src.join("Outlook Files/me.pst"), "mail");
    chmod(&src.join("Outlook Files/me.pst"), 0o000);
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, SyncMode::Backup)).unwrap().job.id;

    // The scan can list the file, so the first list copies it; the copy can't read it.
    let first = confirm(&engine, &id);
    assert_eq!(steps_for(&first, "Outlook Files/me.pst").len(), 1, "{:?}", first.steps);
    assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "a", "everything else crosses");
    assert!(!dst.join("Outlook Files/me.pst").exists());
    let view = engine.job(&id).unwrap();
    let run = view.job.last_run.clone().unwrap();
    assert_eq!((run.failed, run.unreadable, run.found_unreadable, run.error.as_deref()), (0, 1, 1, None), "{run:?}");
    assert_eq!(view.status, JobStatus::InSync, "not an error: {:?}", view.problem);
    assert_eq!(view.problem, None);
    // The row lists it among the places Charon couldn't read, where it can be left out.
    let place = view.unreadable.iter().find(|place| place.path == "Outlook Files/me.pst").expect("listed at once");
    assert_eq!((place.kind, place.side, place.reason.as_str()), (NoteKind::Unreadable, Side::Source, "permission denied"));
    let overview = engine.overview();
    assert_eq!(overview.attention, 0);
    assert_eq!(overview.headline(), "1 job doesn't sync everything");
    let log: Vec<String> = engine.log(&id, 50).into_iter().map(|line| line.text).collect();
    assert!(log.iter().any(|line| line.starts_with("Outlook Files/me.pst: couldn't be read")), "{log:?}");

    // A background look lists it too, and the job stays as it is.
    look(&engine, &id);
    let view = engine.job(&id).unwrap();
    assert_eq!(view.status, JobStatus::InSync, "{:?}", view.problem);
    assert!(view.unreadable.iter().any(|place| place.path == "Outlook Files/me.pst"), "{:?}", view.unreadable);

    // The next list leaves it out with a note that offers to leave it out for good, and
    // carries what's new.
    write(&src.join("new.txt"), "n");
    let next = engine.prepare_review(&id).unwrap();
    assert!(steps_for(&next, "Outlook Files/me.pst").is_empty(), "{:?}", next.steps);
    let note = next.notes.iter().find(|note| note.path == "Outlook Files/me.pst").expect("the list names it");
    assert_eq!((note.kind, note.side, note.can_leave_out), (NoteKind::Unreadable, Some(Side::Source), true));
    engine.approve(&id, &next.review_id, true).unwrap();
    wait(&engine, &id);
    assert!(dst.join("new.txt").exists());
    let view = engine.job(&id).unwrap();
    assert_eq!(view.status, JobStatus::InSync, "{:?}", view.problem);
    let run = view.job.last_run.unwrap();
    assert_eq!((run.failed, run.unreadable), (0, 1));
    assert_eq!(run.found_unreadable, 0, "it was on the list already: nothing new to mention");

    // "Leave this out" takes it off the list for good.
    engine.leave_out(&id, Side::Source, "Outlook Files/me.pst").unwrap();
    let after = engine.prepare_review(&id).unwrap();
    assert!(after.notes.iter().all(|note| note.path != "Outlook Files/me.pst"), "{:?}", after.notes);
    assert!(steps_for(&after, "Outlook Files/me.pst").is_empty());
}

#[test]
fn a_row_counts_every_place_it_cannot_read_although_it_lists_only_the_first_hundred() {
    if !permissions_bite("a_row_counts_every_place_it_cannot_read_although_it_lists_only_the_first_hundred") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    // One folder more than a row lists, and a file only the copy finds it can't read.
    let folder = |i: usize| src.join(format!("locked {i:03}"));
    for i in 0..=PLACES_SHOWN {
        write(&folder(i).join("a.txt"), "a");
        chmod(&folder(i), 0o000);
    }
    write(&src.join("me.pst"), "mail");
    chmod(&src.join("me.pst"), 0o000);
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, SyncMode::Backup)).unwrap().job.id;
    let shown = |engine: &Engine| {
        let view = engine.job(&id).unwrap();
        (view.unreadable.len(), view.unreadable_count)
    };

    look(&engine, &id);
    assert_eq!(shown(&engine), (PLACES_SHOWN, PLACES_SHOWN as u64 + 1));

    // The sync finds the file; the row counts it, although its list is full.
    confirm(&engine, &id);
    assert_eq!(engine.job(&id).unwrap().job.last_run.unwrap().unreadable, PLACES_SHOWN as u64 + 2);
    assert_eq!(shown(&engine), (PLACES_SHOWN, PLACES_SHOWN as u64 + 2));
    look(&engine, &id);
    assert_eq!(shown(&engine), (PLACES_SHOWN, PLACES_SHOWN as u64 + 2));

    for i in 0..=PLACES_SHOWN {
        chmod(&folder(i), 0o755);
    }
}

#[test]
fn a_file_that_can_be_read_again_is_copied_at_the_next_sync() {
    if !permissions_bite("a_file_that_can_be_read_again_is_copied_at_the_next_sync") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("locked.dat"), "locked");
    chmod(&src.join("locked.dat"), 0o000);
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, SyncMode::Mirror)).unwrap().job.id;
    confirm(&engine, &id);
    assert!(!dst.join("locked.dat").exists());

    chmod(&src.join("locked.dat"), 0o644);
    let review = confirm(&engine, &id);
    assert_eq!(steps_for(&review, "locked.dat").len(), 1, "{:?}", review.steps);
    assert_eq!(fs::read_to_string(dst.join("locked.dat")).unwrap(), "locked");
    let view = engine.job(&id).unwrap();
    assert!(view.unreadable.is_empty(), "{:?}", view.unreadable);
    assert_eq!(view.job.last_run.unwrap().unreadable, 0);
}

/// Two copies of a file that only their dates tell apart, one of them unreadable for a
/// while, on a drive that keeps no change times (tmpfs here; Windows, FAT and exFAT drives
/// and network shares too), where a file given its permissions back looks just as before:
/// the comparison that couldn't read it counts for its own look only. Once the file can be
/// read again, the next look finds both copies identical.
#[cfg(target_os = "linux")]
#[test]
fn a_comparison_that_could_not_read_a_file_counts_for_that_look_only() {
    if !permissions_bite("a_comparison_that_could_not_read_a_file_counts_for_that_look_only") {
        return;
    }
    let tmp = tempfile::tempdir_in("/dev/shm").unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write_at(&a.join("report.doc"), "the same report", 0);
    write_at(&b.join("report.doc"), "the same report", 60);
    chmod(&b.join("report.doc"), 0o000);
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay)).unwrap().job.id;
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.conflicts, 1, "as if they differ: {:?}", review.steps);

    chmod(&b.join("report.doc"), 0o644);
    let review = engine.prepare_review(&id).unwrap();
    assert!(steps_for(&review, "report.doc").is_empty(), "{:?}", review.steps);
    assert_eq!(review.tally.agreed, 1);
}

#[test]
fn a_two_way_file_that_cannot_be_read_is_never_removed_on_the_other_side() {
    if !permissions_bite("a_two_way_file_that_cannot_be_read_is_never_removed_on_the_other_side") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write_at(&a.join("keep.txt"), "keep", 0);
    write_at(&a.join("locked.dat"), "v1", 0);
    write_at(&a.join("old.txt"), "old", 0);
    fs::create_dir_all(&b).unwrap();
    let id = {
        let engine = engine(&data);
        let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay)).unwrap().job.id;
        confirm(&engine, &id);
        assert_eq!(fs::read_to_string(b.join("locked.dat")).unwrap(), "v1");

        // The file is edited, then held where Charon can't read it; another file goes.
        write_at(&a.join("locked.dat"), "v2, edited", 60);
        chmod(&a.join("locked.dat"), 0o000);
        fs::remove_file(a.join("old.txt")).unwrap();
        let review = confirm(&engine, &id);
        assert_eq!(review.tally.destination.removed, 1);
        assert_eq!(fs::read_to_string(b.join("locked.dat")).unwrap(), "v1", "the other side's version stays");
        assert!(b.join("old.txt").exists(), "nothing is removed there in a run that couldn't copy everything");
        let view = engine.job(&id).unwrap();
        assert_ne!(view.status, JobStatus::Error, "{:?}", view.problem);
        assert_eq!(view.job.last_run.clone().unwrap().failed, 0);
        assert!(view.unreadable.iter().any(|place| place.path == "locked.dat" && place.side == Side::Source), "{:?}", view.unreadable);

        // The next list leaves it alone on both sides, and the removal still waits while it
        // can't be copied: what it holds may be in the file that goes.
        let next = engine.prepare_review(&id).unwrap();
        assert!(steps_for(&next, "locked.dat").is_empty(), "{:?}", next.steps);
        assert!(next.notes.iter().any(|note| note.path == "locked.dat" && note.kind == NoteKind::Unreadable && note.can_leave_out));
        assert_eq!(next.tally.destination.removed, 0, "{:?}", next.steps);
        let kept = next.notes.iter().find(|note| note.path == "old.txt").expect("the list says why it stays");
        assert_eq!((kept.kind, kept.side), (NoteKind::Blocked, Some(Side::Destination)));
        assert!(kept.reason.contains("“locked.dat”"), "{}", kept.reason);
        engine.approve(&id, &next.review_id, true).unwrap();
        wait(&engine, &id);
        assert!(b.join("old.txt").exists());
        assert_eq!(fs::read_to_string(b.join("locked.dat")).unwrap(), "v1");
        id
    };

    // After a restart Charon no longer remembers it: the edit is tried again, as a copy,
    // never as a removal of the file on either side.
    let engine = engine(&data);
    let review = engine.prepare_review(&id).unwrap();
    let steps = steps_for(&review, "locked.dat");
    assert_eq!(steps.len(), 1, "{:?}", review.steps);
    assert_eq!((steps[0].kind, steps[0].side), (StepKind::Replace, Some(Side::Destination)));
    assert_eq!(review.tally.source.removed, 0, "{:?}", review.steps);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait(&engine, &id);
    assert_eq!(fs::read_to_string(b.join("locked.dat")).unwrap(), "v1");
    assert!(b.join("old.txt").exists(), "a run that couldn't copy everything removes nothing there");
    assert_eq!(engine.job(&id).unwrap().job.last_run.unwrap().found_unreadable, 1);

    // Readable again, the edit crosses, and then the removal.
    chmod(&a.join("locked.dat"), 0o644);
    confirm(&engine, &id);
    assert_eq!(fs::read_to_string(b.join("locked.dat")).unwrap(), "v2, edited");
    assert!(!b.join("old.txt").exists());
    assert_eq!(fs::read_to_string(b.join("keep.txt")).unwrap(), "keep");
}

/// A file renamed, then held where Charon can't read it (Outlook keeps its mailbox open,
/// and goes on writing to it): the old name on the other side may be the only copy of it
/// Charon can read. It stays there, run after run, until the new name is copied.
fn a_renamed_file_that_cannot_be_read_keeps_its_old_name(mode: SyncMode) {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write_at(&src.join("keep.txt"), "keep", 0);
    write_at(&src.join("Outlook Files/old.pst"), "mail", 0);
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let mut job = setup(&src, &dst, mode);
    job.full_automation = true;
    job.safe_delete = false;
    let id = engine.add_job(job).unwrap().job.id;
    confirm(&engine, &id);
    assert_eq!(fs::read_to_string(dst.join("Outlook Files/old.pst")).unwrap(), "mail");

    let new = src.join("Outlook Files/new.pst");
    fs::rename(src.join("Outlook Files/old.pst"), &new).unwrap();
    chmod(&new, 0o000);
    for round in 1..=3 {
        if round == 3 {
            // The program that holds it writes to it.
            chmod(&new, 0o600);
            write_at(&new, "mail, and more mail", 120);
            chmod(&new, 0o000);
        }
        assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started, "round {round}");
        wait(&engine, &id);
        assert_eq!(fs::read_to_string(dst.join("Outlook Files/old.pst")).unwrap(), "mail", "round {round}: the only copy Charon can read stays");
        assert!(!dst.join("Outlook Files/new.pst").exists());
        let view = engine.job(&id).unwrap();
        assert_ne!(view.status, JobStatus::Error, "round {round}: {:?}", view.problem);
        let run = view.job.last_run.unwrap();
        assert_eq!((run.failed, run.deleted, run.error.as_deref()), (0, 0, None), "round {round}: {run:?}");
        assert!(view.unreadable.iter().any(|place| place.path == "Outlook Files/new.pst"), "round {round}: {:?}", view.unreadable);
    }

    // A list says why it stays.
    let review = engine.prepare_review(&id).unwrap();
    assert!(steps_for(&review, "Outlook Files/old.pst").is_empty(), "{:?}", review.steps);
    let kept = review.notes.iter().find(|note| note.path == "Outlook Files/old.pst").expect("the list says why it stays");
    assert_eq!((kept.kind, kept.side), (NoteKind::Blocked, Some(Side::Destination)));
    assert!(kept.reason.contains("“Outlook Files/new.pst”"), "{}", kept.reason);
    engine.discard_review(&id, &review.review_id);

    // Once the program lets go, the new name crosses, and only then does the old one go.
    chmod(&new, 0o644);
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait(&engine, &id);
    assert_eq!(fs::read_to_string(dst.join("Outlook Files/new.pst")).unwrap(), "mail, and more mail");
    assert!(!dst.join("Outlook Files/old.pst").exists());
    assert_eq!(fs::read_to_string(dst.join("keep.txt")).unwrap(), "keep");
}

#[test]
fn a_renamed_file_that_cannot_be_read_keeps_its_old_name_in_a_mirror() {
    if !permissions_bite("a_renamed_file_that_cannot_be_read_keeps_its_old_name_in_a_mirror") {
        return;
    }
    a_renamed_file_that_cannot_be_read_keeps_its_old_name(SyncMode::Mirror);
}

#[test]
fn a_renamed_file_that_cannot_be_read_keeps_its_old_name_in_two_way() {
    if !permissions_bite("a_renamed_file_that_cannot_be_read_keeps_its_old_name_in_two_way") {
        return;
    }
    a_renamed_file_that_cannot_be_read_keeps_its_old_name(SyncMode::TwoWay);
}

/// A program that holds a file open may go on writing to it (Outlook does): changed or not,
/// it stays on the list of places Charon couldn't read, and what waits for it says so.
#[test]
fn a_file_that_keeps_changing_while_it_cannot_be_read_stays_on_the_couldnt_read_list() {
    if !permissions_bite("a_file_that_keeps_changing_while_it_cannot_be_read_stays_on_the_couldnt_read_list") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write_at(&src.join("a.txt"), "a", 0);
    write_at(&src.join("gone.txt"), "gone", 0);
    let locked = src.join("locked.pst");
    write_at(&locked, "mail", 0);
    chmod(&locked, 0o000);
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, SyncMode::Mirror)).unwrap().job.id;
    confirm(&engine, &id);
    assert!(dst.join("gone.txt").exists() && !dst.join("locked.pst").exists());

    fs::remove_file(src.join("gone.txt")).unwrap();
    for round in 1..=3 {
        chmod(&locked, 0o600);
        write_at(&locked, &"mail ".repeat(round + 1), 60 * round as i64);
        chmod(&locked, 0o000);
        let review = engine.prepare_review(&id).unwrap();
        assert!(steps_for(&review, "locked.pst").is_empty(), "round {round}: {:?}", review.steps);
        let note = review.notes.iter().find(|note| note.path == "locked.pst").expect("the list names it");
        assert_eq!((note.kind, note.side, note.can_leave_out), (NoteKind::Unreadable, Some(Side::Source), true));
        assert!(steps_for(&review, "gone.txt").is_empty(), "round {round}: {:?}", review.steps);
        let kept = review.notes.iter().find(|note| note.path == "gone.txt").expect("the list says why it stays");
        assert!(kept.reason.contains("“locked.pst”"), "{}", kept.reason);
        engine.approve(&id, &review.review_id, true).unwrap();
        wait(&engine, &id);
        assert!(dst.join("gone.txt").exists());
        let view = engine.job(&id).unwrap();
        assert_eq!(view.status, JobStatus::InSync, "round {round}: {:?}", view.problem);
        assert!(view.unreadable.iter().any(|place| place.path == "locked.pst"), "round {round}: {:?}", view.unreadable);
        assert!(view.not_synced.iter().any(|place| place.path == "gone.txt"), "round {round}: the row shows what waits: {:?}", view.not_synced);
    }

    // Left out, the file holds nothing back anymore.
    engine.leave_out(&id, Side::Source, "locked.pst").unwrap();
    let review = confirm(&engine, &id);
    assert_eq!(steps_for(&review, "gone.txt").len(), 1, "{:?}", review.steps);
    assert!(!dst.join("gone.txt").exists());
    assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "a");
}

#[test]
fn a_database_with_a_file_that_cannot_be_read_is_never_mixed() {
    if !permissions_bite("a_database_with_a_file_that_cannot_be_read_is_never_mixed") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write_at(&src.join("notes.db"), "database 1", 0);
    write_at(&src.join("notes.db-wal"), "journal 1", 0);
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, SyncMode::Backup)).unwrap().job.id;
    confirm(&engine, &id);

    write_at(&src.join("notes.db"), "database 2", 60);
    write_at(&src.join("notes.db-wal"), "journal 2", 60);
    chmod(&src.join("notes.db"), 0o000);
    confirm(&engine, &id);
    assert_eq!(fs::read_to_string(dst.join("notes.db")).unwrap(), "database 1");
    assert_eq!(fs::read_to_string(dst.join("notes.db-wal")).unwrap(), "journal 1", "a database's files only go together");
    assert_ne!(engine.job(&id).unwrap().status, JobStatus::Error);

    // The journal changes again: it still waits for the database it belongs to.
    write_at(&src.join("notes.db-wal"), "journal 3", 120);
    let review = engine.prepare_review(&id).unwrap();
    assert!(steps_for(&review, "notes.db").is_empty() && steps_for(&review, "notes.db-wal").is_empty(), "{:?}", review.steps);
    let note = review.notes.iter().find(|note| note.path == "notes.db-wal").expect("the list says why the journal waits");
    assert_eq!(note.kind, NoteKind::Blocked);
    assert!(note.reason.contains("“notes.db”"), "{}", note.reason);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait(&engine, &id);
    assert_eq!(fs::read_to_string(dst.join("notes.db-wal")).unwrap(), "journal 1");
}

#[test]
fn a_copy_that_fails_names_the_file_in_the_row() {
    if !permissions_bite("a_copy_that_fails_names_the_file_in_the_row") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("a.txt"), "a");
    write(&src.join("shared/report.pdf"), "report");
    fs::create_dir_all(dst.join("shared")).unwrap();
    // Charon may read the destination's folder but not write into it.
    chmod(&dst.join("shared"), 0o555);
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, SyncMode::Backup)).unwrap().job.id;
    confirm(&engine, &id);
    chmod(&dst.join("shared"), 0o755);

    assert!(dst.join("a.txt").exists());
    let view = engine.job(&id).unwrap();
    assert_eq!(view.status, JobStatus::Error);
    assert_eq!(view.problem.as_deref(), Some("1 item couldn't be synced: “shared/report.pdf”. The job's Activity says why."));
    let run = view.job.last_run.unwrap();
    assert_eq!((run.failed, run.unreadable), (1, 0), "a folder Charon can't write to is no place it can't read");
    assert_eq!(engine.overview().headline(), "1 job needs attention");
}

/// A trash Charon can't write into is no file it can't read: when the version a copy would
/// replace can't be kept there, the step fails naming the trash, never the person's file.
#[test]
fn a_trash_charon_cannot_write_into_is_never_taken_for_a_file_it_cannot_read() {
    if !permissions_bite("a_trash_charon_cannot_write_into_is_never_taken_for_a_file_it_cannot_read") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write_at(&a.join("report.txt"), "v1", 0);
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay)).unwrap().job.id;
    confirm(&engine, &id);
    assert_eq!(fs::read_to_string(b.join("report.txt")).unwrap(), "v1");

    // Charon may look into b's trash, but not write into it.
    let trash = b.join(".charon-trash");
    fs::create_dir_all(&trash).unwrap();
    chmod(&trash, 0o555);
    write_at(&a.join("report.txt"), "v2, edited", 60);
    let review = confirm(&engine, &id);
    chmod(&trash, 0o755);
    assert_eq!(review.tally.destination.replaced, 1);
    assert_eq!(fs::read_to_string(b.join("report.txt")).unwrap(), "v1", "never replaced without keeping the old version");
    let run = engine.job(&id).unwrap().job.last_run.unwrap();
    assert_eq!((run.failed, run.unreadable), (1, 0));
    let line = engine.log(&id, 20).into_iter().find(|line| line.text.starts_with("report.txt: ")).expect("the Activity says why");
    assert!(line.text.starts_with(&format!("report.txt: Couldn't read or write {}", trash.display())), "{}", line.text);
}

/// The same for a removal: when the trash won't take the file, the step fails naming the
/// trash, and the file stays where it is.
#[test]
fn a_removal_the_trash_refuses_names_the_trash() {
    if !permissions_bite("a_removal_the_trash_refuses_names_the_trash") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write_at(&a.join("report.txt"), "v1", 0);
    write_at(&a.join("notes.txt"), "stays", 0);
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay)).unwrap().job.id;
    confirm(&engine, &id);
    assert_eq!(fs::read_to_string(b.join("report.txt")).unwrap(), "v1");

    // Charon may look into b's trash, but not write into it.
    let trash = b.join(".charon-trash");
    fs::create_dir_all(&trash).unwrap();
    chmod(&trash, 0o555);
    fs::remove_file(a.join("report.txt")).unwrap();
    let review = confirm(&engine, &id);
    chmod(&trash, 0o755);
    assert_eq!(review.tally.destination.removed, 1);
    assert_eq!(fs::read_to_string(b.join("report.txt")).unwrap(), "v1", "never removed without keeping it in the trash");
    let run = engine.job(&id).unwrap().job.last_run.unwrap();
    assert_eq!((run.failed, run.unreadable), (1, 0));
    let line = engine.log(&id, 20).into_iter().find(|line| line.text.starts_with("report.txt: ")).expect("the Activity says why");
    assert!(line.text.starts_with(&format!("report.txt: Couldn't read or write {}", trash.display())), "{}", line.text);
}
