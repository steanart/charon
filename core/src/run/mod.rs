//! Carrying out a plan.
//!
//! * `executor.rs` – `Executor`: checks each step against the disk, runs it, reports progress
//! * `copier.rs`   – `Copier`: copies one file or link safely (partial file, then rename)
//! * `rename.rs`   – renaming without ever replacing what's at the new name, and swapping two names
//! * `identity.rs` – `Identity`: which file an item is, to tell it from another one saved under its name
//! * `trash.rs`    – `Trash`: safe deletes into `.charon-trash`, what each run put there, and its clean-up
//! * `room.rs`     – `Room`: how much room a drive has left, so a sync never fills one up, and
//!   `KeptCopies`: the room the replaced versions it keeps take there
//! * `durable.rs`  – making a run's changes durable on the drive before they're recorded
//! * `marker.rs`   – `Marker`: tells Charon a folder is the one it synced with
//! * `busy.rs`     – `Claim`: tells other syncs (and other Charons) a sync is at work in a folder
//! * `meter.rs`    – `ProgressMeter`: counts what's done for the progress bar
//! * `names.rs`    – fitting new names (conflict copies, numbered trash items) to a drive's limits

mod busy;
mod copier;
pub(crate) mod durable;
mod executor;
pub(crate) mod identity;
mod marker;
mod meter;
mod names;
pub(crate) mod rename;
mod room;
mod trash;

pub use busy::{BusyNote, BusyWatch, Claim, BUSY_EXTENSION};
#[cfg(test)]
pub(crate) use busy::QUIET_OVERDUE;
pub use copier::{Copier, Written};
pub use executor::{Care, Executor, Outcome, Refused, Shores, Spellings, Unread};
pub use marker::{Found, Marker, Markers, Origin, PROBE_EXTENSION};
pub use meter::ProgressMeter;
pub use room::{KeptCopies, Room};
pub use trash::{Expired, Trash, TrashLook};
