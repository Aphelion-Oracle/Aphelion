# Contract reference

Every function on the five Aphelion contracts, what it costs to call, and who
may call it. The README explains *why* the system is shaped this way; this is
the surface it presents.

> **Pre-alpha.** Interfaces, storage layouts and the canonical signing payload
> may still change in backwards-incompatible ways. Anything marked
> **consensus-breaking** below cannot change without every node and every
> deployment changing with it.

## Contents

- [Deployment order](#deployment-order)
- [Registry](#registry)
- [Aggregator](#aggregator)
- [Slashing](#slashing)
- [Governance](#governance)
- [Consumer example](#consumer-example)
- [Error codes](#error-codes)
- [The interface the node depends on](#the-interface-the-node-depends-on)

---

## Deployment order

The three core contracts refer to each other: the registry accepts reputation
changes only from the aggregator and slashes only from the slashing contract,
and the aggregator reads weights only from the registry. So all of them are
deployed first and initialised afterwards — deploying is what fixes an address,
initialising is what teaches each contract the others'. That is the reason
`initialize` is a separate call rather than a constructor, and
[`scripts/deploy.sh`](../scripts/deploy.sh) does it in that order.

```
  deploy registry   ─┐
  deploy aggregator  ├─▶ initialize registry(admin, aggregator, slasher, token, …)
  deploy slashing    │   initialize aggregator(Config{registry, …})
  deploy governance ─┘   initialize slashing(Config{registry, …}, committee)
                         initialize governance(Config{guardian, …}, proposers)
                         set_feed(…) per feed
                              │
                              ▼
                         admin on the first three ──▶ the governance timelock
```

`set_aggregator` and `set_slasher` exist so a deployment can be repointed
afterwards — for instance to bootstrap with the admin in both roles and hand
over once the other contracts are live.

The handover is last on purpose: every call above is made by one key in one
transaction, because a deployment that had to serve the timelock's delay to add
its first feed would never finish. From that point the same changes are
proposals. See [What it governs](#what-it-governs).

---

## Registry

Node identity, stake, reputation. Its only job is to answer *how much should
this node's opinion count?*

A node is identified by its **Ed25519 public key**, never by an `Address`.
Submissions are authenticated by signature, so the account that pays a
transaction fee carries no authority at all.

### Administration

| Function | Caller | Notes |
| --- | --- | --- |
| `initialize(admin, aggregator, slasher, token, min_stake, unbonding_period, jail_period)` | Admin, once | Traps if already initialised, if `min_stake <= 0`, or if either period is zero |
| `set_aggregator(aggregator)` | Admin | Repoint after deployment |
| `set_slasher(slasher)` | Admin | Repoint after deployment |
| `set_min_stake(min_stake)` | Admin | 1 stroop – 10 million XLM; does not retroactively jail nodes below the new floor |
| `set_admin(new_admin)` | Admin | |
| `get_config()` | Anyone | |

`min_stake` is bounded above as well as below, which is less obvious than it
looks: the bond is a door as much as a deposit. Set high enough it closes the
network to newcomers without any proposal having to say so, and every
already-bonded node is unaffected — so the change is invisible to exactly the
people who would object to it.

`unbonding_period` and `jail_period` have no setters. Both are promises made to
operators who bonded stake under them, and a key able to lengthen a jail term
after the fact would be a key able to expropriate. They are fixed at
`initialize` and cannot be changed.

### Stake

| Function | Caller | Notes |
| --- | --- | --- |
| `register(owner, pubkey, stake)` | The bonding account | Transfers `stake` to the contract. Starts at 5 000 reputation — half weight |
| `add_stake(pubkey, amount)` | The node's owner | Restores the bond. Does **not** clear jail — see `release` |
| `release(pubkey)` | Anyone | Ends a served jail term. Traps unless the node is jailed, `jailed_until` has passed, and the bond is at or above `min_stake` |
| `request_unbond(pubkey)` | The node's owner | Voting stops immediately; stake stays locked |
| `withdraw(pubkey)` | The node's owner | Only after `unbonding_until`; returns what is left after any slashing |
| `fund_rewards(from, amount)` | Anyone | Tops up the pool node rewards are paid from |
| `reward_pool()` / `slash_pool()` | Anyone | |

Stake is **transferred**, not attested. Slashing is then arithmetic on funds the
contract already holds rather than a claim against an account that may be empty
by the time it matters.

`release` is permissionless because all three of its conditions are facts on the
ledger rather than judgements, and it restores reputation to exactly
`STARTING_REPUTATION` — a newcomer's standing. It cannot return less, because an
operator can always unbond, withdraw and register a fresh key to arrive there
anyway; it must not return more, because that would make jail cheaper than being
new. Note that capital cannot substitute for the wait: jail begins exactly when
reputation falls below the threshold, so no top-up can satisfy a condition
written on reputation.

### Called by the aggregator

| Function | Caller | Effect |
| --- | --- | --- |
| `record_success(pubkey, reward)` | Aggregator | +50 reputation, reward paid if the pool covers it, misses reset |
| `record_miss(pubkey)` | Aggregator | −25 reputation. **Never** touches stake |
| `penalize(pubkey, reputation_delta, slash_amount)` | Aggregator | The mechanical outlier penalty |

### Called by the slashing contract

| Function | Caller | Effect |
| --- | --- | --- |
| `slash(pubkey, reputation_delta, slash_amount)` | Slasher | The post-dispute penalty |
| `pay_from_slash_pool(to, amount)` | Slasher | Releases seized stake; traps if the pool cannot cover it |

`penalize` and `slash` are separate entry points with identical bodies on
purpose. Two callers with different authority and different evidentiary
standards — an arithmetic check versus a human vote — should not share a door
where a mistake in one role test silently grants one the other's power.

### Reads

| Function | Returns | Notes |
| --- | --- | --- |
| `get_node(pubkey)` | `Option<NodeView>` | `None` rather than a trap: the aggregator calls this for keys that were never registered |
| `owner_of(pubkey)` | `Option<Address>` | Who bonded the stake |
| `weight_of(pubkey)` | `u32` | Voting weight in bps; zero for unknown, jailed or exiting |
| `list_nodes()` | `Vec<BytesN<32>>` | |
| `total_weight()` | `u32` | Distinguishes "not enough nodes reported" from "the network has too little healthy weight to reach quorum at all" |

### Weight

| Reputation | Status | Weight |
| --- | --- | --- |
| ≥ 7 000 | Active | 10 000 bps |
| 3 000 – 6 999 | Active | 5 000 bps |
| < 3 000 | Jailed | 0 |
| — | Exiting | 0 |

A step function rather than a curve: trivial to reason about, cheap to compute,
and it gives an operator a target ("get above 7 000") instead of a model.

This number decides two things, not one. It is how much a node's price counts
towards the median, and it is how much its owner's ballot counts when the
[dispute committee is elected](#electing-the-committee). Nothing about who is
entitled to a say is maintained twice.

---

## Aggregator

Verifies signed submissions, takes a reputation-weighted median, publishes.

### Submission

```
submit_price(feed, pubkey, price, timestamp, confidence_bps, nonce, signature) -> bool
```

Returns `true` when this submission closed the round — whether or not the round
managed to publish a price. **No `require_auth`.** Authority is the Ed25519
signature over the canonical payload; anyone may relay.

Checks, in order, cheapest first so that a flood of invalid submissions cannot
be used to spend the network's CPU budget:

1. Feed exists and is enabled
2. `price > 0`
3. `timestamp` within `max_future_drift` ahead and `max_staleness` behind ledger time
4. `nonce` strictly greater than the last accepted for this `(node, feed)`
5. The registry gives the key a non-zero weight
6. `ed25519_verify` over the canonical 117-byte payload — **consensus-breaking**
7. Not a second submission from this node in the open round

A round closes when it holds `quorum` distinct nodes **and** `min_weight_bps` of
total weight. Both, not either.

Nothing on chain calls `sweep_absent`. It is upkeep the contracts permit and do
not perform, so it happens only if somebody off chain does it: `aphelion-node`
will, when `upkeep.sweep_absent` is set, and `aphelion-node sweep --commit` does
it once by hand. A deployment where nobody runs either has nodes carrying weight
for rounds they stopped taking part in;
`scripts/verify-deployment.sh` counts them.

| Function | Caller | Notes |
| --- | --- | --- |
| `sweep_absent(pubkeys) -> u32` | Anyone | Charges a missed round to nodes silent past `absence_threshold`. One charge per silence, not per sweep. Returns how many of the offered keys were actually charged |
| `pending_round(feed)` | Anyone | The round currently accepting submissions |

### Reads

| Function | Returns | Notes |
| --- | --- | --- |
| `get_price(feed)` | `Option<PriceData>` | `None` if never published — must not read as zero |
| `get_price_checked(feed, max_age)` | `PriceData` | Traps with `StalePrice`. Prefer this |
| `get_twap(feed, window)` | `i128` | Traps with `InsufficientHistory` if the ring does not span the window |
| `history(feed)` | `Vec<Observation>` | Oldest first |
| `feeds()` / `feed_config(feed)` | | |
| `last_nonce(pubkey, feed)` | `u64` | What a restored node reads to skip past spent nonces |
| `ledger_time()` | `u64` | For client-side skew checks |

`PriceData.timestamp` is the **oldest** contributing observation, so a
consumer's freshness check cannot be satisfied by one fast node in an otherwise
stale round. `num_nodes`, `deviation` and `confidence_bps` come from the in-band
submissions only: a node being penalised must not also widen the interval its
consumers rely on.

### Metering

| Function | Caller | Notes |
| --- | --- | --- |
| `deposit(from, amount)` | A consumer | Prepay for metered reads |
| `refund(consumer, amount)` | The consumer | Reclaim unspent balance |
| `get_price_metered(consumer, feed, max_age)` | The consumer | Charges `read_fee`, then behaves as `get_price_checked` |
| `balance(consumer)` / `collected_fees()` | Anyone | |
| `forward_fees()` | Anyone | Pushes collected fees into the registry's reward pool |

Fees accrue and are forwarded in a batch: a token transfer on every read would
cost more than the read it charges for.

### Administration

| Function | Caller | Notes |
| --- | --- | --- |
| `initialize(config)` | Admin, once | Takes the whole `Config` as a struct — sixteen positional numbers is how a deployment script sets the slash amount to the reward |
| `set_config(config)` | Admin | Validated as a whole, because the parameters constrain each other |
| `set_feed(feed, enabled, heartbeat, min_nodes)` | Admin | Adds or reconfigures; `min_nodes: 0` means "use the network quorum" |
| `get_config()` | Anyone | |
| `param_bounds()` | Anyone | The range every governable parameter must stay inside |

#### Parameter bounds

`set_config` is reachable only by the admin, which in a deployed network is the
timelock. The timelock decides *when* a change lands and has nothing to say
about *what to*, so the contract bounds the values themselves:

| Parameter | Range | Why the edge is there |
| --- | --- | --- |
| `quorum` | 2 – 100 | One makes the aggregator a relay for a single key |
| `min_weight_bps` | 1 – 1 000 000 | Above it no achievable set of submissions closes a round |
| `max_deviation_bps` | 1 – 10 000 | Above 100% nothing can fall outside the band, so lying costs nothing |
| `max_staleness` | 1 s – 1 day | Past a day, "the current price" describes nothing |
| `max_future_drift` | 0 – 1 hour, and `< max_staleness` | Otherwise a price can be not-yet-stale and never current |
| `min_round_interval` | 0 – 1 day | A feed publishing once a day is one nothing can borrow against |
| `round_timeout` | 1 s – 1 day | |
| `absence_threshold` | 1 s – 30 days, and `> round_timeout` | Otherwise nodes are charged for absence while still on time |
| `history_len` | 1 – 1 000 | Read in full to compute a TWAP |
| `outlier_rep_penalty` | 0 – 10 000 | The registry's whole reputation scale |

A value outside a range traps with `ParameterOutOfRange` (#6); a pair that is
each in range and cannot both hold traps with `InconsistentConfig` (#7). The
non-negative fee checks remain `InvalidConfig` (#4) — those are values that
could never work at all, rather than values that work and mean nothing.

The bounds are wide on purpose. They are not a view about how the network
should be tuned, which is what governance is for; they rule out the values at
which a parameter stops being the guarantee its name claims.

---

## Slashing

Disputes that arithmetic cannot settle: collusion across rounds, a stolen key, a
venue manipulated on purpose.

```
  open_dispute ──▶ vote ──▶ resolve ──▶ [appeal ──▶ vote ──▶ resolve] ──▶ settle
```

| Function | Caller | Notes |
| --- | --- | --- |
| `open_dispute(reporter, accused, feed, round_id, evidence) -> u64` | Anyone | Posts `dispute_bond`. `(accused, feed, round_id)` may be filed once |
| `vote(member, dispute_id, uphold)` | A committee member | Once per voting round. Refused if the member owns the accused node |
| `resolve(dispute_id) -> DisputeStatus` | Anyone, after the voting period | Upheld only on quorum *and* a majority. A tie favours the accused |
| `appeal(appellant, dispute_id)` | Either side, once, in the appeal window | Posts `appeal_bond`, clears the votes, reopens voting |
| `settle(dispute_id)` | Anyone, after the appeal window | Moves stake and bonds |
| `remove_member(member)` | Admin | Cannot shrink the committee below its own quorum. There is no `add_member` — see [Electing the committee](#electing-the-committee) |
| `initialize` / `set_config` / `get_config` / `committee` | Admin / anyone | |
| `get_dispute(id)` / `vote_of(id, member)` / `dispute_for(accused, feed, round)` / `dispute_count()` | Anyone | |

Where the money goes at settlement:

| Outcome | Dispute bond | Appeal bond | Stake |
| --- | --- | --- | --- |
| Upheld | Returned to the reporter | To the reporter, unless the appeal changed the outcome | `slash_amount` seized; `reporter_reward` paid from the slash pool, capped at what it holds |
| Dismissed | To the accused's owner | To the owner, unless the appeal changed the outcome | Untouched |

The reporter's reward is paid **from the pool**, in a separate step, never
carved out of the seizure. A reward that came out of the penalty would give the
committee a standing financial interest in finding against nodes, scaling with
the size of the penalty.

### Electing the committee

Seats are won in a ballot of the node operators, weighted by the same
`weight_of` the aggregator uses to weigh a price. A deployment appoints its
first committee — an election needs an electorate, and at genesis there are no
nodes — and that committee serves `term_length` like any other.

```
  open_election ──▶ nominate ──▶ cast_ballot ──▶ finalize_election
   (anyone, once     (an operator,   (one ballot      (anyone, once the
    the term is       on a node       per node,        ballot closes)
    served)           they own)       weighted)
```

| Function | Caller | Notes |
| --- | --- | --- |
| `open_election() -> u64` | Anyone, once the term is served | One election at a time |
| `nominate(candidate, node)` | An operator, during the nomination period | `node` must be theirs and carry weight. Once per election per person, however many nodes they run |
| `cast_ballot(voter, node, candidate)` | A node's owner, during the ballot | One ballot per **node**; its weight is read now, not at the count |
| `finalize_election() -> ElectionStatus` | Anyone, once the ballot closes | Seats the top `seats` candidates, replacing every seat |
| `get_election(id)` / `election_phase(id)` / `candidates(id)` / `ballot_of(id, node)` | Anyone | `candidates` is the running tally, readable mid-ballot |
| `current_election()` / `next_election()` / `election_count()` | Anyone | |

What the rules are, and why:

| Rule | Reason |
| --- | --- |
| The electorate is the node set, weighted by `weight_of` | The committee's power is to take an operator's stake. Weight is already the network's Sybil-resistant answer to "how much should this identity count" |
| One ballot names **one** candidate, for a race with `seats` winners | A slate ballot would let a bare majority of weight take *every* seat. Naming one means a faction holding more than `1/(seats+1)` of the weight can seat somebody regardless |
| A candidate must stand on a node they own | Anyone can make an address; a node carrying weight costs a bond. It also makes the committee operators judging operators — a real cost, bounded by the per-dispute conflict-of-interest rule |
| A candidate with no votes takes no seat | An unopposed slate must not take the committee on zero turnout |
| Eligibility is rechecked at the count | An operator jailed during the ballot does not take a seat on votes cast before anyone knew |
| Ties go to whoever stood first | Deterministic, and not computable your way around after the ballot closes |
| Fewer eligible winners than `quorum` → nobody is seated, incumbents stay | Vacating the seats would let an attacker switch slashing off for everybody by suppressing turnout. The cost is that a committee nobody replaces holds over indefinitely |
| A failed election may be retried at once; a successful one starts a fresh term | Failing already cost a nomination period and a ballot, so there is nothing to spam with |
| `seats` and `quorum` are captured when the election opens | A `set_config` landing mid-ballot must not move the bar under votes already cast |
| A reseating binds votes cast after it, and does not disturb an open dispute's existing votes | Voiding them would let an election timed against a dispute erase evidence. Quorum counts votes cast, so a reseating can never make a dispute *easier* to resolve |

Admin — the governance timelock — keeps `remove_member` and has no
`add_member`. A power that can only subtract cannot install anybody, so the
worst a captured admin key achieves is a smaller committee, and it cannot
shrink one below its quorum either. A removed seat stays empty until an
election fills it: promoting a runner-up would be an appointment wearing an
election's clothes.

---

## Governance

The timelock that holds `admin` on the other three. It can do nothing the key
before it could not; what it removes is *instantly* and *invisibly*.

```
  propose ──▶ waiting ──▶ ready ──▶ execute (anyone)
                 │           │
                 │           └──▶ expired, once the grace period runs out
                 └── cancel, by the guardian or the proposer
```

| Function | Caller | Notes |
| --- | --- | --- |
| `initialize(config, proposers)` | The **guardian**, once | Authorised by the guardian rather than by the deployer: it is the one role this contract cannot appoint for itself afterwards. At least one proposer, and no address twice |
| `propose(proposer, target, function, args, description) -> u64` | A proposer | Stores the call exactly as it will be made. `eta` and `expires_at` are fixed here |
| `execute(id)` | **Anyone**, between `eta` and `expires_at` | Dispatches internally when `target` is the timelock itself, and invokes the target contract otherwise |
| `cancel(canceller, id)` | The guardian, or the proposal's own proposer | Permanent; there is no un-cancel |
| `state(id)` | Anyone | `Waiting` · `Ready` · `Expired` · `Executed` · `Cancelled` — the stored status combined with the clock, which is what a watcher actually wants |
| `get_proposal(id)` / `proposal_count()` | Anyone | `get_proposal` returns `None` for an unknown id; every other function traps on one |
| `get_config()` / `proposers()` | Anyone | |

`description` is a URL or content hash, not the rationale itself — as with
dispute evidence, ledger space is the wrong place for prose, and a hash is
enough to prove nobody rewrote it afterwards.

[`scripts/govern.sh`](../scripts/govern.sh) wraps these — `propose`, `list`,
`show`, `execute`, `cancel` — reading the contract ids from the deployment
record, so a proposal does not have to be reassembled by hand across the days
that separate queueing it from executing it. It holds no privilege of its own:
whoever runs it signs with their own key, and the contract decides whether that
key may do what is being asked.

| `Config` | Bounds | |
| --- | --- | --- |
| `guardian` | — | May cancel a queued proposal, and may do nothing else |
| `delay` | 1–30 days | Between publication and executability. This is the whole feature: the window in which an operator who dislikes a change can unbond before it binds them |
| `grace_period` | 1–30 days | How long it stays executable afterwards. Past it the proposal is dead and has to be queued again — delay included |

What the rules are, and why:

| Rule | Reason |
| --- | --- |
| The call is stored as `(target, function, args)` | What the delay publishes is then the change itself, not a description of it that could turn out to differ |
| `eta` and `expires_at` are captured when the proposal is queued, not recomputed at execution | A proposal that shortens the delay must not shorten the wait of the proposals queued beside it, or a proposer queueing two together would escape the delay in one step |
| Execution is permissionless | The call was fixed when it was queued and the delay is a fact about the clock, so nothing is left for the caller to decide. Restricting it to the proposer would add no safety and would let one strand a change everybody had agreed to by going quiet |
| A proposal is marked executed *before* its target is called | Soroban refuses re-entry today, but a proposal that could re-enter `execute` during its own call would otherwise run twice off one delay |
| The guardian can only say no | A stolen guardian key stalls governance until a proposal moves the guardian; it cannot move stake, prices or admin rights anywhere. That is what makes it a key worth handing to somebody other than the proposer |
| There is no un-cancel, and an expired proposal cannot be cancelled | Reviving one would return a call to the executable state without a fresh delay. Cancelling a dead one would write a decision into the record that the clock had already made |
| A proposal against the timelock is decoded when it is queued, and again when it runs | Publishing a governance change, waiting out the delay and only then discovering it names a function that does not exist would spend the delay on nothing |
| The last proposer cannot be removed | Every route into this contract's own configuration is a proposal, so a proposer set emptied by accident could not be refilled: every parameter of the network would freeze where it stood, permanently |

### Governing itself

Its own guardian, delay and proposer set change the same way as anything else:
a proposal naming this contract as its `target`, which serves the delay first.
Changing the delay therefore takes the delay — a timelock whose delay could be
set to zero in one transaction is a timelock for exactly as long as nobody
attacks it.

| `function` | `args` | Effect |
| --- | --- | --- |
| `set_config` | `[Config]` | Replaces guardian, delay and grace period together, re-validated against the same bounds as `initialize` |
| `add_proposer` | `[Address]` | |
| `remove_proposer` | `[Address]` | Refused on the last one |

These three are **not** entry points. Soroban refuses contract re-entry, so
`execute` cannot reach them by invoking this contract and dispatches them
internally instead. The consequence is worth more than the mechanism: there is
no public `set_config` here to protect, and an entry point that does not exist
cannot be left unguarded by a later edit.

`MIN_DELAY` and `MAX_DELAY` bound the delay even so. A proposal cannot collapse
it to nothing, and cannot set it so long that governance is dead.

### What it governs

Nothing here is restricted to Aphelion's own contracts: what makes a target
governable is that it named this contract as its `admin`.

| Contract | Handed over with |
| --- | --- |
| Registry | `set_admin(governance)` |
| Aggregator | `set_config(...)` with `admin` replaced |
| Slashing | `set_config(...)` with `admin` replaced |

[`scripts/deploy.sh`](../scripts/deploy.sh) does this **last**, after the feeds
are configured and the first committee seated, because a deployment that had to
serve a day's delay to add its first feed would never finish. The three calls
are independent and one-way, so a failure part-way leaves the deploying key
holding whichever contracts it has not reached yet — finish those by hand
rather than rerunning the script, which would deploy a second set of contracts
instead of resuming this one. `APHELION_SKIP_HANDOVER=1` skips it entirely and
leaves the admin key in place: reasonable while iterating on a throwaway
deployment, wrong for one anybody relies on.

### What this costs

An urgent parameter change now takes a day, and that is a real loss rather than
a detail. If a feed has to stop being trusted *now*, this contract is not the
instrument — a consumer's own `max_age` argument, the aggregator's out-of-band
rejection and an operator's freedom to stop signing all act in seconds, need
nobody's permission, and unlike an emergency admin power none of them can be
aimed at anything else.

---

## Consumer example

A collateralised vault. Not a lending protocol — no interest, no per-asset risk
configuration, no bad-debt policy — but a complete worked example of reading an
oracle the way one has to.

| Function | Caller |
| --- | --- |
| `deposit_collateral(user, amount)` / `withdraw_collateral(user, amount)` | The borrower |
| `borrow(user, amount)` / `repay(user, amount)` | The borrower |
| `liquidate(liquidator, user, repay_amount) -> i128` | Anyone |
| `fund(from, amount)` | A liquidity supplier |
| `position(user)` / `health_bps(user)` | Anyone |
| `conservative_unit_price()` / `favourable_unit_price()` | Anyone |

The one thing to copy: the same collateral is worth two different numbers,
depending on which way being wrong hurts.

- **To lend**: the *lower* of spot and TWAP, minus the network's confidence
  half-width. Being optimistic here means lending too much against too little.
- **To liquidate**: the *higher* of spot and TWAP, plus the half-width. Being
  pessimistic here means selling a solvent borrower's position over a momentary
  dip on one venue.

Both read the same two inputs. The asymmetry is entirely in which error the
vault is willing to make.

---

## Error codes

Soroban returns these as `Error(Contract, #n)`.

### Registry

| # | Name | |
| ---: | --- | --- |
| 1 | `AlreadyInitialized` | |
| 2 | `NotInitialized` | |
| 3 | `NotAdmin` | |
| 4 | `NotAggregator` | |
| 5 | `NotSlasher` | |
| 6 | `NodeNotFound` | |
| 7 | `NodeAlreadyRegistered` | |
| 8 | `StakeTooLow` | Below `min_stake` |
| 9 | `StillBonded` | Withdrawal before `unbonding_until` |
| 10 | `NodeExiting` | |
| 11 | `InvalidAmount` | |
| 12 | `RewardPoolExhausted` | |
| 13 | `SlashPoolExhausted` | |
| 14 | `InvalidConfig` | A zero `unbonding_period` or `jail_period` |
| 15 | `NotJailed` | `release` on a node that is not in jail |
| 16 | `StillJailed` | `release` before `jailed_until` |

### Aggregator

| # | Name | |
| ---: | --- | --- |
| 1–3 | `AlreadyInitialized`, `NotInitialized`, `NotAdmin` | |
| 4 | `InvalidConfig` | A configuration that could never work |
| 5 | `NotContractAddress` | An account address where a contract belongs |
| 10–12 | `UnknownFeed`, `FeedDisabled`, `FeedAlreadyExists` | |
| 20 | `BadSignature` | Usually surfaces as a host trap: `ed25519_verify` does not return on failure |
| 21 | `NotAuthorizedNode` | Unregistered, jailed or exiting |
| 22 | `NonceNotIncreasing` | |
| 23 | `StaleObservation` | |
| 24 | `FutureObservation` | |
| 25 | `DuplicateSubmission` | Second vote in one round |
| 26 | `RoundTooSoon` | Inside `min_round_interval` of the last publication |
| 27 | `InvalidPrice` | Non-positive |
| 28 | `MathOverflow` | |
| 30 | `NoPrice` | Never published |
| 31 | `StalePrice` | Older than the caller's `max_age` |
| 32 | `InsufficientHistory` | The ring does not span the requested TWAP window |
| 33 | `InvalidWindow` | |
| 40 | `InsufficientBalance` | |
| 41 | `InvalidAmount` | |

### Slashing

| # | Name | |
| ---: | --- | --- |
| 1–5 | `AlreadyInitialized`, `NotInitialized`, `NotAdmin`, `InvalidConfig`, `InvalidAmount` | |
| 10, 12 | `NotCommitteeMember`, `CommitteeTooSmall` | |
| 11 | `AlreadyCommitteeMember` | The same address twice in a genesis committee: it would count twice towards quorum and vote once |
| 20 | `UnknownDispute` | |
| 21 | `UnknownNode` | The accused key is not registered |
| 22 | `DuplicateDispute` | This allegation has been filed |
| 23 | `WrongPhase` | |
| 24 | `VotingClosed` | |
| 25 | `VotingOpen` | Resolving before the deadline |
| 26 | `AlreadyVoted` | |
| 27 | `ConflictOfInterest` | An operator voting on their own node |
| 28 | `AppealWindowOpen` | Settling too early |
| 29 | `AppealWindowClosed` | Appealing too late |
| 30 | `AlreadyAppealed` | |
| 31 | `AlreadySettled` | |
| 40 | `ElectionRunning` | One at a time |
| 41 | `UnknownElection` | No election running, or no election with that id |
| 42 | `TermNotServed` | Opening one before the sitting committee's term ends |
| 43 | `WrongElectionPhase` | Nominating after nominations closed, voting before they did, counting early or twice |
| 44 | `AlreadyNominated` | |
| 45 | `NotCandidate` | A ballot for somebody who did not stand |
| 46 | `AlreadyBalloted` | This node has voted in this election |
| 47 | `NotEligible` | The node is unregistered, not yours, or carries no weight |

### Governance

| # | Name | |
| ---: | --- | --- |
| 1–2 | `AlreadyInitialized`, `NotInitialized` | |
| 4 | `InvalidConfig` | A delay or grace period outside one to thirty days |
| 10 | `NotProposer` | The address may not queue proposals |
| 11 | `AlreadyProposer` | Including the same address twice in a genesis proposer set |
| 12 | `NoProposersLeft` | An empty genesis set, or removing the last proposer |
| 13 | `NotCancellable` | Neither the guardian nor the proposal's own proposer |
| 20 | `UnknownProposal` | |
| 21 | `WrongPhase` | Already executed or cancelled |
| 22 | `StillWaiting` | The delay has not been served |
| 23 | `Expired` | The grace period ran out. Queue it again, and serve the delay again |
| 24 | `UnknownAction` | A proposal against the timelock naming something it cannot do to itself |
| 25 | `InvalidArguments` | The arguments do not fit the action they were queued for |

### Consumer example

| # | Name | |
| ---: | --- | --- |
| 1–4 | `AlreadyInitialized`, `NotInitialized`, `InvalidConfig`, `InvalidAmount` | |
| 10–13 | `NoPosition`, `Undercollateralized`, `ExceedsPosition`, `InsufficientLiquidity` | |
| 20–22 | `NotLiquidatable`, `RepayExceedsDebt`, `SeizureExceedsCollateral` | |
| 30–31 | `MathOverflow`, `UnsupportedDecimals` | |

---

## The interface the node depends on

Changing any of these signatures breaks running nodes, so they are listed here
as the contract between the two halves of the repository. They are grouped by
what stops working when one of them changes, because that is not the same for
all of them.

**The round loop.** A break here stops the node publishing.

| Contract | Function | Used by |
| --- | --- | --- |
| Aggregator | `ledger_time()` | Every round, to check the node's clock before signing |
| Aggregator | `submit_price(feed, pubkey, price, timestamp, confidence_bps, nonce, signature)` | Submission |
| Aggregator | `get_price(feed)` | `/v1/prices/{feed}`, to report divergence from the network |
| Aggregator | `last_nonce(pubkey, feed)` | Startup, to move past nonces a restored database has forgotten |
| Registry | `get_node(pubkey)` | `/v1/node`, and the cached standing behind `/health` |

**Absence sweeps.** A break here stops the node doing upkeep on other people's
records; it goes on publishing. See [`engine::upkeep`](../crates/aphelion-node/src/engine/upkeep.rs).

| Contract | Function | Used by |
| --- | --- | --- |
| Registry | `list_nodes()` | The candidate set for a sweep |
| Aggregator | `get_config()` | The absence threshold, read from the chain rather than mirrored in config |
| Aggregator | `sweep_absent(pubkeys)` | Charging a missed round |

**Committee participation.** A break here stops the node reporting disputes and
elections; it goes on publishing and sweeping. Every read is through
[`CommitteeClient`](../crates/aphelion-node/src/chain/committee.rs), and every
write is authorised by the account that bonded the stake rather than by the
submitter.

| Contract | Function | Used by |
| --- | --- | --- |
| Registry | `owner_of(pubkey)` | Establishing which account may act for this node |
| Slashing | `get_config()` | Quorum, bonds, and the length of every window |
| Slashing | `committee()` | Whether this operator holds a seat |
| Slashing | `dispute_count()`, `get_dispute(id)`, `vote_of(id, member)` | The dispute scan behind `duties` |
| Slashing | `current_election()`, `next_election()`, `get_election(id)`, `candidates(id)`, `ballot_of(id, node)` | The election half of `duties` |
| Slashing | `open_dispute`, `vote`, `resolve`, `appeal`, `settle` | `aphelion-node dispute ...` |
| Slashing | `open_election`, `nominate`, `cast_ballot`, `finalize_election` | `aphelion-node election ...` |

One decoding rule spans all of them, and it is the one most likely to bite a
future contract change: a status or phase the node does not recognise is an
error rather than a default. A dispute whose `status` this build cannot name is
refused at the point it is read, because the duty derived from that status is
whether to tell an operator to appeal — and guessing wrong in the reassuring
direction costs them their stake.

The canonical signing payload is the fifth and most important part of that
contract, and the one that cannot be checked at compile time. It is pinned by
[`tests/vectors/price_message.json`](../tests/vectors/price_message.json),
asserted from `aphelion-core`, from the aggregator, and generated by a third
implementation in Python. The aggregation arithmetic is pinned the same way by
[`tests/vectors/aggregation.json`](../tests/vectors/aggregation.json).
