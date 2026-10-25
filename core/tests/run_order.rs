//! The order a run carries out its steps in, and what it holds back: nothing may leave an
//! item on one side only while the run is under way, whether it is stopped, a copy fails,
//! or a file changes while the others are carried.
//!
//! An item that has to make room for one of another kind (a folder renamed, with a file
//! taking its old name) goes after every other copy, right before what takes its place, and
//! into .charon-trash even with safe deletes off. A copy that fails keeps every removal on
//! its side for the next sync. One left for the next sync because its file changed keeps
//! the removals there that could be its old copy, and only those: a file an app keeps
//! writing to never stops the rest of a job's removals. An app library the run removes
//! something of is carried whole after every other copy, and before anything else is
//! removed: what it removes waits like any removal, and what it brings lands before its
//! other copy goes. Every library carried then is written next to its targets and checked
//! again before any of them takes its place, and each is checked once more right before it
//! does: one that another program wrote to meanwhile waits whole. What each removes goes
//! only once they all landed, so a file moved from one library into another keeps its old
//! copy when the other can't take it. Nothing of a library is removed while one of its new
//! versions can't land, and a database's old journal goes back next to it. What only adds
//! to a library (a new photo) lands even when the rest of it waits: the version there goes
//! on as it was, with an item it doesn't use. A library new on the receiving side has no
//! old version to be mixed with: its files land as each is ready, while a database in it
//! waits for its journal.

use charon_core::model::{Progress, Review, StepFilter};
use charon_core::plan::OneWayPlanner;
use charon_core::run::{Care, Executor, Outcome, ProgressMeter, Shores};
use charon_core::scan::{ExcludeFilter, Scanner, Volume};
use charon_core::{ConflictRule, Engine, EngineEvent, JobSetup, JobView, Phase, SyncMode, Trigger};
use std::cell::RefCell;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// How long any wait may take before the test fails.
const DEADLINE: Duration = Duration::from_secs(60);

/// The photos of the folder the tests rename.
const PHOTOS: [&str; 4] = ["a.jpg", "b.jpg", "c.jpg", "d.jpg"];

type Hook = Box<dyn FnOnce() + Send>;

/// An engine that can run a one-shot hook on its sync thread right before the first step of
/// a run acts (after the list was made and the run began).
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
                // The first step is about to act: nothing of this run has been carried yet.
                if progress.phase == Phase::Copying && progress.done_items == 0 && progress.current.is_some() {
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

    /// Runs `hook` on the sync thread right before the next run's first step acts.
    fn at_first_step(&self, hook: impl FnOnce() + Send + 'static) {
        self.fired.store(false, Ordering::SeqCst);
        *self.hook.lock().unwrap() = Some(Box::new(hook));
    }

    fn hook_fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }
}

/// The engine sends live progress at most every 120 ms. A hook needs the first step's
/// progress, so the previous run's last progress must be older than that. (This only lets
/// the window pass; whether the hook ran is always asserted.)
fn let_progress_throttle_pass() {
    thread::sleep(Duration::from_millis(150));
}

/// A job with safe deletes off: what it removes is gone for good, unless it makes room.
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
        safe_delete: false,
        full_automation: false,
    }
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

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{} can't be read: {e}", path.display()))
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

/// The contents of every file called `name` in `root`'s .charon-trash.
fn trashed(root: &Path, name: &str) -> Vec<String> {
    walk(&root.join(".charon-trash")).iter().filter(|path| path.file_name().is_some_and(|n| n == name)).map(|path| read(path)).collect()
}

/// The contents of a photo. Each photo has its own size, so none looks like another.
fn photo(name: &str) -> String {
    let place = PHOTOS.iter().position(|photo| *photo == name).expect("one of the photos");
    format!("the photo {name}").repeat(place + 1)
}

/// A job whose folders hold the same `Photos` folder after its first confirmed sync, then
/// `Photos` renamed to `Photos 2025` in the source. Returns the folders and the job.
fn renamed_photos(rig: &Rig, root: &Path, mode: SyncMode) -> (PathBuf, PathBuf, String) {
    let (src, dst) = (root.join("src"), root.join("dst"));
    for name in PHOTOS {
        write(&src.join("Photos").join(name), &photo(name));
    }
    let id = rig.engine.add_job(job_setup(&src, &dst, mode)).expect("the job is added").job.id;
    confirm(&rig.engine, &id);
    assert_eq!(read(&dst.join("Photos/d.jpg")), photo("d.jpg"));
    fs::rename(src.join("Photos"), src.join("Photos 2025")).unwrap();
    (src, dst, id)
}

/// Each photo is still somewhere in `dst`: in its old folder, its new one, or the trash.
fn assert_every_photo_kept(dst: &Path) {
    for name in PHOTOS {
        let kept = [dst.join("Photos").join(name), dst.join("Photos 2025").join(name)]
            .iter()
            .any(|path| path.is_file() && read(path) == photo(name))
            || trashed(dst, name).contains(&photo(name));
        assert!(kept, "the destination lost {name} while the source still has it");
    }
}

/// A folder renamed in the source, with a file taking its old name, and the person stops
/// the sync right as it starts: whatever ran first, every photo is still in the
/// destination.
fn a_folder_that_became_a_file_keeps_its_photos_when_the_run_stops(mode: SyncMode) {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (src, dst, id) = renamed_photos(&rig, tmp.path(), mode);
    write(&src.join("Photos"), "a note: moved to Photos 2025");
    let review = rig.engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.removed, 4, "{:?}", review.tally);
    let (engine, stopped) = (rig.engine.clone(), id.clone());
    rig.at_first_step(move || engine.cancel(&stopped));
    let_progress_throttle_pass();
    rig.engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&rig.engine, &id);
    assert!(rig.hook_fired());
    assert!(view(&rig.engine, &id).job.last_run.unwrap().cancelled);
    assert_every_photo_kept(&dst);
    assert!(dst.join("Photos").is_dir(), "the old folder goes only after every other copy");
}

#[test]
fn two_way_keeps_a_folder_that_became_a_file_when_the_run_stops() {
    a_folder_that_became_a_file_keeps_its_photos_when_the_run_stops(SyncMode::TwoWay);
}

#[test]
fn mirror_keeps_a_folder_that_became_a_file_when_the_run_stops() {
    a_folder_that_became_a_file_keeps_its_photos_when_the_run_stops(SyncMode::Mirror);
}

/// The same, but the copies into the new folder fail (a file took its name on the
/// destination meanwhile): the old folder, which only makes room, stays as it is.
fn a_folder_that_became_a_file_stays_when_copies_fail(mode: SyncMode) {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (src, dst, id) = renamed_photos(&rig, tmp.path(), mode);
    write(&src.join("Photos"), "a note: moved to Photos 2025");
    let review = rig.engine.prepare_review(&id).unwrap();
    let in_the_way = dst.join("Photos 2025");
    rig.at_first_step(move || write(&in_the_way, "a file where the new folder goes"));
    let_progress_throttle_pass();
    rig.engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&rig.engine, &id);
    assert!(rig.hook_fired());
    assert!(view(&rig.engine, &id).job.last_run.unwrap().failed > 0);
    for name in PHOTOS {
        assert_eq!(read(&dst.join("Photos").join(name)), photo(name), "the old folder keeps {name}");
    }
}

#[test]
fn two_way_keeps_a_folder_that_became_a_file_when_copies_fail() {
    a_folder_that_became_a_file_stays_when_copies_fail(SyncMode::TwoWay);
}

#[test]
fn mirror_keeps_a_folder_that_became_a_file_when_copies_fail() {
    a_folder_that_became_a_file_stays_when_copies_fail(SyncMode::Mirror);
}

/// With nothing in the way, the file takes the folder's place, and the folder's items go
/// into .charon-trash although safe deletes are off: until the file landed, they were the
/// destination's only copy of what was there. The list says so.
fn what_makes_room_goes_to_the_trash_with_safe_deletes_off(mode: SyncMode) {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (src, dst, id) = renamed_photos(&rig, tmp.path(), mode);
    write(&src.join("Photos"), "a note: moved to Photos 2025");
    let review = rig.engine.prepare_review(&id).unwrap();
    let removals = rig.engine.review_steps(&id, &review.review_id, StepFilter::Removals, "", 0, 50).unwrap();
    assert_eq!(removals.total, 5, "four photos and their folder");
    for step in removals.steps.iter().filter(|step| step.path != "Photos") {
        assert!(step.reason.ends_with("goes to .charon-trash"), "{}: {}", step.path, step.reason);
    }
    assert!(!review.warnings.iter().any(|w| w.text.contains("Safe deletes are off")), "{:?}", review.warnings);
    rig.engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&rig.engine, &id);
    assert_eq!(view(&rig.engine, &id).job.last_run.unwrap().error, None);
    assert_eq!(read(&dst.join("Photos")), "a note: moved to Photos 2025");
    for name in PHOTOS {
        assert_eq!(read(&dst.join("Photos 2025").join(name)), photo(name));
        assert_eq!(trashed(&dst, name), vec![photo(name)], "{name} went into the trash");
    }
}

#[test]
fn two_way_keeps_what_makes_room_in_the_trash_with_safe_deletes_off() {
    what_makes_room_goes_to_the_trash_with_safe_deletes_off(SyncMode::TwoWay);
}

#[test]
fn mirror_keeps_what_makes_room_in_the_trash_with_safe_deletes_off() {
    what_makes_room_goes_to_the_trash_with_safe_deletes_off(SyncMode::Mirror);
}

/// A plain folder rename, and an app writes to one photo in the new folder while the run
/// carries the others: its copy waits for the next sync, and so does the removal of its old
/// copy, the destination's only one. The old copies of the photos that arrived go.
fn a_photo_edited_during_the_run_keeps_its_old_copy(mode: SyncMode) {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (src, dst, id) = renamed_photos(&rig, tmp.path(), mode);
    let review = rig.engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.removed, 4);
    let edited = src.join("Photos 2025/d.jpg");
    rig.at_first_step(move || fs::OpenOptions::new().append(true).open(&edited).unwrap().write_all(b", with new tags").unwrap());
    let_progress_throttle_pass();
    rig.engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&rig.engine, &id);
    assert!(rig.hook_fired());
    let run = view(&rig.engine, &id).job.last_run.unwrap();
    assert_eq!((run.copied, run.deleted), (3, 3), "{}", run.describe());
    assert_eq!(read(&dst.join("Photos/d.jpg")), photo("d.jpg"), "the old copy waits for the new one");
    assert!(!dst.join("Photos/a.jpg").exists(), "a.jpg arrived in its new folder, so its old copy went");

    // The next sync carries the edited photo, then removes the old folder.
    confirm(&rig.engine, &id);
    assert_eq!(read(&dst.join("Photos 2025/d.jpg")), format!("{}, with new tags", photo("d.jpg")));
    assert!(!dst.join("Photos").exists());
}

#[test]
fn two_way_keeps_the_old_copy_of_a_photo_edited_during_the_run() {
    a_photo_edited_during_the_run_keeps_its_old_copy(SyncMode::TwoWay);
}

#[test]
fn mirror_keeps_the_old_copy_of_a_photo_edited_during_the_run() {
    a_photo_edited_during_the_run_keeps_its_old_copy(SyncMode::Mirror);
}

/// A plain folder rename, and something new takes one photo's new place on the destination
/// while the run goes on: that copy is left for the next sync, and the removal of the
/// photo's old copy waits too, while the others go.
fn a_photo_whose_new_place_was_taken_keeps_its_old_copy(mode: SyncMode) {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (_src, dst, id) = renamed_photos(&rig, tmp.path(), mode);
    let review = rig.engine.prepare_review(&id).unwrap();
    let taken = dst.join("Photos 2025/d.jpg");
    rig.at_first_step(move || write(&taken, "saved by an app on the destination"));
    let_progress_throttle_pass();
    rig.engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&rig.engine, &id);
    assert!(rig.hook_fired());
    assert_eq!(view(&rig.engine, &id).job.last_run.unwrap().deleted, 3);
    assert_eq!(read(&dst.join("Photos/d.jpg")), photo("d.jpg"), "the old copy waits for the new one");
    assert!(!dst.join("Photos/c.jpg").exists());
    assert_eq!(read(&dst.join("Photos 2025/d.jpg")), "saved by an app on the destination");
}

#[test]
fn two_way_keeps_the_old_copy_of_a_photo_whose_new_place_was_taken() {
    a_photo_whose_new_place_was_taken_keeps_its_old_copy(SyncMode::TwoWay);
}

#[test]
fn mirror_keeps_the_old_copy_of_a_photo_whose_new_place_was_taken() {
    a_photo_whose_new_place_was_taken_keeps_its_old_copy(SyncMode::Mirror);
}

/// An app keeps writing to a log while the person cleans up an old project: the log's copy
/// waits for the next sync on every run, and the project still goes. Nothing that was
/// removed could be an old copy of the log.
fn a_busy_file_doesnt_hold_back_other_removals(mode: SyncMode) {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
    write(&src.join("app/activity.log"), "started\n");
    write(&src.join("old-project/big.mov"), "a long film nobody needs anymore");
    let id = rig.engine.add_job(job_setup(&src, &dst, mode)).expect("the job is added").job.id;
    confirm(&rig.engine, &id);
    assert!(dst.join("old-project/big.mov").is_file());

    fs::remove_dir_all(src.join("old-project")).unwrap();
    let log = src.join("app/activity.log");
    fs::OpenOptions::new().append(true).open(&log).unwrap().write_all(b"saved\n").unwrap();
    let review = rig.engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.destination.replaced, review.tally.destination.removed), (1, 1), "{:?}", review.tally);
    rig.at_first_step(move || fs::OpenOptions::new().append(true).open(&log).unwrap().write_all(b"saved again\n").unwrap());
    let_progress_throttle_pass();
    rig.engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&rig.engine, &id);
    assert!(rig.hook_fired());
    let run = view(&rig.engine, &id).job.last_run.unwrap();
    assert_eq!((run.copied, run.skipped), (0, 1), "{}", run.describe());
    assert!(!dst.join("old-project").exists(), "the old project went although the log was busy");
    assert_eq!(read(&dst.join("app/activity.log")), "started\n");
}

#[test]
fn two_way_removes_what_a_busy_file_couldnt_be_an_old_copy_of() {
    a_busy_file_doesnt_hold_back_other_removals(SyncMode::TwoWay);
}

#[test]
fn mirror_removes_what_a_busy_file_couldnt_be_an_old_copy_of() {
    a_busy_file_doesnt_hold_back_other_removals(SyncMode::Mirror);
}

/// A folder renamed in the source, and a note moved from another folder into its old place,
/// in a Mirror job. While the run clears the old folder on the destination, an app saves a
/// new photo into it there: the folder stays, so the note can't take its place, and the
/// removal of the note's old copy waits with it. It says why, truthfully: the note itself
/// didn't change.
#[test]
fn mirror_says_why_the_old_copy_of_a_file_whose_place_couldnt_be_cleared_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
    for name in PHOTOS {
        write(&src.join("Photos").join(name), &photo(name));
    }
    write(&src.join("Notes/Photos"), "where the photos went");
    fs::create_dir_all(&dst).unwrap();
    assert_eq!(mirror_on_its_own(&src, &dst, "", || {}).copied, 5);
    fs::rename(src.join("Photos"), src.join("Photos 2025")).unwrap();
    fs::rename(src.join("Notes/Photos"), src.join("Photos")).unwrap();
    let saved = dst.join("Photos/new.jpg");
    let outcome = mirror_on_its_own(&src, &dst, "Photos/", || write(&saved, "a photo saved on the destination"));
    assert_eq!(read(&saved), "a photo saved on the destination");
    assert_eq!(read(&dst.join("Notes/Photos")), "where the photos went", "the note's old copy waits for the note");
    // Keys, which a drive that ignores case folds.
    let skipped: Vec<(String, &str)> = outcome.skipped.iter().map(|(key, why)| (key.to_lowercase(), why.as_str())).collect();
    assert!(
        skipped.contains(&(
            "notes/photos".into(),
            "kept for now: it may be the old copy of “Photos”, which wasn't copied to the destination yet: the item of another kind in its place there couldn't go first"
        )),
        "{skipped:?}"
    );
}

/// A job whose folders hold the same Logic project, with its arrangement and a take, after
/// its first confirmed sync. Returns the folders and the job.
fn a_logic_project(rig: &Rig, root: &Path, mode: SyncMode) -> (PathBuf, PathBuf, String) {
    let (src, dst) = (root.join("src"), root.join("dst"));
    write(&src.join("Song.logicx/arrangement.txt"), "the arrangement");
    write(&src.join("Song.logicx/take.wav"), "the take");
    let id = rig.engine.add_job(job_setup(&src, &dst, mode)).expect("the job is added").job.id;
    confirm(&rig.engine, &id);
    assert_eq!(read(&dst.join("Song.logicx/take.wav")), "the take");
    (src, dst, id)
}

/// A take moved out of a Logic project whose arrangement changed too, and an app writes to
/// the take in its new place while the run goes on. The project is carried whole, and what
/// it removes waits for every other copy, like any removal: the take's copy is left for the
/// next sync, and so is the whole project, with the removal of the take's old copy (the
/// destination's only one).
fn a_take_moved_out_of_a_library_and_edited_during_the_run_keeps_its_old_copy(mode: SyncMode) {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (src, dst, id) = a_logic_project(&rig, tmp.path(), mode);
    fs::create_dir(src.join("Takes")).unwrap();
    fs::rename(src.join("Song.logicx/take.wav"), src.join("Takes/take.wav")).unwrap();
    write(&src.join("Song.logicx/arrangement.txt"), "the arrangement, with a new verse");
    let review = rig.engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.removed, 1, "{:?}", review.tally);
    let take = src.join("Takes/take.wav");
    rig.at_first_step(move || fs::OpenOptions::new().append(true).open(&take).unwrap().write_all(b", recorded again").unwrap());
    let_progress_throttle_pass();
    rig.engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&rig.engine, &id);
    assert!(rig.hook_fired());
    assert_eq!(read(&dst.join("Song.logicx/take.wav")), "the take", "the old copy waits for the new one");
    assert_eq!(read(&dst.join("Song.logicx/arrangement.txt")), "the arrangement", "the project isn't mixed");
    let run = view(&rig.engine, &id).job.last_run.unwrap();
    assert_eq!((run.copied, run.deleted), (0, 0), "{}", run.describe());

    // The next sync carries the take and the project.
    confirm(&rig.engine, &id);
    assert_eq!(read(&dst.join("Takes/take.wav")), "the take, recorded again");
    assert_eq!(read(&dst.join("Song.logicx/arrangement.txt")), "the arrangement, with a new verse");
    assert!(!dst.join("Song.logicx/take.wav").exists());
}

#[test]
fn two_way_keeps_the_old_copy_of_a_take_moved_out_of_a_library_and_edited_during_the_run() {
    a_take_moved_out_of_a_library_and_edited_during_the_run_keeps_its_old_copy(SyncMode::TwoWay);
}

#[test]
fn mirror_keeps_the_old_copy_of_a_take_moved_out_of_a_library_and_edited_during_the_run() {
    a_take_moved_out_of_a_library_and_edited_during_the_run_keeps_its_old_copy(SyncMode::Mirror);
}

/// The same, but the take's new folder can't be made (a file took its name on the
/// destination meanwhile), so its copy fails: the project stays as it was.
fn a_take_moved_out_of_a_library_keeps_its_old_copy_when_its_copy_fails(mode: SyncMode) {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (src, dst, id) = a_logic_project(&rig, tmp.path(), mode);
    fs::create_dir(src.join("Takes")).unwrap();
    fs::rename(src.join("Song.logicx/take.wav"), src.join("Takes/take.wav")).unwrap();
    write(&src.join("Song.logicx/arrangement.txt"), "the arrangement, with a new verse");
    let review = rig.engine.prepare_review(&id).unwrap();
    let in_the_way = dst.join("Takes");
    rig.at_first_step(move || write(&in_the_way, "a file where the new folder goes"));
    let_progress_throttle_pass();
    rig.engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&rig.engine, &id);
    assert!(rig.hook_fired());
    assert_eq!(read(&dst.join("Song.logicx/take.wav")), "the take", "the old copy waits for the new one");
    assert_eq!(read(&dst.join("Song.logicx/arrangement.txt")), "the arrangement", "the project isn't mixed");
    let run = view(&rig.engine, &id).job.last_run.unwrap();
    assert!(run.failed > 0 && run.deleted == 0, "{}", run.describe());
}

#[test]
fn two_way_keeps_the_old_copy_of_a_take_moved_out_of_a_library_when_its_copy_fails() {
    a_take_moved_out_of_a_library_keeps_its_old_copy_when_its_copy_fails(SyncMode::TwoWay);
}

#[test]
fn mirror_keeps_the_old_copy_of_a_take_moved_out_of_a_library_when_its_copy_fails() {
    a_take_moved_out_of_a_library_keeps_its_old_copy_when_its_copy_fails(SyncMode::Mirror);
}

/// A take moved into a Logic project from a folder deeper down, another take removed from
/// the project, and an app writes to the moved take while the run goes on. The project is
/// carried after every other copy, but still before anything else is removed: it waits for
/// the next sync, and so does the removal of the moved take's old copy, the destination's
/// only one.
fn a_take_moved_into_a_library_keeps_its_old_copy_when_the_library_waits(mode: SyncMode) {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (src, dst, id) = a_logic_project(&rig, tmp.path(), mode);
    write(&src.join("Old takes/2024/solo.wav"), "the solo");
    confirm(&rig.engine, &id);
    assert_eq!(read(&dst.join("Old takes/2024/solo.wav")), "the solo");
    fs::rename(src.join("Old takes/2024/solo.wav"), src.join("Song.logicx/solo.wav")).unwrap();
    fs::remove_file(src.join("Song.logicx/take.wav")).unwrap();
    let review = rig.engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.removed, 2, "{:?}", review.tally);
    let solo = src.join("Song.logicx/solo.wav");
    rig.at_first_step(move || fs::OpenOptions::new().append(true).open(&solo).unwrap().write_all(b", recorded again").unwrap());
    let_progress_throttle_pass();
    rig.engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&rig.engine, &id);
    assert!(rig.hook_fired());
    assert_eq!(read(&dst.join("Old takes/2024/solo.wav")), "the solo", "the old copy waits for the new one");
    assert_eq!(read(&dst.join("Song.logicx/take.wav")), "the take", "the project waits whole");
    let run = view(&rig.engine, &id).job.last_run.unwrap();
    assert_eq!((run.copied, run.deleted), (0, 0), "{}", run.describe());

    // The next sync carries the project, then removes the old copy.
    confirm(&rig.engine, &id);
    assert_eq!(read(&dst.join("Song.logicx/solo.wav")), "the solo, recorded again");
    assert!(!dst.join("Song.logicx/take.wav").exists() && !dst.join("Old takes/2024/solo.wav").exists());
}

#[test]
fn two_way_keeps_the_old_copy_of_a_take_moved_into_a_library_that_waits() {
    a_take_moved_into_a_library_keeps_its_old_copy_when_the_library_waits(SyncMode::TwoWay);
}

#[test]
fn mirror_keeps_the_old_copy_of_a_take_moved_into_a_library_that_waits() {
    a_take_moved_into_a_library_keeps_its_old_copy_when_the_library_waits(SyncMode::Mirror);
}

/// Every file under `root`, by its path there with `/` between the names, in order.
fn files_in(root: &Path) -> Vec<String> {
    let mut files: Vec<String> = walk(root)
        .iter()
        .map(|path| path.strip_prefix(root).unwrap().components().map(|part| part.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/"))
        .collect();
    files.sort();
    files
}

/// Puts a file where `root`'s .charon-trash goes: nothing can be kept there until it's
/// removed, so a copy that has to keep the version it replaces fails.
fn block_the_trash(root: &Path) {
    let trash = root.join(".charon-trash");
    if trash.is_dir() {
        fs::remove_dir_all(&trash).unwrap();
    }
    fs::write(&trash, "a file where the trash goes").unwrap();
}

/// A job whose folders hold the same two Logic projects after its first confirmed sync, an
/// album with a take and a single with an old take. Then, in the source, the take is moved
/// from the album into the single (to `into`, a path in the single), and the single's old
/// take is removed. Returns the folders and the job.
fn a_take_moved_between_two_projects(rig: &Rig, root: &Path, mode: SyncMode, into: &str) -> (PathBuf, PathBuf, String) {
    let (src, dst) = (root.join("src"), root.join("dst"));
    write(&src.join("Album.logicx/arrangement.txt"), "the album");
    write(&src.join("Album.logicx/take.wav"), "the take");
    write(&src.join("Single.logicx/arrangement.txt"), "the single");
    write(&src.join("Single.logicx/old.wav"), "an old take");
    let id = rig.engine.add_job(job_setup(&src, &dst, mode)).expect("the job is added").job.id;
    confirm(&rig.engine, &id);
    assert_eq!(read(&dst.join("Album.logicx/take.wav")), "the take");
    let moved = src.join("Single.logicx").join(into);
    fs::create_dir_all(moved.parent().unwrap()).unwrap();
    fs::rename(src.join("Album.logicx/take.wav"), &moved).unwrap();
    fs::remove_file(src.join("Single.logicx/old.wav")).unwrap();
    (src, dst, id)
}

/// A take moved from one Logic project into another that loses an old take too, and an app
/// writes to the take in its new place while the run goes on. Both projects remove
/// something, so both are carried after every other copy, the album first by name, and what
/// they remove goes only once both landed: the single waits for the next sync, and so does
/// the take's old copy in the album, the destination's only one.
fn a_take_moved_between_libraries_keeps_its_old_copy_when_the_receiving_one_waits(mode: SyncMode) {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (src, dst, id) = a_take_moved_between_two_projects(&rig, tmp.path(), mode, "take.wav");
    let review = rig.engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.removed, 2, "{:?}", review.tally);
    let take = src.join("Single.logicx/take.wav");
    rig.at_first_step(move || fs::OpenOptions::new().append(true).open(&take).unwrap().write_all(b", recorded again").unwrap());
    let_progress_throttle_pass();
    rig.engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&rig.engine, &id);
    assert!(rig.hook_fired());
    assert_eq!(read(&dst.join("Album.logicx/take.wav")), "the take", "the old copy waits for the new one");
    assert_eq!(files_in(&dst.join("Single.logicx")), ["arrangement.txt", "old.wav"], "the single waits whole");
    let run = view(&rig.engine, &id).job.last_run.unwrap();
    assert_eq!((run.copied, run.deleted), (0, 0), "{}", run.describe());

    // The next sync carries the take into the single, then removes its old copies.
    confirm(&rig.engine, &id);
    assert_eq!(read(&dst.join("Single.logicx/take.wav")), "the take, recorded again");
    assert_eq!(files_in(&dst.join("Album.logicx")), ["arrangement.txt"]);
    assert_eq!(files_in(&dst.join("Single.logicx")), ["arrangement.txt", "take.wav"]);
}

#[test]
fn two_way_keeps_the_old_copy_of_a_take_moved_into_another_library_that_waits() {
    a_take_moved_between_libraries_keeps_its_old_copy_when_the_receiving_one_waits(SyncMode::TwoWay);
}

#[test]
fn mirror_keeps_the_old_copy_of_a_take_moved_into_another_library_that_waits() {
    a_take_moved_between_libraries_keeps_its_old_copy_when_the_receiving_one_waits(SyncMode::Mirror);
}

/// The same, but the take goes into a new folder of the single, and that folder can't be
/// made on the destination (a file took its name meanwhile), so the take's copy fails:
/// neither project changes there.
fn a_take_moved_between_libraries_keeps_its_old_copy_when_its_copy_fails(mode: SyncMode) {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (_src, dst, id) = a_take_moved_between_two_projects(&rig, tmp.path(), mode, "Audio Files/take.wav");
    let review = rig.engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.removed, 2, "{:?}", review.tally);
    let in_the_way = dst.join("Single.logicx/Audio Files");
    rig.at_first_step(move || write(&in_the_way, "a file where the new folder goes"));
    let_progress_throttle_pass();
    rig.engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&rig.engine, &id);
    assert!(rig.hook_fired());
    assert_eq!(read(&dst.join("Album.logicx/take.wav")), "the take", "the old copy waits for the new one");
    assert_eq!(read(&dst.join("Single.logicx/old.wav")), "an old take", "the single waits whole");
    let run = view(&rig.engine, &id).job.last_run.unwrap();
    assert!(run.failed > 0 && run.deleted == 0, "{}", run.describe());
}

#[test]
fn two_way_keeps_the_old_copy_of_a_take_moved_into_another_library_when_its_copy_fails() {
    a_take_moved_between_libraries_keeps_its_old_copy_when_its_copy_fails(SyncMode::TwoWay);
}

#[test]
fn mirror_keeps_the_old_copy_of_a_take_moved_into_another_library_when_its_copy_fails() {
    a_take_moved_between_libraries_keeps_its_old_copy_when_its_copy_fails(SyncMode::Mirror);
}

/// The same in a two-way job, but the single's arrangement changed too, and its new
/// version can't take its place on the destination (the version it replaces can't be kept
/// there: .charon-trash is a file). Every project is ready by then, so this only shows once
/// the single's new versions take their places: the album's removal still waits for them,
/// and neither project's version changes. Only the take lands in the single, which its
/// version there doesn't use.
#[test]
fn two_way_keeps_the_old_copy_of_a_take_moved_into_another_library_whose_new_version_cant_land() {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (src, dst, id) = a_take_moved_between_two_projects(&rig, tmp.path(), SyncMode::TwoWay, "take.wav");
    write(&src.join("Single.logicx/arrangement.txt"), "the single, with a new chorus");
    let review = rig.engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.destination.replaced, review.tally.destination.removed), (1, 2), "{:?}", review.tally);
    let blocked = dst.clone();
    rig.at_first_step(move || block_the_trash(&blocked));
    let_progress_throttle_pass();
    rig.engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&rig.engine, &id);
    assert!(rig.hook_fired());
    assert_eq!(read(&dst.join("Album.logicx/take.wav")), "the take", "the old copy waits for the new one");
    assert_eq!(files_in(&dst.join("Single.logicx")), ["arrangement.txt", "old.wav", "take.wav"], "the single isn't mixed");
    assert_eq!(read(&dst.join("Single.logicx/arrangement.txt")), "the single");
    let run = view(&rig.engine, &id).job.last_run.unwrap();
    assert!(run.failed > 0 && (run.copied, run.deleted) == (1, 0), "{}", run.describe());

    // With the trash a folder again, the next sync carries both projects.
    fs::remove_file(dst.join(".charon-trash")).unwrap();
    confirm(&rig.engine, &id);
    assert_eq!(files_in(&dst.join("Album.logicx")), ["arrangement.txt"]);
    assert_eq!(files_in(&dst.join("Single.logicx")), ["arrangement.txt", "take.wav"]);
    assert_eq!(read(&dst.join("Single.logicx/arrangement.txt")), "the single, with a new chorus");
}

/// A Mirror sync of `src` onto `dst` by an executor of its own, outside an engine, that runs
/// `hook` right when the run starts on the first item whose path begins with `needle`. An
/// engine sends its live progress at most every 120 ms, so a hook on it can miss an item
/// done sooner; the executor's own progress misses none. Returns what the run did, once its
/// changes are on the drive.
fn mirror_on_its_own(src: &Path, dst: &Path, needle: &str, hook: impl FnOnce()) -> Outcome {
    let filter = ExcludeFilter::none();
    let scan = |root: &Path| Scanner::new(&filter).scan(root, Volume::of(root)).expect("the folder is scanned");
    let (source, destination) = (scan(src), scan(dst));
    let plan = OneWayPlanner::mirror().plan(&source, &destination);
    let shores = Shores { source: src.to_path_buf(), destination: dst.to_path_buf() };
    let care = Care { safe_delete: false, keep_replaced: false, two_way: false };
    let cancel = AtomicBool::new(false);
    let hook = RefCell::new(Some(hook));
    let observer = |progress: &Progress| {
        if progress.current.as_deref().is_some_and(|current| current.starts_with(needle)) {
            let armed = hook.borrow_mut().take();
            if let Some(hook) = armed {
                hook();
            }
        }
    };
    let mut meter = ProgressMeter::for_plan(&plan, &observer);
    let executor = Executor::new(&shores, &source, &destination, care, &cancel);
    let mut outcome = executor.run(&plan, &mut meter);
    executor.flush(&mut outcome).expect("the run's changes are on the drive");
    assert!(hook.borrow().is_none(), "the run came to an item starting with {needle}");
    outcome
}

/// Two Logic projects that both change and both lose an old take, in a Mirror job: both are
/// carried right before the plan's first plain removal, the album written next to its
/// targets first, then the single's takes recorded again. While those are written,
/// `meanwhile` changes the album in the destination (another computer saving it there). The
/// album hasn't taken its place yet, and it's checked again before it does: it waits whole,
/// as it was there, but for the bounce it adds, which its version there doesn't use. The
/// single, which nothing changed meanwhile, is carried.
fn a_library_written_to_while_the_next_one_is_written_waits_whole(meanwhile: impl FnOnce(&Path)) {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
    write(&src.join("Album.logicx/arrangement.txt"), "the album");
    write(&src.join("Album.logicx/notes.txt"), "the album's notes");
    write(&src.join("Album.logicx/old.wav"), "an old take of the album");
    write(&src.join("Single.logicx/arrangement.txt"), "the single");
    write(&src.join("Single.logicx/old.wav"), "an old take of the single, long gone");
    for n in 1..=3 {
        write(&src.join(format!("Single.logicx/take{n}.wav")), &format!("take {n}"));
    }
    fs::create_dir_all(&dst).unwrap();
    assert_eq!(mirror_on_its_own(&src, &dst, "", || {}).copied, 8);
    write(&src.join("Album.logicx/arrangement.txt"), "the album, remixed");
    write(&src.join("Album.logicx/notes.txt"), "the album's notes, remixed");
    write(&src.join("Album.logicx/bounce.wav"), "a bounce of the album");
    fs::remove_file(src.join("Album.logicx/old.wav")).unwrap();
    for n in 1..=3 {
        write(&src.join(format!("Single.logicx/take{n}.wav")), &format!("take {n}, recorded again"));
    }
    fs::remove_file(src.join("Single.logicx/old.wav")).unwrap();
    let album = dst.join("Album.logicx");
    let outcome = mirror_on_its_own(&src, &dst, "Single.logicx/take", || meanwhile(&album));
    assert_eq!(read(&album.join("arrangement.txt")), "the album", "nothing of the album took its place");
    assert_eq!(read(&album.join("old.wav")), "an old take of the album", "nothing of the album was removed");
    assert_eq!(read(&album.join("bounce.wav")), "a bounce of the album", "what only adds to the album landed");
    for n in 1..=3 {
        assert_eq!(read(&dst.join(format!("Single.logicx/take{n}.wav"))), format!("take {n}, recorded again"));
    }
    assert_eq!(files_in(&dst.join("Single.logicx")), ["arrangement.txt", "take1.wav", "take2.wav", "take3.wav"], "the single is carried");
    assert_eq!((outcome.copied, outcome.deleted), (4, 1), "{outcome:?}");
    // Keys, which a drive that ignores case folds.
    let mut waited: Vec<String> = outcome.waited.iter().map(|(key, _)| key.to_lowercase()).collect();
    waited.sort();
    assert_eq!(waited, ["album.logicx/arrangement.txt", "album.logicx/notes.txt", "album.logicx/old.wav"]);
    assert!(
        outcome.waited.iter().all(|(_, why)| why.ends_with("something in “Album.logicx”, an app library or document, changed since the scan")),
        "{:?}",
        outcome.waited
    );
}

#[test]
fn mirror_leaves_a_library_saved_to_while_the_next_one_is_written_whole() {
    a_library_written_to_while_the_next_one_is_written_waits_whole(|album| {
        fs::write(album.join("notes.txt"), "the album's notes, saved on another computer").unwrap();
    });
}

#[test]
fn mirror_leaves_a_library_given_a_file_while_the_next_one_is_written_whole() {
    a_library_written_to_while_the_next_one_is_written_waits_whole(|album| {
        fs::write(album.join("vocals.wav"), "vocals recorded on another computer").unwrap();
    });
}

/// A take moved from one Logic project into another in a Mirror job, and the single it goes
/// into records its vocals again and loses an old take. While the vocals are written next to
/// their target, the take changes in the source (its app still writes it). The single waits
/// whole, and so does the album, which removes the take's old copy, though nothing of the
/// album changed: every project is checked again before any of them takes its place, so the
/// album, carried first, never lands without that removal.
#[test]
fn mirror_leaves_a_library_whole_for_a_take_that_changed_while_the_next_one_was_written() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
    write(&src.join("Album.logicx/arrangement.txt"), "the album");
    write(&src.join("Album.logicx/take.wav"), "the take");
    write(&src.join("Single.logicx/old.wav"), "an old take of the single, long gone");
    write(&src.join("Single.logicx/vocals.wav"), "the vocals");
    fs::create_dir_all(&dst).unwrap();
    assert_eq!(mirror_on_its_own(&src, &dst, "", || {}).copied, 4);
    write(&src.join("Album.logicx/arrangement.txt"), "the album, without the take");
    fs::rename(src.join("Album.logicx/take.wav"), src.join("Single.logicx/take.wav")).unwrap();
    write(&src.join("Single.logicx/vocals.wav"), "the vocals, recorded again");
    fs::remove_file(src.join("Single.logicx/old.wav")).unwrap();
    let take = src.join("Single.logicx/take.wav");
    let outcome = mirror_on_its_own(&src, &dst, "Single.logicx/vocals", || {
        fs::OpenOptions::new().append(true).open(&take).unwrap().write_all(b", recorded again").unwrap();
    });
    assert_eq!(files_in(&dst.join("Album.logicx")), ["arrangement.txt", "take.wav"]);
    assert_eq!(read(&dst.join("Album.logicx/arrangement.txt")), "the album", "the album waits whole");
    assert_eq!(read(&dst.join("Album.logicx/take.wav")), "the take", "the old copy waits for the new one");
    assert_eq!(files_in(&dst.join("Single.logicx")), ["old.wav", "vocals.wav"]);
    assert_eq!(read(&dst.join("Single.logicx/vocals.wav")), "the vocals", "the single waits whole");
    assert_eq!((outcome.copied, outcome.deleted), (0, 0), "{outcome:?}");
}

/// A Photos library whose albums changed and whose photo was renamed, in a two-way job,
/// and the albums' new version can't take its place on the destination (the version it
/// replaces can't be kept there: .charon-trash is a file). Nothing of a library is removed
/// before all its new versions have landed: its version there stays as it was, with the
/// photo under its old name, the destination's only copy of it. Only the photo under its
/// new name lands, which that version doesn't use.
#[test]
fn two_way_leaves_a_library_as_it_was_when_a_new_version_cant_take_its_place() {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
    let library = |root: &Path| root.join("Lib.photoslibrary");
    write(&library(&src).join("Albums/data.bin"), "the albums");
    write(&library(&src).join("originals/IMG_1.jpg"), "a photo");
    let id = rig.engine.add_job(job_setup(&src, &dst, SyncMode::TwoWay)).expect("the job is added").job.id;
    confirm(&rig.engine, &id);
    write(&library(&src).join("Albums/data.bin"), "the albums, with a new one");
    fs::rename(library(&src).join("originals/IMG_1.jpg"), library(&src).join("originals/IMG_0001.jpg")).unwrap();
    let review = rig.engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.destination.replaced, review.tally.destination.removed), (1, 1), "{:?}", review.tally);
    let blocked = dst.clone();
    rig.at_first_step(move || block_the_trash(&blocked));
    let_progress_throttle_pass();
    rig.engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&rig.engine, &id);
    assert!(rig.hook_fired());
    assert_eq!(files_in(&library(&dst)), ["Albums/data.bin", "originals/IMG_0001.jpg", "originals/IMG_1.jpg"], "the library's version stays");
    assert_eq!(read(&library(&dst).join("Albums/data.bin")), "the albums");
    assert_eq!(read(&library(&dst).join("originals/IMG_1.jpg")), "a photo");
    let run = view(&rig.engine, &id).job.last_run.unwrap();
    assert!(run.failed > 0 && (run.copied, run.deleted) == (1, 0), "{}", run.describe());

    // With the trash a folder again, the next sync carries the whole library.
    fs::remove_file(dst.join(".charon-trash")).unwrap();
    confirm(&rig.engine, &id);
    assert_eq!(files_in(&library(&dst)), ["Albums/data.bin", "originals/IMG_0001.jpg"]);
    assert_eq!(read(&library(&dst).join("Albums/data.bin")), "the albums, with a new one");
}

/// A library with a new album, a renamed photo, and a database to replace with a version
/// too big to keep in .charon-trash on the destination's drive (the database there grew to
/// 8 TiB, all of it a hole that takes no room), in a Mirror job with safe deletes on.
/// Before any new version of a library takes its place, the trash must have room for the
/// versions they replace, one at a time: none replaces one, nothing is removed, and the run
/// says why. Only what the library adds lands (the album, the photo under its new name),
/// which its version there doesn't use. Where kept copies are clones (APFS), they take no
/// room, and the whole library lands. Never half of it.
#[test]
fn mirror_leaves_a_library_whole_when_the_trash_has_no_room_for_what_it_replaces() {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
    let library = |root: &Path| root.join("Lib.photoslibrary");
    write(&library(&src).join("database/Photos.sqlite"), "the database");
    write(&library(&src).join("originals/IMG_1.jpg"), "a photo");
    let mut setup = job_setup(&src, &dst, SyncMode::Mirror);
    setup.safe_delete = true;
    let id = rig.engine.add_job(setup).expect("the job is added").job.id;
    confirm(&rig.engine, &id);
    let huge = library(&dst).join("database/Photos.sqlite");
    if fs::OpenOptions::new().write(true).open(&huge).unwrap().set_len(1 << 43).is_err() {
        eprintln!("SKIPPED mirror_leaves_a_library_whole_when_the_trash_has_no_room_for_what_it_replaces: the temporary folder's drive can't hold a sparse 8 TiB file");
        return;
    }
    write(&library(&src).join("Albums/Trip.album"), "an album");
    fs::rename(library(&src).join("originals/IMG_1.jpg"), library(&src).join("originals/IMG_0001.jpg")).unwrap();
    confirm(&rig.engine, &id);
    let run = view(&rig.engine, &id).job.last_run.unwrap();
    if cfg!(target_os = "macos") && !library(&dst).join("originals/IMG_1.jpg").exists() {
        assert_eq!(files_in(&library(&dst)), ["Albums/Trip.album", "database/Photos.sqlite", "originals/IMG_0001.jpg"], "{}", run.describe());
        assert_eq!(read(&library(&dst).join("database/Photos.sqlite")), "the database");
        return;
    }
    assert_eq!(
        files_in(&library(&dst)),
        ["Albums/Trip.album", "database/Photos.sqlite", "originals/IMG_0001.jpg", "originals/IMG_1.jpg"],
        "the library's version stays: {}",
        run.describe()
    );
    assert_eq!(fs::metadata(&huge).unwrap().len(), 1 << 43);
    assert_eq!((run.copied, run.deleted, run.failed), (2, 0, 1), "{}", run.describe());
    let log = rig.engine.log(&id, 20);
    assert!(
        log.iter().any(|line| line.text.contains("to keep the replaced versions of “Lib.photoslibrary” in .charon-trash")),
        "the log says why: {:?}",
        log.iter().map(|line| &line.text).collect::<Vec<_>>()
    );
}

/// A database checkpointed in the source (its journal written into it, and removed), in a
/// two-way job, and its new version can't take its place on the destination (the version
/// it replaces can't be kept there: .charon-trash is a file). The old journal, renamed aside
/// before the new version would have met it, goes back next to its database: the
/// destination's database keeps the changes only its journal holds.
fn a_database_keeps_its_journal_when_its_new_version_cant_take_its_place(database: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
    let journal = format!("{database}-wal");
    let folder = Path::new(database).parent().unwrap();
    let names = |names: &[&str]| -> Vec<String> {
        names.iter().map(|name| format!("{}{name}", Path::new(database).file_name().unwrap().to_string_lossy())).collect()
    };
    write(&src.join(database), "the database");
    write(&src.join(&journal), "changes not in the database yet");
    let id = rig.engine.add_job(job_setup(&src, &dst, SyncMode::TwoWay)).expect("the job is added").job.id;
    confirm(&rig.engine, &id);
    write(&src.join(database), "the database, with the changes written into it");
    fs::remove_file(src.join(&journal)).unwrap();
    let review = rig.engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.destination.replaced, review.tally.destination.removed), (1, 1), "{:?}", review.tally);
    let blocked = dst.clone();
    rig.at_first_step(move || block_the_trash(&blocked));
    let_progress_throttle_pass();
    rig.engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&rig.engine, &id);
    assert!(rig.hook_fired());
    assert_eq!(read(&dst.join(database)), "the database");
    assert_eq!(read(&dst.join(&journal)), "changes not in the database yet", "the journal is back next to its database");
    assert_eq!(files_in(&dst.join(folder)), names(&["", "-wal"]), "nothing else is left there");
    let run = view(&rig.engine, &id).job.last_run.unwrap();
    assert!(run.failed > 0 && (run.copied, run.deleted) == (0, 0), "{}", run.describe());

    // With the trash a folder again, the next sync carries the new version, and the old
    // journal goes.
    fs::remove_file(dst.join(".charon-trash")).unwrap();
    confirm(&rig.engine, &id);
    assert_eq!(read(&dst.join(database)), "the database, with the changes written into it");
    assert_eq!(files_in(&dst.join(folder)), names(&[""]));
    assert_eq!(view(&rig.engine, &id).job.last_run.unwrap().deleted, 1);
}

#[test]
fn two_way_keeps_the_journal_of_a_database_whose_new_version_cant_take_its_place() {
    a_database_keeps_its_journal_when_its_new_version_cant_take_its_place("app/data.db");
}

#[test]
fn two_way_keeps_the_journal_of_a_library_database_whose_new_version_cant_take_its_place() {
    a_database_keeps_its_journal_when_its_new_version_cant_take_its_place("Lib.photoslibrary/database/Photos.sqlite");
}

/// The first sync of a Photos library while its app writes to the database's journal now
/// and then, once during each run. The destination has no version of the library yet that
/// a new one could be mixed with: its photos land as each is ready, and only the database,
/// which goes with its journal, waits for the next sync. The run says so truthfully: the
/// journal changed meanwhile, the database only waits for it.
fn a_busy_library_new_on_the_destination_lands_but_its_database(mode: SyncMode) {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
    let library = |root: &Path| root.join("Photos Library.photoslibrary");
    write(&library(&src).join("database/Photos.sqlite"), "the database");
    write(&library(&src).join("database/Photos.sqlite-wal"), "recent changes");
    for n in 1..=5 {
        write(&library(&src).join(format!("originals/IMG_{n}.jpg")), &format!("photo {n}"));
    }
    let id = rig.engine.add_job(job_setup(&src, &dst, mode)).expect("the job is added").job.id;
    let wal = library(&src).join("database/Photos.sqlite-wal");
    rig.at_first_step(move || fs::OpenOptions::new().append(true).open(&wal).unwrap().write_all(b", one more").unwrap());
    let_progress_throttle_pass();
    let review = rig.engine.prepare_review(&id).unwrap();
    rig.engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&rig.engine, &id);
    assert!(rig.hook_fired());
    for n in 1..=5 {
        assert_eq!(read(&library(&dst).join(format!("originals/IMG_{n}.jpg"))), format!("photo {n}"));
    }
    assert!(!library(&dst).join("database").join("Photos.sqlite").exists(), "the database waits for its journal");
    assert!(!library(&dst).join("database").join("Photos.sqlite-wal").exists());
    let run = view(&rig.engine, &id).job.last_run.unwrap();
    assert_eq!((run.copied, run.skipped, run.waited), (5, 1, 1), "{}", run.describe());
    assert!(
        run.describe().ends_with("1 item skipped (changed meanwhile), 1 item left for the next sync with the rest of its app library or database"),
        "{}",
        run.describe()
    );

    // The next sync, while the app is quiet, carries the database with its journal.
    confirm(&rig.engine, &id);
    assert_eq!(read(&library(&dst).join("database/Photos.sqlite")), "the database");
    assert_eq!(read(&library(&dst).join("database/Photos.sqlite-wal")), "recent changes, one more");
}

#[test]
fn two_way_carries_the_photos_of_a_busy_library_new_on_the_destination() {
    a_busy_library_new_on_the_destination_lands_but_its_database(SyncMode::TwoWay);
}

#[test]
fn mirror_carries_the_photos_of_a_busy_library_new_on_the_destination() {
    a_busy_library_new_on_the_destination_lands_but_its_database(SyncMode::Mirror);
}

/// The first backup of a Photos library is stopped at its fourth photo. The destination has
/// no version of the library its files could be mixed with, so each lands as it's ready: the
/// photos written before the stop stay, and the next sync goes on from there instead of
/// copying them all again.
#[test]
fn mirror_keeps_the_photos_of_a_new_library_that_landed_before_the_run_stopped() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
    let library = |root: &Path| root.join("Lib.photoslibrary");
    for n in 1..=5 {
        write(&library(&src).join(format!("originals/IMG_{n}.jpg")), &format!("photo {n}"));
    }
    write(&library(&src).join("database/Photos.sqlite"), "the database");
    fs::create_dir_all(&dst).unwrap();
    let filter = ExcludeFilter::none();
    let scan = |root: &Path| Scanner::new(&filter).scan(root, Volume::of(root)).expect("the folder is scanned");
    let (source, destination) = (scan(&src), scan(&dst));
    let plan = OneWayPlanner::mirror().plan(&source, &destination);
    let shores = Shores { source: src.clone(), destination: dst.clone() };
    let care = Care { safe_delete: false, keep_replaced: false, two_way: false };
    let cancel = AtomicBool::new(false);
    let observer = |progress: &Progress| {
        if progress.current.as_deref().is_some_and(|current| current.ends_with("IMG_4.jpg")) {
            cancel.store(true, Ordering::SeqCst);
        }
    };
    let mut meter = ProgressMeter::for_plan(&plan, &observer);
    let outcome = Executor::new(&shores, &source, &destination, care, &cancel).run(&plan, &mut meter);
    assert!(outcome.cancelled);
    assert_eq!(outcome.copied, 4, "the database and the first three photos: {outcome:?}");
    assert_eq!(read(&library(&dst).join("database/Photos.sqlite")), "the database");
    for n in 1..=3 {
        assert_eq!(read(&library(&dst).join(format!("originals/IMG_{n}.jpg"))), format!("photo {n}"), "written before the stop");
    }
}

/// A Photos library already in the destination gets five new photos (an import), and its
/// database and journal change with them. Its app writes to the journal now and then, once
/// during each run, so the library can't be carried whole. The new photos only add to the
/// library's version there, which doesn't use them: they land in the first run, while the
/// database waits for its journal. Once the app is quiet, the next sync carries the rest.
fn the_new_photos_of_a_busy_library_land_while_the_rest_of_it_waits(mode: SyncMode) {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
    let library = |root: &Path| root.join("Photos Library.photoslibrary");
    write(&library(&src).join("database/Photos.sqlite"), "the database");
    write(&library(&src).join("database/Photos.sqlite-wal"), "recent changes");
    write(&library(&src).join("originals/IMG_1.jpg"), "photo 1");
    let id = rig.engine.add_job(job_setup(&src, &dst, mode)).expect("the job is added").job.id;
    confirm(&rig.engine, &id);
    assert_eq!(read(&library(&dst).join("originals/IMG_1.jpg")), "photo 1");
    for n in 2..=6 {
        write(&library(&src).join(format!("originals/IMG_{n}.jpg")), &format!("photo {n}"));
    }
    write(&library(&src).join("database/Photos.sqlite"), "the database, with five more photos");
    write(&library(&src).join("database/Photos.sqlite-wal"), "recent changes, the import");
    let wal = library(&src).join("database/Photos.sqlite-wal");
    rig.at_first_step(move || fs::OpenOptions::new().append(true).open(&wal).unwrap().write_all(b", one more").unwrap());
    let_progress_throttle_pass();
    let review = rig.engine.prepare_review(&id).unwrap();
    rig.engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&rig.engine, &id);
    assert!(rig.hook_fired());
    for n in 2..=6 {
        assert_eq!(read(&library(&dst).join(format!("originals/IMG_{n}.jpg"))), format!("photo {n}"));
    }
    assert_eq!(read(&library(&dst).join("database/Photos.sqlite")), "the database", "the database waits for its journal");
    assert_eq!(read(&library(&dst).join("database/Photos.sqlite-wal")), "recent changes");
    let run = view(&rig.engine, &id).job.last_run.unwrap();
    assert_eq!((run.copied, run.skipped, run.waited), (5, 1, 1), "{}", run.describe());

    // The next sync, while the app is quiet, carries the database with its journal.
    confirm(&rig.engine, &id);
    assert_eq!(read(&library(&dst).join("database/Photos.sqlite")), "the database, with five more photos");
    assert_eq!(read(&library(&dst).join("database/Photos.sqlite-wal")), "recent changes, the import, one more");
}

#[test]
fn two_way_carries_the_new_photos_of_a_busy_library_already_there() {
    the_new_photos_of_a_busy_library_land_while_the_rest_of_it_waits(SyncMode::TwoWay);
}

#[test]
fn mirror_carries_the_new_photos_of_a_busy_library_already_there() {
    the_new_photos_of_a_busy_library_land_while_the_rest_of_it_waits(SyncMode::Mirror);
}

/// A Photos library already in the destination whose albums now list a new photo, and the
/// photo is still being written while the run goes on (its import isn't done). A library's
/// new version never lands without what it adds: the albums, which would list a photo the
/// destination doesn't have, wait for it, and the library there stays one its app opens.
/// The next sync carries both.
fn a_library_waits_for_a_new_photo_that_changes_during_the_run(mode: SyncMode) {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
    let library = |root: &Path| root.join("Lib.photoslibrary");
    write(&library(&src).join("Albums/data.bin"), "the albums");
    write(&library(&src).join("originals/IMG_1.jpg"), "photo 1");
    let id = rig.engine.add_job(job_setup(&src, &dst, mode)).expect("the job is added").job.id;
    confirm(&rig.engine, &id);
    write(&library(&src).join("Albums/data.bin"), "the albums, with photo 2");
    write(&library(&src).join("originals/IMG_2.jpg"), "photo 2");
    let photo = library(&src).join("originals/IMG_2.jpg");
    rig.at_first_step(move || fs::OpenOptions::new().append(true).open(&photo).unwrap().write_all(b", the rest of it").unwrap());
    let_progress_throttle_pass();
    let review = rig.engine.prepare_review(&id).unwrap();
    rig.engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&rig.engine, &id);
    assert!(rig.hook_fired());
    assert_eq!(read(&library(&dst).join("Albums/data.bin")), "the albums", "the albums wait for their photo");
    assert_eq!(files_in(&library(&dst)), ["Albums/data.bin", "originals/IMG_1.jpg"]);
    let run = view(&rig.engine, &id).job.last_run.unwrap();
    assert_eq!((run.copied, run.skipped, run.waited), (0, 1, 1), "{}", run.describe());

    confirm(&rig.engine, &id);
    assert_eq!(read(&library(&dst).join("Albums/data.bin")), "the albums, with photo 2");
    assert_eq!(read(&library(&dst).join("originals/IMG_2.jpg")), "photo 2, the rest of it");
}

#[test]
fn two_way_waits_with_a_library_for_a_new_photo_that_changes_during_the_run() {
    a_library_waits_for_a_new_photo_that_changes_during_the_run(SyncMode::TwoWay);
}

#[test]
fn mirror_waits_with_a_library_for_a_new_photo_that_changes_during_the_run() {
    a_library_waits_for_a_new_photo_that_changes_during_the_run(SyncMode::Mirror);
}

/// A database whose journal its app writes to during the run, and a stale copy of the
/// database in another folder, removed in the source. The journal changed meanwhile, and the
/// database, which itself didn't, only waits for it. The stale copy may be the database's old
/// copy, so its removal waits too, and says why truthfully: the database waits with the rest
/// of its files, it didn't change.
fn a_removal_held_for_a_database_that_only_waits_says_so(mode: SyncMode) {
    let tmp = tempfile::tempdir().unwrap();
    let rig = Rig::open(&tmp.path().join("data"));
    let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
    write(&src.join("app/data.db"), "the database");
    write(&src.join("app/data.db-wal"), "recent changes");
    write(&src.join("old/data.db"), "a stale copy of the database");
    let id = rig.engine.add_job(job_setup(&src, &dst, mode)).expect("the job is added").job.id;
    confirm(&rig.engine, &id);
    write(&src.join("app/data.db"), "the database, checkpointed");
    write(&src.join("app/data.db-wal"), "newer changes");
    fs::remove_file(src.join("old/data.db")).unwrap();
    let wal = src.join("app/data.db-wal");
    rig.at_first_step(move || fs::OpenOptions::new().append(true).open(&wal).unwrap().write_all(b", one more").unwrap());
    let_progress_throttle_pass();
    let review = rig.engine.prepare_review(&id).unwrap();
    rig.engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&rig.engine, &id);
    assert!(rig.hook_fired());
    assert_eq!(read(&dst.join("app/data.db")), "the database", "the database waits for its journal");
    assert_eq!(read(&dst.join("old/data.db")), "a stale copy of the database", "the stale copy waits for the database");
    let log: Vec<String> = rig.engine.log(&id, 20).into_iter().map(|line| line.text).collect();
    let line = |path: &str| log.iter().find(|line| line.starts_with(&format!("{path}: "))).unwrap_or_else(|| panic!("no line for {path}: {log:?}")).clone();
    assert_eq!(line("app/data.db-wal"), "app/data.db-wal: changed on the source side since the scan");
    assert_eq!(
        line("app/data.db"),
        "app/data.db: left for the next sync: another file of its database couldn't be synced, and a database's files only go together"
    );
    assert_eq!(
        line("old/data.db"),
        "old/data.db: kept for now: it may be the old copy of “app/data.db”, which waits for the next sync with the rest of its app library or database"
    );
}

#[test]
fn two_way_says_a_removal_waits_for_a_database_that_only_waits() {
    a_removal_held_for_a_database_that_only_waits_says_so(SyncMode::TwoWay);
}

#[test]
fn mirror_says_a_removal_waits_for_a_database_that_only_waits() {
    a_removal_held_for_a_database_that_only_waits_says_so(SyncMode::Mirror);
}
