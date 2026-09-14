//! The two loops that make up the node, and the upkeep beside them.
//!
//! `duty` is neither: it is what the node notices on an operator's behalf
//! about the contracts that can take their stake, and reports rather than acts
//! on. See [`duty`] for why that line is where it is.

pub mod aggregate;
pub mod beacon;
pub mod collector;
pub mod duty;
pub mod round;
pub mod status;
pub mod upkeep;

pub use aggregate::{aggregate, confidence_bps, Aggregated, AggregationParams};
pub use beacon::{decide as decide_beacon, Action as BeaconAction, OurPart, OwedReveal};
pub use collector::{run_retention, Collector};
pub use duty::{derive as derive_duties, Consequence, Duty, DutyKind, Snapshot, Standing, Watch};
pub use round::{RoundOutcome, RoundRunner};
pub use status::{
    assess as assess_status, ChainStatus, DutiesStatus, FeedStatus, Finding, Registration, Report,
    SourceStatus, Verdict,
};
pub use upkeep::{plan_sweep, Candidate, Excuse, SweepPlan, SweepReport, Sweeper};
