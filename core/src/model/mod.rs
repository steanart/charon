//! The data Charon keeps and the shapes the window receives.
//!
//! * `job.rs`      – a sync job and its setup (mode, conflict rule, trigger, excludes)
//! * `settings.rs` – app-wide settings
//! * `view.rs`     – what the window shows: status, progress, pending changes, summaries
//! * `review.rs`   – the list of changes waiting for the person's OK, and the upgrade notice

mod job;
mod review;
mod settings;
mod view;

pub use job::*;
pub use review::*;
pub use settings::*;
pub use view::*;

#[cfg(test)]
pub(crate) use job::asking;
