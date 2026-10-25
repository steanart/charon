//! End-to-end: which folders a Nextcloud or ownCloud client syncs is told by the client's
//! own settings on this computer, and by the client's journal in a folder they don't list.
//!
//! The test sets the variables that lead Charon to the clients' settings before anything
//! reads them, so it's the only test in this file: it goes through each case in turn.

use charon_core::model::SyncMode;
use charon_core::plan::{NoteKind, Side};
use charon_core::{ConflictRule, Engine, EngineEvent, JobSetup, Trigger};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn engine(data: &Path) -> Engine {
    Engine::open(data, Arc::new(|_: EngineEvent| {})).unwrap()
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

/// Makes `home` the home folder of this run, with a Nextcloud client's settings that list
/// `folders` as the folders it syncs, where the client keeps them on Linux, macOS and
/// Windows.
fn home_with_nextcloud_syncing(home: &Path, folders: &[&Path]) {
    let mut settings = String::from("[General]\nclientVersion=3.13.0\n\n[Accounts]\n");
    for (number, folder) in folders.iter().enumerate() {
        // As the client writes a folder: with `/` between its parts on Windows too.
        settings += &format!("0\\Folders\\{}\\localPath={}/\n", number + 1, folder.display().to_string().replace('\\', "/"));
    }
    for settings_folder in [".config", "Library/Preferences", "AppData/Roaming"] {
        write(&home.join(settings_folder).join("Nextcloud/nextcloud.cfg"), &settings);
    }
    std::env::set_var("HOME", home);
    std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
    std::env::set_var("APPDATA", home.join("AppData/Roaming"));
    std::env::set_var("LOCALAPPDATA", home.join("AppData/Local"));
}

/// This computer's Nextcloud client syncs one folder for each mode, and only those.
#[test]
fn which_folders_a_client_syncs_is_told_by_its_settings_and_by_its_journal() {
    let tmp = tempfile::tempdir().unwrap();
    let modes = [SyncMode::Mirror, SyncMode::Backup, SyncMode::TwoWay];
    let synced = |mode: SyncMode| tmp.path().join(format!("home/{mode:?}/Nextcloud"));
    home_with_nextcloud_syncing(&tmp.path().join("home"), &modes.map(synced).iter().map(|folder| folder.as_path()).collect::<Vec<_>>());
    for mode in modes {
        a_copy_of_a_journal_is_named_in_the_note_of_the_file_it_keeps_from_a_backup(tmp.path(), &synced(mode), mode);
        a_placeholder_on_a_drive_another_computers_client_syncs_is_one(tmp.path(), mode);
    }
}

/// A backup made by a tool that copied a Nextcloud folder as it was, from its top (Charon
/// 0.2, rsync, cp), holds a copy of the client's journal, and copies of the placeholders of
/// the files that were only in the cloud then. The client's settings here don't list the
/// backup, but Charon can't tell it from a drive a client on another computer syncs: the
/// file each copy names is left alone, and the note names the journal and says what to do.
/// Once the person removes the old copies, the file is backed up. In the folder the
/// settings list, a placeholder is the client's own, and its note needs no journal.
fn a_copy_of_a_journal_is_named_in_the_note_of_the_file_it_keeps_from_a_backup(tmp: &Path, src: &Path, mode: SyncMode) {
    let (dst, data) = (tmp.join(format!("usb/{mode:?}/Nextcloud backup")), tmp.join(format!("data/{mode:?}")));
    // The Nextcloud folder, at its top: the client's journal, and the report, downloaded
    // again since the backup was made.
    write(&src.join(".sync_0123456789ab.db"), "the client's journal");
    write(&src.join("Docs/Report.pdf"), "the report");
    write(&src.join("Docs/Notes.txt"), "notes");
    write(&dst.join(".sync_0123456789ab.db"), "an old copy of the client's journal");
    write(&dst.join("Docs/Report.pdf.nextcloud"), " ");
    write(&dst.join("Docs/Notes.txt"), "notes");
    let engine = engine(&data);
    let id = engine.add_job(setup(src, &dst, mode)).unwrap().job.id;
    let review = confirm(&engine, &id);
    let note = review.notes.iter().find(|note| note.path == "Docs/Report.pdf").unwrap_or_else(|| panic!("{mode:?}: {:?}", review.notes));
    assert_eq!((note.kind, note.side), (NoteKind::Unreadable, Some(Side::Destination)), "{mode:?}");
    assert_eq!(
        note.reason,
        "it's only in the cloud (only its placeholder “Report.pdf.nextcloud” is here, inside a folder that holds a Nextcloud or ownCloud client's journal, “.sync_0123456789ab.db”), so Charon leaves it alone (if no such client on any computer syncs that folder, the journal and the placeholder are old copies: remove them)",
        "{mode:?}"
    );
    assert!(!dst.join("Docs/Report.pdf").exists(), "{mode:?}: left alone");
    assert!(dst.join("Docs/Report.pdf.nextcloud").exists(), "{mode:?}: never removed");
    assert!(!src.join("Docs/Report.pdf.nextcloud").exists(), "{mode:?}: never made where the client would take it for its own");
    assert_eq!(fs::read_to_string(src.join("Docs/Report.pdf")).unwrap(), "the report", "{mode:?}");

    // The person removes the old copies, as the note says.
    fs::remove_file(dst.join(".sync_0123456789ab.db")).unwrap();
    fs::remove_file(dst.join("Docs/Report.pdf.nextcloud")).unwrap();
    let review = confirm(&engine, &id);
    assert!(review.notes.iter().all(|note| note.path != "Docs/Report.pdf"), "{mode:?}: {:?}", review.notes);
    assert_eq!(fs::read_to_string(dst.join("Docs/Report.pdf")).ok().as_deref(), Some("the report"), "{mode:?}: backed up");
    assert!(!dst.join(".sync_0123456789ab.db").exists(), "{mode:?}: the journal is never copied");
    let again = engine.prepare_review(&id).unwrap();
    assert_eq!(again.total_steps, 0, "{mode:?}: {:?}", again.steps);

    // In the folder the client syncs, a placeholder is the client's own: the file it stands
    // for is only in the cloud, and left alone on both sides.
    fs::remove_file(src.join("Docs/Notes.txt")).unwrap();
    write(&src.join("Docs/Notes.txt.nextcloud"), " ");
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.total_steps, 0, "{mode:?}: {:?}", review.steps);
    let note = review.notes.iter().find(|note| note.path == "Docs/Notes.txt").unwrap_or_else(|| panic!("{mode:?}: {:?}", review.notes));
    assert_eq!(
        (note.kind, note.reason.as_str()),
        (
            NoteKind::Unreadable,
            "it's only in the cloud (only its placeholder “Notes.txt.nextcloud” is on this computer), so Charon leaves it alone (if that placeholder is an old copy, remove it)"
        ),
        "{mode:?}"
    );
}

/// A drive whose Nextcloud folder a client on another computer syncs (a laptop's external
/// drive), backed up from this computer: the settings here don't list it, but the client's
/// journal at its top says a client syncs it. When that client frees a file up, the file is
/// only in the cloud, and its copy on the other side stays: it never looks removed, and the
/// placeholder is never copied.
fn a_placeholder_on_a_drive_another_computers_client_syncs_is_one(tmp: &Path, mode: SyncMode) {
    let (src, dst, data) = (tmp.join(format!("ssd/{mode:?}/Nextcloud")), tmp.join(format!("backup/{mode:?}")), tmp.join(format!("data/ssd {mode:?}")));
    write(&src.join(".sync_0123456789ab.db"), "the laptop client's journal");
    write(&src.join("Docs/Report.pdf"), "the report");
    fs::create_dir_all(&dst).unwrap();
    let engine = engine(&data);
    let id = engine.add_job(setup(&src, &dst, mode)).unwrap().job.id;
    confirm(&engine, &id);
    assert_eq!(fs::read_to_string(dst.join("Docs/Report.pdf")).unwrap(), "the report", "{mode:?}");

    // The laptop's client frees the report up.
    fs::remove_file(src.join("Docs/Report.pdf")).unwrap();
    write(&src.join("Docs/Report.pdf.nextcloud"), " ");
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.total_steps, 0, "{mode:?}: {:?}", review.steps);
    let note = review.notes.iter().find(|note| note.path == "Docs/Report.pdf").unwrap_or_else(|| panic!("{mode:?}: {:?}", review.notes));
    assert_eq!((note.kind, note.side), (NoteKind::Unreadable, Some(Side::Source)), "{mode:?}");
    assert!(note.reason.contains("“Report.pdf.nextcloud”") && note.reason.contains("“.sync_0123456789ab.db”"), "{mode:?}: {}", note.reason);
    assert_eq!(fs::read_to_string(dst.join("Docs/Report.pdf")).unwrap(), "the report", "{mode:?}: kept");
}
