#!/usr/bin/env bash
#
# Queue, inspect, execute and cancel proposals against the Aphelion governance
# timelock.
#
# Usage:
#   scripts/govern.sh list
#   scripts/govern.sh show <id>
#   scripts/govern.sh propose <target> <function> <args-json> <description>
#   scripts/govern.sh execute <id>
#   scripts/govern.sh cancel <id>
#
#   <target> is registry | aggregator | slashing | governance, or a contract id
#   <args-json> is a JSON array of the arguments, in the order the function
#   takes them -- '["20000000000"]' for set_min_stake, '[]' for a function
#   that takes none.
#
# Once `scripts/deploy.sh` has handed the contracts over, every admin call is a
# proposal: published now, executable after the delay, and dead after the grace
# period. That is two transactions and a wait where it used to be one
# transaction, which is the entire point -- but it is also two chances to
# mistype a contract id, separated by long enough to forget what was queued.
# This script reads the deployment record so nothing has to be retyped, and
# prints what a proposal will do before it submits anything.
#
# It holds no privilege of its own. Whoever runs it signs with their own key,
# and the contract decides whether that key may do what is being asked.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

NETWORK="${APHELION_NETWORK:-testnet}"
RECORD="$ROOT/deployments/$NETWORK.json"

command -v stellar >/dev/null 2>&1 || {
    echo "error: the stellar CLI is not installed." >&2
    echo "see https://developers.stellar.org/docs/tools/developer-tools/cli/stellar-cli" >&2
    exit 69
}
command -v jq >/dev/null 2>&1 || {
    echo "error: jq is required to read the deployment record and the" >&2
    echo "contract's replies." >&2
    exit 69
}

usage() {
    sed -n '3,16p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    # Asking for this is not a misuse: only being told it after a mistake is.
    exit "${1:-64}"
}

# The deployment record is the default for every address, and each one can be
# overridden. A record is per network and gitignored, so an operator who has
# cloned the repository and been handed a set of contract ids has no file to
# read -- the environment is the way in for them.
from_record() {
    [[ -f "$RECORD" ]] || return 0
    jq -r "$1 // empty" "$RECORD" 2>/dev/null || true
}

RPC_URL="${APHELION_RPC_URL:-$(from_record .rpc_url)}"
RPC_URL="${RPC_URL:-https://soroban-testnet.stellar.org}"
PASSPHRASE="${APHELION_NETWORK_PASSPHRASE:-$(from_record .network_passphrase)}"
PASSPHRASE="${PASSPHRASE:-Test SDF Network ; September 2015}"

GOVERNANCE="${APHELION_GOVERNANCE_CONTRACT:-$(from_record .contracts.governance)}"
REGISTRY="${APHELION_REGISTRY_CONTRACT:-$(from_record .contracts.registry)}"
AGGREGATOR="${APHELION_AGGREGATOR_CONTRACT:-$(from_record .contracts.aggregator)}"
SLASHING="${APHELION_SLASHING_CONTRACT:-$(from_record .contracts.slashing)}"

if [[ -z "$GOVERNANCE" ]]; then
    echo "error: no governance contract id. Either deploy to $NETWORK -- which" >&2
    echo "writes deployments/$NETWORK.json -- or set" >&2
    echo "APHELION_GOVERNANCE_CONTRACT to the deployed timelock (C...)." >&2
    exit 64
fi

: "${APHELION_STELLAR_SECRET:?set APHELION_STELLAR_SECRET to the seed (S...) of the account submitting this}"

# Reads are simulated rather than submitted, but the CLI still wants an account
# to simulate as. Using the same one costs nothing and keeps this to one secret.
invoke() {
    stellar contract invoke \
        --id "$1" \
        --source-account "$APHELION_STELLAR_SECRET" \
        --rpc-url "$RPC_URL" \
        --network-passphrase "$PASSPHRASE" \
        -- "${@:2}"
}

# `date -d @` is GNU, `date -r` is BSD, and an operator on macOS should not get
# a broken line in the middle of a proposal they are about to sign.
when() {
    local ts="$1"
    date -u -d "@$ts" +"%Y-%m-%dT%H:%M:%SZ" 2>/dev/null \
        || date -u -r "$ts" +"%Y-%m-%dT%H:%M:%SZ" 2>/dev/null \
        || echo "unix $ts"
}

# Unit variants come back as a bare string, so this is mostly belt and braces
# against a future variant that carries a value.
state_of() {
    local raw
    raw="$(invoke "$GOVERNANCE" state --id "$1" 2>/dev/null || true)"
    if [[ -z "$raw" ]]; then
        echo "unknown"
        return 0
    fi
    jq -r 'if type == "object" then (keys[0]) else . end' <<<"$raw" 2>/dev/null \
        || echo "unknown"
}

resolve_target() {
    case "$1" in
        registry)   [[ -n "$REGISTRY" ]]   || die_unknown registry;   echo "$REGISTRY" ;;
        aggregator) [[ -n "$AGGREGATOR" ]] || die_unknown aggregator; echo "$AGGREGATOR" ;;
        slashing)   [[ -n "$SLASHING" ]]   || die_unknown slashing;   echo "$SLASHING" ;;
        governance) echo "$GOVERNANCE" ;;
        C*)         echo "$1" ;;
        *)
            echo "error: unknown target '$1'. Use registry, aggregator," >&2
            echo "slashing, governance, or a contract id starting with C." >&2
            exit 64
            ;;
    esac
}

die_unknown() {
    echo "error: no contract id for '$1' in $RECORD, and" >&2
    echo "APHELION_$(echo "$1" | tr '[:lower:]' '[:upper:]')_CONTRACT is unset." >&2
    exit 64
}

confirm() {
    read -r -p "$1 [y/N] " reply
    [[ "$reply" == "y" || "$reply" == "Y" ]] || { echo "aborted"; exit 1; }
}

# -- subcommands ------------------------------------------------------------

cmd_list() {
    local count
    count="$(invoke "$GOVERNANCE" proposal_count | tr -d '"')"
    if [[ "$count" == "0" ]]; then
        echo "No proposals have been queued against $GOVERNANCE."
        return 0
    fi

    printf '%4s  %-9s  %-56s  %s\n' "ID" "STATE" "TARGET" "FUNCTION"
    # Ids are allocated from 1 and never reused, so counting up reaches all of
    # them. Persistent entries can expire from the ledger if their TTL is not
    # extended, which is why a missing one is skipped rather than fatal.
    local id json
    for (( id = 1; id <= count; id++ )); do
        json="$(invoke "$GOVERNANCE" get_proposal --id "$id" 2>/dev/null || echo null)"
        [[ "$json" == "null" || -z "$json" ]] && continue
        printf '%4s  %-9s  %-56s  %s\n' \
            "$id" \
            "$(state_of "$id")" \
            "$(jq -r '.target' <<<"$json")" \
            "$(jq -r '.function' <<<"$json")"
    done

    echo
    echo "scripts/govern.sh show <id> for the arguments and the timings."
}

cmd_show() {
    local id="${1:?usage: $0 show <id>}"
    local json
    json="$(invoke "$GOVERNANCE" get_proposal --id "$id")"
    if [[ "$json" == "null" || -z "$json" ]]; then
        echo "error: no proposal $id on $GOVERNANCE" >&2
        exit 65
    fi

    local state eta expires proposed executed
    state="$(state_of "$id")"
    eta="$(jq -r '.eta' <<<"$json")"
    expires="$(jq -r '.expires_at' <<<"$json")"
    proposed="$(jq -r '.proposed_at' <<<"$json")"
    executed="$(jq -r '.executed_at' <<<"$json")"

    cat <<SHOW
Proposal $id on $GOVERNANCE

  state       : $state
  proposer    : $(jq -r '.proposer' <<<"$json")
  target      : $(jq -r '.target' <<<"$json")
  function    : $(jq -r '.function' <<<"$json")
  arguments   : $(jq -c '.args' <<<"$json")
  description : $(jq -r '.description' <<<"$json")

  queued      : $(when "$proposed")
  executable  : $(when "$eta")
  expires     : $(when "$expires")
SHOW

    if [[ "$executed" != "0" ]]; then
        echo "  executed    : $(when "$executed")"
    fi

    echo
    case "$state" in
        Waiting)
            echo "Not executable yet. The delay is the window in which an operator who"
            echo "dislikes this change can unbond before it binds them."
            ;;
        Ready)
            echo "Executable now, by anyone: scripts/govern.sh execute $id"
            ;;
        Expired)
            echo "The grace period ran out and nothing will execute this. Queue it"
            echo "again if it is still wanted -- which serves the delay again."
            ;;
    esac
}

cmd_propose() {
    (( $# == 4 )) || usage
    local target_name="$1" function="$2" args="$3" description="$4"
    local target
    target="$(resolve_target "$target_name")"

    jq -e 'type == "array"' <<<"$args" >/dev/null 2>&1 || {
        echo "error: the arguments must be a JSON array, in the order the" >&2
        echo "function takes them. Got: $args" >&2
        echo "hint: '[\"20000000000\"]' for one argument, '[]' for none." >&2
        exit 64
    }

    local proposer delay grace config
    proposer="${APHELION_PROPOSER_ACCOUNT:-}"
    if [[ -z "$proposer" ]]; then
        echo "error: set APHELION_PROPOSER_ACCOUNT to the account queueing this" >&2
        echo "(G...). It has to be one the timelock knows as a proposer, and it" >&2
        echo "has to be the account APHELION_STELLAR_SECRET belongs to: the" >&2
        echo "contract requires its authorisation, and the CLI signs with one key." >&2
        exit 64
    fi

    config="$(invoke "$GOVERNANCE" get_config)"
    delay="$(jq -r '.delay' <<<"$config")"
    grace="$(jq -r '.grace_period' <<<"$config")"
    local now eta
    now="$(date -u +%s)"
    eta=$(( now + delay ))

    # Read from the chain rather than assumed, because a proposal queued by an
    # account the contract does not know as a proposer fails at submission, and
    # the reason is worth having before paying for it rather than after.
    if ! jq -e --arg p "$proposer" 'index($p)' \
        <<<"$(invoke "$GOVERNANCE" proposers)" >/dev/null 2>&1; then
        echo "error: $proposer is not a proposer on $GOVERNANCE." >&2
        echo "Adding one is itself a proposal, queued by somebody who already is:" >&2
        echo "  $0 propose governance add_proposer '[\"$proposer\"]' <description>" >&2
        exit 64
    fi

    cat <<PLAN

Proposal against $GOVERNANCE

  proposer    : $proposer
  target      : $target$( [[ "$target_name" != "$target" ]] && echo "  ($target_name)" )
  function    : $function
  arguments   : $args
  description : $description

  executable  : ~$(when "$eta")  (in $delay seconds)
  expires     : ~$(when "$(( eta + grace ))")

  Both approximate: they are this machine's clock plus the delay, and the
  contract dates a proposal by the ledger's.

PLAN

    if [[ ! "$description" =~ ^(https?|ipfs):// ]]; then
        echo "Note: the description is published as-is and is the only account"
        echo "anyone gets of why this change is being made. A URL or content hash"
        echo "is what the field is for -- prose on the ledger cannot be revised,"
        echo "and a bare sentence cannot be checked against anything."
        echo
    fi

    confirm "Queue this proposal?"

    local id
    id="$(invoke "$GOVERNANCE" propose \
        --proposer "$proposer" \
        --target "$target" \
        --function "$function" \
        --args "$args" \
        --description "$description" | tr -d '"')"

    echo
    echo "Queued as proposal $id."
    echo
    # Read back rather than reported from what was sent: the timings that
    # matter are the ledger's, and this is the record anyone else will read.
    cmd_show "$id"
    echo
    echo "Until it becomes executable the guardian or the proposer can cancel"
    echo "it; once it expires nobody can execute it at all."
}

cmd_execute() {
    local id="${1:?usage: $0 execute <id>}"
    local state
    state="$(state_of "$id")"

    case "$state" in
        Ready) ;;
        Waiting)
            echo "error: proposal $id is still serving its delay." >&2
            echo "$0 show $id for when it becomes executable." >&2
            exit 65
            ;;
        unknown)
            echo "error: no proposal $id on $GOVERNANCE." >&2
            echo "$0 list for the ones there are." >&2
            exit 65
            ;;
        *)
            echo "error: proposal $id is $state, and only a Ready proposal can" >&2
            echo "be executed." >&2
            exit 65
            ;;
    esac

    cmd_show "$id"
    echo
    # Permissionless by design: the call was fixed when it was queued and the
    # delay is a fact about the clock, so there is nothing left for whoever
    # sends this transaction to decide.
    confirm "Execute this proposal? The call above is made immediately."

    invoke "$GOVERNANCE" execute --id "$id"
    echo
    echo "Executed."
}

cmd_cancel() {
    local id="${1:?usage: $0 cancel <id>}"
    local canceller="${APHELION_CANCELLER_ACCOUNT:-}"
    if [[ -z "$canceller" ]]; then
        echo "error: set APHELION_CANCELLER_ACCOUNT to the account cancelling" >&2
        echo "this (G...) -- the guardian, or the proposal's own proposer. It has" >&2
        echo "to be the account APHELION_STELLAR_SECRET belongs to." >&2
        exit 64
    fi

    cmd_show "$id"
    echo
    echo "Cancelling is permanent: there is no un-cancel, because reviving a"
    echo "proposal would return a call to the executable state without it having"
    echo "served a fresh delay. Queueing it again serves the delay again."
    echo
    confirm "Cancel proposal $id as $canceller?"

    invoke "$GOVERNANCE" cancel --canceller "$canceller" --id "$id"
    echo
    echo "Cancelled."
}

# -- dispatch ---------------------------------------------------------------

case "${1:-}" in
    list)    shift; cmd_list "$@" ;;
    show)    shift; cmd_show "$@" ;;
    propose) shift; cmd_propose "$@" ;;
    execute) shift; cmd_execute "$@" ;;
    cancel)  shift; cmd_cancel "$@" ;;
    -h|--help|help) usage 0 ;;
    "") usage ;;
    *)
        echo "error: unknown subcommand '$1'" >&2
        echo >&2
        usage
        ;;
esac
