#!/usr/bin/env bash
#
# Tests for scripts/verify-deployment.sh.
#
# That script is the gate between "deploy.sh printed no error" and "this
# deployment is what it was meant to be", and until now nothing checked the
# gate itself. A verifier that quietly stops verifying is worse than none: it
# is the same clean output over a deployment nobody looked at.
#
# So each case here takes a healthy deployment (chain.json, the four
# contracts' answers; record.json, what deploy.sh would have written), breaks
# exactly one thing about it, and asserts that the script says so and exits
# non-zero. Breaking one thing at a time is the point -- a case that breaks
# three cannot tell which of them the script actually noticed.
#
# The chain is `stellar-stub`, which answers from the fixture instead of a
# network. Nothing here needs an RPC endpoint, a funded account or a network
# at all.
#
# Usage:
#   tests/deployment/run.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
VERIFY="$ROOT/scripts/verify-deployment.sh"

command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 69; }

# The script looks up `stellar` on PATH, so the stub goes in front of it under
# that name. A symlink rather than a copy, so editing the stub cannot leave a
# stale duplicate behind to be debugged later.
BIN="$(mktemp -d)"
ln -s "$HERE/stellar-stub" "$BIN/stellar"
WORK="$(mktemp -d)"
trap 'rm -rf "$BIN" "$WORK"' EXIT

PASSED=0
FAILED=0

# run <chain-patch> <record-patch> [args...]
#
# Both patches are jq programs applied to the healthy fixtures; `.` leaves one
# alone. Output lands in $OUT and the exit status in $STATUS.
run() {
    local chain_patch="$1" record_patch="$2"
    shift 2
    jq "$chain_patch" "$HERE/chain.json" > "$WORK/chain.json"
    jq "$record_patch" "$HERE/record.json" > "$WORK/record.json"
    OUT="$(
        PATH="$BIN:$PATH" \
        FIXTURE="$WORK/chain.json" \
        APHELION_DEPLOYMENT_RECORD="$WORK/record.json" \
        APHELION_STELLAR_SECRET=SDUMMYSECRETTHATSIGNSNOTHING \
        "$VERIFY" "$@" 2>&1
    )"
    STATUS=$?
}

# case <name> <expected-status> <expected-substring>
case_is() {
    local name="$1" want_status="$2" want_text="$3"
    local why=""
    (( STATUS == want_status )) || why="exit $STATUS, wanted $want_status"
    if [[ -n "$want_text" ]] && ! grep -qF -- "$want_text" <<<"$OUT"; then
        why="${why:+$why; }no line containing: $want_text"
    fi
    if [[ -z "$why" ]]; then
        printf 'ok    %s\n' "$name"
        PASSED=$(( PASSED + 1 ))
    else
        printf 'FAIL  %s\n      %s\n' "$name" "$why"
        printf '%s\n' "$OUT" | sed 's/^/      | /'
        FAILED=$(( FAILED + 1 ))
    fi
}

REG=CCCP65FAZK5QJEUDLJY3MHX56SP7DTV2IQZH2P4EQGDQBHSNKZRU3P5Q
AGG=CAREHR44HR57R6JNDZTQ5TGVBCJFRKYQ66ZDIV5CVPPSZZA33I3PJAOC
SLA=CALKFEVQ4TT3B6U4HGTKY3PTDRCNWIH7P6GA2NDXPPQ7LS3BASS43P5J
GOV=CDGOVXX4TT3B6U4HGTKY3PTDRCNWIH7P6GA2NDXPPQ7LS3BASS43QQQQ
RND=CDRNDXX4TT3B6U4HGTKY3PTDRCNWIH7P6GA2NDXPPQ7LS3BASS43QQQQ
DEPLOYER=GCNS2KFQDCMRG4EWA2DQY5XZDPIAFUWNSA7236OCEMDCU7PY7G3ZHF2V
GUARDIAN=GDGUARDIANQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ
STRANGER=GDSTRANGERQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ
NODE1=1111111111111111111111111111111111111111111111111111111111111111

# -- the fixture itself -----------------------------------------------------
#
# First, because every case below is "this, with one thing wrong". If the
# healthy deployment does not pass, nothing a later case reports means what it
# says it means.

run . .
case_is "a healthy deployment passes" 0 "0 failure(s)"

run . . --strict
case_is "and passes --strict, with nothing to warn about" 0 "0 failure(s)"

# -- the handover -----------------------------------------------------------
#
# The reason this script exists. deploy.sh's last three calls are independent
# and one-way, so the failure worth catching is the one that got some of them.

run ".\"$AGG.get_config\".admin = \"$DEPLOYER\"" .
case_is "a contract left with the deploying key" 1 "aggregator admin is still the deploying key"

run ".\"$SLA.get_config\".admin = \"$STRANGER\"" .
case_is "a contract whose admin is neither" 1 "slashing admin is neither the timelock nor the deployer"

run . 'del(.contracts.governance)'
case_is "no governance id: unchecked, not passed" 0 "handover: unchecked without a governance contract id"

run "del(.\"$GOV.get_config\")" .
case_is "a timelock that was never initialised" 1 "governance does not answer get_config"

run ".\"$GOV.proposers\" = []" .
case_is "a timelock nobody can propose to" 1 "nobody may queue a proposal"

run ".\"$GOV.proposers\" = [\"$DEPLOYER\", \"$GUARDIAN\"]" .
case_is "the guardian holding a proposer key too" 0 "the guardian is also a proposer"

# -- wiring -----------------------------------------------------------------

run ".\"$AGG.get_config\".registry = \"$GOV\"" .
case_is "the aggregator pointed at the wrong registry" 1 "aggregator -> registry"

run ".\"$REG.get_config\".slasher = \"$AGG\"" .
case_is "the registry pointed at the wrong slasher" 1 "registry -> slasher"

run ".\"$SLA.get_config\".token = \"$GOV\"" .
case_is "two assets across three contracts" 1 "do not agree on the token"

run . '.token = "CDIFFERENTTOKENQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ"'
case_is "a record naming a token the chain does not" 0 "the deployment record names a different token"

run "del(.\"$REG.get_config\")" .
case_is "a contract that does not answer stops the run" 1 "Stopping: the checks below all read the contracts above"

# -- parameters that constrain each other -----------------------------------

run ".\"$REG.get_config\".jail_period = 604800" .
case_is "a jail term nobody would serve" 1 "is not shorter than unbonding"

run ".\"$AGG.get_config\".min_round_interval = 600" .
case_is "a round interval past the staleness window" 1 "at or beyond the staleness window"

run ".\"$AGG.get_config\".absence_threshold = 60" .
case_is "an absence threshold inside one round" 1 "does not outlast a round interval"

# -- unearned weight --------------------------------------------------------
#
# Not a fault in the contracts: sweep_absent removes this and anybody may call
# it. What the script is measuring is whether anybody does, which is the one
# thing about a live network that a correctness check cannot tell you.

run ".\"$REG.get_node.$NODE1\".last_submission = 1757500000" .
case_is "weight held by a node that stopped publishing" 0 "1 node(s) carry 10000 bps"

run ".\"$REG.get_node.$NODE1\".last_submission = 1757500000" . --strict
case_is "and unearned weight is a failure under --strict" 1 "1 node(s) carry 10000 bps"

run ".\"$REG.get_node.$NODE1\".status = \"Jailed\" | .\"$REG.get_node.$NODE1\".weight_bps = 0 | .\"$REG.get_node.$NODE1\".last_submission = 1757500000" .
case_is "a jailed node's silence is not unearned weight" 0 "no node is carrying weight"

run "del(.\"$AGG.ledger_time\")" .
case_is "no ledger time: unmeasured, not passed" 0 "did not answer ledger_time"

run ".\"$SLA.get_config\".seats = 2" .
case_is "seats below the dispute quorum" 1 "are below the dispute quorum"

run ".\"$SLA.committee\" = [\"$DEPLOYER\"]" .
case_is "a seated committee below quorum" 1 "cannot reach quorum"

run ".\"$SLA.get_config\".appeal_bond = \"1000000000\"" .
case_is "an appeal no dearer than a dispute" 0 "is not above the dispute bond"

# -- readiness --------------------------------------------------------------
#
# A correct deployment that cannot produce a price. Nothing here is a wiring
# error, and from a consumer's side it is indistinguishable from one.

run ".\"$AGG.feeds\" = []" .
case_is "a deployment with no feeds" 1 "no feeds are configured"

run ".\"$AGG.feed_config.BTC_USD\".enabled = false" .
case_is "a feed configured but disabled" 0 "BTC_USD: disabled"

run ".\"$REG.list_nodes\" = [] | .\"$REG.total_weight\" = 0" .
case_is "a fresh deployment with no operators" 0 "no nodes are registered"

run ".\"$REG.total_weight\" = 15000" .
case_is "nodes that do not carry the weight floor" 0 "no round can close yet"

run ".\"$AGG.get_config\".reward_per_submission = \"1000000\"" .
case_is "rewards promised from an empty pool" 0 "the pool holds 0"

# A failed read must not arrive as a plausible zero: piping an invocation
# through `tr` to strip quotes would make the exit status `tr`'s, and "the
# registry did not answer" would print as "carrying 0 bps".
run "del(.\"$REG.total_weight\")" .
case_is "a weight the registry would not report" 0 "the weights are not comparable"

# -- the beacon -------------------------------------------------------------
#
# The randomness contract is optional, which makes every case here about
# telling "not deployed" apart from "deployed and mis-wired". The second is
# the dangerous one: the registry's `randomness` address starts at the admin
# rather than unset, so a deployment that skips `set_randomness` produces
# rounds that finalize cleanly and charge nobody -- a failure that looks
# exactly like success from every other angle.

run . 'del(.contracts.randomness)'
case_is "no randomness id: reported as absent, not as broken" 0 "randomness not deployed"

run "del(.\"$RND.get_config\")" .
case_is "a randomness id pointing at nothing" 1 "randomness does not answer get_config"

run ".\"$REG.get_config\".randomness = \"$GOV\"" .
case_is "the registry never pointed at the beacon" 1 "registry -> randomness"

run "del(.\"$REG.get_config\".randomness)" .
case_is "a registry with no randomness field at all" 1 "registry -> randomness"

run ".\"$RND.get_config\".registry = \"$AGG\"" .
case_is "a beacon asking the wrong contract who may commit" 1 "randomness -> registry"

run ".\"$RND.get_config\".admin = \"$DEPLOYER\"" .
case_is "a beacon left with the deploying key" 1 "randomness admin is still the deploying key"

# -- reaching a deployment without a record ---------------------------------
#
# The path an operator takes when they were handed contract ids rather than
# deploy.sh's output, which docs/node-operator.md tells them to use before
# bonding anything.

OUT="$(
    PATH="$BIN:$PATH" \
    FIXTURE="$HERE/chain.json" \
    APHELION_DEPLOYMENT_RECORD="$WORK/no-such-record.json" \
    APHELION_STELLAR_SECRET=SDUMMYSECRETTHATSIGNSNOTHING \
    APHELION_REGISTRY_CONTRACT="$REG" \
    APHELION_AGGREGATOR_CONTRACT="$AGG" \
    APHELION_SLASHING_CONTRACT="$SLA" \
    APHELION_GOVERNANCE_CONTRACT="$GOV" \
    "$VERIFY" 2>&1
)"
STATUS=$?
case_is "contract ids with no record at all" 0 "registry admin is the timelock"

OUT="$(
    PATH="$BIN:$PATH" \
    FIXTURE="$HERE/chain.json" \
    APHELION_DEPLOYMENT_RECORD="$WORK/no-such-record.json" \
    APHELION_STELLAR_SECRET=SDUMMYSECRETTHATSIGNSNOTHING \
    "$VERIFY" 2>&1
)"
STATUS=$?
case_is "neither a record nor contract ids" 64 "no registry contract id"

# -- the strict gate --------------------------------------------------------

run ".\"$AGG.feed_config.BTC_USD\".enabled = false" . --strict
case_is "a warning is a failure under --strict" 1 "Warnings are failures under --strict"

# -- arguments --------------------------------------------------------------

run . . --nope
case_is "an argument that is not --strict" 64 "unknown argument"

run . . --help
case_is "--help prints the usage block" 0 "treat warnings as failures"

echo
printf '%d passed, %d failed\n' "$PASSED" "$FAILED"
(( FAILED == 0 ))
