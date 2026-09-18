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
probes, outstanding duties and — on a deployment with a randomness contract —
this node's part in the beacon, on one page with a verdict on the end. It exits
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

### If the page says jailed or exiting

Both states end at a timestamp, and neither ends on its own — `release` and
`withdraw` are transactions somebody has to send. So the registry line carries
the clock:

```
registry   : jailed · 0 bps · reputation 2500 · stake 6 · releasable now
```

`releasable now` means the term is served and the only thing between this node
and voting again is a call. `release` is permissionless — anybody can send it,
including you — and until somebody does, the node sits at zero weight earning
nothing. That is downtime no part of the network is imposing on you.

`release in 6h` means wait. A jailed node has zero weight, the aggregator
refuses a zero-weight submission outright, and so there is nothing to do in the
meantime: jail is served as time, not as work, and `release` reverts before the
term ends. [If your node is jailed](#if-your-node-is-jailed) has the rest —
what release costs you, and why the term is the length it is.

**Check the bond as well as the clock.** `release` has three conditions —
jailed, term served, and a stake at or above the registry's minimum — and only
the middle one is served by waiting. If a slash took you below the minimum you
will serve the whole term and still be refused:

```
  [critical] jailed and under-bonded, which is the harder half: `release` refuses
             a node whose stake is below the registry's minimum ... `add_stake`
             the 40 short
```

Top up with `add_stake` before the term ends, not after. The two remedies do not
substitute for each other: topping up does not clear jail, because jail is a
statement about reputation that capital cannot answer, and waiting does not
restore a bond a slash took.

The same warning appears at `degraded` on a node that is still **active** and
below the minimum. It costs nothing that day — the aggregator weighs reputation,
not stake, so your submissions still count — which is exactly why it is worth
saying then. From there, one bad spell puts you in a jail you cannot leave by
waiting.

For an exiting node the line reads `unlocks in 3d` or `withdrawable now`. The
stake stays bonded, and stays slashable, until `withdraw` is called: that
exposure is what the unbonding period is for, and it does not end when the
period does — it ends when the stake leaves. See [Exiting](#9-exiting).

A deadline the node reports as unknown is a read that did not land, not a term
that has run. `status` never takes the clock from this machine; the ledger's
time is the only one a claim about a contract deadline can honestly be made in.

### What `status` checks about retention

One line on that page is not about whether the node is working:

```
evidence   : 30d retained · defends rounds up to 27d old
```

`database.retention` decides how long raw observations survive, and observations
are the whole of a defence — `replay` grades a round whose inputs have been
pruned `incomplete`, and an accused operator with an `incomplete` replay has
nothing to answer with. So `status` reads the slashing contract's periods and
does the arithmetic against your setting.

The sum is not the obvious one. A dispute can go on asking for **two** voting
periods plus an appeal period, because an appeal opens a second voting round and
asks again — the first round's answer is not carried into it. And retention has
to cover that *on top of the disputed round's age*, since the observations are
read when the answer is given rather than when the allegation is made. What is
left over is the horizon: the age of the oldest round you could still defend if
a dispute over it were filed now.

| What `status` says | What it means |
| --- | --- |
| `[critical] no round published by this node can be defended` | Retention does not even cover one dispute's full life. Nothing else on the page has moved: the node is collecting, signing and submitting perfectly, and can prove none of it |
| `[degraded] only the last <window> of rounds can be defended` | Working, with a horizon shorter than one dispute takes to run. Worth widening — nothing on chain refuses an allegation about an old round |
| `evidence : unread` | The periods could not be read, so your retention is unchecked against them. Not the same as fine |

There is no staleness rule in `open_dispute`: it refuses a duplicate and an
unregistered key and checks nothing about the round's age. Your retention is the
only limit on how far back an allegation can reach and still be answerable, and
it is set by you alone.

Set it on a good day. Retention that is too short costs nothing, breaks nothing
and shows up nowhere else, right up until the afternoon somebody files a
dispute — at which point raising it changes nothing, because the observations
the defence needed are already gone.

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
and open the next one when the last has closed. Off by default: it spends
transaction fees on work nobody is obliged to do.

That second half matters more than it sounds. `open_round` is permissionless
and the contract runs one round at a time, so on a deployment where no operator
has switched participation on, the beacon produces one value and then sits
still — not broken, just never started again. The cadence is the contract's
`min_round_interval`, counted from one opening to the next, and `beacon status`
counts it down on a closed round:

```
round 41
  status   : Finalized
  our part : revealed
  next     : a round may open in 248s
```

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
- A round the contract has closed without your reveal is recorded locally as
  closed rather than retried. The no-show penalty was charged on chain by
  `finalize` and no later reveal will be accepted, so `beacon_rounds.closed_at`
  is where to look for which round it was and when.
- `beacon.interval` must be comfortably shorter than the contract's reveal
  window. A node that looks once per window can sleep through one. `beacon
  status` compares the two and warns; the config refuses anything above 120s.

`aphelion-node status` carries a `beacon` line too, read from the ledger
without the database. It goes critical when a commitment of yours is still
unopened two polls into the reveal window, degraded once a missed reveal has
become a penalty, and degraded when a round has been openable for longer than
two polls and nobody — including your participating node — has opened it. That
last one is the only way a stopped beacon shows at all: from outside, it looks
exactly like one between rounds.

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

A `skipped` row carries the reason, and one of them is not about the price at
all: **this node has no voting weight**, so the transaction was withheld rather
than paid for. The aggregator reads your weight and reverts on zero — jailed,
exiting, or not in the registry — and it does that after the fee. Rather than
buy that refusal once per feed per round, the node signs and records the round
as usual and stops at the transaction.

That is why a jailed node looks quiet rather than broken on every panel: there
are no failures to count, because nothing was sent. `aphelion_weight_bps` is the
metric that shows it, and one line in the log says it when it starts:

```
WARN this node's submissions would not be counted; not paying to send them
     authority="jailed, so the aggregator would refuse every submission until
     the term is served and `release` is called; see `aphelion-node status`"
```

Logged on the change rather than on the tick — a line per feed per interval for
the length of a jail term would be the loudest thing in the log and the least
informative — so if you have missed it, `aphelion-node status` or `/v1/node`
will tell you the same thing at any time.

Nothing extra is charged for the silence. `sweep_absent` excuses a zero-weight
node explicitly and marks it swept, on the reasoning that its silence is the
punishment already running, so skipping is not trading a fee for a missed round.

**A registry read that fails never stops a submission.** The node keeps the last
answer it had, and a node that has never managed to read its record submits
anyway. A refused submission costs one fee; a round you stay quiet for is a
missed round somebody may sweep. An RPC blip must not turn the first into the
second.

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

You do not have to read your term off the ledger to know where you stand —
`aphelion-node status` reports all three conditions, the wait while the term
runs, the call once it is served, and the shortfall when the bond is the thing
in the way. See [If the page says jailed or
exiting](#if-the-page-says-jailed-or-exiting).

The record itself, if you want it:

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

1. **A dispute is filed** against your public key for a specific `(feed, nonce)`
   — the nonce you signed the submission under, not the round id the aggregator
   allocated afterwards — with a bond the reporter forfeits to you if it is
   dismissed, a link to their evidence and the SHA-256 of it. That digest was
   fixed when they filed and cannot be changed afterwards, so check whatever
   file you are sent against it before you spend any time answering: a case that
   does not match the one on the ledger is not the case against you.

   ```bash
   aphelion-node dispute check 7 --file the-case.json
   ```

   It reports the file as the `case` or not, and audits it where it is a bundle.
   `duties` reports the dispute as `COSTLY` with the time left on the voting
   period; `dispute show` prints the evidence link, and the commands that answer
   it and check either document, ready to paste.
2. **The committee votes** for the configured voting period. If it does not reach
   quorum, or the vote ties, the dispute is **dismissed** — silence is not
   evidence against you.
3. **Answer it.** Your case is your own records, and `replay` assembles it — it
   takes the feed and nonce the dispute names, which is why the dispute names a
   nonce, and rebuilds that round from the observations retained underneath it:

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
   observations are retained at all, and `aphelion-node status` now checks
   `database.retention` against the periods the slashing contract holds rather
   than leaving the sum to you — see [What `status` checks about
   retention](#what-status-checks-about-retention). Nothing can recover what
   retention has already deleted, which is why it is checked on a good day.

   Hand over the `--json` bundle, not the page. The other side can check it
   themselves with no access to your node, your key or your database:

   ```bash
   aphelion-node verify-evidence bundle.json \
       --node <your key> --feed BTC_USD --nonce 4812 --aggregator C...
   ```

   That command ignores your node's verdict entirely and re-derives everything
   from the signed bytes — which is the point. The flags are the allegation as
   the ledger states it, and they are what stops a bundle for the wrong round
   being accepted as an answer; `dispute show` prints the whole line. A bundle is worth something to a
   committee precisely because they do not have to take your word for any part
   of it. If you are ever on the other side of one, this is also the command to
   run on somebody else's bundle before voting on it.

   **Then put it on the record, while the vote is still open.** The document
   stays where it is; what goes on the ledger is its SHA-256.

   ```bash
   aphelion-node replay BTC_USD 4812 --json > evidence.json
   aphelion-node dispute respond 7 --file evidence.json            # shows what it would publish
   aphelion-node dispute respond 7 --file evidence.json --commit   # publishes it
   ```

   Do this even when you are handing the file over privately, and do it before
   the voting deadline — after it the contract refuses, which is the whole
   point. A digest published while the vote is open was fixed before you could
   know how the vote was going; one published afterwards would be worth nothing,
   because a file assembled after a result can be assembled to suit it. It also
   answers, for good, a question you would otherwise have no way to answer: that
   you replied at all, and that the file the committee read is the file you sent.

   The dry run audits the bundle against the allegation before anything is sent
   and refuses to publish one that is `unsigned`, `misdescribed` or `unrelated`
   — those are not weak evidence, they are not evidence, and answering with one
   spends your one clear shot at the committee's attention on a file that
   establishes nothing. `--uri` is optional: a digest with no locator still
   fixes which document you answered with.

   An answer can be **corrected but never withdrawn** — up to three per voting
   round, all of them on the record in the order you gave them, so a committee
   sees a substitution as a substitution. An appeal opens a second round and
   asks for the answer again; the first round's answer stays where it is and is
   not carried forward.
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
stake, so check the evidence rather than the summary attached to it. There are
two documents in a dispute and the same command reads both:

```bash
aphelion-node dispute check 7 --file their-bundle.json     # or `-` for stdin
aphelion-node dispute check 7 --file the-case.json         # the reporter's
```

You are an operator, so your node already knows the deployment — and everything
else comes off the ledger: the accused, the feed and the nonce from the dispute,
and the digest from whichever commitment on the record these bytes turn out to
be. Nothing is read out of the file. It grades `sound`, `unsupported`,
`unrelated`, `misdescribed` or `unsigned` with the exit code to match (0, 1, 2,
2, 2), ignoring the bundle's own verdict and re-deriving everything from the
signed payload. It sends nothing: reading a document is not voting on it.

Above the audit it prints three things the file cannot tell you.

**Whose stake is at risk** — `registry.owner_of` on the accused key, which is
the one question no bundle can settle; an allegation against a key the registry
has never seen is an allegation against nobody's stake.

**Where the file stands on the record.** A dispute holds a commitment from each
party: the reporter's case, fixed when it was filed, and the accused's answers,
appended while the vote is open.

| Standing | What it means |
| --- | --- |
| `case` | The document the allegation was filed on. Pinned when the dispute was opened and unchangeable since |
| `offered` | They committed to these bytes while the vote was open, and this is the answer they are standing on |
| `superseded` | They committed to these bytes and answered over them afterwards. Both are on the record; the last is the answer |
| `absent` | They answered, and this is not what they answered with |
| `unanswered` | Nothing on the accused's side of the record commits them to this file, or to any other |

`superseded` is not a finding against them. Up to three answers are allowed in a
voting round, they are kept oldest first, and an answer may be corrected but
never erased — so a document that was answered over is still genuinely theirs,
and the audit runs on it the same way.

Which side the file is on changes one thing about the standard, and it is the
accused's key. An answer is held to it: an answer that does not carry the
accused's own signature answers nothing. A case is not, because the ordinary
allegation is a second operator's node reporting what it saw, and a reporter
required to produce the accused's signature could only ever file the case the
accused had already signed for them.

**Whose key signed it**, resolved through the registry rather than taken from
the file's own account of itself. None of the four is a defect, and the
difference between them is the difference between the kinds of case that can be
made:

| Signed by | What it is |
| --- | --- |
| the accused | The strongest allegation there is: a payload nobody but the accused could have produced |
| the reporter's node | One operator's word that their node saw something else. Evidence, and not by itself a finding — two nodes disagreeing is what the median exists for |
| a third operator's node | Corroboration, on the same terms |
| a key the registry has never seen | A genuine signature with no stake in this network behind it |

A sound case is not a finding either way. What it establishes is that the
payload in it was signed by the key it names; whether the accused's own
submission was wrong is the question you are voting on, and their answer is the
other half of it.

A document that is not an evidence bundle at all — a written account, a log
archive, an exchange's own export — is legitimate on either side and is read
rather than graded. `dispute check` still places it on the record and says so,
and exits 65 rather than 1: an operator who answered in prose has not earned a
finding.

Where you have no node pointed at the deployment, the same judgement is one
command with no configuration, no key, no database and no chain access at all,
and `dispute show` prints it filled in:

```bash
aphelion-node verify-evidence their-bundle.json \
    --node <accused> --feed BTC_USD --nonce 4812 --aggregator C... \
    --digest <the answer on the record>
```

A mistyped flag exits 64 rather than 1, so a slip of yours is never read back as
a finding against them.

**`--digest` is the one to reach for first**, and it is the one flag that is
not about the signed bytes. It is the SHA-256 the accused published with
`respond` while the voting period was open, and `dispute show` prints it under
`answer` along with the time it landed. Compared against the *bytes of the file
in front of you*, it answers a question the payload cannot: whether this is the
document they committed to, or one that turned up afterwards. A bundle can be
signed, honestly described, reproducible and about exactly the right round, and
still not be the file that was answered with — say, the same round with a venue
added that helps their case. A `sound` verdict with `--digest` given is a much
narrower claim than a `sound` verdict without it.

A dispute with no answer at all says so in as many words, and what to make of
that is yours to weigh.

**Type the rest off the dispute, not off the bundle.** They are checked against
the signed bytes, and a file asked to supply the standard it is measured against
will meet it. Run without them and the audit says, in as many words, that nobody
asked whether the file bears on this dispute at all — which is not the same as
asking and being satisfied. The substitution they catch is the cheapest move
available to an accused operator: replaying a round they reported honestly. It
forges nothing, it reproduces, and on its own terms it is beyond reproach.

Read what it says it cannot check as carefully as what it can. Neither command
can tell a bundle showing four venues from one showing four of six, where the
two left out would have moved the median — only the operator holds the full
table, and no arithmetic on what they chose to hand over closes that. The other
limit is `verify-evidence`'s alone: `--node` ties the evidence to the
allegation, not the allegation to a person, and it has no registry to ask.
`dispute check` does, and prints the answer.

Check the reporter's document too, with the same command. If the case you were
handed is not on the record as the case, it is not the case on the ledger and
the accused has been answering something else. Without a node of your own the
`verify-evidence` line above reads it, with `--digest` set to the `case` digest
that `dispute show` prints and **`--node` left off** — a reporter's bundle is
normally signed by the reporter's own node, and holding it to the accused's key
would grade every honest case `unrelated`.

One thing the record cannot tell you, and it is worth knowing where the edge is.
A file that is neither the case nor an answer, in a dispute nobody has answered
yet, is measured against nothing: a bundle the accused hands you privately
before they publish and a case the reporter swapped after filing look identical
to the ledger. It says so rather than guessing, and reaching for the case digest
there would grade the first as a substitution for doing the honest thing early.

---

### Filing one against somebody else

A dispute costs a bond you lose if the committee dismisses it, and it needs
three things: the accused's key, the feed, and **the nonce they signed** — which
`SubmissionAccepted` carries alongside the round id, and which is what lets the
committee match their answer to your allegation.

```bash
aphelion-node dispute open <accused> BTC_USD 4812 \
    --file case.json --uri ipfs://bafycase            # prints the bond and the digest
aphelion-node dispute open <accused> BTC_USD 4812 \
    --file case.json --uri ipfs://bafycase --commit   # posts the bond and files
```

`--file` is your case, and only its SHA-256 goes on the ledger. Two things
follow that are worth knowing before the first one runs.

**It is fixed for the life of the dispute.** Unlike an answer, an allegation
cannot be corrected: everything the accused says is a reply to the document you
filed, so moving it afterwards would move the question they already answered. A
wrong digest costs you the bond, the way a broken link would. The dry run prints
exactly what will be published, and nothing is sent without `--commit`.

**`--uri` is optional and worth giving.** A digest nobody can resolve to a
document is a case nobody can read, and a committee that cannot read your case
will dismiss it. If you are handing the file over privately, the digest is still
what proves afterwards that what they read is what you filed.

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
