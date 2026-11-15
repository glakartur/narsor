//! Which backend runs a task and when: the default backend, the limit of
//! concurrent tasks, automatic resume and slot release.

mod auto_resume;
mod backend_limit;
mod backends;
mod concurrency;
mod harness_usage;
mod slot_release;

pub use auto_resume::AutoResumePass;
pub(crate) use harness_usage::HarnessUsages;
pub use slot_release::SlotReleasePass;
