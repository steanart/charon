//! Drives that come back during a run without the run's last changes: a USB drive pulled
//! out and plugged in again, or a network share whose server restarted. Such a drive can
//! return under the same path, with the same device and folder numbers and with its old
//! marker, while the changes it still kept in memory never reached it. A run like that must
//! never be recorded as done: the next sync would read a copy that never arrived as an item
//! removed since, and remove the original. Its next look compares both folders afresh and
//! removes nothing.
//!
//! Each test undoes some of a run's changes on its sync thread right after the run's last
//! step, before anything is flushed or recorded: what a drive that came back without them
//! shows. A change made by a person after a step (an edit, a new file under a removed name)
//! is no lost change, and must not stop the run from being recorded.
//!
//! The side a copy was made from can come back without the person's own last change too.
//! In a two-way job, a file lost there would read as removed on purpose, and the next sync
//! would remove its copy, the only one left. A person who moves or removes the file during
//! the run looks just the same, so only that file is left out of the record: the next look
//! copies it back rather than remove anything, and the run itself still counts.

use charon_core::model::{Level, Review};
use charon_core::{ConflictRule, Engine, EngineEvent, JobSetup, JobView, Phase, SyncMode, SyncStart, Trigger};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// How long any wait may take before the test fails.
const DEADLINE: Duration = Duration::from_secs(60);

type Hook = Box<dyn FnOnce() + Send>;

/// An engine that can run a one-shot hook on its sync thread once a run's last step is done,
/// before the run is flushed and recorded.
struct Rig {
    engine: Engine,
    hook: Arc<Mutex<Option<Hook>>>,
    fired: Arc<AtomicBool>,
}

impl Rig {
    fn open(data: &Path) -> Rig {
        let hook: Arc<Mutex<Option<Hook>>> = Arc::default();
        let fired = Arc::new(AtomicBool::new(false));
        let (slot, flag) = (hook.clone(), fired.clone());
        let emitter = Arc::new(move |event: EngineEvent| {
            if let EngineEvent::Progress { progress, .. } = &event {
                // Every step is done; nothing of the run is flushed or recorded yet.
                if progress.phase == Phase::Finishing {
                    let armed = slot.lock().unwrap().take();
                    if let Some(hook) = armed {
                        hook();
                        flag.store(true, Ordering::SeqCst);
                    }
                }
            }
        });
        let engine = Engine::open(data, emitter).expect("the engine opens");
        Rig { engine, hook, fired }
    }

    /// Runs `hook` on the sync thread once the next run's last step is done.
    fn at_the_end(&self, hook: impl FnOnce() + Send + 'static) {
        self.fired.store(false, Ordering::SeqCst);
        *self.hook.lock().unwrap() = Some(Box::new(hook));
    }

    fn hook_fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }
}

fn job_setup(src: &Path, dst: &Path, mode: SyncMode) -> JobSetup {
    fs::create_dir_all(src).unwrap();
    fs::create_dir_all(dst).unwrap();
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

fn add(engine: &Engine, setup: JobSetup) -> String {
    engine.add_job(setup).expect("the job is added").job.id
}

fn view(engine: &Engine, id: &String) -> JobView {
    engine.job(id).expect("the job exists")
}

/// Waits until no sync of the job runs.
fn wait_idle(engine: &Engine, id: &String) {
    let deadline = Instant::now() + DEADLINE;
    while engine.is_running(id) {
        assert!(Instant::now() < deadline, "the sync took too long");
        thread::sleep(Duration::from_millis(10));
    }
}

/// The person looks at the list and confirms it as it is; waits for the sync to finish.
fn confirm(engine: &Engine, id: &String) -> Review {
    let review = engine.prepare_review(id).expect("the list is made");
    assert!(review.blocked.is_none(), "the list is blocked: {:?}", review.blocked);
    engine.approve(id, &review.review_id, true).expect("the list is confirmed");
    wait_idle(engine, id);
    review
}

/// `confirm`, for a sync that must go through and be recorded.
fn synced(engine: &Engine, id: &String) -> Review {
    let review = confirm(engine, id);
    assert_eq!(last_error(engine, id), None, "the sync went through");
    review
}

/// The error of the job's last run, if it had one.
fn last_error(engine: &Engine, id: &String) -> Option<String> {
    view(engine, id).job.last_run.expect("the sync ran").error
}

/// Checks that the last run was not recorded, for the reason `why`, and that the job's next
/// look compares both folders afresh: it removes nothing and waits for the person's OK.
fn assert_not_recorded(engine: &Engine, id: &String, why: &str) -> Review {
    let error = last_error(engine, id).expect("the run reports that it couldn't be recorded");
    assert!(error.contains(why) && error.contains("Charon recorded nothing of this sync"), "{error}");
    let job = view(engine, id).job;
    assert!(job.run_in_progress && job.needs_review, "the job waits for an OK and won't trust its memory");
    assert_eq!(engine.sync_now(id).unwrap(), SyncStart::Review, "nothing runs by itself now");
    let review = engine.prepare_review(id).unwrap();
    assert_eq!((review.tally.source.removed, review.tally.destination.removed), (0, 0), "the next look removes nothing");
    assert!(
        review.warnings.iter().any(|w| w.level == Level::Caution && w.text.contains("compares both folders afresh")),
        "{:?}",
        review.warnings
    );
    review
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{} can't be read: {e}", path.display()))
}

fn set_mtime(path: &Path, time: SystemTime) {
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(time)).unwrap();
}

/// A time well in the past, `minutes` after a fixed start.
fn at(minutes: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + minutes * 60)
}

/// Whether the drive of `folder` keeps a change time for each file, which tells two
/// versions apart that have the same size and time.
#[cfg(unix)]
fn keeps_change_times(folder: &Path) -> bool {
    let keeps = charon_core::scan::Volume::of(folder).keeps_change_times();
    if !keeps {
        eprintln!("{} keeps no change times: nothing tells two look-alike versions apart there", folder.display());
    }
    keeps
}

/// Waits until a file saved now gets a later change time than `earlier`: drives keep change
/// times in steps (of a few milliseconds on Linux), and these tests need two saves told apart.
#[cfg(unix)]
fn wait_for_a_later_change_time(earlier: &Path) {
    use std::os::unix::fs::MetadataExt;
    let changed = |path: &Path| {
        let meta = fs::metadata(path).unwrap();
        (meta.ctime(), meta.ctime_nsec())
    };
    let probe = earlier.with_extension("probe");
    let deadline = Instant::now() + DEADLINE;
    loop {
        fs::write(&probe, "probe").unwrap();
        if changed(&probe) > changed(earlier) {
            break;
        }
        assert!(Instant::now() < deadline, "the drive's change times never moved");
        thread::sleep(Duration::from_millis(2));
    }
    fs::remove_file(&probe).unwrap();
}

#[test]
fn a_copy_the_drive_came_back_without_is_never_recorded_and_nothing_is_removed_for_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (mac, drive, data) = (tmp.path().join("mac"), tmp.path().join("drive"), tmp.path().join("data"));
    write(&mac.join("a.txt"), "a");
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let mut setup = job_setup(&mac, &drive, SyncMode::TwoWay);
    setup.full_automation = true;
    let id = add(engine, setup);
    synced(engine, &id);
    assert!(view(engine, &id).job.runs_by_itself());

    write(&mac.join("a_new.jpg"), "the only copy of this photo");
    // The copy's last step, the rename to its own name, never reached the drive: the drive
    // came back with the copy under its temporary name.
    let (landed, partial) = (drive.join("a_new.jpg"), drive.join(".charon-0123456789abcdef.part"));
    rig.at_the_end(move || fs::rename(&landed, &partial).unwrap());
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait_idle(engine, &id);
    assert!(rig.hook_fired());

    let review = assert_not_recorded(engine, &id, "“a_new.jpg” isn't on the destination anymore");
    assert_eq!(review.tally.destination.new_files, 1, "the photo is copied again");
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(engine, &id);
    assert_eq!(last_error(engine, &id), None);
    assert_eq!(read(&mac.join("a_new.jpg")), "the only copy of this photo");
    assert_eq!(read(&drive.join("a_new.jpg")), "the only copy of this photo");
}

#[test]
fn a_removal_the_drive_came_back_without_is_never_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let (mac, drive, data) = (tmp.path().join("mac"), tmp.path().join("drive"), tmp.path().join("data"));
    write(&mac.join("old.txt"), "old");
    set_mtime(&mac.join("old.txt"), at(0));
    write(&mac.join("keep.txt"), "keep");
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let id = add(engine, job_setup(&mac, &drive, SyncMode::TwoWay));
    synced(engine, &id);

    fs::remove_file(mac.join("old.txt")).unwrap();
    // The removal never reached the drive: the file is back, as it was.
    let removed = drive.join("old.txt");
    rig.at_the_end(move || {
        write(&removed, "old");
        set_mtime(&removed, at(0));
    });
    let review = confirm(engine, &id);
    assert_eq!(review.tally.destination.removed, 1);
    assert!(rig.hook_fired());
    assert_not_recorded(engine, &id, "“old.txt”, which this sync removed from the destination, is back");
}

#[test]
fn a_replacement_the_drive_came_back_without_is_never_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let (mac, drive, data) = (tmp.path().join("mac"), tmp.path().join("drive"), tmp.path().join("data"));
    write(&mac.join("plan.txt"), "first version");
    set_mtime(&mac.join("plan.txt"), at(0));
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let id = add(engine, job_setup(&mac, &drive, SyncMode::TwoWay));
    synced(engine, &id);

    write(&mac.join("plan.txt"), "second version");
    set_mtime(&mac.join("plan.txt"), at(10));
    // The new version's swap never reached the drive: the old version is back in its place.
    let replaced = drive.join("plan.txt");
    rig.at_the_end(move || {
        write(&replaced, "first version");
        set_mtime(&replaced, at(0));
    });
    let review = confirm(engine, &id);
    assert_eq!(review.tally.destination.replaced, 1);
    assert!(rig.hook_fired());
    let review = assert_not_recorded(engine, &id, "“plan.txt” on the destination is the version this sync replaced");
    assert_eq!(review.tally.source.replaced, 0, "the old version never replaces the new one: {:?}", review.tally);
    assert_eq!(read(&mac.join("plan.txt")), "second version");
}

/// A folder the run changed something in is gone at the end, although the run didn't
/// remove it: everything recorded about it would be wrong.
#[test]
fn a_folder_the_drive_came_back_without_is_never_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let (mac, drive, data, elsewhere) = (tmp.path().join("mac"), tmp.path().join("drive"), tmp.path().join("data"), tmp.path().join("elsewhere"));
    write(&mac.join("photos/keep.jpg"), "keep");
    write(&mac.join("photos/old.jpg"), "old");
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let id = add(engine, job_setup(&mac, &drive, SyncMode::TwoWay));
    synced(engine, &id);

    fs::remove_file(mac.join("photos/old.jpg")).unwrap();
    let photos = drive.join("photos");
    rig.at_the_end(move || fs::rename(&photos, &elsewhere).unwrap());
    let review = confirm(engine, &id);
    assert_eq!(review.tally.destination.removed, 1);
    assert!(rig.hook_fired());
    let review = assert_not_recorded(engine, &id, "isn't there anymore, although this sync had just made changes in it");
    assert_eq!(review.tally.destination.new_files, 1, "keep.jpg is copied again, never removed from the Mac");
    assert_eq!(read(&mac.join("photos/keep.jpg")), "keep");
}

/// What a person changes after a step is a change for the next look, not a lost one: an
/// edit to a file that was just copied, to the original it was copied from, and a new file
/// under the name of one that was just removed.
#[test]
fn changes_made_after_their_step_still_let_the_run_be_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let (mac, drive, data) = (tmp.path().join("mac"), tmp.path().join("drive"), tmp.path().join("data"));
    write(&mac.join("gone.txt"), "removed on the Mac");
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let id = add(engine, job_setup(&mac, &drive, SyncMode::TwoWay));
    synced(engine, &id);

    write(&mac.join("copied.txt"), "as copied");
    write(&mac.join("original.txt"), "as copied");
    fs::remove_file(mac.join("gone.txt")).unwrap();
    let (copied, original, gone) = (drive.join("copied.txt"), mac.join("original.txt"), drive.join("gone.txt"));
    rig.at_the_end(move || {
        write(&copied, "edited on the drive right after it arrived");
        set_mtime(&copied, SystemTime::now() + Duration::from_secs(60));
        write(&original, "edited on the Mac while the run went on");
        set_mtime(&original, SystemTime::now() + Duration::from_secs(60));
        write(&gone, "a new file under the old name");
    });
    synced(engine, &id);
    assert!(rig.hook_fired());
    assert!(!view(engine, &id).job.run_in_progress);

    // The next sync carries each of them as a change.
    let review = synced(engine, &id);
    assert_eq!((review.tally.source.removed, review.tally.destination.removed), (0, 0));
    assert_eq!(read(&mac.join("copied.txt")), "edited on the drive right after it arrived");
    assert_eq!(read(&drive.join("original.txt")), "edited on the Mac while the run went on");
    assert_eq!(read(&mac.join("gone.txt")), "a new file under the old name");
}

/// The drive comes back with an old version of a file this sync replaced, of the same size
/// and time as the new one (an encrypted container keeps its time when it's edited). Only
/// its change time, from before the scan, tells: the run isn't recorded, and the old
/// version never goes back over the new one.
#[cfg(unix)]
#[test]
fn a_replacement_lost_on_a_drive_is_seen_even_when_both_versions_look_alike() {
    let tmp = tempfile::tempdir().unwrap();
    if !keeps_change_times(tmp.path()) {
        return;
    }
    let (mac, drive, data) = (tmp.path().join("mac"), tmp.path().join("drive"), tmp.path().join("data"));
    let (before, elsewhere) = (tmp.path().join("before"), tmp.path().join("elsewhere"));
    // The drive's folder as it comes back: written before the copy the next sync replaces,
    // so its change time is older than the one the scan sees.
    write(&before.join("box/vault.hc"), "OLD-CONTENTS");
    set_mtime(&before.join("box/vault.hc"), at(0));
    wait_for_a_later_change_time(&before.join("box/vault.hc"));
    write(&mac.join("box/vault.hc"), "OLD-CONTENTS");
    set_mtime(&mac.join("box/vault.hc"), at(0));
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let id = add(engine, job_setup(&mac, &drive, SyncMode::TwoWay));
    synced(engine, &id);

    write(&mac.join("box/vault.hc"), "NEW-CONTENTS");
    set_mtime(&mac.join("box/vault.hc"), at(0));
    let (folder, old) = (drive.join("box"), before.join("box"));
    rig.at_the_end(move || {
        fs::rename(&folder, &elsewhere).unwrap();
        fs::rename(&old, &folder).unwrap();
    });
    let review = confirm(engine, &id);
    assert_eq!(review.tally.destination.replaced, 1);
    assert!(rig.hook_fired());
    assert_not_recorded(engine, &id, "“box/vault.hc” on the destination is a version older than the one this sync replaced");
    synced(engine, &id);
    assert_eq!(read(&mac.join("box/vault.hc")), "NEW-CONTENTS", "the old version never replaces the new one");
}

/// The same drive, coming back without the run that carried an edit of the container. The
/// next look compares both folders afresh, and the memory it can't trust shows that the
/// container changed on the Mac since the last sync: the two look-alike versions are read,
/// and they're a conflict for the person. Left open, it stays one on the looks after, so the
/// container is never taken for synced, and a later edit on the drive (which keeps its date
/// too) never replaces the Mac's edit unasked.
#[cfg(unix)]
#[test]
fn an_edit_that_kept_its_date_stays_a_conflict_after_a_run_that_never_landed() {
    let tmp = tempfile::tempdir().unwrap();
    if !keeps_change_times(tmp.path()) {
        return;
    }
    let (mac, drive, data) = (tmp.path().join("mac"), tmp.path().join("drive"), tmp.path().join("data"));
    let (before, elsewhere) = (tmp.path().join("before"), tmp.path().join("elsewhere"));
    write(&before.join("box/vault.hc"), "OLD-CONTENTS");
    set_mtime(&before.join("box/vault.hc"), at(0));
    wait_for_a_later_change_time(&before.join("box/vault.hc"));
    write(&mac.join("box/vault.hc"), "OLD-CONTENTS");
    set_mtime(&mac.join("box/vault.hc"), at(0));
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let id = add(engine, job_setup(&mac, &drive, SyncMode::TwoWay));
    synced(engine, &id);

    write(&mac.join("box/vault.hc"), "NEW-CONTENTS");
    set_mtime(&mac.join("box/vault.hc"), at(0));
    let (folder, old) = (drive.join("box"), before.join("box"));
    rig.at_the_end(move || {
        fs::rename(&folder, &elsewhere).unwrap();
        fs::rename(&old, &folder).unwrap();
    });
    confirm(engine, &id);
    assert!(rig.hook_fired());
    let review = assert_not_recorded(engine, &id, "“box/vault.hc” on the destination is a version older than the one this sync replaced");
    assert_eq!(review.tally.conflicts, 1, "the fresh comparison reads the container: {:?}", review.steps);

    // The person confirms the list and leaves the conflict for later.
    synced(engine, &id);
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.conflicts, 1, "still a conflict: {:?}", review.steps);
    write(&drive.join("box/vault.hc"), "DRV-CONTENTS");
    set_mtime(&drive.join("box/vault.hc"), at(0));
    let review = synced(engine, &id);
    assert_eq!((review.tally.conflicts, review.tally.source.replaced), (1, 0), "{:?}", review.steps);
    assert_eq!(read(&mac.join("box/vault.hc")), "NEW-CONTENTS");
    assert_eq!(read(&drive.join("box/vault.hc")), "DRV-CONTENTS");
}

/// The person sorts photos on the Mac while a long run copies them: a photo moved after its
/// copy was made doesn't fail the run or stop it from running by itself. Only that photo is
/// left out of the record, so the next look copies it back rather than take the move for a
/// removal.
#[test]
fn an_original_moved_during_the_run_is_left_out_of_the_record_and_never_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let (mac, drive, data) = (tmp.path().join("mac"), tmp.path().join("drive"), tmp.path().join("data"));
    write(&mac.join("keep.txt"), "keep");
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let mut setup = job_setup(&mac, &drive, SyncMode::TwoWay);
    setup.full_automation = true;
    let id = add(engine, setup);
    synced(engine, &id);

    write(&mac.join("inbox/a.jpg"), "photo a");
    write(&mac.join("inbox/b.jpg"), "photo b");
    let (from, to) = (mac.join("inbox/a.jpg"), mac.join("sorted/a.jpg"));
    rig.at_the_end(move || {
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::rename(&from, &to).unwrap();
    });
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait_idle(engine, &id);
    assert!(rig.hook_fired());
    assert_eq!(last_error(engine, &id), None, "a move during the run is no failure");
    let job = view(engine, &id).job;
    assert!(!job.run_in_progress && job.runs_by_itself());
    assert!(engine.log(&id, 20).iter().any(|line| line.text.starts_with("inbox/a.jpg: copied, but by the end of the sync its original was gone")));

    let review = engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.source.removed, review.tally.destination.removed), (0, 0), "{:?}", review.tally);
    assert_eq!(review.tally.source.new_files, 1, "inbox/a.jpg is copied back to the Mac");
    assert_eq!(review.tally.destination.new_files, 1, "sorted/a.jpg is copied to the drive");
}

/// The Mac's side is a drive too, and it comes back without the file the person saved on
/// it right before the sync: the copy just made is the only one left. The next look copies
/// it back instead of removing it.
#[test]
fn an_original_its_drive_lost_is_copied_back_never_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let (mac, drive, data) = (tmp.path().join("mac"), tmp.path().join("drive"), tmp.path().join("data"));
    write(&mac.join("keep.txt"), "keep");
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let id = add(engine, job_setup(&mac, &drive, SyncMode::TwoWay));
    synced(engine, &id);

    write(&mac.join("report.txt"), "the only copy of this report");
    let lost = mac.join("report.txt");
    rig.at_the_end(move || fs::remove_file(&lost).unwrap());
    let review = confirm(engine, &id);
    assert_eq!(review.tally.destination.new_files, 1);
    assert!(rig.hook_fired());
    assert_eq!(last_error(engine, &id), None);

    let review = synced(engine, &id);
    assert_eq!((review.tally.source.new_files, review.tally.destination.removed), (1, 0), "{:?}", review.tally);
    assert_eq!(read(&mac.join("report.txt")), "the only copy of this report");
    assert_eq!(read(&drive.join("report.txt")), "the only copy of this report");
}

/// The Mac's drive comes back with an older version of a file the person saved right before
/// the sync: its change time is from before the scan. The new version, now only on the
/// other drive, is left out of the record, so the old one never goes over it. The two
/// versions were saved a second apart, too close for their modification times to tell
/// which is older, so only the change time tells.
#[cfg(unix)]
#[test]
fn an_original_its_drive_turned_back_never_replaces_its_copy() {
    let tmp = tempfile::tempdir().unwrap();
    if !keeps_change_times(tmp.path()) {
        return;
    }
    let (mac, drive, data) = (tmp.path().join("mac"), tmp.path().join("drive"), tmp.path().join("data"));
    let (before, elsewhere) = (tmp.path().join("before"), tmp.path().join("elsewhere"));
    write(&mac.join("notes/report.txt"), "first draft");
    set_mtime(&mac.join("notes/report.txt"), at(10));
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let id = add(engine, job_setup(&mac, &drive, SyncMode::TwoWay));
    synced(engine, &id);

    // The Mac's folder as it comes back: with the first draft, from before the last save.
    write(&before.join("notes/report.txt"), "first draft");
    set_mtime(&before.join("notes/report.txt"), at(10));
    wait_for_a_later_change_time(&before.join("notes/report.txt"));
    write(&mac.join("notes/report.txt"), "final version, saved right before the sync");
    set_mtime(&mac.join("notes/report.txt"), at(10) + Duration::from_secs(1));
    let (folder, old) = (mac.join("notes"), before.join("notes"));
    rig.at_the_end(move || {
        fs::rename(&folder, &elsewhere).unwrap();
        fs::rename(&old, &folder).unwrap();
    });
    let review = confirm(engine, &id);
    assert_eq!(review.tally.destination.replaced, 1);
    assert!(rig.hook_fired());
    assert_eq!(last_error(engine, &id), None);

    let review = synced(engine, &id);
    assert_eq!(review.tally.destination.replaced, 0, "the first draft never replaces the final version: {:?}", review.tally);
    assert_eq!(read(&drive.join("notes/report.txt")), "final version, saved right before the sync");
}

/// Mirror and Backup keep no two-way memory, so an original removed during the run (a scan
/// folder another tool cleans up) is no concern of the run's: it's recorded, and the job
/// keeps running by itself.
fn an_original_removed_during_a_one_way_run_is_no_failure(mode: SyncMode) {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("keep.txt"), "keep");
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let mut setup = job_setup(&src, &dst, mode);
    setup.full_automation = true;
    let id = add(engine, setup);
    synced(engine, &id);

    write(&src.join("scan.pdf"), "a scanned letter");
    let removed = src.join("scan.pdf");
    rig.at_the_end(move || fs::remove_file(&removed).unwrap());
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait_idle(engine, &id);
    assert!(rig.hook_fired());
    assert_eq!(last_error(engine, &id), None);
    assert!(view(engine, &id).job.runs_by_itself());
    assert_eq!(read(&dst.join("scan.pdf")), "a scanned letter");
}

#[test]
fn an_original_removed_during_a_backup_is_no_failure() {
    an_original_removed_during_a_one_way_run_is_no_failure(SyncMode::Backup);
}

#[test]
fn an_original_removed_during_a_mirror_is_no_failure() {
    an_original_removed_during_a_one_way_run_is_no_failure(SyncMode::Mirror);
}
