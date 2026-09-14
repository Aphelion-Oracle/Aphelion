//! Shared state for the HTTP handlers.

use std::sync::Arc;
use std::time::Instant;

use metrics_exporter_prometheus::PrometheusHandle;

use crate::chain::{ChainClient, RpcClient};
use crate::config::Config;
use crate::db::Repo;
use crate::engine::{Sweeper, Watch};
use crate::signer::NodeSigner;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub repo: Repo,
    pub chain: Arc<dyn ChainClient>,
    pub rpc: Arc<RpcClient>,
    pub signer: Arc<NodeSigner>,
    /// The same sweeper the upkeep loop runs, so `/v1/upkeep` reports the plan
    /// that would actually be submitted rather than a fresh one's guess at it.
    pub sweeper: Arc<Sweeper>,
    /// The same watcher the committee loop runs, so `/v1/duties` reports what
    /// the log and the gauges are reporting. `None` when the deployment has no
    /// `slashing_contract` configured, which the endpoint says rather than
    /// answering with an empty list -- "nothing outstanding" and "not looking"
    /// are different facts and only one of them is reassuring.
    pub watch: Option<Arc<Watch>>,
    pub metrics: Option<PrometheusHandle>,
    pub started_at: Instant,
}

impl AppState {
    pub fn uptime_seconds(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}
