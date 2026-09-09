<div align="center">

# Aphelion Oracle Network

**Decentralized, Byzantine-fault-tolerant price oracles for Stellar and Soroban.**

[![Status](https://img.shields.io/badge/status-pre--alpha-orange)](#project-status)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.91%2B-b7410e)](rust-toolchain.toml)
[![Soroban](https://img.shields.io/badge/soroban--sdk-27.0-7d00ff)](contracts/Cargo.toml)

[Overview](#overview) · [How it works](#how-it-works) · [Security model](#security-model) · [Quick start](#quick-start) · [Integration](#integrating-a-dapp) · [Roadmap](#roadmap)

</div>

---

> [!WARNING]
> **Pre-alpha. Unaudited. Testnet only.**
> Aphelion has not been audited and is not running on Stellar mainnet. Contract
> interfaces, storage layouts and the canonical signing payload may still change
> in backwards-incompatible ways. Do not secure real value with it yet.
> See [Project status](#project-status) for what is actually built today.

---

## Overview

Aphelion is the first decentralized price oracle network built natively for
Stellar and Soroban. It closes the gap between where prices exist — order books
on Binance, Kraken, Coinbase — and where smart contracts need them: on chain,
verifiable, and not under any single party's control.

### The problem

A lending protocol on Soroban needs to know what a borrower's collateral is
worth. Every naive answer puts the protocol at the mercy of one party:

| Approach | Failure mode |
| --- | --- |
| Call an exchange API from a backend | The backend can lie, and the exchange can go down |
| One trusted signer publishes prices | A single key liquidates every position on the platform |
| A handful of signers run by one organisation | Still one organisation, with extra steps |

Each is a single point of failure wearing a different hat. Any of them can
liquidate a solvent borrower.

### The approach

Aphelion replaces that trusted party with a network of independently operated
nodes, each of which:

1. Reads the same market from **several exchanges at once**, and discards any
   venue that disagrees with the rest,
2. Signs its conclusion with an **Ed25519 key bound to one deployment**,
3. Submits it to a Soroban contract that takes a **reputation-weighted median**
   across nodes.

A node that lies is outvoted by the median, loses reputation, and — beyond a
threshold — loses staked XLM. A node that is merely broken stops carrying
weight before anyone has to notice manually.

The result is a price that no participant can move on their own, including the
people who built it.

### Design principles

- **Two independent consensus layers.** Cross-source aggregation inside each
  node defends against a broken exchange; cross-node aggregation on chain
  defends against a dishonest operator. Neither substitutes for the other.
- **Signature authority, not transaction authority.** A node is identified by
  its Ed25519 public key, never by the account that pays the fee. Anyone can
  relay a signed price; only the key holder can produce one.
- **Refuse rather than guess.** Not enough sources, a crossed order book, a
  skewed clock, a stale observation — every one of these makes the node publish
  nothing instead of publishing something plausible.
- **Everything reproducible.** Every raw observation is retained, so any
  published price can be replayed from the inputs that produced it. That is what
  makes a dispute answerable with evidence.

---

## How it works

### The round cycle

Each node runs two decoupled loops. The collector polls exchanges continuously
and writes what it sees to Postgres; the round loop reads a consistent snapshot
on a fixed cadence. Decoupling them means a flaky exchange degrades a round
rather than stalling the node, and a node that loses its RPC endpoint keeps
collecting evidence while it waits.

```
  ┌── continuously ─────────────────────────────────────────────────────┐
  │  Binance · Kraken · Coinbase · CoinGecko                            │
  │        │  best bid / best ask, per venue                            │
  │        ▼                                                            │
  │  collector ──▶ Postgres  (raw_prices: every observation, retained)  │
  └─────────────────────────────────────────────────────────────────────┘
                                  │
  ┌── every round_interval (default 60s) ───────────────────────────────┐
  │  1. clock check      node time vs ledger time, else abort           │
  │  2. snapshot         freshest observation per source                │
  │  3. aggregate        provisional median ▶ drop outliers ▶ re-median │
  │  4. worth a fee?     moved > threshold, or heartbeat due            │
  │  5. sign             Ed25519 over the canonical 117-byte payload    │
  │  6. submit           aggregator contract, via Stellar               │
  └─────────────────────────────────────────────────────────────────────┘
                                  │
  ┌── on chain ─────────────────────────────────────────────────────────┐
  │  verify signature ─▶ check nonce, staleness ─▶ look up node weight  │
  │  quorum reached? ─▶ weighted median ─▶ reward in-band, penalise out │
  │  publish PriceData ─▶ append to history ring (TWAP)                 │
  └─────────────────────────────────────────────────────────────────────┘
                                  │
                        dApp: get_price("BTC_USD")
```

### Cross-source aggregation, inside one node

A node never trusts a single venue. Its local pass is deliberately two-stage:

1. Take a provisional median across every source. The **median**, not the mean —
   the reference used to hunt for an outlier must not already be dragged by it.
2. Discard any source further than `max_source_deviation_bps` from that
   reference, then re-median what survives.

If fewer than `min_sources_per_feed` survive, the node signs **nothing**.
Publishing a price backed by one exchange is how an oracle launders a single
venue's outage into apparent consensus.

Quotes are the **mid of best bid and ask**, not the last trade: one small trade
at a bad price moves the last print, but it does not move the mid. A crossed
book (bid above ask) or an implausibly wide spread is rejected outright, because
the mid of an inconsistent snapshot is a number nobody could have traded at.

### Cross-node aggregation, on chain

The aggregator collects signed submissions until quorum, then takes a
**reputation-weighted median**. Weight comes from the registry:

| Reputation | Status | Voting weight |
| --- | --- | --- |
| ≥ 7 000 | Active | 10 000 bps (full) |
| 3 000 – 6 999 | Active | 5 000 bps (half) |
| < 3 000 | Jailed | 0 |
| — | Exiting | 0 |

A newly registered node starts at 5 000 — half weight — and must earn its way to
full. This is the arithmetic that makes a Sybil attack unattractive rather than
merely detectable: registering ten fresh identities buys five nodes' worth of
influence for ten nodes' worth of bonded capital, and none of it counts until
those identities have behaved correctly for long enough to matter.

Weight is captured **at submission time**, not read again at finalisation, so a
reputation change mid-round cannot retroactively re-weight votes already cast.

### Why the median

The median is what makes a round survive a liar. Moving it requires controlling
more than half the weight; it does not care how extreme a minority's number is.

```
Real price: $42 500.  Five nodes, one lying.

  submissions   30 000 · 42 498 · 42 499 · 42 500 · 42 501
  median        42 499                       ← the lie is discarded, not averaged
  mean          40 099                       ← the lie moves it by $2 400
```

The liar's reward for that attempt is −500 reputation and, if it is far enough
out of band, seized stake.

---

## Security model

### What Aphelion tolerates

With the default parameters — 7 registered nodes, quorum of 5 — the network
produces correct prices while **up to 2 nodes** are malicious or broken. That is
28.6%, not "one third"; the honest bound is stated here rather than rounded up.

An attacker holding 4 of 7 nodes *can* move the median. Aphelion's defence at
that point is economic and social rather than arithmetic: acquiring that
position requires bonding four minimum stakes and building four reputations to
full weight over hundreds of rounds, all of which is publicly observable on
chain, and all of which is forfeit when the slashing contract resolves against
them. The mitigation is to grow the node set — the parameters are governance
values, not constants.

### Defences

| Attack | Defence |
| --- | --- |
| One node submits a fabricated price | Weighted median discards it; reputation penalty; stake slashed if far out of band |
| A minority colludes | Same, plus quorum requires both a headcount **and** a minimum total weight |
| Sybil — many fresh identities | Each needs its own bonded stake, and starts at half weight until it earns full |
| Replaying an old signed price | Strictly increasing per-`(node, feed)` nonce, enforced on chain |
| Replaying a testnet price onto mainnet | The aggregator's contract id is inside the signed payload |
| Replaying a BTC quote as an XLM quote | The feed id is inside the signed payload |
| Signature forgery | Ed25519, verified on chain by `env.crypto().ed25519_verify` |
| A single exchange being manipulated | Multi-source median with deviation filtering, inside every node |
| Momentary price spike / flash loan | `get_twap` weights each observation by how long it stood |
| A node's clock drifting | Node aborts the round if it disagrees with ledger time; contract independently rejects stale and future-dated observations |
| A node quietly dying | Missed rounds erode reputation, so weight decays before an operator notices |
| An exchange serving a stale cache | Venue-supplied timestamps are preserved, never replaced with `now()` |

### The canonical signing payload

The single most safety-critical detail in the system. A node signs exactly these
117 bytes, and the aggregator reconstructs them byte-for-byte before verifying.
If the two implementations ever disagree, every submission is rejected.

| Offset | Length | Field |
| ---: | ---: | --- |
| 0 | 17 | Domain separator, ASCII `APHELION_PRICE_V1` |
| 17 | 32 | Aggregator contract id (raw 32 bytes) |
| 49 | 32 | Feed id, ASCII, right-padded with `0x00` |
| 81 | 16 | Price, `i128` big-endian, scaled by 1e8 |
| 97 | 8 | Observation timestamp, `u64` unix seconds |
| 105 | 4 | Confidence half-width, `u32` basis points |
| 109 | 8 | Nonce, `u64`, strictly increasing per `(node, feed)` |

Every field earns its place: the domain separator stops an Aphelion signature
being replayed as a signature over anything else the key signs; the contract id
binds it to one deployment; the feed id binds it to one market; the timestamp
lets the contract reject stale data; the nonce blocks replay inside the
staleness window.

Because the reference implementation is off chain (`aphelion-core`) and the
mirror is on chain (`aphelion-aggregator`), the two are pinned together by
shared test vectors in [`tests/vectors/price_message.json`](tests/vectors/price_message.json),
asserted from both sides and generated by a third, independent implementation in
[`scripts/gen_test_vectors.py`](scripts/gen_test_vectors.py). A failure there is
a consensus-breaking bug, not a flaky test.

The aggregation arithmetic is pinned the same way, in
[`tests/vectors/aggregation.json`](tests/vectors/aggregation.json). A node
predicts a round's outcome with `aphelion-core::math` and the contract decides
it with `contracts/aggregator/src/math.rs`; a single unit of disagreement
between them is enough to slash an honest node for arithmetic it had no way to
see. The cases that matter are the ones where two implementations could
plausibly differ and both look right: the exact-tie branch of the median,
truncation toward zero rather than rounding, and a TWAP window whose
observations all predate it.

### Reporting a vulnerability

Please do not open a public issue for security problems. See
[SECURITY.md](SECURITY.md) for the disclosure process.

---

## Project status

Aphelion is under active development. This table is the honest state of the
repository, not the target architecture.

| Component | Status | Tests |
| --- | --- | --- |
| `aphelion-core` — fixed-point prices, aggregation math, signing payload | ✅ Implemented | 26 |
| `aphelion-node` — sources, collector, round loop, signer, HTTP API, CLI | ✅ Implemented | 54 |
| `aphelion-registry` contract — identity, stake, reputation, slashing accounting | ✅ Implemented | 24 |
| `aphelion-aggregator` contract — consensus, TWAP, metering, absence sweeps | ✅ Implemented | 56 |
| `aphelion-slashing` contract — disputes, committee voting, appeals | ✅ Implemented | 31 |
| `consumer-example` contract — reference dApp integration | ✅ Implemented | 17 |
| On-chain Byzantine simulation — multi-round adversarial scenarios | ✅ Implemented | 6 |
| Multi-node simulation — several signers against one in-memory network | ✅ Implemented | 10 |
| Multi-node harness — several node *processes* against one deployment | 📋 Planned | — |
| Testnet deployment | 📋 Planned | — |
| Mainnet deployment | 📋 Planned | — |

Legend: ✅ implemented and tested · 🚧 in progress · 📋 planned

The Byzantine simulation runs the real registry and aggregator together across
multiple rounds with a mix of honest and dishonest nodes. The multi-node
simulation does the same on the off-chain side, running several independently
keyed signers against one in-memory network whose median is computed by the
same function the contract mirrors.

What neither covers is several node *processes*, each with its own database and
RPC connection, racing each other for real. Everything at that level — a lost
RPC endpoint, clock drift between machines, two nodes contending for the same
round — is still only covered by unit tests.

---

## Repository layout

```
aphelion/
├── contracts/                  Soroban contracts (separate cargo workspace, wasm target)
│   ├── registry/               Node identity, stake, reputation, slashing accounting
│   ├── aggregator/             Submission verification, consensus, price storage, TWAP
│   ├── slashing/               Dispute resolution and committee governance
│   └── consumer-example/       Reference integration for dApp authors
├── crates/                     Off-chain services (root cargo workspace, host target)
│   ├── aphelion-core/          Shared price math and the canonical signing payload
│   └── aphelion-node/          The node binary
│       └── src/
│           ├── sources/        Binance, Kraken, Coinbase, CoinGecko
│           ├── engine/         Collector, aggregation, round loop
│           ├── chain/          ChainClient trait: CLI-backed, RPC reads, in-memory mock
│           ├── db/             Postgres schema access
│           └── api/            Read-only HTTP surface
├── migrations/                 SQL migrations, applied automatically at startup
├── tests/vectors/              Cross-implementation signing and aggregation vectors
├── scripts/                    Deployment, registration and vector generation
├── deploy/                     Prometheus and Grafana provisioning
└── docs/
    ├── node-operator.md        Setup, daily operation and troubleshooting
    └── contracts.md            Every contract function, error code and caller
```

The two cargo workspaces are separate on purpose: the contracts target wasm32 and
pin `soroban-sdk`, while the node targets the host and pins tokio, sqlx and
reqwest. One lockfile for both would let a routine bump on either side break the
other.

---

## Quick start

### Prerequisites

- Rust 1.91+ (`rustup toolchain install 1.91.0`), with the `wasm32v1-none`
  target for the contracts (`rustup target add wasm32v1-none`)
- PostgreSQL 14+
- [Stellar CLI](https://developers.stellar.org/docs/tools/developer-tools/cli/stellar-cli) —
  used by the node to build, sign and submit transactions
- `jq`, for `scripts/deploy.sh`

### Build and test

```bash
git clone https://github.com/aphelion-oracle/aphelion.git
cd aphelion

# Off-chain services
cargo build --workspace
cargo test  --workspace

# Contracts (separate workspace)
cargo test  --manifest-path contracts/Cargo.toml

# Contract wasm, for deployment
scripts/build-contracts.sh
```

### Deploying the contract set

```bash
export APHELION_STELLAR_SECRET="S..."   # pays for the deployment
export APHELION_ADMIN_ACCOUNT="G..."    # administers the contracts afterwards
scripts/deploy.sh
```

The three contracts refer to each other, so all of them are deployed before any
of them is initialised — deploying is what fixes an address, initialising is
what teaches each contract the others'. That is why `initialize` is a separate
call rather than a constructor. Every parameter is an environment variable with
a conservative default, the whole plan is printed and confirmed before anything
is submitted, and the result is written to `deployments/<network>.json`.

### Running a node

The short version is below; [`docs/node-operator.md`](docs/node-operator.md)
covers setup, daily operation and troubleshooting in full.

```bash
# 1. Create the node's signing identity. This key is the node's on-chain
#    identity; back it up before bonding any stake against it.
cargo run -p aphelion-node -- keygen --out ./node-key.json

# 2. Configure. Secrets are named by the config, never stored in it.
cp aphelion.example.toml aphelion.toml
export DATABASE_URL="postgres://aphelion:aphelion@localhost/aphelion"
export APHELION_STELLAR_SECRET="S..."      # funds submission transactions

# 3. Verify every exchange is reachable. Needs no database, no chain access
#    and no registration — run this first whenever a feed misbehaves.
cargo run -p aphelion-node -- check-sources

# 4. Watch what the node *would* publish, without submitting anything.
cargo run -p aphelion-node -- run --dry-run

# 5. Register on chain, then run for real.
scripts/register-node.sh "$(cargo run -q -p aphelion-node -- pubkey)"
cargo run --release -p aphelion-node -- run
```

Health, once running:

```bash
curl -s localhost:8080/health          | jq   # per-feed liveness; 503 when degraded
curl -s localhost:8080/v1/prices/BTC_USD | jq # local price, on-chain price, per-source breakdown
```

### With Docker Compose

```bash
docker compose up -d          # Postgres, node, Prometheus, Grafana
docker compose logs -f node
```

### CLI reference

| Command | Purpose |
| --- | --- |
| `run [--dry-run]` | Collect, aggregate, sign, submit, serve |
| `keygen --out <path>` | Generate an Ed25519 node key (refuses to overwrite) |
| `pubkey` | Print the configured node's public key |
| `check-sources` | Fetch every configured source once and print the result |
| `sign <feed> <price> <ts> [conf] [nonce]` | Reproduce the exact bytes and signature for a submission |
| `migrate` | Apply database migrations and exit |
| `show-config` | Print the effective configuration after environment overrides |

---

## Integrating a dApp

Aphelion is read directly from your Soroban contract — there is no relayer, no
callback, and no subscription to manage.

```rust
use soroban_sdk::{contract, contractimpl, symbol_short, Address, Env, Symbol};

// Generated from the aggregator's interface.
mod aphelion {
    soroban_sdk::contractimport!(
        file = "../aggregator/target/wasm32-unknown-unknown/release/aphelion_aggregator.wasm"
    );
}

#[contract]
pub struct Lending;

#[contractimpl]
impl Lending {
    pub fn liquidate(env: Env, oracle: Address, borrower: Address, collateral: i128) {
        let oracle = aphelion::Client::new(&env, &oracle);

        // Reverts if the feed is unknown or the price is older than 300s.
        // Prefer this over `get_price` + a manual age check: the freshness
        // requirement belongs in the same call that reads the value, or it
        // eventually gets forgotten on some other code path.
        let price = oracle.get_price_checked(&symbol_short!("BTC_USD"), &300);

        // For anything a borrower could profitably manipulate within a single
        // block, use the time-weighted average instead of the spot price.
        let twap = oracle.get_twap(&symbol_short!("BTC_USD"), &3600);

        // Prices are scaled by 1e8.
        let collateral_value = collateral * twap / 100_000_000;

        if collateral_value < Self::debt_of(&env, &borrower) {
            Self::seize(&env, &borrower);
        }

        // `price.confidence_bps` widens when the network's sources disagree.
        // Treating it as a constant discards the network's own warning that
        // it is less certain than usual.
        let _ = price.confidence_bps;
    }
}
```

### Read interface

| Function | Returns | Notes |
| --- | --- | --- |
| `get_price(feed)` | `Option<PriceData>` | Current published price; `None` if never published |
| `get_price_checked(feed, max_age)` | `PriceData` | Reverts with `StalePrice` past `max_age` seconds |
| `get_price_metered(consumer, feed, max_age)` | `PriceData` | As above, charging `read_fee` to a prepaid balance |
| `get_twap(feed, window)` | `i128` | Time-weighted average over `window` seconds |
| `history(feed)` | `Vec<Observation>` | Retained observation ring, for charts and settlement |
| `feeds()` | `Vec<Symbol>` | Every configured feed |
| `feed_config(feed)` | `FeedConfig` | Heartbeat and quorum, so you can size your own checks |
| `pending_round(feed)` | `Option<PendingRound>` | The round currently accepting submissions |
| `last_nonce(pubkey, feed)` | `u64` | Highest nonce accepted from a node |
| `ledger_time()` | `u64` | Ledger clock, for client-side skew checks |

`get_twap` reverts with `InsufficientHistory` when the retained observations do
not span the whole window you asked for. That is deliberate: a TWAP computed
over a tenth of the requested window is not a conservative answer, it is a
wrong one, and it is indistinguishable from a real one once returned.

Every function, error code and caller is listed in
[`docs/contracts.md`](docs/contracts.md).

`PriceData` carries the price, the **oldest** contributing observation's
timestamp, the contributing node count, the confidence half-width in basis
points, the cross-node standard deviation, the round id, and the publication
time. The timestamp is deliberately the most pessimistic of the round, so a
consumer's freshness check cannot be satisfied by one fast node in an otherwise
stale round.

### Write interface

| Function | Who calls it | Purpose |
| --- | --- | --- |
| `submit_price(feed, pubkey, price, timestamp, confidence_bps, nonce, signature)` | Anyone, relaying for a node | Verify a signed observation and join the open round |
| `sweep_absent(pubkeys)` | Anyone | Charge a missed round to nodes silent past `absence_threshold` |
| `deposit(from, amount)` / `refund(consumer, amount)` | A metered consumer | Fund or reclaim a prepaid read balance |
| `forward_fees()` | Anyone | Push collected read fees into the registry's reward pool |

`submit_price` has no `require_auth`. A submission is authorised by the Ed25519
signature over the canonical payload, so the account paying the fee carries no
authority at all.

`sweep_absent` is batched and permissionless rather than automatic. Sweeping
the whole node set at the end of every round would make the cost of closing a
round grow with the size of the network, which charges the feed for the
network's success. Here the caller pays for the keys it names, and a single
silence is charged once however many times it is swept.

### Integration guidance

- **Always bound staleness.** `get_price_checked` makes it hard to forget.
- **Use TWAP for anything liquidatable.** Spot price is appropriate for display;
  a position that an attacker can open and close in one block is not.
- **Respect `confidence_bps`.** It widens when the network's own sources
  disagree. Widen your safety margins with it.
- **Handle `None`.** A feed that has never published, or one that has been
  disabled, must not read as a price of zero in your contract.

---

## Data model

### Prices

Every price crossing a boundary is an `i128` scaled by **1e8**. Floating point
appears only at the very edge, when parsing an exchange's JSON, and is converted
immediately. Eight decimals covers both ends of the range that matters: BTC at
six integer digits, and a long-tail Stellar asset at $0.00000042 with two
significant figures still intact.

### Feed identifiers

Feed ids are constrained to `[A-Za-z0-9_]`, at most 32 bytes — the character set
`soroban_sdk::Symbol` accepts — so the same identifier is a storage key on chain,
a column value in Postgres and a URL path segment with no escaping anywhere:
`BTC_USD`, `ETH_USD`, `XLM_USD`.

### Node-local schema

The node's database is **not** consensus state; the chain is the source of truth
for prices. What lives here is the evidence trail.

| Table | Purpose |
| --- | --- |
| `raw_prices` | Every observation from every source, retained for the configured window |
| `local_rounds` | Every round this node composed, signed or skipped, with its signature |
| `feed_nonces` | Monotonic nonce allocator; must survive restarts or submissions read as replays |
| `source_health` | Per-source success/failure history, behind `/health` and `/v1/sources` |
| `node_snapshot` | Cached on-chain view, so reputation is reportable while RPC is down |

---

## Operating a node

### Configuration

Layered, in increasing precedence: built-in defaults, the TOML file, then
environment variables. **Secrets are only ever read from the environment** — the
TOML names the variable to read rather than carrying the value, so a config file
can live in a private repo without becoming a credential.

```toml
[node]
name     = "aphelion-eu-1"
key_path = "./node-key.json"

[network]
rpc_url             = "https://soroban-testnet.stellar.org"
network_passphrase  = "Test SDF Network ; September 2015"
registry_contract   = "C..."
aggregator_contract = "C..."
submitter_account   = "G..."
submitter_secret_env = "APHELION_STELLAR_SECRET"   # the name, not the secret

[engine]
round_interval           = "60s"   # must match the aggregator's min_round_interval
poll_interval            = "10s"
max_observation_age      = "120s"
min_sources_per_feed     = 2       # sign nothing below this
max_source_deviation_bps = 1000    # discard a venue more than 10% off the median
submit_deviation_bps     = 25      # publish on a 0.25% move...
heartbeat                = "300s"  # ...or every 5 minutes regardless
max_clock_skew           = "30s"   # refuse to sign beyond this drift from ledger time

[[feeds]]
id             = "BTC_USD"
confidence_bps = 50
sources        = { binance = "BTCUSDT", kraken = "XBTUSD", coinbase = "BTC-USD" }
```

Configuration is validated at startup, and a config that could never work is a
startup failure rather than a silent runtime one — a feed mapping fewer sources
than `min_sources_per_feed`, a feed pointing at a disabled source, a round
interval shorter than the poll interval, an account address where a contract
address belongs.

### When the node publishes, and when it does not

Submitting every round on a quiet feed spends real money republishing a number
no position depends on. Submitting only on movement leaves consumers unable to
tell "unchanged" from "this node is dead". Aphelion resolves this with movement
**and** a heartbeat: publish when the price moves more than
`submit_deviation_bps`, and at least once per `heartbeat` regardless.

### HTTP API

Deliberately **read-only**. Nothing on this port can change what the node
publishes, which is what makes it safe to expose inside a cluster without an
auth layer.

| Endpoint | Purpose |
| --- | --- |
| `GET /health` | Per-feed liveness; **503** when a feed is short of sources or overdue |
| `GET /ready` | Database and RPC reachability |
| `GET /metrics` | Prometheus exposition |
| `GET /v1/node` | Identity, version, uptime, on-chain registry record |
| `GET /v1/feeds` | Configured feeds and their sources |
| `GET /v1/prices/{feed}` | Local price, on-chain price, divergence, per-source breakdown |
| `GET /v1/rounds?feed=&limit=` | Recent rounds and their outcomes |
| `GET /v1/sources` | Per-source health |

`/health` reports **usefulness**, not just liveness: a process that is running
but has not composed a round in ten minutes is not healthy in any sense an
on-call engineer cares about, so it returns 503 and pages somebody.

### Metrics

| Metric | Meaning |
| --- | --- |
| `aphelion_source_fetches_total{source,outcome}` | Fetch attempts by venue and outcome |
| `aphelion_source_latency_seconds{source}` | Per-venue fetch latency |
| `aphelion_source_price{source,feed}` | Latest price seen per venue |
| `aphelion_source_spread_bps{feed}` | Highest-to-lowest source spread — the best early warning available |
| `aphelion_local_price{feed}` | This node's aggregated price |
| `aphelion_rounds_total{feed,outcome}` | Rounds by outcome |
| `aphelion_submissions_total{feed,outcome}` | On-chain submissions by outcome |
| `aphelion_clock_skew_seconds` | Node clock minus ledger clock |
| `aphelion_reputation`, `aphelion_stake` | On-chain standing |
| `aphelion_seconds_since_submission{feed}` | Time since this node last landed a price |

Suggested alerts: `aphelion_seconds_since_submission > 2 × heartbeat`,
`aphelion_clock_skew_seconds` outside ±30, `aphelion_reputation < 4000`,
`aphelion_source_spread_bps` sustained above the usual band. All five ship in
[`deploy/prometheus/alerts.yml`](deploy/prometheus/alerts.yml).

`docker compose up` also provisions a Grafana dashboard
([`deploy/grafana/provisioning/dashboards/node-overview.json`](deploy/grafana/provisioning/dashboards/node-overview.json))
at <http://localhost:3000>. Its panels are ordered by the question an operator
asks first: am I still counted, is what I publish right, and if not, which venue
is at fault. It is provisioned read-only from the repository — a dashboard that
exists only in one operator's browser is one nobody else can reproduce when they
are the person on call.

---

## Economics

Aphelion's incentives are a mechanism, not a forecast. This section describes how
value moves; it deliberately makes no projection of revenue or operator earnings,
because those depend on adoption and on the XLM price, and a README is a bad
place to guess at either. Concrete parameters are governance values, set at
deployment and adjustable on chain.

### Flows

```
  consumers ──── deposit ────▶ aggregator ──── get_price_metered ────┐
                                   │                                 │
                                   │  read_fee, accrued per read     │
                                   ▼                                 │
                              forward_fees ──▶ registry reward pool ◀┘
                                                           │
  registry ◀──── record_success(node, reward) ─────────────┘
      │
      ├── in-band submission   ▶  +reputation, reward paid from the pool
      ├── out-of-band          ▶  −reputation, stake seized into the slash pool
      └── missed round         ▶  −reputation, no stake seized
```

Fees are accrued per read and forwarded to the reward pool in a batch, because
a token transfer on every read would cost more than the read it is charging
for. `forward_fees` is permissionless: anyone may pay to move money towards the
people producing the data.

The free reads stay free. A read simulated by a consumer costs the network
nothing and cannot be billed anyway; metering is what a consumer contract opts
into when it wants its usage to fund the operators it depends on. Until enough
consumers do, the pool is funded by whoever calls `fund_rewards`, and an empty
pool skips payment rather than blocking consensus.

### Node incentives

| Event | Reputation | Stake |
| --- | --- | --- |
| Submission inside the consensus band | +50 | Reward paid, if the pool can cover it |
| Submission outside `max_deviation_bps` | −500 | `outlier_slash` seized |
| Missed round | −25 | Untouched |
| Dispute resolved against the node | Governance-set | Governance-set |

Two properties are deliberate. **Recovery is slower than defection**: one bad
round costs what ten good ones earn, so a node cannot profitably alternate
between honest rounds and opportunistic ones. And **downtime is not theft** — a
server outage erodes weight but never seizes stake, because punishing honest
operators for hardware failure drives away exactly the people the network needs.

An empty reward pool skips payment but never blocks consensus: a round that
cannot pay is still a round that produced a correct price.

### Exit and accountability

Unbonding is not instant. A node that requests an exit stops voting immediately,
but its stake stays locked for the unbonding period — the window in which a
dispute over its past submissions can still reach it. Publishing a bad price and
withdrawing before anyone notices is not an available strategy.

---

## Disputes

The aggregator penalises what it can prove arithmetically: a submission outside
the band around a median it computed itself. That covers a node lying about a
price in a round it took part in. It does not cover collusion across rounds, a
stolen key, or a venue manipulated on purpose, because none of those are
visible in one round's arithmetic.

The slashing contract is where those are decided by people, on evidence, with
money at stake on both sides.

```
  open_dispute ──▶ vote (committee) ──▶ resolve ──▶ [appeal] ──▶ settle
     bond posted      one vote each      majority     larger      stake moves
                      voting period      or dismissed  bond        or bond does
```

| Step | Who | Cost of being wrong |
| --- | --- | --- |
| `open_dispute` | Anyone | Bond forfeited to the operator if dismissed |
| `vote` | A committee member | — |
| `resolve` | Anyone, after the voting period | — |
| `appeal` | Either side, once | Bond forfeited unless the outcome changes |
| `settle` | Anyone, after the appeal window | — |

Four properties are deliberate:

- **It fails closed.** A dispute that does not reach voting quorum is
  dismissed, and a tie favours the accused. Silence from the committee is not
  evidence against an operator — a rule that treated it as such would let an
  attacker slash a node by making sure nobody was watching.
- **The reward is not paid by the seizure.** Upheld disputes move stake into
  the registry's slash pool, and the reporter is paid *from that pool* in a
  separate step. If the reward were carved out of the penalty, a committee
  would hold a standing financial interest in finding against nodes, scaling
  with the size of the penalty.
- **Both sides post a bond.** Without one, a competitor can bury an operator in
  disputes for free; an appeal costs more, because it asks the whole committee
  to do its work a second time.
- **An allegation may be filed once**, identified by `(node, feed, round)`.
  Otherwise one offence becomes any number of seizures by re-filing after each
  settlement.

An operator may not vote on a dispute against their own node, and the committee
cannot be shrunk below the quorum it has to reach — which would dismiss every
dispute against anybody and switch slashing off without anyone appearing to
decide it.

The committee is admin-managed at launch. That is a governance posture, not an
end state: it is the part of Aphelion that is least decentralised today, and
naming it here is more useful than describing it as something it is not.

---

## Roadmap

| Phase | Scope |
| --- | --- |
| **1 — Foundation** ✅ | Core math and signing payload · node service · registry contract · aggregator contract |
| **2 — Integration** *(current)* | Slashing contract ✅ · consumer-example ✅ · on-chain Byzantine simulation ✅ · multi-node harness · testnet deployment |
| **3 — Hardening** | Dispute governance beyond an admin-managed committee · Grafana dashboards ✅ · external review |
| **4 — Launch** | Mainnet deployment with conservative parameters · recruit independent operators · first dApp integrations |
| **5 — Expansion** | Additional feeds · verifiable randomness · non-price data · parameter governance |

Phase boundaries are gated on the work being done, not on a date.

---

## Development

```bash
cargo test --workspace                              # off-chain
cargo test --manifest-path contracts/Cargo.toml     # contracts
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --manifest-path contracts/Cargo.toml --all-targets -- -D warnings
cargo fmt --all && cargo fmt --manifest-path contracts/Cargo.toml --all

# Build every contract for the ledger
scripts/build-contracts.sh

# Regenerate the shared vectors (must be committed with any change to the
# signing layout or the aggregation maths)
python3 scripts/gen_test_vectors.py
```

Contracts build for `wasm32v1-none`, not `wasm32-unknown-unknown`. Since Rust
1.82 the latter enables reference-types and multi-value, which the Soroban
environment does not accept and which cannot easily be disabled; `soroban-sdk`
refuses to build for it. If a build script or CI job still names the old
target, that is why it stopped working.

### Testing philosophy

Tests here are written to document behaviour that matters, not to reach a
coverage number. The ones worth reading first, because they encode the design
decisions rather than the plumbing:

- `aphelion-core::math` — that one outlier cannot move a median, and that low
  reputation genuinely costs influence
- `aphelion-node::engine::aggregate` — that a broken exchange is excluded with a
  recorded reason, and that too few survivors means publishing nothing
- `aphelion-node::signer` — that a testnet signature does not verify on mainnet
- `aphelion-registry` — that the jail threshold is exact, that capital cannot buy
  back reputation, and that downtime never seizes stake
- `aphelion-aggregator::test` — that weight is captured when a vote is cast and
  not re-read at finalisation, that an outlier votes in the median but appears
  in no published statistic, and that a round nobody agreed on publishes nothing
- `aphelion-aggregator::test::byzantine` — the multi-round properties: liars
  are shed and the network keeps working, fresh identities cannot outvote proven
  ones, and undoing one dishonest round takes ten honest ones
- `aphelion-slashing` — that a dispute nobody voted on is dismissed rather than
  upheld, and that a failed appeal pays the side it dragged back
- `aphelion-consumer-example` — that a single-round crash cannot liquidate a
  solvent borrower, and a sustained one can
- `aphelion-node` integration `multi_node` — that the median a node predicts
  locally is the median the network publishes, and that one node's signature
  authorises nothing under another node's key

### Contributing

Contributions are welcome. Please open an issue before starting substantial
work, keep changes focused, and include tests that would fail without the change.

Any change to the signing payload, the storage layout, or a contract interface is
consensus-breaking and needs explicit discussion first — regenerate the shared
test vectors in the same commit.

---

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).

## Acknowledgements

Built on [Stellar](https://stellar.org) and [Soroban](https://developers.stellar.org/docs/build/smart-contracts).
The consensus and staking design draws on lessons from the wider oracle
ecosystem, Chainlink's in particular.
