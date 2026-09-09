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
# submitted. The parameters below are governance values, not constants: they
# can be changed later with `set_config`, and the defaults here are deliberately
# conservative rather than ambitious.

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

command -v stellar >/dev/null 2>&1 || {
    echo "error: the stellar CLI is not installed." >&2
    echo "see https://developers.stellar.org/docs/tools/developer-tools/cli/stellar-cli" >&2
    exit 69
}
command -v jq >/dev/null 2>&1 || {
    echo "error: jq is required to build the configuration arguments." >&2
    exit 69
}

if [[ ! -f "$WASM_DIR/aphelion_registry.wasm" ]]; then
    echo "No build artefacts found; building..."
    "$ROOT/scripts/build-contracts.sh"
fi

invoke() {
    local contract="$1"
    shift
    stellar contract invoke \
        --id "$contract" \
        --source-account "$APHELION_STELLAR_SECRET" \
        --rpc-url "$RPC_URL" \
        --network-passphrase "$PASSPHRASE" \
        -- "$@"
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
    TOKEN="$(stellar contract id asset \
        --asset native \
        --source-account "$APHELION_STELLAR_SECRET" \
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
  quorum             : $QUORUM nodes / $MIN_WEIGHT_BPS bps
  deviation band     : $MAX_DEVIATION_BPS bps
  round interval     : $MIN_ROUND_INTERVAL seconds
  feeds              : $FEEDS
  dispute committee  : $COMMITTEE
  dispute quorum     : $DISPUTE_QUORUM

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
    --unbonding_period "$UNBONDING"

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
    '{admin: $admin, registry: $registry, token: $token, quorum: $quorum,
      voting_period: $voting_period, appeal_period: $appeal_period,
      dispute_bond: $dispute_bond, appeal_bond: $appeal_bond,
      rep_penalty: $rep_penalty, slash_amount: $slash_amount,
      reporter_reward: $reporter_reward}')"
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
DONE
