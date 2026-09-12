//! The two loops that make up the node, and the upkeep beside them.

pub mod aggregate;
pub mod collector;
pub mod round;
pub mod upkeep;

pub use aggregate::{aggregate, confidence_bps, Aggregated, AggregationParams};
pub use collector::{run_retention, Collector};
pub use round::{RoundOutcome, RoundRunner};
pub use upkeep::{plan_sweep, Candidate, Excuse, SweepPlan, SweepReport, Sweeper};
