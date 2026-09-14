#!/usr/bin/env bash
#
# Read a live deployment and check that it is what it was meant to be.
#
# Usage:
#   scripts/verify-deployment.sh [--strict]
#
#   --strict  treat warnings as failures. What a mainnet gate wants; too
#             strict for a testnet deployment that has no nodes yet.
#
#   Reads deployments/$APHELION_NETWORK.json, or the record named by
#   APHELION_DEPLOYMENT_RECORD. Contract ids can be given directly instead,
#   as APHELION_{REGISTRY,AGGREGATOR,SLASHING,GOVERNANCE}_CONTRACT.
#
# `scripts/deploy.sh` submits a dozen-odd transactions across four contracts
# and then writes a JSON record of what it believes it did. Nothing until now
# read the chain back. That gap matters most exactly where the script says so
# itself: the handover at the end is three independent one-way calls, and a
# failure part-way through leaves the admin key holding whichever contracts it
# did not reach -- a deployment that looks finished, prints no error, and is
# still governed by one key.
#
# So this checks the deployment against the chain rather than against the
# record, and the record against the chain too. Everything it reads is a
# simulated call: it submits nothing, signs nothing and costs nothing, which is
# what makes it safe to run against somebody else's deployment as well as your
# own. Run it after deploying, before announcing a deployment to operators,
# and after any proposal that changes a parameter.
#
# Exit status is 0 when nothing failed, 1 otherwise, so it can gate a release.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

STRICT=0
case "${1:-}" in
    --strict) STRICT=1 ;;
    -h|--help)
        # The usage block is the header down to where the rationale starts,
        # found rather than counted: a line range here is a comment that goes
        # stale the first time the header grows a line.
        awk '/^# `scripts\/deploy.sh`/ { exit } NR >= 3 { sub(/^# ?/, ""); print }' \
            "${BASH_SOURCE[0]}"
        exit 0
        ;;
    "") ;;
    *)
        echo "error: unknown argument '$1'. Only --strict is accepted." >&2
        exit 64
        ;;
esac

NETWORK="${APHELION_NETWORK:-testnet}"
# Overridable so a record somebody sent you can be checked where it lies,
# rather than by copying it over your own deployment's. Nothing is written
# back to it either way.
RECORD="${APHELION_DEPLOYMENT_RECORD:-$ROOT/deployments/$NETWORK.json}"

command -v stellar >/dev/null 2>&1 || {
    echo "error: the stellar CLI is not installed." >&2
    echo "see https://developers.stellar.org/docs/tools/developer-tools/cli/stellar-cli" >&2
    exit 69
}
command -v jq >/dev/null 2>&1 || {
    echo "error: jq is required to read the deployment record and the" >&2
    echo "contracts' replies." >&2
    exit 69
}

from_record() {
    [[ -f "$RECORD" ]] || return 0
    jq -r "$1 // empty" "$RECORD" 2>/dev/null || true
}

RPC_URL="${APHELION_RPC_URL:-$(from_record .rpc_url)}"
RPC_URL="${RPC_URL:-https://soroban-testnet.stellar.org}"
PASSPHRASE="${APHELION_NETWORK_PASSPHRASE:-$(from_record .network_passphrase)}"
PASSPHRASE="${PASSPHRASE:-Test SDF Network ; September 2015}"

REGISTRY="${APHELION_REGISTRY_CONTRACT:-$(from_record .contracts.registry)}"
AGGREGATOR="${APHELION_AGGREGATOR_CONTRACT:-$(from_record .contracts.aggregator)}"
SLASHING="${APHELION_SLASHING_CONTRACT:-$(from_record .contracts.slashing)}"
GOVERNANCE="${APHELION_GOVERNANCE_CONTRACT:-$(from_record .contracts.governance)}"
# Optional: a network that only wants prices never deploys one, so an absent
# randomness contract is a fact about the deployment rather than a fault in it.
RANDOMNESS="${APHELION_RANDOMNESS_CONTRACT:-$(from_record .contracts.randomness)}"

for pair in "REGISTRY:registry" "AGGREGATOR:aggregator" "SLASHING:slashing"; do
    var="${pair%%:*}"
    if [[ -z "${!var}" ]]; then
        echo "error: no ${pair##*:} contract id. Either deploy to $NETWORK --" >&2
        echo "which writes deployments/$NETWORK.json -- or set" >&2
        echo "APHELION_$(echo "$var")_CONTRACT to the deployed contract (C...)." >&2
        exit 64
    fi
done

# Reads are simulated rather than submitted, but the CLI still wants an account
# to simulate as. It signs nothing here; any funded account will do, including
# one with no relationship to this deployment at all.
: "${APHELION_STELLAR_SECRET:?set APHELION_STELLAR_SECRET to any account seed (S...); it signs nothing, but the CLI simulates as it}"

invoke() {
    stellar contract invoke \
        --id "$1" \
        --source-account "$APHELION_STELLAR_SECRET" \
        --rpc-url "$RPC_URL" \
        --network-passphrase "$PASSPHRASE" \
        -- "${@:2}" 2>/dev/null
}

# -- reporting --------------------------------------------------------------

PASSED=0
WARNED=0
FAILED=0

section() { printf '\n%s\n' "$1"; }
explain() { local line; for line in "$@"; do printf '        %s\n' "$line"; done; }

ok() {
    printf '  ok    %s\n' "$1"
    PASSED=$(( PASSED + 1 ))
}
warn() {
    printf '  WARN  %s\n' "$1"
    shift
    explain "$@"
    WARNED=$(( WARNED + 1 ))
}
fail() {
    printf '  FAIL  %s\n' "$1"
    shift
    explain "$@"
    FAILED=$(( FAILED + 1 ))
}
# Not a pass and not a failure: something this script could not determine. Kept
# distinct so a run that checked less than it looks like cannot read as clean.
skip() {
    printf '  ----  %s\n' "$1"
    shift
    explain "$@"
}

expect() {
    local label="$1" got="$2" want="$3"
    shift 3
    if [[ "$got" == "$want" ]]; then
        ok "$label"
    else
        fail "$label" "expected $want" "     got $got" "$@"
    fi
}

# Bash arithmetic is 64-bit and the i128 fields are stroops, so every realistic
# value fits. A value that does not is reported as unchecked rather than
# compared wrongly.
numeric() { [[ "$1" =~ ^-?[0-9]{1,18}$ ]]; }

# -- reachability -----------------------------------------------------------
#
# Everything below reads these four, so a contract that does not answer makes
# the rest of the run meaningless rather than merely incomplete.

section "Reachability ($NETWORK, $RPC_URL)"

REG_CFG="$(invoke "$REGISTRY" get_config || true)"
AGG_CFG="$(invoke "$AGGREGATOR" get_config || true)"
SLA_CFG="$(invoke "$SLASHING" get_config || true)"
RND_CFG=""
if [[ -n "$RANDOMNESS" ]]; then
    RND_CFG="$(invoke "$RANDOMNESS" get_config || true)"
fi

unreachable=0
for pair in "REG_CFG:registry:$REGISTRY" "AGG_CFG:aggregator:$AGGREGATOR" "SLA_CFG:slashing:$SLASHING"; do
    var="${pair%%:*}"; rest="${pair#*:}"; name="${rest%%:*}"; id="${rest##*:}"
    if [[ -n "${!var}" ]] && jq -e 'type == "object"' <<<"${!var}" >/dev/null 2>&1; then
        ok "$name answers get_config  ($id)"
    else
        fail "$name does not answer get_config  ($id)" \
            "Either the contract id is wrong, initialize was never called, or" \
            "the RPC endpoint is not the network this deployment is on."
        unreachable=1
    fi
done

if [[ -z "$GOVERNANCE" ]]; then
    skip "governance: no contract id to check" \
        "The deployment record names none and APHELION_GOVERNANCE_CONTRACT is" \
        "unset. Every authority check below is skipped, so this run cannot" \
        "tell a deployment governed by a timelock from one governed by a key."
    GOV_CFG=""
else
    GOV_CFG="$(invoke "$GOVERNANCE" get_config || true)"
    if [[ -n "$GOV_CFG" ]] && jq -e 'type == "object"' <<<"$GOV_CFG" >/dev/null 2>&1; then
        ok "governance answers get_config  ($GOVERNANCE)"
    else
        fail "governance does not answer get_config  ($GOVERNANCE)" \
            "A timelock that is deployed but not initialised governs nothing," \
            "and any contract already handed to it is frozen: its admin is an" \
            "address that will not act."
        unreachable=1
    fi
fi

if (( unreachable )); then
    echo
    echo "Stopping: the checks below all read the contracts above."
    exit 1
fi

# -- wiring -----------------------------------------------------------------
#
# The cross-references deploy.sh's deploy-all-then-initialise ordering exists
# to get right. Each one is a single address, and each one wrong is a network
# that deploys cleanly and then refuses every submission.

section "Wiring"

expect "registry -> aggregator" \
    "$(jq -r .aggregator <<<"$REG_CFG")" "$AGGREGATOR" \
    "The registry only accepts reputation changes from the address it holds" \
    "here. Pointed elsewhere, every round's record_success is rejected and no" \
    "node's reputation ever moves."

expect "registry -> slasher" \
    "$(jq -r .slasher <<<"$REG_CFG")" "$SLASHING" \
    "The registry only accepts slashes from this address. Pointed elsewhere," \
    "an upheld dispute cannot reach the stake it was upheld against."

expect "aggregator -> registry" \
    "$(jq -r .registry <<<"$AGG_CFG")" "$REGISTRY" \
    "The aggregator asks this address for every submission's voting weight." \
    "Pointed elsewhere, every submission weighs nothing and no round closes."

expect "slashing -> registry" \
    "$(jq -r .registry <<<"$SLA_CFG")" "$REGISTRY" \
    "The slashing contract resolves accused nodes and committee eligibility" \
    "through this address."

# The beacon's one penalty depends on a call that is easy to leave out, because
# it is the only piece of wiring that runs after a contract the registry knew
# nothing about at initialisation.
if [[ -n "$RANDOMNESS" ]]; then
    if [[ -n "$RND_CFG" ]] && jq -e 'type == "object"' <<<"$RND_CFG" >/dev/null 2>&1; then
        ok "randomness answers get_config  ($RANDOMNESS)"
    else
        fail "randomness does not answer get_config  ($RANDOMNESS)" \
            "The deployment record names a randomness contract at this address" \
            "and nothing is there. Either it failed to deploy or the record is" \
            "pointing at the wrong network."
    fi

    expect "registry -> randomness" \
        "$(jq -r .randomness <<<"$REG_CFG")" "$RANDOMNESS" \
        "The registry only accepts a no-show penalty from this address, and it" \
        "starts at the admin rather than unset. Left unpointed, every beacon" \
        "round finalizes cleanly and charges nobody -- the one failure here" \
        "that looks exactly like success."

    expect "randomness -> registry" \
        "$(jq -r .registry <<<"$RND_CFG")" "$REGISTRY" \
        "The randomness contract asks this address who may commit, and charges" \
        "no-shows through it."
else
    skip "randomness not deployed" \
        "No randomness contract in the deployment record. A network that only" \
        "wants prices is complete without one."
fi

REG_TOKEN="$(jq -r .token <<<"$REG_CFG")"
AGG_TOKEN="$(jq -r .token <<<"$AGG_CFG")"
SLA_TOKEN="$(jq -r .token <<<"$SLA_CFG")"
if [[ "$REG_TOKEN" == "$AGG_TOKEN" && "$REG_TOKEN" == "$SLA_TOKEN" ]]; then
    ok "one token across all three  ($REG_TOKEN)"
else
    fail "the three contracts do not agree on the token" \
        "registry   : $REG_TOKEN" \
        "aggregator : $AGG_TOKEN" \
        "slashing   : $SLA_TOKEN" \
        "Stake, dispute bonds and read fees are denominated in this. Two" \
        "assets means a bond posted in one cannot pay a slash owed in the" \
        "other, and the accounting silently stops adding up."
fi

RECORD_TOKEN="$(from_record .token)"
if [[ -n "$RECORD_TOKEN" && "$RECORD_TOKEN" != "$REG_TOKEN" ]]; then
    warn "the deployment record names a different token" \
        "record : $RECORD_TOKEN" \
        "chain  : $REG_TOKEN" \
        "The chain is the authority. The record is what an operator reads" \
        "before bonding, so a stale one is a stake posted in the wrong asset."
fi

# -- authority --------------------------------------------------------------

section "Authority"

if [[ -z "$GOV_CFG" ]]; then
    skip "handover: unchecked without a governance contract id" \
        "See the note above."
else
    DEPLOYER="$(from_record .deployer)"
    handover_complete=1
    admin_pairs=("registry:$(jq -r .admin <<<"$REG_CFG")"
                 "aggregator:$(jq -r .admin <<<"$AGG_CFG")"
                 "slashing:$(jq -r .admin <<<"$SLA_CFG")")
    if [[ -n "$RND_CFG" ]]; then
        admin_pairs+=("randomness:$(jq -r .admin <<<"$RND_CFG")")
    fi
    for pair in "${admin_pairs[@]}"; do
        name="${pair%%:*}"
        admin="${pair##*:}"
        if [[ "$admin" == "$GOVERNANCE" ]]; then
            ok "$name admin is the timelock"
        else
            handover_complete=0
            # Reported per contract rather than as one verdict, because the
            # handover is three independent calls and the interesting failure
            # is the one that got two of them.
            if [[ -n "$DEPLOYER" && "$admin" == "$DEPLOYER" ]]; then
                fail "$name admin is still the deploying key  ($admin)" \
                    "Either APHELION_SKIP_HANDOVER was set, or the handover" \
                    "failed part-way. One key can change this contract's" \
                    "parameters in a single transaction, with no delay and no" \
                    "publication -- which is the thing the timelock exists to" \
                    "prevent. Hand it over by hand; rerunning deploy.sh would" \
                    "deploy a second set of contracts rather than resume this."
            else
                fail "$name admin is neither the timelock nor the deployer" \
                    "admin : $admin" \
                    "Somebody moved it. Whoever holds that key can change" \
                    "this contract's parameters without a delay."
            fi
        fi
    done

    GUARDIAN="$(jq -r .guardian <<<"$GOV_CFG")"
    DELAY="$(jq -r .delay <<<"$GOV_CFG" | tr -d '"')"
    GRACE="$(jq -r .grace_period <<<"$GOV_CFG" | tr -d '"')"
    PROPOSERS_JSON="$(invoke "$GOVERNANCE" proposers || echo '[]')"
    PROPOSER_COUNT="$(jq -r 'length' <<<"$PROPOSERS_JSON" 2>/dev/null || echo 0)"

    if (( PROPOSER_COUNT > 0 )); then
        ok "$PROPOSER_COUNT proposer(s) may queue a change"
    else
        fail "nobody may queue a proposal" \
            "Every parameter of the network is frozen permanently at whatever" \
            "it happens to be, the timelock's own configuration included:" \
            "there is no other route in."
    fi

    if jq -e --arg g "$GUARDIAN" 'index($g)' <<<"$PROPOSERS_JSON" >/dev/null 2>&1; then
        warn "the guardian is also a proposer  ($GUARDIAN)" \
            "A key whose only power is to say no is worth nothing held in the" \
            "same hands as the key that says go. Move it with a proposal" \
            "against the timelock naming set_config."
    else
        ok "the guardian is not a proposer  ($GUARDIAN)"
    fi

    RECORD_GUARDIAN="$(from_record .guardian)"
    if [[ -n "$RECORD_GUARDIAN" && "$RECORD_GUARDIAN" != "$GUARDIAN" ]]; then
        warn "the deployment record names a different guardian" \
            "record : $RECORD_GUARDIAN" \
            "chain  : $GUARDIAN" \
            "Expected if a proposal has moved it since the deployment. The" \
            "record is not updated by govern.sh, so this is worth confirming" \
            "against the executed proposal rather than assuming either way."
    fi

    # Bounds the contract enforces at initialize and on every set_config. A
    # deployment outside them is not possible today; checked anyway, because
    # a value drifting outside a bound is exactly what a later change to the
    # bounds would do silently.
    if numeric "$DELAY" && (( DELAY >= 86400 && DELAY <= 2592000 )); then
        ok "timelock delay is $DELAY s ($(( DELAY / 3600 )) h)"
    else
        fail "timelock delay is $DELAY s, outside one to thirty days" \
            "Below a day the delay stops being one; above thirty it stops" \
            "being governance."
    fi
    if numeric "$GRACE" && (( GRACE >= 86400 && GRACE <= 2592000 )); then
        ok "timelock grace period is $GRACE s ($(( GRACE / 86400 )) d)"
    else
        fail "timelock grace period is $GRACE s, outside one to thirty days" ""
    fi
fi

# -- parameters -------------------------------------------------------------
#
# The ones that constrain each other. Each is individually plausible and
# jointly broken, which is the combination no single setter can catch.

section "Parameters"

MIN_STAKE="$(jq -r .min_stake <<<"$REG_CFG" | tr -d '"')"
UNBONDING="$(jq -r .unbonding_period <<<"$REG_CFG" | tr -d '"')"
JAIL="$(jq -r .jail_period <<<"$REG_CFG" | tr -d '"')"

if numeric "$JAIL" && numeric "$UNBONDING"; then
    if (( JAIL < UNBONDING )); then
        ok "jail term ($JAIL s) is shorter than unbonding ($UNBONDING s)"
    else
        fail "jail term ($JAIL s) is not shorter than unbonding ($UNBONDING s)" \
            "Unbonding, withdrawing and registering a fresh key would be the" \
            "faster way back, so nobody would ever serve the term. Neither" \
            "value can be changed after initialize: both are promises made to" \
            "nodes that already bonded under them."
    fi
else
    skip "jail term against unbonding: values not comparable" "$JAIL / $UNBONDING"
fi

QUORUM="$(jq -r .quorum <<<"$AGG_CFG" | tr -d '"')"
MIN_WEIGHT="$(jq -r .min_weight_bps <<<"$AGG_CFG" | tr -d '"')"
MAX_STALENESS="$(jq -r .max_staleness <<<"$AGG_CFG" | tr -d '"')"
MIN_INTERVAL="$(jq -r .min_round_interval <<<"$AGG_CFG" | tr -d '"')"
ABSENCE="$(jq -r .absence_threshold <<<"$AGG_CFG" | tr -d '"')"
REWARD="$(jq -r .reward_per_submission <<<"$AGG_CFG" | tr -d '"')"

if numeric "$MIN_INTERVAL" && numeric "$MAX_STALENESS"; then
    if (( MIN_INTERVAL < MAX_STALENESS )); then
        ok "round interval ($MIN_INTERVAL s) is inside the staleness window ($MAX_STALENESS s)"
    else
        fail "round interval ($MIN_INTERVAL s) is at or beyond the staleness window ($MAX_STALENESS s)" \
            "A node cannot publish again until the interval elapses, and by" \
            "then the observations it has are too old to accept. The feed" \
            "stops: every submission is refused as stale, and nothing about" \
            "either value alone looks wrong."
    fi
fi

# `sweep_absent` charges a missed round against any node silent for longer
# than absence_threshold, and a miss costs reputation and eventually jails.
# A node submitting exactly on cadence is silent for the whole interval
# between its rounds, so the threshold has to outlast one.
if numeric "$ABSENCE" && numeric "$MIN_INTERVAL"; then
    if (( ABSENCE > MIN_INTERVAL )); then
        ok "absence threshold ($ABSENCE s) outlasts a round interval ($MIN_INTERVAL s)"
    else
        fail "absence threshold ($ABSENCE s) does not outlast a round interval ($MIN_INTERVAL s)" \
            "sweep_absent is permissionless, so anyone could charge a missed" \
            "round against every node on the network in the ordinary gap" \
            "between its submissions. Nodes doing exactly what they are meant" \
            "to would lose reputation until they were jailed for it."
    fi
fi

# Both a headcount and a weight, so this says how many nodes the weight floor
# needs -- which is the number an operator actually plans around.
if numeric "$MIN_WEIGHT" && numeric "$QUORUM"; then
    full_nodes=$(( (MIN_WEIGHT + 9999) / 10000 ))
    new_nodes=$(( (MIN_WEIGHT + 4999) / 5000 ))
    # `ok` takes no explanation, so the arithmetic goes out through `explain`
    # directly. It is not a finding -- nothing here can be wrong -- it is the
    # number an operator plans recruitment around.
    ok "quorum is $QUORUM nodes and $MIN_WEIGHT bps"
    explain "$MIN_WEIGHT bps is $full_nodes node(s) at full weight, or $new_nodes freshly" \
            "registered at half. A round needs whichever of that and the" \
            "headcount is larger."
fi

DIS_QUORUM="$(jq -r .quorum <<<"$SLA_CFG" | tr -d '"')"
SEATS="$(jq -r .seats <<<"$SLA_CFG" | tr -d '"')"
DIS_SLASH="$(jq -r .slash_amount <<<"$SLA_CFG" | tr -d '"')"
DIS_BOND="$(jq -r .dispute_bond <<<"$SLA_CFG" | tr -d '"')"
APP_BOND="$(jq -r .appeal_bond <<<"$SLA_CFG" | tr -d '"')"
COMMITTEE_JSON="$(invoke "$SLASHING" committee || echo '[]')"
COMMITTEE_SIZE="$(jq -r 'length' <<<"$COMMITTEE_JSON" 2>/dev/null || echo 0)"

if numeric "$SEATS" && numeric "$DIS_QUORUM"; then
    if (( SEATS >= DIS_QUORUM )); then
        ok "seats ($SEATS) can reach the dispute quorum ($DIS_QUORUM)"
    else
        fail "seats ($SEATS) are below the dispute quorum ($DIS_QUORUM)" \
            "Every election would seat a committee that could not reach" \
            "quorum at full strength, and a dispute that does not reach" \
            "quorum is dismissed. Slashing would be off for everybody."
    fi
fi

if numeric "$COMMITTEE_SIZE" && numeric "$DIS_QUORUM"; then
    if (( COMMITTEE_SIZE >= DIS_QUORUM )); then
        ok "the seated committee ($COMMITTEE_SIZE) can reach quorum ($DIS_QUORUM)"
    else
        fail "the seated committee ($COMMITTEE_SIZE) cannot reach quorum ($DIS_QUORUM)" \
            "Every dispute filed against anybody is dismissed until an" \
            "election seats more members. Nothing can add one directly:" \
            "seats are only ever filled by a vote."
    fi
fi

if numeric "$DIS_SLASH" && numeric "$MIN_STAKE" && (( DIS_SLASH > MIN_STAKE )); then
    warn "an upheld dispute slashes $DIS_SLASH, more than the minimum stake ($MIN_STAKE)" \
        "A node bonded at the floor cannot pay it in full, so the penalty an" \
        "operator actually faces is capped by what they bonded. Intended if" \
        "the floor is meant to rise; worth knowing either way."
fi

if numeric "$APP_BOND" && numeric "$DIS_BOND" && (( APP_BOND <= DIS_BOND )); then
    warn "the appeal bond ($APP_BOND) is not above the dispute bond ($DIS_BOND)" \
        "An appeal asks the whole committee to do its work twice, so it is" \
        "meant to cost more than filing did. Equal or cheaper makes appealing" \
        "every resolution the rational default."
fi

# -- readiness --------------------------------------------------------------
#
# Not whether the deployment is correct, but whether it can currently produce
# a price. A correct deployment with no nodes is a correct deployment that
# publishes nothing, and the two look identical from a consumer's side.

section "Readiness"

FEEDS_JSON="$(invoke "$AGGREGATOR" feeds || echo '[]')"
FEED_COUNT="$(jq -r 'length' <<<"$FEEDS_JSON" 2>/dev/null || echo 0)"

if (( FEED_COUNT == 0 )); then
    fail "no feeds are configured" \
        "The aggregator will accept no submission for anything: a feed that" \
        "was never added is an unknown feed, not an empty one."
else
    ok "$FEED_COUNT feed(s) configured"
    for feed in $(jq -r '.[]' <<<"$FEEDS_JSON"); do
        fc="$(invoke "$AGGREGATOR" feed_config --feed "$feed" || echo '{}')"
        enabled="$(jq -r '.enabled // false' <<<"$fc")"
        heartbeat="$(jq -r '.heartbeat // 0' <<<"$fc" | tr -d '"')"
        min_nodes="$(jq -r '.min_nodes // 0' <<<"$fc" | tr -d '"')"
        needed="$QUORUM"
        [[ "$min_nodes" != "0" ]] && needed="$min_nodes"

        price="$(invoke "$AGGREGATOR" get_price --feed "$feed" || echo null)"
        if [[ "$price" == "null" || -z "$price" ]]; then
            published="never published"
        else
            published="last round $(jq -r '.round_id' <<<"$price" | tr -d '"')"
        fi

        if [[ "$enabled" == "true" ]]; then
            ok "  $feed: enabled, $needed node(s), heartbeat ${heartbeat}s, $published"
            # The heartbeat is the gap this feed advertises to consumers as
            # acceptable. A node publishing within it is doing what the feed
            # promised, and must not be sweepable for the silence in between.
            if numeric "$heartbeat" && numeric "$ABSENCE" && (( heartbeat >= ABSENCE )); then
                warn "  $feed: heartbeat (${heartbeat}s) is not under the absence threshold (${ABSENCE}s)" \
                    "A node publishing this feed at the advertised cadence and" \
                    "nothing else is silent for longer than sweep_absent" \
                    "tolerates, so it can be charged a missed round for" \
                    "keeping the promise. Only a real risk for a node that" \
                    "serves this feed alone -- last_seen is per node, not per" \
                    "feed, so anything busier keeps its own clock alive."
            fi
        else
            warn "  $feed: disabled" \
                "Configured but refusing submissions. A consumer reading it" \
                "gets the last price it ever published, ageing, until their" \
                "own max_age check rejects it."
        fi
    done
fi

NODES_JSON="$(invoke "$REGISTRY" list_nodes || echo '[]')"
NODE_COUNT="$(jq -r 'length' <<<"$NODES_JSON" 2>/dev/null || echo 0)"
# Stripped in a second step rather than piped through `tr`, so that a call
# that failed stays empty and is reported as unchecked. Piping makes the exit
# status `tr`'s, which is always zero, and a failed read would arrive as a
# plausible-looking 0 bps.
TOTAL_WEIGHT="$(invoke "$REGISTRY" total_weight || true)"
TOTAL_WEIGHT="${TOTAL_WEIGHT//\"/}"

if (( NODE_COUNT == 0 )); then
    warn "no nodes are registered" \
        "Expected immediately after deploying, and the reason the network" \
        "publishes nothing until operators bond stake. Nothing below can be" \
        "checked until at least $QUORUM have."
else
    # Ledger time, for measuring how long ago each node last took part. Read
    # from the chain rather than taken from this machine's clock: the threshold
    # it is compared against is the contract's, and so is the clock it is
    # measured on.
    NOW="$(invoke "$AGGREGATOR" ledger_time || true)"
    NOW="${NOW//\"/}"

    # One call per node. Fine at the scale a deployment has any business
    # being at; if it is not, the registry index is the wrong thing to be
    # walking from a shell script anyway.
    active=0; jailed=0; exiting=0; stale_weight=0; stale_bps=0
    for pubkey in $(jq -r '.[]' <<<"$NODES_JSON"); do
        node="$(invoke "$REGISTRY" get_node --pubkey "$pubkey" || echo null)"
        [[ "$node" == "null" || -z "$node" ]] && continue
        status="$(jq -r 'if (.status | type) == "object" then (.status | keys[0]) else .status end' <<<"$node" 2>/dev/null || echo unknown)"
        case "$status" in
            Active)  active=$(( active + 1 )) ;;
            Jailed)  jailed=$(( jailed + 1 )) ;;
            Exiting) exiting=$(( exiting + 1 )) ;;
        esac

        # Weight a node is still carrying for rounds it has stopped taking part
        # in. sweep_absent exists to remove it and is permissionless, so a
        # standing count here is not a bug in the contracts -- it is the
        # measure of whether anybody is actually calling it.
        weight="$(jq -r '.weight_bps // 0' <<<"$node" | tr -d '"')"
        last="$(jq -r '.last_submission // 0' <<<"$node" | tr -d '"')"
        if numeric "$NOW" && numeric "$ABSENCE" && numeric "$weight" && numeric "$last" \
           && (( weight > 0 && last > 0 && NOW >= last && NOW - last >= ABSENCE )); then
            stale_weight=$(( stale_weight + 1 ))
            stale_bps=$(( stale_bps + weight ))
        fi
    done
    ok "$NODE_COUNT node(s) registered: $active active, $jailed jailed, $exiting exiting"

    if ! numeric "$NOW"; then
        skip "whether any weight is unearned: the aggregator did not answer ledger_time" \
            "How long ago each node last took part cannot be measured without" \
            "the clock the threshold is measured on."
    elif (( stale_weight > 0 )); then
        warn "$stale_weight node(s) carry $stale_bps bps despite being silent past the ${ABSENCE}s absence threshold" \
            "That weight still counts towards every median. sweep_absent" \
            "removes it and anyone may call it, so this is a count of upkeep" \
            "nobody is doing rather than a fault in the contracts. Operators" \
            "can have their nodes do it continuously (upkeep.sweep_absent in" \
            "aphelion.toml), or charge it once with: aphelion-node sweep --commit"
    else
        ok "no node is carrying weight for rounds it has stopped taking part in"
    fi

    # Jailed and exiting nodes weigh zero, so total_weight is already the
    # number the aggregator will actually see -- no need to model it here.
    if ! numeric "$TOTAL_WEIGHT" || ! numeric "$MIN_WEIGHT"; then
        skip "whether a round can close: the weights are not comparable" \
            "registry total_weight : ${TOTAL_WEIGHT:-<no answer>}" \
            "aggregator min_weight : ${MIN_WEIGHT:-<no answer>}"
    elif (( active >= QUORUM && TOTAL_WEIGHT >= MIN_WEIGHT )); then
        ok "a round can close: $active active node(s) carrying $TOTAL_WEIGHT bps"
    else
        warn "no round can close yet: $active active node(s) carrying $TOTAL_WEIGHT bps" \
            "Needs $QUORUM node(s) and $MIN_WEIGHT bps between them, and that is" \
            "the ceiling rather than the expectation -- every one of them" \
            "has to submit within the same round for it to count."
    fi
fi

if numeric "$REWARD" && (( REWARD > 0 )); then
    POOL="$(invoke "$REGISTRY" reward_pool || true)"
    POOL="${POOL//\"/}"
    if ! numeric "$POOL"; then
        skip "the reward pool: the registry did not answer reward_pool" \
            "Whether operators are being paid is unknown, rather than no."
    elif (( POOL >= REWARD )); then
        ok "the reward pool holds $POOL, at least one round's rewards"
    else
        warn "rewards are configured at $REWARD per submission, and the pool holds $POOL" \
            "An empty pool does not stall consensus -- a round that cannot" \
            "pay still produces a correct price -- so nothing breaks and" \
            "nothing errors. Operators simply earn nothing, silently. Fund it" \
            "with fund_rewards, or set reward_per_submission to 0 and say so."
    fi
fi

# -- verdict ----------------------------------------------------------------

echo
printf '%d passed, %d warning(s), %d failure(s)\n' "$PASSED" "$WARNED" "$FAILED"

if (( FAILED > 0 )); then
    echo
    echo "This deployment is not what it was meant to be. Nothing above was"
    echo "changed -- every call was simulated -- so the repairs are yours to"
    echo "make, by proposal if the contracts have been handed over."
    exit 1
fi

if (( WARNED > 0 && STRICT )); then
    echo
    echo "Warnings are failures under --strict."
    exit 1
fi

if (( WARNED > 0 )); then
    echo
    echo "Nothing is wired wrongly. The warnings above are things worth"
    echo "deciding about rather than things that are broken."
fi
