//! End-to-end: real folders on disk, real looks, reviews and runs through the engine.

use charon_core::model::{ConflictChoice, Level, ReviewKind, StepFilter, StepKind, SyncMode};
use charon_core::{ConflictRule, Emitter, Engine, EngineEvent, Error, JobSetup, JobStatus, SyncStart, Trigger};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Opens the engine once a previous one on `data` has let go of it (its threads may still
/// hold it for a moment after the last handle is dropped).
fn engine(data: &Path) -> Engine {
    opened(data, Arc::new(|_: EngineEvent| {}))
}

/// An engine that keeps every event it sends, opened as `engine` opens one.
fn recording(data: &Path) -> (Engine, Arc<Mutex<Vec<EngineEvent>>>) {
    let events: Arc<Mutex<Vec<EngineEvent>>> = Arc::default();
    let sink = events.clone();
    let engine = opened(data, Arc::new(move |event: EngineEvent| sink.lock().unwrap().push(event)));
    (engine, events)
}

/// The engine on `data`, sending its events to `emit`, once a previous one has let go of it.
fn opened(data: &Path, emit: Emitter) -> Engine {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match Engine::open(data, emit.clone()) {
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

/// Every file under `root`.
fn walk(root: &Path) -> Vec<std::path::PathBuf> {
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

#[test]
fn mirror_copies_and_removes_into_the_trash_once_confirmed() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("photos/cat.jpg"), "meow");
    write(&src.join(".DS_Store"), "junk");
    write(&dst.join("old.txt"), "old");

    let engine = engine(&data);
    let job = engine.add_job(setup(&src, &dst, SyncMode::Mirror, ConflictRule::Ask)).unwrap();
    let id = job.job.id.clone();
    assert_eq!(job.job.setup.name, "src → dst");

    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.kind, ReviewKind::Sync);
    assert_eq!(review.tally.destination.new_files, 1);
    assert_eq!(review.tally.destination.removed, 1);
    assert!(review.blocked.is_none());
    let removals = engine.review_steps(&id, &review.review_id, StepFilter::Removals, "", 0, 50).unwrap();
    assert_eq!(removals.total, 1);
    assert!(removals.steps[0].danger);
    // Nothing happened yet.
    assert!(dst.join("old.txt").exists());
    assert!(!dst.join("photos").exists());

    engine.approve(&id, &review.review_id, true).unwrap();
    wait(&engine, &id);
    assert_eq!(fs::read_to_string(dst.join("photos/cat.jpg")).unwrap(), "meow");
    assert!(!dst.join(".DS_Store").exists(), "excluded files stay behind");
    assert!(!dst.join("old.txt").exists());
    let trashed: Vec<_> = walk(&dst.join(".charon-trash"));
    assert!(trashed.iter().any(|p| p.ends_with("old.txt")), "removed file is in the trash");
    let view = engine.job(&id).unwrap();
    assert!(view.job.last_run.as_ref().unwrap().approved);
    assert!(!view.job.needs_review);
    assert!(engine.log(&id, 10).iter().any(|l| l.text.contains("copied 1 file")));
}

#[test]
fn a_list_is_confirmed_once_and_only_as_it_was_made() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("a.txt"), "a");
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, SyncMode::Backup, ConflictRule::Ask)).unwrap().job.id;

    // A list made before a change in the folders can't be confirmed.
    let review = engine.prepare_review(&id).unwrap();
    assert!(engine.note_change(&id, &src.join("a.txt")));
    assert!(matches!(engine.approve(&id, &review.review_id, true), Err(Error::ReviewOutdated(_))));

    // Nor one made before the job's settings changed.
    let review = engine.prepare_review(&id).unwrap();
    let mut changed = engine.job(&id).unwrap().job.setup;
    changed.excludes.push("*.bak".into());
    engine.update_job(&id, changed).unwrap();
    assert!(engine.approve(&id, &review.review_id, true).is_err());

    // Changes to excluded items don't matter.
    let review = engine.prepare_review(&id).unwrap();
    assert!(!engine.note_change(&id, &src.join("x.bak")));
    assert!(!engine.note_change(&id, &src.join(".DS_Store")));
    engine.approve(&id, &review.review_id, true).unwrap();
    wait(&engine, &id);
    assert!(dst.join("a.txt").exists());
    // Confirmed lists are used up.
    assert!(engine.approve(&id, &review.review_id, true).is_err());
}

#[test]
fn a_removed_folder_lists_only_what_is_inside_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    for path in ["Scans/2019/a.pdf", "Scans/2019/b.pdf", "Old/Scans/2019/c.pdf", "Scans/2019 copy/d.pdf", "keep.txt"] {
        write(&src.join(path), "x");
    }
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, SyncMode::Mirror, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);
    for folder in ["Scans/2019", "Old/Scans/2019", "Scans/2019 copy"] {
        fs::remove_dir_all(src.join(folder)).unwrap();
    }

    let review = engine.prepare_review(&id).unwrap();
    let page = engine.review_steps_in(&id, &review.review_id, StepFilter::Removals, "", Some("Scans/2019"), 0, 50).unwrap();
    let mut paths: Vec<&str> = page.steps.iter().map(|step| step.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, ["Scans/2019", "Scans/2019/a.pdf", "Scans/2019/b.pdf"], "nothing from another folder with the same name in its path");
    assert_eq!(page.total, 3);
    // Without a folder, the search still finds a part of the path anywhere.
    let all = engine.review_steps(&id, &review.review_id, StepFilter::Removals, "scans/2019", 0, 50).unwrap();
    assert!(all.total > 3);
}

#[test]
fn full_automation_is_confirmed_once_even_when_nothing_needs_to_cross() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("same.txt"), "same");
    write(&dst.join("same.txt"), "same");
    let engine = engine(&data);
    let mut automatic = setup(&src, &dst, SyncMode::TwoWay, ConflictRule::Ask);
    automatic.full_automation = true;
    let id = engine.add_job(automatic).unwrap().job.id;
    assert!(!engine.job(&id).unwrap().runs_by_itself);

    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.total_steps, 0);
    engine.approve(&id, &review.review_id, false).unwrap();
    wait(&engine, &id);
    assert!(engine.job(&id).unwrap().runs_by_itself, "the empty list, confirmed, lets it run by itself");
}

#[test]
fn mirror_refuses_to_wipe_from_an_empty_source() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    fs::create_dir_all(&src).unwrap();
    write(&dst.join("precious.txt"), "keep me");

    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, SyncMode::Mirror, ConflictRule::Ask)).unwrap().job.id;
    let review = engine.prepare_review(&id).unwrap();
    assert!(review.blocked.is_some());
    assert!(engine.approve(&id, &review.review_id, true).is_err());
    assert!(dst.join("precious.txt").exists());
}

#[test]
fn removals_need_the_tick_when_safe_deletes_are_off() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("keep.txt"), "k");
    write(&dst.join("keep.txt"), "k");
    write(&dst.join("extra.txt"), "x");
    let engine = engine(&data);
    let mut job = setup(&src, &dst, SyncMode::Mirror, ConflictRule::Ask);
    job.safe_delete = false;
    let id = engine.add_job(job).unwrap().job.id;
    let review = engine.prepare_review(&id).unwrap();
    assert!(review.needs_tick);
    assert!(engine.approve(&id, &review.review_id, false).is_err());
    assert!(dst.join("extra.txt").exists());
    engine.approve(&id, &review.review_id, true).unwrap();
    wait(&engine, &id);
    assert!(!dst.join("extra.txt").exists());
}

#[test]
fn two_way_carries_changes_and_deletions_both_ways() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("from-a.txt"), "a");
    write(&b.join("from-b.txt"), "b");

    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    let first = confirm(&engine, &id);
    assert!(first.warnings.iter().any(|w| w.level == Level::Info && w.text.contains("first two-way sync")));
    assert!(a.join("from-b.txt").exists() && b.join("from-a.txt").exists());
    assert!(a.join(".charon").is_dir() && b.join(".charon").is_dir(), "both sides carry the job's marker");

    // Deleting on one side now deletes on the other, instead of copying back.
    fs::remove_file(b.join("from-a.txt")).unwrap();
    let review = confirm(&engine, &id);
    assert_eq!(review.tally.source.removed, 1);
    assert!(!a.join("from-a.txt").exists());
    assert!(walk(&a.join(".charon-trash")).iter().any(|p| p.ends_with("from-a.txt")));
}

#[test]
fn two_way_conflicts_are_settled_through_a_list() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("plan.txt"), "first");
    fs::create_dir_all(&b).unwrap();

    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);

    write(&a.join("plan.txt"), "edited on a");
    write(&b.join("plan.txt"), "edited on b, longer");
    confirm(&engine, &id);
    let view = engine.job(&id).unwrap();
    assert_eq!(view.status, JobStatus::Conflict);
    let conflict = view.conflicts[0].clone();
    assert_eq!(conflict.path, "plan.txt");

    let review = engine.prepare_resolution(&id, &conflict, ConflictChoice::KeepBoth).unwrap();
    assert_eq!(review.kind, ReviewKind::Resolve);
    assert_eq!(fs::read_to_string(b.join("plan.txt")).unwrap(), "edited on b, longer", "nothing changes before the OK");
    engine.approve(&id, &review.review_id, false).unwrap();
    wait(&engine, &id);
    let names: Vec<String> = fs::read_dir(&a).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(names.iter().any(|n| n.starts_with("plan (conflict ")), "{names:?}");
    assert_eq!(fs::read_to_string(b.join("plan.txt")).unwrap(), "edited on a");
    assert!(engine.job(&id).unwrap().conflicts.is_empty());

    // The next look sees both sides in agreement.
    let next = engine.prepare_review(&id).unwrap();
    assert_eq!(next.total_steps, 0, "{:?}", next.steps);
}

#[test]
fn a_conflict_that_changed_since_it_was_shown_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("plan.txt"), "first");
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);
    write(&a.join("plan.txt"), "edited on a");
    write(&b.join("plan.txt"), "edited on b, longer");
    let review = engine.prepare_review(&id).unwrap();
    engine.discard_review(&id, &review.review_id);
    let conflict = engine.job(&id).unwrap().conflicts[0].clone();
    write(&b.join("plan.txt"), "edited on b again, much longer");
    assert!(matches!(engine.prepare_resolution(&id, &conflict, ConflictChoice::KeepSource), Err(Error::ReviewOutdated(_))));
}

#[test]
fn nothing_changes_by_itself_without_full_automation() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("doc.txt"), "a");
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let mut job = setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask);
    job.trigger = Trigger::OnChange { settle_seconds: 1 };
    let id = engine.add_job(job).unwrap().job.id;
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Review);
    // Only a look: the row says changes wait for the OK.
    engine.check(&id);
    let deadline = Instant::now() + Duration::from_secs(10);
    while engine.job(&id).unwrap().waiting.is_none() {
        assert!(Instant::now() < deadline, "the look never finished");
        thread::sleep(Duration::from_millis(20));
    }
    let view = engine.job(&id).unwrap();
    assert_eq!(view.status, JobStatus::Review);
    assert_eq!(view.waiting.as_ref().unwrap().items, 1);
    assert!(!b.join("doc.txt").exists());
}

/// When the large test files were last modified: `seconds` after a fixed moment.
fn vault_time(seconds: i64) -> filetime::FileTime {
    filetime::FileTime::from_unix_time(1_700_000_000 + seconds, 0)
}

/// Puts “vault.hc” into each of `folders`: the same on every side, more than a background
/// look reads (1 GiB), and taking no room. False where the drive can't hold such a file.
fn vaults(folders: &[&Path]) -> bool {
    for folder in folders {
        fs::create_dir_all(folder).unwrap();
        let vault = fs::File::create(folder.join("vault.hc")).unwrap();
        if vault.set_len((1 << 30) + 4096).is_err() {
            return false;
        }
        drop(vault);
        filetime::set_file_mtime(folder.join("vault.hc"), vault_time(0)).unwrap();
    }
    true
}

/// Writes inside “vault.hc” in `folder`, keeping its size, and gives it the time `seconds`
/// (an encrypted container may put its old time back).
fn edit_vault(folder: &Path, seconds: i64) {
    use std::io::{Seek, SeekFrom, Write};
    let mut vault = fs::OpenOptions::new().write(true).open(folder.join("vault.hc")).unwrap();
    vault.seek(SeekFrom::Start(4096)).unwrap();
    vault.write_all(b"secret").unwrap();
    drop(vault);
    filetime::set_file_mtime(folder.join("vault.hc"), vault_time(seconds)).unwrap();
}

/// Whether the drive of `folder` keeps a change time for each file: only there does a change
/// of a file's details alone show.
#[cfg(unix)]
fn keeps_change_times(folder: &Path) -> bool {
    let keeps = charon_core::scan::Volume::of(folder).keeps_change_times();
    if !keeps {
        eprintln!("{} keeps no change times: a change of a file's details alone doesn't show there", folder.display());
    }
    keeps
}

/// A background look reads at most 1 GiB of files to compare them, and what it leaves
/// unread never shows as in sync: a large file edited inside, its size the same and its
/// time moved by a second only (an encrypted container may keep its time altogether), is
/// not compared yet, and the row says so.
#[test]
fn a_large_file_a_background_look_leaves_unread_never_shows_as_in_sync() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    if !vaults(&[&a, &b]) {
        return;
    }
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);
    look(&engine, &id);
    assert_eq!(engine.job(&id).unwrap().status, JobStatus::InSync);

    edit_vault(&a, 1);
    look(&engine, &id);
    let view = engine.job(&id).unwrap();
    assert_eq!(view.status, JobStatus::Review);
    let waiting = view.waiting.unwrap();
    assert_eq!((waiting.items, waiting.uncompared), (0, 1), "not compared yet, which is no change the look knows of");
    // The list reads it: the edit crosses.
    assert_eq!(engine.prepare_review(&id).unwrap().total_steps, 1);
}

/// A large file whose details alone changed on one side (a permission, a tag, an app noting
/// that it was opened) isn't compared by a background look. Once a list has read it and found
/// both sides identical, the job is in sync, also after Charon starts again: what the list
/// found holds while neither file changes.
#[cfg(unix)]
#[test]
fn a_large_file_whose_details_alone_changed_keeps_no_job_waiting_after_a_restart() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    if !vaults(&[&a, &b]) || !keeps_change_times(&a) {
        return;
    }
    let first = engine(&data);
    let id = first.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&first, &id);
    look(&first, &id);
    assert_eq!(first.job(&id).unwrap().status, JobStatus::InSync);

    fs::set_permissions(a.join("vault.hc"), fs::Permissions::from_mode(0o600)).unwrap();
    look(&first, &id);
    let view = first.job(&id).unwrap();
    assert_eq!(view.status, JobStatus::Review, "without reading it, nothing tells what changed");
    let waiting = view.waiting.unwrap();
    assert_eq!((waiting.items, waiting.uncompared), (0, 1), "not compared yet, which is no change to confirm");
    assert_eq!(first.prepare_review(&id).unwrap().total_steps, 0, "the list reads it: both sides are identical");
    look(&first, &id);
    assert_eq!(first.job(&id).unwrap().status, JobStatus::InSync);
    drop(first);

    let again = engine(&data);
    look(&again, &id);
    assert_eq!(again.job(&id).unwrap().status, JobStatus::InSync, "the row says a change waits, and the list it opens has none");
}

/// A folder on a drive that keeps no change times (tmpfs, as an exFAT drive or a network
/// share), for the other side of a job. `None` where there is none at hand.
#[cfg(target_os = "linux")]
fn untimed_folder() -> Option<tempfile::TempDir> {
    let shared_memory = Path::new("/dev/shm");
    if !shared_memory.is_dir() {
        eprintln!("no drive at hand that keeps no change times");
        return None;
    }
    let folder = tempfile::tempdir_in(shared_memory).unwrap();
    assert!(!charon_core::scan::Volume::of(folder.path()).keeps_change_times());
    Some(folder)
}

/// The job's other side is on a drive that keeps no change times (an exFAT drive, a network
/// share). A large file whose details alone changed on the side that keeps them isn't
/// compared by a background look. Once a list has read it and found both sides identical,
/// the job is in sync after every launch, and no reminder says a file waits to be compared.
#[cfg(target_os = "linux")]
#[test]
fn a_large_file_whose_details_alone_changed_keeps_no_job_waiting_where_the_other_drive_keeps_no_change_times() {
    use std::os::unix::fs::PermissionsExt;
    let Some(far) = untimed_folder() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), far.path().join("b"), tmp.path().join("data"));
    if !vaults(&[&a, &b]) || !keeps_change_times(&a) {
        return;
    }
    let first = engine(&data);
    let id = first.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&first, &id);
    fs::set_permissions(a.join("vault.hc"), fs::Permissions::from_mode(0o600)).unwrap();
    look(&first, &id);
    let waiting = first.job(&id).unwrap().waiting.unwrap();
    assert_eq!((waiting.items, waiting.uncompared), (0, 1), "without reading it, nothing tells what changed");
    assert_eq!(first.prepare_review(&id).unwrap().total_steps, 0, "the list reads it: both sides are identical");
    drop(first);

    for launch in 1..=2 {
        let (again, events) = recording(&data);
        look(&again, &id);
        let view = again.job(&id).unwrap();
        assert_eq!(view.status, JobStatus::InSync, "launch {launch}: {:?}", view.waiting);
        again.remind();
        assert!(!events.lock().unwrap().iter().any(|e| matches!(e, EngineEvent::Waiting(_))), "launch {launch}: a reminder");
    }
}

/// Both sides are on drives that keep no change times, and a drive moved the large file's
/// time by a second (a rounding). Once a list has read it and found both sides identical,
/// the job is in sync after a launch too.
#[cfg(target_os = "linux")]
#[test]
fn a_large_file_whose_time_a_drive_moved_keeps_no_job_waiting_where_no_drive_keeps_change_times() {
    let Some(far) = untimed_folder() else { return };
    let (a, b, data) = (far.path().join("a"), far.path().join("b"), far.path().join("data"));
    if !vaults(&[&a, &b]) {
        return;
    }
    let first = engine(&data);
    let id = first.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&first, &id);
    filetime::set_file_mtime(b.join("vault.hc"), vault_time(1)).unwrap();
    look(&first, &id);
    let waiting = first.job(&id).unwrap().waiting.unwrap();
    assert_eq!((waiting.items, waiting.uncompared), (0, 1), "without reading it, nothing tells what changed");
    assert_eq!(first.prepare_review(&id).unwrap().total_steps, 0, "the list reads it: both sides are identical");
    drop(first);

    let again = engine(&data);
    look(&again, &id);
    assert_eq!(again.job(&id).unwrap().status, JobStatus::InSync);
}

/// What a list found identical holds only for the folders it read. On a drive that keeps no
/// change times another drive under the same name, or the same folder restored from an
/// older copy, can hold a file of the same size and time with other contents: once a
/// folder's marker isn't the one it carried, the file is not compared yet, never in sync,
/// and once the person confirms the folders as they are now, it's the conflict their list
/// found.
#[cfg(target_os = "linux")]
#[test]
fn what_a_list_found_identical_holds_only_while_the_folders_carry_the_same_markers() {
    use std::os::unix::fs::PermissionsExt;
    let Some(far) = untimed_folder() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), far.path().join("b"), tmp.path().join("data"));
    if !vaults(&[&a, &b]) || !keeps_change_times(&a) {
        return;
    }
    let first = engine(&data);
    let id = first.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&first, &id);
    fs::set_permissions(a.join("vault.hc"), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(first.prepare_review(&id).unwrap().total_steps, 0, "the list reads it: both sides are identical");
    drop(first);

    // The destination is now an older copy of itself, holding another version of the file
    // with the same size and time.
    set_marker_generation(&b, &id, 0);
    edit_vault(&b, 0);
    let again = engine(&data);
    look(&again, &id);
    let view = again.job(&id).unwrap();
    assert_eq!(view.status, JobStatus::Review);
    let waiting = view.waiting.unwrap();
    assert_eq!((waiting.items, waiting.uncompared), (0, 1), "not compared yet");
    assert_eq!(confirm(&again, &id).tally.conflicts, 1, "the list reads it: the sides differ");
    drop(again);

    let last = engine(&data);
    look(&last, &id);
    assert_eq!(last.job(&id).unwrap().status, JobStatus::Conflict);
}

/// The same while Charon runs: what this session read in one folder never stands for
/// another. The destination becomes an older copy of itself after a list found both sides
/// identical. The next look neither goes by that read nor keeps it for the restored folder:
/// the file is not compared yet, in this session and after a restart, and the list reads it
/// and finds the conflict.
#[cfg(target_os = "linux")]
#[test]
fn what_this_session_read_holds_only_while_the_folders_carry_the_same_markers() {
    use std::os::unix::fs::PermissionsExt;
    let Some(far) = untimed_folder() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), far.path().join("b"), tmp.path().join("data"));
    if !vaults(&[&a, &b]) || !keeps_change_times(&a) {
        return;
    }
    let first = engine(&data);
    let id = first.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&first, &id);
    fs::set_permissions(a.join("vault.hc"), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(first.prepare_review(&id).unwrap().total_steps, 0, "the list reads it: both sides are identical");

    // While Charon runs, the destination becomes an older copy of itself, holding another
    // version of the file with the same size and time.
    set_marker_generation(&b, &id, 0);
    edit_vault(&b, 0);
    let not_compared = |engine: &Engine| {
        look(engine, &id);
        let view = engine.job(&id).unwrap();
        assert_eq!(view.status, JobStatus::Review);
        let waiting = view.waiting.unwrap();
        assert_eq!((waiting.items, waiting.uncompared), (0, 1), "not compared yet");
    };
    not_compared(&first);
    assert_eq!(first.prepare_review(&id).unwrap().tally.conflicts, 1, "the list reads it: the sides differ");
    drop(first);

    let again = engine(&data);
    not_compared(&again);
}

/// Before a job's first sync neither folder carries Charon's marker, and nothing tells them
/// from other folders under the same names: what this session read in them never stands for
/// another. A list reads a pair whose times differ and finds both sides identical. Then
/// another drive under the same name takes the destination's place, holding another version
/// of the file with the same size and time (an encrypted container keeps its time). The next
/// list reads it again and finds the conflict, and confirming that list leaves it for the
/// person: the two copies are never recorded as in sync.
#[cfg(target_os = "linux")]
#[test]
fn what_this_session_read_before_the_first_sync_never_stands_for_another_folder() {
    let Some(far) = untimed_folder() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), far.path().join("b"), tmp.path().join("data"));
    let other = far.path().join("other");
    if !vaults(&[&a, &b, &other]) {
        return;
    }
    // Copied without its time: the same contents, another date, so a list reads the pair.
    filetime::set_file_mtime(b.join("vault.hc"), vault_time(5)).unwrap();
    // Another version of the file, with the same size and time as the destination's.
    edit_vault(&other, 5);
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    assert_eq!(engine.prepare_review(&id).unwrap().tally.conflicts, 0, "the list reads it: both sides are identical");

    // Another drive under the same name takes the destination's place.
    fs::rename(&b, far.path().join("b-before")).unwrap();
    fs::rename(&other, &b).unwrap();
    assert_eq!(confirm(&engine, &id).tally.conflicts, 1, "the list reads it: the sides differ");
    assert_eq!(engine.prepare_review(&id).unwrap().tally.conflicts, 1, "the conflict waits for the person");
}

/// What a list found about a large file holds only while neither side changes: an edit that
/// keeps the file's size and time (an encrypted container does) is never taken for in sync,
/// before or after a restart, and the list carries it.
#[cfg(unix)]
#[test]
fn a_large_file_edited_after_a_list_found_it_identical_never_shows_as_in_sync() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    if !vaults(&[&a, &b]) || !keeps_change_times(&a) {
        return;
    }
    let first = engine(&data);
    let id = first.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&first, &id);
    fs::set_permissions(a.join("vault.hc"), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(first.prepare_review(&id).unwrap().total_steps, 0, "found identical");

    edit_vault(&a, 0);
    let not_compared = |engine: &Engine| {
        look(engine, &id);
        let view = engine.job(&id).unwrap();
        assert_eq!(view.status, JobStatus::Review);
        let waiting = view.waiting.unwrap();
        assert_eq!((waiting.items, waiting.uncompared), (0, 1));
    };
    not_compared(&first);
    drop(first);
    let again = engine(&data);
    not_compared(&again);
    assert_eq!(again.prepare_review(&id).unwrap().total_steps, 1, "the list reads it: the edit crosses");
}

/// A large file whose details changed on both sides is no conflict before anything has
/// compared it: the row says it isn't compared yet.
#[cfg(unix)]
#[test]
fn a_large_file_touched_on_both_sides_is_no_conflict_before_it_is_compared() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    if !vaults(&[&a, &b]) || !keeps_change_times(&a) {
        return;
    }
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);
    for side in [&a, &b] {
        fs::set_permissions(side.join("vault.hc"), fs::Permissions::from_mode(0o600)).unwrap();
    }
    look(&engine, &id);
    let view = engine.job(&id).unwrap();
    assert_eq!(view.status, JobStatus::Review);
    assert!(view.conflicts.is_empty(), "{:?}", view.conflicts);
    let waiting = view.waiting.unwrap();
    assert_eq!((waiting.items, waiting.conflicts, waiting.uncompared), (0, 0, 1));
}

/// A large file a list read and found to differ, though it looks alike on both sides, and
/// that was left for the person, is still a conflict for the background looks after a
/// restart, which can't read it again: never "not compared", nor in sync.
#[test]
fn a_large_file_found_to_differ_and_left_for_the_person_stays_a_conflict_after_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    if !vaults(&[&a, &b]) {
        return;
    }
    {
        let engine = engine(&data);
        let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
        confirm(&engine, &id);
    }
    // Charon stopped in the middle of the next sync, so both folders are compared afresh,
    // and one side's file was edited inside, its time moved by a second only.
    let state_path = data.join("charon-state.json");
    let text = fs::read_to_string(&state_path).unwrap();
    fs::write(&state_path, text.replace("\"runInProgress\": false", "\"runInProgress\": true")).unwrap();
    edit_vault(&a, 1);
    {
        let engine = engine(&data);
        let id = engine.jobs()[0].job.id.clone();
        let review = confirm(&engine, &id);
        assert_eq!(review.tally.conflicts, 1, "the list read it and found it different");
        assert_eq!(engine.job(&id).unwrap().status, JobStatus::Conflict);
    }
    let engine = engine(&data);
    let id = engine.jobs()[0].job.id.clone();
    look(&engine, &id);
    let view = engine.job(&id).unwrap();
    assert_eq!(view.status, JobStatus::Conflict);
    assert_eq!(view.conflicts.len(), 1);
}

/// A job that syncs by itself compares what its background look couldn't: the look starts a
/// sync, which reads the file.
#[test]
fn a_large_file_a_background_look_leaves_unread_starts_a_sync_of_a_job_that_syncs_by_itself() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    if !vaults(&[&a, &b]) {
        return;
    }
    let engine = engine(&data);
    let mut job = setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask);
    job.full_automation = true;
    let id = engine.add_job(job).unwrap().job.id;
    confirm(&engine, &id);
    edit_vault(&a, 1);
    look(&engine, &id);
    let view = engine.job(&id).unwrap();
    assert_eq!(view.status, JobStatus::Pending);
    let pending = view.pending.unwrap();
    assert_eq!((pending.items, pending.uncompared), (0, 1));
    assert!(engine.has_pending(&id), "its sync reads the file");
}

/// A job that syncs by itself once an hour, not on change, waits for its next sync to compare
/// what its background look couldn't. A large file whose details alone changed on one side
/// (both copies identical) is all the look found: the row says "1 file not compared yet", and
/// the tray says so too, never that the job has changes waiting.
#[cfg(unix)]
#[test]
fn a_job_that_syncs_by_itself_with_only_files_to_compare_is_not_said_to_have_changes_waiting() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    if !vaults(&[&a, &b]) || !keeps_change_times(&a) {
        return;
    }
    let engine = engine(&data);
    let mut job = setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask);
    job.full_automation = true;
    job.trigger = Trigger::Interval { minutes: 60 };
    let id = engine.add_job(job).unwrap().job.id;
    confirm(&engine, &id);
    look(&engine, &id);
    assert_eq!(engine.job(&id).unwrap().status, JobStatus::InSync);

    fs::set_permissions(a.join("vault.hc"), fs::Permissions::from_mode(0o600)).unwrap();
    look(&engine, &id);
    let view = engine.job(&id).unwrap();
    assert_eq!(view.status, JobStatus::Pending);
    let pending = view.pending.as_ref().unwrap();
    assert_eq!((pending.items, pending.uncompared), (0, 1), "no change found, one file not compared yet");
    assert!(view.compares_at_next_sync());
    let overview = engine.overview();
    assert_eq!((overview.pending, overview.pending_compare, overview.lists_waiting()), (0, 1, 0));
    assert_eq!(overview.headline(), "1 job with files to compare at its next sync");
    assert_eq!(engine.prepare_review(&id).unwrap().total_steps, 0, "the list reads it: both sides are identical");
}

/// What reads found identical goes with the job's memory: when its folders or its mode
/// change, and when it's removed.
#[test]
fn files_found_identical_are_forgotten_with_the_jobs_memory() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    for side in [&a, &b] {
        write(&side.join("same.txt"), "same");
        filetime::set_file_mtime(side.join("same.txt"), vault_time(0)).unwrap();
    }
    let engine = engine(&data);
    let two_way = setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask);
    let id = engine.add_job(two_way.clone()).unwrap().job.id;
    let record = data.join("same-contents").join(format!("{id}.json"));
    // After a sync, a drive moves the file's time by a second: a list reads both sides.
    let found_identical = |seconds: i64| {
        confirm(&engine, &id);
        filetime::set_file_mtime(a.join("same.txt"), vault_time(seconds)).unwrap();
        engine.prepare_review(&id).unwrap();
    };
    found_identical(1);
    assert!(record.is_file(), "the list read both files and found them identical");

    engine.update_job(&id, JobSetup { mode: SyncMode::Mirror, ..two_way.clone() }).unwrap();
    assert!(!record.exists(), "the job's mode changed");
    engine.update_job(&id, two_way).unwrap();
    found_identical(2);
    assert!(record.is_file());
    engine.remove_job(&id).unwrap();
    assert!(!record.exists(), "the job was removed");
}

#[test]
fn full_automation_starts_only_after_the_first_confirmed_sync() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("one.txt"), "1");
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let mut job = setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask);
    job.full_automation = true;
    let id = engine.add_job(job).unwrap().job.id;
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Review, "the first sync is always looked at");
    confirm(&engine, &id);
    assert!(engine.job(&id).unwrap().runs_by_itself);

    write(&a.join("two.txt"), "2");
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait(&engine, &id);
    assert!(b.join("two.txt").exists());
    assert!(!engine.job(&id).unwrap().job.last_run.unwrap().approved);
}

#[test]
fn full_automation_holds_back_a_mass_removal() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    for i in 0..40 {
        write(&a.join(format!("f{i}.txt")), "x");
    }
    fs::create_dir_all(&b).unwrap();
    let (engine, events) = recording(&data);
    let mut job = setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask);
    job.full_automation = true;
    let id = engine.add_job(job).unwrap().job.id;
    confirm(&engine, &id);
    for i in 0..30 {
        fs::remove_file(a.join(format!("f{i}.txt"))).unwrap();
    }
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait(&engine, &id);
    assert_eq!(walk(&b).iter().filter(|p| !p.to_string_lossy().contains(".charon")).count(), 40, "nothing was removed");
    let view = engine.job(&id).unwrap();
    assert!(view.job.needs_review && view.job.held.is_some());
    assert_eq!(view.status, JobStatus::Review);
    assert!(events.lock().unwrap().iter().any(|e| matches!(e, EngineEvent::Held { .. })));
    // The person can still look and confirm, with the tick.
    let review = engine.prepare_review(&id).unwrap();
    assert!(review.needs_tick);
    assert!(review.warnings.iter().any(|w| w.level == Level::Danger));
}

#[test]
fn stopping_a_run_makes_the_next_one_wait_for_an_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("one.txt"), "1");
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let mut job = setup(&a, &b, SyncMode::Backup, ConflictRule::Ask);
    job.full_automation = true;
    let id = engine.add_job(job).unwrap().job.id;
    confirm(&engine, &id);
    write(&a.join("big.bin"), &"x".repeat(1 << 20));
    engine.sync_now(&id).unwrap();
    engine.cancel(&id);
    wait(&engine, &id);
    let view = engine.job(&id).unwrap();
    assert!(view.job.last_run.as_ref().unwrap().cancelled);
    assert!(view.job.needs_review);
    assert!(view.job.held.is_some(), "the row says why it waits");
    assert_eq!(view.status, JobStatus::Review);
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Review);

    // Only a confirmed list lets it run by itself again.
    confirm(&engine, &id);
    let view = engine.job(&id).unwrap();
    assert!(!view.job.needs_review && view.job.held.is_none());
    assert!(a.join("big.bin").exists() && b.join("big.bin").exists());
}

#[test]
fn settling_a_conflict_does_not_confirm_the_rest_of_the_job() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("plan.txt"), "first");
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let mut job = setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask);
    job.full_automation = true;
    let id = engine.add_job(job).unwrap().job.id;
    confirm(&engine, &id);
    assert!(!engine.job(&id).unwrap().job.needs_review);

    // The person changes the job: its next sync waits for an OK.
    let mut changed = engine.job(&id).unwrap().job.setup.clone();
    changed.safe_delete = false;
    engine.update_job(&id, changed).unwrap();
    assert!(engine.job(&id).unwrap().job.needs_review);

    write(&a.join("plan.txt"), "edited on a");
    write(&b.join("plan.txt"), "edited on b, longer");
    write(&a.join("other.txt"), "waiting");
    engine.prepare_review(&id).map(|r| engine.discard_review(&id, &r.review_id)).unwrap();
    let conflict = engine.job(&id).unwrap().conflicts[0].clone();
    let review = engine.prepare_resolution(&id, &conflict, ConflictChoice::KeepSource).unwrap();
    engine.approve(&id, &review.review_id, false).unwrap();
    wait(&engine, &id);
    assert_eq!(fs::read_to_string(b.join("plan.txt")).unwrap(), "edited on a");
    assert!(!b.join("other.txt").exists(), "nothing but the conflict changed");
    let view = engine.job(&id).unwrap();
    assert!(view.job.needs_review, "the rest of the job's changes still wait for an OK");
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Review);
}

#[test]
fn quitting_stops_runs_without_asking_for_an_ok_and_starts_nothing_new() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("one.txt"), "1");
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let mut job = setup(&a, &b, SyncMode::Backup, ConflictRule::Ask);
    job.full_automation = true;
    let id = engine.add_job(job).unwrap().job.id;
    confirm(&engine, &id);
    assert!(!engine.job(&id).unwrap().job.needs_review);

    write(&a.join("big.bin"), &"x".repeat(1 << 20));
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    assert!(engine.shut_down(Duration::from_secs(30)), "the run stops in time");
    let view = engine.job(&id).unwrap();
    assert!(view.job.last_run.as_ref().unwrap().cancelled);
    assert!(!view.job.needs_review, "a quit is not the person pressing Stop");
    assert!(engine.sync_now(&id).is_err(), "nothing starts while quitting");
    assert!(engine.prepare_review(&id).is_err());
}

#[test]
fn a_second_charon_on_the_same_data_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let first = engine(tmp.path());
    assert!(matches!(Engine::open(tmp.path(), Arc::new(|_: EngineEvent| {})), Err(Error::AlreadyRunning)));
    drop(first);
}

#[test]
fn a_two_way_folder_without_its_marker_is_compared_afresh() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("one.txt"), "1");
    write(&a.join("two.txt"), "2");
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);

    // Another drive under the same name: empty apart from one file.
    fs::rename(&b, tmp.path().join("old-b")).unwrap();
    write(&b.join("one.txt"), "1");
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.source.removed, 0, "nothing is removed from the source");
    assert!(review.warnings.iter().any(|w| w.text.contains("marker")));
    engine.approve(&id, &review.review_id, true).unwrap();
    wait(&engine, &id);
    assert!(a.join("two.txt").exists());
    assert!(b.join("two.txt").exists());
}

#[test]
fn an_unfinished_sync_is_not_trusted_next_time() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("one.txt"), "1");
    fs::create_dir_all(&b).unwrap();
    {
        let engine = engine(&data);
        let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
        confirm(&engine, &id);
    }
    // Charon stopped in the middle of the next sync: the mark is still set.
    let state_path = data.join("charon-state.json");
    let text = fs::read_to_string(&state_path).unwrap();
    fs::write(&state_path, text.replace("\"runInProgress\": false", "\"runInProgress\": true")).unwrap();
    fs::remove_file(b.join("one.txt")).unwrap();

    let engine = engine(&data);
    let id = engine.jobs()[0].job.id.clone();
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.source.removed, 0);
    assert!(review.warnings.iter().any(|w| w.text.contains("didn't finish")));
}

/// Rewrites the counter of the job's marker in `root`, as an older copy of the drive has it.
fn set_marker_generation(root: &Path, id: &str, generation: u64) {
    let path = root.join(".charon").join(format!("{id}.json"));
    let mut marker: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    marker["generation"] = serde_json::json!(generation);
    fs::write(&path, serde_json::to_vec(&marker).unwrap()).unwrap();
}

fn marker_generation(root: &Path, id: &str) -> u64 {
    let path = root.join(".charon").join(format!("{id}.json"));
    let marker: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    marker["generation"].as_u64().unwrap()
}

#[test]
fn a_drive_swapped_while_the_list_is_open_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("one.txt"), "1");
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);
    write(&a.join("new.txt"), "new");
    let review = engine.prepare_review(&id).unwrap();

    // The drive is swapped for an older copy of itself: same folder, older marker.
    set_marker_generation(&b, &id, 0);
    match engine.approve(&id, &review.review_id, true) {
        Err(Error::ReviewOutdated(text)) => assert!(text.contains("marker"), "{text}"),
        other => panic!("the list must be refused: {other:?}"),
    }
    assert!(!b.join("new.txt").exists());
}

/// Something a test runs once, from inside the engine.
type Hook = Box<dyn FnOnce() + Send>;

#[test]
fn a_drive_swapped_during_the_sync_records_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("one.txt"), "1");
    write(&a.join("two.txt"), "2");
    fs::create_dir_all(&b).unwrap();
    // An engine that swaps the drive (an older copy of it) right before the first step.
    let swap: Arc<Mutex<Option<Hook>>> = Arc::default();
    let armed = swap.clone();
    let engine = Engine::open(
        &data,
        Arc::new(move |event: EngineEvent| {
            if let EngineEvent::Progress { progress, .. } = &event {
                if progress.current.is_some() {
                    if let Some(hook) = armed.lock().unwrap().take() {
                        hook();
                    }
                }
            }
        }),
    )
    .unwrap();
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);
    assert_eq!(marker_generation(&b, &id), 1);

    write(&a.join("new.txt"), "new");
    let review = engine.prepare_review(&id).unwrap();
    let (older, marker_id) = (b.clone(), id.clone());
    thread::sleep(Duration::from_millis(150)); // let the progress throttle pass
    *swap.lock().unwrap() = Some(Box::new(move || set_marker_generation(&older, &marker_id, 0)));
    engine.approve(&id, &review.review_id, true).unwrap();
    wait(&engine, &id);
    assert!(swap.lock().unwrap().is_none(), "the swap happened during the sync");

    let view = engine.job(&id).unwrap();
    let run = view.job.last_run.clone().unwrap();
    assert!(run.error.as_deref().is_some_and(|e| e.contains("marker")), "{run:?}");
    assert_eq!(marker_generation(&b, &id), 0, "the other drive isn't stamped as the synced one");
    assert!(view.job.needs_review && view.job.held.is_some(), "the next sync waits for an OK");

    // The next look trusts nothing: it removes nothing anywhere.
    fs::remove_file(b.join("one.txt")).unwrap();
    let next = engine.prepare_review(&id).unwrap();
    assert_eq!((next.tally.source.removed, next.tally.destination.removed), (0, 0));
}

#[test]
fn mirror_from_an_older_copy_of_its_source_is_held() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    for n in 0..20 {
        write(&a.join(format!("p{n}.jpg")), "photo");
    }
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let mut job = setup(&a, &b, SyncMode::Mirror, ConflictRule::Ask);
    job.full_automation = true;
    let id = engine.add_job(job).unwrap().job.id;
    confirm(&engine, &id);
    write(&a.join("p20.jpg"), "newest");
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait(&engine, &id);
    assert!(b.join("p20.jpg").exists());
    assert_eq!(marker_generation(&a, &id), 2);

    // The source drive is swapped for an older copy of itself (made before p20).
    fs::remove_file(a.join("p20.jpg")).unwrap();
    set_marker_generation(&a, &id, 1);
    engine.sync_now(&id).unwrap();
    wait(&engine, &id);
    let view = engine.job(&id).unwrap();
    assert!(view.job.held.as_deref().is_some_and(|held| held.contains("back in time")), "held for a look: {:?}", view.job.held);
    assert!(b.join("p20.jpg").exists(), "the newest backup is still there");
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Review);
    let review = engine.prepare_review(&id).unwrap();
    assert!(review.needs_tick);
    assert!(review.warnings.iter().any(|w| w.level == Level::Danger && w.text.contains("back in time")), "{:?}", review.warnings);
}

/// What a scan sees of a file (size and date; the change time left out matches anything).
fn seen(path: &Path) -> charon_core::scan::Entry {
    let meta = fs::metadata(path).unwrap();
    let modified = meta.modified().unwrap();
    let ms = match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(after) => after.as_millis() as i64,
        Err(before) => -(before.duration().as_millis() as i64),
    };
    charon_core::scan::Entry::file(meta.len(), ms)
}

fn set_modified(path: &Path, secs: u64) {
    let time = std::time::UNIX_EPOCH + Duration::from_secs(secs);
    fs::File::options().write(true).open(path).unwrap().set_times(fs::FileTimes::new().set_modified(time)).unwrap();
}

fn marker_token(root: &Path, id: &str) -> String {
    let text = fs::read_to_string(root.join(".charon").join(format!("{id}.json"))).unwrap();
    serde_json::from_str::<serde_json::Value>(&text).unwrap()["token"].as_str().unwrap().to_string()
}

#[test]
fn a_backup_leaves_alone_copies_whose_dates_didnt_stick() {
    use charon_core::plan::{Baseline, Pair};
    use charon_core::scan::Volume;
    use charon_core::store::Landings;
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("old.txt"), "1975");
    set_modified(&src.join("old.txt"), 157_766_400); // 1 January 1975
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let job = engine.add_job(setup(&src, &dst, SyncMode::Backup, ConflictRule::Ask)).unwrap().job;
    let id = job.id.clone();
    confirm(&engine, &id);
    // The destination's drive couldn't keep the date (FAT starts in 1980).
    set_modified(&dst.join("old.txt"), 315_532_800);
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.replaced, 1, "without a record it's copied again");
    engine.discard_review(&id, &review.review_id);

    // The record a run keeps of such a copy.
    let mut landed = Baseline::new();
    landed.insert("old.txt", Pair::new(seen(&src.join("old.txt")), seen(&dst.join("old.txt"))));
    let landings = Landings::new(data.join("landings"));
    // Kept with keys folded as the job's looks fold them: where a drive ignores case.
    let folded = [&src, &dst].iter().any(|root| !Volume::of(root).case_sensitive);
    landings.save(&id, &job.setup, folded, &marker_token(&dst, &id), &landed).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.replaced, 0);
    assert!(review.warnings.iter().any(|w| w.level == Level::Info && w.text.contains("other dates")), "{:?}", review.warnings);
    engine.discard_review(&id, &review.review_id);

    // Edited in the source: copied again, and the record of it goes once the date sticks.
    write(&src.join("old.txt"), "1976");
    let review = confirm(&engine, &id);
    assert_eq!(review.tally.destination.replaced, 1);
    assert_eq!(fs::read_to_string(dst.join("old.txt")).unwrap(), "1976");
    assert!(!data.join("landings").join(format!("{id}.json")).exists());
}

#[test]
fn copies_whose_dates_didnt_stick_on_another_drive_are_copied_again() {
    use charon_core::plan::{Baseline, Pair};
    use charon_core::store::Landings;
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("old.txt"), "1975");
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let job = engine.add_job(setup(&src, &dst, SyncMode::Backup, ConflictRule::Ask)).unwrap().job;
    let id = job.id.clone();
    confirm(&engine, &id);
    set_modified(&dst.join("old.txt"), 315_532_800);
    let mut landed = Baseline::new();
    landed.insert("old.txt", Pair::new(seen(&src.join("old.txt")), seen(&dst.join("old.txt"))));
    // Made for a destination whose marker had another token: another drive.
    Landings::new(data.join("landings")).save(&id, &job.setup, false, "another-drive", &landed).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.replaced, 1);
}

#[test]
fn full_automation_counts_each_file_once_a_day_and_the_list_shows_the_day() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    for i in 0..40 {
        write(&a.join(format!("f{i}.txt")), "x");
    }
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let mut job = setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask);
    job.full_automation = true;
    let id = engine.add_job(job).unwrap().job.id;
    confirm(&engine, &id);
    let run = |engine: &Engine| {
        assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
        wait(engine, &id);
        engine.job(&id).unwrap().job.held
    };

    // A log saved all day long counts once.
    for n in 0..25 {
        write(&a.join("f0.txt"), &"log ".repeat(n + 2));
        assert_eq!(run(&engine), None, "run {n} was held");
    }
    // Four other files at a time: by the fifth run, half of the folder was replaced today.
    for round in 0..4 {
        for i in 1 + round * 4..5 + round * 4 {
            write(&a.join(format!("f{i}.txt")), &format!("edit {round}"));
        }
        assert_eq!(run(&engine), None, "round {round} was held");
    }
    for i in 17..21 {
        write(&a.join(format!("f{i}.txt")), "edit 4");
    }
    let held = run(&engine).expect("held back");
    assert!(held.contains("within a day"), "{held}");
    assert_eq!(fs::read_to_string(b.join("f17.txt")).unwrap(), "x", "nothing was replaced");

    // The list shows why, and once confirmed the day's count starts again.
    let review = engine.prepare_review(&id).unwrap();
    assert!(review.needs_tick);
    assert!(review.warnings.iter().any(|w| w.level == Level::Danger && w.text.contains("within a day")), "{:?}", review.warnings);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait(&engine, &id);
    assert_eq!(fs::read_to_string(b.join("f17.txt")).unwrap(), "edit 4");
    for i in 21..25 {
        write(&a.join(format!("f{i}.txt")), "edit 5");
    }
    assert_eq!(run(&engine), None);
}

#[test]
fn a_database_and_its_journal_cross_together() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("app/data.db"), "db 1");
    write(&a.join("app/data.db-wal"), "wal 1");
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);
    write(&a.join("app/data.db"), "db 2 checkpointed");
    write(&a.join("app/data.db-wal"), "wal 2 after the checkpoint");
    confirm(&engine, &id);
    assert_eq!(fs::read_to_string(b.join("app/data.db")).unwrap(), "db 2 checkpointed");
    assert_eq!(fs::read_to_string(b.join("app/data.db-wal")).unwrap(), "wal 2 after the checkpoint");
}

#[test]
fn a_database_whose_journal_changes_mid_sync_isnt_half_copied() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("data.db"), "db 1");
    write(&a.join("data.db-wal"), "wal 1");
    fs::create_dir_all(&b).unwrap();
    // The app writes to the journal as soon as Charon starts copying the database.
    let hook: Arc<Mutex<Option<Hook>>> = Arc::default();
    let armed = hook.clone();
    let engine = Engine::open(
        &data,
        Arc::new(move |event: EngineEvent| {
            if let EngineEvent::Progress { progress, .. } = &event {
                if progress.current.is_some() {
                    if let Some(hook) = armed.lock().unwrap().take() {
                        hook();
                    }
                }
            }
        }),
    )
    .unwrap();
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);
    write(&a.join("data.db"), "db 2 checkpointed");
    write(&a.join("data.db-wal"), "wal 2");
    let wal = a.join("data.db-wal");
    *hook.lock().unwrap() = Some(Box::new(move || fs::write(&wal, "wal 3, written by the app meanwhile").unwrap()));
    thread::sleep(Duration::from_millis(200)); // live progress reaches the window at most this often
    confirm(&engine, &id);
    assert!(hook.lock().unwrap().is_none(), "the app wrote during the sync");
    assert_eq!(fs::read_to_string(b.join("data.db")).unwrap(), "db 1", "the database waits for its journal");
    assert_eq!(fs::read_to_string(b.join("data.db-wal")).unwrap(), "wal 1");
    let run = engine.job(&id).unwrap().job.last_run.unwrap();
    assert_eq!((run.skipped, run.waited), (1, 1), "the journal changed meanwhile, the database only waits for it: {run:?}");
    // The next sync carries both, as they are now.
    confirm(&engine, &id);
    assert_eq!(fs::read_to_string(b.join("data.db")).unwrap(), "db 2 checkpointed");
    assert_eq!(fs::read_to_string(b.join("data.db-wal")).unwrap(), "wal 3, written by the app meanwhile");
}

#[test]
fn a_library_changed_on_both_sides_is_kept_whole_from_one_side() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    let lib = "Photos Library.photoslibrary";
    write(&a.join(lib).join("originals/one.jpg"), "one");
    write(&a.join(lib).join("database/Photos.sqlite"), "db 1");
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::NewestWins)).unwrap().job.id;
    confirm(&engine, &id);

    // Photos used on both computers before a sync.
    write(&a.join(lib).join("originals/two.jpg"), "two");
    write(&a.join(lib).join("database/Photos.sqlite"), "db 2 from a");
    write(&b.join(lib).join("originals/three.jpg"), "three");
    write(&b.join(lib).join("database/Photos.sqlite"), "db 2 from b, longer");
    let review = confirm(&engine, &id);
    assert_eq!(review.tally.conflicts, 1);
    assert!(review.warnings.iter().all(|w| !w.text.contains("half-written")), "nothing of the library moves");
    assert!(!b.join(lib).join("originals/two.jpg").exists() && !a.join(lib).join("originals/three.jpg").exists());
    let view = engine.job(&id).unwrap();
    assert_eq!(view.conflicts.len(), 1);
    let conflict = view.conflicts[0].clone();
    assert_eq!(conflict.path, lib);
    assert!(conflict.whole.is_some());
    assert!(engine.prepare_resolution(&id, &conflict, ConflictChoice::KeepBoth).is_err());

    // Keeping the source's library makes the destination's exactly that.
    let review = engine.prepare_resolution(&id, &conflict, ConflictChoice::KeepSource).unwrap();
    engine.approve(&id, &review.review_id, review.needs_tick).unwrap();
    wait(&engine, &id);
    assert_eq!(fs::read_to_string(b.join(lib).join("database/Photos.sqlite")).unwrap(), "db 2 from a");
    assert!(b.join(lib).join("originals/two.jpg").exists());
    assert!(!b.join(lib).join("originals/three.jpg").exists());
    let trashed = walk(&b.join(".charon-trash"));
    assert!(trashed.iter().any(|p| p.ends_with("three.jpg")), "the destination's version is in the trash: {trashed:?}");
    assert!(trashed.iter().any(|p| p.ends_with("Photos.sqlite")));
    let next = engine.prepare_review(&id).unwrap();
    assert_eq!(next.total_steps, 0, "{:?}", next.steps);
    assert_eq!(next.tally.conflicts, 0);
}

/// An engine that runs the hook put in its slot once, right before the next step acts.
fn hooked(data: &Path) -> (Engine, Arc<Mutex<Option<Hook>>>) {
    let hook: Arc<Mutex<Option<Hook>>> = Arc::default();
    let armed = hook.clone();
    let engine = Engine::open(
        data,
        Arc::new(move |event: EngineEvent| {
            if let EngineEvent::Progress { progress, .. } = &event {
                if progress.current.is_some() {
                    if let Some(hook) = armed.lock().unwrap().take() {
                        hook();
                    }
                }
            }
        }),
    )
    .unwrap();
    (engine, hook)
}

/// Something else writes into an app library on the side it's carried to while the sync
/// runs (its app there, or another computer's sync): a file the list doesn't touch
/// changes, or a new one turns up. Then nothing of the library moves, and it's never left
/// a mix of two versions. System clutter and excluded items don't count.
#[test]
fn a_library_written_to_on_the_other_side_during_the_sync_isnt_mixed() {
    let cases = [
        ("a file the list doesn't touch changes", "c.txt", false),
        ("a new file turns up", "Media/take 2.wav", false),
        ("Finder's clutter turns up", ".DS_Store", true),
        ("an excluded file turns up", "cache.tmp", true),
    ];
    for (case, meddle, carried) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
        let song = |root: &Path, name: &str| root.join("Song.logicx").join(name);
        for name in ["a.txt", "b.txt", "c.txt", "Media/take 1.wav"] {
            write(&song(&a, name), "1");
        }
        fs::create_dir_all(&b).unwrap();
        let (engine, hook) = hooked(&data);
        let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
        confirm(&engine, &id);

        write(&song(&a, "a.txt"), "2 from a");
        write(&song(&a, "b.txt"), "2 from a");
        let meddled = song(&b, meddle);
        *hook.lock().unwrap() = Some(Box::new(move || write(&meddled, "written on b meanwhile")));
        thread::sleep(Duration::from_millis(200)); // live progress reaches the window at most this often
        confirm(&engine, &id);
        assert!(hook.lock().unwrap().is_none(), "{case}: something wrote during the sync");
        let run = engine.job(&id).unwrap().job.last_run.unwrap();
        // The two files themselves didn't change: they wait for the rest of the library.
        let expected = if carried { ((2, 0, 0), "2 from a") } else { ((0, 0, 2), "1") };
        assert_eq!((run.copied, run.skipped, run.waited), expected.0, "{case}: {run:?}");
        for name in ["a.txt", "b.txt"] {
            assert_eq!(fs::read_to_string(song(&b, name)).unwrap(), expected.1, "{case}: {name}");
        }
    }
}

/// An app that had a removed file open follows it into .charon-trash and saves the person's
/// work there: Charon says so, never empties it, and stops running by itself until then.
#[test]
fn work_saved_into_the_trash_is_shown_and_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    for i in 0..10 {
        write(&a.join(format!("f{i}.txt")), "x");
    }
    write(&a.join("report.txt"), "the report");
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let mut job = setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask);
    job.full_automation = true;
    let id = engine.add_job(job).unwrap().job.id;
    confirm(&engine, &id);

    // Renamed on one computer: removed from the other one, into its trash.
    fs::rename(a.join("report.txt"), a.join("report-final.txt")).unwrap();
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait(&engine, &id);
    assert!(engine.job(&id).unwrap().job.held.is_none());
    let trashed = walk(&b.join(".charon-trash"));
    let kept = trashed.iter().find(|p| p.ends_with("report.txt")).expect("in the trash").clone();
    assert!(trashed.iter().any(|p| p.file_name().is_some_and(|n| n == ".charon")), "the run listed what it put there: {trashed:?}");

    // The app that had it open saves into it.
    thread::sleep(Duration::from_millis(20));
    fs::write(&kept, "the report, with today's work").unwrap();
    write(&a.join("f0.txt"), "edited");
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait(&engine, &id);
    let held = engine.job(&id).unwrap().job.held.expect("held for a look");
    assert_eq!(fs::read_to_string(b.join("f0.txt")).unwrap(), "x", "nothing ran by itself");
    assert!(held.contains("changed after Charon put it there"), "{held}");
    let review = engine.prepare_review(&id).unwrap();
    let warning = review.warnings.iter().find(|w| w.text.contains("changed after Charon put it there")).expect("warned");
    assert_eq!(warning.level, Level::Caution);
    assert!(warning.text.contains("report.txt"), "{}", warning.text);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait(&engine, &id);
    assert_eq!(fs::read_to_string(&kept).unwrap(), "the report, with today's work");
}

/// The person opens a dated folder of .charon-trash in Finder to get a removed file back,
/// and Finder writes its .DS_Store there. That's nobody's work: nothing is held up, nothing
/// warns, and the folder is emptied when its time comes.
#[test]
fn what_finder_writes_into_the_trash_holds_nothing_up() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    for i in 0..10 {
        write(&a.join(format!("f{i}.txt")), "x");
    }
    write(&a.join("report.txt"), "the report");
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let mut job = setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask);
    job.full_automation = true;
    let id = engine.add_job(job).unwrap().job.id;
    confirm(&engine, &id);
    fs::remove_file(a.join("report.txt")).unwrap();
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait(&engine, &id);
    let trash = b.join(".charon-trash");
    let kept = walk(&trash).into_iter().find(|p| p.ends_with("report.txt")).expect("in the trash");

    // Weeks later, the person looks into that folder in Finder.
    let old = trash.join(format!("{}_0a0b0c", (chrono::Local::now() - chrono::Duration::days(40)).format("%Y-%m-%d_%H-%M-%S")));
    fs::rename(kept.parent().unwrap(), &old).unwrap();
    write(&old.join(".DS_Store"), "Finder's view of the folder");
    write(&a.join("f0.txt"), "edited");
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait(&engine, &id);
    assert!(engine.job(&id).unwrap().job.held.is_none(), "{:?}", engine.job(&id).unwrap().job.held);
    assert_eq!(fs::read_to_string(b.join("f0.txt")).unwrap(), "edited", "ran by itself");
    let review = engine.prepare_review(&id).unwrap();
    assert!(review.warnings.iter().all(|w| !w.text.contains("changed after Charon put")), "{:?}", review.warnings);
    assert!(review.housekeeping.iter().any(|line| line.contains("Also empties 2 items")), "{:?}", review.housekeeping);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait(&engine, &id);
    assert!(!old.exists(), "emptied");
}

/// iCloud Drive before macOS 14 frees up the files nobody used for long, the trash's old ones
/// first: a trashed file goes, and a new hidden `.report.txt.icloud` stands in for it, which
/// holds the file's size. That's the service's stand-in for a file Charon put there as it
/// was, not someone's work: nothing is held up, nothing warns, and the folder is emptied with
/// it when its time comes.
#[test]
fn what_icloud_leaves_for_a_trashed_file_holds_nothing_up() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    for i in 0..10 {
        write(&a.join(format!("f{i}.txt")), "x");
    }
    write(&a.join("report.txt"), "the report");
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let mut job = setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask);
    job.full_automation = true;
    let id = engine.add_job(job).unwrap().job.id;
    confirm(&engine, &id);
    fs::remove_file(a.join("report.txt")).unwrap();
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait(&engine, &id);
    let trash = b.join(".charon-trash");
    let kept = walk(&trash).into_iter().find(|p| p.ends_with("report.txt")).expect("in the trash");

    // Weeks later, iCloud Drive frees the trashed file up.
    let old = trash.join(format!("{}_0a0b0c", (chrono::Local::now() - chrono::Duration::days(40)).format("%Y-%m-%d_%H-%M-%S")));
    fs::rename(kept.parent().unwrap(), &old).unwrap();
    fs::remove_file(old.join("report.txt")).unwrap();
    let mut about = plist::Dictionary::new();
    about.insert("NSURLFileResourceTypeKey".into(), plist::Value::String("NSURLFileResourceTypeRegular".into()));
    about.insert("NSURLFileSizeKey".into(), ("the report".len() as u64).into());
    about.insert("NSURLNameKey".into(), plist::Value::String("report.txt".into()));
    plist::Value::Dictionary(about).to_writer_binary(fs::File::create(old.join(".report.txt.icloud")).unwrap()).unwrap();
    write(&a.join("f0.txt"), "edited");
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait(&engine, &id);
    assert!(engine.job(&id).unwrap().job.held.is_none(), "{:?}", engine.job(&id).unwrap().job.held);
    assert_eq!(fs::read_to_string(b.join("f0.txt")).unwrap(), "edited", "ran by itself");
    let review = engine.prepare_review(&id).unwrap();
    assert!(review.warnings.iter().all(|w| !w.text.contains("changed after Charon put")), "{:?}", review.warnings);
    assert!(review.housekeeping.iter().any(|line| line.contains("Also empties 1 item")), "{:?}", review.housekeeping);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait(&engine, &id);
    assert!(!old.exists(), "emptied");
}

/// A sync that would fill a drive says so first, and its copies fail with the reason
/// instead of filling the drive to the last byte.
#[test]
fn a_drive_without_room_is_never_filled() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("small.txt"), "x");
    fs::create_dir_all(&b).unwrap();
    // A file bigger than any drive here, that takes no room itself.
    let huge = fs::File::create(a.join("huge.bin")).unwrap();
    if huge.set_len(1 << 43).is_err() {
        return;
    }
    drop(huge);
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::Backup, ConflictRule::Ask)).unwrap().job.id;
    let review = engine.prepare_review(&id).unwrap();
    let warning = review.warnings.iter().find(|w| w.text.contains("isn't enough room on the destination's drive")).expect("warned");
    assert_eq!(warning.level, Level::Caution);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait(&engine, &id);
    assert_eq!(fs::read_to_string(b.join("small.txt")).unwrap(), "x");
    assert!(!b.join("huge.bin").exists());
    let run = engine.job(&id).unwrap().job.last_run.unwrap();
    assert_eq!(run.failed, 1);
    assert!(engine.log(&id, 20).iter().any(|l| l.text.contains("huge.bin") && l.text.contains("isn't enough room")));
}

#[test]
fn the_first_list_after_charon_0_2_says_which_items_come_back() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    for side in [&src, &dst] {
        write(&side.join("a.txt"), "a");
        write(&side.join("b.txt"), "b");
        write(&side.join("Photos/x.jpg"), "x");
    }
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);

    // What Charon 0.2 left of the last sync: a bare list of the items both sides had.
    let item = r#"{"kind":"file","size":1,"modified":10000}"#;
    let folder = r#"{"kind":"dir","size":0,"modified":0}"#;
    let legacy = format!(r#"{{"a.txt":{item},"b.txt":{item},"Photos":{folder},"Photos/x.jpg":{item}}}"#);
    fs::write(data.join("baselines").join(format!("{id}.json")), legacy).unwrap();
    // Since then the person removed b.txt and Photos on the destination, and made new.txt.
    fs::remove_file(dst.join("b.txt")).unwrap();
    fs::remove_dir_all(dst.join("Photos")).unwrap();
    write(&src.join("new.txt"), "new");

    let review = engine.prepare_review(&id).unwrap();
    assert!(review.steps.iter().all(|step| step.kind != StepKind::Remove), "nothing is removed: {:?}", review.steps);
    let warning = review.warnings.iter().find(|w| w.text.contains("older version of Charon")).expect("the list says why");
    assert_eq!(warning.level, Level::Caution);
    assert!(warning.text.contains("4 items only on the source are copied to the destination."), "{}", warning.text);
    assert!(warning.text.contains("3 of them were on both sides then, so they were most likely removed since:"), "{}", warning.text);
    assert!(warning.text.contains("“Photos”") && warning.text.contains("“b.txt”") && !warning.text.contains("x.jpg"), "{}", warning.text);
    assert_eq!(warning.text.matches("afresh").count(), 1, "said once: {}", warning.text);
}

/// Settling one conflict of that first list leaves Charon 0.2's record as it is: the lists
/// after it still say why they remove nothing, and which items most likely were removed.
#[test]
fn settling_a_conflict_after_charon_0_2_keeps_saying_which_items_come_back() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    for side in [&src, &dst] {
        write(&side.join("gone.txt"), "g");
        write(&side.join("plan.txt"), "first");
    }
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);
    let item = r#"{"kind":"file","size":1,"modified":10000}"#;
    fs::write(data.join("baselines").join(format!("{id}.json")), format!(r#"{{"gone.txt":{item},"plan.txt":{item}}}"#)).unwrap();
    // Since then the person removed gone.txt on the destination, and edited plan.txt on both sides.
    fs::remove_file(dst.join("gone.txt")).unwrap();
    write(&src.join("plan.txt"), "edited on the source");
    write(&dst.join("plan.txt"), "edited on the destination");
    let review = engine.prepare_review(&id).unwrap();
    engine.discard_review(&id, &review.review_id);

    let conflict = engine.job(&id).unwrap().conflicts[0].clone();
    let settle = engine.prepare_resolution(&id, &conflict, ConflictChoice::KeepSource).unwrap();
    assert!(settle.warnings.iter().any(|w| w.text.contains("no memory of the last sync that it can trust")), "{:?}", settle.warnings);
    engine.approve(&id, &settle.review_id, false).unwrap();
    wait(&engine, &id);
    assert_eq!(fs::read_to_string(dst.join("plan.txt")).unwrap(), "edited on the source");

    let next = engine.prepare_review(&id).unwrap();
    assert!(next.steps.iter().all(|step| step.kind != StepKind::Remove), "nothing is removed: {:?}", next.steps);
    let warning = next.warnings.iter().find(|w| w.text.contains("older version of Charon")).expect("the list still says why");
    assert_eq!(warning.level, Level::Caution);
    assert!(warning.text.contains("so it was most likely removed since: “gone.txt”."), "{}", warning.text);
    assert!(!next.warnings.iter().any(|w| w.text.contains("first two-way sync")), "{:?}", next.warnings);
}

/// A job whose memory of the last sync is gone isn't syncing its folders for the first
/// time: its list says that it compares them afresh, and that what was removed comes back.
#[test]
fn a_job_that_lost_its_memory_never_calls_its_list_the_first_sync() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("gone.txt"), "g");
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);
    fs::remove_file(data.join("baselines").join(format!("{id}.json"))).unwrap();
    fs::remove_file(dst.join("gone.txt")).unwrap();

    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.new_files, 1, "gone.txt comes back: {:?}", review.steps);
    let warning = review.warnings.iter().find(|w| w.text.contains("no memory of this job's last sync")).expect("the list says why");
    assert_eq!(warning.level, Level::Caution);
    assert!(warning.text.contains("removed since the last sync may come back"), "{}", warning.text);
    assert!(!review.warnings.iter().any(|w| w.text.contains("first two-way sync")), "{:?}", review.warnings);
}
