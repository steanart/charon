//! Questions to the disk about the jobs' folders, with a time limit.
//!
//! A network share whose server stopped answering can hold up any question about a folder
//! on it (is it there, which drive is it on, where does it really lead) for minutes, or
//! until the server comes back (a "hard" NFS mount, the usual kind on Linux). Charon asks
//! such questions about every job's folders all the time: the autopilot every second, every
//! row the window shows, every sync that starts and checks which other jobs use its
//! folders. So each question is asked on a thread of its own, and waited for at most a
//! moment (`LIMIT`): a folder that doesn't answer in time counts as out of reach. While a
//! question about it is still out, the folder isn't asked again: it counts as out of reach
//! at once. So a share that doesn't answer holds up nothing but the one thread still
//! waiting for its answer.

use crate::engine::Engine;
use crate::model::{Folder, Job};
use crate::plan::Side;
use crate::scan::Volume;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// How long a question about a folder is waited for. A share that answers at all usually
/// answers far sooner; one that must wake its disks first can take a few seconds.
pub(crate) const LIMIT: Duration = Duration::from_secs(10);

/// The folders with questions still out past their time limit, with how many.
type Late = Arc<Mutex<HashMap<PathBuf, usize>>>;

/// Asks the disk about folders, with a time limit (see the module's notes).
pub(crate) struct Reach {
    limit: Duration,
    late: Late,
}

impl Reach {
    pub fn new(limit: Duration) -> Self {
        Self { limit, late: Arc::default() }
    }

    /// Asks `question` about `folder`, and waits for the answer at most the time limit.
    /// `None` when the folder doesn't answer in time, or hasn't answered an earlier
    /// question yet.
    pub fn ask<T: Send + 'static>(&self, folder: &Path, question: impl FnOnce(&Path) -> T + Send + 'static) -> Option<T> {
        self.start(folder, question).answer(Instant::now() + self.limit)
    }

    /// Asks `question` about each of `folders` at once, and waits for the answers at most
    /// the time limit in all.
    pub fn ask_each<T: Send + 'static>(&self, folders: &[&Path], question: fn(&Path) -> T) -> Vec<Option<T>> {
        let asked: Vec<Question<T>> = folders.iter().map(|folder| self.start(folder, question)).collect();
        let by = Instant::now() + self.limit;
        asked.into_iter().map(|question| question.answer(by)).collect()
    }

    /// Whether a question about `folder` is still out past its time limit.
    pub fn is_late(&self, folder: &Path) -> bool {
        lock(&self.late).contains_key(folder)
    }

    /// Starts asking `question` about `folder` on a thread of its own.
    fn start<T: Send + 'static>(&self, folder: &Path, question: impl FnOnce(&Path) -> T + Send + 'static) -> Question<T> {
        let mut asked = Question { folder: folder.to_path_buf(), answer: Answer::Late, late: self.late.clone() };
        if self.is_late(folder) {
            return asked;
        }
        let (sender, answer) = mpsc::sync_channel(1);
        let (late, place) = (self.late.clone(), folder.to_path_buf());
        // The question goes in a box the thread takes it from, so it can still be asked
        // here if no thread can be started.
        let question = Arc::new(Mutex::new(Some(question)));
        let handed = question.clone();
        let spawned = thread::Builder::new().name("charon-folder".into()).spawn(move || {
            let Some(question) = lock(&handed).take() else { return };
            // A question that ends in a panic (a mistake in Charon itself) answers nothing.
            let found = std::panic::catch_unwind(AssertUnwindSafe(|| question(&place))).ok();
            // Sent with the count locked: the asker either takes the answer, or has counted
            // the question as late and gone (then the count goes down again here).
            let mut late = lock(&late);
            if sender.send(found).is_err() {
                if let Some(count) = late.get_mut(&place) {
                    *count -= 1;
                    if *count == 0 {
                        late.remove(&place);
                    }
                }
            }
        });
        asked.answer = match spawned {
            Ok(_) => Answer::Coming(answer),
            // No thread to spare: asked here, the way it was before there was a time limit.
            Err(_) => match lock(&question).take() {
                Some(question) => Answer::Given(Some(question(folder))),
                None => Answer::Late,
            },
        };
        asked
    }
}

/// A question about one folder, on its way.
struct Question<T> {
    folder: PathBuf,
    answer: Answer<T>,
    late: Late,
}

/// An answer is `None` when the question ended in a panic.
enum Answer<T> {
    /// The thread asking it will send the answer.
    Coming(Receiver<Option<T>>),
    /// It was answered here.
    Given(Option<T>),
    /// The folder still hasn't answered an earlier question.
    Late,
}

impl<T> Question<T> {
    /// The answer, if it comes by `by`. One that doesn't is counted as late, until it comes.
    fn answer(self, by: Instant) -> Option<T> {
        let receiver = match self.answer {
            Answer::Coming(receiver) => receiver,
            Answer::Given(found) => return found,
            Answer::Late => return None,
        };
        match receiver.recv_timeout(by.saturating_duration_since(Instant::now())) {
            Ok(found) => return found,
            // The thread always answers, if only with `None`; gone without one, it's not late.
            Err(RecvTimeoutError::Disconnected) => return None,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let mut late = lock(&self.late);
        // It may have come just now.
        match receiver.try_recv() {
            Ok(found) => return found,
            Err(TryRecvError::Disconnected) => return None,
            Err(TryRecvError::Empty) => {}
        }
        *late.entry(self.folder).or_default() += 1;
        // The receiver goes while the count is locked: the thread's answer then can't be sent.
        drop(receiver);
        None
    }
}

impl Engine {
    /// Looks at both folders of each of `jobs`, all at once: waits at most `LIMIT` in all,
    /// and a folder that doesn't answer by then counts as `Presence::Silent`. A folder on a
    /// drive a sync keeps quiet isn't asked: what was found before is said (see `quiet.rs`).
    pub(crate) fn whereabouts_of<'a>(&self, jobs: impl IntoIterator<Item = &'a Job>) -> Vec<Whereabouts> {
        let folders: Vec<&Path> = jobs.into_iter().flat_map(|job| [job.setup.source.as_path(), job.setup.destination.as_path()]).collect();
        let found = self.inner.quiet.presences(&folders, |asked| self.inner.reach.ask_each(asked, Presence::of));
        let found: Vec<Presence> = found.into_iter().map(|p| p.unwrap_or(Presence::Silent)).collect();
        found.chunks_exact(2).map(|pair| Whereabouts { source: pair[0].clone(), destination: pair[1].clone() }).collect()
    }

    /// Looks at both folders of `job` (see `whereabouts_of`).
    pub(crate) fn whereabouts(&self, job: &Job) -> Whereabouts {
        let mut found = self.whereabouts_of([job]);
        found.pop().unwrap_or(Whereabouts { source: Presence::Silent, destination: Presence::Silent })
    }

    /// Each of `paths` as a folder two paths can be compared in (see `Folder::of`), all at
    /// once: `None` for one that doesn't answer in time. One on a drive a sync keeps quiet
    /// isn't asked: it's as found before, or `None` (see `quiet.rs`).
    pub(crate) fn folders_of(&self, paths: &[&Path]) -> Vec<Option<Folder>> {
        self.inner.quiet.folders(paths, |asked| self.inner.reach.ask_each(asked, Folder::of))
    }

    /// Asks `question` about `folder` with the time limit: `None` when it doesn't answer. One
    /// on a drive a sync keeps quiet isn't asked, and is `None` too (see `quiet.rs`).
    pub(crate) fn ask_about<T: Send + 'static>(&self, folder: &Path, question: impl FnOnce(&Path) -> T + Send + 'static) -> Option<T> {
        if self.kept_quiet(folder) {
            return None;
        }
        self.inner.reach.ask(folder, question)
    }
}

#[cfg(test)]
impl Engine {
    /// Makes `folder` stop answering, as a share whose server went away does, until the
    /// sender this returns is used or dropped.
    pub(crate) fn silence(&self, folder: &Path) -> mpsc::Sender<()> {
        let (answer, wait) = mpsc::channel::<()>();
        let asked = self.inner.reach.start(folder, move |_| {
            let _ = wait.recv();
        });
        assert!(asked.answer(Instant::now()).is_none(), "the folder answered");
        answer
    }

    /// Whether a question about `folder` is still out past its time limit.
    pub(crate) fn is_silent(&self, folder: &Path) -> bool {
        self.inner.reach.is_late(folder)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a look at one of a job's folders found.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Presence {
    /// It's there, on this drive (when the system can tell).
    There(Option<Volume>),
    /// It isn't there, or isn't a folder.
    Away,
    /// It didn't answer in time (a network share whose server is away, say).
    Silent,
}

impl Presence {
    /// Looks at `folder` (asks the disk; see `Reach`).
    pub fn of(folder: &Path) -> Presence {
        if folder.is_dir() {
            Presence::There(Volume::basics(folder))
        } else {
            Presence::Away
        }
    }

    pub fn is_there(&self) -> bool {
        matches!(self, Presence::There(_))
    }
}

/// What a look at both of a job's folders found.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Whereabouts {
    pub source: Presence,
    pub destination: Presence,
}

impl Whereabouts {
    pub fn side(&self, side: Side) -> &Presence {
        match side {
            Side::Source => &self.source,
            Side::Destination => &self.destination,
        }
    }

    /// Both folders are there.
    pub fn both_there(&self) -> bool {
        self.source.is_there() && self.destination.is_there()
    }

    /// The side whose folder is now on another drive than at the last sync of `job`, with
    /// that drive: an unplugged drive's leftover mount folder, or another disk under the name.
    pub fn moved(&self, job: &Job) -> Option<(Side, Volume, Volume)> {
        let drives = job.drives.as_ref()?;
        [(Side::Source, &drives.source), (Side::Destination, &drives.destination)].into_iter().find_map(|(side, before)| {
            let Presence::There(Some(now)) = self.side(side) else { return None };
            (!crate::engine::Engine::same_drive(before, now)).then(|| (side, before.clone(), now.clone()))
        })
    }

    /// Both folders are there, on the drives of the last sync: the job's folders, which can
    /// be looked at and synced.
    pub fn reachable(&self, job: &Job) -> bool {
        self.both_there() && self.moved(job).is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::status::SOURCE_SILENT;
    use crate::model::{ConflictRule, JobSetup, JobStatus, SyncMode, Trigger};
    use std::fs;
    use std::sync::mpsc::Sender;

    const SHORT: Duration = Duration::from_millis(200);

    /// A question that waits until it is told to answer.
    fn held_up(answer: u32) -> (impl FnOnce(&Path) -> u32 + Send + 'static, Sender<()>) {
        let (go, wait) = mpsc::channel::<()>();
        (
            move |_: &Path| {
                let _ = wait.recv();
                answer
            },
            go,
        )
    }

    fn setup(name: &str, source: &Path, destination: &Path) -> JobSetup {
        JobSetup {
            name: name.into(),
            source: source.to_path_buf(),
            destination: destination.to_path_buf(),
            mode: SyncMode::Backup,
            conflict: ConflictRule::Ask,
            trigger: Trigger::Manual,
            excludes: vec![],
            safe_delete: true,
            full_automation: false,
        }
    }

    fn until(done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "took too long");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_folder_that_answers_in_time_gives_its_answer() {
        let reach = Reach::new(SHORT);
        assert_eq!(reach.ask(Path::new("/share"), |_| 7), Some(7));
        assert!(!reach.is_late(Path::new("/share")));
    }

    #[test]
    fn a_folder_that_doesnt_answer_holds_up_nothing_until_it_answers_again() {
        let reach = Reach::new(SHORT);
        let share = Path::new("/share");
        let (question, answer_now) = held_up(1);
        let asked = Instant::now();
        assert_eq!(reach.ask(share, question), None);
        assert!(asked.elapsed() < SHORT * 5, "waited {:?}", asked.elapsed());
        assert!(reach.is_late(share));
        // While it still hasn't answered, it isn't asked again: no waiting at all.
        let again = Instant::now();
        assert_eq!(reach.ask(share, |_| 2), None);
        assert!(again.elapsed() < SHORT, "waited {:?}", again.elapsed());
        // Other folders answer as ever.
        assert_eq!(reach.ask(Path::new("/home/me/Documents"), |_| 3), Some(3));
        // Once the first question is answered, the folder is asked again.
        answer_now.send(()).unwrap();
        until(|| !reach.is_late(share));
        assert_eq!(reach.ask(share, |_| 4), Some(4));
    }

    #[test]
    fn a_question_that_ends_in_a_panic_answers_nothing_and_never_leaves_its_folder_late() {
        let reach = Reach::new(SHORT);
        let share = Path::new("/share");
        assert_eq!(reach.ask(share, |_| -> u32 { panic!("a mistake in a question") }), None);
        assert!(!reach.is_late(share));
        // One that ends in a panic after its time limit.
        let (go, wait) = mpsc::channel::<()>();
        let late_mistake = move |_: &Path| -> u32 {
            let _ = wait.recv();
            panic!("a mistake in a late question")
        };
        assert_eq!(reach.ask(share, late_mistake), None);
        assert!(reach.is_late(share));
        go.send(()).unwrap();
        until(|| !reach.is_late(share));
        assert_eq!(reach.ask(share, |_| 5), Some(5));
    }

    #[test]
    fn a_job_whose_folder_doesnt_answer_is_offline_and_holds_up_no_row() {
        let tmp = tempfile::tempdir().unwrap();
        let [home, usb, share, copy] = ["home", "usb", "share", "copy"].map(|name| tmp.path().join(name));
        for folder in [&home, &usb, &share, &copy] {
            fs::create_dir_all(folder).unwrap();
        }
        let engine = Engine::open(tmp.path().join("data"), Arc::new(|_| {})).unwrap();
        let local = engine.add_job(setup("Home", &home, &usb)).unwrap().job.id;
        let shared = engine.add_job(setup("Share", &share, &copy)).unwrap().job.id;
        let answer = engine.silence(&share);

        // No row waits for the share.
        let asked = Instant::now();
        let rows = engine.jobs();
        let overview = engine.overview();
        let row = engine.job(&shared).unwrap();
        // "Sync all" offers the job whose folders are there, not the one waiting for the share.
        assert_eq!(engine.sync_all().unwrap(), vec![local.clone()]);
        assert!(asked.elapsed() < LIMIT, "waited {:?}", asked.elapsed());
        assert_eq!(row.status, JobStatus::Offline);
        assert_eq!(row.problem.as_deref(), Some(SOURCE_SILENT));
        assert!(!row.source_online && row.destination_online);
        assert_eq!(overview.offline, 1);
        let home_row = rows.iter().find(|row| row.job.id == local).unwrap();
        assert_ne!(home_row.status, JobStatus::Offline);

        // Once the share answers again, so does its row.
        answer.send(()).unwrap();
        until(|| !engine.is_silent(&share));
        assert_ne!(engine.job(&shared).unwrap().status, JobStatus::Offline);
    }

    #[test]
    fn folders_asked_together_wait_once_in_all() {
        let reach = Reach::new(SHORT);
        let (first, answer_first) = held_up(1);
        let (second, answer_second) = held_up(2);
        let started = Instant::now();
        let (a, b) = (reach.start(Path::new("/a"), first), reach.start(Path::new("/b"), second));
        let by = Instant::now() + SHORT;
        assert_eq!((a.answer(by), b.answer(by)), (None, None));
        assert!(started.elapsed() < SHORT * 2, "waited {:?}", started.elapsed());
        answer_first.send(()).unwrap();
        answer_second.send(()).unwrap();
        until(|| !reach.is_late(Path::new("/a")) && !reach.is_late(Path::new("/b")));
        let found = reach.ask_each(&[Path::new("/a"), Path::new("/b")], |folder| folder.to_path_buf());
        assert_eq!(found, vec![Some(PathBuf::from("/a")), Some(PathBuf::from("/b"))]);
    }
}
