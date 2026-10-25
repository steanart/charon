//! Charon core: everything that isn't user interface.
//!
//! Charon ferries files between two folders. A *job* names the two shores (source and
//! destination), how files cross (mirror, backup or two-way) and when (by hand, on change,
//! on a schedule or when a drive is connected).
//!
//! | module      | responsibility                                                         |
//! |-------------|------------------------------------------------------------------------|
//! | `model`     | the data Charon stores (jobs, settings) and the rows the window shows |
//! | `scan`      | reading a folder into a snapshot, honouring exclude patterns          |
//! | `plan`      | comparing snapshots and deciding what has to cross, and which way     |
//! | `run`       | carrying out a plan: copies, safe deletes, conflict copies, progress  |
//! | `store`     | the settings file, two-way baselines and per-job logs on disk         |
//! | `engine`    | the orchestration: jobs, reviews, runs, status, events                |
//! | `autopilot` | automatic triggers: folder watching, schedules, drives coming online  |

pub mod autopilot;
pub mod engine;
pub mod error;
pub mod model;
pub mod plan;
pub mod run;
pub mod scan;
pub mod store;

pub use autopilot::Autopilot;
pub use engine::{Approval, Emitter, Engine, EngineEvent, NotePage, StepPage, SyncStart};
pub use error::{Error, Result};
pub use model::*;
