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

## 2. Two keys, and one account

| Key | What it does | Where it lives |
| --- | --- | --- |
| **Ed25519 node key** (`node-key.json`) | Signs prices. **This is the node's identity**; the registry stores its public half and your stake is bonded against it. | On the node host, mode `0600` |
| **Stellar secret seed** (`S...`) | Pays transaction fees. Carries no authority over prices. | Environment variable |

They are separate because a signature, not a transaction, is what authorises a
price. Practically: you can rotate the funding account, share it with a relayer,
or run out of XLM in it, and none of that lets anyone publish a price as you.
Losing the *node* key, by contrast, means losing the identity your stake is
bonded to.

There is a third account in the picture, and on most deployments it is the same
one as the second: **the account that bonded the stake**. It is what the
registry answers `owner_of` with, and it is what authorises everything to do
with the committee — standing for a seat, casting a ballot, voting on a
dispute, appealing one. If you bonded from a different account than the one
that pays for submissions, say so in the config:

```toml
[network]
operator_account    = "G..."
operator_secret_env = "APHELION_OPERATOR_SECRET"
```

Getting this wrong produces `NotEligible` from the contract, which reads
exactly like a jailed node. `aphelion-node duties` prints the account it would
sign as, and warns when it is not the one that bonded the stake.

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
                         # set slashing_contract too, or disputes go unreported

export APHELION_STELLAR_SECRET="S..."
```

Confirm the configuration is coherent before going any further:

```bash
./target/release/aphelion-node show-config
```

Startup validation is deliberately strict, because a configuration that could
never work should fail loudly at boot rather than quietly at 03:00: a feed
mapping fewer sources than `min_sources_per_feed`, a feed pointing at a disabled
source, a round interval shorter than the poll interval, an upkeep interval
faster than the round loop, or an account address where a contract address
belongs are all startup failures.

---

## 4. Verify before you bond

Four checks, in order. The first is about the network you are joining; the
rest are about your node.

```bash
# 0. Is this deployment what its operators say it is?
#    Needs nothing installed but the stellar CLI and jq, and no relationship
#    to the deployment: every call it makes is simulated.
export APHELION_STELLAR_SECRET="S..."   # any funded account; it signs nothing

# If you were handed the deployment record, it names every contract:
APHELION_DEPLOYMENT_RECORD=./testnet.json scripts/verify-deployment.sh

# If you were handed contract ids instead, which is the usual case:
APHELION_REGISTRY_CONTRACT="C..." \
APHELION_AGGREGATOR_CONTRACT="C..." \
APHELION_SLASHING_CONTRACT="C..." \
APHELION_GOVERNANCE_CONTRACT="C..." \
  scripts/verify-deployment.sh
```

Give it the governance id even if nobody offered you one. Without it the script
cannot tell a network governed by a timelock from one governed by a key, and it
says so rather than passing quietly — but a deployment whose operators cannot
produce that id has answered the question anyway.

Read the **Authority** section of the output before anything else. Admin on all
three contracts should be the governance timelock, which means a parameter
change is published a day or more before it binds you — long enough to unbond
if you dislike it. If it is still a plain account, one key can change your
minimum stake, your reputation penalties and the dispute rules in a single
transaction with no warning. That is a fact about the deal you are being
offered, and it is worth knowing before your stake is in rather than after.

The **Readiness** section says whether the network can currently produce a
price at all. A fresh deployment legitimately warns that it has too few nodes;
that is what you are being recruited to fix.

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

Dry run composes and signs real rounds but cannot submit them. Its *reads* are
live — real ledger time, your real registry record — so what it reports is the
deployment you are pointed at; only submission is refused, one layer below the
code that decides whether to submit. Watch it for a few minutes and compare
`/v1/prices/{feed}` against a public price.

```bash
# 3. Check what the slashing contract is asking of you. Reads only, and safe
#    before registering: an unregistered key simply has nothing outstanding.
./target/release/aphelion-node duties

# 4. Register, then run for real.
export APHELION_REGISTRY_CONTRACT="C..."
export APHELION_OWNER_ACCOUNT="G..."   # this is the account that owns the node;
                                       # it is also what authorises committee
                                       # actions, so see `operator_account` in
                                       # section 2 if it is not the submitter
scripts/register-node.sh "$(./target/release/aphelion-node pubkey)"

./target/release/aphelion-node run
```

Your node starts at **5 000 reputation — half voting weight**. Full weight comes
from sustained correct submissions, not from stake. That is intentional: it is
what makes registering many fresh identities an expensive way to buy influence.

---

## 5. Daily operation

### The one command

```bash
./target/release/aphelion-node status
```

Identity, chain, registry standing, per-feed source counts, live exchange
probes and outstanding duties, on one page with a verdict on the end. It exits
0 when healthy, 1 when degraded and 2 when critical, so it works as a health
check or a cron line with nothing parsing its output.

Run it first whenever something looks wrong. It needs no database and does not
need the node to be running -- which is the case it is most useful in, because
a node that will not start cannot serve `/health`. Add `--no-probe` to skip the
exchange calls when this machine cannot reach them, and `--json` for a monitor.

A section it cannot read is reported as unread rather than as empty. Contract
reads in particular go through the Stellar CLI, which wants
`APHELION_STELLAR_SECRET` even though `status` never writes; without it the
page still renders and those lines say so.

### What to watch

Once the node is up, these are the same facts continuously:

```bash
curl -s localhost:8080/health           | jq   # 503 when any feed is degraded
curl -s localhost:8080/v1/node          | jq   # reputation, stake, status
curl -s localhost:8080/v1/prices/BTC_USD | jq  # per-source breakdown and divergence
curl -s localhost:8080/v1/rounds?limit=20 | jq # why recent rounds were skipped
curl -s localhost:8080/v1/duties        | jq   # disputes and elections, with deadlines
```

`/health` returns 503 when a feed is short of sources or overdue, not merely
when the process has died. Point your load balancer and your pager at it.

The alerts in [`deploy/prometheus/alerts.yml`](../deploy/prometheus/alerts.yml)
are the ones worth being woken for. Note what is deliberately *not* alerted:
individual source fetch failures, which happen constantly as exchanges
rate-limit, and which would train you to ignore the channel.

### Taking part in the randomness beacon

Optional, and only if the deployment runs one (`randomness_contract` in the
`[network]` section).

```bash
./target/release/aphelion-node beacon status
```

Set `[beacon] participate = true` to have a running node commit to each round
and open one when none is running. Off by default: it spends transaction fees
on work nobody is obliged to do.

The thing to understand before turning it on is what a commitment obliges you
to. A round is two transactions — commit, then reveal — separated by a window
of minutes, and **a node that commits and does not reveal is slashed**. Not for
being wrong: for going quiet after everyone else has spoken, which the contract
cannot tell apart from withholding on purpose.

So three things follow, and the node is built around them:

- The secret is written to Postgres **before** the commitment is submitted. A
  crash between the two costs a round, not stake.
- Revealing is not gated by `participate`. Switching it off stops the node
  entering new rounds; it does not release it from one it has already entered,
  and the loop keeps running to send that reveal.
- `beacon.interval` must be comfortably shorter than the contract's reveal
  window. A node that looks once per window can sleep through one. `beacon
  status` compares the two and warns; the config refuses anything above 120s.

**Back up your database.** This is the one part of the node where restoring
from a backup older than your last commitment costs money: the commitment is a
hash, the secret that opens it is only in `beacon_rounds`, and a round whose
secret is gone is a penalty that cannot be avoided. `beacon status` reports it
as `SECRET LOST` so that it is at least not a surprise.

### What governance is about to change

The parameters you bonded under — the minimum stake, the dispute bonds, the
slash amount, which contract is the aggregator — are held by a timelock rather
than by a key, so a change to any of them is published before it takes effect.
That delay is yours: it is the window in which you can unbond if you do not
want to operate under the new numbers.

```bash
export APHELION_STELLAR_SECRET="S..."     # any account; reads are simulated
export APHELION_GOVERNANCE_CONTRACT="C.." # unless you have the deployment record

scripts/govern.sh list                    # what is queued, and its state
scripts/govern.sh show 7                  # the exact call, and when it lands
```

`Waiting` means the delay is still running. `Ready` means anybody can execute
it now — including you, if it is a change you want and the proposer has gone
quiet. Nothing about executing is privileged: the call was fixed when it was
queued.

Worth checking weekly rather than daily — but do check. The delay cannot be
shorter than a day and a day is the default, while unbonding takes
`unbonding_period` on top of it, a week by default. So a change first noticed
on the day it becomes executable is one you will be operating under whether or
not you want to: the two windows do not overlap in your favour.

### Charging the nodes that have gone quiet

There is one penalty on this network that nothing does by itself. The aggregator
catches an outlier with arithmetic it was already doing, and a dispute is filed
by whoever noticed — but a **missed round** has to be charged by somebody.
`sweep_absent` is permissionless, which means anyone may call it and nobody is
obliged to. Until one of you does, an operator who switched their node off last
month still carries the weight they earned while it was running, and their last
price still counts towards every median.

See who that currently is. This reads the chain and submits nothing:

```bash
aphelion-node sweep
```

```
27 registered node(s); absence threshold 3600s
    1 this node (never its own business)
    2 carry no weight (unknown, jailed or exiting)
   22 seen recently enough

public key                                                             silent  weight
9f2c1b4a5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708        91204s  10000bp
4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f7081920304       14511s   5000bp

Nothing submitted. Re-run with --commit to charge these nodes a missed round.
```

Charge them once, by hand:

```bash
aphelion-node sweep --commit
```

Or have your node do it continuously, in `aphelion.toml`:

```toml
[upkeep]
sweep_absent = true
interval     = "30m"
max_batch    = 25
```

A running node serves the same plan over HTTP, so this needs no shell on the
box:

```bash
curl -s localhost:8080/v1/upkeep | jq
```

**Why you would.** Weight is relative. The median is taken over whoever turns
up, so every basis point a dead node still carries is a basis point yours does
not have, and every reward paid out to a round it did not join is smaller than
it should be. A sweep is a small fee to reduce a competitor's weight to what it
has recently earned.

**Why it is off unless you ask.** It is a transaction fee for a call that pays
nothing back directly, and nobody should find fees on their account for upkeep
they did not opt into.

**What it will not do.** It never includes your own key. Your absence is
somebody else's to charge, and every other operator has the same reason to
charge it that you have to charge theirs — see
[Charged a missed round while you were down](#charged-a-missed-round-while-you-were-down)
for that side of it.

Two things to expect once it is on. First, a sweep sometimes charges fewer nodes
than it offered: you read the registry's `last_submission`, which moves only
when a round the node joined actually closed, while the aggregator decides from
the last submission it *accepted*. Your view is the more pessimistic one, the
contract declines the difference, and the log says so — the fee is spent either
way, and the node will not re-offer a declined key inside the same window.
Second, on a healthy network most passes send no transaction at all, and the
`nothing to sweep` line in the log is the loop working.

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

Reputation moves in three ways, and the size of the move tells you which
happened:

| Change | Cause | Stake |
| ---: | --- | --- |
| **+50** | A submission inside the consensus band | Reward paid, if the pool can cover it |
| **−25** | A missed round | Untouched |
| **−500** | A submission outside `max_deviation_bps` of the round median | `outlier_slash` seized |

A run of −25s is a connectivity or scheduling problem. A single −500 is a data
problem: one of your venues was wrong and your filtering let it through. The
`OutlierPenalized` event carries your price, the round median and the deviation
in basis points, so you can check the arithmetic without replaying anything.

### If your node is jailed

Below 3 000 reputation the node is jailed and its submissions carry zero weight.
Zero weight is absolute: the aggregator refuses the submission outright, so you
cannot work your way back up. Adding stake does not clear jail either — jail
begins exactly when reputation crosses below the threshold, so no top-up can
satisfy a condition written on reputation.

What clears it is serving the term. Once `jailed_until` has passed, anyone may
call:

```bash
stellar contract invoke --id "$APHELION_REGISTRY" -- release --pubkey "$PUBKEY"
```

It is permissionless because it only checks facts on the ledger. Three of them,
and all three must hold:

| Condition | If it fails |
| --- | --- |
| The node is jailed | `NotJailed` (15) |
| `jailed_until` has passed | `StillJailed` (16) |
| Bond is at or above `min_stake` | `StakeTooLow` (8) |

The third is the one that catches operators out. If a slash took you below the
minimum, `release` refuses until you `add_stake` back up to it — returning
under-bonded would mean voting with less at risk than the network asks of
everyone else. So the recovery for a slashed-and-jailed node is: top up, wait
out the term, then release.

Release returns you to **5 000 — a newcomer's standing**, not the reputation you
had before. Everything earned above that is gone, and you climb back to full
weight the same way a new node does: 40 in-band rounds. Your stake stays bonded
the whole time, which is the point of jail rather than ejection — you remain
reachable while the term runs.

Read your term off the node record:

```bash
stellar contract invoke --id "$APHELION_REGISTRY" -- get_node --pubkey "$PUBKEY"
```

`jailed_until` is a ledger timestamp; zero means not jailed.

The alternative is to `request_unbond`, wait out the unbonding period, withdraw
and register a fresh key — which also lands you at 5 000. Any sane deployment
sets the jail term shorter than the unbonding period, so serving it is the
faster route, and it keeps the operating history that a future dispute might
need to work in your favour.

### Charged a missed round while you were down

`sweep_absent` is permissionless: once your node has been silent longer than the
aggregator's `absence_threshold`, anyone may pay a fee to charge you a missed
round. A single stretch of silence can only be charged once however many times
it is swept, so this costs 25 reputation per absence window, not per caller.

It is not a punishment for downtime as such — downtime never seizes stake — but
it is why a node left switched off keeps losing weight rather than sitting
frozen at its old standing.

Expect it to be other operators' nodes doing the charging: the incentive is
symmetric, and the same `[upkeep]` setting is available to you
([Charging the nodes that have gone quiet](#charging-the-nodes-that-have-gone-quiet)).
A node that swept itself would be paying a fee to penalise itself, so none does.

Time spent in jail is not chargeable this way. A jailed node is silent because
the aggregator refuses its submissions, not because it chose to be, and it is
already serving a penalty for that; the sweep skips it and keeps its clock
moving, so `release` does not hand you a missed round in the first moment you
are allowed to submit again.

---

## 7. If you are disputed

The aggregator penalises what it can prove arithmetically, in the round itself.
Anything else — a claim that you colluded across rounds, or fed a manipulated
venue on purpose — goes through the slashing contract, where people decide it on
evidence.

**You will be told.** A node with `slashing_contract` configured re-reads it
every `committee.watch_interval` and reports what it finds to the log, to
`/v1/duties` and to `aphelion_duties_outstanding`. The shipped alert rules page
on the last of those with no delay at all, because the windows below close and
do not reopen. Check it by hand at any time:

```bash
aphelion-node duties         # everything outstanding, worst first
aphelion-node dispute show 7 # one allegation in full
```

What happens, and what you should do:

1. **A dispute is filed** against your public key for a specific `(feed, round)`,
   with a bond the reporter forfeits to you if it is dismissed, and a link to
   their evidence. `duties` reports it as `COSTLY` with the time left on the
   voting period; `dispute show` prints the evidence link.
2. **The committee votes** for the configured voting period. If it does not reach
   quorum, or the vote ties, the dispute is **dismissed** — silence is not
   evidence against you.
3. **Answer it.** There is nothing to file on chain: a dispute is answered by
   evidence and argument in front of the committee, wherever that conversation
   happens. Your case is your own records, and `replay` assembles it — it takes
   the round the dispute names and rebuilds it from the observations retained
   underneath it:

   ```bash
   aphelion-node replay BTC_USD 4812          # the page, for you
   aphelion-node replay BTC_USD 4812 --json   # the bundle, for the committee
   ```

   It answers two separate questions, and it is worth knowing which is which
   before you quote either at anybody.

   **Does the stored signature cover the stored round?** If it does, the row in
   your database is the one you signed, and the command prints the exact
   canonical payload the contract verified. Anyone can check that against your
   public key without running Aphelion or believing anything you say about it,
   which is what makes it evidence rather than an assertion. If it does *not*,
   stop and find out why before you say anything: either a column has been
   edited since the round was signed, or the node is configured against a
   different aggregator than the one that signed it. Check
   `network.aggregator_contract` first — repointing a node at a new deployment
   makes every older round read as unverifiable, and that is not tampering.

   **Do the retained observations still produce the published price?** Re-run
   over the window the round actually read — the freshest observation from each
   venue that had *arrived* by then — and compare. Sources excluded as outliers
   are listed with the reason they were dropped, which is usually the whole
   answer to "why was your price different from everyone else's".

   The exit code is the verdict: `0` reproduced, `1` too little retained to
   answer, `2` diverged or unverifiable.

   Two things it will not do. It will not call a difference close enough — the
   price either comes back identical or it does not. And it will not read a
   divergence as an admission: `min_sources`, `max_source_deviation_bps` and the
   confidence floor are configuration and are not recorded beside a round, so a
   round composed before you retuned them was computed under numbers the replay
   cannot recover. It prints those explanations next to the divergence rather
   than deciding between them for you.

   A round whose observations have been pruned cannot be replayed at all, and
   grades as `incomplete` rather than as anything worse. This is the reason
   observations are retained at all, and the reason to check that
   `retention.raw_prices` is longer than the dispute and appeal windows
   combined *before* you need it — the command cannot recover what retention
   has already deleted.

   Hand over the `--json` bundle, not the page. The other side can check it
   themselves with no access to your node, your key or your database:

   ```bash
   aphelion-node verify-evidence bundle.json
   ```

   That command ignores your node's verdict entirely and re-derives everything
   from the signed bytes — which is the point. A bundle is worth something to a
   committee precisely because they do not have to take your word for any part
   of it. If you are ever on the other side of one, this is also the command to
   run on somebody else's bundle before voting on it.
4. **Appeal, once**, within the appeal window, if the committee finds against you
   and you believe it is wrong. The appeal bond is larger than the dispute bond
   and is returned only if the second vote changes the outcome — so
   `dispute appeal` prints the bond and sends nothing until you add `--commit`.

   ```bash
   aphelion-node dispute appeal 7            # shows what it would cost
   aphelion-node dispute appeal 7 --commit   # posts the bond
   ```
5. **Settlement** moves stake after the appeal window closes, and only when
   somebody calls it. Nothing moves before then. If the dispute was *dismissed*,
   settling is what hands you the reporter's bond, and `duties` reports it as
   `OWED` once the window has passed:

   ```bash
   aphelion-node dispute settle 7
   ```

Your stake stays reachable through the unbonding period even if you have asked
to exit, which is the point of the delay.

If a committee member is also the operator of the node under dispute, the
contract refuses their vote. That is checked on chain rather than left to
etiquette, and `duties` never offers you a vote on your own node — it would be
a transaction fee spent on a guaranteed refusal.

---

### Voting on somebody else's dispute

If you sit on the committee, a vote is a decision about another operator's
stake, so check their evidence rather than the summary attached to it:

```bash
aphelion-node verify-evidence their-bundle.json
```

It needs nothing from you but the file — no configuration, no key, no database,
no chain access. It ignores the bundle's own verdict and re-derives everything
from the signed payload, and it grades `sound`, `unsupported`, `misdescribed` or
`unsigned` with the exit code to match (0, 1, 2, 2).

Read what it says it cannot check as carefully as what it can. It cannot tell a
bundle showing four venues from one showing four of six, where the two left out
would have moved the median — only the operator holds the full table. And a
sound bundle about a different node is still a sound bundle, so confirm the key
it names is the key under dispute before the grade means anything:

```bash
aphelion-node dispute show 7   # whose key the allegation is against
```

---

## 8. Electing the committee

The committee that can take your stake is elected by operators, weighted by the
same `weight_of` that decides how much your price counts. Your node is part of
that electorate, and nothing casts its ballot for you.

```bash
aphelion-node election show          # phase, deadlines, who stands, whether you voted
aphelion-node election nominate      # stand, on the strength of this node
aphelion-node election ballot G...   # cast this node's weight for one candidate
```

Four things are worth knowing before the first one runs.

**One ballot names one candidate**, in a race with several winners. That is
deliberate: a slate ballot would let a bare majority of weight take every seat.

**A failed election changes nothing.** Too few eligible candidates draw weight
to fill the quorum and the sitting committee stays. The cost is real — a
committee nobody replaces holds over indefinitely — and the alternative is
worse, because vacating the seats would let an attacker switch slashing off for
everybody by suppressing turnout.

**Two steps are permissionless and nobody is assigned them**, in the same way
`sweep_absent` is. An election has to be *opened* once the sitting term is
served, and a closed ballot has to be *counted* before any later election can
open. `duties` reports both as `HOUSEKEEPING`; either costs a transaction fee
and nothing else:

```bash
aphelion-node election open
aphelion-node election finalize
```

**A jailed or exiting node has no ballot and cannot stand**, because its weight
is zero. `duties` says so in the `weight` line rather than offering you
something the contract would refuse.

---

## 9. Exiting

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
