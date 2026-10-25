//! Full automation and damage that spreads slowly: the long count of what automatic runs
//! removed or replaced since the last confirmed list, and the trash only the person empties.

use charon_core::model::{Level, SyncMode};
use charon_core::store::Unattended;
use charon_core::{ConflictRule, Engine, EngineEvent, JobSetup, SyncStart, Trigger};
use chrono::Utc;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn engine(data: &Path) -> Engine {
    Engine::open(data, Arc::new(|_: EngineEvent| {})).unwrap()
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn wait(engine: &Engine, id: &String) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while engine.is_running(id) {
        assert!(Instant::now() < deadline, "sync took too long");
        thread::sleep(Duration::from_millis(10));
    }
}

fn confirm(engine: &Engine, id: &String) -> charon_core::model::Review {
    let review = engine.prepare_review(id).unwrap();
    engine.approve(id, &review.review_id, true).unwrap();
    wait(engine, id);
    review
}

/// A two-way job with full automation between `a` and `b`, holding `files` files, after
/// its first sync was confirmed.
fn automatic_job(tmp: &Path, files: usize) -> (Engine, String, PathBuf, PathBuf, PathBuf) {
    let (a, b, data) = (tmp.join("a"), tmp.join("b"), tmp.join("data"));
    for i in 0..files {
        write(&a.join(format!("f{i}.txt")), "original");
    }
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let setup = JobSetup {
        name: String::new(),
        source: a.clone(),
        destination: b.clone(),
        mode: SyncMode::TwoWay,
        conflict: ConflictRule::Ask,
        trigger: Trigger::Manual,
        excludes: JobSetup::default_excludes(),
        safe_delete: true,
        full_automation: true,
    };
    let id = engine.add_job(setup).unwrap().job.id;
    confirm(&engine, &id);
    assert!(engine.job(&id).unwrap().runs_by_itself);
    (engine, id, a, b, data)
}

/// Starts an automatic run and returns why it was held, if it was.
fn run_by_itself(engine: &Engine, id: &String) -> Option<String> {
    assert_eq!(engine.sync_now(id).unwrap(), SyncStart::Started);
    wait(engine, id);
    engine.job(id).unwrap().job.held
}

/// The dated folders in `root`'s trash.
fn trash_folders(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root.join(".charon-trash"))
        .map(|items| items.flatten().map(|item| item.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    names.sort();
    names
}

#[test]
fn runs_nobody_looked_at_never_empty_the_trash() {
    let tmp = tempfile::tempdir().unwrap();
    let (engine, id, a, b, _) = automatic_job(tmp.path(), 4);

    // An automatic run replaces f0.txt: its original goes into the destination's trash.
    write(&a.join("f0.txt"), "damaged");
    assert_eq!(run_by_itself(&engine, &id), None);
    let folders = trash_folders(&b);
    assert_eq!(folders.len(), 1);
    // That was 31 days ago: old enough to be emptied after 30 days.
    let old_stamp = (chrono::Local::now() - chrono::Duration::days(31)).format("%Y-%m-%d_%H-%M-%S").to_string();
    let old = format!("{old_stamp}{}", &folders[0][19..]);
    fs::rename(b.join(".charon-trash").join(&folders[0]), b.join(".charon-trash").join(&old)).unwrap();

    // More automatic runs: the old trash stays, with the only good copy of f0.txt in it.
    write(&a.join("new.txt"), "new");
    assert_eq!(run_by_itself(&engine, &id), None);
    assert!(b.join("new.txt").exists());
    assert!(trash_folders(&b).contains(&old), "an automatic run emptied the trash");

    // A list the person confirms says it empties the old trash, and does.
    write(&a.join("newer.txt"), "newer");
    let review = confirm(&engine, &id);
    assert!(review.housekeeping.iter().any(|line| line.contains(".charon-trash on the destination")), "{:?}", review.housekeeping);
    assert!(!trash_folders(&b).contains(&old));
}

#[test]
fn damage_spreading_over_days_is_held_while_the_originals_are_still_in_the_trash() {
    let tmp = tempfile::tempdir().unwrap();
    let (engine, id, a, b, data) = automatic_job(tmp.path(), 100);
    let count = Unattended::new(data.join("unattended"));

    // One file replaced now and then is ordinary, and counted.
    write(&a.join("f99.txt"), "edited");
    assert_eq!(run_by_itself(&engine, &id), None);
    assert_eq!(fs::read_to_string(b.join("f99.txt")).unwrap(), "edited");
    assert!(count.load(&id, 30, Utc::now()).destination.keys.contains("f99.txt"));

    // Over the past days automatic runs replaced 30 of the 100 files, each day too few for
    // the day's count to notice.
    let earlier: Vec<String> = (0..30).map(|i| format!("f{i}.txt")).collect();
    count.note(&id, Utc::now() - chrono::Duration::days(3), &[], &earlier, 30).unwrap();

    // The next replacement is held, and the destination keeps its copy.
    write(&a.join("f50.txt"), "damaged");
    let held = run_by_itself(&engine, &id).expect("the run went ahead");
    assert!(held.contains("since you last confirmed a list"), "{held}");
    assert_eq!(fs::read_to_string(b.join("f50.txt")).unwrap(), "original");

    // The person looks, ticks and confirms: the count starts again from there.
    let review = engine.prepare_review(&id).unwrap();
    assert!(review.needs_tick);
    assert!(review.warnings.iter().any(|w| w.level == Level::Danger && w.text.contains("since you last confirmed a list")));
    engine.approve(&id, &review.review_id, true).unwrap();
    wait(&engine, &id);
    assert_eq!(fs::read_to_string(b.join("f50.txt")).unwrap(), "damaged");
    assert!(count.load(&id, 30, Utc::now()).destination.keys.is_empty());
    write(&a.join("f60.txt"), "edited");
    assert_eq!(run_by_itself(&engine, &id), None);
}

#[test]
fn a_count_that_cant_be_read_holds_what_would_remove_or_replace() {
    let tmp = tempfile::tempdir().unwrap();
    let (engine, id, a, b, data) = automatic_job(tmp.path(), 10);
    write(&data.join("unattended").join(format!("{id}.json")), "{ damaged");

    // Adding a file harms nothing, so it goes ahead.
    write(&a.join("new.txt"), "new");
    assert_eq!(run_by_itself(&engine, &id), None);
    assert!(b.join("new.txt").exists());

    // Replacing one is held: nobody can tell what automatic runs did before.
    write(&a.join("f1.txt"), "edited");
    assert!(run_by_itself(&engine, &id).is_some());
    assert_eq!(fs::read_to_string(b.join("f1.txt")).unwrap(), "original");

    // A confirmed list starts a fresh count.
    let review = engine.prepare_review(&id).unwrap();
    assert!(review.warnings.iter().any(|w| w.text.contains("couldn't read its count")));
    engine.approve(&id, &review.review_id, true).unwrap();
    wait(&engine, &id);
    assert!(!Unattended::new(data.join("unattended")).load(&id, 30, Utc::now()).lost);
    write(&a.join("f2.txt"), "edited");
    assert_eq!(run_by_itself(&engine, &id), None);
}
