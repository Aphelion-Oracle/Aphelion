//! Several real node processes against one deployment.
//!
//! Every other test in this repository runs the node's code in-process. This
//! one runs the shipped `aphelion-node` binary -- several copies of it, each
//! with its own Postgres database, its own signing key, its own HTTP port and
//! its own child-process calls to the chain -- and points all of them at a
//! single aggregator. What that buys, and nothing else provides, is the
//! failures that only exist between processes: two nodes racing for the same
//! round, a node whose endpoint disappears while its collector keeps working,
//! a node whose clock has drifted from the ledger's, and a node dying without
//! taking the network with it.
//!
//! # What is real and what is not
//!
//! Real: the node binary, the collector, Postgres and the migrations, the
//! round loop, the signing, the process boundary, the `stellar` subprocess
//! spawn, and the aggregator's acceptance rules (see [`deployment`]).
//!
//! Fixtures: the exchanges ([`exchange`]) and the transport to the chain
//! ([`deployment`], reached through a fake `stellar` binary). Both are
//! replaced at the same seams an operator can already use -- the
//! `APHELION_SOURCE_URL_*` and `APHELION_STELLAR_BIN` environment variables --
//! so the node under test is unmodified and unaware.
//!
//! # Requirements
//!
//! A Postgres to create databases in, named by `APHELION_TEST_DATABASE_URL`
//! (or `DATABASE_URL`). Without one [`Harness::start`] returns
//! [`Unavailable`], and the tests skip rather than fail: a contributor who has
//! cloned the repository and not yet provisioned anything should still get a
//! green `cargo test`.

pub mod deployment;
pub mod exchange;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use aphelion_core::FeedId;
use aphelion_node::signer::NodeSigner;
use sqlx::{Connection, Executor, PgConnection};

pub use deployment::Deployment;
pub use exchange::Exchange;

/// Why a harness could not start.
///
/// Distinguished from a test failure on purpose: "there is no database here"
/// is a fact about the machine, and reporting it as a broken node would train
/// people to ignore this suite.
#[derive(Debug)]
pub enum Unavailable {
    NoDatabase(String),
    Failed(String),
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDatabase(why) => write!(f, "{why}"),
            Self::Failed(why) => write!(f, "harness failed to start: {why}"),
        }
    }
}

/// Run `body` against a harness, or skip with an explanation.
///
/// The skip is deliberately loud on stdout: a suite that silently does nothing
/// is worse than one that does not exist, because it reads as passing.
#[macro_export]
macro_rules! harness_or_skip {
    ($built:expr) => {
        match $built.await {
            Ok(h) => h,
            Err($crate::Unavailable::NoDatabase(why)) => {
                eprintln!("SKIP {}: {why}", module_path!());
                return;
            }
            Err(e) => panic!("{e}"),
        }
    };
}

/// One node process, and everything the harness needs to talk to or about it.
pub struct Node {
    pub name: String,
    pub public_key_hex: String,
    pub api_port: u16,
    database: String,
    child: Option<tokio::process::Child>,
    log: PathBuf,
}

impl Node {
    /// `http://127.0.0.1:<port>` for this node's own read-only API.
    pub fn api(&self) -> String {
        format!("http://127.0.0.1:{}", self.api_port)
    }

    /// Everything the process has written to stdout and stderr so far.
    ///
    /// The node logs to stderr, so this is how a failing assertion explains
    /// itself: the reason a node did not publish is almost always in here.
    pub fn logs(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Stop the process, as an operator killing a node does.
    pub async fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
        }
    }

    pub fn is_running(&self) -> bool {
        self.child.is_some()
    }
}

/// Harnesses run one at a time.
///
/// Each is several node processes and several databases on a machine that is
/// usually also compiling Rust. Running six concurrently turns every timing
/// assumption in the suite into a fight for CPU, and the failure reads as a
/// flaky node rather than as an overloaded runner. Serialising costs wall
/// clock and buys a suite whose failures mean something.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A running network of node processes.
pub struct Harness {
    pub deployment: Deployment,
    pub exchange: Exchange,
    pub nodes: Vec<Node>,
    dir: PathBuf,
    admin_url: String,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

/// How to shape the network before it starts.
pub struct Options {
    /// How many node processes to run.
    pub nodes: usize,
    /// Distinct nodes required before a round publishes.
    pub quorum: usize,
    /// Nodes to leave out of the registry, by index. A node in this set runs
    /// normally and signs correctly; the chain simply does not know its key.
    pub unregistered: Vec<usize>,
    /// The price every venue quotes at startup.
    pub price: String,
    /// Sources that must agree before a node signs anything.
    pub min_sources: usize,
    /// How long a round will keep using a source's last observation.
    ///
    /// Adjustable because it decides how quickly a venue going quiet stops
    /// counting. The default is long enough that a collector hiccup on a
    /// loaded runner cannot starve a round; a test about a venue outage wants
    /// it short, or it spends a minute watching stale observations expire.
    pub max_observation_age: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            nodes: 3,
            quorum: 3,
            unregistered: Vec::new(),
            price: "64231.55".into(),
            min_sources: 2,
            max_observation_age: Duration::from_secs(60),
        }
    }
}

impl Harness {
    /// Start `n` nodes at full weight, with quorum `n`.
    pub async fn start(nodes: usize) -> Result<Self, Unavailable> {
        Self::with(Options {
            nodes,
            quorum: nodes,
            ..Default::default()
        })
        .await
    }

    pub async fn with(opts: Options) -> Result<Self, Unavailable> {
        let _guard = ONE_AT_A_TIME.lock().await;
        let admin_url = database_url()?;
        probe(&admin_url).await?;
        let node_bin = find_binary("aphelion-node", "aphelion-node")?;

        let run = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join(format!("aphelion-harness-{}", &run[..12]));
        std::fs::create_dir_all(&dir).map_err(|e| Unavailable::Failed(e.to_string()))?;

        // Keys first: the deployment's registry is fixed when it starts, so
        // every public key has to exist before it does.
        let aggregator_id = aphelion_node::strkey::contract_id_bytes(deployment::AGGREGATOR)
            .map_err(|e| Unavailable::Failed(e.to_string()))?;
        let mut keys = Vec::new();
        for i in 0..opts.nodes {
            let path = dir.join(format!("node-{i}.json"));
            NodeSigner::generate(&path).map_err(|e| Unavailable::Failed(e.to_string()))?;
            let signer = NodeSigner::load(&path, aggregator_id)
                .map_err(|e| Unavailable::Failed(e.to_string()))?;
            keys.push((path, signer.public_key_hex()));
        }

        let registered: Vec<(String, u32)> = keys
            .iter()
            .enumerate()
            .filter(|(i, _)| !opts.unregistered.contains(i))
            .map(|(_, (_, pk))| (pk.clone(), 10_000))
            .collect();

        let deployment = Deployment::start(opts.quorum, &registered)
            .await
            .map_err(|e| Unavailable::Failed(e.to_string()))?;
        let exchange = Exchange::start(&opts.price)
            .await
            .map_err(|e| Unavailable::Failed(e.to_string()))?;

        let fake_stellar = find_binary("fake-stellar", "aphelion-harness")?;

        let mut nodes = Vec::new();
        for (i, (key_path, public_key_hex)) in keys.into_iter().enumerate() {
            let name = format!("harness-{i}");
            let database = format!("aphelion_h_{}_{i}", &run[..12]);
            create_database(&admin_url, &database).await?;

            let api_port = free_port()?;
            let config = dir.join(format!("node-{i}.toml"));
            std::fs::write(&config, config_toml(&name, &key_path, api_port, &opts))
                .map_err(|e| Unavailable::Failed(e.to_string()))?;

            let log = dir.join(format!("node-{i}.log"));
            let out =
                std::fs::File::create(&log).map_err(|e| Unavailable::Failed(e.to_string()))?;
            let err = out
                .try_clone()
                .map_err(|e| Unavailable::Failed(e.to_string()))?;

            let mut cmd = tokio::process::Command::new(&node_bin);
            cmd.arg("--config")
                .arg(&config)
                .arg("run")
                .env("DATABASE_URL", url_for(&admin_url, &database))
                // The fake CLI never looks at this, but `CliChain::new`
                // refuses to start without something seed-shaped.
                .env(
                    "APHELION_STELLAR_SECRET",
                    "SBUW3DVYLKLY5ZUJD5PL2ZHOFWJSVWGJA5DVLPVDNGTOFUEBEIRJXNQO",
                )
                .env("APHELION_STELLAR_BIN", &fake_stellar)
                .env("APHELION_FAKE_LEDGER_URL", deployment.url())
                .env("RUST_LOG", "aphelion_node=debug,warn")
                .stdin(Stdio::null())
                .stdout(Stdio::from(out))
                .stderr(Stdio::from(err))
                .kill_on_drop(true);
            for (k, v) in exchange.env() {
                cmd.env(k, v);
            }

            let child = cmd
                .spawn()
                .map_err(|e| Unavailable::Failed(format!("cannot start {node_bin:?}: {e}")))?;

            nodes.push(Node {
                name,
                public_key_hex,
                api_port,
                database,
                child: Some(child),
                log,
            });
        }

        let harness = Self {
            deployment,
            exchange,
            nodes,
            dir,
            admin_url,
            _guard,
        };
        harness.await_ready().await?;
        Ok(harness)
    }

    /// Wait until every node is serving its API.
    ///
    /// A node that never gets there has its log dumped into the failure: a
    /// harness that times out with no explanation costs more to debug than the
    /// bug it found.
    async fn await_ready(&self) -> Result<(), Unavailable> {
        for node in &self.nodes {
            let deadline = Instant::now() + Duration::from_secs(45);
            let url = format!("{}/health", node.api());
            loop {
                if http_get(&url).await.is_some() {
                    break;
                }
                if Instant::now() > deadline {
                    return Err(Unavailable::Failed(format!(
                        "node `{}` never served its API. Log:\n{}",
                        node.name,
                        node.logs()
                    )));
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        Ok(())
    }

    /// Wait for `feed` to have a published price, or return `None` on timeout.
    pub async fn await_published(
        &self,
        feed: &FeedId,
        within: Duration,
    ) -> Option<aphelion_node::chain::OnChainPrice> {
        self.until(within, || async { self.deployment.published(feed).await })
            .await
    }

    /// Poll `check` until it yields a value, or `within` elapses.
    ///
    /// Polling rather than sleeping a fixed interval: the nodes are real
    /// processes on a shared machine, and a test that assumes they are prompt
    /// is a test that fails on a loaded CI runner for no reason.
    pub async fn until<T, F, Fut>(&self, within: Duration, mut check: F) -> Option<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Option<T>>,
    {
        let deadline = Instant::now() + within;
        loop {
            if let Some(v) = check().await {
                return Some(v);
            }
            if Instant::now() > deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Every node's log, labelled. For explaining a failed assertion.
    pub fn logs(&self) -> String {
        self.nodes
            .iter()
            .map(|n| format!("--- {} ---\n{}", n.name, n.logs()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Stop the processes and drop the databases they were using.
    ///
    /// Called by `Drop` too, but that cannot await, so a test that wants the
    /// databases actually gone should call this.
    pub async fn shutdown(&mut self) {
        for node in &mut self.nodes {
            node.kill().await;
        }
        for node in &self.nodes {
            let _ = drop_database(&self.admin_url, &node.database).await;
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Processes are `kill_on_drop`, so they go regardless. The databases
        // need an await to remove, which is not available here; a best-effort
        // blocking attempt keeps a developer's Postgres from filling up with
        // leftovers when a test panics before `shutdown`.
        for node in &mut self.nodes {
            if let Some(child) = node.child.as_mut() {
                let _ = child.start_kill();
            }
        }
        let admin = self.admin_url.clone();
        let names: Vec<String> = self.nodes.iter().map(|n| n.database.clone()).collect();
        let _ = std::thread::spawn(move || {
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                rt.block_on(async {
                    for name in names {
                        let _ = drop_database(&admin, &name).await;
                    }
                });
            }
        })
        .join();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---------------------------------------------------------------------------
// plumbing
// ---------------------------------------------------------------------------

fn database_url() -> Result<String, Unavailable> {
    std::env::var("APHELION_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .map_err(|_| {
            Unavailable::NoDatabase(
                "no APHELION_TEST_DATABASE_URL (or DATABASE_URL) set; \
                 the multi-process harness needs a Postgres to create databases in"
                    .into(),
            )
        })
}

async fn probe(url: &str) -> Result<(), Unavailable> {
    PgConnection::connect(url).await.map(|_| ()).map_err(|e| {
        Unavailable::NoDatabase(format!("cannot reach Postgres at the configured URL: {e}"))
    })
}

/// Swap the database name in a Postgres URL, keeping credentials and host.
fn url_for(admin_url: &str, database: &str) -> String {
    match admin_url.rsplit_once('/') {
        // Preserve any query string (sslmode, and so on).
        Some((prefix, tail)) => match tail.split_once('?') {
            Some((_, query)) => format!("{prefix}/{database}?{query}"),
            None => format!("{prefix}/{database}"),
        },
        None => format!("{admin_url}/{database}"),
    }
}

async fn create_database(admin_url: &str, name: &str) -> Result<(), Unavailable> {
    let mut conn = PgConnection::connect(admin_url)
        .await
        .map_err(|e| Unavailable::Failed(e.to_string()))?;
    // Dropped first so a crashed previous run cannot leave a database with
    // rows in it that this one would then read as its own.
    let _ = conn
        .execute(format!(r#"DROP DATABASE IF EXISTS "{name}""#).as_str())
        .await;
    conn.execute(format!(r#"CREATE DATABASE "{name}""#).as_str())
        .await
        .map_err(|e| Unavailable::Failed(format!("cannot create database {name}: {e}")))?;
    Ok(())
}

async fn drop_database(admin_url: &str, name: &str) -> Result<(), String> {
    let mut conn = PgConnection::connect(admin_url)
        .await
        .map_err(|e| e.to_string())?;
    conn.execute(format!(r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#).as_str())
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Ask the OS for a port, then let it go.
///
/// Racy in principle -- something could take the port between here and the
/// node binding it -- but the alternative is a fixed range, which collides
/// with any other harness on the machine rather than with a rare accident.
fn free_port() -> Result<u16, Unavailable> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| Unavailable::Failed(e.to_string()))?;
    listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| Unavailable::Failed(e.to_string()))
}

/// Locate a binary built by this workspace, building it if it is not there yet.
///
/// `CARGO_BIN_EXE_` is only set for integration tests, and only for binaries of
/// the crate under test, so neither the node's binary nor the fake CLI can be
/// found that way from library code. Walking up from the current executable
/// finds them in whichever profile directory cargo is using, and building on a
/// miss means the suite works however it is invoked -- `cargo test -p
/// aphelion-harness` included, which does not otherwise build another crate's
/// binary. The build is a no-op once it is up to date.
fn find_binary(name: &str, package: &str) -> Result<PathBuf, Unavailable> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };

    let exe = std::env::current_exe().map_err(|e| Unavailable::Failed(e.to_string()))?;
    if let Some(found) = search_up(&exe, &file) {
        return Ok(found);
    }

    let status = std::process::Command::new(std::env::var("CARGO").unwrap_or("cargo".into()))
        .args(["build", "-p", package, "--bin", name])
        .status()
        .map_err(|e| Unavailable::Failed(format!("cannot run cargo to build `{name}`: {e}")))?;
    if !status.success() {
        return Err(Unavailable::Failed(format!("building `{name}` failed")));
    }

    search_up(&exe, &file).ok_or_else(|| {
        Unavailable::Failed(format!("built `{name}` but cannot find it near {exe:?}"))
    })
}

/// Look for `file` in each ancestor directory of `from`.
///
/// A test binary lives in `target/<profile>/deps`, and the binaries it wants
/// are one level up in `target/<profile>`.
fn search_up(from: &Path, file: &str) -> Option<PathBuf> {
    let mut dir = from.parent();
    while let Some(d) = dir {
        let candidate = d.join(file);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

/// A GET that yields the body on success and `None` on any failure.
///
/// The node's `/health` answers 503 while a feed is degraded, which is still a
/// perfectly good "the process is up", so the status is deliberately not
/// checked here -- only that something answered.
pub async fn http_get(url: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    client.get(url).send().await.ok()?.text().await.ok()
}

/// The configuration one node runs with.
///
/// The intervals are compressed hard: a round every two seconds rather than
/// every sixty, because the suite has to observe several rounds and a test
/// that takes ten minutes is a test nobody runs. Everything the compression
/// touches is still checked -- `max_observation_age` stays comfortably above
/// `poll_interval`, as the node's own validation insists.
fn config_toml(name: &str, key_path: &Path, api_port: u16, opts: &Options) -> String {
    format!(
        r#"# Generated by the multi-process harness.
[node]
name = "{name}"
key_path = "{key}"

[network]
rpc_url = "http://127.0.0.1:1/unused-by-the-fake-cli"
network_passphrase = "Test SDF Network ; September 2015"
network = "testnet"
registry_contract = "{registry}"
aggregator_contract = "{aggregator}"
submitter_secret_env = "APHELION_STELLAR_SECRET"
submitter_account = "GCEZWKCA5VLDNRLN3RPRJMRZOX3Z6G5CHCGSNFHEYVXM3XOJMDS674JZ"

[database]
url_env = "DATABASE_URL"
max_connections = 4
retention = "1h"

[api]
bind = "127.0.0.1:{api_port}"
metrics_enabled = true

[engine]
round_interval = "2s"
poll_interval = "1s"
max_observation_age = "{observation_age}s"
min_sources_per_feed = {min_sources}
max_source_deviation_bps = 1000
submit_deviation_bps = 1
heartbeat = "4s"
max_clock_skew = "30s"

[sources]
binance = true
kraken = true
coinbase = true
timeout = "5s"

[[feeds]]
id = "BTC_USD"
confidence_bps = 50
sources = {{ binance = "BTCUSDT", kraken = "XBTUSD", coinbase = "BTC-USD" }}
"#,
        key = key_path.display(),
        registry = deployment::REGISTRY,
        aggregator = deployment::AGGREGATOR,
        min_sources = opts.min_sources,
        observation_age = opts.max_observation_age.as_secs(),
    )
}
