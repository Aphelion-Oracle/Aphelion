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

Jail is a floor, not a grave: a jailed node serves a fixed term and is then
released to a newcomer's standing. See [Jail and release](#jail-and-release).

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

The timestamp a node signs is the age of the **data**, not of the round: the
oldest observation that actually contributed to the price, clamped to never run
ahead of ledger time. Oldest, because a freshness check has to be answerable by
the weakest input — one fast venue must not make four stale ones look current.
Clamped, because a node with a fast clock would otherwise sign a timestamp in
the ledger's future, which the aggregator rejects as drift.

*Contributed* is the load-bearing word. A source discarded as an outlier is not
part of what was published, so it does not date it. This matters because the two
failures arrive together: a frozen venue reports an hour-old price and gets
discarded for being wrong, in the same round, for the same underlying reason.
Letting the discarded observation drag the signed timestamp backwards would
understate the freshness of a price it played no part in — and past the
aggregator's `max_staleness` it costs the node the entire submission: a rejected
transaction, a burned nonce and a missed round, all for the age of data the
round never used.

This is the same rule the aggregator applies one level up, where `PriceData`
carries the oldest contributing *node*'s timestamp. Both layers date a result by
its weakest surviving input, and neither counts anything it threw away.

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
| `aphelion-node` — sources, collector, round loop, signer, HTTP API, CLI | ✅ Implemented | 118 |
| `aphelion-registry` contract — identity, stake, reputation, jail, slashing accounting | ✅ Implemented | 35 |
| `aphelion-aggregator` contract — consensus, TWAP, metering, absence sweeps | ✅ Implemented | 52 |
| `aphelion-slashing` contract — disputes, committee voting, appeals, elections | ✅ Implemented | 60 |
| `aphelion-governance` contract — timelocked proposals, guardian veto, self-amendment | ✅ Implemented | 30 |
| `consumer-example` contract — reference dApp integration | ✅ Implemented | 17 |
| On-chain Byzantine simulation — multi-round adversarial scenarios | ✅ Implemented | 6 |
| Multi-node simulation — several signers against one in-memory network | ✅ Implemented | 10 |
| Absence sweeps — a node that charges the silence nobody else is charging | ✅ Implemented | 11 |
| Committee participation — disputes and elections, from the node | ✅ Implemented | 9 |
| Multi-process harness — several node *processes* against one deployment | ✅ Implemented | 15 |
| `verify-deployment.sh` — reads a live deployment back and checks it | ✅ Implemented | 34 |
| Testnet deployment | 📋 Planned | — |
| Mainnet deployment | 📋 Planned | — |

Legend: ✅ implemented and tested · 🚧 in progress · 📋 planned

423 tests in total: 189 off-chain (`cargo test --workspace`), 200 against the
contracts (`cargo test --manifest-path contracts/Cargo.toml`) and 34 against the
deployment verifier (`tests/deployment/run.sh`, no cargo and no network). The
Byzantine simulation's 6 tests live inside the aggregator crate, so its 52 and
their 6 are reported as one figure of 58 by `cargo test`. The absence sweep's 11
are its own integration suite; the decision it makes has a further 13 unit tests
counted inside the node's 118. Committee participation is the same shape: its 9
cover assembling a snapshot off the chain, and the rules applied to that
snapshot have 25 more unit tests, with 12 on decoding what the contract returns
and 5 on the commands — all four counted inside the 118. The harness's 15 are 12
process-level tests plus 3 covering the fake CLI's argument parsing.

Be aware of what the 12 do without a database: they skip, and a skipped Rust
test still reports as **passed**. A green `cargo test --workspace` on a machine
with no Postgres has run 129 tests and reported 141. The skip prints a `SKIP`
line, but `cargo test` swallows it unless you pass `--nocapture`, so treat the
harness as covered only where it is actually given a database — which is what
the `harness` job in CI is for.

The Byzantine simulation runs the real registry and aggregator together across
multiple rounds with a mix of honest and dishonest nodes. The multi-node
simulation does the same on the off-chain side, running several independently
keyed signers against one in-memory network whose median is computed by the
same function the contract mirrors.

Neither covers several node *processes*, and that is what the multi-process
harness adds: it runs the shipped `aphelion-node` binary several times over, each
copy with its own Postgres database, its own key, its own HTTP port and its own
subprocess calls to the chain, all pointed at one deployment. See
[Multi-process harness](#multi-process-harness).

---

## Repository layout

```
aphelion/
├── contracts/                  Soroban contracts (separate cargo workspace, wasm target)
│   ├── registry/               Node identity, stake, reputation, slashing accounting
│   ├── aggregator/             Submission verification, consensus, price storage, TWAP
│   ├── slashing/               Dispute resolution, and the committee's elections
│   ├── governance/             Timelock: every privileged call, queued and published first
│   └── consumer-example/       Reference integration for dApp authors
├── crates/                     Off-chain services (root cargo workspace, host target)
│   ├── aphelion-core/          Shared price math and the canonical signing payload
│   ├── aphelion-harness/       Multi-process test harness (not shipped)
│   └── aphelion-node/          The node binary
│       └── src/
│           ├── sources/        Binance, Kraken, Coinbase, CoinGecko
│           ├── engine/         Collector, aggregation, round loop, absence sweeps, duties
│           ├── chain/          ChainClient and CommitteeClient: CLI-backed, RPC reads, mock
│           ├── cmd/            Subcommands belonging to the binary rather than the library
│           ├── db/             Postgres schema access
│           └── api/            Read-only HTTP surface
├── migrations/                 SQL migrations, applied automatically at startup
├── tests/
│   ├── vectors/                Cross-implementation signing and aggregation vectors
│   └── deployment/             A fixture deployment, for the verification script
├── scripts/                    Deployment, verification, governance, registration, vectors
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
- `jq`, for `scripts/deploy.sh` and `scripts/govern.sh`

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
export APHELION_ADMIN_ACCOUNT="G..."    # configures the contracts, then hands them over
export APHELION_PROPOSERS="G... G..."   # may queue proposals afterwards
export APHELION_GUARDIAN="G..."         # may veto one, and may do nothing else
export APHELION_GUARDIAN_SECRET="S..."  # the guardian signs its own initialize
scripts/deploy.sh
```

The core contracts refer to each other, so all of them are deployed before any
of them is initialised — deploying is what fixes an address, initialising is
what teaches each contract the others'. That is why `initialize` is a separate
call rather than a constructor. Every parameter is an environment variable with
a conservative default, the whole plan is printed and confirmed before anything
is submitted, and the result is written to `deployments/<network>.json`.

The **last** thing the script does is hand the registry, the aggregator and the
slashing contract to the governance timelock it just deployed. Everything above
is configured by the admin key acting in single transactions, because a
deployment that had to serve a day's delay to add its first feed would never
finish; from that point on the same changes are proposals — published, delayed,
and executable by anyone. Set `APHELION_SKIP_HANDOVER=1` to keep the admin key
instead: reasonable while iterating on a throwaway deployment, wrong for one
anybody relies on.

Leave `APHELION_GUARDIAN` unset and it defaults to the admin account, which
gets you none of the separation the guardian is for — a key whose only power is
to say no is worth nothing held in the same hands as the key that says go. The
script says so at the end if that is how it was run.

### Checking what was actually deployed

`deploy.sh` writes a record of what it believes it did. Nothing in that record
is read back from the chain, and the gap matters most where the script itself
says so: the handover at the end is three independent one-way calls, so a
failure part-way through leaves the admin key holding whichever contracts it
did not reach — a deployment that looks finished, prints no error, and is still
governed by one key.

[`scripts/verify-deployment.sh`](scripts/verify-deployment.sh) reads the
deployment back and checks it against what it was meant to be:

```bash
export APHELION_STELLAR_SECRET="S..."   # any funded account; it signs nothing
scripts/verify-deployment.sh            # 0 if nothing failed, 1 otherwise
scripts/verify-deployment.sh --strict   # warnings are failures too
```

It reports on four things: that every contract answers and points at the
others, that admin is the timelock rather than a key, that the parameters
which constrain each other are jointly sensible, and whether the network can
currently produce a price at all. That last one is not correctness — a correct
deployment with no operators publishes nothing, and from a consumer's side the
two look identical.

Every call it makes is simulated: it submits nothing, signs nothing and costs
nothing, so it is as safe to point at somebody else's deployment as at your
own. `--strict` is the mainnet gate; plain is right for a fresh testnet
deployment, which legitimately warns that it has no nodes yet. Run it after
deploying, before announcing a deployment to operators, and after any proposal
that changes a parameter.

### Changing a parameter afterwards

Once the handover is done, every admin call is a proposal.
[`scripts/govern.sh`](scripts/govern.sh) reads `deployments/<network>.json`, so
no contract id has to be retyped between queueing one and executing it a day or
more later:

```bash
export APHELION_STELLAR_SECRET="S..."      # signs; also simulates the reads
export APHELION_PROPOSER_ACCOUNT="G..."    # must be a proposer, and own that seed

# Queue it. The arguments are a JSON array, in the order the function takes them.
scripts/govern.sh propose registry set_min_stake '["20000000000"]' ipfs://bafy...

scripts/govern.sh list                     # what is queued, and its state
scripts/govern.sh show 7                   # the exact call, and when it lands

# ...once the delay is served, from any account at all:
scripts/govern.sh execute 7
```

`propose` prints the call, the target it resolves to and the dates it will
become executable and expire, then asks before submitting. `execute` is
permissionless — the call was fixed when it was queued and the delay is a fact
about the clock, so there is nothing left for the sender to decide. The
guardian, and only the guardian, can `cancel` somebody else's.

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
#    Reads are live -- real ledger time, your real registry record -- so what
#    it reports is the deployment you are pointed at; only submission is
#    refused, one layer below the code that decides whether to submit.
cargo run -p aphelion-node -- run --dry-run

# 5. Check what the slashing contract is asking of you. Reads only; no
#    database needed, and safe to run before registering -- an unregistered
#    key simply has nothing outstanding.
cargo run -p aphelion-node -- duties

# 6. Register on chain, then run for real.
scripts/register-node.sh "$(cargo run -q -p aphelion-node -- pubkey)"
cargo run --release -p aphelion-node -- run
```

Health, once running:

```bash
curl -s localhost:8080/health          | jq   # per-feed liveness; 503 when degraded
curl -s localhost:8080/v1/prices/BTC_USD | jq # local price, on-chain price, per-source breakdown
curl -s localhost:8080/v1/duties       | jq   # disputes and elections outstanding, with deadlines
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
| `sweep [--commit]` | Show which registered nodes have gone silent; charge them with `--commit` |
| `duties [--json]` | What the slashing contract is waiting on from this operator, and by when |
| `election show \| open \| nominate \| ballot \| finalize` | The committee's elections |
| `dispute list \| show \| open \| vote \| resolve \| appeal \| settle` | Disputes |
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

A node with no weight — unknown, jailed or exiting — is skipped rather than
charged, and a jailed one has its clock advanced as it is skipped. Its silence
is the penalty already running, so billing it again on release would charge it
twice for one absence, in the first moment it was allowed to speak.

Who actually calls it is [a separate question](#who-charges-an-absence), and for
a while the answer here was nobody.

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
slashing_contract   = "C..."   # optional; without it, disputes go unreported
submitter_account   = "G..."
submitter_secret_env = "APHELION_STELLAR_SECRET"   # the name, not the secret
# operator_account    = "G..."   # the account that bonded the stake, if not the
# operator_secret_env = "..."    # submitter. Committee actions are signed by it.

[engine]
round_interval           = "60s"   # must match the aggregator's min_round_interval
poll_interval            = "10s"
max_observation_age      = "120s"
min_sources_per_feed     = 2       # sign nothing below this
max_source_deviation_bps = 1000    # discard a venue more than 10% off the median
submit_deviation_bps     = 25      # publish on a 0.25% move...
heartbeat                = "300s"  # ...or every 5 minutes regardless
max_clock_skew           = "30s"   # refuse to sign beyond this drift from ledger time

[upkeep]
sweep_absent = false   # charge silent nodes the missed round anyone may charge
interval     = "30m"
max_batch    = 25

[committee]
watch_interval = "15m"  # re-read the slashing contract this often. Reads only.
scan_depth     = 50     # how many disputes back from the newest each pass reads

[[feeds]]
id             = "BTC_USD"
confidence_bps = 50
sources        = { binance = "BTCUSDT", kraken = "XBTUSD", coinbase = "BTC-USD" }
```

Two settings are environment-only, because they describe where a particular
process happens to run rather than what the deployment is, and a config file
copied between machines should not carry them: `APHELION_STELLAR_BIN` names the
Stellar CLI to shell out to when it is not `stellar` on `PATH`, and
`APHELION_SOURCE_URL_<VENUE>` — `APHELION_SOURCE_URL_BINANCE`, and so on —
points a venue at a cache or regional proxy you run yourself instead of its
public endpoint. Both are also the seams the
[multi-process harness](#multi-process-harness) uses to replace the outside
world without modifying the binary.

Configuration is validated at startup, and a config that could never work is a
startup failure rather than a silent runtime one — a feed mapping fewer sources
than `min_sources_per_feed`, a feed pointing at a disabled source, a round
interval shorter than the poll interval, a `max_observation_age` at or below
`poll_interval`, an account address where a contract address belongs, an upkeep
interval faster than the round loop.

That last one is the least obvious and the worst to debug in production. An age
limit no longer than one poll discards data the collector is still working on:
the freshest observation is already a full interval old the instant before it is
replaced, so every source reports, every observation is stored, and every round
finds nothing it is allowed to use. The result is a starved feed whose venues
are all visibly healthy. The check is a floor rather than a recommendation —
one failed poll doubles the age of the freshest observation — so a usable
setting is several intervals above it, as the `10s` / `120s` defaults above are.

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
| `GET /v1/upkeep` | Which registered nodes read as absent, and what a sweep would charge |
| `GET /v1/duties` | Disputes and elections outstanding against this operator, with deadlines |

`/health` reports **usefulness**, not just liveness: a process that is running
but has not composed a round in ten minutes is not healthy in any sense an
on-call engineer cares about, so it returns 503 and pages somebody.

It reaches that verdict by running the real aggregation, not by counting rows.
Enough live sources is not the same as enough *usable* ones — four venues that
disagree past `max_source_deviation_bps` are four healthy HTTP endpoints and no
publishable price — so each feed reports both numbers:

```json
{
  "feed": "BTC_USD",
  "live_sources": 3,
  "usable_sources": 1,
  "required_sources": 2,
  "seconds_since_last_submission": 412,
  "healthy": false,
  "reason": "feed `BTC_USD` has 1 usable source(s), need at least 2"
}
```

`live_sources` counts venues that answered inside `max_observation_age`;
`usable_sources` counts those that then survived outlier filtering. A gap
between them points at disagreement rather than at an outage, which is a
different call to make at 3am. When a feed is both short of sources *and*
overdue, the `reason` names the sources: they are the cause and the silence is
the symptom, and reporting the symptom first sends the operator to the chain to
debug a problem that lives at the exchanges.

### Metrics

| Metric | Meaning |
| --- | --- |
| `aphelion_source_fetches_total{source,outcome}` | Fetch attempts by venue and outcome |
| `aphelion_source_latency_seconds{source}` | Per-venue fetch latency |
| `aphelion_source_price{source,feed}` | Latest price seen per venue |
| `aphelion_source_spread_bps{feed}` | Highest-to-lowest source spread — the best early warning available |
| `aphelion_local_price{feed}` | This node's aggregated price |
| `aphelion_rounds_total{feed,outcome}` | Rounds by outcome — `submitted`, `skipped_unchanged`, `skipped_no_data`, `dry_run`, `failed` |
| `aphelion_round_errors_total{kind[,feed]}` | Failed rounds by error kind — see the note below |
| `aphelion_round_duration_seconds` | Wall time from opening a round to submitting or skipping it |
| `aphelion_submissions_total{feed,outcome}` | On-chain submissions by outcome |
| `aphelion_clock_skew_seconds` | Node clock minus ledger clock |
| `aphelion_reputation`, `aphelion_stake` | On-chain standing |
| `aphelion_seconds_since_submission{feed}` | Time since this node last landed a price |
| `aphelion_registry_nodes` | Nodes in the registry, as this node last read it |
| `aphelion_sweep_candidates` | Registered nodes this node currently reads as absent |
| `aphelion_sweeps_total{outcome}` | Sweep passes by outcome — `submitted`, `nothing_to_do`, `error` |
| `aphelion_nodes_charged_total` | Missed rounds this node has actually charged |
| `aphelion_duties_outstanding{consequence}` | Things the slashing contract is waiting on from this operator |
| `aphelion_duty_deadline_seconds` | Seconds to the soonest deadline that can cost stake; negative once one has closed |

`aphelion_round_errors_total` carries a `feed` label only when a feed is to
blame. Two failures abort the whole tick before any feed is reached — an
unreadable ledger time, and a clock too far from it — and those are labelled by
`kind` alone, because no feed caused them. Aggregate with `sum by (kind)`; a
`sum by (feed)` drops them into an empty bucket. The same two paths increment no
`aphelion_rounds_total` at all, so a node with a bad clock shows as *silence* on
a round-outcome panel rather than as failures — which is exactly why the error
counter and `aphelion_clock_skew_seconds` are the ones worth alerting on.

`aphelion_sweep_candidates` is worth a panel on any network, whether or not
this node is the one sweeping: it is the amount of voting weight currently being
carried by nodes that have stopped earning it, which is a property of the
network rather than of this process. A number that climbs and never falls means
nobody is calling `sweep_absent`.

`aphelion_duties_outstanding` is labelled by what going undone costs, and the
label is the point: `costly` is stake or a finding that could have been
contested, `forfeited` is a vote not cast, `owed` is money sitting unsettled,
`housekeeping` is work anybody may do. One number covering all four would be
tuned for whichever is most common — which is the routine one — and would then
be too quiet for the one that matters. `aphelion_duty_deadline_seconds` is
absent rather than zero when there is nothing costly outstanding, so a
threshold rule on it does not fire permanently on a quiet network.

Suggested alerts: `aphelion_seconds_since_submission > 2 × heartbeat`,
`aphelion_clock_skew_seconds` outside ±30, `aphelion_reputation < 4000`,
`aphelion_source_spread_bps` sustained above the usual band,
`aphelion_sweep_candidates > 0` held for a couple of hours, and
`aphelion_duties_outstanding{consequence="costly"} > 0` with no `for:` at all.
All nine ship in
[`deploy/prometheus/alerts.yml`](deploy/prometheus/alerts.yml). Two are
deliberately not pages. `aphelion_sweep_candidates` is about other people's
nodes: nothing is broken at your end, and the weight it names is still
counting. An outstanding *vote* is a warning on a twelve-hour delay, because
nothing is taken from an operator who has not voted yet and a committee member
is entitled to think about it. A dispute against your own node is the
exception: it pages immediately, because the window it names does not reopen.

`docker compose up` also provisions a Grafana dashboard
([`deploy/grafana/provisioning/dashboards/node-overview.json`](deploy/grafana/provisioning/dashboards/node-overview.json))
at <http://localhost:3000>. Its panels are ordered by the question an operator
asks first: am I still counted, is what I publish right, if not then which venue
is at fault, how much weight is being carried by nodes that have stopped earning
it, and — last, because it is the one thing on the dashboard with a deadline
attached — what the slashing contract is waiting on from this operator. It is
provisioned read-only from the repository — a dashboard that exists only in one
operator's browser is one nobody else can reproduce when they are the person on
call.

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

### Who charges an absence

Every penalty above happens by itself except one. An outlier is caught by
arithmetic the aggregator was already doing to close the round; a dispute is
filed by whoever noticed. A **missed round** is different: `sweep_absent` is
permissionless, and permissionless is not the same as automatic. Until somebody
calls it, a node that stopped working keeps the weight it earned while it was
working, and its last price goes on counting towards the median for as long as
nobody bothers.

That is a real hole and not a theoretical one. A dead node's weight is not
merely stale — it is the difference between a median taken over the nodes that
are working and a median taken over the nodes that were.

The node closes it, opt-in, through `[upkeep]`:

```toml
[upkeep]
sweep_absent = true
interval     = "30m"
max_batch    = 25
```

Why an operator would turn it on is that weight is relative. A feed's median is
taken over whoever turns up, so every basis point a dead node still carries is a
basis point the live ones do not have, and every reward paid to a round it did
not join is smaller than it should be. Sweeping is a small fee to reduce a
competitor's weight to what it has recently earned.

Why it is **off by default** is that it is still a fee for a call that pays
nothing back directly. An operator who did not ask to spend on network upkeep
should not discover that they have been.

Four properties, and the reasoning behind each:

- **A node never sweeps itself.** Paying a fee to take reputation off your own
  node is not something anyone wants, and the symmetry is what makes leaving it
  out honest rather than self-serving: every other operator has exactly the
  reason to sweep you that you have to sweep them. Your own absence is somebody
  else's to charge, and on a network where more than one operator runs this
  loop, somebody will.
- **One silence is charged once,** however many operators sweep it. That
  protection is the contract's `Swept` marker, not the node's own memory — a
  network of ten sweepers must not bill one absence ten times, and a rule that
  depended on each caller behaving would not be a rule.
- **The node's view is deliberately the more pessimistic one.** It reads the
  registry's `last_submission`, which moves only when a round the node joined
  actually closed; the aggregator decides from the last submission it
  *accepted*, closed round or not. So a node whose rounds keep falling short of
  quorum can look absent from outside and not be absent to the contract. The
  contract re-checks every key and declines the ones it should, which makes the
  error safe — but not free, because the fee is paid either way. `sweep_absent`
  returns how many keys it charged, so the gap between offered and charged is
  reported rather than swallowed, and a key that was declined is not offered
  again inside the same absence window.
- **A batch is bounded and the overflow waits.** `max_batch` is capped at 50,
  because one sweep reads a registry record per key and a transaction that
  exceeds its resource budget fails as a whole — charging nobody and costing the
  fee anyway. Candidates beyond the limit are carried to the next pass, longest
  silence first.

Nothing here needs to be running to inspect it. Both of these read the chain and
submit nothing:

```bash
aphelion-node sweep                       # who is absent, and what it would cost
curl -s localhost:8080/v1/upkeep | jq     # the same plan, from a running node
aphelion-node sweep --commit              # charge them, once
```

And `scripts/verify-deployment.sh` reports the network-level version of the same
question — how much weight is currently held by nodes that have stopped earning
it. That figure is not a fault in the contracts; it is the measure of whether
anybody is doing this upkeep at all.

### Jail and release

A node whose reputation falls below 3 000 is **jailed**: still bonded, still on
the record, but carrying zero voting weight.

Zero weight is absolute. The aggregator refuses a zero-weight submission
outright, so a jailed node cannot submit, cannot be recorded as successful, and
cannot earn reputation back. "Recover by behaving" is not a path that exists for
it — and neither is buying its way out, because jail begins exactly when
reputation falls below the threshold, so no amount of stake can satisfy a
condition written on reputation.

What clears jail is time:

```
  reputation < 3 000  ─▶  Jailed, weight 0, jailed_until = now + jail_period
                                    │
                       ( term runs; the stake stays bonded )
                                    │
                          release() ─▶  Active again, reputation 5 000
```

`release` is permissionless — it only checks facts on the ledger, not
judgements — and requires three of them: the node is jailed, the term is served,
and the bond is still at or above the minimum. A node slashed below the minimum
must top up before it can return, because coming back under-bonded would mean
voting with less at risk than the network requires of everyone else.

Release restores **exactly a newcomer's standing**, 5 000, and no more. That
number is forced from both sides:

- Anything **lower** would be pointless. An operator can always unbond,
  withdraw, and register a fresh key at 5 000. If release returned less than
  that, nobody would use it, and the network would churn identities — discarding
  the history a dispute might need — for no benefit.
- Anything **higher** would make jail cheaper than being new, which is the wrong
  way round.

So the price of jail is the wait, plus the loss of everything earned above a
newcomer's standing. The stake stays bonded throughout, which is the point: the
operator remains reachable for the whole term.

Two bounds make `jail_period` a real penalty rather than a formality, and a
deployment that violates either has misconfigured itself:

| Bound | Why | Enforced by |
| --- | --- | --- |
| Longer than the climb from 3 000 back to 5 000 (40 in-band rounds) | Otherwise dipping below the line is a shortcut, not a punishment | `scripts/deploy.sh` |
| Shorter than `unbonding_period` | Otherwise unbond-and-re-register is the faster way back, and nobody ever serves the term | `Registry::initialize` |

The defaults — a one-day term against a seven-day unbonding period — satisfy
both comfortably.

The split is deliberate. The second bound is a fact about two numbers the
registry already holds, so the registry checks it: `initialize` panics with
`InvalidConfig` on a `jail_period` at or above `unbonding_period`, and an
operator can verify that for themselves before bonding stake, without trusting
whatever script the deployer happened to run. The first bound depends on the
aggregator's round cadence, which the registry cannot read, so it stays in the
deployment script. `scripts/deploy.sh` checks both — the contract is the
authority on the second, but a refusal that arrives as a sentence about
incentives beats contract error #14 surfacing from inside a transaction the
operator has already paid for.

Unlike most parameters, `unbonding_period` and `jail_period` cannot be changed
after `initialize`. Both are promises made to operators who bonded stake under
them, and a governance key able to extend a jail term retroactively would be a
governance key able to expropriate.

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

### Who is on the committee

The committee is elected by the operators, weighted by the same `weight_of`
that decides how much a node's price counts towards the median. A deployment
appoints its first one — an election needs an electorate, and at genesis there
are no nodes — and that committee serves a term like any other before the
operators replace it.

```
  open_election ──▶ nominate ──▶ cast_ballot ──▶ finalize_election
   anyone, once      an operator,   one ballot      anyone, once the
   the term is       on a node      per node,       ballot closes;
   served            they own       weighted        seats the top `seats`
```

Four properties again, and the same shape of reasoning behind them:

- **The electorate is the node set.** The committee's power is to take an
  operator's stake, so the choice of who holds it belongs to the people whose
  stake is at risk. Weight is already the network's Sybil-resistant answer to
  how much an identity should count: a fresh one starts at half weight and a
  jailed one is worth nothing.
- **One ballot names one candidate**, in a race with several winners. A slate
  ballot would let a bare majority of weight take *every* seat; naming one
  means a faction holding more than `1/(seats+1)` of the weight can seat
  somebody no matter who else votes. The committee that judges operators
  should not be winnable outright by whoever is largest this quarter.
- **A failed election changes nothing.** Fewer eligible winners than the quorum
  and the sitting committee stays. Vacating the seats would let an attacker
  switch slashing off for everybody by suppressing turnout — the same
  fail-open the quorum rule exists to refuse. The cost is real and worth
  stating plainly: a committee nobody replaces holds over indefinitely.
- **Governance can remove a member and cannot seat one.** There is no
  `add_member`. That asymmetry is the guardian's, one level down: a power that
  can only subtract cannot install anybody, so the worst a captured timelock
  achieves is a smaller committee — and it cannot shrink one below its quorum
  either.

Two things this is not. The committee is drawn from operators, so it is
operators judging operators; the conflict-of-interest rule and a reward paid to
the reporter rather than the committee bound that, but they do not remove it.
And every seat turns over at once, which is simple to reason about and loses
the continuity a staggered committee would keep. Both are worth revisiting
against a real network rather than in advance of one.

### Taking part

Everything above is permissionless on chain, which is not the same as
reachable. A franchise nobody can exercise is not a franchise, and an appeal
window nobody is told about is a penalty by default — so the node watches the
slashing contract on the operator's behalf and reports what it finds.

```console
$ aphelion-node duties
node       : 608dd6533ec133907390ad8dd4c9ecdb256ebef2db914be9a6e48801dc1e7227
owner      : GOPERATOR...
signing as : GOPERATOR...
weight     : 7500 bps
committee  : seated

[COSTLY] dispute 2 against this node (BTC_USD round 91); 1 for, 0 against,
         quorum 3. Evidence: ipfs://bafyevidence
    10h left
    aphelion-node dispute show 2
[FORFEITED] election 4 is balloting for 5 seats until 1700090000; this node's
            7500 bps has not been cast
    24h left
    aphelion-node election ballot <candidate>

1 that can cost stake, 1 that can cost a say. Deadlines are ledger time, not
this machine's clock.
```

A running node does the same pass every `committee.watch_interval` and
publishes it three ways: a log line, `GET /v1/duties`, and
`aphelion_duties_outstanding`. That is what makes it an alert rather than
something an operator has to remember to check — see
[Metrics](#metrics). It needs `slashing_contract` in the `[network]` section;
without it the node says so at startup and `/v1/duties` answers
`watching: false` rather than an empty list, because "nothing is outstanding"
and "nobody is looking" are different facts and only one of them is reassuring.

Duties are sorted by what going undone costs, which is also how they are
labelled:

| Label | Meaning | Example |
| --- | --- | --- |
| `COSTLY` | Stake, or a finding that could have been contested. A window closes and does not reopen | A dispute against this node; an open appeal window |
| `FORFEITED` | A say this operator is entitled to, spent by not using it | A committee vote; a ballot in a running election |
| `OWED` | Money already owed, sitting until somebody moves it. No deadline | A dismissed dispute whose bond is due to you |
| `HOUSEKEEPING` | Nothing, to you. The network needs it and anybody may do it | Counting a closed ballot; opening an election after a served term |

Acting on one is always a separate command, run on purpose:

| Command | What it does |
| --- | --- |
| `dispute list` / `dispute show <id>` | The allegation, the evidence link, the votes and the clock |
| `dispute open <accused> <feed> <round> <evidence> --commit` | File, posting the bond |
| `dispute vote <id> --uphold\|--dismiss` | A committee vote |
| `dispute resolve <id>` | Record the outcome once voting has closed |
| `dispute appeal <id> --commit` | Contest a resolved dispute, posting the larger bond |
| `dispute settle <id>` | Move the money once the appeal window has closed |
| `election show` | Phase, deadlines, candidates, and whether this node has voted |
| `election open` / `election finalize` | Open one after a served term; count a closed ballot |
| `election nominate` | Stand for a seat, on the strength of this node |
| `election ballot <candidate>` | Cast this node's weight |

**Nothing here votes on anybody's behalf.** A node that cast committee votes on
a schedule would be a committee seat held by a cron job, which is the failure
the elected committee exists to avoid; a node that appealed automatically would
spend the appeal bond on every dispute it ever lost. The one thing automation
is good for here is noticing, and that is all the watcher does. The two
commands that move the operator's own money — `dispute open` and
`dispute appeal` — print the bond and send nothing until `--commit`, the same
shape as `sweep`.

One configuration trap is worth naming, because the contract's answer to it is
`NotEligible` and that does not distinguish it from a jailed node. Committee
actions are authorised by the account the registry answers `owner_of` with —
the one that bonded the stake — and the submitter deliberately holds no
authority over the node's identity. Where the two differ, set
`operator_account` and `operator_secret_env`. `duties` prints the account it
would sign as and warns when it is not the one that bonded the stake, and every
refusal repeats it.

---

## Roadmap

| Phase | Scope |
| --- | --- |
| **1 — Foundation** ✅ | Core math and signing payload · node service · registry contract · aggregator contract |
| **2 — Integration** *(current)* | Slashing contract ✅ · consumer-example ✅ · on-chain Byzantine simulation ✅ · multi-process harness ✅ · deployment verification ✅ · testnet deployment |
| **3 — Hardening** | Governance timelock over every admin action ✅ · Grafana dashboards ✅ · a dispute committee elected rather than appointed ✅ · absence sweeps performed rather than merely permitted ✅ · disputes and elections an operator can actually reach ✅ · external review |
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

# The deployment verifier, against a fixture chain: no cargo, no network
tests/deployment/run.sh

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
- `aphelion-registry` — that the jail threshold is exact, that capital can buy
  back a bond but never a release, that serving the term returns a node to a
  newcomer's standing and no further, and that downtime never seizes stake
- `aphelion-aggregator::test` — that weight is captured when a vote is cast and
  not re-read at finalisation, that an outlier votes in the median but appears
  in no published statistic, and that a round nobody agreed on publishes nothing
- `aphelion-aggregator::test::byzantine` — the multi-round properties: liars
  are shed and the network keeps working, fresh identities cannot outvote proven
  ones, and undoing one dishonest round takes ten honest ones
- `aphelion-slashing` — that a dispute nobody voted on is dismissed rather than
  upheld, that a failed appeal pays the side it dragged back, that a ballot is
  worth what the node was worth when it was cast, that a candidate jailed
  during the ballot does not take the seat they were winning, and that an
  election too thin to fill its quorum leaves the sitting committee in place
  rather than vacating it
- `aphelion-governance` — that changing the delay takes the delay, that the
  window a proposal was queued under cannot be widened underneath it, that the
  guardian can stop a proposal and start nothing, and that the last proposer
  cannot be removed
- `aphelion-consumer-example` — that a single-round crash cannot liquidate a
  solvent borrower, and a sustained one can
- `aphelion-node::engine::upkeep` — that a node never offers its own key to a
  sweep, that a key the aggregator would decline is not paid for twice, and that
  the longest silence goes first when a batch has to be truncated
- `aphelion-node` integration `multi_node` — that the median a node predicts
  locally is the median the network publishes, and that one node's signature
  authorises nothing under another node's key
- `aphelion-node` integration `sweep` — that sustained absence actually jails a
  node and stops its vote counting, that one silence is charged once however many
  operators sweep it, and that a key the aggregator has seen more recently is
  declined and reported rather than assumed charged
- `aphelion-harness` integration `multi_process` — the failures that only exist
  between processes: a node dying without taking the network with it, an
  endpoint disappearing under a node that is otherwise healthy, and a clock
  drifting far enough from the ledger's that the node stops signing. Also that
  the price the chain carries actually tracks the venues, that one exchange
  printing a bad tick is absorbed rather than published, that a node left below
  `min_sources_per_feed` signs nothing until its venues return, and that one live
  node process charges a dead one for its silence while a node with upkeep
  switched off charges nobody

### Multi-process harness

Everything above runs the node's code in-process. The harness runs the shipped
`aphelion-node` **binary**, several copies of it, each with its own Postgres
database, its own signing key, its own HTTP port and its own subprocess calls to
the chain — all pointed at one deployment.

```bash
# Any Postgres will do; the harness creates and drops a database per node.
docker run -d --name aphelion-pg -p 5432:5432 \
  -e POSTGRES_USER=aphelion -e POSTGRES_PASSWORD=aphelion \
  -e POSTGRES_DB=aphelion postgres:16-alpine

APHELION_TEST_DATABASE_URL=postgres://aphelion:aphelion@localhost:5432/aphelion \
  cargo test -p aphelion-harness
```

Without that variable the suite **skips** rather than failing: someone who has
just cloned the repository should still get a green `cargo test`. Note what that
costs — a skipped Rust test reports as passed, and the `SKIP` line it prints is
swallowed unless you pass `--nocapture` — so these count as covered only where a
database is actually provided, which is why CI runs them in a job of their own
with a Postgres service attached.

What is real: the node binary, the collector, Postgres and the migrations, the
round loop, the signing, the process boundary, the `stellar` subprocess spawn,
and the rules that decide whether a submission is accepted — those are
`chain::mock::MockChain`, the same in-memory aggregator the single-process
simulation runs against, put behind an HTTP socket so several processes can
reach one instance of it. Reimplementing the acceptance rules for the harness
would only have tested the harness.

What is a fixture: the exchanges and the transport to the chain. Both are
swapped at seams an operator can already use — `APHELION_SOURCE_URL_<VENUE>`
and `APHELION_STELLAR_BIN` — so the binary under test is unmodified and has no
idea it is in a test. Nothing in `crates/aphelion-harness` is reachable from a
shipped node; that is why it is a separate crate.

Rounds are compressed to two seconds so the whole suite finishes in a minute or
two rather than an hour, and harnesses run one at a time: each is several processes and several
databases, and running six concurrently turns every timing assumption into a
fight for CPU that reads as a flaky node rather than an overloaded runner.

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
