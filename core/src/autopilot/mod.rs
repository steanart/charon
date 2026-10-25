//! The autopilot: looks at jobs by themselves, and syncs the ones with full automation.
//!
//! * `mod.rs`      – `Autopilot`: a background loop that looks at every job once a second
//! * `schedule.rs` – `Schedule`: when an interval or daily job is due next
//! * `watcher.rs`  – `FolderWatcher`: hears about changes inside the folders
//!
//! A job *runs by itself* only with full automation on and nothing waiting for a look
//! (`Job::runs_by_itself`). Every other job is only *looked at*: Charon scans and plans in
//! the background and, when changes wait, shows them and tells the person. Nothing changes
//! in its folders until the person confirms the list.
//!
//! Every second, first for every job, a sync that waits (a list the person confirmed, or a
//! run of a job that runs by itself) starts if nothing stands in its way anymore. Then, for
//! each job, in this order:
//! 1. **Drive connected:** both folders just became reachable → sync, or look. A folder
//!    that is there but on another drive than at the last sync (the folder an unplugged
//!    drive leaves behind) doesn't count as reachable, nor does one that doesn't answer
//!    within a few seconds (a network share whose server is away): the folders are asked
//!    about on threads of their own, so such a share holds up no other job.
//! 2. **Changes settled:** the folders changed and have been quiet for a moment → sync
//!    (on-change jobs that run by themselves) or look (every other job).
//! 3. **Schedule:** an interval or daily job is due → sync, or look. Either way the next
//!    time is counted from now.
//! 4. **Removals left for later:** a run of a job that runs by itself left removals for
//!    later, because the items went missing only a few minutes ago on a network share or a
//!    cloud drive (see `Engine::take_follow_up`). Once they've been missing long enough →
//!    sync, whatever the job's trigger.
//! 5. **Regular look:** every few minutes, in case the watcher missed something (network
//!    shares, sleep). An on-change job that runs by itself and has changes waiting syncs,
//!    unless its folders are still changing (step 2 then syncs once they have settled), or
//!    it synced less than its settle time ago (what was saved into the sync's own items
//!    meanwhile isn't heard as a change): then it syncs once that time has passed. Folders
//!    that never settle (an app that saves every few seconds) still sync here, once they have
//!    kept changing for a whole interval of the regular look since they began to and since
//!    the job last synced.
//!
//! A sync that is due waits (it isn't dropped) while another sync uses the same folders, and
//! starts as soon as that sync is done. So does a list the person confirmed meanwhile.
//!
//! When a tick comes long after the one before (the computer slept, or the clock on the wall
//! jumped), the shares may have gone away and come back unseen: what looks found missing on
//! them has to be found missing again for a while before a run follows it (see `Missing`).
//!
//! Paused jobs (and everything under "Pause everything") are still looked at, so the
//! window shows what's waiting, but never sync by themselves. A sync the person asked for
//! with Sync all that waits for another sync still starts once that sync is done.

mod schedule;
mod watcher;

pub use schedule::Schedule;
pub use watcher::{FolderWatcher, Route};

use crate::engine::{Engine, Moment, Whereabouts};
use crate::model::{Job, JobId, Trigger};
use crate::plan::Side;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_secs(1);
/// Wait after launch, so the window can appear before any work starts.
const STARTUP_DELAY: Duration = Duration::from_secs(3);
/// After a run, changes seen this soon are Charon's own writes.
const QUIET_AFTER_RUN: Duration = Duration::from_secs(3);
/// How long changes must settle before a job that doesn't sync on change is looked at.
const CHECK_SETTLE: Duration = Duration::from_secs(5);
/// After a tick ran into a mistake in Charon itself, the next one waits this long.
const AFTER_A_PANIC: Duration = Duration::from_secs(60);
/// How often a confirmed sync that waits for another sync tries again (each try asks the
/// disk). A sync of this Charon that ends starts it at once; this is for other Charons'.
const CONFIRMED_RETRY: Duration = Duration::from_secs(5);
/// A tick that comes this long after the one before, by either clock, means Charon wasn't
/// looking for a while: the computer slept, or the clock on the wall jumped.
const AWAY: Duration = Duration::from_secs(30);

pub struct Autopilot {
    engine: Engine,
    watcher: Option<FolderWatcher>,
    changes: Receiver<(JobId, PathBuf)>,
    /// The changes in each job's folders since they were last handled.
    changed: HashMap<JobId, Changes>,
    /// Whether both folders of each job were reachable at the last tick.
    reachable: HashMap<JobId, bool>,
    /// When each job's schedule last came due, also kept here: if the job's record can't be
    /// saved, the schedule still doesn't come due again every second.
    scheduled: HashMap<JobId, DateTime<Utc>>,
    /// Each watched folder as it really is (links resolved), when it could be resolved.
    real_roots: HashMap<PathBuf, Option<PathBuf>>,
    /// When each job's confirmed sync that waits last tried to start.
    confirmed_tried: HashMap<JobId, Instant>,
    /// When the last tick began.
    last_tick: Option<Moment>,
}

impl Autopilot {
    /// Starts the autopilot on its own thread. It runs until the app quits.
    pub fn start(engine: Engine) {
        let spawned = thread::Builder::new().name(crate::engine::AUTOPILOT_THREAD.into()).spawn(move || {
            let (sender, changes) = mpsc::channel();
            let watcher = FolderWatcher::new(sender)
                .map_err(|e| log::error!("folder watching is unavailable: {e}"))
                .ok();
            let mut pilot = Autopilot {
                engine,
                watcher,
                changes,
                changed: HashMap::new(),
                reachable: HashMap::new(),
                scheduled: HashMap::new(),
                real_roots: HashMap::new(),
                confirmed_tried: HashMap::new(),
                last_tick: None,
            };
            thread::sleep(STARTUP_DELAY);
            loop {
                // A mistake in Charon itself stops nothing for good: the next tick tries
                // again, a little later.
                match std::panic::catch_unwind(AssertUnwindSafe(|| pilot.tick())) {
                    Ok(()) => thread::sleep(TICK),
                    Err(_) => {
                        log::error!("the autopilot ran into a panic; it carries on in a minute");
                        thread::sleep(AFTER_A_PANIC);
                    }
                }
            }
        });
        if let Err(e) = spawned {
            log::error!("couldn't start the autopilot: {e}");
        }
    }

    /// One look at every job.
    fn tick(&mut self) {
        self.engine.note_a_tick();
        // After a gap (the computer slept), the shares may have gone away and come back
        // without any tick seeing it.
        let moment = Moment::now();
        if self.last_tick.is_some_and(|last| Self::away_between(last, moment)) {
            self.engine.after_a_gap();
        }
        self.last_tick = Some(moment);
        // A sync dated later than now: the clock was ahead then, or is behind now.
        self.engine.note_syncs_dated_ahead(moment);
        // Notices an older Charon at work, so the window says so before anything tries to sync.
        self.engine.older_charon_running();
        // While Charon's state can't be saved, tries again now and then (at most once a
        // minute), so the header stops saying so once it can, even when no job syncs by itself.
        self.engine.can_save();
        let settings = self.engine.settings();
        let jobs = self.engine.job_list();
        // Every folder is asked about at once, on threads of their own: a share that doesn't
        // answer in time counts as away for this tick, and holds up no other job.
        let whereabouts = self.engine.whereabouts_of(&jobs);
        self.follow_folders(&jobs, &whereabouts);
        self.collect_changes();
        let now = Utc::now();

        // Runs and lists that wait are tried first, for every job, before any job's steps
        // below start a look. On a drive run by an add-on driver, jobs take their turns in the
        // order they began to wait, whatever their order here (see `engine::quiet`).
        let mut ready = Vec::new();
        for (job, found) in jobs.iter().zip(&whereabouts) {
            let id = &job.id;
            // A folder on another drive than at the last sync (a leftover mount folder) isn't
            // the job's folder: nothing is looked at or synced there by itself.
            let present = found.both_there();
            let reachable = found.reachable(job);
            let moved = present && !reachable;
            let was_reachable = self.reachable.insert(id.clone(), reachable);
            if was_reachable.is_some() && was_reachable != Some(reachable) {
                // A drive went away or came (maybe another one under the same name): a list
                // made before can't be confirmed anymore, and what was found before it left
                // says nothing about it now.
                self.engine.mark_review_stale(id);
                if reachable {
                    self.engine.forget_look(id);
                }
                self.engine.publish(id); // the row turns offline / back online
            }
            let automatic = !settings.paused_all && !job.paused;
            // A due sync waits only while the job could still run by itself; while it's
            // paused, only one the person asked for (Sync all) still waits.
            if !present {
                self.engine.unwant(id, "a folder isn't reachable");
            } else if moved {
                self.engine.unwant(id, "a folder is on another drive than at the last sync");
            } else if !job.runs_by_itself() {
                self.engine.unwant(id, "the job waits for your OK");
            } else if !automatic {
                self.engine.unwant_automatic(id, if job.paused { "the job is paused" } else { "everything is paused" });
            }
            // A job whose folder is away doesn't wait for its turn on a drive meanwhile: it
            // would hold up the other jobs there.
            if !present || !reachable {
                self.engine.forget_drive_wait(id);
            }
            if !present || self.engine.is_running(id) {
                continue;
            }
            // A list the person confirmed while another sync used the folders (that sync
            // may have been another Charon's, which says nothing when it's done). Even with a
            // folder on another drive than at the last sync: the person saw that in the list,
            // and `carry_out` makes sure the folders are still the ones it was made for.
            if self.confirmed_tried.get(id).is_none_or(|tried| tried.elapsed() >= CONFIRMED_RETRY) {
                self.confirmed_tried.insert(id.clone(), Instant::now());
                if self.engine.start_confirmed(id) {
                    continue;
                }
            }
            if !reachable {
                continue;
            }
            // A wanted run may start: full automation, nothing waiting for a look, and no
            // list of its changes open for the person. A trigger's own run also needs the
            // job and everything not paused (runs by itself now).
            let may_start = job.runs_by_itself() && !self.engine.review_open(id);
            if may_start {
                self.engine.start_wanted(id);
                if self.engine.is_running(id) {
                    continue;
                }
            }
            ready.push((job, was_reachable, automatic, automatic && may_start));
        }

        for (job, was_reachable, automatic, by_itself) in ready {
            let id = &job.id;
            // 1. Drive connected (also at launch, when the drive was already there).
            if job.setup.trigger == Trigger::DriveConnected && was_reachable != Some(true) {
                if by_itself {
                    self.run(id, "because the drive was connected");
                } else {
                    // Launching Charon with the drive already there isn't news.
                    if was_reachable.is_some() {
                        self.engine.mention_next_look(id);
                    }
                    self.engine.check(id);
                }
                continue;
            }
            // 2. Changes settled: quiet for the settle time since the last change heard, and
            //    since the last sync ended (what changes in a sync's own items while it runs
            //    isn't heard here).
            let settle = Self::settle_time(job);
            if let Some(changes) = self.changed.get(id) {
                if changes.last.elapsed() >= settle && !self.engine.synced_within(id, settle) {
                    self.changed.remove(id);
                    if by_itself && job.setup.trigger.syncs_on_change() {
                        self.run(id, "after changes");
                    } else {
                        self.engine.check(id);
                    }
                    continue;
                }
            }
            // 3. Schedule. The next time counts from now, whether it synced or only looked,
            //    so a job waiting for an OK isn't looked at again every second.
            let since = self.scheduled.get(id).copied();
            if automatic && Schedule::is_due_after(job, since, now) {
                self.scheduled.insert(id.clone(), now);
                self.engine.mark_scheduled(id);
                if by_itself {
                    self.run(id, "on schedule");
                } else {
                    self.engine.check(id);
                }
                continue;
            }
            // 4. Removals left for later.
            if by_itself && self.engine.take_follow_up(id) {
                self.run(id, "to carry out removals left for later");
                continue;
            }
            // 5. Regular look. While the folders still change, step 2 waits for them to settle,
            //    and a sync that ended less than the settle time ago counts as a change (what
            //    was saved into its own items meanwhile isn't heard). Folders that never
            //    settle still sync here, but only once they have kept changing for a whole
            //    interval of the regular look since they began to and since the job last
            //    synced.
            let interval = settings.check_interval();
            let settling = self.engine.synced_within(id, settle)
                || self.changed.get(id).is_some_and(|changes| changes.first.elapsed() < interval || self.engine.synced_within(id, interval));
            if by_itself && job.setup.trigger.syncs_on_change() && !settling && self.engine.has_pending(id) {
                self.run(id, "for changes found by a look");
            } else if self.engine.check_due(id, interval) {
                self.engine.check(id);
            }
        }
        self.reachable.retain(|id, _| jobs.iter().any(|job| &job.id == id));
        self.scheduled.retain(|id, _| jobs.iter().any(|job| &job.id == id));
        self.confirmed_tried.retain(|id, _| jobs.iter().any(|job| &job.id == id));
        self.engine.remind();
    }

    /// Whether Charon wasn't looking for a while between a tick at `last` and one at `now`:
    /// they are far apart by Charon's own clock (which may stand still while the computer
    /// sleeps) or by the clock on the wall (which may jump).
    fn away_between(last: Moment, now: Moment) -> bool {
        let own = now.own.saturating_duration_since(last.own);
        let wall = (now.wall - last.wall).to_std().unwrap_or_default();
        own.max(wall) >= AWAY
    }

    /// How long a job's folders must be quiet, since the last change heard and since the job
    /// last synced, before their changes count as settled: the job's settle time when it
    /// syncs on change, a moment for any other job.
    fn settle_time(job: &Job) -> Duration {
        match job.setup.trigger {
            Trigger::OnChange { settle_seconds } => Duration::from_secs(settle_seconds as u64),
            _ => CHECK_SETTLE,
        }
    }

    /// Starts a sync of a job that runs by itself, or keeps it waiting until nothing stands
    /// in its way.
    fn run(&self, id: &JobId, reason: &str) {
        self.engine.want(id, reason);
        self.engine.start_wanted(id);
    }

    /// Watches the folders of every job that are there (as `whereabouts` found them), but not
    /// those on a drive a sync keeps quiet (see `routes`).
    fn follow_folders(&mut self, jobs: &[Job], whereabouts: &[Whereabouts]) {
        if self.watcher.is_none() {
            return;
        }
        let keeps = self.engine.quiet_keeps();
        let routes = self.routes(jobs, whereabouts);
        if let Some(watcher) = self.watcher.as_mut() {
            watcher.follow(routes);
        }
        // A sync that keeps a drive quiet waits for this before it writes there.
        self.engine.quiet_followed(keeps);
    }

    /// The folders to watch: every job's folders that are there (as `whereabouts` found them).
    /// Each folder is known by its path as the job names it and as it really is (a link
    /// resolved), since some systems report changes under the real path. A folder on a drive a
    /// sync keeps quiet isn't watched until the sync is done: watching it has the system work
    /// on the drive for Charon too (see `engine::quiet`). What changes there meanwhile isn't
    /// heard; the regular look finds it.
    fn routes(&mut self, jobs: &[Job], whereabouts: &[Whereabouts]) -> Vec<Route> {
        let mut routes: Vec<Route> = Vec::new();
        let mut real = HashMap::new();
        for (job, found) in jobs.iter().zip(whereabouts) {
            for side in [Side::Source, Side::Destination] {
                let root = job.setup.root(side).to_path_buf();
                if !found.side(side).is_there() || self.engine.kept_quiet(&root) {
                    continue;
                }
                // Resolved again only for a folder that just came (back), not every second.
                // One that doesn't answer in time is watched from a later tick on.
                let resolved = match self.real_roots.remove(&root) {
                    Some(resolved) => resolved,
                    None => match self.engine.ask_about(&root, |root| std::fs::canonicalize(root).ok()) {
                        Some(resolved) => resolved,
                        None => continue,
                    },
                };
                real.insert(root.clone(), resolved.clone());
                routes.push(Route::new(&root, resolved, &job.id));
            }
        }
        self.real_roots = real;
        routes
    }

    /// Notes which jobs saw changes that matter, leaving out changes to excluded items, and
    /// Charon's own writes during and just after a sync (a change elsewhere during a sync
    /// counts, and also has the job looked at once its sync is done).
    fn collect_changes(&mut self) {
        while let Ok((id, path)) = self.changes.try_recv() {
            let matters = if self.engine.synced_within(&id, QUIET_AFTER_RUN) {
                self.engine.note_change_while_syncing(&id, &path)
            } else {
                self.engine.note_change(&id, &path)
            };
            if matters {
                let now = Instant::now();
                self.changed.entry(id).and_modify(|changes| changes.last = now).or_insert(Changes { first: now, last: now });
            }
        }
    }
}

/// Changes in a job's folders that haven't been handled yet.
#[derive(Debug, Clone, Copy)]
struct Changes {
    /// When the first of them came.
    first: Instant,
    /// When the last of them came.
    last: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineEvent;
    use crate::model::{ConflictRule, JobSetup, Phase, SyncMode};
    use std::collections::HashSet;
    use std::fs;
    use std::path::Path;
    use std::sync::mpsc::Sender;
    use std::sync::{Arc, Mutex};

    /// How long any wait may take before the test fails.
    const DEADLINE: Duration = Duration::from_secs(30);

    /// An autopilot for `engine` that hears about changes in the folders from the sender it
    /// comes with, instead of watching them.
    fn pilot(engine: &Engine) -> (Autopilot, Sender<(JobId, PathBuf)>) {
        let (sender, changes) = mpsc::channel();
        let pilot = Autopilot {
            engine: engine.clone(),
            watcher: None,
            changes,
            changed: HashMap::new(),
            reachable: HashMap::new(),
            scheduled: HashMap::new(),
            real_roots: HashMap::new(),
            confirmed_tried: HashMap::new(),
            last_tick: None,
        };
        (pilot, sender)
    }

    fn until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + DEADLINE;
        while !done() {
            assert!(Instant::now() < deadline, "{what} took too long");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Two jobs back up into folders on a stick run by an add-on driver, "Documents" and
    /// "Photos", and "Notes" from one folder of this computer's own disk to another. Returns
    /// the engine, the jobs, and the folders on the stick.
    fn jobs_on_a_stick(tmp: &Path) -> (Engine, crate::scan::pretend::AddOnDrive, [JobId; 3], [PathBuf; 2]) {
        let (home, stick) = (tmp.join("home"), tmp.join("stick"));
        let pretending = crate::scan::pretend::AddOnDrive::at(&stick);
        let engine = Engine::open(tmp.join("data"), Arc::new(|_| {})).unwrap();
        let add = |name: &str, source: PathBuf, destination: PathBuf| {
            fs::create_dir_all(&source).unwrap();
            fs::create_dir_all(&destination).unwrap();
            fs::write(source.join("a.txt"), "a").unwrap();
            let setup = JobSetup {
                name: name.into(),
                source,
                destination: destination.clone(),
                mode: SyncMode::Backup,
                conflict: ConflictRule::Ask,
                trigger: Trigger::Manual,
                excludes: vec![],
                safe_delete: true,
                full_automation: false,
            };
            (engine.add_job(setup).unwrap().job.id, destination)
        };
        let (documents, docs_on_stick) = add("Documents", home.join("docs"), stick.join("docs"));
        let (photos, photos_on_stick) = add("Photos", home.join("photos"), stick.join("photos"));
        let (notes, _) = add("Notes", home.join("notes"), home.join("copies"));
        until("the first looks", || engine.job_list().iter().all(|job| !engine.job(&job.id).unwrap().looking));
        engine.forget_drive_waits();
        (engine, pretending, [documents, photos, notes], [docs_on_stick, photos_on_stick])
    }

    #[test]
    fn folders_on_a_drive_kept_quiet_arent_watched_until_it_is_free() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, _stick, [documents, photos, notes], [docs_on_stick, photos_on_stick]) = jobs_on_a_stick(tmp.path());
        let (mut pilot, _changes) = pilot(&engine);
        let jobs = engine.job_list();
        let mut watched = || pilot.routes(&jobs, &engine.whereabouts_of(&jobs)).iter().map(|route| route.root().to_path_buf()).collect::<HashSet<_>>();
        let all = watched();
        assert_eq!(all.len(), 6, "every folder is watched");
        // A list of Photos waits for the person's OK while Documents syncs.
        let list = engine.prepare_review(&photos).unwrap();

        let held = engine.hold_a_sync(&documents).unwrap();
        let while_held = watched();
        assert!(!while_held.contains(&docs_on_stick) && !while_held.contains(&photos_on_stick), "nothing on the stick is watched");
        assert_eq!(while_held.len(), 4, "the folders elsewhere still are");
        for id in [&photos, &notes] {
            engine.just_looked_at(id);
        }

        drop(held);
        assert_eq!(watched(), all, "watched again once the sync is done");
        // What changed on the stick meanwhile wasn't heard. The regular look finds it, as on a
        // network share: nothing is looked at, started or refused for it now.
        for id in [&photos, &notes] {
            assert!(!engine.check_due(id, Duration::from_secs(3600)), "not looked at again at once");
        }
        assert!(!engine.list_is_stale(&photos, &list.review_id), "the list of Photos can still be confirmed");
        pilot.collect_changes();
        assert!(pilot.changed.is_empty(), "nothing counts as a change heard");
    }

    #[test]
    fn a_tick_lets_a_sync_that_keeps_a_drive_quiet_go_ahead() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, _stick, [documents, ..], [docs_on_stick, _]) = jobs_on_a_stick(tmp.path());
        let (mut pilot, _changes) = pilot(&engine);
        let (sender, changes) = mpsc::channel();
        pilot.watcher = Some(FolderWatcher::new(sender).unwrap());
        pilot.changes = changes;
        pilot.tick();
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
        // The sync waits for the folder watcher to stop watching the stick, and the next tick
        // does, and says so: the sync doesn't wait out its time limit.
        until("the sync's keep", || engine.kept_quiet(&docs_on_stick) || syncing.is_finished());
        while !syncing.is_finished() {
            pilot.tick();
            thread::sleep(Duration::from_millis(20));
        }
        let waited = syncing.join().unwrap();
        assert!(waited < Duration::from_secs(2), "{waited:?}");
    }

    /// `jobs_on_a_stick`, and "Backup", listed after them, which syncs onto the stick by
    /// itself: its first sync is done, and a new file waits to be copied. A look at Documents
    /// reads the stick; the other jobs were just looked at. Returns the four jobs and Backup's
    /// folder on the stick.
    fn a_backup_onto_the_stick(tmp: &Path) -> (Engine, crate::scan::pretend::AddOnDrive, [JobId; 4], PathBuf) {
        let (engine, stick, [documents, photos, notes], _) = jobs_on_a_stick(tmp);
        let (source, destination) = (tmp.join("home").join("backup"), tmp.join("stick").join("backup"));
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(source.join("a.txt"), "a").unwrap();
        let setup = JobSetup {
            name: "Backup".into(),
            source: source.clone(),
            destination: destination.clone(),
            mode: SyncMode::Backup,
            conflict: ConflictRule::Ask,
            trigger: Trigger::Manual,
            excludes: vec![],
            safe_delete: true,
            full_automation: true,
        };
        let backup = engine.add_job(setup).unwrap().job.id;
        until("the first looks", || [&documents, &photos, &notes, &backup].iter().all(|id| !engine.job(id).unwrap().looking));
        let review = engine.prepare_review(&backup).unwrap();
        engine.approve(&backup, &review.review_id, true).unwrap();
        until("the first sync", || !engine.is_running(&backup));
        fs::write(source.join("b.txt"), "b").unwrap();
        engine.forget_drive_waits();
        engine.whereabouts_of(&engine.job_list());
        for id in [&photos, &notes, &backup] {
            engine.just_looked_at(id);
        }
        engine.begin_a_look(&documents);
        (engine, stick, [documents, photos, notes, backup], destination)
    }

    #[test]
    fn a_run_that_waited_for_a_look_on_the_drive_goes_before_the_next_look_there() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, _stick, [documents, photos, _, backup], destination) = a_backup_onto_the_stick(tmp.path());
        let (mut pilot, _changes) = pilot(&engine);
        // Backup's run waits for Documents' look, and then Photos' look does too.
        engine.want(&backup, "after changes");
        engine.start_wanted(&backup);
        assert_eq!(engine.job(&backup).unwrap().waiting_for.as_deref(), Some("Documents"));
        engine.check(&photos);
        assert!(engine.look_held_back(&photos));

        // The look ends. At the next tick the run goes first, though Photos is listed before
        // it (or Photos' look would start, and Backup wait for it; then Documents' look, and so
        // on), and Photos' look comes once the run is done.
        engine.end_a_look(&documents);
        pilot.tick();
        assert!(!engine.run_wanted(&backup), "the run started");
        until("the sync", || !engine.is_running(&backup));
        assert!(destination.join("b.txt").exists());
        let deadline = Instant::now() + DEADLINE;
        while engine.look_held_back(&photos) {
            assert!(Instant::now() < deadline, "Photos' look took too long to come");
            pilot.tick();
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_look_that_began_to_wait_on_the_drive_before_a_run_goes_first() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, _stick, [documents, photos, _, backup], destination) = a_backup_onto_the_stick(tmp.path());
        let (mut pilot, _changes) = pilot(&engine);
        // Photos' look waits for Documents' look, and then Backup's run does too.
        engine.check(&photos);
        assert!(engine.look_held_back(&photos));
        engine.want(&backup, "after changes");
        engine.start_wanted(&backup);
        assert_eq!(engine.job(&backup).unwrap().waiting_for.as_deref(), Some("Documents"));

        // The look ends. At the next tick Photos' look goes first, though the runs that wait
        // are tried first, and the run waits for it.
        engine.end_a_look(&documents);
        pilot.tick();
        assert!(!engine.look_held_back(&photos), "Photos' look came");
        assert!(engine.run_wanted(&backup), "the run waits");
        assert_eq!(engine.job(&backup).unwrap().waiting_for.as_deref(), Some("Photos"));
        // The run comes once the look is done.
        let deadline = Instant::now() + DEADLINE;
        while engine.run_wanted(&backup) {
            assert!(Instant::now() < deadline, "the run took too long to come");
            pilot.tick();
            thread::sleep(Duration::from_millis(20));
        }
        until("the sync", || !engine.is_running(&backup));
        assert!(destination.join("b.txt").exists());
    }

    #[test]
    fn a_job_whose_folder_went_away_holds_up_no_turn_on_the_drive() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, _stick, [documents, photos, _, backup], destination) = a_backup_onto_the_stick(tmp.path());
        let (mut pilot, _changes) = pilot(&engine);
        // Photos' look waits for Documents' look, and then Backup's run does too.
        engine.check(&photos);
        assert!(engine.look_held_back(&photos));
        engine.want(&backup, "after changes");
        engine.start_wanted(&backup);

        // The look ends, and Photos' folder at home goes away (an unplugged drive): its look
        // can't come, and the run doesn't wait for it.
        engine.end_a_look(&documents);
        fs::remove_dir_all(tmp.path().join("home").join("photos")).unwrap();
        pilot.tick();
        assert!(!engine.run_wanted(&backup), "the run started");
        until("the sync", || !engine.is_running(&backup));
        assert!(destination.join("b.txt").exists());
    }

    /// A two-way job with full automation that syncs once changes have settled for
    /// `settle_seconds`, after its first sync was confirmed.
    fn on_change_job(tmp: &Path, settle_seconds: u32) -> (Engine, JobId, PathBuf, PathBuf) {
        let engine = Engine::open(tmp.join("data"), Arc::new(|_| {})).unwrap();
        let (id, a, b) = add_on_change_job(&engine, tmp, settle_seconds, ConflictRule::Ask);
        (engine, id, a, b)
    }

    /// Adds such a job to `engine`, with `conflict` for what changed on both sides.
    fn add_on_change_job(engine: &Engine, tmp: &Path, settle_seconds: u32, conflict: ConflictRule) -> (JobId, PathBuf, PathBuf) {
        let (a, b) = (tmp.join("a"), tmp.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("first.txt"), "first").unwrap();
        let setup = JobSetup {
            name: "Notes".into(),
            source: a.clone(),
            destination: b.clone(),
            mode: SyncMode::TwoWay,
            conflict,
            trigger: Trigger::OnChange { settle_seconds },
            excludes: vec![],
            safe_delete: true,
            full_automation: true,
        };
        let id = engine.add_job(setup).unwrap().job.id;
        until("the first look", || !engine.job(&id).unwrap().looking);
        let review = engine.prepare_review(&id).unwrap();
        engine.approve(&id, &review.review_id, true).unwrap();
        until("the first sync", || !engine.is_running(&id));
        assert!(b.join("first.txt").exists());
        (id, a, b)
    }

    #[test]
    fn an_on_change_job_waits_for_its_folders_to_settle_even_when_a_look_found_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id, a, b) = on_change_job(tmp.path(), 30);
        let (mut pilot, changes) = pilot(&engine);
        // A look found an app's file, which the app is still saving.
        fs::write(a.join("notes.txt"), "draft").unwrap();
        engine.check(&id);
        until("the look", || engine.has_pending(&id));
        changes.send((id.clone(), a.join("notes.txt"))).unwrap();
        pilot.tick();
        until("the tick's sync, if any", || !engine.is_running(&id));
        assert!(!b.join("notes.txt").exists(), "synced while the folder still changed");

        // Quiet for the settle time since the change, and since the last sync: it syncs.
        let quiet = Instant::now().checked_sub(Duration::from_secs(31)).expect("the computer has run for half a minute");
        engine.pretend_the_last_sync_ended(&id, Moment { wall: Utc::now() - chrono::Duration::seconds(31), own: quiet });
        pilot.changed.insert(id.clone(), Changes { first: quiet, last: quiet });
        pilot.tick();
        until("the sync", || !engine.is_running(&id));
        assert!(b.join("notes.txt").exists());
    }

    #[test]
    fn an_on_change_job_whose_folders_never_settle_still_syncs_once_a_regular_look() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id, a, b) = on_change_job(tmp.path(), 30);
        let mut settings = engine.settings();
        settings.check_minutes = 1;
        engine.set_settings(settings).unwrap();
        let interval = engine.settings().check_interval();
        let (mut pilot, changes) = pilot(&engine);
        // An app saves notes.txt every few seconds, for hours: the folder is never quiet for
        // the settle time.
        let save = |version: &str| {
            fs::write(a.join("notes.txt"), version).unwrap();
            changes.send((id.clone(), a.join("notes.txt"))).unwrap();
            engine.check(&id);
            until("the look", || engine.has_pending(&id));
        };
        let synced = || {
            until("the tick's sync, if any", || !engine.is_running(&id));
            fs::read_to_string(b.join("notes.txt")).ok()
        };
        // The job last synced long ago, but the folder has only just begun to change: it waits
        // for the folder to settle.
        let long_ago = Moment { wall: Utc::now() - chrono::Duration::from_std(interval).unwrap(), own: Instant::now().checked_sub(interval).expect("the computer has run for a minute") };
        engine.pretend_the_last_sync_ended(&id, long_ago);
        save("1");
        pilot.tick();
        assert_eq!(synced(), None, "synced while the folder had only just begun to change");

        // It has kept changing for as long as the regular look waits: the regular look syncs it.
        pilot.changed.get_mut(&id).unwrap().first = long_ago.own;
        save("2");
        pilot.tick();
        assert_eq!(synced().as_deref(), Some("2"));

        // That sync took longer than the settle time, and the app saved while it ran: the
        // folder hasn't been quiet since it ended, so no other sync follows at once.
        pilot.changed.get_mut(&id).unwrap().last = Instant::now().checked_sub(Duration::from_secs(31)).expect("the computer has run for half a minute");
        fs::write(a.join("notes.txt"), "3").unwrap();
        engine.check(&id);
        until("the look", || engine.has_pending(&id));
        pilot.tick();
        assert_eq!(synced().as_deref(), Some("2"), "synced again as soon as that sync ended");

        // It goes on changing: no sync again until the regular look's interval has passed
        // since that one.
        save("3");
        pilot.tick();
        assert_eq!(synced().as_deref(), Some("2"), "synced again before the regular look's interval passed");
        engine.pretend_the_last_sync_ended(&id, long_ago);
        pilot.tick();
        assert_eq!(synced().as_deref(), Some("3"));
    }

    #[test]
    fn an_on_change_job_never_syncs_again_right_after_its_sync_while_an_app_keeps_saving() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id, a, b) = on_change_job(tmp.path(), 30);
        let (mut pilot, changes) = pilot(&engine);
        let save = |version: &str| {
            fs::write(a.join("notes.txt"), version).unwrap();
            changes.send((id.clone(), a.join("notes.txt"))).unwrap();
        };
        let synced = || {
            until("the tick's sync, if any", || !engine.is_running(&id));
            fs::read_to_string(b.join("notes.txt")).ok()
        };
        // A sync starts while no change waits to settle (it came from Sync all, or to carry out
        // removals left for later), and an app saves notes.txt while the sync still looks at
        // the folders.
        save("first");
        let carry_on = engine.begin_automatic_run(&id, "to carry out removals left for later");
        save("second version");
        pilot.tick();
        assert!(pilot.changed.contains_key(&id), "a save heard while the sync looked doesn't count as a change");
        carry_on();
        let after_the_sync = synced();
        assert!(after_the_sync.is_some());

        // The app saves again right after it, into one of the sync's own items: that isn't heard
        // as a change. The sync's plan was outdated, so the folders are looked at again at once.
        save("the third version");
        pilot.tick();
        until("the look after the sync", || engine.has_pending(&id));
        pilot.tick();
        assert_eq!(synced(), after_the_sync, "synced again right after the last sync, while the app still saved");

        // The app stops. Once the folders have been quiet for the settle time, since the last
        // save and since the sync, the job syncs, and the last save is carried too.
        let quiet = Instant::now().checked_sub(Duration::from_secs(31)).expect("the computer has run for half a minute");
        engine.pretend_the_last_sync_ended(&id, Moment { wall: Utc::now() - chrono::Duration::seconds(31), own: quiet });
        pilot.changed.insert(id.clone(), Changes { first: quiet, last: quiet });
        pilot.tick();
        assert_eq!(synced().as_deref(), Some("the third version"));
    }

    #[test]
    fn a_syncs_own_conflict_copy_heard_while_it_runs_is_no_change() {
        let tmp = tempfile::tempdir().unwrap();
        // Once armed, the next sync stops on its own thread right after its last step, still
        // running: it says so, and waits until the test lets it go on.
        type Hold = (Sender<()>, Receiver<()>);
        let armed: Arc<Mutex<Option<Hold>>> = Arc::default();
        let hold = armed.clone();
        let emitter = Arc::new(move |event: EngineEvent| {
            let EngineEvent::Progress { progress, .. } = event else { return };
            let held = if progress.phase == Phase::Finishing { hold.lock().unwrap().take() } else { None };
            if let Some((reached, go_on)) = held {
                reached.send(()).unwrap();
                let _ = go_on.recv_timeout(DEADLINE);
            }
        });
        let engine = Engine::open(tmp.path().join("data"), emitter).unwrap();
        let (id, a, b) = add_on_change_job(&engine, tmp.path(), 30, ConflictRule::KeepBoth);
        let (mut pilot, changes) = pilot(&engine);
        let (reached, at_the_end) = mpsc::channel();
        let (go_on, held) = mpsc::channel();
        *armed.lock().unwrap() = Some((reached, held));

        // first.txt changed on both sides, and the folders have been quiet for the settle time:
        // the job syncs, keeping both versions.
        fs::write(a.join("first.txt"), "the source's version").unwrap();
        fs::write(b.join("first.txt"), "the destination's version").unwrap();
        let quiet = Instant::now().checked_sub(Duration::from_secs(31)).expect("the computer has run for half a minute");
        engine.pretend_the_last_sync_ended(&id, Moment { wall: Utc::now() - chrono::Duration::seconds(31), own: quiet });
        pilot.changed.insert(id.clone(), Changes { first: quiet, last: quiet });
        pilot.tick();
        at_the_end.recv_timeout(DEADLINE).expect("the sync reached its end");

        // While it still runs, the folder watcher reports the conflict copy it made on both sides.
        let copy = fs::read_dir(&b).unwrap().flatten().map(|item| item.file_name()).find(|name| name.to_string_lossy().starts_with("first (conflict "));
        let copy = copy.expect("both versions are kept");
        assert!(engine.is_running(&id));
        for folder in [&a, &b] {
            changes.send((id.clone(), folder.join(&copy))).unwrap();
        }
        pilot.tick();
        go_on.send(()).unwrap();
        until("the sync", || !engine.is_running(&id));
        assert!(engine.job(&id).unwrap().job.last_run.unwrap().error.is_none());
        assert!(!pilot.changed.contains_key(&id), "the sync's own conflict copy counts as a change: the job syncs again once its settle time has passed");
        assert!(!engine.check_due(&id, Duration::from_secs(15 * 60)), "the sync's own conflict copy has the folders looked at again at once");
    }

    #[test]
    fn an_on_change_job_waits_its_settle_time_after_a_sync_before_it_syncs_what_a_look_found() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id, a, b) = on_change_job(tmp.path(), 30);
        let (mut pilot, _changes) = pilot(&engine);
        // Right after the job's last sync, an app saves first.txt, which that sync wrote. The
        // save can't be told from the sync's own write, so it isn't heard as a change; a look
        // finds it.
        engine.pretend_the_last_sync_ended(&id, Moment::now());
        fs::write(a.join("first.txt"), "first, as the app saved it").unwrap();
        engine.check(&id);
        until("the look", || engine.has_pending(&id));
        pilot.tick();
        until("the tick's sync, if any", || !engine.is_running(&id));
        assert_eq!(fs::read_to_string(b.join("first.txt")).unwrap(), "first", "synced right after the last sync");

        // Once the settle time has passed since that sync, it syncs.
        let quiet = Instant::now().checked_sub(Duration::from_secs(31)).expect("the computer has run for half a minute");
        engine.pretend_the_last_sync_ended(&id, Moment { wall: Utc::now() - chrono::Duration::seconds(31), own: quiet });
        pilot.tick();
        until("the sync", || !engine.is_running(&id));
        assert_eq!(fs::read_to_string(b.join("first.txt")).unwrap(), "first, as the app saved it");
    }

    #[test]
    fn a_share_that_doesnt_answer_counts_as_away_and_holds_up_no_other_job() {
        let tmp = tempfile::tempdir().unwrap();
        let [home, usb, share, copy] = ["home", "usb", "share", "copy"].map(|name| tmp.path().join(name));
        for folder in [&home, &usb, &share, &copy] {
            fs::create_dir_all(folder).unwrap();
        }
        let engine = Engine::open(tmp.path().join("data"), Arc::new(|_| {})).unwrap();
        let job = |name: &str, source: &Path, destination: &Path| JobSetup {
            name: name.into(),
            source: source.to_path_buf(),
            destination: destination.to_path_buf(),
            mode: SyncMode::Backup,
            conflict: ConflictRule::Ask,
            trigger: Trigger::DriveConnected,
            excludes: vec![],
            safe_delete: true,
            full_automation: false,
        };
        let local = engine.add_job(job("Home", &home, &usb)).unwrap().job.id;
        let shared = engine.add_job(job("Share", &share, &copy)).unwrap().job.id;
        let (mut pilot, _changes) = pilot(&engine);
        pilot.tick();
        assert_eq!((pilot.reachable[&local], pilot.reachable[&shared]), (true, true));
        until("the first looks", || [&local, &shared].iter().all(|id| !engine.job(id).unwrap().looking));

        // The share stops answering: it counts as away, at once, and the other job carries on.
        let answer = engine.silence(&share);
        fs::write(home.join("letter.txt"), "letter").unwrap();
        let settled = Instant::now().checked_sub(CHECK_SETTLE).expect("the computer has run for a few seconds");
        pilot.changed.insert(local.clone(), Changes { first: settled, last: settled });
        let started = Instant::now();
        pilot.tick();
        assert!(started.elapsed() < crate::engine::reach::LIMIT, "the tick waited {:?}", started.elapsed());
        assert_eq!((pilot.reachable[&local], pilot.reachable[&shared]), (true, false));
        assert_eq!(engine.job(&shared).unwrap().status, crate::model::JobStatus::Offline);
        until("the look at the other job", || engine.job(&local).unwrap().waiting.is_some_and(|waiting| waiting.items == 1));

        // Once it answers again, it's back.
        answer.send(()).unwrap();
        until("the share's answer", || !engine.is_silent(&share));
        pilot.tick();
        assert!(pilot.reachable[&shared]);
    }

    #[test]
    fn a_tick_long_after_the_one_before_by_either_clock_means_charon_was_away() {
        let last = Moment::now();
        let next = last.after(TICK);
        assert!(!Autopilot::away_between(last, next));
        // The computer slept: Charon's own clock stood still (Linux, macOS), the wall's went on.
        assert!(Autopilot::away_between(last, Moment { wall: last.wall + chrono::Duration::hours(5), own: next.own }));
        // The computer slept, and Charon's own clock counted it too (Windows).
        assert!(Autopilot::away_between(last, last.after(Duration::from_secs(5 * 60 * 60))));
        // The clock on the wall jumped forward.
        assert!(Autopilot::away_between(last, Moment { wall: next.wall + chrono::Duration::minutes(10), own: next.own }));
        // A clock set back says nothing about time away.
        assert!(!Autopilot::away_between(last, Moment { wall: last.wall - chrono::Duration::hours(1), own: next.own }));
    }

    #[test]
    fn after_a_tick_long_after_the_one_before_items_missing_on_a_share_are_looked_for_again() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id, _, _) = on_change_job(tmp.path(), 30);
        let (mut pilot, _changes) = pilot(&engine);
        let minutes = |n: u64| Duration::from_secs(n * 60);
        // A run left the removal of photo.jpg for later: it had only just gone missing on a share.
        let start = Moment::now();
        engine.leave_a_removal_for_later(&id, "photo.jpg", start);
        // The tick before this one was five hours ago by the clock on the wall: the computer
        // slept, and the share may have gone away and come back meanwhile.
        pilot.last_tick = Some(Moment { wall: start.wall - chrono::Duration::hours(5), own: start.own });
        pilot.tick();
        assert!(!engine.take_follow_up_at(&id, start.after(minutes(10))), "no look since the computer woke up has found it missing");
        // The looks since then have to find it missing for a while first.
        let looked = start.after(minutes(10));
        engine.leave_a_removal_for_later(&id, "photo.jpg", looked);
        assert!(!engine.take_follow_up_at(&id, looked.after(minutes(1))));
        assert!(engine.take_follow_up_at(&id, looked.after(minutes(2))));
    }

    /// The job's last sync as Charon keeps it (in its state file too), and as its row shows it.
    fn last_synced(engine: &Engine, id: &JobId, data: &Path) -> (DateTime<Utc>, DateTime<Utc>, DateTime<Utc>) {
        let kept = engine.job_list().into_iter().find(|job| &job.id == id).and_then(|job| job.last_synced).unwrap();
        let state: serde_json::Value = serde_json::from_slice(&fs::read(data.join("charon-state.json")).unwrap()).unwrap();
        let saved = state["jobs"].as_array().unwrap().iter().find(|job| job["id"] == id.as_str()).unwrap()["lastSynced"].clone();
        let saved: DateTime<Utc> = serde_json::from_value(saved).unwrap();
        (kept, saved, engine.job(id).unwrap().job.last_synced.unwrap())
    }

    #[test]
    fn a_sync_dated_later_than_now_counts_as_made_when_charon_finds_it() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let (engine, id, _, b) = on_change_job(tmp.path(), 30);
        let (mut pilot, _changes) = pilot(&engine);
        let (synced, _, shown) = last_synced(&engine, &id, &data);
        pilot.tick();
        assert_eq!(shown, synced);
        assert_eq!(last_synced(&engine, &id, &data), (synced, synced, synced), "a sync dated before now shows as it was");

        // The job synced while the clock was three days ahead, which has been set right since,
        // and its drive then went into a drawer.
        let ahead = Utc::now() + chrono::Duration::days(3);
        engine.pretend_the_last_sync_ended(&id, Moment { wall: ahead, own: Instant::now() });
        fs::remove_dir_all(&b).unwrap();
        let set_right = Utc::now();
        pilot.tick();
        let (kept, saved, shown) = last_synced(&engine, &id, &data);
        assert_eq!((kept, saved), (ahead, ahead), "the date Charon keeps changed");
        assert!(set_right <= shown && shown <= Utc::now(), "the row shows {shown}");
        // A week later, with the clock past the date by then, the row says the job hasn't
        // synced since.
        let later = Moment::now().after(Duration::from_secs((7 * 24 + 1) * 60 * 60));
        engine.note_syncs_dated_ahead(later);
        let job = Job { last_synced: engine.last_synced_at(&id, later), ..engine.job(&id).unwrap().job };
        assert_eq!(Engine::stale(&job, None, true, later.wall).as_deref(), Some("Not synced for 7 days"));
    }

    #[test]
    fn a_clock_behind_at_launch_never_dates_a_recent_sync_years_back() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let (engine, id, _, b) = on_change_job(tmp.path(), 30);
        let (mut pilot, _changes) = pilot(&engine);
        // The computer starts with its clock years behind (a flat clock battery: here, that's
        // the clock the test runs by). The job really synced a day ago, and its drive is in a
        // drawer.
        let behind = chrono::Duration::days(2850);
        let synced = Utc::now() + behind - chrono::Duration::days(1);
        engine.pretend_the_last_sync_ended(&id, Moment { wall: synced, own: Instant::now() });
        fs::remove_dir_all(&b).unwrap();
        pilot.tick();
        let (kept, saved, shown) = last_synced(&engine, &id, &data);
        assert_eq!((kept, saved), (synced, synced), "the date Charon keeps changed");
        assert!(shown <= Utc::now(), "until the clock is right, the row counts the sync as made when Charon found it so");

        // Once the system sets the clock right, the row says when the job really synced, and
        // that's not long ago.
        let set_right = Moment { wall: Utc::now() + behind, own: Instant::now() };
        engine.note_syncs_dated_ahead(set_right);
        let shown = engine.last_synced_at(&id, set_right);
        assert_eq!(shown, Some(synced));
        let job = Job { last_synced: shown, ..engine.job(&id).unwrap().job };
        assert_eq!(Engine::stale(&job, None, true, set_right.wall), None);
    }
}
