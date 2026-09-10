#!/usr/bin/env bash
#
# Deploy and wire the Aphelion contracts.
#
# Usage:
#   APHELION_STELLAR_SECRET=S... APHELION_ADMIN_ACCOUNT=G... scripts/deploy.sh
#
# The three contracts refer to each other -- the registry only accepts
# reputation changes from the aggregator, and only accepts slashes from the
# slashing contract -- so they are all deployed first and initialised
# afterwards. Deploying is what fixes an address; initialising is what teaches
# each contract the others'. Doing it in that order is what avoids the circular
# dependency, and it is why `initialize` is a separate call rather than a
# constructor.
#
# Everything this script decides is printed and confirmed before anything is
# submitted. The defaults below are deliberately conservative rather than
# ambitious. Most are governance values the admin can revise later -- the
# aggregator's and the slashing contract's whole config via `set_config`, the
# registry's admin, aggregator, slasher and minimum stake through their own
# setters. The registry's unbonding period and jail term are the exception:
# nothing can change them after `initialize`, because both are promises made to
# nodes that already bonded stake under them. Choose those two carefully here.
#
# The last thing this script does is hand all three contracts to the governance
# timelock, so "the admin can revise later" means a proposal published in
# advance and executed after a delay, not a key acting in one transaction.
# Everything above is set by the admin account first and handed over
# afterwards, because a deployment that had to serve a day's delay to add its
# first feed would never finish. Set APHELION_SKIP_HANDOVER=1 to keep the admin
# key: reasonable while iterating on a throwaway deployment, wrong for one
# anybody relies on.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="wasm32v1-none"
WASM_DIR="$ROOT/contracts/target/$TARGET/release"

: "${APHELION_STELLAR_SECRET:?set APHELION_STELLAR_SECRET to the deploying account secret seed (S...)}"
: "${APHELION_ADMIN_ACCOUNT:?set APHELION_ADMIN_ACCOUNT to the account that will administer the contracts (G...)}"

NETWORK="${APHELION_NETWORK:-testnet}"
RPC_URL="${APHELION_RPC_URL:-https://soroban-testnet.stellar.org}"
PASSPHRASE="${APHELION_NETWORK_PASSPHRASE:-Test SDF Network ; September 2015}"

# -- registry --------------------------------------------------------------
# 1000 XLM. High enough that an identity costs something, low enough that an
# independent operator can afford one.
MIN_STAKE="${APHELION_MIN_STAKE:-10000000000}"
# Seven days. The window in which a dispute over a node's past submissions can
# still reach its stake after it asks to leave.
UNBONDING="${APHELION_UNBONDING:-604800}"
# One day. What a jailed node waits before `release` returns it to a newcomer's
# standing. Two bounds make this a real penalty rather than a formality:
# comfortably longer than the ~40 rounds it takes an active node to climb from
# the jail threshold back to a newcomer's reputation, so falling below the line
# is never a shortcut; and shorter than UNBONDING, because an operator can
# always unbond, withdraw and register a fresh key instead -- if serving the
# term cost more than that, nobody would serve it and the network would churn
# identities for nothing.
JAIL_PERIOD="${APHELION_JAIL_PERIOD:-86400}"

# -- aggregator ------------------------------------------------------------
QUORUM="${APHELION_QUORUM:-3}"
# Three full-weight nodes, or a larger number of half-weight ones. Both a
# headcount and a weight, so fresh identities cannot close a round on their own.
MIN_WEIGHT_BPS="${APHELION_MIN_WEIGHT_BPS:-20000}"
# 5%. Wide enough to survive a disorderly market, narrow enough that a
# fabricated price is outside it.
MAX_DEVIATION_BPS="${APHELION_MAX_DEVIATION_BPS:-500}"
MAX_STALENESS="${APHELION_MAX_STALENESS:-300}"
MAX_FUTURE_DRIFT="${APHELION_MAX_FUTURE_DRIFT:-30}"
MIN_ROUND_INTERVAL="${APHELION_MIN_ROUND_INTERVAL:-60}"
ROUND_TIMEOUT="${APHELION_ROUND_TIMEOUT:-300}"
ABSENCE_THRESHOLD="${APHELION_ABSENCE_THRESHOLD:-3600}"
REWARD_PER_SUBMISSION="${APHELION_REWARD_PER_SUBMISSION:-0}"
OUTLIER_REP_PENALTY="${APHELION_OUTLIER_REP_PENALTY:-500}"
OUTLIER_SLASH="${APHELION_OUTLIER_SLASH:-0}"
HISTORY_LEN="${APHELION_HISTORY_LEN:-64}"
READ_FEE="${APHELION_READ_FEE:-0}"
FEEDS="${APHELION_FEEDS:-BTC_USD ETH_USD XLM_USD}"
HEARTBEAT="${APHELION_HEARTBEAT:-300}"

# -- slashing --------------------------------------------------------------
COMMITTEE="${APHELION_COMMITTEE:-$APHELION_ADMIN_ACCOUNT}"
DISPUTE_QUORUM="${APHELION_DISPUTE_QUORUM:-1}"
VOTING_PERIOD="${APHELION_VOTING_PERIOD:-259200}"   # 3 days
APPEAL_PERIOD="${APHELION_APPEAL_PERIOD:-172800}"   # 2 days
DISPUTE_BOND="${APHELION_DISPUTE_BOND:-1000000000}" # 100 XLM
APPEAL_BOND="${APHELION_APPEAL_BOND:-3000000000}"   # 300 XLM
DISPUTE_REP_PENALTY="${APHELION_DISPUTE_REP_PENALTY:-2000}"
DISPUTE_SLASH="${APHELION_DISPUTE_SLASH:-5000000000}" # 500 XLM
REPORTER_REWARD="${APHELION_REPORTER_REWARD:-500000000}"

# -- the committee's elections ---------------------------------------------
# $COMMITTEE above is only the committee this deployment *starts* with. There
# is no way around appointing that one -- an election needs an electorate, and
# at genesis there are no nodes -- but it serves a term like any other and is
# replaced by a vote of the operators, not by the admin key. Nothing can add a
# member to a committee; the admin can only remove one.
#
# Five seats. Odd, so a committee at full strength cannot deadlock, and small
# enough that every member is a person somebody can name.
SEATS="${APHELION_SEATS:-5}"
# Three days to stand. Long enough that an operator who reads the ledger
# weekly can still enter a race they did not know was running.
NOMINATION_PERIOD="${APHELION_NOMINATION_PERIOD:-259200}"
# Seven days to vote. Voting costs a transaction per node and the electorate
# is spread across every timezone that runs one.
ELECTION_PERIOD="${APHELION_ELECTION_PERIOD:-604800}"
# Ninety days a seat. Elections are permissionless, so this is the only thing
# stopping anyone from running one continuously; against that, a term nobody
# can shorten is a committee nobody can replace for that long.
TERM_LENGTH="${APHELION_TERM_LENGTH:-7776000}"

# -- governance ------------------------------------------------------------
# The timelock that administers the other three once this script is done.
#
# One day. The window between a parameter change being published and becoming
# executable, which is the window an operator who dislikes it has to unbond.
# The contract's own floor, and the shortest delay that is still a delay: an
# emergency that cannot survive a day is not answered by governance anyway.
TIMELOCK_DELAY="${APHELION_TIMELOCK_DELAY:-86400}"
# Seven days to execute a proposal once its delay is served. Past that it is
# dead and has to be queued again -- delay included -- which is what stops a
# proposal from last spring being fired at a network it no longer suits.
TIMELOCK_GRACE="${APHELION_TIMELOCK_GRACE:-604800}"
# May cancel a queued proposal, and may do nothing else: it cannot queue one,
# cannot execute one, and cannot touch the timelock's own configuration. That
# is what makes it a key worth holding somewhere other than the proposer --
# which the default below is not. See the note this script prints at the end.
GUARDIAN="${APHELION_GUARDIAN:-$APHELION_ADMIN_ACCOUNT}"
# `Governance::initialize` is authorised by the guardian, so the guardian's key
# is the one that has to sign it -- not the account paying for the deployment.
GUARDIAN_SECRET="${APHELION_GUARDIAN_SECRET:-}"
# Who may queue proposals. Space-separated, like FEEDS.
PROPOSERS="${APHELION_PROPOSERS:-$APHELION_ADMIN_ACCOUNT}"

if [[ -n "${APHELION_SKIP_HANDOVER:-}" ]]; then
    HANDOVER=0
else
    HANDOVER=1
fi

command -v stellar >/dev/null 2>&1 || {
    echo "error: the stellar CLI is not installed." >&2
    echo "see https://developers.stellar.org/docs/tools/developer-tools/cli/stellar-cli" >&2
    exit 69
}
command -v jq >/dev/null 2>&1 || {
    echo "error: jq is required to build the configuration arguments." >&2
    exit 69
}

# `Registry::initialize` refuses this combination too, and it is the authority:
# a deployment script is a convenience, and nothing stops an operator invoking
# the contract directly. Checked here as well so the failure arrives as a
# sentence about incentives rather than as contract error #14 from inside a
# transaction the operator has already paid for.
#
# The other bound -- that a jail term outlast the climb from the jail threshold
# back to a newcomer's reputation -- stays here and only here. It depends on the
# aggregator's round cadence, which the registry has no way to read.
if (( JAIL_PERIOD >= UNBONDING )); then
    echo "error: jail term ($JAIL_PERIOD s) must be shorter than the unbonding" >&2
    echo "period ($UNBONDING s), or no operator would ever serve it: unbonding," >&2
    echo "withdrawing and registering a fresh key would be the faster way back." >&2
    exit 64
fi

# Both bounds are the governance contract's own, checked here for the same
# reason as the jail term above: a sentence beats contract error #4 arriving
# from inside a transaction that has already been paid for.
if (( TIMELOCK_DELAY < 86400 || TIMELOCK_DELAY > 2592000 )); then
    echo "error: timelock delay ($TIMELOCK_DELAY s) must be between one day" >&2
    echo "(86400) and thirty (2592000). Below the floor the delay stops being" >&2
    echo "one; above the ceiling it stops being governance." >&2
    exit 64
fi

# Checked here rather than left to contract error #4, for the same reason as
# the jail term and the timelock delay: a sentence beats an error code
# arriving from inside a transaction that has already been paid for.
if (( SEATS < DISPUTE_QUORUM )); then
    echo "error: APHELION_SEATS ($SEATS) is below APHELION_DISPUTE_QUORUM" >&2
    echo "($DISPUTE_QUORUM). Every election would then seat a committee that" >&2
    echo "could not reach quorum at full strength, which dismisses every" >&2
    echo "dispute filed against anybody." >&2
    exit 64
fi

if (( NOMINATION_PERIOD < 86400 || ELECTION_PERIOD < 86400 )); then
    echo "error: the nomination period ($NOMINATION_PERIOD s) and the ballot" >&2
    echo "($ELECTION_PERIOD s) must each be at least a day. A window shorter" >&2
    echo "than that is one an operator can miss by being asleep, and an" >&2
    echo "election only the attentive can enter is not much of an election." >&2
    exit 64
fi

if (( TERM_LENGTH < 2592000 || TERM_LENGTH > 31536000 )); then
    echo "error: the term ($TERM_LENGTH s) must be between thirty days" >&2
    echo "(2592000) and a year (31536000). Below the floor the network spends" >&2
    echo "its time electing rather than operating; above the ceiling a" >&2
    echo "committee nobody can replace sits for longer than anyone agreed to." >&2
    exit 64
fi

if (( TIMELOCK_GRACE < 86400 || TIMELOCK_GRACE > 2592000 )); then
    echo "error: timelock grace period ($TIMELOCK_GRACE s) must be between one" >&2
    echo "day (86400) and thirty (2592000). A window shorter than a day can be" >&2
    echo "missed by an operator who was simply asleep." >&2
    exit 64
fi

# The guardian authorises `initialize`, and the stellar CLI signs with exactly
# one account. If the guardian is somebody else -- which is the arrangement
# worth having -- their secret has to be here too.
if [[ "$GUARDIAN" != "$APHELION_ADMIN_ACCOUNT" && -z "$GUARDIAN_SECRET" ]]; then
    echo "error: APHELION_GUARDIAN is $GUARDIAN, which is not the admin" >&2
    echo "account, so the deploying key cannot authorise the timelock's" >&2
    echo "initialize on its behalf. Either set APHELION_GUARDIAN_SECRET to" >&2
    echo "that account's seed, or deploy with the guardian left at the admin" >&2
    echo "account and move it afterwards with a proposal." >&2
    exit 64
fi
: "${GUARDIAN_SECRET:=$APHELION_STELLAR_SECRET}"

if [[ ! -f "$WASM_DIR/aphelion_registry.wasm" ]]; then
    echo "No build artefacts found; building..."
    "$ROOT/scripts/build-contracts.sh"
fi

invoke_as() {
    local secret="$1" contract="$2"
    shift 2
    stellar contract invoke \
        --id "$contract" \
        --source-account "$secret" \
        --rpc-url "$RPC_URL" \
        --network-passphrase "$PASSPHRASE" \
        -- "$@"
}

invoke() {
    local contract="$1"
    shift
    invoke_as "$APHELION_STELLAR_SECRET" "$contract" "$@"
}

deploy() {
    stellar contract deploy \
        --wasm "$WASM_DIR/$1" \
        --source-account "$APHELION_STELLAR_SECRET" \
        --rpc-url "$RPC_URL" \
        --network-passphrase "$PASSPHRASE" 2>/dev/null
}

TOKEN="${APHELION_TOKEN:-}"
if [[ -z "$TOKEN" ]]; then
    echo "Resolving the native asset contract..."
    # No --source-account here, unlike every other call: deriving a builtin
    # asset's contract id is pure arithmetic over the asset and the network
    # passphrase, so the CLI stopped accepting an account for it. Passing one
    # is an "unexpected argument" error, not a warning.
    TOKEN="$(stellar contract id asset \
        --asset native \
        --rpc-url "$RPC_URL" \
        --network-passphrase "$PASSPHRASE" 2>/dev/null)"
fi

cat <<SUMMARY

Aphelion deployment
  network            : $NETWORK
  rpc                : $RPC_URL
  admin              : $APHELION_ADMIN_ACCOUNT
  token              : $TOKEN
  minimum stake      : $MIN_STAKE stroops
  unbonding period   : $UNBONDING seconds
  jail term          : $JAIL_PERIOD seconds
  quorum             : $QUORUM nodes / $MIN_WEIGHT_BPS bps
  deviation band     : $MAX_DEVIATION_BPS bps
  round interval     : $MIN_ROUND_INTERVAL seconds
  feeds              : $FEEDS
  dispute committee  : $COMMITTEE
  dispute quorum     : $DISPUTE_QUORUM
  committee seats    : $SEATS, elected every $TERM_LENGTH seconds
  election windows   : $NOMINATION_PERIOD s to stand, $ELECTION_PERIOD s to vote

SUMMARY

read -r -p "Deploy four contracts and initialise them? [y/N] " reply
[[ "$reply" == "y" || "$reply" == "Y" ]] || { echo "aborted"; exit 1; }

echo
echo "==> Deploying registry"
REGISTRY="$(deploy aphelion_registry.wasm)"
echo "    $REGISTRY"

echo "==> Deploying aggregator"
AGGREGATOR="$(deploy aphelion_aggregator.wasm)"
echo "    $AGGREGATOR"

echo "==> Deploying slashing"
SLASHING="$(deploy aphelion_slashing.wasm)"
echo "    $SLASHING"

echo
echo "==> Initialising registry"
invoke "$REGISTRY" initialize \
    --admin "$APHELION_ADMIN_ACCOUNT" \
    --aggregator "$AGGREGATOR" \
    --slasher "$SLASHING" \
    --token "$TOKEN" \
    --min_stake "$MIN_STAKE" \
    --unbonding_period "$UNBONDING" \
    --jail_period "$JAIL_PERIOD"

echo "==> Initialising aggregator"
AGGREGATOR_CONFIG="$(jq -nc \
    --arg admin "$APHELION_ADMIN_ACCOUNT" \
    --arg registry "$REGISTRY" \
    --arg token "$TOKEN" \
    --argjson quorum "$QUORUM" \
    --argjson min_weight_bps "$MIN_WEIGHT_BPS" \
    --argjson max_deviation_bps "$MAX_DEVIATION_BPS" \
    --argjson max_staleness "$MAX_STALENESS" \
    --argjson max_future_drift "$MAX_FUTURE_DRIFT" \
    --argjson min_round_interval "$MIN_ROUND_INTERVAL" \
    --argjson round_timeout "$ROUND_TIMEOUT" \
    --argjson absence_threshold "$ABSENCE_THRESHOLD" \
    --arg reward_per_submission "$REWARD_PER_SUBMISSION" \
    --argjson outlier_rep_penalty "$OUTLIER_REP_PENALTY" \
    --arg outlier_slash "$OUTLIER_SLASH" \
    --argjson history_len "$HISTORY_LEN" \
    --arg read_fee "$READ_FEE" \
    '{admin: $admin, registry: $registry, token: $token,
      quorum: $quorum, min_weight_bps: $min_weight_bps,
      max_deviation_bps: $max_deviation_bps, max_staleness: $max_staleness,
      max_future_drift: $max_future_drift, min_round_interval: $min_round_interval,
      round_timeout: $round_timeout, absence_threshold: $absence_threshold,
      reward_per_submission: $reward_per_submission,
      outlier_rep_penalty: $outlier_rep_penalty, outlier_slash: $outlier_slash,
      history_len: $history_len, read_fee: $read_fee}')"
invoke "$AGGREGATOR" initialize --config "$AGGREGATOR_CONFIG"

for feed in $FEEDS; do
    echo "==> Adding feed $feed"
    invoke "$AGGREGATOR" set_feed \
        --feed "$feed" \
        --enabled true \
        --heartbeat "$HEARTBEAT" \
        --min_nodes 0
done

echo "==> Initialising slashing"
SLASHING_CONFIG="$(jq -nc \
    --arg admin "$APHELION_ADMIN_ACCOUNT" \
    --arg registry "$REGISTRY" \
    --arg token "$TOKEN" \
    --argjson quorum "$DISPUTE_QUORUM" \
    --argjson voting_period "$VOTING_PERIOD" \
    --argjson appeal_period "$APPEAL_PERIOD" \
    --arg dispute_bond "$DISPUTE_BOND" \
    --arg appeal_bond "$APPEAL_BOND" \
    --argjson rep_penalty "$DISPUTE_REP_PENALTY" \
    --arg slash_amount "$DISPUTE_SLASH" \
    --arg reporter_reward "$REPORTER_REWARD" \
    --argjson seats "$SEATS" \
    --argjson nomination_period "$NOMINATION_PERIOD" \
    --argjson election_period "$ELECTION_PERIOD" \
    --argjson term_length "$TERM_LENGTH" \
    '{admin: $admin, registry: $registry, token: $token, quorum: $quorum,
      voting_period: $voting_period, appeal_period: $appeal_period,
      dispute_bond: $dispute_bond, appeal_bond: $appeal_bond,
      rep_penalty: $rep_penalty, slash_amount: $slash_amount,
      reporter_reward: $reporter_reward, seats: $seats,
      nomination_period: $nomination_period, election_period: $election_period,
      term_length: $term_length}')"
COMMITTEE_JSON="$(printf '%s\n' $COMMITTEE | jq -Rc '[.]' | jq -sc 'add')"
invoke "$SLASHING" initialize \
    --config "$SLASHING_CONFIG" \
    --committee "$COMMITTEE_JSON"

mkdir -p "$ROOT/deployments"
RECORD="$ROOT/deployments/$NETWORK.json"
jq -nc \
    --arg network "$NETWORK" \
    --arg passphrase "$PASSPHRASE" \
    --arg rpc "$RPC_URL" \
    --arg admin "$APHELION_ADMIN_ACCOUNT" \
    --arg token "$TOKEN" \
    --arg registry "$REGISTRY" \
    --arg aggregator "$AGGREGATOR" \
    --arg slashing "$SLASHING" \
    --arg deployed_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    '{network: $network, network_passphrase: $passphrase, rpc_url: $rpc,
      admin: $admin, token: $token,
      contracts: {registry: $registry, aggregator: $aggregator, slashing: $slashing},
      deployed_at: $deployed_at}' | jq . > "$RECORD"

cat <<DONE

Deployed.

  registry   : $REGISTRY
  aggregator : $AGGREGATOR
  slashing   : $SLASHING

Written to deployments/$NETWORK.json (gitignored: contract ids are per
deployment, and a committed one is a contract id somebody will paste into the
wrong network).

Next:
  1. Point a node at it:
       registry_contract   = "$REGISTRY"
       aggregator_contract = "$AGGREGATOR"
  2. Register the node:
       APHELION_REGISTRY_CONTRACT=$REGISTRY \\
         scripts/register-node.sh "\$(aphelion-node pubkey)"
  3. Fund the reward pool, if you are paying operators:
       stellar contract invoke --id $REGISTRY ... -- fund_rewards \\
         --from $APHELION_ADMIN_ACCOUNT --amount <stroops>

The aggregator will not publish until $QUORUM nodes carrying
$MIN_WEIGHT_BPS bps between them are submitting. A registered node starts at
half weight, so the first rounds need more nodes than the steady state does.

The dispute committee above is appointed, and only until the term ends. From
$(date -u -d "@$(( $(date -u +%s) + TERM_LENGTH ))" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo "+$TERM_LENGTH seconds") anyone may call
open_election on the slashing contract, and operators elect its replacement
with the nodes they run. Recruit at least $DISPUTE_QUORUM operators willing to
stand before then: an election that draws fewer eligible candidates than the
quorum seats nobody and leaves this committee sitting.
DONE
