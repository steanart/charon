//! Safety regressions: what Charon promises about never losing data, checked against the real
//! engine with real folders on disk.
//!
//! The first part turns around every probe written against Charon 0.2 for the code review
//! (review/review-probes.rs): each of those showed a way to lose data; here each one asserts
//! that it can no longer happen. The second part checks the promises 0.3 makes: nothing
//! changes without the person's OK (unless full automation is on and the job had its first
//! confirmed sync), a list can't be confirmed once it no longer matches the folders or the
//! job, unreadable places never turn into removals, excluded items are never deleted, edits
//! made after a list was made are never overwritten, and so on.
//!
//! Every test asserts the SAFE outcome. Waiting is always polling with a deadline; the few
//! short sleeps only let a time window pass (the 120 ms progress throttle, the pause between
//! pokes of a watched folder) and never decide a result: each test asserts the event it
//! relies on (a hook that fired, a control job that ran).
//!
//! Tests that need unreadable folders must run without CAP_DAC_OVERRIDE when the process is
//! root. Otherwise they skip, and say so on stderr in the normal test output ("SKIPPED ...");
//! with CHARON_REQUIRE_PERMISSION_TESTS=1 they fail instead of skipping (for CI):
//!
//! ```text
//! capsh --drop=cap_dac_override,cap_dac_read_search -- -c \
//!     "cargo test -p charon-core --test safety_regressions"
//! ```
//!
//! One test waits for a list to expire (15 minutes) and is ignored by default:
//! `cargo test -p charon-core --test safety_regressions -- --ignored`.

use charon_core::plan::{NoteKind, Side};
#[cfg(unix)]
use charon_core::scan::EntryKind;
use charon_core::{
    Autopilot, ConflictChoice, ConflictRule, Engine, EngineEvent, Error, JobSetup, JobStatus, JobView, Level, Phase, Review,
    SyncMode, SyncStart, Trigger,
};
#[cfg(unix)]
use charon_core::StepFilter;
use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use unicode_normalization::UnicodeNormalization;

/// How long any wait may take before the test fails.
const DEADLINE: Duration = Duration::from_secs(60);

// ==================================================================== helpers

type Hook = Box<dyn FnOnce() + Send>;

/// An engine that keeps the events it sends, and can run a one-shot hook on its sync thread
/// right before the first step of a run acts (after the list was made and the run began).
struct Rig {
    engine: Engine,
    hook: Arc<Mutex<Option<Hook>>>,
    fired: Arc<AtomicBool>,
    events: Arc<Mutex<Vec<EngineEvent>>>,
}

impl Rig {
    fn open(data: &Path) -> Rig {
        let hook: Arc<Mutex<Option<Hook>>> = Arc::default();
        let fired = Arc::new(AtomicBool::new(false));
        let events: Arc<Mutex<Vec<EngineEvent>>> = Arc::default();
        let (slot, flag, sink) = (hook.clone(), fired.clone(), events.clone());
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
                return;
            }
            if !matches!(event, EngineEvent::ScanProgress { .. }) {
                sink.lock().unwrap().push(event);
            }
        });
        let engine = Engine::open(data, emitter).expect("the engine opens");
        Rig { engine, hook, fired, events }
    }

    /// Runs `hook` on the sync thread right before the next run's first step acts.
    fn at_first_step(&self, hook: impl FnOnce() + Send + 'static) {
        self.fired.store(false, Ordering::SeqCst);
        *self.hook.lock().unwrap() = Some(Box::new(hook));
    }

    fn hook_fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }

    fn held_events(&self, id: &String) -> usize {
        self.events.lock().unwrap().iter().filter(|e| matches!(e, EngineEvent::Held { id: held, .. } if held == id)).count()
    }
}

/// The engine sends live progress at most every 120 ms. A hook needs the first step's
/// progress, so the previous run's last progress must be older than that. (This only lets
/// the window pass; whether the hook ran is always asserted.)
fn let_progress_throttle_pass() {
    thread::sleep(Duration::from_millis(150));
}

fn plain_engine(data: &Path) -> Engine {
    Engine::open(data, Arc::new(|_: EngineEvent| {})).expect("the engine opens")
}

/// Opens the engine once the previous one is completely gone (its threads may still hold it
/// for a moment after the last handle is dropped).
fn reopen(data: &Path) -> Engine {
    let deadline = Instant::now() + DEADLINE;
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

/// Waits until the previous engine on `data` is gone, without keeping a new one open.
fn wait_until_released(data: &Path) {
    drop(reopen(data));
}

fn job_setup(src: &Path, dst: &Path, mode: SyncMode) -> JobSetup {
    fs::create_dir_all(src).unwrap();
    fs::create_dir_all(dst).unwrap();
    JobSetup {
        name: String::new(),
        source: src.to_path_buf(),
        destination: dst.to_path_buf(),
        mode,
        conflict: ConflictRule::KeepBoth,
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

/// Polls `condition` until it holds; fails with `what` at the deadline.
fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// The person looks at the list and confirms it as it is; waits for the sync to finish.
fn confirm(engine: &Engine, id: &String) -> Review {
    let review = engine.prepare_review(id).expect("the list is made");
    assert!(review.blocked.is_none(), "the list is blocked: {:?}", review.blocked);
    engine.approve(id, &review.review_id, true).expect("the list is confirmed");
    wait_idle(engine, id);
    let run = view(engine, id).job.last_run.expect("the confirmed sync ran");
    assert!(run.approved && !run.cancelled && run.error.is_none(), "the confirmed sync failed: {run:?}");
    review
}

/// Whether the job's latest run was one nobody confirmed (full automation), and it ended.
fn ran_by_itself(engine: &Engine, id: &String) -> bool {
    !engine.is_running(id) && view(engine, id).job.last_run.is_some_and(|run| !run.approved)
}

/// A job's row in a few words, for failure messages.
fn describe(engine: &Engine, id: &String) -> String {
    let v = view(engine, id);
    format!(
        "{}: status={:?} running={} needs_review={} runs_by_itself={} held={:?} problem={:?} last_run={:?} waiting={:?}",
        v.job.setup.name,
        v.status,
        engine.is_running(id),
        v.job.needs_review,
        v.runs_by_itself,
        v.job.held,
        v.problem,
        v.job.last_run.as_ref().map(|r| (r.approved, r.cancelled, r.describe())),
        v.waiting.as_ref().map(|w| w.items),
    )
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

/// Everything under `root`, relative path → what it is ("file: contents", "folder",
/// "link -> target"). Links are listed, never followed. A folder that is missing or can't be
/// listed, or a file that can't be read, fails the test: it is never taken for an empty one.
fn tree(root: &Path) -> BTreeMap<String, String> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        let items = fs::read_dir(dir).unwrap_or_else(|e| panic!("{} can't be listed: {e}", dir.display()));
        for item in items {
            let path = item.unwrap_or_else(|e| panic!("an item in {} can't be listed: {e}", dir.display())).path();
            // Spelled with `/` on every system, as Charon's keys are.
            let relative = path.strip_prefix(root).unwrap().components().map(|part| part.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
            let meta = fs::symlink_metadata(&path).unwrap_or_else(|e| panic!("{} can't be looked at: {e}", path.display()));
            if meta.file_type().is_symlink() {
                out.insert(relative, format!("link -> {}", fs::read_link(&path).unwrap().display()));
            } else if meta.is_dir() {
                out.insert(relative, "folder".into());
                visit(root, &path, out);
            } else {
                let bytes = fs::read(&path).unwrap_or_else(|e| panic!("{} can't be read: {e}", path.display()));
                out.insert(relative, format!("file: {}", String::from_utf8_lossy(&bytes)));
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}

/// Copies the folder `from` to `to` (which must not exist yet) with each file's time, as a
/// copy on another drive would look, leaving out the items whose name is in `skip`.
fn copy_tree(from: &Path, to: &Path, skip: &[&str]) {
    fs::create_dir(to).unwrap();
    for item in fs::read_dir(from).unwrap() {
        let item = item.unwrap();
        let name = item.file_name().to_string_lossy().into_owned();
        if skip.contains(&name.as_str()) {
            continue;
        }
        let (path, target) = (item.path(), to.join(&name));
        let meta = fs::symlink_metadata(&path).unwrap();
        assert!(!meta.file_type().is_symlink(), "copy_tree doesn't copy links: {}", path.display());
        if meta.is_dir() {
            copy_tree(&path, &target, skip);
        } else {
            fs::copy(&path, &target).unwrap();
            filetime::set_file_mtime(&target, filetime::FileTime::from_last_modification_time(&meta)).unwrap();
        }
    }
}

/// The names in `dir`, for failure messages.
fn names_in(dir: &Path) -> Vec<String> {
    fs::read_dir(dir).unwrap().map(|item| format!("{:?}", item.unwrap().file_name())).collect()
}

/// Renames `nfc` (a name in its composed Unicode form) in `dir` to its other spelling, the
/// way a drive that hands names back in another form would show it. Exactly one of the two
/// spellings must be there. Returns the new name.
fn respell(dir: &Path, nfc: &str) -> String {
    assert_eq!(nfc.nfc().collect::<String>(), nfc, "{nfc:?} is given composed");
    let nfd: String = nfc.nfd().collect();
    assert_ne!(nfd, nfc, "{nfc:?} has a second spelling");
    // Read from the folder's listing, not by looking the names up: a drive that ignores the
    // Unicode form (APFS) finds the item under either spelling.
    let present: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|item| item.unwrap().file_name().into_string().ok())
        .filter(|name| *name == nfc || *name == nfd)
        .collect();
    assert_eq!(present.len(), 1, "exactly one spelling of {nfc:?} in {}: {:?}", dir.display(), names_in(dir));
    let to = if present[0] == nfc { nfd.clone() } else { nfc.to_string() };
    fs::rename(dir.join(&present[0]), dir.join(&to)).unwrap();
    to
}

/// `tree`, without Charon's own folders (`.charon`, `.charon-trash`).
fn user_tree(root: &Path) -> BTreeMap<String, String> {
    tree(root).into_iter().filter(|(path, _)| !path.starts_with(".charon")).collect()
}

/// `tree`, without the files tests write to make a folder watcher notice a change.
fn without_pokes(tree: BTreeMap<String, String>) -> BTreeMap<String, String> {
    tree.into_iter().filter(|(path, _)| !path.rsplit('/').next().unwrap_or(path).starts_with("poke-")).collect()
}

/// The contents of every file called `name` in `root`'s .charon-trash (none when Charon
/// never made a trash there).
fn trashed(root: &Path, name: &str) -> Vec<String> {
    let trash = root.join(".charon-trash");
    match fs::symlink_metadata(&trash) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => panic!("{} can't be looked at: {e}", trash.display()),
        Ok(_) => {}
    }
    tree(&trash)
        .into_iter()
        .filter(|(path, what)| path.rsplit('/').next() == Some(name) && what.starts_with("file: "))
        .map(|(_, what)| what.trim_start_matches("file: ").to_string())
        .collect()
}

/// The contents of every file in `folder` (not below it) whose name starts with `stem`.
fn versions(folder: &Path, stem: &str) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(folder)
        .unwrap()
        .flatten()
        .filter(|item| item.file_name().to_string_lossy().starts_with(stem) && item.path().is_file())
        .map(|item| read(&item.path()))
        .collect();
    found.sort();
    found
}

#[cfg(unix)]
fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
/// A folder made unreadable (or read-only) for as long as this lives.
struct Locked(PathBuf);

#[cfg(unix)]
impl Locked {
    fn new(path: &Path, mode: u32) -> Locked {
        chmod(path, mode);
        Locked(path.to_path_buf())
    }

    /// Only restores the folder's permissions at the end (something else locks it).
    fn later(path: &Path) -> Locked {
        Locked(path.to_path_buf())
    }
}

#[cfg(unix)]
impl Drop for Locked {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

#[cfg(unix)]
fn i_am_unprivileged() -> bool {
    // Root with DAC override reads a 000 folder: then the permission tests prove nothing.
    let tmp = tempfile::tempdir().unwrap();
    let probe = tmp.path().join("x");
    fs::create_dir(&probe).unwrap();
    chmod(&probe, 0o000);
    let blocked = fs::read_dir(&probe).is_err();
    chmod(&probe, 0o755);
    blocked
}

#[cfg(unix)]
/// Whether file permissions bite in this process. If not, the test can't make a folder
/// unreadable: it says so where everyone sees it and skips, or fails when
/// CHARON_REQUIRE_PERMISSION_TESTS=1.
fn permissions_bite(test: &str) -> bool {
    if i_am_unprivileged() {
        return true;
    }
    let how = "capsh --drop=cap_dac_override,cap_dac_read_search -- -c \"cargo test -p charon-core --test safety_regressions\"";
    let why = "this process can read folders whose permissions are 000 (root with CAP_DAC_OVERRIDE), so it can't make a \
               folder unreadable and the test would prove nothing";
    if std::env::var("CHARON_REQUIRE_PERMISSION_TESTS").is_ok_and(|v| v == "1") {
        panic!("{test} can't run: {why}. CHARON_REQUIRE_PERMISSION_TESTS=1 forbids skipping it. Run it with: {how}");
    }
    // Straight to stderr: the test harness swallows println!/eprintln! of a test that
    // passes, and a skipped safety test must never look like one that ran.
    let _ = writeln!(
        std::io::stderr(),
        "SKIPPED {test}: {why}. It did NOT run. Run it with: {how} (or set CHARON_REQUIRE_PERMISSION_TESTS=1 to fail instead of skipping)"
    );
    false
}

/// A folder bound at another place in the tree (`mount --bind`): the same drive, but another
/// mount, which a rename never crosses. Unmounted when dropped.
#[cfg(target_os = "linux")]
struct BindMount(std::ffi::CString);

#[cfg(target_os = "linux")]
impl BindMount {
    /// Binds `folder` at `at`. Mounting takes special rights (CAP_SYS_ADMIN): without them
    /// the test says so where everyone sees it and skips, or fails when
    /// CHARON_REQUIRE_PERMISSION_TESTS=1.
    fn new(test: &str, folder: &Path, at: &Path) -> Option<BindMount> {
        use std::os::unix::ffi::OsStrExt;
        let (from, to) = (std::ffi::CString::new(folder.as_os_str().as_bytes()).unwrap(), std::ffi::CString::new(at.as_os_str().as_bytes()).unwrap());
        // SAFETY: both paths are NUL-terminated and outlive the call; a bind mount reads
        // neither a drive type nor options.
        if unsafe { libc::mount(from.as_ptr(), to.as_ptr(), std::ptr::null(), libc::MS_BIND, std::ptr::null()) } == 0 {
            return Some(BindMount(to));
        }
        let why = format!("a folder can't be mounted in this process ({})", std::io::Error::last_os_error());
        let how = "as root: cargo test -p charon-core --test safety_regressions";
        if std::env::var("CHARON_REQUIRE_PERMISSION_TESTS").is_ok_and(|v| v == "1") {
            panic!("{test} can't run: {why}. CHARON_REQUIRE_PERMISSION_TESTS=1 forbids skipping it. Run it {how}");
        }
        // Straight to stderr: the test harness swallows output of a test that passes, and a
        // skipped safety test must never look like one that ran.
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED {test}: {why}. It did NOT run. Run it {how} (or set CHARON_REQUIRE_PERMISSION_TESTS=1 to fail instead of skipping)"
        );
        None
    }
}

#[cfg(target_os = "linux")]
impl Drop for BindMount {
    fn drop(&mut self) {
        // SAFETY: the path is NUL-terminated.
        unsafe { libc::umount2(self.0.as_ptr(), libc::MNT_DETACH) };
    }
}

fn has_note(review: &Review, kind: NoteKind, side: Side, path: &str) -> bool {
    review.notes.iter().any(|note| note.kind == kind && note.side == Some(side) && note.path == path)
}

/// Writes a new file into each folder, then lets the folders be quiet (on-change jobs wait
/// until changes settle, here 1 second), and again every 4 seconds, until `done`. A folder
/// watcher only hears about changes made after it started watching, so the first pokes may
/// go unheard. At the deadline the test fails, showing the rows of `jobs`.
fn poke_until(folders: &[PathBuf], what: &str, engine: &Engine, jobs: &[&String], mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    let mut n = 0;
    let mut next_poke = Instant::now();
    while !done() {
        if Instant::now() >= deadline {
            let rows: Vec<String> = jobs.iter().map(|id| describe(engine, id)).collect();
            panic!("timed out waiting: {what}\n{}", rows.join("\n"));
        }
        if Instant::now() >= next_poke {
            for folder in folders {
                write(&folder.join(format!("poke-{n}.txt")), "poke");
            }
            n += 1;
            next_poke = Instant::now() + Duration::from_secs(4);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Moves every job's schedule two days into the past (created, last run, last scheduled), so
/// interval and daily jobs are due at the autopilot's first look. The engine must be closed.
fn backdate_jobs(data: &Path) {
    let path = data.join("charon-state.json");
    let mut state: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let past = serde_json::json!((chrono::Utc::now() - chrono::Duration::days(2)).to_rfc3339());
    for job in state["jobs"].as_array_mut().expect("jobs in the state file") {
        job["createdAt"] = past.clone();
        job["lastScheduled"] = serde_json::Value::Null;
        if job["lastRun"].is_object() {
            job["lastRun"]["startedAt"] = past.clone();
            job["lastRun"]["finishedAt"] = past.clone();
        }
    }
    fs::write(&path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
}

/// Writes into the state file that a run of job `id` was carrying files when Charon stopped
/// (the computer lost power mid-run). The engine must be closed.
fn mark_run_in_progress(data: &Path, id: &String) {
    let path = data.join("charon-state.json");
    let mut state: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let jobs = state["jobs"].as_array_mut().expect("jobs in the state file");
    let job = jobs.iter_mut().find(|job| job["id"] == serde_json::json!(id)).expect("the job in the state file");
    assert_eq!(job.get("runInProgress"), Some(&serde_json::json!(false)), "the state file has the field, and the last run finished");
    job["runInProgress"] = serde_json::json!(true);
    fs::write(&path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
}

/// How many changes the job's row says wait for an OK (0 when none).
fn items_waiting(engine: &Engine, id: &String) -> u64 {
    view(engine, id).waiting.map_or(0, |waiting| waiting.items)
}

/// Asks for a look at the job now (as the autopilot does) and waits until its row shows what
/// waits; returns how many items that is. The job's row must show nothing counted as waiting
/// before.
/// From the moment the look starts, the autopilot's regular look isn't due for this job for
/// many minutes (`check_due`), so any later look can only come from a trigger.
fn look_now(engine: &Engine, id: &String) -> u64 {
    // Changes that waited before a restart still show, but uncounted until a look counts them.
    assert!(view(engine, id).waiting.is_none_or(|waiting| !waiting.counted), "no look of this setup yet: {}", describe(engine, id));
    engine.check(id);
    assert!(!engine.check_due(id, Duration::from_secs(60)), "the look started");
    eventually("the look finished", || view(engine, id).waiting.is_some_and(|waiting| waiting.counted));
    items_waiting(engine, id)
}

// ==================================================================== 1. the 0.2 probes, turned around

// ------------------------------------------------------------------ probe 1: scan errors (mirror)

#[test]
#[cfg(unix)] // folder permissions and symbolic links work differently on Windows
fn an_unreadable_source_folder_never_empties_its_mirror() {
    if !permissions_bite("an_unreadable_source_folder_never_empties_its_mirror") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("keep/a.txt"), "a");
    write(&src.join("projects/b.txt"), "b");
    write(&src.join("projects/c.txt"), "c");
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&src, &dst, SyncMode::Mirror));
    confirm(&engine, &id);
    assert!(dst.join("projects/b.txt").exists());

    let locked = Locked::new(&src.join("projects"), 0o000); // the folder can't be listed for a while
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.removed, 0, "an unreadable folder is not an empty one");
    assert_eq!(review.tally.destination.removed_folders, 0);
    assert!(has_note(&review, NoteKind::Unreadable, Side::Source, "projects"), "{:?}", review.notes);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    drop(locked);

    assert_eq!(read(&dst.join("projects/b.txt")), "b");
    assert_eq!(read(&dst.join("projects/c.txt")), "c");
    assert!(trashed(&dst, "b.txt").is_empty() && trashed(&dst, "c.txt").is_empty());
    // Readable again: still nothing to remove.
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.removed, 0);
    assert_eq!(review.total_steps, 0, "{:?}", review.steps);
}

// ------------------------------------------------------------------ probe 2: scan errors (two-way)

#[test]
#[cfg(unix)] // folder permissions and symbolic links work differently on Windows
fn a_two_way_folder_unreadable_for_a_moment_never_causes_removals() {
    if !permissions_bite("a_two_way_folder_unreadable_for_a_moment_never_causes_removals") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("docs/report.txt"), "the only copy of my report");
    write(&a.join("docs/notes.txt"), "notes");
    write(&a.join("other.txt"), "other");
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let id = add(engine, job_setup(&a, &b, SyncMode::TwoWay));
    confirm(engine, &id);
    assert!(b.join("docs/report.txt").exists());

    // Run 2: a/docs can't be listed while the folders are looked at (a network hiccup, a
    // permission glitch), and is readable again by the time the run carries files.
    let locked = Locked::new(&a.join("docs"), 0o000);
    write(&a.join("new.txt"), "something to carry");
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.source.removed, review.tally.destination.removed), (0, 0), "{:?}", review.steps);
    assert!(has_note(&review, NoteKind::Unreadable, Side::Source, "docs"), "{:?}", review.notes);
    let docs = a.join("docs");
    rig.at_first_step(move || chmod(&docs, 0o755));
    let_progress_throttle_pass();
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(engine, &id);
    drop(locked);
    assert!(rig.hook_fired(), "the folder became readable during the run");
    assert_eq!(read(&b.join("docs/report.txt")), "the only copy of my report");
    assert_eq!(read(&a.join("docs/report.txt")), "the only copy of my report");

    // Run 3: everything is normal again; nothing was deleted on either side, and nothing is.
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.source.removed, review.tally.destination.removed), (0, 0), "{:?}", review.steps);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(engine, &id);
    for side in [&a, &b] {
        assert_eq!(read(&side.join("docs/report.txt")), "the only copy of my report");
        assert_eq!(read(&side.join("docs/notes.txt")), "notes");
        assert!(trashed(side, "report.txt").is_empty() && trashed(side, "notes.txt").is_empty());
    }
}

#[test]
#[cfg(unix)] // folder permissions and symbolic links work differently on Windows
fn a_two_way_destination_folder_unreadable_for_a_while_never_causes_removals_in_the_source() {
    if !permissions_bite("a_two_way_destination_folder_unreadable_for_a_while_never_causes_removals_in_the_source") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("docs/report.txt"), "report");
    write(&a.join("docs/deep/plan.txt"), "plan");
    write(&a.join("other.txt"), "other");
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&a, &b, SyncMode::TwoWay));
    confirm(&engine, &id);

    let locked = Locked::new(&b.join("docs"), 0o000);
    write(&b.join("from-b.txt"), "new on b");
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.source.removed, review.tally.destination.removed), (0, 0), "{:?}", review.steps);
    assert!(has_note(&review, NoteKind::Unreadable, Side::Destination, "docs"), "{:?}", review.notes);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    drop(locked);
    assert!(a.join("from-b.txt").exists(), "the readable part was synced");

    // Readable again: the next look removes nothing either.
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.source.removed, review.tally.destination.removed), (0, 0), "{:?}", review.steps);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    for side in [&a, &b] {
        assert_eq!(read(&side.join("docs/report.txt")), "report");
        assert_eq!(read(&side.join("docs/deep/plan.txt")), "plan");
    }
    assert!(trashed(&a, "report.txt").is_empty() && trashed(&a, "plan.txt").is_empty());
}

#[test]
#[cfg(unix)] // folder permissions and symbolic links work differently on Windows
fn an_unreadable_job_folder_stops_the_look_and_removes_nothing() {
    if !permissions_bite("an_unreadable_job_folder_stops_the_look_and_removes_nothing") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    for n in 0..5 {
        write(&a.join(format!("f{n}.txt")), "mine");
    }
    let engine = plain_engine(&data);
    let mut setup = job_setup(&a, &b, SyncMode::TwoWay);
    setup.full_automation = true;
    let id = add(&engine, setup);
    confirm(&engine, &id);
    let before = user_tree(&a);

    let locked = Locked::new(&b, 0o000);
    match engine.prepare_review(&id) {
        Err(Error::Unreadable { path, .. }) => assert_eq!(path, b, "the error names the folder that can't be read"),
        other => panic!("a list of an unreadable folder can't be made, and says why: {:?}", other.map(|r| r.total_steps)),
    }
    // A run nobody looks at fails before it touches anything.
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait_idle(&engine, &id);
    drop(locked);
    assert!(view(&engine, &id).job.last_run.unwrap().error.is_some());
    assert_eq!(user_tree(&a), before, "nothing changed in the source");
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.source.removed, review.tally.destination.removed), (0, 0), "{:?}", review.steps);
}

#[test]
#[cfg(unix)] // folder permissions and symbolic links work differently on Windows
fn a_folder_that_turns_unreadable_during_a_run_stops_removals_on_the_other_side() {
    if !permissions_bite("a_folder_that_turns_unreadable_during_a_run_stops_removals_on_the_other_side") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("docs/x.txt"), "x");
    write(&a.join("docs/y.txt"), "y");
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let id = add(engine, job_setup(&a, &b, SyncMode::TwoWay));
    confirm(engine, &id);

    // Removed on a: the list removes b's copy. While the run goes, a/docs can't be read
    // anymore, so Charon can't make sure x.txt is still gone there.
    fs::remove_file(a.join("docs/x.txt")).unwrap();
    write(&a.join("aaa.txt"), "carried first");
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.removed, 1);
    let restore = Locked::later(&a.join("docs"));
    let docs = a.join("docs");
    rig.at_first_step(move || chmod(&docs, 0o000));
    let_progress_throttle_pass();
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(engine, &id);
    drop(restore);
    assert!(rig.hook_fired());
    assert_eq!(read(&b.join("docs/x.txt")), "x", "not removed while the other side can't be checked");
    assert!(trashed(&b, "x.txt").is_empty());
    // The copy went through, so the removal was kept back only because a/docs can't be read.
    assert_eq!(read(&b.join("aaa.txt")), "carried first");
    let run = view(engine, &id).job.last_run.unwrap();
    assert_eq!(run.failed + run.skipped, 1, "the one step that didn't happen (the removal) is reported: {run:?}");
    // The removal is still known and comes back in the next list, for the person to confirm.
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.removed, 1, "{:?}", review.steps);
    assert_eq!(read(&b.join("docs/y.txt")), "y");
}

// ------------------------------------------------------------------ probes 3 and 4: a list isn't a gate

/// A new job (full automation, never confirmed) with `trigger` and its first list open, a
/// control job with the same trigger that runs by itself, and a witness that proves the
/// autopilot looked several times. The new job must not sync: before its first OK it never
/// runs by itself (whether or not its list is open; see
/// `the_autopilot_never_syncs_a_job_whose_list_is_open` for a job that does run by itself).
/// Returns its id, the engine and its list.
fn new_job_under_the_autopilot(trigger: Trigger) -> (tempfile::TempDir, Engine, String, Review, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let (src, dst, data) = (root.join("src"), root.join("dst"), root.join("data"));
    let (c_src, c_dst) = (root.join("control-src"), root.join("control-dst"));
    let (w_src, w_dst) = (root.join("witness-src"), root.join("witness-dst"));
    write(&src.join("photo.jpg"), "photo");
    write(&dst.join("important-only-here.txt"), "precious");
    write(&c_src.join("first.txt"), "first");
    write(&w_src.join("first.txt"), "first");
    let engine = plain_engine(&data);

    // "Save and look": the new job, with full automation (both warnings accepted).
    let mut setup = job_setup(&src, &dst, SyncMode::Mirror);
    setup.trigger = trigger;
    setup.full_automation = true;
    let id = add(&engine, setup);
    let mut control = job_setup(&c_src, &c_dst, SyncMode::Backup);
    control.trigger = trigger;
    control.full_automation = true;
    let control = add(&engine, control);
    let mut witness = job_setup(&w_src, &w_dst, SyncMode::Backup);
    witness.trigger = Trigger::OnChange { settle_seconds: 1 };
    witness.full_automation = true;
    let witness = add(&engine, witness);
    confirm(&engine, &control);
    confirm(&engine, &witness);
    write(&c_src.join("new.txt"), "carried by itself");

    let review = engine.prepare_review(&id).unwrap(); // the window shows this list
    assert_eq!(review.tally.destination.removed, 1);
    let before = tree(&dst);
    Autopilot::start(engine.clone());

    let mut pokes = vec![w_src.clone()];
    if trigger.syncs_on_change() {
        pokes.extend([src.clone(), c_src.clone()]);
    }
    poke_until(&pokes, "the control job and the witness ran by themselves", &engine, &[&id, &control, &witness], || {
        ran_by_itself(&engine, &control) && ran_by_itself(&engine, &witness)
    });
    assert!(c_dst.join("new.txt").exists(), "the trigger fired for the control job");
    wait_idle(&engine, &id);
    let job = view(&engine, &id);
    assert!(job.job.last_run.is_none(), "the new job never synced: {:?}", job.job.last_run);
    assert!(job.job.needs_review && !job.runs_by_itself);
    assert_eq!(read(&dst.join("important-only-here.txt")), "precious");
    assert!(!dst.join("photo.jpg").exists());
    assert_eq!(tree(&dst), before, "nothing changed in the destination");
    (tmp, engine, id, review, dst)
}

#[test]
fn a_new_drive_connected_job_never_syncs_by_itself_before_its_first_ok() {
    let (_tmp, engine, _id, _review, _dst) = new_job_under_the_autopilot(Trigger::DriveConnected);
    engine.shut_down(Duration::from_secs(10));
}

#[test]
fn a_new_on_change_job_never_syncs_by_itself_before_its_first_ok() {
    let (_tmp, engine, id, review, _dst) = new_job_under_the_autopilot(Trigger::OnChange { settle_seconds: 1 });
    // The folder watcher saw the changes made after the list was made: it can't be confirmed.
    assert!(
        matches!(engine.approve(&id, &review.review_id, true), Err(Error::ReviewOutdated(_))),
        "a list made before the folders changed can't be confirmed"
    );
    engine.shut_down(Duration::from_secs(10));
}

/// Two mirror jobs with full automation that already run by themselves (each had its
/// confirmed sync), both started when their drive is connected (so the autopilot's first
/// look syncs them), each with changes waiting: a new file to copy and a file only the
/// destination has. The job under test is first in the list, so in every round the
/// autopilot decides about it before it starts the control job: once the control job ran
/// by itself, the job's fate in that round is sealed.
struct DriveJobs {
    _tmp: tempfile::TempDir,
    engine: Engine,
    id: String,
    control: String,
    dst: PathBuf,
    c_dst: PathBuf,
}

impl DriveJobs {
    fn new() -> DriveJobs {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let (src, dst, c_src, c_dst, data) =
            (root.join("src"), root.join("dst"), root.join("control-src"), root.join("control-dst"), root.join("data"));
        write(&src.join("first.txt"), "first");
        write(&c_src.join("first.txt"), "first");
        let engine = plain_engine(&data);
        let drive_job = |src: &Path, dst: &Path| {
            let mut setup = job_setup(src, dst, SyncMode::Mirror);
            setup.trigger = Trigger::DriveConnected;
            setup.full_automation = true;
            setup
        };
        let id = add(&engine, drive_job(&src, &dst));
        let control = add(&engine, drive_job(&c_src, &c_dst));
        confirm(&engine, &id);
        confirm(&engine, &control);
        assert_eq!(engine.job_list().iter().map(|job| job.id.clone()).collect::<Vec<_>>(), vec![id.clone(), control.clone()]);
        write(&src.join("new.txt"), "new");
        write(&dst.join("precious.txt"), "only here");
        write(&c_src.join("new.txt"), "carried by itself");
        DriveJobs { _tmp: tmp, engine, id, control, dst, c_dst }
    }

    /// Starts the autopilot and waits until the control job ran by itself.
    fn let_the_autopilot_go_round(&self) {
        Autopilot::start(self.engine.clone());
        eventually("the control job ran by itself because its drive was there", || ran_by_itself(&self.engine, &self.control));
        assert_eq!(read(&self.c_dst.join("new.txt")), "carried by itself", "the trigger fired");
        wait_idle(&self.engine, &self.id);
    }
}

#[test]
fn the_autopilot_never_syncs_a_job_whose_list_is_open() {
    let jobs = DriveJobs::new();
    let engine = &jobs.engine;
    let review = engine.prepare_review(&jobs.id).unwrap(); // the window shows this list
    assert_eq!(review.tally.destination.removed, 1);
    let job = view(engine, &jobs.id);
    assert!(job.runs_by_itself, "the job runs by itself: only its open list holds it back");
    let (last_run, before) = (job.job.last_run, tree(&jobs.dst));

    jobs.let_the_autopilot_go_round();
    assert_eq!(view(engine, &jobs.id).job.last_run, last_run, "the job didn't sync while its list was open");
    assert_eq!(tree(&jobs.dst), before, "nothing changed in its destination");
    assert_eq!(read(&jobs.dst.join("precious.txt")), "only here");
    engine.shut_down(Duration::from_secs(10));
}

#[test]
fn the_autopilot_never_syncs_a_paused_job() {
    let jobs = DriveJobs::new();
    let engine = &jobs.engine;
    engine.set_paused(&jobs.id, true).unwrap();
    let job = view(engine, &jobs.id);
    assert!(job.runs_by_itself, "the job runs by itself: only the pause holds it back");
    assert!(job.pending.is_none(), "not looked at yet: {}", describe(engine, &jobs.id));
    let (last_run, before) = (job.job.last_run, tree(&jobs.dst));

    jobs.let_the_autopilot_go_round();
    // Paused jobs are still looked at: the row shows what would be carried.
    eventually("the autopilot looked at the paused job", || {
        view(engine, &jobs.id).pending.is_some_and(|pending| pending.items > 0) && !engine.is_running(&jobs.id)
    });
    assert_eq!(view(engine, &jobs.id).job.last_run, last_run, "the paused job didn't sync");
    assert_eq!(tree(&jobs.dst), before, "nothing changed in its destination");
    engine.shut_down(Duration::from_secs(10));
}

#[test]
fn pause_everything_stops_every_automatic_sync() {
    let jobs = DriveJobs::new();
    let engine = &jobs.engine;
    engine.set_all_paused(true).unwrap();
    let ids = [&jobs.id, &jobs.control];
    let last_runs: Vec<_> = ids.iter().map(|id| view(engine, id).job.last_run).collect();
    let before = (tree(&jobs.dst), tree(&jobs.c_dst));
    for id in ids {
        assert!(view(engine, id).runs_by_itself && view(engine, id).pending.is_none(), "{}", describe(engine, id));
    }

    Autopilot::start(engine.clone());
    // Everything is still looked at, so the rows show what would be carried: that is the
    // proof the autopilot went round (the drive-connected trigger fired for both jobs).
    for id in ids {
        eventually("the autopilot looked at the job", || {
            view(engine, id).pending.is_some_and(|pending| pending.items > 0) && !engine.is_running(id)
        });
    }
    let now: Vec<_> = ids.iter().map(|id| view(engine, id).job.last_run).collect();
    assert_eq!(now, last_runs, "no job synced");
    assert_eq!((tree(&jobs.dst), tree(&jobs.c_dst)), before, "nothing changed");
    assert!(!jobs.c_dst.join("new.txt").exists());
    engine.shut_down(Duration::from_secs(10));
}

#[test]
fn pause_everything_stops_an_automatic_sync_that_is_still_looking() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("first.txt"), "first");
    // Everything is paused the moment the run's look has counted its first items (it says
    // so every few hundred).
    let pause: Arc<Mutex<Option<Hook>>> = Arc::default();
    let paused = Arc::new(AtomicBool::new(false));
    let (slot, flag) = (pause.clone(), paused.clone());
    let emitter = Arc::new(move |event: EngineEvent| {
        if matches!(event, EngineEvent::ScanProgress { .. }) {
            // Taken out first: pausing sends events of its own.
            let armed = slot.lock().unwrap().take();
            if let Some(pause) = armed {
                pause();
                flag.store(true, Ordering::SeqCst);
            }
        }
    });
    let engine = Engine::open(&data, emitter).expect("the engine opens");
    let mut setup = job_setup(&src, &dst, SyncMode::Mirror);
    setup.full_automation = true;
    let id = add(&engine, setup);
    confirm(&engine, &id);
    for n in 0..600 {
        write(&src.join(format!("new-{n}.txt")), "new");
    }
    write(&dst.join("precious.txt"), "only here");
    let (before, last_run) = (tree(&dst), view(&engine, &id).job.last_run);
    let pauser = engine.clone();
    *pause.lock().unwrap() = Some(Box::new(move || pauser.set_all_paused(true).unwrap()));

    engine.want(&id, "after changes");
    engine.start_wanted(&id);
    wait_idle(&engine, &id);
    assert!(paused.load(Ordering::SeqCst), "everything was paused while the run looked");
    let job = view(&engine, &id);
    let run = job.job.last_run.clone().expect("the run ended");
    assert_ne!(Some(&run), last_run.as_ref(), "the run began before the pause");
    assert!(run.cancelled && !run.approved, "the run stopped: {run:?}");
    assert_eq!(tree(&dst), before, "nothing changed in the destination");
    assert!(job.job.needs_review && !job.runs_by_itself, "the next sync waits for an OK");
    let log = engine.log(&id, 20);
    assert!(log.iter().any(|line| line.text == "everything paused: this sync stops, since you didn't confirm its list"), "{log:?}");
    engine.shut_down(Duration::from_secs(10));
}

#[test]
fn a_trigger_never_starts_a_sync_once_the_job_or_everything_is_paused() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("first.txt"), "first");
    let engine = plain_engine(&data);
    let mut setup = job_setup(&src, &dst, SyncMode::Backup);
    setup.full_automation = true;
    let id = add(&engine, setup);
    confirm(&engine, &id);
    write(&src.join("new.txt"), "new");
    let last_run = view(&engine, &id).job.last_run;

    // The autopilot reads the jobs once a tick: a trigger it saw before the pause still
    // asks for a run after it, which doesn't begin.
    for everything in [false, true] {
        if everything {
            engine.set_all_paused(true).unwrap();
        } else {
            engine.set_paused(&id, true).unwrap();
        }
        engine.want(&id, "after changes");
        engine.start_wanted(&id);
        assert!(!engine.is_running(&id), "no sync began (everything paused: {everything})");
        assert_eq!(view(&engine, &id).job.last_run, last_run);
        assert!(!dst.join("new.txt").exists());
        engine.set_paused(&id, false).unwrap();
        engine.set_all_paused(false).unwrap();
    }

    // Sync now still starts it, paused or not: the person asked for it.
    engine.set_paused(&id, true).unwrap();
    engine.set_all_paused(true).unwrap();
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait_idle(&engine, &id);
    assert_eq!(read(&dst.join("new.txt")), "new");
    engine.shut_down(Duration::from_secs(10));
}

// ------------------------------------------------------------------ probe 5: editing a running job

#[test]
fn a_job_cannot_be_edited_while_it_syncs_and_an_edit_afterwards_removes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, c, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("c"), tmp.path().join("data"));
    for n in 1..=5 {
        write(&a.join(format!("f{n}.txt")), "mine");
    }
    write(&c.join("f1.txt"), "mine"); // the other drive has an older, partial copy
    let rig = Rig::open(&data);
    let engine = rig.engine.clone();
    let job = engine.add_job(job_setup(&a, &b, SyncMode::TwoWay)).unwrap();
    let id = job.job.id.clone();
    confirm(&engine, &id);

    // A new file makes the next run carry something; while it does, the person edits the job
    // and points it at drive C.
    write(&a.join("new.txt"), "new");
    let mut moved = job.job.setup.clone();
    moved.destination = c.clone();
    let answer: Arc<Mutex<Option<bool>>> = Arc::default();
    let (editor, edit, told) = (engine.clone(), moved.clone(), answer.clone());
    let edited_id = id.clone();
    rig.at_first_step(move || {
        *told.lock().unwrap() = Some(editor.update_job(&edited_id, edit).is_ok());
    });
    let review = engine.prepare_review(&id).unwrap();
    let_progress_throttle_pass();
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    assert!(rig.hook_fired());
    assert_eq!(*answer.lock().unwrap(), Some(false), "a job can't be edited while it syncs");
    assert_eq!(view(&engine, &id).job.setup.destination, b);
    assert!(b.join("new.txt").exists());

    // Edited once the run is over: Charon forgets the last sync, and the next look removes nothing.
    engine.update_job(&id, moved).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.source.removed, review.tally.destination.removed), (0, 0), "{:?}", review.steps);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    for n in 1..=5 {
        assert!(a.join(format!("f{n}.txt")).exists(), "a/f{n}.txt is still there");
        assert!(c.join(format!("f{n}.txt")).exists());
    }
    assert!(a.join("new.txt").exists());
    assert!(!a.join(".charon-trash").exists(), "nothing was removed or replaced in the source");
}

// ------------------------------------------------------------------ probe 6: silent overwrite during a run

#[test]
fn an_edit_made_while_a_run_carries_files_is_never_overwritten() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("plan.txt"), "v1");
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let id = add(engine, job_setup(&a, &b, SyncMode::TwoWay));
    confirm(engine, &id);

    write(&a.join("plan.txt"), "v2 from a");
    write(&a.join("aaa.txt"), "carried first");
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.replaced, 1);
    let bp = b.join("plan.txt");
    rig.at_first_step(move || fs::write(&bp, "fresh edit typed on b while the sync ran").unwrap());
    let_progress_throttle_pass();
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(engine, &id);
    assert!(rig.hook_fired());
    assert_eq!(read(&b.join("plan.txt")), "fresh edit typed on b while the sync ran", "the fresh edit was not overwritten");
    assert_eq!(view(engine, &id).job.last_run.unwrap().skipped, 1);

    // The next sync sees two versions and keeps both, on both sides.
    confirm(engine, &id);
    let both = vec!["fresh edit typed on b while the sync ran".to_string(), "v2 from a".to_string()];
    assert_eq!(versions(&a, "plan"), both);
    assert_eq!(versions(&b, "plan"), both);
}

// ------------------------------------------------------------------ probe 7: excluded files count as clutter

#[test]
fn excluded_files_inside_a_folder_removed_on_the_other_side_are_never_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("Project/brief.txt"), "brief");
    write(&a.join("keep.txt"), "keep");
    let engine = plain_engine(&data);
    let mut setup = job_setup(&a, &b, SyncMode::TwoWay);
    setup.excludes.push("*.psd".into());
    setup.excludes.push("render-cache".into());
    setup.safe_delete = false;
    let id = add(&engine, setup);
    confirm(&engine, &id);
    write(&b.join("Project/artwork.psd"), "hours of work, never synced because excluded");
    write(&b.join("Project/sub/render-cache/frame1.exr"), "a whole night of rendering");

    fs::remove_dir_all(a.join("Project")).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    assert!(has_note(&review, NoteKind::KeptFolder, Side::Destination, "Project"), "the folder is kept, with a note: {:?}", review.notes);
    assert_eq!(review.tally.destination.removed_folders, 0, "{:?}", review.steps);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    assert!(!b.join("Project/brief.txt").exists(), "the synced file follows the removal");
    assert_eq!(read(&b.join("Project/artwork.psd")), "hours of work, never synced because excluded");
    assert_eq!(read(&b.join("Project/sub/render-cache/frame1.exr")), "a whole night of rendering");
}

#[test]
fn a_mirror_keeps_a_removed_folder_that_holds_excluded_files() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("Project/brief.txt"), "brief");
    write(&src.join("keep.txt"), "keep");
    let engine = plain_engine(&data);
    let mut setup = job_setup(&src, &dst, SyncMode::Mirror);
    setup.excludes.push("*.psd".into());
    setup.safe_delete = false;
    let id = add(&engine, setup);
    confirm(&engine, &id);
    write(&dst.join("Project/deep/artwork.psd"), "only here");

    fs::remove_dir_all(src.join("Project")).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    assert!(has_note(&review, NoteKind::KeptFolder, Side::Destination, "Project"), "{:?}", review.notes);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    assert!(!dst.join("Project/brief.txt").exists());
    assert_eq!(read(&dst.join("Project/deep/artwork.psd")), "only here");
}

// ------------------------------------------------------------------ probe 8: what isn't carried

#[test]
#[cfg(unix)] // folder permissions and symbolic links work differently on Windows
fn symbolic_links_are_carried_as_links_and_never_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data, outside) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"), tmp.path().join("outside"));
    write(&src.join("real/file.txt"), "x");
    write(&outside.join("secret.txt"), "outside the job");
    std::os::unix::fs::symlink("real", src.join("link-to-folder")).unwrap();
    std::os::unix::fs::symlink("real/file.txt", src.join("link-to-file")).unwrap();
    std::os::unix::fs::symlink(&outside, src.join("escape")).unwrap();
    let outside_before = tree(&outside);
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&src, &dst, SyncMode::Mirror));

    let review = engine.prepare_review(&id).unwrap();
    let links = engine.review_steps(&id, &review.review_id, StepFilter::New, "", 0, 100).unwrap();
    let mut linked: Vec<&str> = links.steps.iter().filter(|s| s.item == EntryKind::Link).map(|s| s.path.as_str()).collect();
    linked.sort();
    assert_eq!(linked, vec!["escape", "link-to-file", "link-to-folder"], "every link is in the list, none dropped");
    assert!(review.warnings.iter().any(|w| w.text.contains("symbolic link")), "the list says links are copied as links: {:?}", review.warnings);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);

    assert_eq!(fs::read_link(dst.join("link-to-folder")).unwrap(), PathBuf::from("real"));
    assert_eq!(fs::read_link(dst.join("link-to-file")).unwrap(), PathBuf::from("real/file.txt"));
    assert_eq!(fs::read_link(dst.join("escape")).unwrap(), outside);
    assert!(fs::symlink_metadata(dst.join("escape")).unwrap().file_type().is_symlink(), "not a copy of what it points to");
    assert!(view(&engine, &id).job.last_run.unwrap().skipped == 0);

    // The link goes away in the source: only the link is removed, never what it points to.
    fs::remove_file(src.join("escape")).unwrap();
    confirm(&engine, &id);
    assert!(fs::symlink_metadata(dst.join("escape")).is_err());
    assert_eq!(tree(&outside), outside_before, "nothing outside the job folder changed");
}

#[test]
#[cfg(unix)] // folder permissions and symbolic links work differently on Windows
fn a_link_in_the_destination_where_the_source_has_a_folder_is_replaced_without_touching_its_target() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data, outside) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"), tmp.path().join("outside"));
    write(&src.join("docs/file.txt"), "from the source");
    write(&outside.join("file.txt"), "outside the job");
    write(&outside.join("other.txt"), "outside too");
    fs::create_dir_all(&dst).unwrap();
    std::os::unix::fs::symlink(&outside, dst.join("docs")).unwrap();
    let outside_before = tree(&outside);
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&src, &dst, SyncMode::Mirror));
    confirm(&engine, &id);
    assert!(fs::symlink_metadata(dst.join("docs")).unwrap().is_dir(), "a real folder now");
    assert_eq!(read(&dst.join("docs/file.txt")), "from the source");
    assert_eq!(tree(&outside), outside_before, "nothing was written or removed through the link");
}

#[test]
#[cfg(unix)] // folder permissions and symbolic links work differently on Windows
fn a_folder_replaced_by_a_link_during_a_run_is_never_removed_through() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data, outside) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"), tmp.path().join("outside"));
    write(&src.join("sub/keep.txt"), "keep");
    write(&src.join("sub/old.txt"), "old");
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let id = add(engine, job_setup(&src, &dst, SyncMode::Mirror));
    confirm(engine, &id);

    fs::remove_file(src.join("sub/old.txt")).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.removed, 1);
    // While the run goes, dst/sub is moved elsewhere and a link to it is left in its place:
    // the very same file sits behind the link.
    let (sub, moved) = (dst.join("sub"), outside.clone());
    rig.at_first_step(move || {
        fs::rename(&sub, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &sub).unwrap();
    });
    let_progress_throttle_pass();
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(engine, &id);
    assert!(rig.hook_fired());
    assert_eq!(read(&outside.join("old.txt")), "old", "nothing is removed through a link");
    assert_eq!(read(&outside.join("keep.txt")), "keep");
}

#[test]
#[cfg(unix)] // folder permissions and symbolic links work differently on Windows
fn a_linked_trash_folder_is_never_used_and_nothing_is_lost_for_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data, outside) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"), tmp.path().join("outside"));
    write(&src.join("keep.txt"), "keep");
    write(&dst.join("keep.txt"), "keep");
    write(&dst.join("old.txt"), "only copy");
    // The link points at a folder that holds something of its own, which must stay as it is.
    write(&outside.join("sentinel.txt"), "outside the job");
    std::os::unix::fs::symlink(&outside, dst.join(".charon-trash")).unwrap();
    let outside_before = tree(&outside);
    assert_eq!(outside_before.len(), 1);
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&src, &dst, SyncMode::Mirror));
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.removed, 1);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    assert_eq!(read(&dst.join("old.txt")), "only copy", "a removal that can't go to the trash doesn't happen");
    assert_eq!(tree(&outside), outside_before, "nothing went through the link, and nothing there was touched");
    assert!(view(&engine, &id).job.last_run.unwrap().failed > 0, "the failure is reported");

    // A replaced file isn't lost either.
    fs::remove_file(dst.join("old.txt")).unwrap();
    write(&src.join("keep.txt"), "a newer, longer version");
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.replaced, 1);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    assert_eq!(read(&dst.join("keep.txt")), "keep", "the old version stays when it can't be kept in the trash");
    assert_eq!(tree(&outside), outside_before);
}

/// Two-way: a removal the trash can't take fails before the file is touched. Moved aside and
/// put back, the file would carry a new change time, and the next look would take Charon's own
/// renames for an edit on that side and copy the removed file back as new.
#[test]
#[cfg(unix)] // symbolic links work differently on Windows
fn a_two_way_removal_the_trash_cant_take_never_brings_the_file_back() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data, outside) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"), tmp.path().join("outside"));
    write(&a.join("keep.txt"), "keep");
    write(&a.join("photo.jpg"), "the photo");
    fs::create_dir_all(&outside).unwrap();
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&a, &b, SyncMode::TwoWay));
    confirm(&engine, &id);
    assert_eq!(read(&b.join("photo.jpg")), "the photo");

    std::os::unix::fs::symlink(&outside, b.join(".charon-trash")).unwrap();
    fs::remove_file(a.join("photo.jpg")).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.destination.removed, review.total_steps), (1, 1), "{:?}", review.steps);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    assert!(view(&engine, &id).job.last_run.unwrap().failed > 0, "the failure is reported");
    assert_eq!(read(&b.join("photo.jpg")), "the photo", "a removal that can't go to the trash doesn't happen");
    assert!(!a.join("photo.jpg").exists());

    let review = engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.destination.removed, review.total_steps), (1, 1), "still the same removal, nothing copied back: {:?}", review.steps);
}

/// A folder mounted inside the job's folder (here a folder of the same drive, bound there)
/// is another mount than the trash at the top of the job's folder, and nothing moves from
/// one mount to another in one step. Whatever goes into the trash from it is copied there
/// and then removed, so no step fails on every sync: a removed folder with nothing but
/// Finder's .DS_Store left in it, a removed link, a replaced link, and a leftover of an
/// interrupted sync, which the list says it moves into the trash.
#[test]
#[cfg(target_os = "linux")]
fn whatever_goes_into_the_trash_from_a_folder_mounted_inside_the_jobs_gets_there() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data, store) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"), tmp.path().join("store"));
    write(&a.join("keep.txt"), "keep");
    write(&a.join("photos/2019/x.jpg"), "the photo");
    write(&a.join("photos/2020/y.jpg"), "a newer photo");
    for link in ["photos/latest", "photos/newest"] {
        std::os::unix::fs::symlink("2019/x.jpg", a.join(link)).unwrap();
    }
    fs::create_dir_all(b.join("photos")).unwrap();
    fs::create_dir_all(&store).unwrap();
    let Some(_mounted) = BindMount::new("whatever_goes_into_the_trash_from_a_folder_mounted_inside_the_jobs_gets_there", &store, &b.join("photos")) else {
        return;
    };
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&a, &b, SyncMode::TwoWay));
    confirm(&engine, &id);
    assert_eq!(read(&store.join("2019/x.jpg")), "the photo", "the look went into the mounted folder");

    // Finder looked into photos/2019 over the network, and a sync two hours ago was cut off
    // with a link it had moved aside.
    write(&b.join("photos/2019/.DS_Store"), "Finder's view settings");
    let started = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() - 2 * 3600;
    let leftover = format!("photos/.charon-{started:08x}{:08x}.part", 0x1234u32);
    std::os::unix::fs::symlink("2019/x.jpg", b.join(&leftover)).unwrap();
    let long_ago = filetime::FileTime::from_unix_time(started as i64, 0);
    filetime::set_symlink_file_times(b.join(&leftover), long_ago, long_ago).unwrap();
    // On the source, photos/2019 and one link are removed, and the other link is pointed elsewhere.
    fs::remove_dir_all(a.join("photos/2019")).unwrap();
    fs::remove_file(a.join("photos/latest")).unwrap();
    fs::remove_file(a.join("photos/newest")).unwrap();
    std::os::unix::fs::symlink("2020/y.jpg", a.join("photos/newest")).unwrap();

    let review = engine.prepare_review(&id).unwrap();
    let tidies = "Also moves 1 item a sync left under a temporary name (most often a half-copied file from an interrupted sync) into .charon-trash on the destination.";
    assert!(review.housekeeping.iter().any(|line| line == tidies), "{:?}", review.housekeeping);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    let run = view(&engine, &id).job.last_run.unwrap();
    let log: Vec<String> = engine.log(&id, 8).into_iter().map(|line| line.text).collect();
    assert_eq!((run.failed, run.error), (0, None), "{log:#?}");
    assert!(!store.join("2019").exists());
    assert!(fs::symlink_metadata(store.join("latest")).is_err());
    assert_eq!(fs::read_link(store.join("newest")).unwrap(), Path::new("2020/y.jpg"));
    assert!(fs::symlink_metadata(b.join(&leftover)).is_err(), "the leftover is tidied away");
    // All of it is in the trash, as it was.
    let kept: BTreeMap<String, String> = tree(&b.join(".charon-trash"))
        .into_iter()
        .filter_map(|(path, what)| path.split_once('/').map(|(_, inside)| (inside.to_string(), what)))
        .filter(|(inside, what)| inside != ".charon" && what != "folder")
        .collect();
    let expected = [
        ("photos/2019/.DS_Store", "file: Finder's view settings"),
        ("photos/2019/x.jpg", "file: the photo"),
        (leftover.as_str(), "link -> 2019/x.jpg"),
        ("photos/latest", "link -> 2019/x.jpg"),
        ("photos/newest", "link -> 2019/x.jpg"),
    ];
    assert_eq!(kept, expected.iter().map(|(path, what)| (path.to_string(), what.to_string())).collect::<BTreeMap<_, _>>());

    let again = engine.prepare_review(&id).unwrap();
    assert_eq!(again.total_steps, 0, "nothing is left to do: {:?}", again.steps);
}

/// So does the old journal of a database in such a folder. The app wrote the journal into the
/// database in the source, so the database changed and its journal is gone. The old journal
/// is renamed aside while the new database takes its place, and it can't move from there into
/// the trash in one step either: it's copied there under its own name and then removed, in a
/// mirror and in a two-way job. Its step doesn't fail, and nothing is left in the mounted
/// folder under a temporary name.
#[test]
#[cfg(target_os = "linux")]
fn a_databases_old_journal_in_a_folder_mounted_inside_the_jobs_goes_into_the_trash() {
    for mode in [SyncMode::Mirror, SyncMode::TwoWay] {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b, data, store) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"), tmp.path().join("store"));
        write(&a.join("keep.txt"), "keep");
        write(&a.join("photos/app/data.db"), "the database");
        write(&a.join("photos/app/data.db-wal"), "changes not in the database yet");
        fs::create_dir_all(b.join("photos")).unwrap();
        fs::create_dir_all(&store).unwrap();
        let Some(_mounted) = BindMount::new("a_databases_old_journal_in_a_folder_mounted_inside_the_jobs_goes_into_the_trash", &store, &b.join("photos")) else {
            return;
        };
        let engine = plain_engine(&data);
        let id = add(&engine, job_setup(&a, &b, mode));
        confirm(&engine, &id);
        assert_eq!(read(&store.join("app/data.db-wal")), "changes not in the database yet", "{mode:?}: the sync went into the mounted folder");

        write(&a.join("photos/app/data.db"), "the database, with the changes written into it");
        fs::remove_file(a.join("photos/app/data.db-wal")).unwrap();
        let review = confirm(&engine, &id);
        assert_eq!((review.tally.destination.replaced, review.tally.destination.removed, review.total_steps), (1, 1, 2), "{mode:?}: {:?}", review.steps);
        let run = view(&engine, &id).job.last_run.unwrap();
        let log: Vec<String> = engine.log(&id, 8).into_iter().map(|line| line.text).collect();
        assert_eq!((run.failed, run.skipped), (0, 0), "{mode:?}: {log:#?}");
        let mounted = BTreeMap::from([("app".to_string(), "folder".to_string()), ("app/data.db".to_string(), "file: the database, with the changes written into it".to_string())]);
        assert_eq!(tree(&store), mounted, "{mode:?}: nothing is left under a temporary name");
        let kept: BTreeMap<String, String> = tree(&b.join(".charon-trash"))
            .into_iter()
            .filter_map(|(path, what)| path.split_once('/').map(|(_, inside)| (inside.to_string(), what)))
            .filter(|(inside, what)| inside != ".charon" && what != "folder")
            .collect();
        let expected = [("photos/app/data.db", "file: the database"), ("photos/app/data.db-wal", "file: changes not in the database yet")];
        assert_eq!(kept, expected.iter().map(|(path, what)| (path.to_string(), what.to_string())).collect::<BTreeMap<_, _>>(), "{mode:?}");

        let again = engine.prepare_review(&id).unwrap();
        assert_eq!((again.total_steps, again.housekeeping.len()), (0, 0), "{mode:?}: nothing is left to do: {:?} {:?}", again.steps, again.housekeeping);
    }
}

// ------------------------------------------------------------------ probe 9: the wipe guard is narrow

#[test]
fn a_mirror_from_an_almost_empty_source_needs_the_tick() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join(".localized"), "");
    for n in 0..200 {
        write(&dst.join(format!("photo-{n}.jpg")), "p");
    }
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&src, &dst, SyncMode::Mirror));
    let before = tree(&dst);
    let review = engine.prepare_review(&id).unwrap();
    assert!(review.blocked.is_none(), "not refused outright: {:?}", review.blocked);
    assert!(review.needs_tick, "removing 200 of 200 photos needs the tick");
    assert!(review.warnings.iter().any(|w| w.level == Level::Danger), "{:?}", review.warnings);
    match engine.approve(&id, &review.review_id, false) {
        Err(Error::Invalid(msg)) => assert!(msg.contains("Tick"), "refused for the missing tick: {msg}"),
        other => panic!("confirmed without the tick: {other:?}"),
    }
    assert!(!engine.is_running(&id));
    assert_eq!(tree(&dst), before, "nothing changed");
    // With the tick it goes ahead, and every photo is kept in the trash.
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    let trash = tree(&dst.join(".charon-trash"));
    for n in 0..200 {
        let name = format!("photo-{n}.jpg");
        assert!(!dst.join(&name).exists());
        let kept = trash.iter().filter(|(path, what)| path.rsplit('/').next() == Some(name.as_str()) && what.as_str() == "file: p");
        assert_eq!(kept.count(), 1, "{name} is in the trash");
    }
}

#[test]
fn full_automation_holds_back_a_mirror_whose_source_is_nearly_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join(".localized"), "");
    for n in 0..200 {
        write(&src.join(format!("photo-{n}.jpg")), "p");
    }
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let mut setup = job_setup(&src, &dst, SyncMode::Mirror);
    setup.full_automation = true;
    let id = add(engine, setup);
    confirm(engine, &id);

    // The photos library moved away; the drive left behind looks almost empty.
    for n in 0..200 {
        fs::remove_file(src.join(format!("photo-{n}.jpg"))).unwrap();
    }
    let before = tree(&dst);
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait_idle(engine, &id);
    let job = view(engine, &id);
    assert!(job.job.held.is_some() && job.job.needs_review, "the run was held for an OK");
    assert_eq!(job.status, JobStatus::Review);
    // The Held event is sent just after the job stops counting as running.
    eventually("the Held event arrived", || rig.held_events(&id) >= 1);
    assert_eq!(rig.held_events(&id), 1);
    assert_eq!(tree(&dst), before, "nothing was touched");
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Review, "the next sync waits for an OK");
}

// ------------------------------------------------------------------ probe 10: a failed save

#[test]
#[cfg(unix)] // folder permissions and symbolic links work differently on Windows
fn a_job_edit_that_cannot_be_saved_changes_nothing() {
    if !permissions_bite("a_job_edit_that_cannot_be_saved_changes_nothing") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, c, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("c"), tmp.path().join("data"));
    for n in 1..=5 {
        write(&a.join(format!("f{n}.txt")), "mine");
    }
    write(&c.join("f1.txt"), "mine");
    let engine = plain_engine(&data);
    let job = engine.add_job(job_setup(&a, &b, SyncMode::TwoWay)).unwrap();
    let id = job.job.id.clone();
    confirm(&engine, &id);

    let locked = Locked::new(&data, 0o555); // e.g. the disk is full: the state file can't be saved
    let mut moved = job.job.setup.clone();
    moved.destination = c.clone();
    let answer = engine.update_job(&id, moved);
    drop(locked);
    assert!(answer.is_err(), "the edit is refused when it can't be saved");
    assert_eq!(view(&engine, &id).job.setup.destination, b, "the job Charon works with is the saved one");
    assert!(read(&data.join("charon-state.json")).contains(&format!("{}\"", b.display())));

    write(&a.join("new.txt"), "new");
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.destination.path, b.display().to_string());
    assert_eq!((review.tally.source.removed, review.tally.destination.removed), (0, 0), "{:?}", review.steps);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    for n in 1..=5 {
        assert!(a.join(format!("f{n}.txt")).exists());
        assert!(b.join(format!("f{n}.txt")).exists());
    }
    assert_eq!(user_tree(&c).len(), 1, "drive C was never touched");
}

// ------------------------------------------------------------------ probe 11: one name, two spellings

#[test]
fn one_name_spelled_in_two_unicode_forms_is_one_item() {
    // HFS+ ("Mac OS Extended") drives store "é" decomposed (e + accent); APFS keeps what it
    // was given, often the composed "é". To the person they are the same file.
    let composed = "r\u{e9}sum\u{e9}.txt";
    let decomposed = "re\u{301}sume\u{301}.txt";
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    // A fresh edit on the APFS side meets an older copy listed under the other spelling.
    write(&a.join(composed), "the fresh edit, bigger and newer");
    write(&b.join(decomposed), "the old copy");
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&a, &b, SyncMode::TwoWay));
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.total_steps, 1, "one item, not two: {:?}", review.steps);
    assert_eq!((review.tally.source.removed, review.tally.destination.removed), (0, 0));
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);

    for side in [&a, &b] {
        let names: Vec<String> = fs::read_dir(side).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        let spellings = names.iter().filter(|n| n.nfc().collect::<String>() == composed).count();
        assert_eq!(spellings, 1, "no duplicate under the other spelling in {}: {names:?}", side.display());
        let mut kept = versions(side, "r");
        kept.sort();
        assert_eq!(kept, vec!["the fresh edit, bigger and newer".to_string(), "the old copy".to_string()], "both versions survive");
    }
    let next = engine.prepare_review(&id).unwrap();
    assert_eq!(next.total_steps, 0, "{:?}", next.steps);
}

#[test]
fn a_name_seen_in_the_other_unicode_form_is_neither_copied_again_nor_removed() {
    let composed = "caf\u{e9}/men\u{fc}.txt";
    let decomposed_file = "cafe\u{301}/menu\u{308}.txt";
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, a, b, data) =
        (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&src.join(decomposed_file), "dinner");
    write(&a.join(composed), "lunch");
    let engine = plain_engine(&data);
    let mirror = add(&engine, job_setup(&src, &dst, SyncMode::Mirror));
    let two_way = add(&engine, job_setup(&a, &b, SyncMode::TwoWay));
    confirm(&engine, &mirror);
    confirm(&engine, &two_way);

    // The copy landed under one spelling; the next look must not find anything to remove.
    let next = engine.prepare_review(&mirror).unwrap();
    assert_eq!(next.total_steps, 0, "{:?}", next.steps);
    // A drive that hands the names back in the other form (as HFS+ does) changes nothing
    // either: the folder and the file in each copy now carry the other spelling.
    for root in [&dst, &b] {
        let folder = respell(root, "caf\u{e9}");
        respell(&root.join(folder), "men\u{fc}.txt");
    }
    for id in [&mirror, &two_way] {
        let review = engine.prepare_review(id).unwrap();
        assert_eq!(review.total_steps, 0, "{:?}", review.steps);
        engine.approve(id, &review.review_id, true).unwrap();
        wait_idle(&engine, id);
    }
    assert_eq!(user_tree(&dst).values().filter(|v| v.as_str() == "file: dinner").count(), 1);
    assert_eq!(user_tree(&b).values().filter(|v| v.as_str() == "file: lunch").count(), 1);
    assert_eq!(user_tree(&a).values().filter(|v| v.as_str() == "file: lunch").count(), 1);
    assert!(!dst.join(".charon-trash").exists() && !a.join(".charon-trash").exists() && !b.join(".charon-trash").exists());
}

// ------------------------------------------------------------------ probe 12: edits run without a look

#[test]
fn an_edited_job_with_full_automation_waits_for_an_ok_before_it_syncs_again() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (src, dst, data) = (root.join("src"), root.join("dst"), root.join("data"));
    let (w_src, w_dst) = (root.join("witness-src"), root.join("witness-dst"));
    write(&src.join("today.txt"), "work from today");
    write(&w_src.join("first.txt"), "first");
    let engine = plain_engine(&data);
    let mut setup = job_setup(&src, &dst, SyncMode::Mirror);
    setup.trigger = Trigger::OnChange { settle_seconds: 1 };
    setup.full_automation = true;
    let job = engine.add_job(setup.clone()).unwrap();
    let id = job.job.id.clone();
    let mut witness = job_setup(&w_src, &w_dst, SyncMode::Backup);
    witness.trigger = Trigger::OnChange { settle_seconds: 1 };
    witness.full_automation = true;
    let witness = add(&engine, witness);
    confirm(&engine, &id);
    confirm(&engine, &witness);
    assert!(view(&engine, &id).runs_by_itself);
    write(&src.join("newer.txt"), "written after the last sync");

    // The person opens Edit, clicks the swap arrow by mistake, saves.
    let mut swapped = setup.clone();
    swapped.source = dst.clone();
    swapped.destination = src.clone();
    engine.update_job(&id, swapped).unwrap();
    let edited = view(&engine, &id);
    assert!(edited.job.needs_review && !edited.runs_by_itself, "the edit makes the job wait for an OK");
    let last_run = edited.job.last_run;
    let (src_before, dst_before) = (tree(&src), tree(&dst));
    // What the edited job's row shows before the autopilot starts: the edit looks at the new
    // setup by itself (it may be done already), and the autopilot's regular look isn't due
    // again for many minutes after it.
    eventually("the edit's look", || view(&engine, &id).waiting.is_some_and(|waiting| waiting.counted));
    assert!(!engine.check_due(&id, Duration::from_secs(60)), "the edit's look counts as the last one");
    let seen = items_waiting(&engine, &id);
    Autopilot::start(engine.clone());
    // New files in its new source make the edited job's on-change trigger fire. Once its row
    // counts more waiting items than before, the autopilot handled that change, and only
    // looked (a sync would have left nothing waiting).
    poke_until(&[dst.clone(), w_src.clone()], "the witness ran by itself and the edited job's change was looked at", &engine, &[&id, &witness], || {
        ran_by_itself(&engine, &witness) && items_waiting(&engine, &id) > seen
    });
    wait_idle(&engine, &id);

    let job = view(&engine, &id);
    assert_eq!(job.job.last_run, last_run, "the edited job didn't sync");
    assert!(job.job.needs_review && !job.runs_by_itself);
    assert_eq!(read(&src.join("newer.txt")), "written after the last sync");
    assert_eq!(tree(&src), src_before);
    assert_eq!(without_pokes(tree(&dst)), without_pokes(dst_before));
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Review);
    engine.shut_down(Duration::from_secs(10));
}

// ------------------------------------------------------------------ probe 13: two app instances

#[test]
fn a_second_charon_on_the_same_data_folder_is_refused_and_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, c, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("c"), tmp.path().join("data"));
    for n in 1..=5 {
        write(&a.join(format!("f{n}.txt")), "mine");
    }
    write(&c.join("f1.txt"), "mine");
    let installed = plain_engine(&data);
    let id = add(&installed, job_setup(&a, &b, SyncMode::TwoWay));
    confirm(&installed, &id);
    let state_before = read(&data.join("charon-state.json"));

    // `cargo tauri dev` starts a second Charon with the same identifier, so the same data.
    assert!(matches!(Engine::open(&data, Arc::new(|_: EngineEvent| {})), Err(Error::AlreadyRunning)));
    assert_eq!(read(&data.join("charon-state.json")), state_before, "the refused one wrote nothing");

    // The installed app carries on as before.
    write(&a.join("new.txt"), "new");
    confirm(&installed, &id);
    for n in 1..=5 {
        assert!(a.join(format!("f{n}.txt")).exists() && b.join(format!("f{n}.txt")).exists());
    }
    assert!(b.join("new.txt").exists());
    assert_eq!(user_tree(&c).len(), 1);

    // Once it quits, the next Charon opens and finds the same job.
    drop(installed);
    let next = reopen(&data);
    assert_eq!(view(&next, &id).job.setup.destination, b);
}

// ==================================================================== 2. promises

// ------------------------------------------------------------------ nothing changes without an OK

/// Under the real autopilot, with `trigger` due: a confirmed mirror and a confirmed two-way
/// job without full automation, and a mirror with full automation that never had a confirmed
/// sync, all with changes waiting, are only looked at. A control job with full automation
/// and the same trigger proves the trigger fired (for Manual it must not run either), and a
/// witness proves the autopilot went round several times.
fn the_autopilot_only_looks_at_jobs_that_wait_for_an_ok(trigger: Trigger) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let data = root.join("data");
    let folders = |name: &str| (root.join(format!("{name}-src")), root.join(format!("{name}-dst")));
    let (p_src, p_dst) = folders("plain");
    let (t_src, t_dst) = folders("two-way");
    let (u_src, u_dst) = folders("unconfirmed");
    let (c_src, c_dst) = folders("control");
    let (w_src, w_dst) = folders("witness");
    for src in [&p_src, &t_src, &u_src, &c_src, &w_src] {
        write(&src.join("first.txt"), "first");
    }
    let job = |src: &Path, dst: &Path, mode: SyncMode, trigger: Trigger, automation: bool| {
        let mut setup = job_setup(src, dst, mode);
        setup.trigger = trigger;
        setup.full_automation = automation;
        setup
    };
    let (p, t, u, c, w) = {
        let engine = plain_engine(&data);
        let p = add(&engine, job(&p_src, &p_dst, SyncMode::Mirror, trigger, false));
        let t = add(&engine, job(&t_src, &t_dst, SyncMode::TwoWay, trigger, false));
        let u = add(&engine, job(&u_src, &u_dst, SyncMode::Mirror, trigger, true));
        let c = add(&engine, job(&c_src, &c_dst, SyncMode::Backup, trigger, true));
        let w = add(&engine, job(&w_src, &w_dst, SyncMode::Backup, Trigger::OnChange { settle_seconds: 1 }, true));
        for id in [&p, &t, &c, &w] {
            confirm(&engine, id);
        }
        (p, t, u, c, w)
    };
    // What a sync would do: copy new files, remove a file only the destination has, carry a
    // removal across in two-way.
    write(&p_src.join("new.txt"), "new");
    write(&p_dst.join("precious.txt"), "only here");
    write(&t_src.join("new-a.txt"), "new on a");
    write(&t_dst.join("new-b.txt"), "new on b");
    fs::remove_file(t_dst.join("first.txt")).unwrap();
    write(&u_src.join("new.txt"), "new");
    write(&u_dst.join("precious.txt"), "only here");
    write(&c_src.join("new.txt"), "carried by itself");

    // Restart, with every schedule due.
    wait_until_released(&data);
    backdate_jobs(&data);
    let engine = plain_engine(&data);
    let watched = [&p_src, &p_dst, &t_src, &t_dst, &u_src, &u_dst];
    let before: Vec<BTreeMap<String, String>> = watched.iter().map(|folder| without_pokes(tree(folder))).collect();
    let last_runs: Vec<_> = [&p, &t, &u].iter().map(|id| view(&engine, id).job.last_run).collect();
    // What each row shows before the autopilot starts; its regular look isn't due again for
    // many minutes.
    let seen: Vec<u64> = [&p, &t, &u].iter().map(|id| look_now(&engine, id)).collect();
    Autopilot::start(engine.clone());

    let automatic = trigger != Trigger::Manual;
    let on_change = trigger.syncs_on_change();
    let mut pokes = vec![w_src.clone()];
    if on_change {
        pokes.extend([p_src.clone(), t_src.clone(), u_src.clone(), c_src.clone()]);
    }
    // For on-change jobs: once a row counts more waiting items than before the autopilot
    // started, the autopilot handled the change in its folders, and only looked (a sync
    // would have left nothing waiting). For the other triggers the control job proves it:
    // the jobs come before it in the list, so the autopilot decided about them first.
    poke_until(&pokes, "the control job and the witness ran by themselves", &engine, &[&p, &t, &u, &c, &w], || {
        ran_by_itself(&engine, &w)
            && (!automatic || ran_by_itself(&engine, &c))
            && (!on_change || [&p, &t, &u].iter().zip(&seen).all(|(id, seen)| items_waiting(&engine, id) > *seen))
    });
    if automatic {
        assert_eq!(read(&c_dst.join("new.txt")), "carried by itself", "the trigger fired");
    } else {
        assert!(!c_dst.join("new.txt").exists(), "a manual job only syncs when asked");
        assert!(view(&engine, &c).job.last_run.unwrap().approved);
    }
    // Every job that waits for an OK was looked at: its row shows the changes waiting.
    for id in [&p, &t, &u] {
        eventually("the autopilot looked at the job", || view(&engine, id).waiting.is_some() && !engine.is_running(id));
        assert_eq!(view(&engine, id).status, JobStatus::Review);
    }
    for (id, last_run) in [&p, &t, &u].iter().zip(&last_runs) {
        assert_eq!(&view(&engine, id).job.last_run, last_run, "the job didn't sync");
    }
    for (folder, before) in watched.iter().zip(&before) {
        assert_eq!(&without_pokes(tree(folder)), before, "nothing changed in {}", folder.display());
    }
    assert!(view(&engine, &u).job.needs_review);
    engine.shut_down(Duration::from_secs(10));
}

#[test]
fn the_autopilot_never_syncs_a_manual_job_without_an_ok() {
    the_autopilot_only_looks_at_jobs_that_wait_for_an_ok(Trigger::Manual);
}

#[test]
fn the_autopilot_never_syncs_an_on_change_job_without_an_ok() {
    the_autopilot_only_looks_at_jobs_that_wait_for_an_ok(Trigger::OnChange { settle_seconds: 1 });
}

#[test]
fn the_autopilot_never_syncs_an_interval_job_without_an_ok() {
    the_autopilot_only_looks_at_jobs_that_wait_for_an_ok(Trigger::Interval { minutes: 1 });
}

#[test]
fn the_autopilot_never_syncs_a_daily_job_without_an_ok() {
    the_autopilot_only_looks_at_jobs_that_wait_for_an_ok(Trigger::Daily { hour: 3, minute: 0 });
}

#[test]
fn the_autopilot_never_syncs_a_drive_connected_job_without_an_ok() {
    the_autopilot_only_looks_at_jobs_that_wait_for_an_ok(Trigger::DriveConnected);
}

#[test]
fn sync_now_sync_all_and_automatic_starts_change_nothing_for_jobs_that_wait_for_an_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let engine = plain_engine(&data);
    let triggers = [
        Trigger::Manual,
        Trigger::OnChange { settle_seconds: 1 },
        Trigger::Interval { minutes: 1 },
        Trigger::Daily { hour: 3, minute: 0 },
        Trigger::DriveConnected,
    ];
    let mut jobs = Vec::new();
    for (n, trigger) in triggers.into_iter().enumerate() {
        for automation in [false, true] {
            let (src, dst) = (tmp.path().join(format!("src-{n}-{automation}")), tmp.path().join(format!("dst-{n}-{automation}")));
            write(&src.join("first.txt"), "first");
            let mut setup = job_setup(&src, &dst, SyncMode::Mirror);
            setup.trigger = trigger;
            setup.full_automation = automation;
            let id = add(&engine, setup);
            // Without full automation: confirmed once. With it: never confirmed yet.
            if !automation {
                confirm(&engine, &id);
            }
            write(&src.join("new.txt"), "new");
            write(&dst.join("precious.txt"), "only here");
            jobs.push((id, dst));
        }
    }
    let before: Vec<_> = jobs.iter().map(|(id, dst)| (tree(dst), view(&engine, id).job.last_run)).collect();
    for (id, _) in &jobs {
        assert_eq!(engine.sync_now(id).unwrap(), SyncStart::Review);
        engine.want(id, "for a test");
        engine.start_wanted(id);
        assert!(!engine.is_running(id), "no run started");
        engine.check(id);
    }
    let mut asking = engine.sync_all().unwrap();
    asking.sort();
    let mut all: Vec<String> = jobs.iter().map(|(id, _)| id.clone()).collect();
    all.sort();
    assert_eq!(asking, all, "Sync all asks for every one of them");
    for (id, _) in &jobs {
        eventually("the look finished", || view(&engine, id).waiting.is_some() && !engine.is_running(id));
    }
    for ((id, dst), (tree_before, run_before)) in jobs.iter().zip(&before) {
        assert_eq!(&tree(dst), tree_before, "nothing changed in {}", dst.display());
        assert_eq!(&view(&engine, id).job.last_run, run_before);
    }
}

#[test]
fn turning_full_automation_on_still_waits_for_one_more_confirmed_sync() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("first.txt"), "first");
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&src, &dst, SyncMode::Mirror));
    confirm(&engine, &id);
    let mut setup = view(&engine, &id).job.setup;
    setup.full_automation = true;
    engine.update_job(&id, setup).unwrap();
    assert!(!view(&engine, &id).runs_by_itself);

    write(&src.join("new.txt"), "new");
    write(&dst.join("precious.txt"), "only here");
    let before = tree(&dst);
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Review);
    engine.want(&id, "for a test");
    engine.start_wanted(&id);
    assert!(!engine.is_running(&id));
    assert_eq!(tree(&dst), before);

    confirm(&engine, &id);
    assert!(view(&engine, &id).runs_by_itself, "after one confirmed sync it runs by itself");
    assert_eq!(trashed(&dst, "precious.txt"), vec!["only here".to_string()]);
}

#[test]
fn a_job_with_full_automation_never_syncs_by_itself_while_its_list_is_open() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("first.txt"), "first");
    let engine = plain_engine(&data);
    let mut setup = job_setup(&src, &dst, SyncMode::Mirror);
    setup.full_automation = true;
    let id = add(&engine, setup);
    confirm(&engine, &id);
    write(&src.join("new.txt"), "new");
    write(&dst.join("precious.txt"), "only here");
    let before = tree(&dst);

    let review = engine.prepare_review(&id).unwrap();
    match engine.sync_now(&id) {
        Err(Error::Invalid(msg)) => assert!(msg.contains("list") && msg.contains("open"), "refused for the open list: {msg}"),
        other => panic!("the open list comes first: {other:?}"),
    }
    engine.want(&id, "for a test");
    engine.start_wanted(&id);
    assert!(!engine.is_running(&id));
    assert_eq!(tree(&dst), before);
    // The list the person looks at is still the one that counts.
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    assert!(dst.join("new.txt").exists());
}

// ------------------------------------------------------------------ an empty list that ends a wait

/// Copies the file `from` to `to` with its time, as the person copying it by hand would.
fn copy_by_hand(from: &Path, to: &Path) {
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    fs::copy(from, to).unwrap();
    filetime::set_file_mtime(to, filetime::FileTime::from_last_modification_time(&fs::metadata(from).unwrap())).unwrap();
}

#[test]
fn a_stopped_job_with_nothing_left_to_carry_is_confirmed_from_its_empty_list() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("a.txt"), "a");
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let id = add(engine, job_setup(&src, &dst, SyncMode::Backup));

    // The person stops the confirmed sync before it carried anything, then copies the file
    // by hand: nothing is left to carry, but the job waits for their OK.
    let review = engine.prepare_review(&id).unwrap();
    let (stopper, stopped) = (engine.clone(), id.clone());
    rig.at_first_step(move || stopper.cancel(&stopped));
    let_progress_throttle_pass();
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(engine, &id);
    assert!(rig.hook_fired());
    copy_by_hand(&src.join("a.txt"), &dst.join("a.txt"));
    let row = view(engine, &id);
    assert!(row.job.held.is_some() && row.waiting.is_some(), "{}", describe(engine, &id));

    // Its empty list says why it's worth confirming, and confirming it ends the wait.
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.total_steps, 0, "{:?}", review.steps);
    assert_eq!(review.confirm_anyway.as_deref(), Some("Confirm it, and the job stops waiting for your OK."));
    engine.approve(&id, &review.review_id, review.needs_tick).unwrap();
    wait_idle(engine, &id);
    let row = view(engine, &id);
    assert!(row.job.held.is_none() && row.waiting.is_none(), "{}", describe(engine, &id));
    assert_eq!(row.status, JobStatus::InSync);
    assert_eq!(engine.overview().waiting, 0);

    // An empty list that ends no wait only says the folders match.
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!((review.total_steps, review.confirm_anyway), (0, None));
}

#[test]
fn a_held_job_with_full_automation_is_confirmed_from_its_empty_list_once_its_warnings_are_ticked() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("a.txt"), "a");
    let engine = plain_engine(&data);
    let mut setup = job_setup(&src, &dst, SyncMode::Mirror);
    setup.full_automation = true;
    let id = add(&engine, setup);
    confirm(&engine, &id);
    assert!(view(&engine, &id).runs_by_itself);

    // A cleanup tool removed Charon's marker from the destination; the files are as they were.
    let marker = dst.join(".charon").join(format!("{id}.json"));
    fs::remove_file(&marker).unwrap();
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait_idle(&engine, &id);
    let row = view(&engine, &id);
    assert!(row.job.held.is_some() && !row.runs_by_itself, "held for an OK: {}", describe(&engine, &id));

    // The list has nothing to carry, but warns, needs the tick, and can be confirmed.
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.total_steps, 0, "{:?}", review.steps);
    assert!(review.needs_tick && review.warnings.iter().any(|w| w.level == Level::Danger), "{:?}", review.warnings);
    assert_eq!(review.confirm_anyway.as_deref(), Some("Confirm once so Charon may sync this job by itself from now on."));
    match engine.approve(&id, &review.review_id, false) {
        Err(Error::Invalid(msg)) => assert_eq!(msg, "Tick “I've read the warnings above” first."),
        other => panic!("confirmed without the tick: {other:?}"),
    }
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    let row = view(&engine, &id);
    assert!(row.runs_by_itself && row.job.held.is_none(), "{}", describe(&engine, &id));
    assert!(marker.is_file(), "the confirmed sync left its marker again");
    assert_eq!(read(&dst.join("a.txt")), "a");
}

#[test]
fn a_destination_without_its_marker_is_taken_as_it_is_from_a_confirmed_empty_list() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("a.txt"), "a");
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&src, &dst, SyncMode::Mirror));
    confirm(&engine, &id);
    let marker = dst.join(".charon").join(format!("{id}.json"));
    fs::remove_file(&marker).unwrap();

    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.total_steps, 0, "{:?}", review.steps);
    assert!(review.needs_tick);
    assert_eq!(
        review.confirm_anyway.as_deref(),
        Some("If these are the right folders, confirm it: Charon then takes them as they are now.")
    );
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    assert!(marker.is_file(), "the confirmed sync left its marker again");
    let review = engine.prepare_review(&id).unwrap();
    assert!(!review.needs_tick && review.warnings.iter().all(|w| w.level != Level::Danger), "{:?}", review.warnings);
    assert_eq!(review.confirm_anyway, None);
}

#[test]
fn a_new_job_over_matching_folders_is_confirmed_from_its_empty_list() {
    let tmp = tempfile::tempdir().unwrap();
    let (photos, backup, data) = (tmp.path().join("photos"), tmp.path().join("backup"), tmp.path().join("data"));
    let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
    for name in ["a.txt", "b.txt"] {
        write(&photos.join(name), name);
        write(&src.join(name), name);
        copy_by_hand(&src.join(name), &dst.join(name));
    }
    copy_tree(&photos, &backup, &[]);
    let engine = plain_engine(&data);

    // One way: the first sync, though it has nothing to copy.
    let id = add(&engine, job_setup(&photos, &backup, SyncMode::Backup));
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.total_steps, 0, "{:?}", review.steps);
    assert_eq!(review.confirm_anyway.as_deref(), Some("Confirm it as the job's first sync, so Charon knows both folders from now on."));
    engine.approve(&id, &review.review_id, review.needs_tick).unwrap();
    wait_idle(&engine, &id);
    assert_eq!(view(&engine, &id).status, JobStatus::InSync);

    // Two-way: without a memory of both folders, nothing removed on one side is ever
    // removed on the other. Confirming the empty list gives Charon that memory.
    let id = add(&engine, job_setup(&src, &dst, SyncMode::TwoWay));
    assert_eq!(view(&engine, &id).status, JobStatus::New);
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.total_steps, 0, "{:?}", review.steps);
    assert_eq!(
        review.confirm_anyway.as_deref(),
        Some("Confirm it, and Charon remembers both folders as they are now, so that later lists can carry what you remove on one side to the other.")
    );
    engine.approve(&id, &review.review_id, review.needs_tick).unwrap();
    wait_idle(&engine, &id);
    assert_eq!(view(&engine, &id).status, JobStatus::InSync);
    fs::remove_file(src.join("a.txt")).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.destination.removed, review.tally.source.new_files), (1, 0), "{:?}", review.tally);
    assert_eq!(review.confirm_anyway, None);
}

// ------------------------------------------------------------------ a list that can't be confirmed

#[test]
fn a_list_cannot_be_confirmed_after_a_change_in_the_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("doc.txt"), "a");
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&a, &b, SyncMode::TwoWay));
    confirm(&engine, &id);
    fs::remove_file(a.join("doc.txt")).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.removed, 1);

    write(&b.join("doc.txt"), "edited on b after the list was made");
    assert!(engine.note_change(&id, &b.join("doc.txt")), "the folder watcher reports it");
    assert!(matches!(engine.approve(&id, &review.review_id, true), Err(Error::ReviewOutdated(_))));
    assert!(!engine.is_running(&id));
    assert_eq!(read(&b.join("doc.txt")), "edited on b after the list was made");
}

#[test]
fn a_list_cannot_be_confirmed_after_the_jobs_setup_changed() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("doc.txt"), "a");
    write(&a.join("gone.txt"), "g");
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&a, &b, SyncMode::TwoWay));
    confirm(&engine, &id);
    fs::remove_file(a.join("gone.txt")).unwrap();
    write(&a.join("doc.txt"), "edited, longer");

    type Change = Box<dyn Fn(&mut JobSetup)>;
    let changes: Vec<(&str, Change)> = vec![
        ("safe deletes turned off", Box::new(|s: &mut JobSetup| s.safe_delete = false)),
        ("full automation turned on", Box::new(|s: &mut JobSetup| s.full_automation = true)),
        ("another conflict rule", Box::new(|s: &mut JobSetup| s.conflict = ConflictRule::NewestWins)),
        ("another trigger", Box::new(|s: &mut JobSetup| s.trigger = Trigger::Interval { minutes: 30 })),
        ("a new exclude", Box::new(|s: &mut JobSetup| s.excludes.push("*.bak".into()))),
        ("another mode", Box::new(|s: &mut JobSetup| s.mode = SyncMode::Mirror)),
    ];
    for (what, change) in changes {
        let original = view(&engine, &id).job.setup;
        let review = engine.prepare_review(&id).unwrap();
        let before = (tree(&a), tree(&b));
        let mut changed = original.clone();
        change(&mut changed);
        engine.update_job(&id, changed).unwrap();
        let answer = engine.approve(&id, &review.review_id, true);
        assert!(matches!(answer, Err(Error::ReviewOutdated(_))), "confirmed after {what}: {answer:?}");
        assert!(!engine.is_running(&id));
        assert_eq!((tree(&a), tree(&b)), before, "nothing changed after {what}");
        engine.update_job(&id, original).unwrap();
    }
    // Changed and changed back is still a change: the list made before is gone.
    let review = engine.prepare_review(&id).unwrap();
    let before = (tree(&a), tree(&b));
    let original = view(&engine, &id).job.setup;
    let mut elsewhere = original.clone();
    elsewhere.destination = tmp.path().join("c");
    fs::create_dir_all(&elsewhere.destination).unwrap();
    engine.update_job(&id, elsewhere).unwrap();
    engine.update_job(&id, original).unwrap();
    let answer = engine.approve(&id, &review.review_id, true);
    assert!(matches!(answer, Err(Error::ReviewOutdated(_))), "confirmed after the folders changed and changed back: {answer:?}");
    assert!(!engine.is_running(&id));
    assert_eq!((tree(&a), tree(&b)), before);
}

#[test]
fn a_list_cannot_be_confirmed_when_a_folder_went_offline() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("new.txt"), "new");
    write(&dst.join("precious.txt"), "only here");
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&src, &dst, SyncMode::Mirror));
    let review = engine.prepare_review(&id).unwrap();

    // The drive is unplugged.
    let unplugged = tmp.path().join("unplugged");
    fs::rename(&dst, &unplugged).unwrap();
    let before = tree(&unplugged);
    assert!(matches!(engine.approve(&id, &review.review_id, true), Err(Error::Offline(_))));
    assert!(!engine.is_running(&id));
    assert!(!dst.exists(), "nothing was written where the drive was");
    assert_eq!(tree(&unplugged), before);

    // The source can go away too.
    fs::rename(&unplugged, &dst).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    fs::rename(&src, tmp.path().join("src-away")).unwrap();
    assert!(matches!(engine.approve(&id, &review.review_id, true), Err(Error::Offline(_))));
    assert_eq!(read(&dst.join("precious.txt")), "only here");
}

#[test]
fn a_list_cannot_be_confirmed_when_a_folder_was_replaced_by_another_at_the_same_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    for n in 0..4 {
        write(&a.join(format!("f{n}.txt")), "mine");
    }
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&a, &b, SyncMode::TwoWay));
    confirm(&engine, &id);
    fs::remove_file(b.join("f0.txt")).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.source.removed, 1);

    // Another drive under the same name.
    let first = tmp.path().join("first-b");
    fs::rename(&b, &first).unwrap();
    write(&b.join("other.txt"), "another drive");
    let (a_before, b_before, first_before) = (tree(&a), tree(&b), tree(&first));
    assert!(matches!(engine.approve(&id, &review.review_id, true), Err(Error::ReviewOutdated(_))));
    assert!(!engine.is_running(&id));
    assert_eq!((tree(&a), tree(&b), tree(&first)), (a_before, b_before, first_before), "nothing changed anywhere");

    // The same for the source.
    fs::remove_dir_all(&b).unwrap();
    fs::rename(&first, &b).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    let old_a = tmp.path().join("old-a");
    fs::rename(&a, &old_a).unwrap();
    fs::create_dir_all(&a).unwrap();
    assert!(matches!(engine.approve(&id, &review.review_id, true), Err(Error::ReviewOutdated(_))));
    assert!(a.is_dir());
    assert!(tree(&a).is_empty());
    assert_eq!(user_tree(&old_a).len(), 4);
}

/// A mirror job after its confirmed sync, with one file removed from the source: the list
/// (open) removes dst/b.txt.
fn mirror_with_a_removal_listed() -> (tempfile::TempDir, Engine, String, Review, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("a.txt"), "a");
    write(&src.join("b.txt"), "b");
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&src, &dst, SyncMode::Mirror));
    confirm(&engine, &id);
    fs::remove_file(src.join("b.txt")).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.removed, 1);
    assert!(review.blocked.is_none());
    (tmp, engine, id, review, src, dst)
}

#[test]
fn a_list_cannot_be_confirmed_when_the_folder_lost_charons_marker() {
    // The very same folder (same drive, same folder numbers), but without the marker the
    // last sync left: maybe the drive was restored or reformatted and refilled meanwhile.
    let (_tmp, engine, id, review, _src, dst) = mirror_with_a_removal_listed();
    let marker = dst.join(".charon").join(format!("{id}.json"));
    assert!(marker.is_file(), "the last sync left its marker at {}", marker.display());
    fs::remove_file(&marker).unwrap();
    let before = user_tree(&dst);
    match engine.approve(&id, &review.review_id, true) {
        Err(Error::ReviewOutdated(msg)) => assert!(msg.contains("marker"), "refused for the marker: {msg}"),
        other => panic!("confirmed although the marker is gone: {other:?}"),
    }
    assert!(!engine.is_running(&id));
    assert_eq!(read(&dst.join("b.txt")), "b");
    assert_eq!(user_tree(&dst), before, "nothing changed");
    assert!(trashed(&dst, "b.txt").is_empty());
}

#[test]
fn a_list_cannot_be_confirmed_when_another_folder_with_the_same_contents_and_marker_took_its_place() {
    // Another folder at the same path holding a full copy of the destination, Charon's
    // marker included (a cloned drive, a folder restored from a copy).
    let (tmp, engine, id, review, _src, dst) = mirror_with_a_removal_listed();
    let first = tmp.path().join("first-dst");
    fs::rename(&dst, &first).unwrap();
    copy_tree(&first, &dst, &[]);
    assert!(dst.join(".charon").join(format!("{id}.json")).is_file(), "the copy carries the marker");
    let before = (tree(&dst), tree(&first));
    match engine.approve(&id, &review.review_id, true) {
        Err(Error::ReviewOutdated(msg)) => assert!(msg.contains("isn't the one"), "refused because it is another folder: {msg}"),
        other => panic!("confirmed in another folder: {other:?}"),
    }
    assert!(!engine.is_running(&id));
    assert_eq!((tree(&dst), tree(&first)), before, "neither folder changed");
    assert_eq!(read(&dst.join("b.txt")), "b");
    assert_eq!(read(&first.join("b.txt")), "b");
}

#[test]
#[ignore = "takes 15 minutes; run with --ignored"]
fn a_list_older_than_15_minutes_cannot_be_confirmed() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("new.txt"), "new");
    write(&dst.join("precious.txt"), "only here");
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&src, &dst, SyncMode::Mirror));
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.removed, 1);
    assert!(review.expires_at <= review.prepared_at + chrono::Duration::minutes(15));
    let before = tree(&dst);
    // The clock decides, not a sleep: wait until the list is past its time.
    let deadline = Instant::now() + Duration::from_secs(20 * 60);
    while chrono::Utc::now() <= review.expires_at + chrono::Duration::seconds(1) {
        assert!(Instant::now() < deadline, "the clock never passed {}", review.expires_at);
        thread::sleep(Duration::from_secs(1));
    }
    match engine.approve(&id, &review.review_id, true) {
        Err(Error::ReviewOutdated(msg)) => assert!(msg.contains("minutes"), "refused for its age: {msg}"),
        other => panic!("a list older than 15 minutes was confirmed: {other:?}"),
    }
    assert!(!engine.is_running(&id));
    assert_eq!(tree(&dst), before, "nothing changed");
}

#[test]
fn a_list_that_needs_the_tick_is_refused_without_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    for n in 0..10 {
        write(&src.join(format!("f{n}.txt")), "x");
    }
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&src, &dst, SyncMode::Mirror));
    confirm(&engine, &id);
    for n in 1..10 {
        fs::remove_file(src.join(format!("f{n}.txt"))).unwrap();
    }
    let review = engine.prepare_review(&id).unwrap();
    assert!(review.needs_tick, "removing 9 of 10 files needs the tick");
    assert!(review.blocked.is_none(), "not refused outright: {:?}", review.blocked);
    let before = tree(&dst);
    match engine.approve(&id, &review.review_id, false) {
        Err(Error::Invalid(msg)) => assert!(msg.contains("Tick"), "refused for the missing tick: {msg}"),
        other => panic!("confirmed without the tick: {other:?}"),
    }
    assert!(!engine.is_running(&id));
    assert_eq!(tree(&dst), before);
    // With the tick, it goes ahead, into the trash.
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    assert_eq!(trashed(&dst, "f5.txt"), vec!["x".to_string()]);
}

#[test]
fn a_blocked_list_is_refused_even_with_the_tick() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    for n in 0..6 {
        write(&a.join(format!("f{n}.txt")), "mine");
    }
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&a, &b, SyncMode::TwoWay));
    confirm(&engine, &id);
    // Everything of the person's is gone from b (Charon's own marker is still there).
    for n in 0..6 {
        fs::remove_file(b.join(format!("f{n}.txt"))).unwrap();
    }
    let review = engine.prepare_review(&id).unwrap();
    assert!(review.blocked.is_some(), "a sync that would empty the source is refused");
    let before = tree(&a);
    match engine.approve(&id, &review.review_id, true) {
        Err(Error::Invalid(msg)) => assert_eq!(Some(msg), review.blocked, "refused for the reason the list gave"),
        other => panic!("a blocked list was confirmed: {other:?}"),
    }
    assert!(!engine.is_running(&id));
    assert_eq!(tree(&a), before);
}

// ------------------------------------------------------------------ verify before acting

#[test]
fn files_edited_in_the_destination_after_the_list_was_made_are_neither_overwritten_nor_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("report.txt"), "v1");
    write(&src.join("old.txt"), "old");
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&src, &dst, SyncMode::Mirror));
    confirm(&engine, &id);
    write(&src.join("report.txt"), "v2, longer");
    fs::remove_file(src.join("old.txt")).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.destination.replaced, review.tally.destination.removed), (1, 1));

    // Before the person confirms, someone edits both files on the destination drive.
    write(&dst.join("report.txt"), "an edit made on the backup drive");
    write(&dst.join("old.txt"), "an edit made on the backup drive too");
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    assert_eq!(read(&dst.join("report.txt")), "an edit made on the backup drive");
    assert_eq!(read(&dst.join("old.txt")), "an edit made on the backup drive too");
    assert_eq!(view(&engine, &id).job.last_run.unwrap().skipped, 2);
}

#[test]
fn a_source_file_emptied_after_the_list_was_made_is_not_carried_over_the_good_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("report.txt"), "v1");
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&src, &dst, SyncMode::Mirror));
    confirm(&engine, &id);
    write(&src.join("report.txt"), "v2, the reviewed version");
    let review = engine.prepare_review(&id).unwrap();
    fs::write(src.join("report.txt"), "").unwrap(); // an app crashed while saving
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    assert_eq!(read(&dst.join("report.txt")), "v1", "what nobody looked at isn't carried");
    assert_eq!(view(&engine, &id).job.last_run.unwrap().skipped, 1);
}

#[test]
fn files_edited_on_either_side_while_a_two_way_run_goes_are_neither_overwritten_nor_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    for name in ["plan.txt", "old.txt", "notes.txt", "draft.txt"] {
        write(&a.join(name), "v1");
    }
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let id = add(engine, job_setup(&a, &b, SyncMode::TwoWay));
    confirm(engine, &id);

    // a → b: a replacement and a removal; b → a: the same.
    write(&a.join("plan.txt"), "v2 from a");
    fs::remove_file(a.join("old.txt")).unwrap();
    write(&b.join("notes.txt"), "v2 from b");
    fs::remove_file(b.join("draft.txt")).unwrap();
    write(&a.join("aaa.txt"), "carried first");
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.destination.replaced + review.tally.destination.removed, 2, "{:?}", review.steps);
    assert_eq!(review.tally.source.replaced + review.tally.source.removed, 2, "{:?}", review.steps);
    let (ap, bp) = (a.clone(), b.clone());
    rig.at_first_step(move || {
        fs::write(bp.join("plan.txt"), "typed on b during the run").unwrap();
        fs::write(bp.join("old.txt"), "typed on b during the run, too").unwrap();
        fs::write(ap.join("notes.txt"), "typed on a during the run").unwrap();
        fs::write(ap.join("draft.txt"), "typed on a during the run, too").unwrap();
    });
    let_progress_throttle_pass();
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(engine, &id);
    assert!(rig.hook_fired());
    assert_eq!(read(&b.join("plan.txt")), "typed on b during the run");
    assert_eq!(read(&b.join("old.txt")), "typed on b during the run, too");
    assert_eq!(read(&a.join("notes.txt")), "typed on a during the run");
    assert_eq!(read(&a.join("draft.txt")), "typed on a during the run, too");
    assert_eq!(view(engine, &id).job.last_run.unwrap().skipped, 4);

    // The next sync keeps every version: the edits win over removals, both versions of a
    // file changed on both sides are kept.
    confirm(engine, &id);
    for side in [&a, &b] {
        assert_eq!(versions(side, "plan"), vec!["typed on b during the run".to_string(), "v2 from a".to_string()]);
        assert_eq!(versions(side, "notes"), vec!["typed on a during the run".to_string(), "v2 from b".to_string()]);
        assert_eq!(read(&side.join("old.txt")), "typed on b during the run, too");
        assert_eq!(read(&side.join("draft.txt")), "typed on a during the run, too");
    }
}

// ------------------------------------------------------------------ forgetting the two-way memory

#[test]
fn changing_a_two_way_jobs_mode_makes_charon_forget_and_the_next_look_removes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    for n in 1..=5 {
        write(&a.join(format!("f{n}.txt")), "mine");
    }
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&a, &b, SyncMode::TwoWay));
    confirm(&engine, &id);
    fs::remove_file(b.join("f3.txt")).unwrap();

    // To Backup and back: what Charon remembered no longer applies.
    let two_way = view(&engine, &id).job.setup;
    let mut backup = two_way.clone();
    backup.mode = SyncMode::Backup;
    engine.update_job(&id, backup).unwrap();
    engine.update_job(&id, two_way).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.source.removed, review.tally.destination.removed), (0, 0), "{:?}", review.steps);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    assert_eq!(read(&a.join("f3.txt")), "mine");
    assert_eq!(read(&b.join("f3.txt")), "mine", "compared afresh, the file is copied back");
}

#[test]
fn swapping_a_two_way_jobs_folders_makes_charon_forget_and_the_next_look_removes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    for n in 1..=5 {
        write(&a.join(format!("f{n}.txt")), "mine");
    }
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&a, &b, SyncMode::TwoWay));
    confirm(&engine, &id);
    fs::remove_file(a.join("f2.txt")).unwrap();
    fs::remove_file(b.join("f4.txt")).unwrap();

    let mut swapped = view(&engine, &id).job.setup;
    std::mem::swap(&mut swapped.source, &mut swapped.destination);
    engine.update_job(&id, swapped).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.source.removed, review.tally.destination.removed), (0, 0), "{:?}", review.steps);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    for n in 1..=5 {
        assert!(a.join(format!("f{n}.txt")).exists() && b.join(format!("f{n}.txt")).exists(), "f{n}.txt is on both sides");
    }
}

#[test]
fn a_two_way_run_that_never_finished_makes_the_next_look_remove_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    for n in 0..5 {
        write(&a.join(format!("f{n}.txt")), "mine");
    }
    let id = {
        let engine = plain_engine(&data);
        let id = add(&engine, job_setup(&a, &b, SyncMode::TwoWay));
        confirm(&engine, &id);
        id
    };
    // The computer lost power while a run carried files: the state file still says so, and
    // what Charon remembers may not match the folders. Meanwhile a file is gone from b.
    wait_until_released(&data);
    mark_run_in_progress(&data, &id);
    fs::remove_file(b.join("f3.txt")).unwrap();

    let engine = reopen(&data);
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.source.removed, review.tally.destination.removed), (0, 0), "{:?}", review.steps);
    assert!(
        review.warnings.iter().any(|w| w.level == Level::Caution && w.text.contains("didn't finish")),
        "the list says why it compares afresh: {:?}",
        review.warnings
    );
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    assert_eq!(read(&a.join("f3.txt")), "mine", "not removed from a");
    assert_eq!(read(&b.join("f3.txt")), "mine", "compared afresh, the file is copied back");
    assert!(trashed(&a, "f3.txt").is_empty());
}

#[test]
fn a_two_way_list_made_after_a_folder_lost_charons_marker_removes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    for n in 0..5 {
        write(&a.join(format!("f{n}.txt")), "mine");
    }
    let engine = plain_engine(&data);
    let id = add(&engine, job_setup(&a, &b, SyncMode::TwoWay));
    confirm(&engine, &id);
    // b no longer carries Charon's marker (another folder, or an older copy of b), and one
    // file is missing from it.
    assert!(b.join(".charon").join(format!("{id}.json")).is_file(), "the last sync left its marker");
    fs::remove_dir_all(b.join(".charon")).unwrap();
    fs::remove_file(b.join("f3.txt")).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!((review.tally.source.removed, review.tally.destination.removed), (0, 0), "{:?}", review.steps);
    assert!(
        review.warnings.iter().any(|w| w.level == Level::Caution && w.text.contains("marker")),
        "the list says why it compares afresh: {:?}",
        review.warnings
    );
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    assert_eq!(read(&a.join("f3.txt")), "mine", "not removed from a");
    assert_eq!(read(&b.join("f3.txt")), "mine");
}

// ------------------------------------------------------------------ full automation is held back

#[test]
fn full_automation_holds_back_a_two_way_run_when_one_side_was_emptied() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    for n in 0..10 {
        write(&a.join(format!("f{n}.txt")), "mine");
    }
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let mut setup = job_setup(&a, &b, SyncMode::TwoWay);
    setup.full_automation = true;
    let id = add(engine, setup);
    confirm(engine, &id);

    // Nearly everything vanished from b (the marker is still there, so the memory is trusted).
    for n in 1..10 {
        fs::remove_file(b.join(format!("f{n}.txt"))).unwrap();
    }
    let (a_before, b_before) = (tree(&a), tree(&b));
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait_idle(engine, &id);
    let job = view(engine, &id);
    assert!(job.job.held.is_some() && job.job.needs_review, "held for an OK");
    eventually("the Held event arrived", || rig.held_events(&id) >= 1);
    assert_eq!(rig.held_events(&id), 1);
    assert_eq!((tree(&a), tree(&b)), (a_before, b_before), "nothing was touched");

    // And the side emptied completely.
    fs::remove_file(b.join("f0.txt")).unwrap();
    let review = engine.prepare_review(&id).unwrap();
    assert!(review.blocked.is_some());
    assert_eq!(user_tree(&a).len(), 10);
}

#[test]
fn full_automation_holds_back_when_a_folder_is_not_the_one_it_synced_with() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, src, dst, data) =
        (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    for n in 0..20 {
        write(&a.join(format!("f{n:02}.txt")), "mine");
        write(&src.join(format!("f{n:02}.txt")), "mine");
    }
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let mut two_way = job_setup(&a, &b, SyncMode::TwoWay);
    two_way.full_automation = true;
    let two_way = add(engine, two_way);
    let mut mirror = job_setup(&src, &dst, SyncMode::Mirror);
    mirror.full_automation = true;
    let mirror = add(engine, mirror);
    confirm(engine, &two_way);
    confirm(engine, &mirror);

    // Another folder under the same names that looks almost exactly like the old one, so
    // only Charon's marker can tell: for b, an older copy of it (one file short, which a
    // trusted memory would turn into a removal in a); for dst, a copy of it holding one more
    // file of someone else's (which a mirror would remove). Neither copy carries the marker.
    let (old_b, old_dst) = (tmp.path().join("old-b"), tmp.path().join("old-dst"));
    fs::rename(&b, &old_b).unwrap();
    copy_tree(&old_b, &b, &[".charon", "f19.txt"]);
    fs::rename(&dst, &old_dst).unwrap();
    copy_tree(&old_dst, &dst, &[".charon"]);
    write(&dst.join("someone-elses.txt"), "not part of the backup");
    let folders = [&a, &b, &old_b, &src, &dst, &old_dst];
    let before: Vec<_> = folders.iter().map(|f| tree(f)).collect();
    for id in [&two_way, &mirror] {
        assert_eq!(engine.sync_now(id).unwrap(), SyncStart::Started);
        wait_idle(engine, id);
        let job = view(engine, id);
        let held = job.job.held.clone().unwrap_or_else(|| panic!("held for an OK: {:?}", job.job.last_run));
        assert!(job.job.needs_review);
        assert!(held.contains("marker"), "held because of the missing marker: {held}");
        eventually("the Held event arrived", || rig.held_events(id) >= 1);
        assert_eq!(rig.held_events(id), 1);
    }
    let after: Vec<_> = folders.iter().map(|f| tree(f)).collect();
    assert_eq!(after, before, "nothing was touched");
    assert_eq!(read(&a.join("f19.txt")), "mine");
    assert_eq!(read(&dst.join("someone-elses.txt")), "not part of the backup");
}

// ------------------------------------------------------------------ the trash

#[test]
fn with_safe_deletes_removed_and_replaced_files_go_to_the_trash() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let engine = plain_engine(&data);
    for mode in [SyncMode::Mirror, SyncMode::Backup] {
        let (src, dst) = (tmp.path().join(format!("src-{mode:?}")), tmp.path().join(format!("dst-{mode:?}")));
        write(&src.join("report.txt"), "v1");
        write(&src.join("old.txt"), "old");
        let id = add(&engine, job_setup(&src, &dst, mode));
        confirm(&engine, &id);
        write(&src.join("report.txt"), "v2, longer");
        fs::remove_file(src.join("old.txt")).unwrap();
        confirm(&engine, &id);
        assert_eq!(read(&dst.join("report.txt")), "v2, longer");
        assert_eq!(trashed(&dst, "report.txt"), vec!["v1".to_string()], "{mode:?}: the replaced version is in the trash");
        if mode == SyncMode::Mirror {
            assert!(!dst.join("old.txt").exists());
            assert_eq!(trashed(&dst, "old.txt"), vec!["old".to_string()], "the removed file is in the trash");
        } else {
            assert_eq!(read(&dst.join("old.txt")), "old", "Backup never removes");
        }
    }
    // Two-way: removals on either side go to that side's trash.
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(&a.join("from-a.txt"), "a");
    write(&a.join("from-b.txt"), "b");
    let id = add(&engine, job_setup(&a, &b, SyncMode::TwoWay));
    confirm(&engine, &id);
    fs::remove_file(a.join("from-a.txt")).unwrap();
    fs::remove_file(b.join("from-b.txt")).unwrap();
    confirm(&engine, &id);
    assert_eq!(trashed(&b, "from-a.txt"), vec!["a".to_string()]);
    assert_eq!(trashed(&a, "from-b.txt"), vec!["b".to_string()]);
}

#[test]
fn two_way_keeps_replaced_versions_in_the_trash_even_with_safe_deletes_off() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("plan.txt"), "v1");
    write(&a.join("notes.txt"), "n1");
    let engine = plain_engine(&data);
    let mut setup = job_setup(&a, &b, SyncMode::TwoWay);
    setup.safe_delete = false;
    let id = add(&engine, setup);
    confirm(&engine, &id);
    write(&a.join("plan.txt"), "v2 from a");
    write(&b.join("notes.txt"), "n2 from b");
    confirm(&engine, &id);
    assert_eq!(read(&b.join("plan.txt")), "v2 from a");
    assert_eq!(read(&a.join("notes.txt")), "n2 from b");
    assert_eq!(trashed(&b, "plan.txt"), vec!["v1".to_string()], "b's replaced version is kept");
    assert_eq!(trashed(&a, "notes.txt"), vec!["n1".to_string()], "a's replaced version is kept");
}

#[test]
fn newest_wins_keeps_the_losing_version_in_the_trash_even_with_safe_deletes_off() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("plan.txt"), "v1");
    let engine = plain_engine(&data);
    let mut setup = job_setup(&a, &b, SyncMode::TwoWay);
    setup.safe_delete = false;
    setup.conflict = ConflictRule::NewestWins;
    let id = add(&engine, setup);
    confirm(&engine, &id);
    let now = SystemTime::now();
    write(&a.join("plan.txt"), "edited on a");
    set_mtime(&a.join("plan.txt"), now + Duration::from_secs(100));
    write(&b.join("plan.txt"), "edited on b, later");
    set_mtime(&b.join("plan.txt"), now + Duration::from_secs(200));
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.newest_wins, 1, "{:?}", review.steps);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    assert_eq!(read(&a.join("plan.txt")), "edited on b, later");
    assert_eq!(trashed(&a, "plan.txt"), vec!["edited on a".to_string()], "the losing version is kept");
}

// ------------------------------------------------------------------ conflicts left for the person

#[test]
fn a_conflict_is_left_alone_and_settled_only_as_the_person_saw_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, data) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("data"));
    write(&a.join("plan.txt"), "v1");
    let engine = plain_engine(&data);
    let mut setup = job_setup(&a, &b, SyncMode::TwoWay);
    setup.conflict = ConflictRule::Ask;
    setup.safe_delete = false;
    let id = add(&engine, setup);
    confirm(&engine, &id);

    // Edited on both sides: with "Ask", the sync leaves both versions as they are.
    write(&a.join("plan.txt"), "edited on a");
    write(&b.join("plan.txt"), "edited on b, longer");
    let review = engine.prepare_review(&id).unwrap();
    assert_eq!(review.tally.conflicts, 1, "{:?}", review.steps);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    assert_eq!(read(&a.join("plan.txt")), "edited on a", "Ask leaves both versions alone");
    assert_eq!(read(&b.join("plan.txt")), "edited on b, longer");
    let card = view(&engine, &id).conflicts.first().cloned().expect("the row shows the conflict");

    // b changes again after the person saw the card: the card can't be settled anymore.
    write(&b.join("plan.txt"), "edited on b again, after the card was shown");
    match engine.prepare_resolution(&id, &card, ConflictChoice::KeepSource) {
        Err(Error::ReviewOutdated(_)) => {}
        other => panic!("a conflict that changed was settled: {:?}", other.map(|r| r.total_steps)),
    }
    assert!(!engine.is_running(&id));
    assert_eq!(read(&b.join("plan.txt")), "edited on b again, after the card was shown");
    assert_eq!(read(&a.join("plan.txt")), "edited on a");

    // Settled from a fresh card: the version that loses is kept in the trash, even with
    // safe deletes off.
    let card = view(&engine, &id).conflicts.first().cloned().expect("the row shows the conflict as it is now");
    let review = engine.prepare_resolution(&id, &card, ConflictChoice::KeepSource).unwrap();
    assert!(review.blocked.is_none(), "{:?}", review.blocked);
    engine.approve(&id, &review.review_id, true).unwrap();
    wait_idle(&engine, &id);
    assert_eq!(read(&a.join("plan.txt")), "edited on a");
    assert_eq!(read(&b.join("plan.txt")), "edited on a", "the person chose the source's version");
    assert_eq!(trashed(&b, "plan.txt"), vec!["edited on b again, after the card was shown".to_string()], "b's version is kept");
    let next = engine.prepare_review(&id).unwrap();
    assert_eq!(next.total_steps, 0, "settled: {:?}", next.steps);
}

// ------------------------------------------------------------------ quitting and stopping

#[test]
fn the_persons_stop_makes_the_next_sync_wait_for_an_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("first.txt"), "first");
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let mut setup = job_setup(&src, &dst, SyncMode::Backup);
    setup.full_automation = true;
    let id = add(engine, setup);
    confirm(engine, &id);
    for n in 0..3 {
        write(&src.join(format!("new-{n}.txt")), "new");
    }
    let (stopper, stopped) = (engine.clone(), id.clone());
    rig.at_first_step(move || stopper.cancel(&stopped));
    let_progress_throttle_pass();
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait_idle(engine, &id);
    assert!(rig.hook_fired());
    let job = view(engine, &id);
    assert!(job.job.last_run.unwrap().cancelled);
    assert!(job.job.needs_review && !job.runs_by_itself, "the next sync waits for an OK");
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Review);
    engine.want(&id, "for a test");
    engine.start_wanted(&id);
    assert!(!engine.is_running(&id));
    // Still so after a restart.
    drop(rig);
    let engine = reopen(&data);
    assert!(view(&engine, &id).job.needs_review);
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Review);
}

#[test]
fn quitting_stops_a_run_without_making_the_next_one_wait_for_an_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
    write(&src.join("first.txt"), "first");
    let rig = Rig::open(&data);
    let engine = &rig.engine;
    let mut setup = job_setup(&src, &dst, SyncMode::Backup);
    setup.full_automation = true;
    let id = add(engine, setup);
    confirm(engine, &id);
    for n in 0..3 {
        write(&src.join(format!("new-{n}.txt")), "new");
    }
    let quitter = engine.clone();
    rig.at_first_step(move || {
        // Charon quits while the run goes (the run is on this very thread, so don't wait).
        quitter.shut_down(Duration::ZERO);
    });
    let_progress_throttle_pass();
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait_idle(engine, &id);
    assert!(rig.hook_fired());
    let job = view(engine, &id);
    assert!(job.job.last_run.unwrap().cancelled, "the run stopped");
    assert!(!job.job.needs_review && job.job.held.is_none(), "a quit is not the person pressing Stop");

    // The next start: the job runs by itself as before and finishes what was left.
    drop(rig);
    let engine = reopen(&data);
    assert!(view(&engine, &id).runs_by_itself);
    assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Started);
    wait_idle(&engine, &id);
    for n in 0..3 {
        assert!(dst.join(format!("new-{n}.txt")).exists());
    }
}
