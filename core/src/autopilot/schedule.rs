//! When a scheduled job runs next.
//!
//! Schedules count from the last run (or the job's creation), not from a timer, so a
//! computer that was asleep catches up as soon as it wakes. A time taken while the clock
//! was ahead (and set right since) doesn't count: how long ago it really was can't be told,
//! so the job catches up the same way rather than wait until the clock gets there again.

use crate::model::{Job, Trigger};
use chrono::{DateTime, Duration, Local, TimeZone, Utc};

pub struct Schedule;

impl Schedule {
    /// The next automatic run of an interval or daily job.
    pub fn next_run(job: &Job) -> Option<DateTime<Utc>> {
        Self::next_after(job, None, Utc::now())
    }

    /// Whether the job's schedule says it should run now.
    pub fn is_due(job: &Job, now: DateTime<Utc>) -> bool {
        Self::is_due_after(job, None, now)
    }

    /// `is_due`, also counting from `since` (when the schedule last came due, if that's
    /// later than the job's record says).
    pub fn is_due_after(job: &Job, since: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
        Self::next_after(job, since, now).is_some_and(|next| now >= next)
    }

    /// The next run as seen at `now`, counting from the latest of the job's record and
    /// `since` that isn't later than `now`. With nothing to count from, the job is due now.
    fn next_after(job: &Job, since: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let reference = job.schedule_reference(now).into_iter().chain(since.filter(|since| *since <= now)).max();
        match reference {
            Some(reference) => Self::next_from(job, reference),
            None => Self::next_from(job, now).map(|_| now),
        }
    }

    fn next_from(job: &Job, reference: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match job.setup.trigger {
            Trigger::Interval { minutes } => Some(reference + Duration::minutes(minutes as i64)),
            Trigger::Daily { hour, minute } => Self::next_daily(reference, hour, minute),
            _ => None,
        }
    }

    /// The first local `hour:minute` after `after`.
    fn next_daily(after: DateTime<Utc>, hour: u8, minute: u8) -> Option<DateTime<Utc>> {
        let after = after.with_timezone(&Local);
        let mut date = after.date_naive();
        for _ in 0..3 {
            let time = date.and_hms_opt(hour as u32, minute as u32, 0)?;
            // A time a daylight-saving change skips doesn't exist that day: it comes the next day.
            if let Some(candidate) = Local.from_local_datetime(&time).earliest() {
                if candidate > after {
                    return Some(candidate.with_timezone(&Utc));
                }
            }
            date = date.succ_opt()?;
        }
        None
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
            mode: SyncMode::Backup,
            conflict: ConflictRule::Ask,
            trigger,
            excludes: vec![],
            safe_delete: true,
            full_automation: false,
        })
    }

    #[test]
    fn interval_counts_from_creation() {
        let j = job(Trigger::Interval { minutes: 30 });
        assert!(!Schedule::is_due(&j, Utc::now()));
        assert!(Schedule::is_due(&j, Utc::now() + Duration::minutes(31)));
    }

    #[test]
    fn daily_is_within_a_day() {
        let j = job(Trigger::Daily { hour: 3, minute: 15 });
        let next = Schedule::next_run(&j).unwrap();
        assert!(next > Utc::now() && next <= Utc::now() + Duration::hours(25));
    }

    #[test]
    fn manual_never_runs_by_itself() {
        assert!(Schedule::next_run(&job(Trigger::Manual)).is_none());
    }

    #[test]
    fn a_time_taken_while_the_clock_was_ahead_never_holds_the_schedule_up() {
        let now = Utc::now();
        for trigger in [Trigger::Interval { minutes: 60 }, Trigger::Daily { hour: 3, minute: 15 }] {
            let mut j = job(trigger);
            j.created_at = now - Duration::days(30);
            // The schedule last came due while the clock was three days ahead; it has been
            // set right since.
            let ahead = now + Duration::days(3);
            j.last_scheduled = Some(ahead);
            assert!(Schedule::is_due_after(&j, Some(ahead), now), "{trigger:?} is due now, not in three days");
            assert!(Schedule::next_run(&j).is_some_and(|next| next <= Utc::now()));
            // Once it came due, the next time counts from then.
            j.last_scheduled = Some(now);
            assert!(!Schedule::is_due_after(&j, Some(now), now), "{trigger:?}");
            assert!(Schedule::is_due_after(&j, Some(now), now + Duration::hours(25)), "{trigger:?}");
        }
        // A job made while the clock was ahead has nothing else to count from.
        let mut j = job(Trigger::Interval { minutes: 60 });
        j.created_at = now + Duration::days(3);
        assert!(Schedule::is_due(&j, now));
    }
}
