//! Automations and queues of the catalogue (R1–R7).
//!
//! ```text
//! {catalog}/automations/{name}/automation.yaml   -- description, params, secrets
//! {catalog}/automations/{name}/check.sh          -- required
//! {catalog}/automations/{name}/in.sh             -- optional, runs on entry
//! {catalog}/automations/{name}/out.sh            -- optional, runs on exit
//! {catalog}/queues/{name}.yaml                   -- automations, mode, schedule
//! ```
//!
//! * [`catalog`] — reads and validates these files, with one problem per
//!   bad file, and reads a script's text when a run is claimed.
//!
//! Like the environment catalogue, nothing is cached: every load reads the
//! directories, so a change applies to the next run.
//!
//! The schedule, mode, task-creation and secret types are the observer
//! types, re-exported here so the catalogue speaks the same language as the
//! scheduler.

pub mod catalog;

pub use crate::observers::{CreateTaskEffect, ObserverMode, ScheduleConfig, SecretRef};
pub use catalog::{
    Automation, AutomationCatalog, AutomationCatalogError, AutomationCatalogLoad, AutomationScript,
    CatalogProblem, HookPhase, Queue, QueueTasks, AUTOMATION_FILE,
};
