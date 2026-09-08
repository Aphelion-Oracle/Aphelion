# Running an Aphelion node

This is the practical companion to the README: what to install, what to set, and
what to check when something is wrong. Read the README's *Security model* first
if you have not — it explains why several of the steps below refuse to proceed
rather than guessing.

> **Pre-alpha.** Aphelion is unaudited and testnet-only. Do not bond stake you
> are not prepared to lose to a bug.

---

## 1. Requirements

| | |
| --- | --- |
| CPU / RAM | 2 vCPU, 4 GB is comfortable for ten feeds |
| Disk | 20 GB, mostly Postgres; observation retention is configurable |
| Network | Outbound HTTPS to the exchanges and to a Soroban RPC endpoint |
| Software | Rust 1.91+, PostgreSQL 14+, [Stellar CLI](https://developers.stellar.org/docs/tools/developer-tools/cli/stellar-cli) |
| Clock | NTP, synchronised. This is not optional — see [Clock skew](#clock-skew) |

The node holds a signing key. Treat the host accordingly: no shared shell
access, no debug endpoints exposed publicly, and backups of the key stored the
way you would store any other production credential.

---

## 2. Two keys, on purpose

| Key | What it does | Where it lives |
| --- | --- | --- |
| **Ed25519 node key** (`node-key.json`) | Signs prices. **This is the node's identity**; the registry stores its public half and your stake is bonded against it. | On the node host, mode `0600` |
| **Stellar secret seed** (`S...`) | Pays transaction fees. Carries no authority over prices. | Environment variable |

They are separate because a signature, not a transaction, is what authorises a
price. Practically: you can rotate the funding account, share it with a relayer,
or run out of XLM in it, and none of that lets anyone publish a price as you.
Losing the *node* key, by contrast, means losing the identity your stake is
bonded to.

**Back up `node-key.json` before you register.** `keygen` refuses to overwrite an
existing file for the same reason.

---

## 3. Setup

```bash
# Build
git clone https://github.com/aphelion-oracle/aphelion.git
cd aphelion
cargo build --release -p aphelion-node

# Database
createdb aphelion
export DATABASE_URL="postgres://aphelion:aphelion@localhost/aphelion"

# Node identity — back this file up now
./target/release/aphelion-node keygen --out ./node-key.json

# Configuration
cp aphelion.example.toml aphelion.toml
$EDITOR aphelion.toml    # contract ids, submitter account, feeds

export APHELION_STELLAR_SECRET="S..."
```

Confirm the configuration is coherent before going any further:

```bash
./target/release/aphelion-node show-config
```

Startup validation is deliberately strict, because a configuration that could
never work should fail loudly at boot rather than quietly at 03:00: a feed
mapping fewer sources than `min_sources_per_feed`, a feed pointing at a disabled
source, a round interval shorter than the poll interval, or an account address
where a contract address belongs are all startup failures.

---

## 4. Verify before you bond

Three checks, in order. Each needs strictly less trust than the next.

```bash
# 1. Can this host reach the exchanges at all?
#    Needs no database, no chain access, no registration.
./target/release/aphelion-node check-sources
```

Every configured feed should print at least `min_sources_per_feed` prices that
agree to within a few basis points. If a venue fails here, fix that before
anything else — a node that cannot see the market cannot publish it.

```bash
# 2. What would this node actually publish?
./target/release/aphelion-node run --dry-run
```

Dry run composes and signs real rounds but cannot submit them; it is wired to an
in-memory chain client rather than trusted not to call a live one. Watch it for
a few minutes and compare `/v1/prices/{feed}` against a public price.

```bash
# 3. Register, then run for real.
export APHELION_REGISTRY_CONTRACT="C..."
export APHELION_OWNER_ACCOUNT="G..."
scripts/register-node.sh "$(./target/release/aphelion-node pubkey)"

./target/release/aphelion-node run
```

Your node starts at **5 000 reputation — half voting weight**. Full weight comes
from sustained correct submissions, not from stake. That is intentional: it is
what makes registering many fresh identities an expensive way to buy influence.

---

## 5. Daily operation

### What to watch

```bash
curl -s localhost:8080/health           | jq   # 503 when any feed is degraded
curl -s localhost:8080/v1/node          | jq   # reputation, stake, status
curl -s localhost:8080/v1/prices/BTC_USD | jq  # per-source breakdown and divergence
curl -s localhost:8080/v1/rounds?limit=20 | jq # why recent rounds were skipped
```

`/health` returns 503 when a feed is short of sources or overdue, not merely
when the process has died. Point your load balancer and your pager at it.

The alerts in [`deploy/prometheus/alerts.yml`](../deploy/prometheus/alerts.yml)
are the ones worth being woken for. Note what is deliberately *not* alerted:
individual source fetch failures, which happen constantly as exchanges
rate-limit, and which would train you to ignore the channel.

### Reading a quiet node

A node that is not submitting is not necessarily broken. Check `/v1/rounds`:

| Status | Meaning |
| --- | --- |
| `submitted` | Landed on chain |
| `skipped` | Price had not moved past `submit_deviation_bps` and the heartbeat was not due. **Normal** on a quiet feed |
| `failed` | The submission was attempted and rejected — investigate |
| `pending` | Signed, outcome not yet recorded |

---

## 6. Troubleshooting

### Clock skew

```
ERROR node clock is too far from ledger time; refusing to sign. Check NTP.
```

The node compares its clock against ledger time every round and stops signing
past `max_clock_skew`. This is protective: the aggregator rejects observations
that are stale or future-dated, so a skewed node would pay a fee per round to
have every submission thrown away. Fix NTP; the node resumes on its own.

Watch `aphelion_clock_skew_seconds` — it should sit near zero.

### "This node is not registered on chain"

The public key in `node-key.json` has no registry record. Either registration
did not land, or the node is pointed at a different aggregator than the one it
registered against. Compare:

```bash
./target/release/aphelion-node pubkey
stellar contract invoke --id "$APHELION_REGISTRY_CONTRACT" ... -- get_node --pubkey <that key>
```

### Submissions rejected as `NonceNotIncreasing`

Nonces must strictly increase per `(node, feed)`, and the counter lives in
Postgres. This almost always means the database was restored from a backup that
predates the node's most recent submissions. The node resynchronises from the
chain at startup, so a restart usually fixes it. If it persists, the node is
sharing a signing key with another running node — which it must never do.

### A feed keeps skipping with "no usable data"

Fewer sources survived filtering than `min_sources_per_feed`. Check
`/v1/prices/{feed}`: each source shows whether it was included and, if not, why.
Common causes are a venue delisting the pair, a symbol typo in the config, and a
venue returning a stale cache during an incident.

This is the node working as designed. Publishing a price backed by one exchange
is how an oracle launders a single venue's outage into apparent consensus.

### Reputation is falling

Compare your price against the network's: the `divergence_bps` field in
`/v1/prices/{feed}` is exactly this. Sustained divergence means your sources
disagree with everyone else's — check whether one venue is dominating your
median, and whether your `max_source_deviation_bps` is loose enough to be
letting a bad venue through.

Below 3 000 reputation the node is jailed and its submissions carry zero weight.
Adding stake does **not** clear that; only correct submissions do.

---

## 7. Exiting

```bash
stellar contract invoke --id "$APHELION_REGISTRY_CONTRACT" ... -- \
    request_unbond --pubkey "$(aphelion-node pubkey)"
```

Voting stops immediately; stake unlocks after the unbonding period, then:

```bash
stellar contract invoke --id "$APHELION_REGISTRY_CONTRACT" ... -- \
    withdraw --pubkey "$(aphelion-node pubkey)"
```

The delay is not friction for its own sake. It is the window in which a dispute
over your past submissions can still reach your stake — which is what makes
"publish a bad price, withdraw before anyone notices" unavailable as a strategy,
for you and for everyone else.

Keep the node running until you withdraw. Missed rounds still cost reputation
while you are exiting.
