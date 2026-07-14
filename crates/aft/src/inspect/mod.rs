pub mod cache;
pub(crate) mod diagnostics_category;
#[doc(hidden)]
pub use diagnostics_category::run_scoped_diagnostics_with_deadline_for_test;
pub mod dispatch;
mod entry_points;
mod frameworks;
pub mod freshness;
mod generated;
pub(crate) use generated::is_generated_file;
pub mod job;
mod manager;
pub mod oxc_engine;
pub mod scanners;
pub mod tier2_scheduler;

pub use cache::{ContributionRecord, InspectCache, InspectCacheError};
pub use dispatch::{DispatchHandles, InspectWorker};
pub(crate) use entry_points::resolve_entry_points;
pub use freshness::{contribution_is_fresh, verify_contribution_file, ContributionFreshness};
pub use job::{
    CallgraphExport, CallgraphOutboundCall, CallgraphSnapshot, FileContribution, InspectCategory,
    InspectJob, InspectResult, InspectScanSuccess, InspectSnapshot, InspectTier, JobKey,
    JobOutcome, JobScope, JobStatus, WorkerCtx,
};
pub use manager::{
    DirectTier2RunOutcome, InspectManager, Tier2RunSubmission, Tier2RunSubmissionError,
};
pub use tier2_scheduler::{Tier2RefreshScheduler, Tier2TriggerReason};
