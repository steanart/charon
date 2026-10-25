//! How runs start, stop and wait, when something around them goes wrong: a run cut off
//! before it was recorded, a record that can't be saved, Charon starting again, and a drive
//! coming back.

use charon_core::model::SyncMode;
use charon_core::{ConflictRule, Engine, EngineEvent, Error, JobSetup, JobStatus, Trigger};
use std::fs;
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

fn setup(src: &Path, dst: &Path, mode: SyncMode, full_automation: bool) -> JobSetup {
    JobSetup {
        name: String::new(),
        source: src.to_path_buf(),
        destination: dst.to_path_buf(),
        mode,
        conflict: ConflictRule::Ask,
        trigger: Trigger::Manual,
        excludes: JobSetup::default_excludes(),
        safe_delete: false,
        full_automation,
    }
}

fn wait(engine: &Engine, id: &String) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while engine.is_running(id) {
        assert!(Instant::now() < deadline, "sync took too long");
        thread::sleep(Duration::from_millis(10));
    }
}

fn confirm(engine: &Engine, id: &String) {
    let review = engine.prepare_review(id).unwrap();
    engine.approve(id, &review.review_id, true).unwrap();
    wait(engine, id);
}

/// Looks at the job in the background and waits until a look has found something. A look
/// that was outdated while it ran doesn't count, and the job is looked at again once it's
/// due, as the autopilot does every second.
fn look_until(engine: &Engine, id: &String, found: impl Fn(&Engine) -> bool) {
    engine.check(id);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !found(engine) {
        assert!(Instant::now() < deadline, "the look never finished");
        thread::sleep(Duration::from_millis(20));
        if engine.check_due(id, Duration::from_secs(60)) {
            engine.check(id);
        }
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A run that was cut off after its delete step, on a drive whose change times can't be
/// trusted (Windows, FAT/exFAT, network shares; tmpfs here), where a file put back from the
/// system's trash looks exactly as it did.
#[cfg(target_os = "linux")]
mod after_an_unfinished_run {
    use super::*;
    use charon_core::model::{ConflictChoice, StepFilter};
    use charon_core::SyncStart;

    fn crash_after_delete(data: &Path, a: &Path, b: &Path, bin: &Path) {
        // The person deleted report.docx on the source by mistake (into the system's trash).
        fs::create_dir_all(bin).unwrap();
        fs::rename(a.join("report.docx"), bin.join("report.docx")).unwrap();
        // A run removed the destination's copy, then Charon was cut off: the mark stays set.
        fs::remove_file(b.join("report.docx")).unwrap();
        let state = data.join("charon-state.json");
        let text = fs::read_to_string(&state).unwrap();
        assert!(text.contains("\"runInProgress\": false"));
        fs::write(&state, text.replace("\"runInProgress\": false", "\"runInProgress\": true")).unwrap();
        // The person puts report.docx back (Put Back keeps its size and date).
        fs::rename(bin.join("report.docx"), a.join("report.docx")).unwrap();
        // A change the cut-off run hadn't carried: the fresh comparison shows a conflict.
        write(&a.join("plan.txt"), "edited on a");
    }

    fn folders(full_automation: bool) -> (tempfile::TempDir, Engine, String) {
        let tmp = tempfile::tempdir_in("/dev/shm").unwrap();
        let (a, b, data, bin) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"), tmp.path().join("bin"));
        write(&a.join("report.docx"), "the only report");
        write(&a.join("plan.txt"), "first");
        write(&b.join("Notes.txt"), "notes");
        {
            let engine = engine(&data);
            let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, full_automation)).unwrap().job.id;
            confirm(&engine, &id);
            assert!(b.join("report.docx").exists());
        }
        crash_after_delete(&data, &a, &b, &bin);
        let engine = engine(&data);
        let id = engine.jobs()[0].job.id.clone();
        (tmp, engine, id)
    }

    fn settle_plan(engine: &Engine, id: &String) {
        look_until(engine, id, |engine| !engine.job(id).unwrap().conflicts.is_empty());
        let conflict = engine.job(id).unwrap().conflicts.iter().find(|c| c.path == "plan.txt").cloned().expect("conflict card");
        let resolution = engine.prepare_resolution(id, &conflict, ConflictChoice::KeepSource).unwrap();
        assert!(
            resolution.warnings.iter().any(|w| w.text.contains("no memory of the last sync that it can trust")),
            "the card says that settling it leaves the rest compared afresh: {:?}",
            resolution.warnings
        );
        engine.approve(id, &resolution.review_id, false).unwrap();
        wait(engine, id);
    }

    #[test]
    fn settling_a_conflict_never_trusts_the_memory_from_before_again() {
        let (tmp, engine, id) = folders(false);
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        settle_plan(&engine, &id);

        // The next list compares afresh, and says why: the restored report goes back,
        // nothing is removed.
        let next = engine.prepare_review(&id).unwrap();
        assert!(next.warnings.iter().any(|w| w.text.contains("didn't finish properly") && w.text.contains("afresh")), "{:?}", next.warnings);
        assert!(!next.warnings.iter().any(|w| w.text.contains("first two-way sync")), "{:?}", next.warnings);
        let removals = engine.review_steps(&id, &next.review_id, StepFilter::Removals, "", 0, 50).unwrap();
        assert_eq!(removals.total, 0, "{:?}", removals.steps.iter().map(|s| &s.path).collect::<Vec<_>>());
        engine.approve(&id, &next.review_id, true).unwrap();
        wait(&engine, &id);
        assert!(a.join("report.docx").exists() && b.join("report.docx").exists());
    }

    #[test]
    fn full_automation_keeps_a_restored_file_after_a_conflict_is_settled() {
        let (tmp, engine, id) = folders(true);
        let a = tmp.path().join("a");
        settle_plan(&engine, &id);
        if engine.sync_now(&id).unwrap() == SyncStart::Started {
            wait(&engine, &id);
        }
        assert!(a.join("report.docx").exists(), "the restored report was removed without a list");
    }
}

/// A record Charon can't save, as on a full system disk.
#[cfg(unix)]
mod when_the_state_cant_be_saved {
    use super::*;
    use charon_core::SyncStart;

    /// Makes Charon's state impossible to save: a folder stands where the state file is
    /// written.
    fn break_saving(data: &Path) -> impl Drop {
        struct Restore(std::path::PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(self.0.join("charon-state.json"));
                let _ = fs::rename(self.0.join("saved-state.json"), self.0.join("charon-state.json"));
            }
        }
        fs::rename(data.join("charon-state.json"), data.join("saved-state.json")).unwrap();
        fs::create_dir_all(data.join("charon-state.json").join("in-the-way")).unwrap();
        Restore(data.to_path_buf())
    }

    #[test]
    fn an_automatic_run_that_cant_save_its_state_tries_each_look_only_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
        write(&a.join("one.txt"), "1");
        fs::create_dir_all(&b).unwrap();
        let engine = engine(&data);
        let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, true)).unwrap().job.id;
        confirm(&engine, &id);
        write(&a.join("two.txt"), "2");
        look_until(&engine, &id, |engine| engine.has_pending(&id));

        let _restore = break_saving(&data);
        assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
        wait(&engine, &id);
        assert!(!b.join("two.txt").exists(), "nothing crosses before the run is marked as running");
        assert!(engine.job(&id).unwrap().job.last_run.unwrap().error.is_some());
        // The autopilot starts a run while a look has changes nobody tried yet: this one was tried.
        assert!(!engine.has_pending(&id), "the same look would start a full run on every tick");
        // A newer look may try once more.
        look_until(&engine, &id, |engine| engine.has_pending(&id));
    }

    #[test]
    fn a_held_run_whose_record_cant_be_saved_still_waits_for_an_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
        for i in 0..40 {
            write(&a.join(format!("f{i}.txt")), "x");
        }
        fs::create_dir_all(&b).unwrap();
        let engine = engine(&data);
        let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, true)).unwrap().job.id;
        confirm(&engine, &id);

        let _restore = break_saving(&data);
        for i in 0..30 {
            fs::remove_file(a.join(format!("f{i}.txt"))).unwrap();
        }
        assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
        wait(&engine, &id);
        assert_eq!(fs::read_dir(&b).unwrap().count(), 40 + 1, "nothing was removed (40 files and Charon's marker folder)");

        // Everything that reads the job sees it waiting for an OK, though that couldn't be saved.
        let view = engine.job(&id).unwrap();
        assert!(view.job.needs_review && view.job.held.is_some());
        assert!(!view.runs_by_itself);
        assert_eq!(view.status, JobStatus::Review);
        assert!(view.waiting.is_some());
        assert_eq!(engine.overview().waiting, 1);
        assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Review);
        assert_eq!(engine.sync_all().unwrap(), vec![id.clone()], "Sync all lists it among the jobs that ask");
    }
}

#[test]
fn after_a_restart_changes_that_waited_still_wait_until_the_first_look() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("one.txt"), "1");
    fs::create_dir_all(&b).unwrap();
    {
        let engine = engine(&data);
        let id = engine.add_job(setup(&a, &b, SyncMode::Backup, false)).unwrap().job.id;
        confirm(&engine, &id);
        write(&a.join("two.txt"), "2");
        look_until(&engine, &id, |engine| engine.job(&id).unwrap().waiting.is_some());
    }

    // Charon starts again: nothing has looked yet, but the change still waits.
    let engine = engine(&data);
    let view = engine.jobs()[0].clone();
    assert_eq!(view.status, JobStatus::Review);
    let waiting = view.waiting.expect("the stored wait is shown");
    assert!(!waiting.counted, "how many changes wait is for the first look to say");
    assert_eq!(engine.overview().waiting, 1);
    assert_ne!(engine.overview().headline(), "All in sync");
}

#[test]
fn a_folder_that_comes_back_is_looked_at_before_it_says_in_sync() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("one.txt"), "1");
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::Backup, false)).unwrap().job.id;
    confirm(&engine, &id);
    look_until(&engine, &id, |engine| engine.job(&id).unwrap().status == JobStatus::InSync);

    // The backup drive was away while files changed; it's back now (the autopilot calls this).
    write(&a.join("two.txt"), "2");
    engine.forget_look(&id);
    let view = engine.job(&id).unwrap();
    assert_eq!(view.status, JobStatus::Looking);
    assert_eq!(engine.overview().headline(), "Looking for changes…");
    look_until(&engine, &id, |engine| engine.job(&id).unwrap().status == JobStatus::Review);
}
