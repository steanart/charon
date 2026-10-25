//! System notifications: only when something needs the person, or a big sync is done.
//! Small routine syncs stay silent.

use crate::state::AppState;
use charon_core::{human_bytes, plural, Finished, JobId, RunSummary, WaitingJob};
use std::collections::HashMap;
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

/// A sync this big is worth a "finished" notification.
const BIG_SYNC_BYTES: u64 = 1_000_000_000;
const BIG_SYNC_FILES: u64 = 1_000;

pub struct Notifier;

/// The last problem told about each job. A job that runs by itself and fails the same way
/// every hour is told about once, until a run of it goes well again.
#[derive(Default)]
pub struct Repeats {
    told: Mutex<HashMap<JobId, String>>,
}

impl Repeats {
    /// Whether `problem` (title and text) about `job` is news. `None`: the run went well, so
    /// the next problem is news again.
    fn is_news(&self, job: &JobId, problem: Option<&(String, String)>) -> bool {
        let mut told = self.told.lock().unwrap_or_else(|e| e.into_inner());
        match problem {
            Some((title, body)) => {
                let said = format!("{title}\n{body}");
                told.insert(job.clone(), said.clone()).as_ref() != Some(&said)
            }
            None => {
                told.remove(job);
                true
            }
        }
    }
}

impl Notifier {
    /// Tells the person about a finished run, if it's worth it, and not about the same
    /// problem again and again.
    pub fn announce(app: &AppHandle, finished: &Finished) {
        let message = Self::finished(finished);
        let run = &finished.summary;
        if !run.cancelled {
            let failed = run.error.is_some() || run.failed > 0 || run.left_out > 0;
            let news = app.try_state::<Repeats>().is_none_or(|repeats| repeats.is_news(&finished.id, message.as_ref().filter(|_| failed)));
            if !news {
                log::info!("didn't tell again about {}: {}", finished.name, run.describe());
                return;
            }
        }
        if let Some((title, body)) = message {
            Self::show(app, &title, &body);
        }
    }

    /// Tells the person that changes wait for their OK (one notification for all jobs due
    /// a mention; the engine decides how often).
    pub fn waiting(app: &AppHandle, jobs: &[WaitingJob]) {
        if let Some((title, body)) = Self::waiting_message(jobs) {
            Self::show(app, &title, &body);
        }
    }

    /// Tells the person that a run of a job with full automation was held back.
    pub fn held(app: &AppHandle, name: &str, reason: &str) {
        let title = format!("“{name}” was held back for your OK");
        let body = format!("{reason} Nothing was changed. Open Charon to look at the list.");
        Self::show(app, &title, &body);
    }

    fn show(app: &AppHandle, title: &str, body: &str) {
        let enabled = app.try_state::<AppState>().is_some_and(|s| s.engine.settings().notifications);
        if !enabled {
            return;
        }
        if let Err(e) = app.notification().builder().title(title).body(body).show() {
            log::warn!("couldn't show a notification: {e}");
        }
    }

    /// The title and text for a run, or `None` if it isn't worth interrupting for.
    fn finished(finished: &Finished) -> Option<(String, String)> {
        let name = &finished.name;
        let run = &finished.summary;
        if run.cancelled {
            return None;
        }
        if let Some(error) = &run.error {
            return Some((format!("{name} didn't sync"), error.clone()));
        }
        if run.failed > 0 {
            return Some((
                format!("{name}: {} couldn't be copied", plural(run.failed, "file")),
                "Open Charon to see which ones in the job's log.".into(),
            ));
        }
        // Said once: the looks after the run list the file, so the next runs don't find it again.
        if run.found_unreadable > 0 {
            let body = if run.found_unreadable == 1 {
                "Another program has it open, or Charon isn't allowed to read it, so it isn't synced. Open Charon to see which one, and to leave it out or let Charon read it."
            } else {
                "Other programs have them open, or Charon isn't allowed to read them, so they aren't synced. Open Charon to see which ones, and to leave them out or let Charon read them."
            };
            return Some((format!("{name}: {} couldn't be read", plural(run.found_unreadable, "file")), body.into()));
        }
        if run.left_out > 0 {
            let what = if run.left_out == 1 { "it" } else { "them" };
            return Some((
                format!("{name}: {} left out", plural(run.left_out, "item")),
                format!("A drive couldn't hold {what} (a name, a path or a size it refused, or a symbolic link). Open Charon to see which ones and why."),
            ));
        }
        if run.conflicts > 0 {
            return Some((
                format!("{name}: {} to resolve", plural(run.conflicts, "conflict")),
                "A file changed on both sides. Open Charon to choose which version to keep.".into(),
            ));
        }
        if run.settled > 0 && !run.approved {
            return Some((
                format!("{name}: {} settled by Newest wins", plural(run.settled, "conflict")),
                "The version saved last was kept on both sides; the other one is in .charon-trash. The job's log lists them.".into(),
            ));
        }
        if run.bytes >= BIG_SYNC_BYTES || run.copied >= BIG_SYNC_FILES {
            return Some(Self::big_sync(name, run));
        }
        None
    }

    /// The title and text for a big sync: "in sync" only when it left nothing for a later
    /// sync. A drive's owner waits for this one before ejecting it.
    fn big_sync(name: &str, run: &RunSummary) -> (String, String) {
        let mut text = format!("Copied {} ({}).", plural(run.copied, "file"), human_bytes(run.bytes));
        if run.skipped == 0 && run.waited == 0 && run.left_for_later == 0 {
            return (format!("{name} is in sync"), text);
        }
        if run.skipped > 0 {
            let (they, wait) = if run.skipped == 1 { ("it was left as it was", "waits") } else { ("they were left as they were", "wait") };
            text.push_str(&format!(" {} changed while Charon synced, so {they} and {wait} for the next sync.", plural(run.skipped, "item")));
        }
        if run.waited > 0 {
            let (wait, its) = if run.waited == 1 { ("waits", "its") } else { ("wait", "their") };
            text.push_str(&format!(" {} {wait} for the next sync with the rest of {its} app library or database.", plural(run.waited, "item")));
        }
        if run.left_for_later > 0 {
            let (were, items) = if run.left_for_later == 1 { ("was", "the item") } else { ("were", "the items") };
            text.push_str(&format!(
                " {} {were} left for a later sync: {items} went missing only a little while ago on a drive that may show changes late.",
                plural(run.left_for_later, "removal")
            ));
        }
        (format!("{name} isn't fully in sync yet"), text)
    }

    fn waiting_message(jobs: &[WaitingJob]) -> Option<(String, String)> {
        match jobs {
            [] => None,
            [job] => {
                let what = Self::what_waits(job.items, job.removals, job.conflicts, job.uncompared);
                // Only files to compare: the list may well have nothing to confirm.
                let to_compare = job.only_to_compare();
                let title = if job.items == 0 && job.conflicts > 0 {
                    format!("“{}” has {} to settle", job.name, plural(job.conflicts, "conflict"))
                } else if to_compare {
                    format!("“{}” has {} to compare", job.name, plural(job.uncompared, "file"))
                } else {
                    format!("“{}” waits for your OK", job.name)
                };
                let next = if job.drive_connected {
                    "Nothing has been copied yet: look at the list before you eject the drive."
                } else if to_compare {
                    "Open Charon to look at the list."
                } else {
                    "Open Charon to look at the list and confirm it."
                };
                Some((title, format!("{what} {next}")))
            }
            jobs => {
                let names: Vec<String> = jobs.iter().map(|job| format!("“{}”", job.name)).collect();
                let (last, rest) = names.split_last()?;
                let total = |f: fn(&WaitingJob) -> u64| jobs.iter().map(f).sum::<u64>();
                let what = Self::what_waits(total(|j| j.items), total(|j| j.removals), total(|j| j.conflicts), total(|j| j.uncompared));
                // Jobs that only have files to compare may well have nothing to confirm.
                let to_compare = jobs.iter().filter(|job| job.only_to_compare()).count() as u64;
                let to_confirm = jobs.len() as u64 - to_compare;
                let wait = |n: u64| format!("{} wait{} for your OK", plural(n, "job"), if n == 1 { "s" } else { "" });
                let title = match (to_confirm, to_compare) {
                    (n, 0) => wait(n),
                    (0, n) => format!("{} have files to compare", plural(n, "job")),
                    (n, m) => format!("{}, {} {} files to compare", wait(n), charon_core::human_count(m), if m == 1 { "has" } else { "have" }),
                };
                let next = if to_confirm == 0 { "Open Charon to look at them." } else { "Open Charon to look at them and confirm." };
                Some((title, format!("{} and {last}. {what} {next}", rest.join(", "))))
            }
        }
    }

    /// "23 changes are ready to sync, 3 of them removals, and 1 file changed on both sides.
    /// 1 file isn't compared yet: its copies on the two sides may differ."
    fn what_waits(items: u64, removals: u64, conflicts: u64, uncompared: u64) -> String {
        let mut text = match items {
            0 => String::new(),
            1 => "1 change is ready to sync".into(),
            n => format!("{} are ready to sync", plural(n, "change")),
        };
        match removals {
            0 => {}
            1 if items == 1 => text.push_str(": a removal"),
            1 => text.push_str(", 1 of them a removal"),
            n => text.push_str(&format!(", {} of them removals", charon_core::human_count(n))),
        }
        if conflicts > 0 {
            let both = if conflicts == 1 { "1 file changed on both sides".to_string() } else { format!("{} files changed on both sides", charon_core::human_count(conflicts)) };
            if text.is_empty() {
                text = format!("{both}: you choose which version to keep");
            } else {
                text.push_str(&format!(", and {both}"));
            }
        }
        let mut sentences = Vec::new();
        if !text.is_empty() {
            text.push('.');
            sentences.push(text);
        }
        match uncompared {
            0 => {}
            1 => sentences.push("1 file isn't compared yet: its copies on the two sides may differ.".into()),
            n => sentences.push(format!("{} files aren't compared yet: their copies on the two sides may differ.", charon_core::human_count(n))),
        }
        if sentences.is_empty() {
            sentences.push("A sync was held back.".into());
        }
        sentences.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run of the job “Documents”, as the engine records it, with `fields` set.
    fn documents(fields: serde_json::Value) -> Finished {
        let mut run = serde_json::json!({
            "startedAt": "2026-10-20T08:00:00Z",
            "finishedAt": "2026-10-20T08:01:00Z",
            "copied": 0,
            "deleted": 0,
            "bytes": 0,
            "conflicts": 0,
            "failed": 0,
            "cancelled": false,
            "error": null,
        });
        if let (Some(run), Some(fields)) = (run.as_object_mut(), fields.as_object()) {
            run.extend(fields.clone());
        }
        Finished { id: "documents".into(), name: "Documents".into(), summary: serde_json::from_value(run).unwrap() }
    }

    #[test]
    fn a_file_a_run_found_it_cannot_read_is_mentioned_once() {
        let (title, body) = Notifier::finished(&documents(serde_json::json!({ "unreadable": 1, "foundUnreadable": 1 }))).unwrap();
        assert_eq!(title, "Documents: 1 file couldn't be read");
        assert_eq!(
            body,
            "Another program has it open, or Charon isn't allowed to read it, so it isn't synced. Open Charon to see which one, and to leave it out or let Charon read it."
        );
        let (title, _) = Notifier::finished(&documents(serde_json::json!({ "unreadable": 3, "foundUnreadable": 2 }))).unwrap();
        assert_eq!(title, "Documents: 2 files couldn't be read");
        // Listed before this run: nothing new to tell.
        assert!(Notifier::finished(&documents(serde_json::json!({ "unreadable": 1 }))).is_none());
    }

    #[test]
    fn a_big_sync_is_only_in_sync_when_it_left_nothing_for_a_later_sync() {
        let big = |fields: serde_json::Value| {
            let mut run = serde_json::json!({ "copied": 1500, "bytes": 12_000_000_000u64 });
            run.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
            Notifier::finished(&documents(run)).unwrap()
        };
        let (title, body) = big(serde_json::json!({}));
        assert_eq!((title.as_str(), body.as_str()), ("Documents is in sync", "Copied 1,500 files (12.0 GB)."));
        // Files the person's apps changed while it ran are not on the drive yet.
        let (title, body) = big(serde_json::json!({ "skipped": 40 }));
        assert_eq!(title, "Documents isn't fully in sync yet");
        assert_eq!(body, "Copied 1,500 files (12.0 GB). 40 items changed while Charon synced, so they were left as they were and wait for the next sync.");
        let (_, body) = big(serde_json::json!({ "skipped": 1 }));
        assert_eq!(body, "Copied 1,500 files (12.0 GB). 1 item changed while Charon synced, so it was left as it was and waits for the next sync.");
        let (title, body) = big(serde_json::json!({ "leftForLater": 3 }));
        assert_eq!(title, "Documents isn't fully in sync yet");
        assert_eq!(
            body,
            "Copied 1,500 files (12.0 GB). 3 removals were left for a later sync: the items went missing only a little while ago on a drive that may show changes late."
        );
        // Items that only wait for the rest of their app library aren't on the drive yet either.
        let (title, body) = big(serde_json::json!({ "skipped": 1, "waited": 5 }));
        assert_eq!(title, "Documents isn't fully in sync yet");
        assert_eq!(
            body,
            "Copied 1,500 files (12.0 GB). 1 item changed while Charon synced, so it was left as it was and waits for the next sync. \
             5 items wait for the next sync with the rest of their app library or database."
        );
        let (title, body) = big(serde_json::json!({ "waited": 1 }));
        assert_eq!(title, "Documents isn't fully in sync yet");
        assert_eq!(body, "Copied 1,500 files (12.0 GB). 1 item waits for the next sync with the rest of its app library or database.");
        let (_, body) = big(serde_json::json!({ "skipped": 1, "leftForLater": 1 }));
        assert_eq!(
            body,
            "Copied 1,500 files (12.0 GB). 1 item changed while Charon synced, so it was left as it was and waits for the next sync. \
             1 removal was left for a later sync: the item went missing only a little while ago on a drive that may show changes late."
        );
    }

    fn waiting(name: &str, items: u64, removals: u64, conflicts: u64) -> WaitingJob {
        WaitingJob { id: name.into(), name: name.into(), items, removals, conflicts, uncompared: 0, held: false, drive_connected: false }
    }

    #[test]
    fn the_same_problem_is_told_once_until_a_run_goes_well() {
        let repeats = Repeats::default();
        let job = "job".to_string();
        let failed = ("Photos didn't sync".to_string(), "The drive is full.".to_string());
        assert!(repeats.is_news(&job, Some(&failed)));
        assert!(!repeats.is_news(&job, Some(&failed)));
        assert!(repeats.is_news(&"other".to_string(), Some(&failed)), "each job on its own");
        let other = ("Photos didn't sync".to_string(), "A folder can't be read.".to_string());
        assert!(repeats.is_news(&job, Some(&other)), "another problem is news");
        assert!(repeats.is_news(&job, None));
        assert!(repeats.is_news(&job, Some(&other)), "after a good run, the problem is news again");
    }

    #[test]
    fn waiting_messages_read_well() {
        let (title, body) = Notifier::waiting_message(&[waiting("Photos", 23, 3, 0)]).unwrap();
        assert_eq!(title, "“Photos” waits for your OK");
        assert_eq!(body, "23 changes are ready to sync, 3 of them removals. Open Charon to look at the list and confirm it.");
        let (_, body) = Notifier::waiting_message(&[waiting("Notes", 1, 0, 0)]).unwrap();
        assert_eq!(body, "1 change is ready to sync. Open Charon to look at the list and confirm it.");
        let (title, body) = Notifier::waiting_message(&[waiting("Notes", 0, 0, 2)]).unwrap();
        assert_eq!(title, "“Notes” has 2 conflicts to settle");
        assert_eq!(body, "2 files changed on both sides: you choose which version to keep. Open Charon to look at the list and confirm it.");
        let mut drive = waiting("Backup", 4, 0, 0);
        drive.drive_connected = true;
        let (_, body) = Notifier::waiting_message(&[drive]).unwrap();
        assert_eq!(body, "4 changes are ready to sync. Nothing has been copied yet: look at the list before you eject the drive.");
        let (title, body) = Notifier::waiting_message(&[waiting("A", 1, 0, 0), waiting("B", 2, 0, 1), waiting("C", 3, 1, 0)]).unwrap();
        assert_eq!(title, "3 jobs wait for your OK");
        assert_eq!(
            body,
            "“A”, “B” and “C”. 6 changes are ready to sync, 1 of them a removal, and 1 file changed on both sides. Open Charon to look at them and confirm."
        );
        assert!(Notifier::waiting_message(&[]).is_none());
    }

    /// A file a background look couldn't compare is no change ready to sync: the list may
    /// find both copies identical, and then has nothing to confirm.
    #[test]
    fn files_not_compared_yet_are_no_changes_ready_to_sync() {
        let video = WaitingJob { uncompared: 1, ..waiting("Videos", 0, 0, 0) };
        let (title, body) = Notifier::waiting_message(std::slice::from_ref(&video)).unwrap();
        assert_eq!(title, "“Videos” has 1 file to compare");
        assert_eq!(body, "1 file isn't compared yet: its copies on the two sides may differ. Open Charon to look at the list.");
        let (title, body) = Notifier::waiting_message(&[WaitingJob { uncompared: 2, ..waiting("Photos", 23, 3, 0) }]).unwrap();
        assert_eq!(title, "“Photos” waits for your OK");
        assert_eq!(
            body,
            "23 changes are ready to sync, 3 of them removals. 2 files aren't compared yet: their copies on the two sides may differ. \
             Open Charon to look at the list and confirm it."
        );
        let music = WaitingJob { uncompared: 1, ..waiting("Music", 0, 0, 0) };
        let (title, body) = Notifier::waiting_message(&[video.clone(), music.clone()]).unwrap();
        assert_eq!(title, "2 jobs have files to compare");
        assert_eq!(body, "“Videos” and “Music”. 2 files aren't compared yet: their copies on the two sides may differ. Open Charon to look at them.");
        let (title, body) = Notifier::waiting_message(&[waiting("Photos", 23, 3, 0), video.clone(), music]).unwrap();
        assert_eq!(title, "1 job waits for your OK, 2 have files to compare");
        assert_eq!(
            body,
            "“Photos”, “Videos” and “Music”. 23 changes are ready to sync, 3 of them removals. 2 files aren't compared yet: their copies on the two sides may differ. \
             Open Charon to look at them and confirm."
        );
        let (title, _) = Notifier::waiting_message(&[waiting("Photos", 23, 3, 0), waiting("Notes", 0, 0, 2), video.clone()]).unwrap();
        assert_eq!(title, "2 jobs wait for your OK, 1 has files to compare");

        // A job held back for a look waits for an OK, whatever its look found.
        let held = WaitingJob { held: true, ..video };
        let (title, body) = Notifier::waiting_message(std::slice::from_ref(&held)).unwrap();
        assert_eq!(title, "“Videos” waits for your OK");
        assert_eq!(body, "1 file isn't compared yet: its copies on the two sides may differ. Open Charon to look at the list and confirm it.");
        let (title, body) = Notifier::waiting_message(&[held, WaitingJob { uncompared: 1, ..waiting("Music", 0, 0, 0) }]).unwrap();
        assert_eq!(title, "1 job waits for your OK, 1 has files to compare");
        assert!(body.ends_with("Open Charon to look at them and confirm."), "{body}");
    }
}
