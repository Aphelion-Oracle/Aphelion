//! The one deployment every node process in a harness submits to.
//!
//! The consensus rules are not reimplemented here. This is [`MockChain`] --
//! the same in-memory aggregator the single-process simulation runs against,
//! which verifies signatures, enforces monotonic nonces and a staleness
//! window, captures weight at submission time, and takes the reputation-
//! weighted median with `aphelion_core::math::weighted_median` -- put behind an
//! HTTP socket so that several *processes* can reach one instance of it.
//!
//! That split is the whole point. A harness with its own notion of what the
//! aggregator does would test the harness. Everything that decides whether a
//! submission is accepted lives in one place, and this module only carries
//! requests to it.
//!
//! # The clock
//!
//! Ledger time is served as wall clock plus an offset the test controls,
//! rather than a frozen instant, because the node processes are real: they
//! sign observations timestamped by a real collector reading a real (fake)
//! exchange seconds ago, and a frozen ledger clock would reject all of it as
//! stale within a minute of the harness starting. The offset is what makes the
//! clock-skew guard testable across a process boundary.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use aphelion_core::{FeedId, Price, PriceMessage};
use aphelion_node::chain::{ChainClient, MockChain};
use aphelion_node::signer::SignedSubmission;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

/// A contract address the harness signs against.
///
/// Any valid `C...` strkey works -- nothing here deploys anything -- but it
/// must be *valid*, because the node decodes it to the 32 bytes that go into
/// the signing payload and refuses a bad checksum. These two are the native
/// XLM SAC on testnet and a second real address, used only for their shape.
pub const AGGREGATOR: &str = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC";
pub const REGISTRY: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

struct Inner {
    chain: Arc<MockChain>,
    aggregator_id: [u8; 32],
    /// Seconds added to wall clock when reporting ledger time.
    skew: AtomicI64,
    /// Requests refused outright, to simulate an RPC endpoint going away
    /// underneath a node that is otherwise healthy.
    offline: AtomicI64,
}

impl Inner {
    /// Ledger time as this deployment currently reports it.
    ///
    /// Pushed into the chain before every call so the staleness and
    /// future-timestamp checks are evaluated against the same clock the node
    /// was told about.
    fn tick(&self) -> u64 {
        let now = chrono::Utc::now().timestamp() + self.skew.load(Ordering::Relaxed);
        let now = now.max(0) as u64;
        self.chain.set_ledger_time(now);
        now
    }
}

/// A running deployment.
pub struct Deployment {
    inner: Arc<Inner>,
    addr: SocketAddr,
}

impl Deployment {
    /// Start serving. `quorum` is how many distinct nodes must submit before a
    /// round publishes; `nodes` are the public keys the registry knows, with
    /// the voting weight each carries.
    ///
    /// Registration is fixed at startup because that is what `MockChain`
    /// supports, and because a key the registry has never heard of is the
    /// interesting case: it exercises the path where a node process is running,
    /// healthy and signing correctly, and the chain still refuses it.
    pub async fn start(quorum: usize, nodes: &[(String, u32)]) -> std::io::Result<Self> {
        let mut chain =
            MockChain::new(chrono::Utc::now().timestamp().max(0) as u64).with_quorum(quorum.max(1));
        for (pubkey, weight) in nodes {
            chain = chain.with_registered(pubkey, *weight);
        }

        let inner = Arc::new(Inner {
            chain: Arc::new(chain),
            aggregator_id: aphelion_node::strkey::contract_id_bytes(AGGREGATOR)
                .expect("the built-in aggregator address is a valid strkey"),
            skew: AtomicI64::new(0),
            offline: AtomicI64::new(0),
        });

        let app = Router::new()
            .route("/invoke", post(invoke))
            .route("/control/skew", post(set_skew))
            .route("/control/offline", post(set_offline))
            .route("/control/state", get(state))
            .with_state(Arc::clone(&inner));

        // Port 0: the OS picks a free one, so several harnesses can run at
        // once without agreeing on a port in advance.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Ok(Self { inner, addr })
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Move this deployment's clock away from wall clock, in seconds.
    ///
    /// Nodes read their ledger time from here, so this is drift between the
    /// node's machine and the ledger as the node experiences it.
    pub fn set_skew(&self, seconds: i64) {
        self.inner.skew.store(seconds, Ordering::Relaxed);
    }

    /// Refuse every call, as an RPC endpoint that has gone away does.
    pub fn set_offline(&self, offline: bool) {
        self.inner.offline.store(offline as i64, Ordering::Relaxed);
    }

    /// The published price for a feed, if a round has closed.
    pub async fn published(&self, feed: &FeedId) -> Option<aphelion_node::chain::OnChainPrice> {
        self.inner.chain.latest_price(feed).await.ok().flatten()
    }

    /// Every submission the deployment has accepted, as `(pubkey, submission)`.
    pub fn accepted(&self) -> Vec<(String, SignedSubmission)> {
        self.inner.chain.submissions()
    }

    /// How many nodes have submitted into the currently open round.
    pub fn pending(&self, feed: &FeedId) -> usize {
        self.inner.chain.pending(feed)
    }
}

/// One `stellar contract invoke`, forwarded by the fake CLI.
#[derive(serde::Deserialize)]
struct Invoke {
    contract: String,
    func: String,
    #[serde(default)]
    args: std::collections::HashMap<String, String>,
}

/// What the real CLI would have written to the two streams.
///
/// The node parses stdout as JSON and scrapes stderr for a transaction hash,
/// so the fake has to produce both, and an error has to arrive as a non-zero
/// exit rather than as JSON -- that is how the node distinguishes "the chain
/// said no" from "the chain said null".
#[derive(serde::Serialize)]
struct CliOutput {
    stdout: String,
    stderr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl CliOutput {
    fn ok(stdout: impl Into<String>) -> Json<Self> {
        Json(Self {
            stdout: stdout.into(),
            stderr: String::new(),
            error: None,
        })
    }

    fn err(reason: impl Into<String>) -> Json<Self> {
        Json(Self {
            stdout: String::new(),
            stderr: String::new(),
            error: Some(reason.into()),
        })
    }
}

async fn invoke(State(inner): State<Arc<Inner>>, Json(req): Json<Invoke>) -> Json<CliOutput> {
    if inner.offline.load(Ordering::Relaxed) != 0 {
        // Shaped like a transport failure rather than a contract error,
        // because that is what a node sees when its endpoint disappears.
        return CliOutput::err("error: failed to connect to RPC endpoint");
    }
    let now = inner.tick();

    match req.func.as_str() {
        "ledger_time" => CliOutput::ok(now.to_string()),

        "submit_price" => submit(&inner, &req).await,

        "get_price" => {
            let Some(feed) = req.args.get("feed").and_then(|f| FeedId::new(f).ok()) else {
                return CliOutput::err("error: get_price needs a valid --feed");
            };
            match inner.chain.latest_price(&feed).await {
                Ok(Some(p)) => CliOutput::ok(
                    json!({
                        "price": p.price.raw().to_string(),
                        "timestamp": p.timestamp,
                        "num_nodes": p.num_nodes,
                        "confidence_bps": p.confidence_bps,
                        "round_id": p.round_id,
                    })
                    .to_string(),
                ),
                // A feed with no closed round yet is `null`, not an error --
                // the node treats those very differently.
                Ok(None) => CliOutput::ok("null"),
                Err(e) => CliOutput::err(e.to_string()),
            }
        }

        "get_node" => {
            let Some(pubkey) = req.args.get("pubkey") else {
                return CliOutput::err("error: get_node needs --pubkey");
            };
            match inner.chain.node_info(pubkey).await {
                Ok(Some(n)) => CliOutput::ok(
                    json!({
                        "stake": n.stake.to_string(),
                        "reputation": n.reputation,
                        "status": n.status,
                        "weight_bps": n.weight_bps,
                        "last_submission": n.last_submission,
                    })
                    .to_string(),
                ),
                Ok(None) => CliOutput::ok("null"),
                Err(e) => CliOutput::err(e.to_string()),
            }
        }

        "last_nonce" => {
            let (Some(pubkey), Some(feed)) = (
                req.args.get("pubkey"),
                req.args.get("feed").and_then(|f| FeedId::new(f).ok()),
            ) else {
                return CliOutput::err("error: last_nonce needs --pubkey and --feed");
            };
            match inner.chain.last_nonce(pubkey, &feed).await {
                Ok(n) => CliOutput::ok(n.to_string()),
                Err(e) => CliOutput::err(e.to_string()),
            }
        }

        other => CliOutput::err(format!(
            "error: unknown function `{other}` on contract {}",
            req.contract
        )),
    }
}

/// Rebuild the signed submission from its arguments and hand it to the chain.
///
/// The signature covers the aggregator id, which never travels over the CLI --
/// it is derived from the address the deployment was configured with, exactly
/// as the node derives it from the address in its config. If those two ever
/// disagreed, every signature would fail to verify, which is the loudest
/// possible way for that mistake to surface.
async fn submit(inner: &Arc<Inner>, req: &Invoke) -> Json<CliOutput> {
    let arg = |k: &str| req.args.get(k).cloned().unwrap_or_default();

    let Ok(feed) = FeedId::new(arg("feed")) else {
        return CliOutput::err("error: submit_price needs a valid --feed");
    };
    let Ok(raw) = arg("price").parse::<i128>() else {
        return CliOutput::err("error: --price must be a raw scaled integer");
    };
    let (Ok(timestamp), Ok(confidence_bps), Ok(nonce)) = (
        arg("timestamp").parse::<u64>(),
        arg("confidence_bps").parse::<u32>(),
        arg("nonce").parse::<u64>(),
    ) else {
        return CliOutput::err("error: --timestamp, --confidence_bps and --nonce must be integers");
    };
    let Ok(sig_bytes) = hex::decode(arg("signature")) else {
        return CliOutput::err("error: --signature is not hex");
    };
    let Ok(signature) = <[u8; 64]>::try_from(sig_bytes.as_slice()) else {
        return CliOutput::err("error: --signature is not 64 bytes");
    };

    let submission = SignedSubmission {
        message: PriceMessage {
            aggregator: inner.aggregator_id,
            feed,
            price: Price::from_raw(raw),
            timestamp,
            confidence_bps,
            nonce,
        },
        signature,
    };

    match inner.chain.submit_price(&arg("pubkey"), &submission).await {
        Ok(receipt) => Json(CliOutput {
            stdout: receipt.finalized_round.to_string(),
            // The node scrapes stderr for a 64-hex-character transaction hash.
            // Deriving it from the signature keeps it stable per submission.
            stderr: format!(
                "Transaction: {}\n",
                hex::encode(&submission.signature[..32])
            ),
            error: None,
        }),
        Err(e) => CliOutput::err(e.to_string()),
    }
}

async fn set_skew(State(inner): State<Arc<Inner>>, Json(v): Json<Value>) -> Json<Value> {
    inner.skew.store(
        v.get("seconds").and_then(Value::as_i64).unwrap_or(0),
        Ordering::Relaxed,
    );
    Json(json!({ "ok": true }))
}

async fn set_offline(State(inner): State<Arc<Inner>>, Json(v): Json<Value>) -> Json<Value> {
    inner.offline.store(
        v.get("offline").and_then(Value::as_bool).unwrap_or(false) as i64,
        Ordering::Relaxed,
    );
    Json(json!({ "ok": true }))
}

async fn state(State(inner): State<Arc<Inner>>) -> Json<Value> {
    Json(json!({
        "ledger_time": inner.tick(),
        "accepted": inner.chain.submission_count(),
    }))
}
