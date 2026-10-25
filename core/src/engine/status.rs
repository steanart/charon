//! Turning a stored job and its live state into the row the window shows.

use crate::autopilot::Schedule;
use crate::engine::{Engine, FoundAhead, JobRuntime, Look, Moment, Presence, Whereabouts};
use crate::model::{Job, JobStatus, JobView, Pending, Progress, RunSummary, Settings, Trigger, VolumeLabel, Waiting};
use chrono::{DateTime, Datelike, Duration, Local, Utc};
use std::sync::atomic::Ordering;

/// The row of a job whose source folder doesn't answer.
pub(crate) const SOURCE_SILENT: &str = "The source folder doesn't answer. If it's on a network share, the server may be off or out of reach.";
/// The row of a job whose destination doesn't answer.
pub(crate) const DESTINATION_SILENT: &str = "The destination doesn't answer. If it's on a network share, the server may be off or out of reach.";

/// A job's live state at the moment its row was taken, with the row's number. Both are
/// taken under the same locks, so a row with a higher number never shows an older state.
pub(crate) struct Snapshot {
    /// The stored job, with its last sync as the row shows it (see `Engine::last_synced`).
    job: Job,
    running: bool,
    /// A look at the folders runs now (in the background, or for a list).
    looking: bool,
    /// Runs nobody looks at are refused until the person confirms a list.
    must_ask: bool,
    progress: Option<Progress>,
    look: Option<Look>,
    problem: Option<String>,
    waiting_for: Option<String>,
    /// The person confirmed the list, and its sync waits for `waiting_for`.
    confirmed: bool,
    rev: u64,
}

impl Engine {
    /// Takes a job's row state. Called with the state lock and then the runtime lock held
    /// (the documented order).
    pub(crate) fn snapshot(&self, job: &Job, rt: Option<&JobRuntime>) -> Snapshot {
        let last_synced = Self::last_synced(job, rt.and_then(|rt| rt.dated_ahead.as_ref()), Moment::now());
        let snapshot = Snapshot {
            job: Job { last_synced, ..job.clone() },
            running: rt.is_some_and(|rt| rt.is_running()),
            looking: rt.is_some_and(|rt| rt.is_checking() || rt.preparing.is_some()),
            must_ask: rt.is_some_and(|rt| rt.must_ask),
            progress: rt.and_then(|rt| rt.progress.clone()),
            look: rt.and_then(|rt| rt.look.clone()),
            problem: rt.and_then(|rt| rt.problem.clone()),
            waiting_for: rt.and_then(|rt| rt.waiting_for.clone()),
            confirmed: rt.is_some_and(|rt| rt.confirmed.is_some()),
            rev: 0,
        };
        Snapshot { rev: self.inner.rev.fetch_add(1, Ordering::Relaxed) + 1, ..snapshot }
    }

    /// The rows the window shows, made outside the locks: they ask the disk about every
    /// job's folders, all at once and with a time limit (see `whereabouts_of`).
    pub(crate) fn views_of(&self, snapshots: Vec<Snapshot>, settings: &Settings) -> Vec<JobView> {
        let whereabouts = self.whereabouts_of(snapshots.iter().map(|snapshot| &snapshot.job));
        snapshots.into_iter().zip(whereabouts).map(|(snapshot, found)| Self::view_with(snapshot, settings, &found)).collect()
    }

    /// The row the window shows, made outside the locks (it asks the disk, with a time limit).
    pub(crate) fn view_of(&self, snapshot: Snapshot, settings: &Settings) -> JobView {
        let found = self.whereabouts(&snapshot.job);
        Self::view_with(snapshot, settings, &found)
    }

    /// The row of a job whose folders were found as `found`.
    fn view_with(snapshot: Snapshot, settings: &Settings, found: &Whereabouts) -> JobView {
        let Snapshot { job, running, looking, must_ask, progress, look, problem, waiting_for, confirmed, rev } = snapshot;
        let waiting_since = job.waiting_since;
        // A look made with another setup says nothing about this one.
        let look = look.filter(|look| look.setup == job.setup);
        // A folder that doesn't answer in time (a share whose server is away) counts as away.
        let source_online = found.source.is_there();
        let destination_online = found.destination.is_there();
        // A folder that's there, but on another drive than at the last sync, counts as
        // offline: its row says why, and a sync still shows the list with that warning.
        let moved = found.both_there().then(|| found.moved(&job)).flatten();
        let paused = job.paused || settings.paused_all;
        // A run nobody looks at is refused while the job must ask, even when its record
        // couldn't be saved to say so: the row, the tray and Sync all go by the same rule.
        let runs_by_itself = job.runs_by_itself() && !must_ask;

        // Files a look couldn't compare yet may differ: never "In sync" while there are any.
        let work = look.as_ref().filter(|look| look.items > 0 || look.uncompared > 0);
        // A list the person confirmed no longer waits for an OK, only for another sync to end:
        // its changes are queued (and the row says what it waits for).
        let confirmed = confirmed && !running;
        let waiting = match (work, &job.held) {
            _ if confirmed => None,
            (Some(look), held) if !runs_by_itself || held.is_some() => Some(Waiting {
                items: look.items,
                bytes: look.bytes,
                removals: look.removals,
                replacements: look.replacements,
                conflicts: look.conflicts.len() as u64,
                held: held.clone(),
                uncompared: look.uncompared,
                since: waiting_since.unwrap_or(look.at),
                counted: true,
            }),
            (None, Some(held)) => Some(Waiting {
                items: 0,
                bytes: 0,
                removals: Default::default(),
                replacements: Default::default(),
                conflicts: look.as_ref().map_or(0, |l| l.conflicts.len() as u64),
                held: Some(held.clone()),
                uncompared: 0,
                since: waiting_since.or(job.last_scheduled).unwrap_or_else(Utc::now),
                counted: look.is_some(),
            }),
            // Not looked at since Charon started (or since a folder came back), but changes
            // waited for an OK when it last looked: they still wait until a look says
            // otherwise, never "In sync".
            (None, None) if look.is_none() && !runs_by_itself => waiting_since.map(|since| Waiting {
                items: 0,
                bytes: 0,
                removals: Default::default(),
                replacements: Default::default(),
                conflicts: 0,
                held: None,
                uncompared: 0,
                since,
                counted: false,
            }),
            _ => None,
        };
        let pending = match work {
            Some(look) if waiting.is_none() => Some(Pending { items: look.items, bytes: look.bytes, uncompared: look.uncompared, checked_at: look.at }),
            _ => None,
        };
        let conflicts = look.as_ref().map(|look| look.conflicts.clone()).unwrap_or_default();
        let unreadable = look.as_ref().map(|look| look.unreadable.clone()).unwrap_or_default();
        let unreadable_count = look.as_ref().map_or(0, |look| look.unreadable_count);
        let not_synced = look.as_ref().map(|look| look.not_synced.clone()).unwrap_or_default();
        let not_synced_count = look.as_ref().map_or(0, |look| look.not_synced_count);

        let failed_run = job.last_run.as_ref().is_some_and(|run| !run.is_clean() && !run.cancelled);
        let status = if running {
            JobStatus::Syncing
        } else if !source_online || !destination_online || moved.is_some() {
            JobStatus::Offline
        } else if paused {
            JobStatus::Paused
        } else if !conflicts.is_empty() {
            JobStatus::Conflict
        } else if problem.is_some() || failed_run {
            JobStatus::Error
        } else if waiting.is_some() {
            JobStatus::Review
        } else if job.last_synced.is_none() && !confirmed {
            JobStatus::New
        } else if pending.is_some() {
            JobStatus::Pending
        } else if look.is_none() {
            // No look since Charon started, or since a folder came back: "In sync" would be
            // a guess.
            JobStatus::Looking
        } else {
            JobStatus::InSync
        };

        let problem = match (status, &moved) {
            (JobStatus::Offline, _) if found.source == Presence::Silent => Some(SOURCE_SILENT.into()),
            (JobStatus::Offline, _) if !source_online => Some("The source folder isn't reachable.".into()),
            (JobStatus::Offline, Some((side, before, now))) => Some(format!(
                "The {} folder is now on {}, but at the last sync it was on {}. If that drive isn't connected, this is a leftover folder: connect the drive, or sync to look at the list if the drive really changed.",
                side.word(),
                now.describe(),
                before.describe()
            )),
            (JobStatus::Offline, None) if found.destination == Presence::Silent => Some(DESTINATION_SILENT.into()),
            (JobStatus::Offline, None) => Some("The destination isn't reachable. Is the drive connected?".into()),
            // A failed run always says what went wrong, naming the items that failed.
            _ => problem.or_else(|| job.last_run.as_ref().and_then(RunSummary::problem)),
        };
        let (source_volume, destination_volume) = match (look.as_ref().and_then(|l| l.volumes.clone()), &job.drives) {
            (Some((source, destination)), _) => (Some(source), Some(destination)),
            (None, Some(drives)) => (Some(VolumeLabel::of(&drives.source)), Some(VolumeLabel::of(&drives.destination))),
            (None, None) => (None, None),
        };
        let next_run = (!paused).then(|| Schedule::next_run(&job)).flatten();
        // Something is left to carry, or the folders couldn't be looked at.
        let unfinished = pending.is_some() || !source_online || !destination_online || moved.is_some() || failed_run;
        let stale = Self::stale(&job, waiting.as_ref(), unfinished, Utc::now());

        JobView {
            rev,
            status,
            runs_by_itself,
            looking: looking && !running,
            waiting,
            pending,
            progress: if running { progress } else { None },
            conflicts,
            unreadable,
            unreadable_count,
            not_synced,
            not_synced_count,
            problem,
            source_online,
            destination_online,
            source_volume,
            destination_volume,
            next_run,
            stale,
            waiting_for: if running { None } else { waiting_for },
            confirmed,
            job,
        }
    }

    /// When the job last synced, as its row shows it at `now`: the date Charon keeps, unless
    /// Charon found that date later than now (`dated_ahead`, see `note_syncs_dated_ahead`).
    /// The sync was made before Charon found it so, and the row then shows it as made then,
    /// if that's earlier (see `FoundAhead::latest`), even once the clock is past the date.
    pub(crate) fn last_synced(job: &Job, dated_ahead: Option<&FoundAhead>, now: Moment) -> Option<DateTime<Utc>> {
        let stored = job.last_synced?;
        Some(dated_ahead.filter(|ahead| ahead.at == stored).map_or(stored, |ahead| ahead.latest(now)))
    }

    /// A line for a job that hasn't crossed for longer than it should have: changes that
    /// waited more than a day, or a job with something to carry (or a folder out of reach)
    /// that hasn't synced for days. A job whose last look found nothing to do is fine.
    pub(crate) fn stale(job: &Job, waiting: Option<&Waiting>, unfinished: bool, now: DateTime<Utc>) -> Option<String> {
        if let Some(waiting) = waiting.filter(|w| (w.items > 0 || w.conflicts > 0) && now - w.since > Duration::hours(24)) {
            return Some(format!("Changes have waited for your OK since {}", Self::when(waiting.since, now)));
        }
        if waiting.is_none() && !unfinished {
            return None;
        }
        let allowed = match job.setup.trigger {
            Trigger::Interval { minutes } => (Duration::minutes(minutes as i64) * 2).max(Duration::days(2)),
            Trigger::Daily { .. } => Duration::days(2),
            // A backup drive left in a drawer, a folder nobody touches: a week says enough.
            Trigger::DriveConnected | Trigger::OnChange { .. } => Duration::days(7),
            Trigger::Manual => return None,
        };
        let since = job.last_synced?;
        (now - since > allowed).then(|| format!("Not synced for {} days", (now - since).num_days()))
    }

    /// "Monday" within the last week, "12 Oct" before that.
    fn when(time: DateTime<Utc>, now: DateTime<Utc>) -> String {
        let local = time.with_timezone(&Local);
        if now - time < Duration::days(6) {
            local.format("%A").to_string()
        } else {
            format!("{} {}", local.day(), local.format("%b"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConflictRule, JobSetup, SyncMode};

    fn job(trigger: Trigger) -> Job {
        Job::new(JobSetup {
            name: "t".into(),
            source: "/a".into(),
            destination: "/b".into(),
            mode: SyncMode::TwoWay,
            conflict: ConflictRule::Ask,
            trigger,
            excludes: vec![],
            safe_delete: true,
            full_automation: false,
        })
    }

    #[test]
    fn a_daily_job_not_synced_for_days_says_so() {
        let now = Utc::now();
        let mut daily = job(Trigger::Daily { hour: 3, minute: 0 });
        daily.last_synced = Some(now - Duration::days(3));
        // The drive was away: it says so.
        assert_eq!(Engine::stale(&daily, None, true, now).as_deref(), Some("Not synced for 3 days"));
        // The last look found nothing to do: nothing to say.
        assert!(Engine::stale(&daily, None, false, now).is_none());
        daily.last_synced = Some(now - Duration::hours(30));
        assert!(Engine::stale(&daily, None, true, now).is_none());
        let mut manual = job(Trigger::Manual);
        manual.last_synced = Some(now - Duration::days(30));
        assert!(Engine::stale(&manual, None, true, now).is_none());
    }
}
