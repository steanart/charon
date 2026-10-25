//! End-to-end: the scanner, network shares and cloud folders, with real folders on disk,
//! real looks, lists and runs through the engine.

use charon_core::model::{Level, SyncMode};
use charon_core::plan::NoteKind;
use charon_core::{ConflictRule, Engine, EngineEvent, JobSetup, SyncStart, Trigger};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

fn engine(data: &Path) -> Engine {
    Engine::open(data, Arc::new(|_: EngineEvent| {})).unwrap()
}

/// Every event an engine sent, with the thread it was sent from.
type Events = Arc<Mutex<Vec<(ThreadId, EngineEvent)>>>;

/// An engine that keeps every event it sends.
fn recording(data: &Path) -> (Engine, Events) {
    let events: Events = Arc::default();
    let sink = events.clone();
    let engine = Engine::open(data, Arc::new(move |event: EngineEvent| sink.lock().unwrap().push((thread::current().id(), event)))).unwrap();
    (engine, events)
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

fn set_modified(path: &Path, secs: u64) {
    let time = std::time::UNIX_EPOCH + Duration::from_secs(secs);
    fs::File::options().write(true).open(path).unwrap().set_times(fs::FileTimes::new().set_modified(time)).unwrap();
}

#[test]
fn full_automation_holds_a_mirror_whose_source_folder_suddenly_looks_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    for i in 0..40 {
        write(&src.join(format!("docs/d{i}.txt")), "x");
    }
    for i in 0..6 {
        write(&src.join(format!("photos/2019/p{i}.jpg")), "x");
    }
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let mut job = setup(&src, &dst, SyncMode::Mirror, ConflictRule::Ask);
    job.full_automation = true;
    let id = engine.add_job(job).unwrap().job.id;
    confirm(&engine, &id);

    // A share that lists the folder before it has loaded: there, but showing nothing inside.
    for i in 0..6 {
        fs::remove_file(src.join(format!("photos/2019/p{i}.jpg"))).unwrap();
    }
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait(&engine, &id);
    for i in 0..6 {
        assert!(dst.join(format!("photos/2019/p{i}.jpg")).exists(), "nothing was removed");
    }
    let view = engine.job(&id).unwrap();
    assert!(view.job.needs_review);
    assert!(view.job.held.as_deref().is_some_and(|held| held.contains("photos/2019") && held.contains("looks empty")), "{:?}", view.job.held);
    // The list says why, in red, and asks for the tick.
    let review = engine.prepare_review(&id).unwrap();
    assert!(review.needs_tick);
    assert!(review.warnings.iter().any(|w| w.level == Level::Danger && w.text.contains("photos/2019")), "{:?}", review.warnings);
}

#[test]
fn looks_never_read_the_folders_on_the_thread_that_asks() {
    // Charon's own threads never download files that are only in the cloud. A list is
    // asked for on the window's thread, which must keep its own setting.
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("same.txt"), "same contents");
    write(&b.join("same.txt"), "same contents");
    set_modified(&a.join("same.txt"), 1_600_000_000);
    set_modified(&b.join("same.txt"), 1_700_000_000);
    let (engine, events) = recording(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;

    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.total_steps, 0, "found identical by their contents");
    let asking = thread::current().id();
    let events = events.lock().unwrap();
    let looking: Vec<&(ThreadId, EngineEvent)> =
        events.iter().filter(|(_, event)| matches!(event, EngineEvent::ScanProgress { .. } | EngineEvent::CompareProgress { .. })).collect();
    assert!(looking.iter().any(|(_, event)| matches!(event, EngineEvent::CompareProgress { .. })), "the contents were compared");
    assert!(looking.iter().filter(|(_, event)| matches!(event, EngineEvent::ScanProgress { .. })).count() >= 2, "both sides were scanned");
    assert!(looking.iter().all(|(thread, _)| *thread != asking), "nothing was read on the thread that asked");
}

#[cfg(target_os = "linux")]
#[test]
fn a_change_left_in_only_by_its_case_makes_the_list_stale() {
    // Both folders tell upper and lower case apart, so "*.tmp" leaves "notes.TMP" in the
    // sync: a change to it matters.
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("a.txt"), "a");
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let mut job = setup(&src, &dst, SyncMode::Backup, ConflictRule::Ask);
    job.excludes.push("*.tmp".into());
    let id = engine.add_job(job).unwrap().job.id;

    let review = engine.prepare_review(&id).unwrap();
    assert!(engine.note_change(&id, &src.join("notes.TMP")));
    assert!(matches!(engine.approve(&id, &review.review_id, true), Err(charon_core::Error::ReviewOutdated(_))));
    // A change to an item the sync leaves out still doesn't matter.
    let review = engine.prepare_review(&id).unwrap();
    assert!(!engine.note_change(&id, &src.join("notes.tmp")));
    engine.approve(&id, &review.review_id, true).unwrap();
    wait(&engine, &id);
}

#[cfg(target_os = "linux")]
#[test]
fn a_first_sync_into_an_empty_folder_is_trusted_the_next_time() {
    // tmpfs is a kind of drive whose case setting Charon can't guess; an empty folder there
    // used to get another answer than the same folder once it held files.
    let shared_memory = Path::new("/dev/shm");
    if !shared_memory.is_dir() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let far = tempfile::tempdir_in(shared_memory).unwrap();
    let (a, b, data) = (tmp.path().join("a"), far.path().join("b"), tmp.path().join("data"));
    write(&a.join("Report.txt"), "1");
    fs::create_dir_all(&b).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&a, &b, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);
    assert!(b.join("Report.txt").exists());

    write(&a.join("Notes.txt"), "2");
    let review = engine.prepare_review(&id).unwrap();
    assert!(review.warnings.iter().all(|w| !w.text.contains("upper and lower case")), "{:?}", review.warnings);
    assert!(review.warnings.iter().all(|w| w.level == Level::Info), "the memory is trusted: {:?}", review.warnings);
    assert_eq!(review.tally.destination.new_files, 1);
}

#[test]
fn a_folder_named_like_a_drives_own_is_left_alone_and_listed() {
    // The top of a drive keeps folders of its own; anywhere else it's the person's folder.
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("lost+found/notes.txt"), "mine");
    write(&src.join("a.txt"), "a");
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, SyncMode::Backup, ConflictRule::Ask)).unwrap().job.id;

    let review = confirm(&engine, &id);
    assert!(review.notes.iter().any(|note| note.kind == NoteKind::LeftOut && note.path == "lost+found"), "{:?}", review.notes);
    assert!(dst.join("a.txt").exists());
    assert!(!dst.join("lost+found").exists(), "left alone");
}

/// A folder a Nextcloud client syncs, in `tmp`: the client keeps its journal at the top of
/// it, which tells Charon so whatever the clients' settings on this computer say. The job's
/// folder is the folder `Work` inside it.
fn synced_by_nextcloud(tmp: &Path) -> std::path::PathBuf {
    write(&tmp.join("Nextcloud/.sync_0123456789ab.db"), "the client's journal");
    let work = tmp.join("Nextcloud/Work");
    fs::create_dir_all(&work).unwrap();
    work
}

/// Frees up `file` the way Nextcloud does with virtual files: the file goes, and a
/// placeholder takes its place.
fn free_up(file: &Path) {
    fs::remove_file(file).unwrap();
    let name = file.file_name().unwrap().to_str().unwrap();
    write(&file.with_file_name(format!("{name}.nextcloud")), " ");
}

/// Downloads `file` again: its placeholder goes, and the file is back as it was.
fn download(file: &Path, text: &str, secs: u64) {
    let name = file.file_name().unwrap().to_str().unwrap();
    fs::remove_file(file.with_file_name(format!("{name}.nextcloud"))).unwrap();
    write(file, text);
    set_modified(file, secs);
}

#[test]
fn a_file_the_cloud_freed_up_never_looks_removed() {
    for mode in [SyncMode::TwoWay, SyncMode::Mirror] {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst, data) = (synced_by_nextcloud(tmp.path()), tmp.path().join("dst"), tmp.path().join("data"));
        for i in 0..10 {
            write(&src.join(format!("Docs/n{i}.txt")), "x");
        }
        write(&src.join("Docs/Report.pdf"), "the report");
        set_modified(&src.join("Docs/Report.pdf"), 1_700_000_000);
        fs::create_dir_all(&dst).unwrap();
        let engine = engine(&data);
        let mut job = setup(&src, &dst, mode, ConflictRule::Ask);
        job.full_automation = true;
        let id = engine.add_job(job).unwrap().job.id;
        confirm(&engine, &id);
        assert!(dst.join("Docs/Report.pdf").exists());

        free_up(&src.join("Docs/Report.pdf"));
        assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
        wait(&engine, &id);
        assert_eq!(fs::read_to_string(dst.join("Docs/Report.pdf")).unwrap(), "the report", "{mode:?}: the other side's copy stays");
        assert!(!dst.join("Docs/Report.pdf.nextcloud").exists(), "{mode:?}: the placeholder isn't copied");
        assert!(!dst.join(".charon-trash").exists(), "{mode:?}: nothing was removed");
        let review = engine.prepare_review(&id).unwrap();
        assert_eq!(review.total_steps, 0, "{mode:?}");
        assert!(
            review.notes.iter().any(|note| note.kind == NoteKind::Unreadable && note.path == "Docs/Report.pdf" && note.reason.contains("only in the cloud")),
            "{mode:?}: {:?}",
            review.notes
        );

        download(&src.join("Docs/Report.pdf"), "the report", 1_700_000_000);
        let review = engine.prepare_review(&id).unwrap();
        assert_eq!(review.total_steps, 0, "{mode:?}: in sync again");
        assert!(review.notes.is_empty(), "{mode:?}: {:?}", review.notes);
    }
}

/// A Mirror into a folder the cloud frees files up in never copies over a file that's only
/// in the cloud, and never removes its placeholder (which would remove the file there).
#[test]
fn a_mirror_leaves_alone_what_the_cloud_freed_up_in_its_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), synced_by_nextcloud(tmp.path()), tmp.path().join("data"));
    write(&src.join("Docs/Report.pdf"), "the report");
    write(&src.join("Docs/Notes.txt"), "notes");
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, SyncMode::Mirror, ConflictRule::Ask)).unwrap().job.id;
    confirm(&engine, &id);

    free_up(&dst.join("Docs/Report.pdf"));
    write(&src.join("Docs/Report.pdf"), "edited on the source");
    let review = confirm(&engine, &id);
    assert!(review.notes.iter().any(|note| note.kind == NoteKind::Unreadable && note.path == "Docs/Report.pdf"), "{:?}", review.notes);
    assert!(dst.join("Docs/Report.pdf.nextcloud").exists(), "the placeholder stays");
    assert!(!dst.join("Docs/Report.pdf").exists(), "nothing is copied over the file in the cloud");
}

/// A backup made by a tool that copied a cloud folder as it was (Charon 0.2, rsync, cp)
/// holds copies of the cloud's placeholders. On a drive where no cloud service keeps files
/// they are ordinary files: the file each one names is backed up all the same, and a Mirror
/// replaces the copy of the placeholder with the file.
#[test]
fn a_copy_of_a_placeholder_never_keeps_the_file_from_a_backup() {
    for mode in [SyncMode::Mirror, SyncMode::Backup, SyncMode::TwoWay] {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
        write(&src.join("Docs/Report.pdf"), "the report");
        write(&src.join("Docs/Plans.odt"), "the plans");
        write(&src.join("Docs/Notes.txt"), "notes");
        write(&dst.join("Docs/.Report.pdf.icloud"), "placeholder");
        write(&dst.join("Docs/Plans.odt.nextcloud"), " ");
        let engine = engine(&data);
        let id = engine.add_job(setup(&src, &dst, mode, ConflictRule::Ask)).unwrap().job.id;
        let review = confirm(&engine, &id);
        assert!(review.notes.is_empty(), "{mode:?}: {:?}", review.notes);
        assert_eq!(fs::read_to_string(dst.join("Docs/Report.pdf")).unwrap(), "the report", "{mode:?}");
        assert_eq!(fs::read_to_string(dst.join("Docs/Plans.odt")).unwrap(), "the plans", "{mode:?}");
        let copies = [dst.join("Docs/.Report.pdf.icloud"), dst.join("Docs/Plans.odt.nextcloud")];
        match mode {
            SyncMode::Mirror => assert!(copies.iter().all(|copy| !copy.exists()), "{mode:?}: replaced by the files"),
            _ => assert!(copies.iter().all(|copy| copy.exists()), "{mode:?}: kept"),
        }
        if mode == SyncMode::TwoWay {
            assert!(src.join("Docs/.Report.pdf.icloud").exists() && src.join("Docs/Plans.odt.nextcloud").exists(), "carried like any other file");
        }
        let review = engine.prepare_review(&id).unwrap();
        assert_eq!(review.total_steps, 0, "{mode:?}: {:?}", review.steps);
        assert!(review.notes.is_empty(), "{mode:?}: {:?}", review.notes);
    }
}

/// Where a cloud service keeps files, Charon never makes a file named like its placeholder:
/// the service would take it for its own, and the file it names for freed up.
#[test]
fn a_copy_of_a_placeholder_is_never_made_where_its_service_keeps_files() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("usb"), synced_by_nextcloud(tmp.path()), tmp.path().join("data"));
    write(&src.join("Docs/a.txt"), "a");
    write(&src.join("Docs/Plans.odt.nextcloud"), " ");
    write(&src.join("Docs/Old.txt.nextcloud"), " ");
    write(&dst.join("Docs/Plans.odt"), "the plans");
    // Only in the cloud there.
    write(&dst.join("Docs/Old.txt.nextcloud"), " ");
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, SyncMode::TwoWay, ConflictRule::Ask)).unwrap().job.id;
    let review = confirm(&engine, &id);
    let note = |path: &str| review.notes.iter().find(|note| note.path == path).unwrap_or_else(|| panic!("{path}: {:?}", review.notes));
    let made = note("Docs/Plans.odt.nextcloud");
    assert_eq!(made.kind, NoteKind::LeftOut);
    assert!(made.reason.contains("named like a placeholder Nextcloud or ownCloud leaves") && made.reason.contains("on the destination"), "{}", made.reason);
    let kept = note("Docs/Old.txt.nextcloud");
    assert_eq!(kept.kind, NoteKind::LeftOut);
    assert!(kept.reason.contains("is the placeholder Nextcloud or ownCloud left"), "{}", kept.reason);
    assert_eq!(note("Docs/Old.txt").kind, NoteKind::Unreadable);
    assert_eq!(review.notes.len(), 3, "{:?}", review.notes);

    assert!(!dst.join("Docs/Plans.odt.nextcloud").exists(), "never made where the client would take it for its own");
    assert_eq!(fs::read_to_string(src.join("Docs/Plans.odt")).unwrap(), "the plans");
    assert_eq!(fs::read_to_string(dst.join("Docs/a.txt")).unwrap(), "a");
    assert!(dst.join("Docs/Old.txt.nextcloud").exists() && src.join("Docs/Old.txt.nextcloud").exists());
}

/// A Nextcloud or ownCloud client's journal, and the files that go with it, are the
/// client's own: a sync never copies them (a copy would make the other side look synced by
/// the client), and never replaces or removes the other side's.
#[test]
fn a_sync_clients_journal_is_never_carried_or_removed() {
    for mode in [SyncMode::Mirror, SyncMode::TwoWay] {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst, data) = (tmp.path().join("Nextcloud"), tmp.path().join("usb/Backup"), tmp.path().join("data"));
        let journals = [
            ".sync_0123456789ab.db",
            ".sync_0123456789ab.db-wal",
            ".sync_0123456789ab.db-shm",
            "Old/.csync_journal.db",
            "Old/._sync_ba9876543210.db-journal",
        ];
        for journal in journals {
            write(&src.join(journal), "the client's journal");
        }
        write(&src.join("Docs/Notes.txt"), "notes");
        // On the destination, an old copy of the journal, and another client's.
        write(&dst.join(".sync_0123456789ab.db"), "an old copy");
        write(&dst.join("Work/.sync_fedcba987654.db"), "another client's journal");
        let engine = engine(&data);
        let id = engine.add_job(setup(&src, &dst, mode, ConflictRule::Ask)).unwrap().job.id;
        let review = confirm(&engine, &id);
        // A Mirror keeps the folder that holds the other client's journal, and says why.
        let notes: Vec<(NoteKind, &str, &str)> = review.notes.iter().map(|note| (note.kind, note.path.as_str(), note.reason.as_str())).collect();
        let why = "kept: the source no longer has it, but it holds items Charon leaves alone (excluded, left out or unreadable items, or what Charon or a cloud service keeps for its own use)";
        let kept = if mode == SyncMode::Mirror { vec![(NoteKind::KeptFolder, "Work", why)] } else { Vec::new() };
        assert_eq!(notes, kept, "{mode:?}");
        assert_eq!(fs::read_to_string(dst.join("Docs/Notes.txt")).unwrap(), "notes", "{mode:?}");
        assert!(dst.join("Old").is_dir(), "{mode:?}: the folder is synced");
        for journal in &journals[1..] {
            assert!(!dst.join(journal).exists(), "{mode:?}: {journal} isn't copied");
        }
        assert_eq!(fs::read_to_string(dst.join(".sync_0123456789ab.db")).unwrap(), "an old copy", "{mode:?}: never replaced");
        assert!(dst.join("Work/.sync_fedcba987654.db").exists(), "{mode:?}: never removed");
        assert!(!src.join("Work/.sync_fedcba987654.db").exists(), "{mode:?}: never copied back");
        assert!(journals.iter().all(|journal| src.join(journal).exists()), "{mode:?}");
        assert_eq!(engine.prepare_review(&id).unwrap().total_steps, 0, "{mode:?}");
    }
}
