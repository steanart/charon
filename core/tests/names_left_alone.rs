//! End-to-end: names Charon leaves alone stay alone, on both sides, with real folders on
//! disk, real looks, lists and runs through the engine.
//!
//! * An exclude pattern leaves out an item under every spelling a drive that ignores upper
//!   and lower case takes for the same name, for every letter, not only a to z: Charon never
//!   writes into a folder the person excluded, and never asks to remove what it put there.
//! * A folder of the person's named like Charon's trash or marker folder in other upper and
//!   lower case is never synced: where a drive ignores case it is Charon's folder, and what
//!   Charon puts into it must never come back to the other side.
//! * An item the scan leaves out (a pipe, a socket, a device) keeps the other side's item of
//!   the same name out of the sync, instead of a copy that fails on every sync and holds
//!   back every removal on that side.
//!
//! The tests about upper and lower case need a folder on a drive that ignores case and
//! folds every letter: the temporary folder does on macOS and Windows. On Linux, set
//! CHARON_CASE_INSENSITIVE_FOLDER to a folder on such a drive (a case-folding ext4 folder,
//! or a FUSE file system); without one they skip, and say so on stderr. With
//! CHARON_REQUIRE_CASE_TESTS=1 they fail instead of skipping.

use charon_core::model::{StepFilter, SyncMode};
#[cfg(unix)]
use charon_core::plan::{NoteKind, Side};
use charon_core::{ConflictRule, Engine, EngineEvent, JobSetup, Trigger};
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn engine(data: &Path) -> Engine {
    Engine::open(data, Arc::new(|_: EngineEvent| {})).unwrap()
}

fn two_way(src: &Path, dst: &Path, excludes: &[&str]) -> JobSetup {
    let mut patterns = JobSetup::default_excludes();
    patterns.extend(excludes.iter().map(|pattern| pattern.to_string()));
    JobSetup {
        name: String::new(),
        source: src.to_path_buf(),
        destination: dst.to_path_buf(),
        mode: SyncMode::TwoWay,
        conflict: ConflictRule::KeepBoth,
        trigger: Trigger::Manual,
        excludes: patterns,
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
/// Returns the paths of the steps the list showed.
fn confirm(engine: &Engine, id: &String) -> Vec<String> {
    let review = engine.prepare_review(id).unwrap();
    let steps = steps_of(engine, id, &review.review_id);
    engine.approve(id, &review.review_id, true).unwrap();
    wait(engine, id);
    steps
}

/// Every step of a list, as "kind side path".
fn steps_of(engine: &Engine, id: &String, review_id: &str) -> Vec<String> {
    let page = engine.review_steps(id, review_id, StepFilter::All, "", 0, 1000).unwrap();
    page.steps.iter().map(|step| format!("{:?} {:?} {}", step.kind, step.side, step.path)).collect()
}

/// The steps a fresh list shows, without confirming it.
fn listed(engine: &Engine, id: &String) -> Vec<String> {
    let review = engine.prepare_review(id).unwrap();
    steps_of(engine, id, &review.review_id)
}

/// Writes a file saved an hour ago: on a drive that shows changes late (a FUSE drive), a
/// file saved a moment ago waits for the next sync.
fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    let an_hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(an_hour_ago)).unwrap();
}

/// The names in a folder, sorted, without Charon's own.
fn names_in(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(folder)
        .unwrap()
        .flatten()
        .map(|item| item.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with(".charon"))
        .collect();
    names.sort();
    names
}

/// A new empty folder on a drive that ignores upper and lower case for every letter, or
/// `None` when there is none to use here (said on stderr, or a failure when
/// CHARON_REQUIRE_CASE_TESTS=1).
fn case_insensitive_folder(test: &str) -> Option<tempfile::TempDir> {
    let given = std::env::var_os("CHARON_CASE_INSENSITIVE_FOLDER");
    let folder = match &given {
        Some(place) => tempfile::tempdir_in(place).unwrap(),
        None => tempfile::tempdir().unwrap(),
    };
    if ignores_case(folder.path()) {
        return Some(folder);
    }
    let why = format!("{} tells “Ü” from “ü”, and the test needs a drive that doesn't", folder.path().display());
    let how = "set CHARON_CASE_INSENSITIVE_FOLDER to a folder on a drive that ignores upper and lower case";
    if given.is_some() || std::env::var("CHARON_REQUIRE_CASE_TESTS").is_ok_and(|v| v == "1") {
        panic!("{test} can't run: {why}. To run it, {how}.");
    }
    // Straight to stderr: the test harness swallows the output of a test that passes, and a
    // skipped safety test must never look like one that ran.
    let _ = writeln!(std::io::stderr(), "SKIPPED {test}: {why}. It did NOT run. To run it, {how}.");
    None
}

/// Whether the drive of `folder` takes "PRÜFUNG" for "Prüfung".
fn ignores_case(folder: &Path) -> bool {
    let probe = folder.join("Pr\u{fc}fung");
    fs::create_dir(&probe).unwrap();
    let ignores = folder.join("PR\u{dc}FUNG").is_dir();
    fs::remove_dir(&probe).unwrap();
    ignores
}

/// The source spells the folder "ENTWÜRFE"; the destination, on a drive that ignores case,
/// has the folder "Entwürfe" the person excluded. Patterns once ignored case only for a to
/// z: the source's folder was synced, its file written into the excluded folder, and its
/// removal from the source asked for on every list after that, and carried out once the
/// stray copy was deleted.
#[test]
fn an_excluded_folder_is_never_written_into_under_another_spelling() {
    let Some(ci) = case_insensitive_folder("an_excluded_folder_is_never_written_into_under_another_spelling") else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), ci.path().join("dst"), tmp.path().join("data"));
    write(&src.join("ENTW\u{dc}RFE/plan.txt"), "the only copy of my plan");
    write(&src.join("other.txt"), "o");
    write(&dst.join("Entw\u{fc}rfe/private.txt"), "private, excluded");
    write(&dst.join("more.txt"), "m");
    let engine = engine(&data);
    let id = engine.add_job(two_way(&src, &dst, &["Entw\u{fc}rfe"])).unwrap().job.id;

    let steps = confirm(&engine, &id);
    assert!(steps.iter().all(|step| !step.to_lowercase().contains("entw")), "{steps:?}");
    assert_eq!(names_in(&dst.join("Entw\u{fc}rfe")), vec!["private.txt"], "nothing was written into the excluded folder");
    assert!(dst.join("other.txt").is_file() && src.join("more.txt").is_file(), "everything else is synced");
    assert_eq!(listed(&engine, &id), Vec::<String>::new(), "the folders are in sync");

    // The person tidies up their excluded folder: the source's file stays where it is.
    fs::remove_file(dst.join("Entw\u{fc}rfe/private.txt")).unwrap();
    assert_eq!(listed(&engine, &id), Vec::<String>::new());
    assert_eq!(fs::read_to_string(src.join("ENTW\u{dc}RFE/plan.txt")).unwrap(), "the only copy of my plan");
}

/// The pattern "Résumé*" leaves out "RÉSUMÉ.pdf" too, where a drive takes it for the
/// excluded "Résumé.pdf". Once, its copy was planned on every list and skipped on every
/// run, because the drive found the excluded file at its place.
#[test]
fn a_pattern_leaves_out_a_name_in_any_case_of_its_letters() {
    let Some(ci) = case_insensitive_folder("a_pattern_leaves_out_a_name_in_any_case_of_its_letters") else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), ci.path().join("dst"), tmp.path().join("data"));
    write(&src.join("R\u{c9}SUM\u{c9}.pdf"), "the laptop's version");
    write(&src.join("letter.txt"), "l");
    write(&dst.join("R\u{e9}sum\u{e9}.pdf"), "private, excluded");
    let engine = engine(&data);
    let id = engine.add_job(two_way(&src, &dst, &["R\u{e9}sum\u{e9}*"])).unwrap().job.id;

    assert_eq!(confirm(&engine, &id), vec!["New Some(Destination) letter.txt"]);
    assert_eq!(names_in(&dst), vec!["R\u{e9}sum\u{e9}.pdf", "letter.txt"]);
    assert_eq!(fs::read_to_string(dst.join("R\u{e9}sum\u{e9}.pdf")).unwrap(), "private, excluded");
    assert_eq!(fs::read_to_string(src.join("R\u{c9}SUM\u{c9}.pdf")).unwrap(), "the laptop's version");
    assert_eq!(listed(&engine, &id), Vec::<String>::new(), "nothing comes back on the next list");
}

/// Both sides have an excluded folder "Entwürfe", each with its own plan. The person
/// renames the source's folder to "ENTWÜRFE", changing only the case. Patterns once
/// missed the new spelling: its file was listed on every list and skipped on every run,
/// and the job waited for an OK forever.
#[test]
fn a_case_only_rename_of_an_excluded_folder_settles() {
    let Some(ci) = case_insensitive_folder("a_case_only_rename_of_an_excluded_folder_settles") else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), ci.path().join("dst"), tmp.path().join("data"));
    write(&src.join("Entw\u{fc}rfe/plan.txt"), "the source's plan");
    write(&src.join("notes.txt"), "n");
    write(&dst.join("Entw\u{fc}rfe/plan.txt"), "the destination's plan");
    let engine = engine(&data);
    let id = engine.add_job(two_way(&src, &dst, &["Entw\u{fc}rfe"])).unwrap().job.id;
    assert_eq!(confirm(&engine, &id), vec!["New Some(Destination) notes.txt"]);

    fs::rename(src.join("Entw\u{fc}rfe"), src.join("ENTW\u{dc}RFE")).unwrap();
    for round in 0..3 {
        assert_eq!(listed(&engine, &id), Vec::<String>::new(), "round {round}");
    }
    assert_eq!(fs::read_to_string(src.join("ENTW\u{dc}RFE/plan.txt")).unwrap(), "the source's plan");
    assert_eq!(fs::read_to_string(dst.join("Entw\u{fc}rfe/plan.txt")).unwrap(), "the destination's plan");
}

/// The names in a folder that a drive ignoring case takes for one of Charon's own folders,
/// as they are written, sorted.
fn charon_like_names_in(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(folder)
        .unwrap()
        .flatten()
        .map(|item| item.file_name().to_string_lossy().into_owned())
        .filter(|name| name.to_lowercase().starts_with(".charon"))
        .collect();
    names.sort();
    names
}

/// The notes of a list, as "kind side path", sorted.
fn notes_of(review: &charon_core::model::Review) -> Vec<String> {
    let mut notes: Vec<String> = review.notes.iter().map(|note| format!("{:?} {:?} {}", note.kind, note.side, note.path)).collect();
    notes.sort();
    notes
}

/// The source holds the person's own folders ".CHARON-TRASH" and ".Charon"; the destination,
/// on a drive that ignores case, has no trash yet. Synced there, they became Charon's trash
/// and marker folder on the first sync: what a later sync removed or replaced went into the
/// person's folder, and came back to the source on the sync after that.
#[test]
fn folders_named_like_charons_own_in_other_case_never_become_charons_on_the_other_drive() {
    let Some(ci) = case_insensitive_folder("folders_named_like_charons_own_in_other_case_never_become_charons_on_the_other_drive") else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), ci.path().join("dst"), tmp.path().join("data"));
    write(&src.join(".CHARON-TRASH/notes.txt"), "my notes");
    write(&src.join(".Charon/settings.txt"), "my settings");
    write(&src.join("a.txt"), "first");
    write(&src.join("gone.txt"), "g");
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(two_way(&src, &dst, &[])).unwrap().job.id;

    let review = engine.prepare_review(&id).unwrap();
    let mut steps = steps_of(&engine, &id, &review.review_id);
    steps.sort();
    assert_eq!(steps, vec!["New Some(Destination) a.txt", "New Some(Destination) gone.txt"]);
    assert_eq!(notes_of(&review), vec!["LeftOut Some(Source) .CHARON-TRASH", "LeftOut Some(Source) .Charon"]);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait(&engine, &id);
    assert_eq!(charon_like_names_in(&dst), vec![".charon"], "only Charon's own marker folder");

    // A replacement and a removal on the destination open Charon's trash there.
    write(&src.join("a.txt"), "second");
    fs::remove_file(src.join("gone.txt")).unwrap();
    let mut steps = confirm(&engine, &id);
    steps.sort();
    assert_eq!(steps, vec!["Remove Some(Destination) gone.txt", "Replace Some(Destination) a.txt"]);
    assert_eq!(charon_like_names_in(&dst), vec![".charon", ".charon-trash"], "Charon's own trash, in a folder of its own");

    assert_eq!(listed(&engine, &id), Vec::<String>::new(), "nothing comes back from the trash");
    assert!(!src.join("gone.txt").exists());
    assert_eq!(names_in(&src.join(".CHARON-TRASH")), vec!["notes.txt"], "the person's folder is as it was");
    assert_eq!(fs::read_to_string(src.join(".CHARON-TRASH/notes.txt")).unwrap(), "my notes");
}

/// The person made a folder ".Charon-Trash" on a drive that ignores case. Charon's trash
/// there is that folder, so it's never synced: what Charon puts into it never comes back
/// to the other side.
#[test]
fn a_folder_named_like_charons_trash_on_a_drive_that_ignores_case_is_never_synced() {
    let Some(ci) = case_insensitive_folder("a_folder_named_like_charons_trash_on_a_drive_that_ignores_case_is_never_synced") else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), ci.path().join("dst"), tmp.path().join("data"));
    write(&src.join("a.txt"), "a");
    write(&src.join("gone.txt"), "g");
    write(&dst.join(".Charon-Trash/notes.txt"), "my notes");
    let engine = engine(&data);
    let id = engine.add_job(two_way(&src, &dst, &[])).unwrap().job.id;

    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(notes_of(&review), vec!["LeftOut Some(Destination) .Charon-Trash"]);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait(&engine, &id);

    fs::remove_file(src.join("gone.txt")).unwrap();
    assert_eq!(confirm(&engine, &id), vec!["Remove Some(Destination) gone.txt"]);
    assert!(!dst.join("gone.txt").exists());
    assert_eq!(listed(&engine, &id), Vec::<String>::new(), "nothing comes back from the trash");
    assert_eq!(charon_like_names_in(&src), vec![".charon"], "only Charon's own marker folder");
    assert_eq!(fs::read_to_string(dst.join(".Charon-Trash/notes.txt")).unwrap(), "my notes");
}

#[cfg(unix)]
fn make_pipe(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let made = unsafe { libc::mkfifo(name.as_ptr(), 0o644) };
    assert_eq!(made, 0, "{}", std::io::Error::last_os_error());
}

/// The source gets a file "channel" where the destination has a named pipe, and the person
/// removes gone.txt on the source. A copy into the pipe's place once failed on every run,
/// and held back the removal of gone.txt forever.
#[cfg(unix)]
#[test]
fn a_pipe_where_the_other_side_has_a_file_never_holds_back_a_removal() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("keep.txt"), "k");
    write(&src.join("gone.txt"), "g");
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(two_way(&src, &dst, &[])).unwrap().job.id;
    confirm(&engine, &id);
    assert!(dst.join("gone.txt").is_file());

    write(&src.join("channel"), "data");
    make_pipe(&dst.join("channel"));
    fs::remove_file(src.join("gone.txt")).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(steps_of(&engine, &id, &review.review_id), vec!["Remove Some(Destination) gone.txt"]);
    let notes: Vec<_> = review.notes.iter().filter(|note| note.path == "channel").collect();
    assert_eq!(notes.len(), 1, "one note for the name: {:?}", review.notes);
    assert_eq!((notes[0].kind, notes[0].side), (NoteKind::LeftOut, Some(Side::Destination)));
    engine.approve(&id, &review.review_id, true).unwrap();
    wait(&engine, &id);

    assert!(!dst.join("gone.txt").exists(), "the removal went ahead");
    let run = engine.job(&id).unwrap().job.last_run.unwrap();
    assert_eq!((run.deleted, run.failed, run.skipped), (1, 0, 0));
    assert_eq!(listed(&engine, &id), Vec::<String>::new(), "the job settles");
    assert_eq!(fs::read_to_string(src.join("channel")).unwrap(), "data");
    assert!(fs::symlink_metadata(dst.join("channel")).unwrap().file_type().is_fifo(), "the pipe is left as it is");
}
