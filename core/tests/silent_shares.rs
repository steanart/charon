//! End-to-end: a network share whose server stopped answering. Every question about a folder
//! on it (is it there, where does it lead, which drive is it on) waits until the server is
//! back, which on a "hard" NFS mount, the usual kind on Linux, can take hours. Such a share
//! must hold up neither the window, nor the autopilot, nor the other jobs: its job's row says
//! it doesn't answer, and the other jobs are looked at, listed and synced as ever.
//!
//! A FUSE drive whose program never answers plays the share. Mounting one takes special
//! rights (CAP_SYS_ADMIN, and /dev/fuse): without them the test skips, and writes a line
//! starting with SKIPPED straight to stderr, or fails when CHARON_REQUIRE_PERMISSION_TESTS=1.

#![cfg(target_os = "linux")]

use charon_core::model::{JobStatus, SyncMode};
use charon_core::{Autopilot, ConflictRule, Engine, EngineEvent, JobSetup, Trigger};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// How long anything here may take: the time limit on questions about folders (a few
/// seconds), the autopilot's wait after launch, and some to spare. Without that time limit,
/// the calls never come back while the share is silent.
const PATIENCE: Duration = Duration::from_secs(30);

/// A drive mounted at a folder, whose program never answers: every question about the folder
/// waits, as on a network share whose server went away. Unmounted when dropped: closing the
/// drive's device first ends every question still waiting, with an error.
struct SilentShare {
    at: CString,
    device: Option<File>,
}

impl SilentShare {
    /// Mounts the drive over the folder `at`, or says why it can't.
    fn mount(at: &Path) -> Result<Self, String> {
        let device = OpenOptions::new().read(true).write(true).open("/dev/fuse").map_err(|e| format!("/dev/fuse can't be opened ({e})"))?;
        let target = CString::new(at.as_os_str().as_bytes()).unwrap();
        // SAFETY: both only read who this process runs as.
        let (user, group) = unsafe { (libc::geteuid(), libc::getegid()) };
        let options = CString::new(format!("fd={},rootmode=40000,user_id={user},group_id={group}", device.as_raw_fd())).unwrap();
        let (source, kind) = (CString::new("charon-silent-share").unwrap(), CString::new("fuse").unwrap());
        // SAFETY: every string is NUL-terminated and outlives the call.
        let mounted =
            unsafe { libc::mount(source.as_ptr(), target.as_ptr(), kind.as_ptr(), libc::MS_NOSUID | libc::MS_NODEV, options.as_ptr().cast()) };
        if mounted != 0 {
            return Err(format!("a FUSE drive can't be mounted ({})", io::Error::last_os_error()));
        }
        Ok(Self { at: target, device: Some(device) })
    }
}

impl Drop for SilentShare {
    fn drop(&mut self) {
        drop(self.device.take());
        // SAFETY: `at` is NUL-terminated.
        unsafe { libc::umount2(self.at.as_ptr(), libc::MNT_DETACH) };
    }
}

/// The silent share mounted at `at`. When it can't be, the test says so where everyone sees
/// it and skips, or fails when CHARON_REQUIRE_PERMISSION_TESTS=1.
fn silent_share(test: &str, at: &Path) -> Option<SilentShare> {
    let why = match SilentShare::mount(at) {
        Ok(share) => return Some(share),
        Err(why) => why,
    };
    let how = "as root, on a system with FUSE: cargo test -p charon-core --test silent_shares";
    if std::env::var("CHARON_REQUIRE_PERMISSION_TESTS").is_ok_and(|v| v == "1") {
        panic!("{test} can't run: {why}. CHARON_REQUIRE_PERMISSION_TESTS=1 forbids skipping it. Run it {how}");
    }
    // Straight to stderr: the test harness swallows output of a test that passes, and a
    // skipped test must never look like one that ran.
    let _ = writeln!(
        io::stderr(),
        "SKIPPED {test}: {why}, and a share that doesn't answer can't be played without one. It did NOT run. Run it {how} \
         (or set CHARON_REQUIRE_PERMISSION_TESTS=1 to fail instead of skipping)"
    );
    None
}

/// What `work` gives, done on a thread of its own: the test fails when it takes longer than
/// `PATIENCE`, as it then waits for the share. (Dropping the share ends that wait.)
fn within<T: Send + 'static>(what: &str, work: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, answer) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(work());
    });
    answer.recv_timeout(PATIENCE).unwrap_or_else(|_| panic!("{what} waited for the share that doesn't answer"))
}

/// Waits until `done`, at most `PATIENCE`.
fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !done() {
        assert!(Instant::now() < deadline, "{what} took too long");
        thread::sleep(Duration::from_millis(20));
    }
}

fn setup(name: &str, source: &Path, destination: &Path, mode: SyncMode) -> JobSetup {
    JobSetup {
        name: name.into(),
        source: source.to_path_buf(),
        destination: destination.to_path_buf(),
        mode,
        conflict: ConflictRule::Ask,
        trigger: Trigger::DriveConnected,
        excludes: JobSetup::default_excludes(),
        safe_delete: true,
        full_automation: false,
    }
}

#[test]
fn a_share_that_doesnt_answer_holds_up_neither_the_window_nor_the_other_jobs() {
    let tmp = tempfile::tempdir().unwrap();
    let [home, usb, share, copy] = ["home", "usb", "share", "copy"].map(|name| tmp.path().join(name));
    for folder in [&home, &usb, &share, &copy] {
        fs::create_dir_all(folder).unwrap();
    }
    fs::write(home.join("letter.txt"), "letter").unwrap();
    let engine = Engine::open(tmp.path().join("data"), Arc::new(|_: EngineEvent| {})).unwrap();
    // The share's job comes first, so the autopilot meets it before the other job. And a
    // Mirror job's looks compare its folders with every other job's (see `clashes_with`).
    let shared = engine.add_job(setup("Share", &share, &copy, SyncMode::TwoWay)).unwrap().job.id;
    let local = engine.add_job(setup("Home", &home, &usb, SyncMode::Mirror)).unwrap().job.id;
    until("the first looks", || [&local, &shared].iter().all(|id| !engine.job(id).unwrap().looking));

    // The share's server goes away.
    let Some(silent) = silent_share("a_share_that_doesnt_answer_holds_up_neither_the_window_nor_the_other_jobs", &share) else { return };
    fs::write(home.join("second letter.txt"), "second letter").unwrap();

    // The window's rows and the tray's overview: the share's row says it doesn't answer.
    let rows = within("the rows", {
        let engine = engine.clone();
        move || engine.jobs()
    });
    let share_row = rows.iter().find(|row| row.job.id == shared).unwrap();
    assert_eq!(share_row.status, JobStatus::Offline);
    assert!(share_row.problem.as_deref().is_some_and(|problem| problem.contains("doesn't answer")), "{:?}", share_row.problem);
    let overview = within("the overview", {
        let engine = engine.clone();
        move || engine.overview()
    });
    assert_eq!(overview.offline, 1);

    // The autopilot looks at the other job by itself, as at every launch with its drive there.
    Autopilot::start(engine.clone());
    until("the autopilot's look at the other job", || {
        within("the other job's row", {
            let (engine, local) = (engine.clone(), local.clone());
            move || engine.job(&local).unwrap().waiting.is_some_and(|waiting| waiting.items == 2)
        })
    });

    // The other job's list is made, confirmed and carried out.
    let review = within("the other job's list", {
        let (engine, local) = (engine.clone(), local.clone());
        move || engine.prepare_review(&local).unwrap()
    });
    within("the other job's sync", {
        let (engine, local) = (engine.clone(), local.clone());
        move || {
            engine.approve(&local, &review.review_id, true).unwrap();
            while engine.is_running(&local) {
                thread::sleep(Duration::from_millis(20));
            }
        }
    });
    assert!(usb.join("letter.txt").is_file() && usb.join("second letter.txt").is_file());
    drop(silent);
}
