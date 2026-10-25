//! End-to-end: items deep down in a folder whose paths come near the drive's limit (4,096
//! bytes on Linux), where Charon's own names need more room than the items themselves: the
//! temporary name a copy is written under, the path an item takes in the trash, and the
//! name of a conflict copy.

#![cfg(target_os = "linux")]

use charon_core::model::SyncMode;
use charon_core::plan::NoteKind;
use charon_core::{ConflictRule, Engine, EngineEvent, JobSetup, Trigger};
use std::fs;
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

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

/// A folder whose own path is `length` bytes long, inside `parent`.
fn deep_folder(parent: &Path, length: usize) -> PathBuf {
    let mut folder = parent.join("dst");
    while folder.as_os_str().len() + 201 < length {
        folder.push("d".repeat(200));
    }
    let rest = length - folder.as_os_str().len() - 1;
    folder.push("e".repeat(rest));
    fs::create_dir_all(&folder).unwrap();
    folder
}

/// Every file under `root`.
fn files_in(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(items) = fs::read_dir(root) {
        for item in items.flatten() {
            let path = item.path();
            if fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
                found.extend(files_in(&path));
            } else {
                found.push(path);
            }
        }
    }
    found
}

/// The names in a folder.
fn names_in(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(folder).unwrap().flatten().map(|item| item.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

fn last_run(engine: &Engine, id: &String) -> charon_core::model::RunSummary {
    engine.job(id).unwrap().job.last_run.unwrap()
}

#[test]
fn a_short_name_deep_down_is_copied_though_a_temporary_name_next_to_it_would_be_too_long() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, data) = (tmp.path().join("a"), tmp.path().join("data"));
    let b = deep_folder(tmp.path(), 3900);
    let folder = "s".repeat(170);
    let item = format!("{folder}/f.txt");
    // The file's own path fits; a temporary name (29 bytes) next to it wouldn't.
    assert!(b.join(&item).as_os_str().len() < 4095 && b.join(&folder).as_os_str().len() + 30 > 4095);
    write(&a.join(&item), "deep down");
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::Backup, ConflictRule::Ask)).unwrap().job.id;

    let review = confirm(&engine, &id);
    assert!(review.steps.iter().any(|step| step.path == item), "{:?}", review.steps);
    let run = last_run(&engine, &id);
    assert_eq!((run.copied, run.left_out, run.failed), (1, 0, 0), "{run:?}");
    assert_eq!(read(&b.join(&item)), "deep down");
    assert_eq!(names_in(&b.join(&folder)), vec!["f.txt"]);
    let next = engine.prepare_review(&id).unwrap();
    assert_eq!((next.total_steps, next.total_notes), (0, 0), "in sync, nothing left out: {:?}", next.notes);
}

#[test]
fn files_deep_down_are_replaced_and_removed_with_their_old_versions_kept_in_the_trash() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, data) = (tmp.path().join("a"), tmp.path().join("data"));
    let b = deep_folder(tmp.path(), 3900);
    // In the trash, these paths would be 42 bytes longer: too long for the drive.
    let folder = "s".repeat(170);
    let (edited, removed) = (format!("{folder}/f.txt"), format!("{folder}/g.txt"));
    write(&a.join(&edited), "version one");
    write(&a.join(&removed), "to be removed");
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);
    assert_eq!(read(&b.join(&edited)), "version one");

    write(&a.join(&edited), "version two, longer");
    fs::remove_file(a.join(&removed)).unwrap();
    confirm(&engine, &id);
    let run = last_run(&engine, &id);
    assert_eq!((run.copied, run.deleted, run.failed, run.left_out), (1, 1, 0, 0), "{run:?}");
    assert_eq!(read(&b.join(&edited)), "version two, longer");
    assert!(!b.join(&removed).exists());
    assert_eq!(names_in(&b.join(&folder)), vec!["f.txt"], "no temporary copy is left behind");
    let kept: Vec<String> = files_in(&b.join(".charon-trash")).iter().filter_map(|path| fs::read_to_string(path).ok()).collect();
    assert!(kept.contains(&"version one".to_string()), "the replaced version is in the trash: {kept:?}");
    assert!(kept.contains(&"to be removed".to_string()), "the removed file is in the trash: {kept:?}");
}

#[test]
fn a_conflict_copy_deep_down_gets_a_name_short_enough_to_fit() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, data) = (tmp.path().join("a"), tmp.path().join("data"));
    let b = deep_folder(tmp.path(), 3900);
    let folder = "s".repeat(110);
    let item = format!("{folder}/{}.txt", "x".repeat(60));
    write(&a.join(&item), "both had this");
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::KeepBoth)).unwrap().job.id;
    confirm(&engine, &id);

    // Both sides change the file. "<name> (conflict 2026-10-20 09.00).txt" next to it is 9
    // bytes too long for the destination's drive.
    write(&a.join(&item), "the source's version");
    write(&b.join(&item), "the destination's");
    confirm(&engine, &id);
    let run = last_run(&engine, &id);
    assert_eq!((run.failed, run.left_out), (0, 0), "{run:?}");
    for (side, root) in [("source", &a), ("destination", &b)] {
        let names = names_in(&root.join(&folder));
        assert_eq!(names.len(), 2, "both versions on the {side}: {names:?}");
        let copy = names.iter().find(|name| name.contains("(conflict ")).expect("a conflict copy");
        assert!(copy.starts_with(&"x".repeat(40)) && copy.ends_with(").txt"), "{copy}");
        assert!(root.join(&folder).join(copy).as_os_str().len() < 4096);
        assert_eq!(read(&root.join(&folder).join(copy)), "the destination's");
        assert_eq!(read(&root.join(&item)), "the source's version");
    }
}

#[test]
fn where_no_conflict_copy_fits_both_versions_stay_and_later_syncs_go_on() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, data) = (tmp.path().join("a"), tmp.path().join("data"));
    let b = deep_folder(tmp.path(), 3900);
    // Not even "r (conflict 2026-10-20 09.00).txt" fits next to this file.
    let item = format!("{}/report.txt", "s".repeat(170));
    write(&a.join(&item), "both had this");
    write(&a.join("other.txt"), "o");
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::KeepBoth)).unwrap().job.id;
    confirm(&engine, &id);

    write(&a.join(&item), "the source's version");
    write(&b.join(&item), "the destination's");
    confirm(&engine, &id);
    let run = last_run(&engine, &id);
    assert_eq!((run.failed, run.left_out), (0, 1), "{run:?}");
    assert_eq!(read(&a.join(&item)), "the source's version");
    assert_eq!(read(&b.join(&item)), "the destination's");

    // The next list leaves the file as it is, says why, and goes on with everything else.
    fs::remove_file(a.join("other.txt")).unwrap();
    let next = engine.prepare_review(&id).unwrap();
    assert!(next.steps.iter().all(|step| step.path != item), "{:?}", next.steps);
    let note = next.notes.iter().find(|note| note.path == item).expect("the list names it");
    assert_eq!(note.kind, NoteKind::LeftOut);
    assert!(note.reason.contains("an earlier sync found") && note.reason.contains("conflict copy"), "{}", note.reason);
    assert_eq!(next.tally.destination.removed, 1);
    engine.approve(&id, &next.review_id, true).unwrap();
    wait(&engine, &id);
    assert!(!b.join("other.txt").exists(), "a removal elsewhere isn't held back");
    assert_eq!(read(&a.join(&item)), "the source's version");
    assert_eq!(read(&b.join(&item)), "the destination's");

    // Once the person renames one version, nothing stands in the other's way any more.
    let renamed = format!("{}/old.txt", "s".repeat(170));
    fs::rename(b.join(&item), b.join(&renamed)).unwrap();
    let resolved = engine.prepare_review(&id).unwrap();
    assert!(resolved.steps.iter().any(|step| step.path == item), "{:?}", resolved.steps);
    assert!(resolved.notes.iter().all(|note| note.path != item), "{:?}", resolved.notes);
}
