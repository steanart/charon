//! Deciding what has to cross, and which way.
//!
//! * `action.rs`  – one step (`Action`), a `Note`, a whole `Plan`, the order its steps run in
//!   (`Step`) and its `PlanSummary`
//! * `ground.rs`  – the places a plan must not touch, shared by both planners
//! * `one_way.rs` – `OneWayPlanner` for Mirror and Backup
//! * `two_way.rs` – `TwoWayPlanner`, which compares both sides with the last synced state
//! * `baseline.rs` – `Baseline`: that last synced state, as each side had it
//! * `guard.rs`   – `Guard`: plans that must not run, or need a person's look first
//! * `resolve.rs` – `Resolution`: settling one conflict the way the person chose
//! * `units.rs`   – `Unit`: app libraries and databases, whose files only work together

mod action;
mod baseline;
mod ground;
mod guard;
mod one_way;
mod resolve;
mod two_way;
mod units;

pub use action::{Action, Clearing, Note, NoteKind, Plan, PlanSummary, Side, Step};
pub use baseline::{Baseline, Pair, PutBack, Settled};
pub use guard::{Footprints, Guard, LongCount, Recent};
pub use one_way::OneWayPlanner;
pub use resolve::Resolution;
pub use two_way::TwoWayPlanner;
pub use units::Unit;

use crate::model::JobSetup;
use crate::scan::Snapshot;
use std::collections::BTreeSet;

/// Picks the planner for the job's mode and plans the run. `record`: two-way, how both
/// sides looked at the last sync (the baseline); Mirror and Backup, the copies whose dates
/// didn't stick on the destination (see `store::Landings`). `different`: two-way pairs
/// that look alike but whose contents were found to differ.
pub fn plan_for(setup: &JobSetup, source: &Snapshot, destination: &Snapshot, record: &Baseline, different: &BTreeSet<String>) -> Plan {
    match setup.mode {
        crate::model::SyncMode::Mirror => OneWayPlanner::mirror().knowing_landed(record.clone()).plan(source, destination),
        crate::model::SyncMode::Backup => OneWayPlanner::backup().knowing_landed(record.clone()).plan(source, destination),
        crate::model::SyncMode::TwoWay => TwoWayPlanner::new(setup.conflict).knowing_different(different.clone()).plan(source, destination, record),
    }
}
