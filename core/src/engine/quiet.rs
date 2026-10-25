//! Drives kept quiet while a sync writes to them.
//!
//! A drive run by a driver Apple didn't make (Paragon's or Tuxera's NTFS, macFUSE: see
//! `Volume::run_by_an_add_on_driver`) works inside the system's own core, where a mistake
//! brings the whole Mac down. Paragon's NTFS driver did, a few seconds into a sync onto a USB
//! stick: it threw away one of its locks while Spotlight, reading the files the sync had just
//! written, still waited for it. Only the driver's maker can fix the driver, and Charon can't
//! keep the system's own services away, but it can keep from adding work of its own there.
//! So Charon works on one job at a time on such a drive (see `Engine::drive_used_by_another`):
//! while a job's run (its look, then its sync) or a list being made for it reads the drive,
//! no other job's run, list or look starts there. What Charon starts by itself (a look in the
//! background, a run a trigger started) also waits for another job's look there, unless that
//! look stopped moving on: it's most likely waiting for a folder that doesn't answer, not
//! reading the drive, and whoever goes ahead of it stops it. And it waits its turn: jobs held
//! back there go in the order they began to wait, whatever their order in the list, so no job
//! keeps another off the drive for good.
//! A run or a list the person asked for waits neither for a look nor for its turn: it stops
//! the looks in the background there, and they come again once it's done. A run that waits
//! says so in its row, and starts by itself in its turn. And while a sync writes to the
//! drive, its thread is meant to be the only one of Charon's at work there:
//! * the sync refreshes its busy notes there itself, as it goes (see `run::Claim::beat`; the
//!   heartbeat does only when the sync is held up so long that they'd go stale);
//! * what the window, the autopilot and the reminders ask about folders on the drive is
//!   answered from what was found before (see `Engine::whereabouts_of`, `Engine::folders_of`),
//!   and what another job's look would ask about one isn't asked (see `Engine::ask_about`):
//!   whether a folder there holds a copy of a marker is as the disk last told it apart (see
//!   `QuietDrives::origin`; a run nobody looks at that can't tell, and has no other reason
//!   to hold back, waits for the sync, see `overlaps::Waiting`), and where a folder a sync
//!   client syncs there really is, as the client wrote it (see `Scanner::keeping_off`);
//! * a file a stopped look still holds open there is closed once the sync is done (see
//!   `Engine::let_go`), as closing it is work for the driver too;
//! * the autopilot stops watching the folders on the drive for changes until the sync is done.
//!   What changes there meanwhile isn't heard, as on a network share: the next regular look
//!   finds it, at most one look interval after the folders stopped being watched, or after
//!   the sync ended when it left something (see `Engine::finish`). A list made before can
//!   still be confirmed, and its folders are looked at again once it's carried out (see
//!   `Engine::let_looks_end`).
//!
//! Before it writes, the sync waits a moment for other looks there to stop (not for one that
//! stopped moving on, which is only stopped) and for the autopilot to stop watching; when
//! either takes too long, it writes all the same, and the app's log says so.
//!
//! A drive is told by where it's mounted, as the disk last said: a folder that leads there
//! through a link is asked about as ever, and so is one Charon knows nothing of yet. The lock
//! here is taken last: no other lock is taken while it's held.

use crate::engine::reach::Presence;
use crate::engine::runtime::JobRuntime;
use crate::engine::survey::Survey;
use crate::engine::Engine;
use crate::error::{Error, Result};
use crate::model::{Folder, Job, JobId};
use crate::plan::Side;
use crate::run::{Claim, Origin};
use crate::scan::Volume;
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// How long a sync waits for the looks it stopped to end, before it writes to a drive it
/// keeps quiet. A look stops before its next step, so this is only a bound for a slow drive.
const LOOKS_END_WITHIN: Duration = Duration::from_secs(5);
/// How long a sync waits for the autopilot to stop watching the folders on a drive it keeps
/// quiet, before it writes there. The autopilot does at its next tick, every second, unless a
/// share that doesn't answer holds it up.
const UNWATCHED_WITHIN: Duration = Duration::from_secs(3);
/// The name of the autopilot's thread, which follows the drives kept quiet itself.
pub(crate) const AUTOPILOT_THREAD: &str = "charon-autopilot";
/// How long a background look that stopped moving on (no items scanned, no pair or piece of a
/// file compared, nothing gone through in a trash, no file it couldn't read before tried again)
/// holds a drive run by an add-on driver: one that doesn't move on for this long is most
/// likely stuck, waiting for something that doesn't answer (a network share on its other
/// side), not reading the drive. A slow look that keeps reading holds it however long it
/// takes.
const STALLED_FOR: Duration = Duration::from_secs(120);

/// Who asks to work on a drive run by an add-on driver, and so what holds them back there
/// (see `Engine::drive_used_among`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Asker {
    /// A run or a list the person asked for: only another job's run or list there holds it
    /// back. It stops other jobs' looks in the background there instead.
    Person,
    /// The person's *Look for changes*: another job's look there holds it back too, unless
    /// that look stopped moving on (see `STALLED_FOR`).
    PersonsLook,
    /// Charon by itself (a look in the background, a run a trigger started): another job's
    /// look holds it back too, unless it stopped moving on, and so does a job that began to
    /// wait for the drive before it.
    Charon,
}

/// Since when a job waits for its turn on a drive run by an add-on driver, and in which of the
/// autopilot's ticks it last asked for it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DriveWait {
    since: Instant,
    asked_in: u64,
}

impl DriveWait {
    /// Whether the job still asks for its turn, in the autopilot's tick `tick`. The autopilot
    /// asks again for every job that waits at every tick: one it didn't ask for in this tick
    /// or the one before has stopped asking (a folder of it went away, its run isn't wanted
    /// anymore), and holds nobody up. Counted in ticks, not in time: one tick can take long
    /// while a share is slow to answer.
    fn asks(&self, tick: u64) -> bool {
        tick <= self.asked_in + 1
    }
}

/// The drives kept quiet now, and what's known of the folders on them.
#[derive(Default)]
pub(crate) struct QuietDrives {
    known: Mutex<Known>,
    /// How many keeps there were: every keep gets the next number, under the lock of `known`.
    keeps: AtomicU64,
    /// How many of them the autopilot followed: it stopped watching their folders.
    followed: AtomicU64,
    /// Whether the autopilot follows them at all (it runs, and watches folders).
    following: std::sync::atomic::AtomicBool,
    /// How many ticks the autopilot began (see `Engine::note_a_tick`).
    ticks: AtomicU64,
    /// How many times looks noted that they moved on, for tests.
    #[cfg(test)]
    moves: AtomicU64,
    /// What a test does each time a look notes that it moved on.
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    when_moved: Mutex<Option<Box<dyn Fn(&Engine, &JobId) + Send + Sync>>>,
}

#[derive(Default)]
struct Known {
    drives: Vec<QuietDrive>,
    /// The disk's last answers about each folder asked about: what's said about a folder of
    /// another job on a drive kept quiet.
    presences: HashMap<PathBuf, Presence>,
    folders: HashMap<PathBuf, Folder>,
    /// Files a stopped look still had open on a drive kept quiet, with their paths: they're
    /// closed once no sync keeps it quiet (see `Engine::let_go`).
    left_open: Vec<(PathBuf, File)>,
    /// Where a job's marker found around another job's folders comes from, by the job, its
    /// folder and the folder it was found in, as the disk last told them apart (a copy, or
    /// the folder itself): what's said while a drive of them is kept quiet (see
    /// `Engine::clashes_by_markers`).
    origins: HashMap<(JobId, PathBuf, PathBuf), Origin>,
}

/// A drive one sync keeps quiet.
struct QuietDrive {
    keep: u64,
    job: JobId,
    /// The job's name, for the rows of the jobs that wait for it.
    name: String,
    /// Where the drive is mounted.
    mount: PathBuf,
    /// The sync's own folders on the drive, with the drive as the sync's look found it, and
    /// the folder as found right before the sync began (`None` when it didn't answer).
    roots: Vec<(PathBuf, Volume, Option<Folder>)>,
}

impl QuietDrive {
    /// Whether `folder` is on this drive: one of the sync's own folders, or a folder under
    /// the place it's mounted.
    fn holds(&self, folder: &Path) -> bool {
        folder.starts_with(&self.mount) || self.roots.iter().any(|(root, ..)| root == folder)
    }
}

impl Known {
    fn drive_of(&self, folder: &Path) -> Option<&QuietDrive> {
        self.drives.iter().find(|drive| drive.holds(folder))
    }

    /// What's said about `folder` while its drive is kept quiet: for the sync's own folders,
    /// there on the drive it found; for another job's, the last answer (there, on a drive not
    /// known, when it was never asked about). `None` when the folder isn't on a quiet drive.
    fn presence(&self, folder: &Path) -> Option<Presence> {
        let drive = self.drive_of(folder)?;
        if let Some((_, volume, _)) = drive.roots.iter().find(|(root, ..)| root == folder) {
            return Some(Presence::There(Some(volume.clone())));
        }
        Some(self.presences.get(folder).cloned().unwrap_or(Presence::There(None)))
    }

    /// `folder` as a folder two paths can be compared in, while its drive is kept quiet:
    /// `Some(None)` when that isn't known, which counts as maybe one place with anything.
    fn folder(&self, folder: &Path) -> Option<Option<Folder>> {
        let drive = self.drive_of(folder)?;
        let last = || self.folders.get(folder).cloned();
        if let Some((.., found)) = drive.roots.iter().find(|(root, ..)| root == folder) {
            return Some(found.clone().or_else(last));
        }
        Some(last())
    }

    /// Where the drive of `folder` is mounted, if it's run by an add-on driver, as far as
    /// Charon knows without asking: the drive kept quiet that holds it, or the disk's last
    /// answer about it.
    fn add_on_mount(&self, folder: &Path) -> Option<PathBuf> {
        if let Some(drive) = self.drive_of(folder) {
            return Some(drive.mount.clone());
        }
        match self.presences.get(folder) {
            Some(Presence::There(Some(volume))) if volume.run_by_an_add_on_driver() => Some(Engine::mount_of(folder, volume)),
            _ => None,
        }
    }
}

impl QuietDrives {
    fn known(&self) -> MutexGuard<'_, Known> {
        self.known.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether `folder` is on a drive kept quiet now.
    pub fn holds(&self, folder: &Path) -> bool {
        self.known().drive_of(folder).is_some()
    }

    /// Whether one of `these` folders and one of `those` are on the same drive run by an
    /// add-on driver, as far as Charon knows without asking (see `Known::add_on_mount`).
    pub fn share_an_add_on_drive(&self, these: &[&Path], those: &[&Path]) -> bool {
        let known = self.known();
        let mounts = |folders: &[&Path]| folders.iter().filter_map(|folder| known.add_on_mount(folder)).collect::<Vec<_>>();
        let (these, those) = (mounts(these), mounts(those));
        these.iter().any(|one| those.iter().any(|other| one.starts_with(other) || other.starts_with(one)))
    }

    /// `folder` as last found (see `Known::folder`), and the name of the job whose sync keeps
    /// its drive quiet, if it's on a drive kept quiet now.
    pub fn remembered(&self, folder: &Path) -> Option<(Option<Folder>, String)> {
        let known = self.known();
        let name = known.drive_of(folder)?.name.clone();
        Some((known.folder(folder).flatten(), name))
    }

    /// Notes where the marker of job `id` found in `found` comes from, when the job's folder
    /// is `root`, as the disk just said.
    pub fn remember_origin(&self, id: &JobId, root: &Path, found: &Path, origin: Origin) {
        self.known().origins.insert((id.clone(), root.to_path_buf(), found.to_path_buf()), origin);
    }

    /// Where the marker of job `id` found in `found` came from when the disk last told it
    /// apart (see `remember_origin`).
    pub fn origin(&self, id: &JobId, root: &Path, found: &Path) -> Option<Origin> {
        self.known().origins.get(&(id.clone(), root.to_path_buf(), found.to_path_buf())).copied()
    }

    /// The name of the job whose sync keeps the drive of one of `folders` quiet, if it isn't
    /// the job `id`.
    pub fn kept_by_another(&self, id: &JobId, folders: &[&Path]) -> Option<String> {
        let known = self.known();
        folders.iter().find_map(|folder| known.drive_of(folder).filter(|drive| &drive.job != id).map(|drive| drive.name.clone()))
    }

    /// Where each of `folders` is: from what's known for those on a drive kept quiet, from
    /// the disk (`ask`) for the others, whose answers are kept for when their drive is.
    pub fn presences(&self, folders: &[&Path], ask: impl FnOnce(&[&Path]) -> Vec<Option<Presence>>) -> Vec<Option<Presence>> {
        self.answer(folders, |known, folder| known.presence(folder).map(Some), |known, folder, found| {
            known.presences.insert(folder.to_path_buf(), found.clone());
        }, ask)
    }

    /// Each of `folders` as a folder two paths can be compared in (see `presences`).
    pub fn folders(&self, folders: &[&Path], ask: impl FnOnce(&[&Path]) -> Vec<Option<Folder>>) -> Vec<Option<Folder>> {
        self.answer(folders, Known::folder, |known, folder, found| {
            known.folders.insert(folder.to_path_buf(), found.clone());
        }, ask)
    }

    fn answer<T: Clone>(
        &self,
        folders: &[&Path],
        from_memory: impl Fn(&Known, &Path) -> Option<Option<T>>,
        remember: impl Fn(&mut Known, &Path, &T),
        ask: impl FnOnce(&[&Path]) -> Vec<Option<T>>,
    ) -> Vec<Option<T>> {
        let mut answers: Vec<Option<Option<T>>> = {
            let known = self.known();
            folders.iter().map(|folder| from_memory(&known, folder)).collect()
        };
        let asked: Vec<&Path> = folders.iter().zip(&answers).filter(|(_, answer)| answer.is_none()).map(|(folder, _)| *folder).collect();
        if !asked.is_empty() {
            let mut found = ask(&asked).into_iter();
            let mut known = self.known();
            for (folder, answer) in folders.iter().zip(answers.iter_mut()).filter(|(_, answer)| answer.is_none()) {
                let given = found.next().flatten();
                if let Some(given) = &given {
                    remember(&mut known, folder, given);
                }
                *answer = Some(given);
            }
        }
        answers.into_iter().map(Option::flatten).collect()
    }
}

/// A sync's keep on the drives it keeps quiet: they're kept quiet until it's dropped.
pub(crate) struct Quiet {
    engine: Engine,
    keep: u64,
    /// When the keep began: the folders on those drives may not have been watched since.
    since: Instant,
    /// The sync's folders on those drives.
    roots: Vec<PathBuf>,
}

impl Quiet {
    /// The sync's folders on the drives it keeps quiet.
    pub fn roots(&self) -> Vec<&Path> {
        self.roots.iter().map(PathBuf::as_path).collect()
    }
}

impl Drop for Quiet {
    /// Ends the keep, and closes the files a stopped look left open on its drives, unless
    /// another keep holds them (outside the lock: closing a file may take a moment).
    fn drop(&mut self) {
        let closing: Vec<(PathBuf, File)> = {
            let mut known = self.engine.inner.quiet.known();
            known.drives.retain(|drive| drive.keep != self.keep);
            let (kept, closing) = std::mem::take(&mut known.left_open).into_iter().partition(|(path, _)| known.drive_of(path).is_some());
            known.left_open = kept;
            closing
        };
        drop(closing);
    }
}

/// A sync's hold on its folders while it runs: its busy notes in them (see `run::Claim`), and
/// its keep on the drives it keeps quiet. Dropped once the sync is done, the notes first.
pub(crate) struct Claimed {
    notes: Claim,
    quiet: Option<Quiet>,
}

impl Claimed {
    pub fn new(notes: Claim, quiet: Option<Quiet>) -> Self {
        Self { notes, quiet }
    }

    /// When the sync began to keep drives quiet, if it does: what changed in its folders
    /// there since then, besides its own items, may not have been heard.
    pub fn unwatched_since(&self) -> Option<Instant> {
        self.quiet.as_ref().map(|quiet| quiet.since)
    }

    /// Refreshes the busy notes on the drives kept quiet when they're due (see `Claim::beat`).
    /// Called by the sync's own thread as it goes.
    pub fn beat(&self) {
        self.notes.beat();
    }
}

impl Engine {
    /// Keeps quiet, for the sync of `survey`, the drives of the job's folders that are run by
    /// an add-on driver, once background looks still reading them have stopped. `None` when
    /// there are none.
    pub(crate) fn keep_quiet(&self, job: &Job, survey: &Survey) -> Result<Option<Quiet>> {
        let sides: Vec<(PathBuf, Volume)> = [Side::Source, Side::Destination]
            .into_iter()
            .filter_map(|side| survey.volume(side).filter(|volume| volume.run_by_an_add_on_driver()).map(|volume| (survey.setup.root(side).to_path_buf(), volume.clone())))
            .collect();
        if sides.is_empty() {
            return Ok(None);
        }
        // Asked before the drives go quiet: other jobs' folders there are compared with these.
        let paths: Vec<&Path> = sides.iter().map(|(root, _)| root.as_path()).collect();
        let folders = self.folders_of(&paths);
        let mut drives: Vec<QuietDrive> = Vec::new();
        for ((root, volume), folder) in sides.into_iter().zip(folders) {
            let mount = Self::mount_of(&root, &volume);
            match drives.iter_mut().find(|drive| drive.mount == mount) {
                Some(drive) => drive.roots.push((root, volume, folder)),
                None => drives.push(QuietDrive { keep: 0, job: job.id.clone(), name: job.setup.name.clone(), mount, roots: vec![(root, volume, folder)] }),
            }
        }
        let roots: Vec<PathBuf> = drives.iter().flat_map(|drive| drive.roots.iter().map(|(root, ..)| root.clone())).collect();
        let keep = {
            let mut known = self.inner.quiet.known();
            let other = known.drives.iter().find(|kept| drives.iter().any(|drive| kept.mount.starts_with(&drive.mount) || drive.mount.starts_with(&kept.mount)));
            if let Some(other) = other {
                return Err(Error::DriveBusy(other.name.clone()));
            }
            // Numbered with the lock held: the autopilot that reads the count before it asks
            // which folders are kept quiet finds every keep counted there.
            let keep = self.inner.quiet.keeps.fetch_add(1, Ordering::SeqCst);
            for drive in &mut drives {
                drive.keep = keep;
            }
            log::info!("job {}: keeps {} quiet while it syncs (a driver Apple didn't make)", job.id, drives.iter().map(|d| d.mount.display().to_string()).collect::<Vec<_>>().join(", "));
            known.drives.extend(drives);
            keep
        };
        let quiet = Quiet { engine: self.clone(), keep, since: Instant::now(), roots };
        self.let_looks_end(job);
        self.wait_until_unwatched(job, keep);
        Ok(Some(quiet))
    }

    /// Waits a moment for the autopilot to stop watching the folders on the drives of the
    /// keep `keep`: watching them has the system work on the drive for Charon too.
    fn wait_until_unwatched(&self, job: &Job, keep: u64) {
        let quiet = &self.inner.quiet;
        if !quiet.following.load(Ordering::SeqCst) || thread::current().name() == Some(AUTOPILOT_THREAD) {
            return;
        }
        let until = Instant::now() + UNWATCHED_WITHIN;
        while quiet.followed.load(Ordering::SeqCst) <= keep {
            if Instant::now() >= until {
                log::warn!("job {}: the folder watcher still follows a drive it keeps quiet", job.id);
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// How many keeps there were so far: read by the autopilot before it asks which folders
    /// are kept quiet (see `kept_quiet`), and handed back to `quiet_followed` once it stopped
    /// watching them.
    pub(crate) fn quiet_keeps(&self) -> u64 {
        self.inner.quiet.keeps.load(Ordering::SeqCst)
    }

    /// The autopilot stopped watching the folders of the first `keeps` keeps.
    pub(crate) fn quiet_followed(&self, keeps: u64) {
        self.inner.quiet.followed.fetch_max(keeps, Ordering::SeqCst);
        self.inner.quiet.following.store(true, Ordering::SeqCst);
    }

    /// Where the drive of `root` (found as `volume`) is mounted; `root` itself when that
    /// can't be told, so nothing but the job's own folder counts as on it.
    fn mount_of(root: &Path, volume: &Volume) -> PathBuf {
        let mount = Path::new(&volume.mount);
        if mount.is_absolute() && mount.parent().is_some() {
            mount.to_path_buf()
        } else {
            root.to_path_buf()
        }
    }

    /// Stops the background looks of other jobs whose folders are on the drives `job`'s sync
    /// now keeps quiet (they count for nothing, and come again once the sync is done), and
    /// waits a moment for them and any look of the job's own (stopped as its run began) to
    /// end, but for a look that stopped moving on (see `stalled`). A look the person asked
    /// for isn't stopped, only waited for. Most were stopped or
    /// waited for as the run began (see `drive_used_by_another`): this is for those that began
    /// before Charon knew the drive.
    ///
    /// The other jobs' folders there aren't watched from now on, so a list of theirs made
    /// before can't be told outgrown by what changes there meanwhile. It isn't refused for
    /// that (each step checks its items right before it acts, and the list is still checked
    /// for its age and for changes heard meanwhile), but once it's carried out, its
    /// folders are looked at again (see `carry_out`).
    fn let_looks_end(&self, job: &Job) {
        let on_drive: Vec<JobId> = self
            .job_list()
            .into_iter()
            .filter(|other| [&other.setup.source, &other.setup.destination].iter().any(|folder| self.kept_quiet(folder)))
            .map(|other| other.id)
            .collect();
        for id in on_drive.iter().filter(|other| *other != &job.id) {
            self.change_known_runtime(id, |rt| {
                if rt.checking.is_some() && rt.preparing.is_none() {
                    rt.outdate_looks();
                    rt.last_checked = None;
                }
                if let Some(review) = &rt.review {
                    review.unheard.store(true, Ordering::Relaxed);
                }
            });
        }
        // A look that stopped moving on most likely waits for its other folder (see
        // `stalled`): stopped, it ends before its next step on either folder.
        let until = Instant::now() + LOOKS_END_WITHIN;
        let looking = || {
            let runtime = self.runtime();
            on_drive.iter().any(|id| runtime.get(id).is_some_and(|rt| (rt.checking.is_some() && !Self::stalled(rt)) || rt.preparing.is_some()))
        };
        while looking() {
            if Instant::now() >= until {
                log::warn!("job {}: a look at a folder on a drive it keeps quiet still reads it", job.id);
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// The name of another job that holds `asker` back on a drive run by an add-on driver
    /// that one of the folders of the job `id` is on (see `drive_used_among`). The job isn't
    /// looked at, listed or synced until that's done (see the module's notes). Its folders
    /// there are still asked about meanwhile (where they are, and whether they overlap another
    /// job's), until a sync keeps the drive quiet: from then on, they're answered from memory.
    pub(crate) fn drive_used_by_another(&self, id: &JobId, asker: Asker) -> Option<String> {
        let state = self.state();
        let runtime = self.runtime();
        let job = state.jobs.iter().find(|job| &job.id == id)?;
        self.drive_used_among(&state.jobs, &runtime, job, asker)
    }

    /// `drive_used_by_another` for `job`, with the live state locked (`jobs`, `runtime`).
    /// Another job on the drive holds `asker` back while its run (its look or its sync) or a
    /// list being made for it is at work there, or its sync keeps the drive quiet. Unless the
    /// person asked for a run or a list, its look in the background holds it back too, but
    /// not one that stopped moving on (`stalled`). And Charon by itself also waits for the
    /// jobs that began to wait for the drive before it and still ask: turns there go in the
    /// order the waits began, whatever the order of the jobs, and whether they wait to look
    /// or to sync (see `note_drive_wait`). The name is of a job at work there when there is
    /// one, for the row that waits says it waits until that job is done.
    pub(crate) fn drive_used_among(&self, jobs: &[Job], runtime: &HashMap<JobId, JobRuntime>, job: &Job, asker: Asker) -> Option<String> {
        let mine = [job.setup.source.as_path(), job.setup.destination.as_path()];
        let looks = asker != Asker::Person;
        let tick = self.tick();
        let my_turn = runtime.get(&job.id).and_then(|rt| rt.drive_wait).filter(|wait| wait.asks(tick)).map_or_else(Instant::now, |wait| wait.since);
        let another_that = |holds: &dyn Fn(&JobRuntime) -> bool| {
            jobs.iter()
                .filter(|other| other.id != job.id && runtime.get(&other.id).is_some_and(holds))
                .find(|other| self.inner.quiet.share_an_add_on_drive(&mine, &[&other.setup.source, &other.setup.destination]))
                .map(|other| other.setup.name.clone())
        };
        another_that(&|rt| rt.is_running() || rt.preparing.is_some() || (looks && rt.is_checking() && !Self::stalled(rt)))
            .or_else(|| self.kept_quiet_by_another(job))
            .or_else(|| {
                let turns = asker == Asker::Charon;
                turns.then(|| another_that(&|rt| rt.drive_wait.is_some_and(|wait| wait.asks(tick) && wait.since < my_turn))).flatten()
            })
    }

    /// Whether the look of `rt` stopped moving on for `STALLED_FOR`: it's most likely stuck,
    /// waiting for a folder that doesn't answer, not reading a drive it shares with other
    /// jobs. Whoever goes ahead of it there stops it (see `stop_looks_sharing_a_drive`), or it
    /// could read the drive again once that folder answers: stopped, it ends before its next
    /// step on either folder (see `Engine::look_at`). One stuck on the drive itself, which
    /// then doesn't answer anyone, is let go the same way.
    fn stalled(rt: &JobRuntime) -> bool {
        rt.look_moved.is_some_and(|moved| moved.elapsed() >= STALLED_FOR)
    }

    /// Notes that the job's look moved on: it scanned more items, compared another pair or
    /// piece of a file, went through another folder or item in a trash, or tried again a file
    /// it couldn't read before (see `stalled`).
    pub(crate) fn note_look_moved(&self, id: &JobId) {
        #[cfg(test)]
        {
            self.inner.quiet.moves.fetch_add(1, Ordering::Relaxed);
            if let Some(then) = &*self.inner.quiet.when_moved.lock().unwrap_or_else(|e| e.into_inner()) {
                then(self, id);
            }
        }
        self.change_known_runtime(id, |rt| rt.look_moved = Some(Instant::now()));
    }

    /// Counts the autopilot's ticks, at the start of each: a job waits for its turn on a drive
    /// run by an add-on driver while the autopilot asks for it at every tick (see `DriveWait`).
    pub(crate) fn note_a_tick(&self) {
        self.inner.quiet.ticks.fetch_add(1, Ordering::Relaxed);
    }

    /// The autopilot's tick now (see `note_a_tick`).
    fn tick(&self) -> u64 {
        self.inner.quiet.ticks.load(Ordering::Relaxed)
    }

    /// Notes that the job of `rt` waits for its turn on a drive run by an add-on driver, as it
    /// was held back there (a look in the background, a run a trigger started, or the
    /// person's *Look for changes*, which the autopilot then takes over as a look of its own):
    /// its turn counts from the first time it was held back, while it keeps asking. Called
    /// with the live state locked.
    pub(crate) fn note_drive_wait(&self, rt: &mut JobRuntime) {
        let tick = self.tick();
        let since = rt.drive_wait.filter(|wait| wait.asks(tick)).map_or_else(Instant::now, |wait| wait.since);
        rt.drive_wait = Some(DriveWait { since, asked_in: tick });
    }

    /// Forgets that the job waits for its turn on a drive: it can't come now (a folder of it
    /// went away).
    pub(crate) fn forget_drive_wait(&self, id: &JobId) {
        self.change_known_runtime(id, |rt| rt.drive_wait = None);
    }

    /// Stops the background looks of other jobs on a drive run by an add-on driver that one
    /// of `job`'s folders is on, as its run, a list being made for it, or its look in the
    /// background begins (the looks still at work there then are those that stopped moving
    /// on): they count for nothing, and come again once it's done. Called with the live state
    /// locked.
    pub(crate) fn stop_looks_sharing_a_drive(&self, jobs: &[Job], runtime: &mut HashMap<JobId, JobRuntime>, job: &Job) {
        let mine = [job.setup.source.as_path(), job.setup.destination.as_path()];
        for other in jobs.iter().filter(|other| other.id != job.id) {
            let Some(rt) = runtime.get_mut(&other.id) else { continue };
            if rt.is_checking() && rt.preparing.is_none() && self.inner.quiet.share_an_add_on_drive(&mine, &[&other.setup.source, &other.setup.destination]) {
                rt.outdate_looks();
                rt.last_checked = None;
            }
        }
    }

    /// Each of `paths` as a folder two paths can be compared in, for saving a job (see
    /// `JobSetup::clashes_as`): from the disk, but as last found for those on a drive a sync
    /// keeps quiet. One of those that was never found can't be compared, so the save waits:
    /// `Err` names the job whose sync keeps the drive quiet.
    pub(crate) fn folders_for_saving(&self, paths: &[&Path]) -> std::result::Result<Vec<Option<Folder>>, String> {
        paths
            .iter()
            .map(|path| match self.inner.quiet.remembered(path) {
                Some((Some(folder), _)) => Ok(Some(folder)),
                Some((None, name)) => Err(name),
                None => Ok(Some(Folder::of(path))),
            })
            .collect()
    }

    /// Closes the files a stopped look still had open (see `Contents::same`), but those on a
    /// drive a sync keeps quiet only once it's done: closing a file is work for its drive's
    /// driver too, and the look may have been stopped for that sync, waiting on a share at
    /// its other side. (Until then, the system can't eject that drive without forcing it.)
    pub(crate) fn let_go(&self, files: Vec<(PathBuf, File)>) {
        let closing: Vec<(PathBuf, File)> = {
            let mut known = self.inner.quiet.known();
            let (kept, closing) = files.into_iter().partition(|(path, _)| known.drive_of(path).is_some());
            known.left_open.extend(kept);
            closing
        };
        drop(closing);
    }

    /// The name of the job whose sync keeps the drive of one of `job`'s folders quiet, if
    /// another job's does: `job` doesn't look or sync until it's done.
    pub(crate) fn kept_quiet_by_another(&self, job: &Job) -> Option<String> {
        self.inner.quiet.kept_by_another(&job.id, &[&job.setup.source, &job.setup.destination])
    }

    /// Whether `folder` is on a drive a sync keeps quiet now (see the module's notes).
    pub(crate) fn kept_quiet(&self, folder: &Path) -> bool {
        self.inner.quiet.holds(folder)
    }
}

#[cfg(test)]
mod testing {
    use super::*;
    use crate::engine::runs::Crossing;
    use crate::engine::survey::Purpose;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// A sync that has looked at its job's folders and claimed them, and goes no further:
    /// dropped, it ends without changing anything.
    pub(crate) struct HeldSync<'a> {
        engine: &'a Engine,
        id: JobId,
        claim: Option<Claimed>,
    }

    impl Drop for HeldSync<'_> {
        fn drop(&mut self) {
            self.claim.take();
            self.engine.change_runtime(&self.id, |rt| {
                Engine::unmark_run(rt);
            });
        }
    }

    impl Engine {
        /// Marks the job as just looked at.
        pub(crate) fn just_looked_at(&self, id: &JobId) {
            self.change_runtime(id, |rt| rt.last_checked = Some(Instant::now()));
        }

        /// Marks a look at the job in the background as at work, as if it read the job's
        /// folders, until `end_a_look`. Returns its stop flag.
        pub(crate) fn begin_a_look(&self, id: &JobId) -> Arc<AtomicBool> {
            let flag = Arc::new(AtomicBool::new(false));
            self.change_runtime(id, |rt| {
                rt.checking = Some(flag.clone());
                rt.look_moved = Some(Instant::now());
            });
            flag
        }

        /// Makes the look `begin_a_look` marked one that stopped moving on (see `stalled`).
        pub(crate) fn a_look_stalls(&self, id: &JobId) {
            self.change_runtime(id, |rt| rt.look_moved = Some(Instant::now().checked_sub(STALLED_FOR).expect("the computer has run for two minutes")));
        }

        /// Ends the look `begin_a_look` marked, as a look that counts.
        pub(crate) fn end_a_look(&self, id: &JobId) {
            self.change_runtime(id, |rt| {
                rt.checking = None;
                rt.last_checked = Some(Instant::now());
            });
        }

        /// Forgets every job's wait for its turn on a drive, as after the first looks (a look
        /// held back then waits for its turn until the autopilot, which a test doesn't run,
        /// asks again).
        pub(crate) fn forget_drive_waits(&self) {
            for rt in self.runtime().values_mut() {
                rt.drive_wait = None;
            }
        }

        /// Whether the job's next look is held back, and no look of it is at work.
        pub(crate) fn look_held_back(&self, id: &JobId) -> bool {
            self.runtime().get(id).is_some_and(|rt| rt.checking.is_none() && rt.last_checked.is_none())
        }

        /// Whether the job has a wanted run that didn't start yet.
        pub(crate) fn run_wanted(&self, id: &JobId) -> bool {
            self.runtime().get(id).is_some_and(|rt| rt.wanted.is_some())
        }

        /// Whether the job's list `review_id` waits for an OK but can't be confirmed anymore,
        /// as its folders may have changed.
        pub(crate) fn list_is_stale(&self, id: &JobId, review_id: &str) -> bool {
            self.runtime().get(id).and_then(|rt| rt.review.as_ref()).is_some_and(|review| review.id == review_id && review.stale.load(Ordering::Relaxed))
        }

        /// Begins a sync of `id` the person asked for, and holds it right after it claimed
        /// its folders (see `HeldSync`).
        pub(crate) fn hold_a_sync(&self, id: &JobId) -> Result<HeldSync<'_>> {
            let (job, cancel) = self.begin_run(id, &Crossing::unattended("for a test", true), |_, _| Ok(()))?;
            let claimed = self.survey(&job, &cancel, Purpose::Automatic).and_then(|survey| self.claim_folders(&job, &survey));
            match claimed {
                Ok(claim) => Ok(HeldSync { engine: self, id: id.clone(), claim: Some(claim) }),
                Err(e) => {
                    self.change_runtime(id, |rt| {
                        Engine::unmark_run(rt);
                    });
                    Err(e)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::runs::Crossing;
    use crate::engine::survey::Purpose;
    use crate::engine::Whereabouts;
    use crate::model::{ConflictRule, JobSetup, SyncMode, Trigger};
    use crate::scan::pretend::AddOnDrive;
    use std::fs;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// Three jobs: "Documents" and "Photos" back up into folders on a stick run by an add-on
    /// driver, "Notes" from one folder of this computer's own disk to another.
    struct Jobs {
        _tmp: tempfile::TempDir,
        _stick: AddOnDrive,
        engine: Engine,
        documents: JobId,
        photos: JobId,
        notes: JobId,
        /// The two jobs' folders on the stick.
        on_stick: [PathBuf; 2],
    }

    fn jobs() -> Jobs {
        let tmp = tempfile::tempdir().unwrap();
        let (home, stick) = (tmp.path().join("home"), tmp.path().join("stick"));
        let pretending = AddOnDrive::at(&stick);
        let folders = [home.join("docs"), stick.join("docs"), home.join("photos"), stick.join("photos"), home.join("notes"), home.join("copies")];
        for folder in &folders {
            fs::create_dir_all(folder).unwrap();
        }
        for source in [&folders[0], &folders[2], &folders[4]] {
            fs::write(source.join("letter.txt"), "Dear…").unwrap();
        }
        let engine = Engine::open(tmp.path().join("data"), Arc::new(|_| {})).unwrap();
        let add = |name: &str, source: &PathBuf, destination: &PathBuf| {
            let setup = JobSetup {
                name: name.into(),
                source: source.clone(),
                destination: destination.clone(),
                mode: SyncMode::Backup,
                conflict: ConflictRule::Ask,
                trigger: Trigger::Manual,
                excludes: vec![],
                safe_delete: true,
                full_automation: false,
            };
            engine.add_job(setup).unwrap().job.id
        };
        let documents = add("Documents", &folders[0], &folders[1]);
        let photos = add("Photos", &folders[2], &folders[3]);
        let notes = add("Notes", &folders[4], &folders[5]);
        until(|| engine.runtime().values().all(|rt| rt.checking.is_none()));
        engine.forget_drive_waits();
        let on_stick = [folders[1].clone(), folders[3].clone()];
        Jobs { _tmp: tmp, _stick: pretending, engine, documents, photos, notes, on_stick }
    }

    fn until(done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "took too long");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_sync_on_a_drive_run_by_an_add_on_driver_keeps_it_quiet_until_it_is_done() {
        let Jobs { _tmp, _stick, engine, documents, photos, notes, on_stick: [docs_on_stick, photos_on_stick] } = jobs();
        let all = engine.job_list();
        let before = engine.whereabouts_of(&all);
        assert!(before.iter().all(Whereabouts::both_there), "{before:?}");
        let held_at = Instant::now();
        let held = engine.hold_a_sync(&documents).unwrap();
        assert!(engine.kept_quiet(&docs_on_stick) && engine.kept_quiet(&photos_on_stick));
        let home_docs = engine.stored_job(&documents).unwrap().setup.source;
        assert!(!engine.kept_quiet(&home_docs), "the source is on another drive");

        // Its busy note on the stick is refreshed by the sync itself as it goes (here, never;
        // the heartbeat would only once that's overdue), the one at home by the heartbeat.
        let heartbeat = |root: &Path| {
            let items = fs::read_dir(root.join(".charon")).unwrap().flatten();
            let file = items.map(|item| item.path()).find(|path| path.extension().is_some_and(|e| e == "busy")).unwrap();
            serde_json::from_slice::<serde_json::Value>(&fs::read(file).unwrap()).unwrap()["heartbeat"].as_str().unwrap().to_string()
        };
        let (on_stick, at_home) = (heartbeat(&docs_on_stick), heartbeat(&home_docs));
        until(|| heartbeat(&home_docs) != at_home);
        thread::sleep(Duration::from_millis(200));
        let still = heartbeat(&docs_on_stick);
        if held_at.elapsed() < crate::run::QUIET_OVERDUE {
            assert_eq!(still, on_stick);
        }

        // Nothing asks the drive anything: folders there that stopped answering are still
        // said to be as they were.
        let silenced = [engine.silence(&docs_on_stick), engine.silence(&photos_on_stick)];
        let found = engine.whereabouts_of(&all);
        assert!(found.iter().all(Whereabouts::both_there), "{found:?}");
        assert!(matches!(&found[0].destination, Presence::There(Some(volume)) if volume.run_by_an_add_on_driver()));
        assert!(engine.folders_of(&[docs_on_stick.as_path(), photos_on_stick.as_path()]).iter().all(Option::is_some));

        // The other job on the stick waits, and isn't looked at; the job elsewhere goes on.
        let refused = engine.prepare_review(&photos).unwrap_err();
        assert!(matches!(&refused, Error::DriveBusy(name) if name == "Documents"), "{refused:?}");
        assert!(refused.to_string().contains("same drive"));
        assert!(matches!(engine.begin_run(&photos, &Crossing::unattended("for a test", true), |_, _| Ok(())), Err(Error::DriveBusy(_))));
        engine.change_runtime(&photos, |rt| rt.last_checked = Some(Instant::now()));
        engine.check(&photos);
        assert!(engine.runtime().get(&photos).is_some_and(|rt| rt.checking.is_none() && rt.last_checked.is_none()), "looked at once the sync is done");
        engine.prepare_review(&notes).unwrap();
        // A sync that got past those checks before (its look began first) can't keep the
        // drive quiet too, so it waits at its claim.
        let job = engine.stored_job(&photos).unwrap();
        let survey = engine.survey(&job, &AtomicBool::new(false), Purpose::Check).unwrap();
        assert!(matches!(engine.claim_folders(&job, &survey), Err(Error::DriveBusy(name)) if name == "Documents"));

        drop(held);
        assert!(!engine.kept_quiet(&docs_on_stick) && !engine.kept_quiet(&photos_on_stick));
        drop(silenced);
        until(|| !engine.is_silent(&docs_on_stick) && !engine.is_silent(&photos_on_stick));
        engine.prepare_review(&photos).unwrap();
    }

    #[test]
    fn a_sync_stops_background_looks_on_the_drive_and_waits_for_them_to_end() {
        let Jobs { _tmp, _stick, engine, documents, photos, .. } = jobs();
        // A background look at "Photos", as if it were reading the stick: its thread ends a
        // moment after it's told to stop.
        let flag = Arc::new(AtomicBool::new(false));
        engine.change_runtime(&photos, |rt| rt.checking = Some(flag.clone()));
        let epoch = engine.runtime()[&photos].epoch;
        let looker = {
            let (engine, flag, photos) = (engine.clone(), flag.clone(), photos.clone());
            thread::spawn(move || {
                until(|| flag.load(Ordering::Relaxed));
                thread::sleep(Duration::from_millis(100));
                engine.change_runtime(&photos, |rt| rt.checking = None);
            })
        };
        let started = Instant::now();
        let held = engine.hold_a_sync(&documents).unwrap();
        assert!(engine.runtime()[&photos].checking.is_none(), "the sync didn't wait for the look to end");
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert!(engine.runtime()[&photos].epoch != epoch, "what the look found counts for nothing");
        looker.join().unwrap();
        drop(held);

        // A list the person asked for isn't stopped: the sync waits until it's made.
        let asked = Arc::new(AtomicBool::new(false));
        engine.change_runtime(&photos, |rt| rt.preparing = Some(asked.clone()));
        assert!(matches!(engine.hold_a_sync(&documents), Err(Error::DriveBusy(name)) if name == "Photos"));
        assert!(!asked.load(Ordering::Relaxed));
        engine.change_runtime(&photos, |rt| rt.preparing = None);
        drop(engine.hold_a_sync(&documents).unwrap());
    }

    #[test]
    fn one_job_at_a_time_works_on_the_drive_from_the_start_of_its_run() {
        let Jobs { _tmp, _stick, engine, documents, photos, notes, .. } = jobs();
        // What the autopilot asks every second tells which drives the folders are on.
        engine.whereabouts_of(&engine.job_list());
        // A background look at Photos reads the stick when Documents' run begins: the run
        // comes first, and the look comes again once it's done.
        let flag = Arc::new(AtomicBool::new(false));
        engine.change_runtime(&photos, |rt| {
            rt.checking = Some(flag.clone());
            rt.last_checked = Some(Instant::now());
        });
        let epoch = engine.runtime()[&photos].epoch;
        let crossing = Crossing::unattended("for a test", true);
        engine.begin_run(&documents, &crossing, |_, _| Ok(())).unwrap();
        assert!(flag.load(Ordering::Relaxed), "the look was stopped");
        assert!(engine.runtime()[&photos].epoch != epoch && engine.runtime()[&photos].last_checked.is_none());
        engine.change_runtime(&photos, |rt| rt.checking = None);

        // Still looking at its folders, Documents' run keeps the others off the stick.
        let refused = engine.begin_run(&photos, &crossing, |_, _| Ok(())).unwrap_err();
        assert!(matches!(&refused, Error::DriveBusy(name) if name == "Documents"), "{refused:?}");
        assert!(matches!(engine.prepare_review(&photos), Err(Error::DriveBusy(_))));
        // No look either, and the person who asks for one is told why.
        let refused = engine.look_now(&photos).unwrap_err();
        assert!(refused.to_string().contains("“Documents” is using the same drive"), "{refused}");
        engine.check(&photos);
        assert!(engine.runtime()[&photos].checking.is_none() && engine.runtime()[&photos].last_checked.is_none(), "it comes once the run is done");
        engine.prepare_review(&notes).unwrap();
        // The caller's checks still come first: a run that can't go ahead says why.
        let refused = engine.begin_run(&photos, &crossing, |_, _| Err(Error::Invalid("not now".into()))).unwrap_err();
        assert_eq!(refused.to_string(), "not now");

        engine.change_runtime(&documents, |rt| {
            Engine::unmark_run(rt);
        });
        engine.look_now(&photos).unwrap();
        let looked = engine.runtime().get(&photos).is_some_and(|rt| rt.checking.is_some() || rt.last_checked.is_some());
        assert!(looked, "looked at once the run is done");
        until(|| engine.runtime()[&photos].checking.is_none());
    }

    #[test]
    fn a_run_nobody_asked_for_waits_for_a_look_on_the_drive_and_one_asked_for_stops_it() {
        let Jobs { _tmp, _stick, engine, documents, photos, .. } = jobs();
        engine.whereabouts_of(&engine.job_list());
        let flag = Arc::new(AtomicBool::new(false));
        engine.change_runtime(&photos, |rt| rt.checking = Some(flag.clone()));
        // Stopping the look for every run a trigger starts could keep Photos from ever being
        // looked at while Documents syncs on change.
        let refused = engine.begin_run(&documents, &Crossing::unattended("after changes", false), |_, _| Ok(())).unwrap_err();
        assert!(matches!(&refused, Error::DriveBusy(name) if name == "Photos"), "{refused:?}");
        assert!(!flag.load(Ordering::Relaxed), "the look goes on");
        // The person asked for this one: it comes first.
        engine.begin_run(&documents, &Crossing::unattended("started by hand", true), |_, _| Ok(())).unwrap();
        assert!(flag.load(Ordering::Relaxed), "the look was stopped");
        engine.change_runtime(&documents, |rt| {
            Engine::unmark_run(rt);
        });
    }

    #[test]
    fn a_look_that_began_before_charon_knew_the_drive_is_stopped_at_the_keep() {
        let Jobs { _tmp, _stick, engine, documents, photos, .. } = jobs();
        // Nothing is known yet of where the folders are, as right after a launch (once the
        // first looks are done: the rows they publish as they end say where the folders are).
        until(|| engine.runtime().values().all(|rt| rt.checking.is_none()));
        until(|| {
            engine.inner.quiet.known().presences.clear();
            thread::sleep(Duration::from_millis(50));
            engine.inner.quiet.known().presences.is_empty()
        });
        let flag = Arc::new(AtomicBool::new(false));
        engine.change_runtime(&photos, |rt| {
            rt.checking = Some(flag.clone());
            rt.last_checked = Some(Instant::now());
        });
        let epoch = engine.runtime()[&photos].epoch;
        let looker = {
            let (engine, flag, photos) = (engine.clone(), flag.clone(), photos.clone());
            thread::spawn(move || {
                until(|| flag.load(Ordering::Relaxed));
                thread::sleep(Duration::from_millis(100));
                engine.change_runtime(&photos, |rt| rt.checking = None);
            })
        };
        let crossing = Crossing::unattended("for a test", true);
        let (job, cancel) = engine.begin_run(&documents, &crossing, |_, _| Ok(())).unwrap();
        assert!(!flag.load(Ordering::Relaxed), "the run couldn't tell the look was on the same drive");
        let survey = engine.survey(&job, &cancel, Purpose::Automatic).unwrap();
        let claim = engine.claim_folders(&job, &survey).unwrap();
        let (now, last_checked, checking) = engine.runtime().get(&photos).map(|rt| (rt.epoch, rt.last_checked, rt.checking.is_some())).unwrap();
        assert!(flag.load(Ordering::Relaxed) && now != epoch && last_checked.is_none(), "the keep stopped it");
        assert!(!checking, "and the sync waited for it to end");
        looker.join().unwrap();
        drop(claim);
        engine.change_runtime(&documents, |rt| {
            Engine::unmark_run(rt);
        });
    }

    #[test]
    fn a_list_confirmed_while_the_drive_is_busy_is_checked_then_waits_for_it() {
        let Jobs { _tmp, _stick, engine, documents, photos, .. } = jobs();
        engine.whereabouts_of(&engine.job_list());
        let list = engine.prepare_review(&photos).unwrap();
        let held = engine.hold_a_sync(&documents).unwrap();
        assert!(engine.runtime()[&photos].review.as_ref().unwrap().unheard.load(Ordering::Relaxed), "its folder on the stick isn't watched");
        let stale = |stale: bool| engine.change_runtime(&photos, |rt| rt.review.as_ref().unwrap().stale.store(stale, Ordering::Relaxed));
        stale(true);
        let refused = engine.approve(&photos, &list.review_id, true).unwrap_err();
        assert!(refused.to_string().contains("Things changed"), "a list that can't go ahead is refused, not kept waiting: {refused}");
        stale(false);
        let approved = engine.approve(&photos, &list.review_id, true).unwrap();
        assert!(matches!(&approved, crate::engine::Approval::Waiting { waiting_for } if waiting_for == "Documents"), "{approved:?}");
        // It waits as the drive is busy, and starts once it's free.
        assert!(!engine.start_confirmed(&photos));
        drop(held);
        assert!(engine.start_confirmed(&photos));
        until(|| !engine.is_running(&photos));
    }

    #[test]
    fn a_list_made_before_another_sync_kept_the_drive_quiet_can_be_confirmed_and_is_looked_at_again() {
        let Jobs { _tmp, _stick, engine, documents, photos, .. } = jobs();
        let list = engine.prepare_review(&photos).unwrap();
        drop(engine.hold_a_sync(&documents).unwrap());
        // Photos' folder on the stick wasn't watched meanwhile. Nothing was heard, so the list
        // can be confirmed (each step checks its items right before it acts), but what changed
        // there isn't in it: once it's carried out, the folders are looked at again at once.
        assert!(!engine.list_is_stale(&photos, &list.review_id));
        let approved = engine.approve(&photos, &list.review_id, true).unwrap();
        assert!(matches!(approved, crate::engine::Approval::Started), "{approved:?}");
        until(|| !engine.is_running(&photos));
        let rt = &engine.runtime()[&photos];
        assert!(rt.look.is_none() && rt.last_checked.is_none(), "looked at again at once");
    }

    #[test]
    fn after_a_sync_that_kept_the_drive_quiet_the_regular_look_counts_from_its_keep() {
        let Jobs { _tmp, _stick, engine, documents, .. } = jobs();
        let crossing = Crossing::unattended("for a test", true);
        let (job, cancel) = engine.begin_run(&documents, &crossing, |_, _| Ok(())).unwrap();
        let survey = engine.survey(&job, &cancel, Purpose::Automatic).unwrap();
        let claim = engine.claim_folders(&job, &survey).unwrap();
        thread::sleep(Duration::from_millis(100));
        engine.cross(&job, &survey, &cancel, &crossing, claim);
        // Nothing heard counts against what it did: its row says what's left, as after any
        // sync, and it isn't looked at again at once (with a file that fails every time, a
        // job that syncs on change would sync again and again). But what changed on the stick
        // meanwhile wasn't heard: the regular look counts from when it stopped being watched.
        let rt = &engine.runtime()[&documents];
        assert!(rt.look.is_some(), "its look still says what's left");
        let (checked, finished) = (rt.last_checked.unwrap(), rt.finished_at.unwrap());
        assert!(finished.duration_since(checked) >= Duration::from_millis(100), "{checked:?} {finished:?}");
    }

    #[test]
    fn after_a_sync_that_left_something_the_regular_look_counts_from_its_end() {
        let Jobs { _tmp, _stick, engine, documents, on_stick: [docs_on_stick, _], .. } = jobs();
        let crossing = Crossing::unattended("for a test", true);
        let (job, cancel) = engine.begin_run(&documents, &crossing, |_, _| Ok(())).unwrap();
        let survey = engine.survey(&job, &cancel, Purpose::Automatic).unwrap();
        // Since the look, a folder took the place the letter was to be copied to: the sync
        // leaves the letter where it is.
        fs::create_dir(docs_on_stick.join("letter.txt")).unwrap();
        let claim = engine.claim_folders(&job, &survey).unwrap();
        thread::sleep(Duration::from_millis(100));
        engine.cross(&job, &survey, &cancel, &crossing, claim);
        let run = engine.stored_job(&documents).unwrap().last_run.unwrap();
        assert!(run.skipped + run.failed > 0, "{run:?}");
        // However long it kept the stick quiet, what it left is tried again a whole interval
        // after it ended at the soonest: a file that fails every time mustn't sync again and
        // again on the very drive the keep is meant to spare.
        let (checked, finished) = engine.runtime().get(&documents).map(|rt| (rt.last_checked.unwrap(), rt.finished_at.unwrap())).unwrap();
        assert!(checked >= finished, "{checked:?} {finished:?}");
    }

    #[test]
    fn a_look_that_stopped_moving_on_no_longer_holds_the_drive() {
        let Jobs { _tmp, _stick, engine, documents, photos, .. } = jobs();
        engine.whereabouts_of(&engine.job_list());
        // A look at Photos reads the stick, however long it takes: it holds the others back.
        let looking = engine.begin_a_look(&photos);
        let after_changes = Crossing::unattended("after changes", false);
        let refused = engine.begin_run(&documents, &after_changes, |_, _| Ok(())).unwrap_err();
        assert!(matches!(&refused, Error::DriveBusy(name) if name == "Photos"), "{refused:?}");
        engine.check(&documents);
        assert!(engine.look_held_back(&documents), "a look at work holds the drive");
        let refused = engine.look_now(&documents).unwrap_err();
        assert!(refused.to_string().contains("“Photos” is using the same drive"), "{refused}");
        engine.forget_drive_waits();

        // The look stops moving on (it waits for its folder at home, a network share whose
        // server went away). Another job's look goes ahead, and stops it first: it would read
        // the stick again alongside, once that folder answers.
        engine.a_look_stalls(&photos);
        engine.check(&documents);
        assert!(!engine.look_held_back(&documents), "Documents was looked at");
        assert!(looking.load(Ordering::Relaxed), "the look was stopped");
        until(|| engine.runtime().get(&documents).is_some_and(|rt| rt.checking.is_none()));
        // A run nobody asked for goes ahead of it too.
        engine.begin_run(&documents, &after_changes, |_, _| Ok(())).unwrap();
        engine.change_runtime(&documents, |rt| {
            Engine::unmark_run(rt);
        });
        engine.change_runtime(&photos, |rt| rt.checking = None);
    }

    #[test]
    fn a_look_notes_that_it_moves_on() {
        let Jobs { _tmp, _stick, engine, documents, on_stick: [docs_on_stick, _], .. } = jobs();
        // An old folder in the trash on the stick, which the look goes through.
        let old = docs_on_stick.join(crate::scan::TRASH_FOLDER).join("2000-01-01_00-00-00");
        fs::create_dir_all(&old).unwrap();
        for n in 0..3 {
            fs::write(old.join(format!("{n}.txt")), "x").unwrap();
        }
        let job = engine.stored_job(&documents).unwrap();
        engine.change_runtime(&documents, |rt| rt.look_moved = None);
        let before = engine.inner.quiet.moves.load(Ordering::Relaxed);
        engine.survey(&job, &AtomicBool::new(false), Purpose::Check).unwrap();
        assert!(engine.runtime()[&documents].look_moved.is_some());
        // At the end of each side's scan, then for the folder in the trash, and each item in it.
        let moves = engine.inner.quiet.moves.load(Ordering::Relaxed) - before;
        assert!(moves >= 2 + 1 + 3, "{moves}");
    }

    #[test]
    fn a_two_way_look_notes_that_it_moves_on_as_it_compares() {
        let Jobs { _tmp, _stick, engine, .. } = jobs();
        // "Books", two-way between home and the stick, has a file on both sides of the same
        // size but other times: the look reads both in full.
        let (home, stick) = (_tmp.path().join("home").join("books"), _tmp.path().join("stick").join("books"));
        let big = vec![1u8; 3 << 20];
        for (folder, seconds) in [(&home, 1_700_000_000), (&stick, 1_700_000_100)] {
            fs::create_dir_all(folder).unwrap();
            fs::write(folder.join("big.bin"), &big).unwrap();
            filetime::set_file_mtime(folder.join("big.bin"), filetime::FileTime::from_unix_time(seconds, 0)).unwrap();
        }
        let setup = JobSetup {
            name: "Books".into(),
            source: home,
            destination: stick,
            mode: SyncMode::TwoWay,
            conflict: ConflictRule::Ask,
            trigger: Trigger::Manual,
            excludes: vec![],
            safe_delete: true,
            full_automation: false,
        };
        let books = engine.add_job(setup).unwrap().job.id;
        until(|| engine.runtime().values().all(|rt| rt.checking.is_none()));
        let job = engine.stored_job(&books).unwrap();
        let before = engine.inner.quiet.moves.load(Ordering::Relaxed);
        engine.survey(&job, &AtomicBool::new(false), Purpose::Check).unwrap();
        // At the end of each side's scan, then for the pair, and for each piece of it.
        let moves = engine.inner.quiet.moves.load(Ordering::Relaxed) - before;
        assert!(moves >= 2 + 1 + 3, "{moves}");
    }

    /// "Books", two-way from `source` to `destination` (one at home, one on the stick), with
    /// two pairs whose contents must be read (same size, other times), and the markers
    /// both folders carry after a sync: what's read is kept only in folders that carry them.
    fn books(engine: &Engine, source: &Path, destination: &Path) -> Job {
        for (folder, text, seconds) in [(source, "one", 1_700_000_000), (destination, "two", 1_700_000_100)] {
            fs::create_dir_all(folder).unwrap();
            for name in ["a.txt", "b.txt"] {
                fs::write(folder.join(name), text).unwrap();
                filetime::set_file_mtime(folder.join(name), filetime::FileTime::from_unix_time(seconds, 0)).unwrap();
            }
        }
        let setup = JobSetup {
            name: "Books".into(),
            source: source.to_path_buf(),
            destination: destination.to_path_buf(),
            mode: SyncMode::TwoWay,
            conflict: ConflictRule::Ask,
            trigger: Trigger::Manual,
            excludes: vec![],
            safe_delete: true,
            full_automation: false,
        };
        let books = engine.add_job(setup).unwrap().job.id;
        until(|| engine.runtime().values().all(|rt| rt.checking.is_none()));
        engine.forget_drive_waits();
        for (folder, side) in [(source, Side::Source), (destination, Side::Destination)] {
            crate::run::Marker::new(&books, side, "token", 1).write(folder).unwrap();
        }
        engine.stored_job(&books).unwrap()
    }

    /// Stops the look of a job as soon as it read a pair it hadn't read before (it notes that
    /// it moved on at the next one: the job's reads kept are more than at its first move), and
    /// does `then` too.
    fn stop_after_the_first_pair(engine: &Engine, stopped: &Arc<AtomicBool>, then: impl Fn() + Send + Sync + 'static) {
        let stop = stopped.clone();
        let before = Mutex::new(HashMap::<JobId, usize>::new());
        *engine.inner.quiet.when_moved.lock().unwrap() = Some(Box::new(move |engine, id| {
            let kept = engine.change_known_runtime(id, |rt| rt.reads.len()).unwrap_or(0);
            let before = *before.lock().unwrap().entry(id.clone()).or_insert(kept);
            if kept > before && !stop.swap(true, Ordering::Relaxed) {
                then();
            }
        }));
    }

    /// A comparison stopped on its last pair, which it then couldn't read, ends as one
    /// stopped on any other pair: what it read is kept while both folders still carry their
    /// markers, but while another job's sync keeps the drive quiet, it reads no marker there
    /// and forgets what it read instead.
    #[test]
    fn a_comparison_stopped_on_its_last_pair_reads_no_marker_on_a_drive_kept_quiet() {
        let Jobs { _tmp, _stick, engine, documents, on_stick: [docs_on_stick, _], .. } = jobs();
        let (home, stick) = (_tmp.path().join("home").join("books"), _tmp.path().join("stick").join("books"));
        let job = books(&engine, &home, &stick);
        // Stopped, the second pair's file at home goes away: its read fails, with no step
        // after it that heeds the stop.
        let gone = home.join("b.txt");
        let stopped = Arc::new(AtomicBool::new(false));
        let removed = gone.clone();
        stop_after_the_first_pair(&engine, &stopped, move || fs::remove_file(&removed).unwrap());
        let kept = || engine.runtime()[&job.id].reads.len();
        assert!(matches!(engine.survey(&job, &stopped, Purpose::Check), Err(Error::Cancelled)));
        assert!(stopped.load(Ordering::Relaxed) && !gone.exists());
        assert_eq!(kept(), 1, "both folders still carry their markers");

        let held = engine.hold_a_sync(&documents).unwrap();
        assert!(engine.kept_quiet(&docs_on_stick) && engine.kept_quiet(&stick));
        engine.change_runtime(&job.id, |rt| rt.reads = Default::default());
        fs::write(&gone, "one").unwrap();
        filetime::set_file_mtime(&gone, filetime::FileTime::from_unix_time(1_700_000_000, 0)).unwrap();
        stopped.store(false, Ordering::Relaxed);
        assert!(matches!(engine.survey(&job, &stopped, Purpose::Check), Err(Error::Cancelled)));
        assert!(stopped.load(Ordering::Relaxed) && !gone.exists());
        assert_eq!(kept(), 0, "the markers aren't read on the stick while the sync keeps it quiet");
        drop(held);
    }

    /// A look its own job's run stopped (see `begin_run`) may still be comparing once that
    /// run's sync keeps the stick quiet: it reads no marker there either, and forgets what it
    /// read instead. What the sync's own look read and checked stays, for the sync to carry.
    #[test]
    fn a_comparison_stopped_by_its_own_sync_reads_no_marker_while_that_sync_keeps_the_drive_quiet() {
        let Jobs { _tmp, _stick, engine, .. } = jobs();
        let (home, stick) = (_tmp.path().join("home").join("books"), _tmp.path().join("stick").join("books"));
        let job = books(&engine, &home, &stick);
        let held = engine.hold_a_sync(&job.id).unwrap();
        assert!(engine.kept_quiet(&stick));
        let kept = |key: &str| engine.runtime()[&job.id].reads.has(key);
        assert!(kept("a.txt") && kept("b.txt"), "the sync's own look read both pairs");
        // A pair that came since, which the stopped look reads first.
        for (folder, text, seconds) in [(&home, "one", 1_700_000_000), (&stick, "two", 1_700_000_100)] {
            fs::write(folder.join("0.txt"), text).unwrap();
            filetime::set_file_mtime(folder.join("0.txt"), filetime::FileTime::from_unix_time(seconds, 0)).unwrap();
        }
        let stopped = Arc::new(AtomicBool::new(false));
        stop_after_the_first_pair(&engine, &stopped, || {});
        assert!(matches!(engine.survey(&job, &stopped, Purpose::Check), Err(Error::Cancelled)));
        assert!(stopped.load(Ordering::Relaxed));
        assert!(!kept("0.txt"), "the markers aren't read on the stick while its own sync keeps it quiet");
        assert!(kept("a.txt") && kept("b.txt"), "what the sync's own look read stays");
        drop(held);
    }

    /// A comparison stopped while it holds a file open on the stick, kept quiet by another
    /// job's sync, doesn't close it until that sync is done: closing it is work for the drive.
    #[test]
    fn a_file_a_stopped_comparison_holds_open_on_a_drive_kept_quiet_is_closed_once_the_sync_is_done() {
        let Jobs { _tmp, _stick, engine, documents, .. } = jobs();
        let (stick, home) = (_tmp.path().join("stick").join("books"), _tmp.path().join("home").join("books"));
        let job = books(&engine, &stick, &home);
        let held = engine.hold_a_sync(&documents).unwrap();
        // The second pair's file on the stick is open when the look sees it was stopped.
        let stopped = Arc::new(AtomicBool::new(false));
        stop_after_the_first_pair(&engine, &stopped, || {});
        assert!(matches!(engine.survey(&job, &stopped, Purpose::Check), Err(Error::Cancelled)));
        let left_open = || engine.inner.quiet.known().left_open.iter().map(|(path, _)| path.clone()).collect::<Vec<_>>();
        assert_eq!(left_open(), vec![stick.join("b.txt")]);
        drop(held);
        assert!(left_open().is_empty(), "closed once the sync is done");
        // Where no sync keeps the drive quiet, it's closed at once.
        let stopped = Arc::new(AtomicBool::new(false));
        engine.change_runtime(&job.id, |rt| rt.reads = Default::default());
        stop_after_the_first_pair(&engine, &stopped, || {});
        assert!(matches!(engine.survey(&job, &stopped, Purpose::Check), Err(Error::Cancelled)));
        assert!(stopped.load(Ordering::Relaxed) && left_open().is_empty());
    }

    /// "Pictures", a Mirror from home to the stick, synced once, and "Inbox", two-way at home
    /// (with full automation when `automatic`), into which the person copied the Mirror's
    /// folder on the stick, hidden .charon folder and all, as "Old pictures". Returns the
    /// Mirror's id and its folder on the stick, Inbox's id, and the folder with the copy.
    /// Nothing is known yet of where the copy came from (as when the stick was away so far).
    fn a_copied_marker(engine: &Engine, tmp: &Path, automatic: bool) -> (JobId, PathBuf, JobId, PathBuf) {
        let (pictures, on_stick) = (tmp.join("home").join("pictures"), tmp.join("stick").join("pictures"));
        let (inbox, elsewhere) = (tmp.join("home").join("inbox"), tmp.join("home").join("inbox-copy"));
        for folder in [&pictures, &on_stick, &inbox, &elsewhere] {
            fs::create_dir_all(folder).unwrap();
        }
        let add = |name: &str, mode: SyncMode, source: &PathBuf, destination: &PathBuf, full_automation: bool| {
            let setup = JobSetup {
                name: name.into(),
                source: source.clone(),
                destination: destination.clone(),
                mode,
                conflict: ConflictRule::Ask,
                trigger: Trigger::Manual,
                excludes: vec![],
                safe_delete: true,
                full_automation,
            };
            engine.add_job(setup).unwrap().job.id
        };
        // "Pictures" synced once: its marker in its folder on the stick, as it remembers it.
        let mirror = add("Pictures", SyncMode::Mirror, &pictures, &on_stick, false);
        engine.change_job(&mirror, |job| job.destination_token = Some("token".into())).unwrap();
        crate::run::Marker::new(&mirror, Side::Destination, "token", 0).write(&on_stick).unwrap();
        let old = inbox.join("Old pictures");
        fs::create_dir_all(old.join(crate::scan::MARKER_FOLDER)).unwrap();
        let marker = format!("{mirror}.json");
        fs::copy(on_stick.join(crate::scan::MARKER_FOLDER).join(&marker), old.join(crate::scan::MARKER_FOLDER).join(&marker)).unwrap();
        let inbox = add("Inbox", SyncMode::TwoWay, &inbox, &elsewhere, automatic);
        until(|| engine.runtime().values().all(|rt| rt.checking.is_none()));
        engine.forget_drive_waits();
        engine.inner.quiet.known().origins.clear();
        (mirror, on_stick, inbox, old)
    }

    /// A copy of the hidden .charon folder of a Mirror's folder on the stick, inside a folder
    /// another job writes to, is only mentioned. While a sync keeps the stick quiet, the stick
    /// isn't asked whether it's a copy: the last time it told them apart holds. A copy never
    /// told apart can't be told then, and counts as maybe one place until that sync is done,
    /// the careful direction.
    #[test]
    fn a_copy_of_a_marker_is_known_for_one_while_its_drive_is_kept_quiet() {
        let Jobs { _tmp, _stick, engine, documents, .. } = jobs();
        let (_, on_stick, inbox, old) = a_copied_marker(&engine, _tmp.path(), false);
        let job = engine.stored_job(&inbox).unwrap();
        let look = || engine.survey(&job, &AtomicBool::new(false), Purpose::Review).unwrap();
        let mentioned = |survey: &Survey| survey.warnings.iter().any(|warning| warning.text.contains("holds a copy of the hidden .charon folder"));
        let first = look();
        assert!(mentioned(&first) && first.holds.is_empty() && first.waits_on.is_none(), "{:?}", first.holds);

        let held = engine.hold_a_sync(&documents).unwrap();
        assert!(engine.kept_quiet(&on_stick));
        let during = look();
        assert!(mentioned(&during) && during.holds.is_empty() && during.waits_on.is_none(), "{:?}", during.holds);
        // Never told apart before, it can't be told while the stick is kept quiet.
        engine.inner.quiet.known().origins.clear();
        let unknown = look();
        assert!(unknown.holds.iter().any(|hold| hold.contains("can't tell yet") && hold.contains("“Documents” is syncing on that drive")), "{:?}", unknown.holds);
        let waits = unknown.waits_on.clone().expect("it waits for that sync");
        assert_eq!((&waits.folder, &waits.found, waits.whose.as_str(), waits.syncing.as_str()), (&on_stick, &old, "Pictures", "Documents"));
        // With another reason to hold back, it doesn't wait for that sync: the clash is among
        // the reasons, after the others.
        let synced_before = Job { last_synced: Some(chrono::Utc::now()), ..job.clone() };
        let held_back = engine.survey(&synced_before, &AtomicBool::new(false), Purpose::Review).unwrap();
        assert!(held_back.waits_on.is_none(), "{:?}", held_back.holds);
        assert!(held_back.holds.len() == 2 && held_back.holds[0].contains("no memory of this job's last sync") && held_back.holds[1].contains("can't tell yet"), "{:?}", held_back.holds);
        drop(held);
        let after = look();
        assert!(mentioned(&after) && after.holds.is_empty() && after.waits_on.is_none(), "{:?}", after.holds);
    }

    /// A run nobody looks at that can't tell whether a folder holds a copy of a marker, only
    /// because another job's sync keeps the stick quiet, isn't held back for the person's OK:
    /// it waits for that sync, without looking again meanwhile, and goes ahead once it's done.
    #[test]
    fn a_run_nobody_looks_at_waits_for_the_sync_that_keeps_it_from_telling_a_copy() {
        let Jobs { _tmp, _stick, engine, documents, .. } = jobs();
        let (_, on_stick, inbox, _) = a_copied_marker(&engine, _tmp.path(), true);
        // Its first list was confirmed: it runs by itself.
        engine.change_job(&inbox, |job| job.needs_review = false).unwrap();
        let held = engine.hold_a_sync(&documents).unwrap();
        let wanted = crate::engine::runtime::Wanted { reason: "automatically after changes".into(), by_hand: false };
        engine.change_runtime(&inbox, |rt| rt.wanted = Some(wanted.clone()));
        engine.start_wanted(&inbox);
        until(|| engine.runtime()[&inbox].waiting_for.is_some() && !engine.runtime()[&inbox].is_running());
        let (waiting_for, waits_on, wanted_still, must_ask) = {
            let runtime = engine.runtime();
            let rt = &runtime[&inbox];
            (rt.waiting_for.clone(), rt.waits_on.clone(), rt.wanted.is_some(), rt.must_ask)
        };
        assert_eq!((waiting_for.as_deref(), waits_on.as_ref()), (Some("Documents"), Some(&on_stick)));
        assert!(wanted_still && !must_ask && !engine.stored_job(&inbox).unwrap().needs_review, "not held back");
        // Its log says why (written once the run ended).
        let said = |what: &str| engine.log(&inbox, 20).iter().any(|line| line.text.contains(what));
        until(|| said("found the hidden .charon folder of “Pictures”"));
        assert!(said("“Documents” is syncing on that drive"), "{:?}", engine.log(&inbox, 5));
        // Until that sync is done, it doesn't begin, nor look.
        let again = engine.start_unreviewed(&inbox, crate::engine::runs::Unreviewed::Wanted(&wanted));
        assert!(matches!(&again, Err(Error::DriveBusy(name)) if name == "Documents"), "{again:?}");
        assert!(!engine.runtime()[&inbox].is_running());

        drop(held);
        engine.start_wanted(&inbox);
        until(|| engine.runtime()[&inbox].wanted.is_none() && !engine.runtime()[&inbox].is_running());
        let job = engine.stored_job(&inbox).unwrap();
        assert!(job.last_synced.is_some() && !job.needs_review, "{:?}", job.held);
        assert!(engine.runtime()[&inbox].waits_on.is_none());
    }

    /// A wanted run that waits for a sync before it can tell a copy of a marker is still
    /// refused, as ever, for what else keeps it from starting: a list the person confirmed
    /// meanwhile, which waits for another sync, stands in for it, and the row keeps saying
    /// what that list waits for; and once the job is paused, it's called off.
    #[test]
    fn a_waiting_run_gives_way_to_a_list_confirmed_meanwhile() {
        let Jobs { _tmp, _stick, engine, documents, .. } = jobs();
        let (_, on_stick, inbox, _) = a_copied_marker(&engine, _tmp.path(), true);
        engine.change_job(&inbox, |job| job.needs_review = false).unwrap();
        let held = engine.hold_a_sync(&documents).unwrap();
        let wanted = crate::engine::runtime::Wanted { reason: "automatically after changes".into(), by_hand: false };
        engine.change_runtime(&inbox, |rt| rt.wanted = Some(wanted.clone()));
        engine.start_wanted(&inbox);
        until(|| engine.runtime()[&inbox].waits_on.is_some() && !engine.runtime()[&inbox].is_running());
        // Confirmed while another Charon was at work in the job's folders.
        let confirmed = crate::engine::runtime::Confirmed { review_id: "a list".into(), acknowledged: true, pauses: engine.pauses() };
        engine.change_runtime(&inbox, |rt| {
            rt.confirmed = Some(confirmed);
            rt.waiting_for = Some("another Charon".into());
        });
        engine.start_wanted(&inbox);
        let (wanted_still, waits_on, waiting_for) = {
            let runtime = engine.runtime();
            let rt = &runtime[&inbox];
            (rt.wanted.is_some(), rt.waits_on.clone(), rt.waiting_for.clone())
        };
        assert!(!wanted_still && waits_on.is_none(), "the confirmed list stands in for it");
        assert_eq!(waiting_for.as_deref(), Some("another Charon"));
        // A run a trigger started doesn't start while the job is paused.
        engine.change_job(&inbox, |job| job.paused = true).unwrap();
        engine.change_runtime(&inbox, |rt| {
            rt.confirmed = None;
            rt.waiting_for = Some("Documents".into());
            rt.wanted = Some(wanted.clone());
            rt.waits_on = Some(on_stick.clone());
        });
        engine.start_wanted(&inbox);
        assert!(engine.runtime()[&inbox].wanted.is_none(), "called off");
        drop(held);
    }

    /// The question whether a folder holds a copy of a marker, which may go on after the look
    /// that asked it, stops before its next step once a sync keeps the drive of either folder
    /// quiet: it then can't tell, and leaves nothing on the stick.
    #[test]
    fn the_question_whether_a_marker_is_a_copy_stops_once_its_drive_is_kept_quiet() {
        let Jobs { _tmp, _stick, engine, documents, .. } = jobs();
        let (mirror, on_stick, _, old) = a_copied_marker(&engine, _tmp.path(), false);
        let listing = || fs::read_dir(on_stick.join(crate::scan::MARKER_FOLDER)).unwrap().count();
        assert_eq!(engine.origin_unless_kept_quiet(&mirror, &on_stick, &old), crate::run::Origin::Copy);
        let before = listing();
        let held = engine.hold_a_sync(&documents).unwrap();
        assert_eq!(engine.origin_unless_kept_quiet(&mirror, &on_stick, &old), crate::run::Origin::Unknown);
        // Nor when it's the folder with the copy that's on the stick.
        assert_eq!(engine.origin_unless_kept_quiet(&mirror, &old, &on_stick), crate::run::Origin::Unknown);
        // It doesn't even look whether the job's folder is there.
        let gone = _tmp.path().join("stick").join("gone");
        assert_eq!(engine.origin_unless_kept_quiet(&mirror, &gone, &old), crate::run::Origin::Unknown);
        assert_eq!(listing(), before, "no probe on the stick");
        drop(held);
    }

    /// While a sync keeps the stick quiet, nothing is asked about a folder there (whether it
    /// holds a copy of another job's marker, see `overlaps.rs`, which then says what the disk
    /// told before): like a folder that doesn't answer, it can't tell.
    #[test]
    fn nothing_is_asked_about_a_folder_on_a_drive_kept_quiet() {
        let Jobs { _tmp, _stick, engine, documents, on_stick: [_, photos_on_stick], .. } = jobs();
        let asked = Arc::new(AtomicU64::new(0));
        let ask = |folder: &Path| {
            let asked = asked.clone();
            engine.ask_about(folder, move |_| asked.fetch_add(1, Ordering::Relaxed))
        };
        let held = engine.hold_a_sync(&documents).unwrap();
        assert!(ask(&photos_on_stick).is_none());
        assert_eq!(asked.load(Ordering::Relaxed), 0);
        assert!(ask(&_tmp.path().join("home").join("photos")).is_some(), "a folder elsewhere is asked about");
        drop(held);
        assert!(ask(&photos_on_stick).is_some());
        assert_eq!(asked.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_sync_doesnt_wait_for_a_look_that_stopped_moving_on() {
        let Jobs { _tmp, _stick, engine, documents, photos, .. } = jobs();
        engine.whereabouts_of(&engine.job_list());
        let looking = engine.begin_a_look(&photos);
        engine.a_look_stalls(&photos);
        // The sync stops the look, and doesn't wait for it to end: it waits for its folder at
        // home, and never reads the stick again.
        let began = Instant::now();
        let held = engine.hold_a_sync(&documents).unwrap();
        assert!(began.elapsed() < LOOKS_END_WITHIN, "{:?}", began.elapsed());
        assert!(looking.load(Ordering::Relaxed), "the look was stopped");
        drop(held);
        engine.change_runtime(&photos, |rt| rt.checking = None);
    }

    #[test]
    fn a_list_the_person_asks_for_ends_the_jobs_wait_for_its_turn() {
        let Jobs { _tmp, _stick, engine, documents, photos, .. } = jobs();
        engine.whereabouts_of(&engine.job_list());
        // Photos' look waits for Documents' look.
        engine.begin_a_look(&documents);
        engine.check(&photos);
        assert!(engine.look_held_back(&photos));
        // The person asks for Photos' list: it stops Documents' look, and Photos' turn came.
        engine.prepare_review(&photos).unwrap();
        // Documents' look comes again at once: Photos, its list made, waits for nothing.
        engine.change_runtime(&documents, |rt| rt.checking = None);
        engine.check(&documents);
        assert!(!engine.look_held_back(&documents), "Documents was looked at");
        until(|| engine.runtime().get(&documents).is_some_and(|rt| rt.checking.is_none()));
    }

    #[test]
    fn jobs_take_turns_on_the_drive_in_the_order_they_began_to_wait() {
        let Jobs { _tmp, _stick, engine, documents, photos, on_stick: [docs_on_stick, _], .. } = jobs();
        // A third job onto the stick, "Music", syncs by itself as Documents does.
        let music_at_home = _tmp.path().join("home").join("music");
        fs::create_dir_all(&music_at_home).unwrap();
        let mut setup = engine.stored_job(&documents).unwrap().setup;
        (setup.name, setup.source, setup.destination) = ("Music".into(), music_at_home, docs_on_stick.parent().unwrap().join("music"));
        fs::create_dir_all(&setup.destination).unwrap();
        let music = engine.add_job(setup).unwrap().job.id;
        until(|| engine.runtime().values().all(|rt| rt.checking.is_none()));
        engine.forget_drive_waits();
        engine.whereabouts_of(&engine.job_list());
        let after_changes = Crossing::unattended("after changes", false);
        let run = |id: &JobId| engine.begin_run(id, &after_changes, |_, _| Ok(()));
        let unmark = |id: &JobId| {
            engine.change_runtime(id, |rt| {
                Engine::unmark_run(rt);
            })
        };

        // Documents syncs. Photos' look waits for it first, then Music's run.
        run(&documents).unwrap();
        engine.check(&photos);
        assert!(engine.look_held_back(&photos));
        assert!(matches!(run(&music), Err(Error::DriveBusy(name)) if name == "Documents"));
        // Photos asks again meanwhile: its turn still counts from the first time.
        engine.check(&photos);
        assert!(engine.look_held_back(&photos));
        // Documents is done. Music asks first, but Photos began to wait first: its look goes
        // ahead, and Music waits for it, whatever the order of the jobs.
        unmark(&documents);
        assert!(matches!(run(&music), Err(Error::DriveBusy(name)) if name == "Photos"));
        engine.check(&photos);
        assert!(!engine.look_held_back(&photos), "Photos was looked at");
        until(|| engine.runtime().get(&photos).is_some_and(|rt| rt.checking.is_none()));
        // Then Music. Documents, wanting to sync again meanwhile, comes after it.
        assert!(matches!(run(&documents), Err(Error::DriveBusy(name)) if name == "Music"));
        run(&music).unwrap();
        assert!(matches!(run(&documents), Err(Error::DriveBusy(name)) if name == "Music"));
        // What holds Photos back is named for its row: Music, at work, though Documents
        // (listed first) began to wait before it.
        assert!(matches!(run(&photos), Err(Error::DriveBusy(name)) if name == "Music"));
        engine.forget_drive_wait(&photos);
        unmark(&music);
        run(&documents).unwrap();
        unmark(&documents);

        // A run the person asked for doesn't wait its turn: only another job at work holds it.
        run(&documents).unwrap();
        engine.check(&photos);
        assert!(engine.look_held_back(&photos));
        unmark(&documents);
        assert!(matches!(run(&music), Err(Error::DriveBusy(name)) if name == "Photos"));
        engine.begin_run(&music, &Crossing::unattended("started by hand", true), |_, _| Ok(())).unwrap();
        unmark(&music);
        // A job Charon stopped asking for holds nobody up once its wait has lapsed: the
        // autopilot didn't ask for it in its last two ticks...
        assert!(matches!(run(&documents), Err(Error::DriveBusy(name)) if name == "Photos"));
        engine.note_a_tick();
        assert!(matches!(run(&documents), Err(Error::DriveBusy(name)) if name == "Photos"));
        engine.note_a_tick();
        run(&documents).unwrap();
        engine.check(&photos);
        assert!(engine.look_held_back(&photos));
        unmark(&documents);
        // ...or as soon as Charon can't ask for it anymore (a folder of it went away).
        assert!(matches!(run(&music), Err(Error::DriveBusy(name)) if name == "Photos"));
        engine.forget_drive_wait(&photos);
        run(&music).unwrap();
        unmark(&music);
        // A look the person asks for doesn't wait its turn either.
        run(&documents).unwrap();
        engine.check(&photos);
        assert!(engine.look_held_back(&photos));
        unmark(&documents);
        engine.look_now(&music).unwrap();
        until(|| engine.runtime().get(&music).is_some_and(|rt| rt.checking.is_none()));
    }

    /// The busy notes in Charon's folder in `folder`.
    fn busy_notes(folder: &Path) -> usize {
        fs::read_dir(folder.join(".charon"))
            .map(|items| items.flatten().filter(|item| item.file_name().to_string_lossy().ends_with(crate::run::BUSY_EXTENSION)).count())
            .unwrap_or(0)
    }

    /// Tries the confirmed list of `id` again as the autopilot does, on a thread of its name.
    /// Returns whether its sync started, and how long that took.
    fn start_confirmed_as_the_autopilot(engine: &Engine, id: &JobId) -> (bool, Duration) {
        let (engine, id) = (engine.clone(), id.clone());
        let pilot = thread::Builder::new().name(AUTOPILOT_THREAD.into()).spawn(move || {
            let began = Instant::now();
            (engine.start_confirmed(&id), began.elapsed())
        });
        pilot.unwrap().join().unwrap()
    }

    #[test]
    fn a_list_the_autopilot_starts_claims_its_folders_on_its_sync_thread() {
        let Jobs { _tmp, _stick, engine, documents, photos, on_stick: [_, photos_on_stick], .. } = jobs();
        engine.whereabouts_of(&engine.job_list());
        let list = engine.prepare_review(&photos).unwrap();
        let held = engine.hold_a_sync(&documents).unwrap();
        let approved = engine.approve(&photos, &list.review_id, true).unwrap();
        assert!(matches!(&approved, crate::engine::Approval::Waiting { waiting_for } if waiting_for == "Documents"), "{approved:?}");
        // The autopilot follows the keeps from now on, and has followed all of them so far.
        engine.quiet_followed(engine.quiet_keeps());
        drop(held);

        // The autopilot tries the list again. It can't wait for its own next follow: the sync
        // claims its folders on its own thread, and the autopilot goes on at once.
        let (started, took) = start_confirmed_as_the_autopilot(&engine, &photos);
        assert!(started && took < UNWATCHED_WITHIN, "{took:?}");
        until(|| engine.kept_quiet(&photos_on_stick));
        let kept_at = Instant::now();
        thread::sleep(Duration::from_millis(100));
        let notes = busy_notes(&photos_on_stick);
        if kept_at.elapsed() < UNWATCHED_WITHIN {
            assert_eq!(notes, 0, "no note on the stick before the autopilot stopped watching it");
        }
        // Its next tick does, and the sync goes ahead.
        engine.quiet_followed(engine.quiet_keeps());
        until(|| !engine.is_running(&photos));
        assert!(photos_on_stick.join("letter.txt").exists(), "the list was carried out");
    }

    #[test]
    fn removing_or_saving_a_job_leaves_a_drive_kept_quiet_alone() {
        let Jobs { _tmp, _stick, engine, documents, photos, on_stick: [docs_on_stick, photos_on_stick], .. } = jobs();
        engine.whereabouts_of(&engine.job_list());
        let stick = docs_on_stick.parent().unwrap().to_path_buf();
        crate::run::Marker::new(&photos, Side::Destination, "token", 1).write(&photos_on_stick).unwrap();
        let marked = || matches!(crate::run::Marker::read(&photos_on_stick, &photos), crate::run::Found::Marker(_));
        let held = engine.hold_a_sync(&documents).unwrap();
        let refused = engine.remove_job(&photos).unwrap_err();
        assert!(refused.to_string().contains("Remove “Photos” once it's done"), "{refused}");
        assert!(marked(), "nothing was taken out of its folder");
        // A Mirror job into a folder on the stick that was never asked about: whether it
        // would clash with a job there can't be told without asking the drive.
        let mut mirror = engine.stored_job(&photos).unwrap().setup;
        (mirror.name, mirror.mode, mirror.destination) = ("Mirror".into(), SyncMode::Mirror, docs_on_stick.join("inside"));
        crate::model::asking::taken();
        let refused = engine.add_job(mirror.clone()).unwrap_err();
        assert!(refused.to_string().contains("Save this job once it's done"), "{refused}");
        // Folders found before are compared as they were found: a Mirror into Documents'
        // folder clashes with it, and saving says so at once.
        mirror.destination = docs_on_stick.clone();
        let refused = engine.add_job(mirror).unwrap_err();
        assert!(refused.to_string().contains("overlap with those of “Documents”"), "{refused}");
        let asked = crate::model::asking::taken();
        assert!(!asked.is_empty() && !asked.iter().any(|path| path.starts_with(&stick)), "the stick wasn't asked about: {asked:?}");

        drop(held);
        engine.remove_job(&photos).unwrap();
        assert!(!marked(), "its marker went with it");
    }

    #[test]
    fn a_folder_the_drive_kept_quiet_didnt_answer_for_is_as_it_was_last_found() {
        let (stick, docs) = (Path::new("/Volumes/STICK"), Path::new("/Volumes/STICK/docs"));
        let mut known = Known::default();
        known.folders.insert(docs.to_path_buf(), Folder::of(Path::new("/")));
        let volume = Volume { mount: stick.display().to_string(), ..Volume::of(Path::new("/")) };
        known.drives.push(QuietDrive { keep: 1, job: "a".into(), name: "A".into(), mount: stick.into(), roots: vec![(docs.into(), volume, None)] });
        assert!(matches!(known.folder(docs), Some(Some(_))));
        assert!(matches!(known.folder(Path::new("/Volumes/STICK/photos")), Some(None)), "never asked: maybe one place with anything");
        assert!(known.folder(Path::new("/Users")).is_none(), "not on the drive");
    }

    #[test]
    fn a_sync_waits_for_the_autopilot_to_stop_watching_the_drive() {
        let Jobs { _tmp, _stick, engine, documents, on_stick: [docs_on_stick, _], .. } = jobs();
        // The autopilot follows the keeps, and has followed all of them so far.
        engine.quiet_followed(engine.quiet_keeps());
        let syncing = {
            let engine = engine.clone();
            thread::spawn(move || {
                let started = Instant::now();
                let held = engine.hold_a_sync(&documents).unwrap();
                let waited = started.elapsed();
                drop(held);
                waited
            })
        };
        // Its next tick stops watching the stick, and says so.
        until(|| engine.kept_quiet(&docs_on_stick));
        thread::sleep(Duration::from_millis(200));
        engine.quiet_followed(engine.quiet_keeps());
        let waited = syncing.join().unwrap();
        assert!(waited >= Duration::from_millis(200) && waited < UNWATCHED_WITHIN, "{waited:?}");
    }

    #[test]
    fn a_sync_elsewhere_keeps_nothing_quiet() {
        let Jobs { _tmp, _stick, engine, notes, on_stick, .. } = jobs();
        let held = engine.hold_a_sync(&notes).unwrap();
        assert!(on_stick.iter().all(|folder| !engine.kept_quiet(folder)));
        assert!(engine.inner.quiet.known().drives.is_empty());
        drop(held);
    }

    #[test]
    fn a_mount_that_cant_be_told_keeps_only_the_folder_itself_quiet() {
        let (mount, root) = if cfg!(windows) { (r"E:\STICK", Path::new(r"E:\STICK\docs")) } else { ("/Volumes/STICK", Path::new("/Volumes/STICK/docs")) };
        let volume = |mount: &str| Volume { mount: mount.into(), ..Volume::of(Path::new("/")) };
        assert_eq!(Engine::mount_of(root, &volume(mount)), PathBuf::from(mount));
        for unknown in ["", "/", "C1A2-33F0"] {
            assert_eq!(Engine::mount_of(root, &volume(unknown)), root, "{unknown:?}");
        }
        let drive = QuietDrive { keep: 0, job: "a".into(), name: "A".into(), mount: "/Volumes/STICK".into(), roots: vec![] };
        assert!(drive.holds(Path::new("/Volumes/STICK/photos")) && drive.holds(Path::new("/Volumes/STICK")));
        assert!(!drive.holds(Path::new("/Volumes/STICK 1/photos")), "another drive's name only starts the same");
    }
}
