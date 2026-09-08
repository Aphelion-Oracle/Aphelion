#!/usr/bin/env bash
#
# Register this node's public key with the Aphelion registry and bond its stake.
#
# Usage:
#   scripts/register-node.sh <public-key-hex> [stake-in-stroops]
#
# The public key comes from `aphelion-node pubkey`. The stake must be at least
# the registry's configured minimum, and is transferred to the registry
# contract — not merely attested — so that slashing is arithmetic on funds
# already held rather than a claim against an account that may be empty by the
# time it matters.

set -euo pipefail

PUBKEY="${1:-}"
STAKE="${2:-10000000000}" # 1000 XLM in stroops

: "${APHELION_STELLAR_SECRET:?set APHELION_STELLAR_SECRET to the funding account secret seed (S...)}"
: "${APHELION_REGISTRY_CONTRACT:?set APHELION_REGISTRY_CONTRACT to the deployed registry contract id (C...)}"
: "${APHELION_OWNER_ACCOUNT:?set APHELION_OWNER_ACCOUNT to the public account that will own the stake (G...)}"

NETWORK="${APHELION_NETWORK:-testnet}"
RPC_URL="${APHELION_RPC_URL:-https://soroban-testnet.stellar.org}"
PASSPHRASE="${APHELION_NETWORK_PASSPHRASE:-Test SDF Network ; September 2015}"

if [[ -z "$PUBKEY" ]]; then
    echo "usage: $0 <public-key-hex> [stake-in-stroops]" >&2
    echo "hint:  $0 \"\$(aphelion-node pubkey)\"" >&2
    exit 64
fi

# A truncated or mistyped key registers an identity nobody holds the secret for,
# and the stake behind it is then locked until the unbonding period elapses.
# Cheaper to catch it here.
if [[ ! "$PUBKEY" =~ ^[0-9a-fA-F]{64}$ ]]; then
    echo "error: public key must be 64 hex characters, got ${#PUBKEY}" >&2
    exit 65
fi

command -v stellar >/dev/null 2>&1 || {
    echo "error: the stellar CLI is not installed." >&2
    echo "see https://developers.stellar.org/docs/tools/developer-tools/cli/stellar-cli" >&2
    exit 69
}

echo "Registering node"
echo "  public key : $PUBKEY"
echo "  owner      : $APHELION_OWNER_ACCOUNT"
echo "  stake      : $STAKE stroops"
echo "  registry   : $APHELION_REGISTRY_CONTRACT"
echo "  network    : $NETWORK"
echo

read -r -p "Proceed? This transfers the stake to the registry contract. [y/N] " reply
[[ "$reply" == "y" || "$reply" == "Y" ]] || { echo "aborted"; exit 1; }

stellar contract invoke \
    --id "$APHELION_REGISTRY_CONTRACT" \
    --source-account "$APHELION_STELLAR_SECRET" \
    --rpc-url "$RPC_URL" \
    --network-passphrase "$PASSPHRASE" \
    -- \
    register \
    --owner "$APHELION_OWNER_ACCOUNT" \
    --pubkey "$PUBKEY" \
    --stake "$STAKE"

echo
echo "Registered. Confirm with:"
echo "  stellar contract invoke --id $APHELION_REGISTRY_CONTRACT \\"
echo "      --source-account \$APHELION_STELLAR_SECRET --rpc-url $RPC_URL \\"
echo "      --network-passphrase '$PASSPHRASE' -- get_node --pubkey $PUBKEY"
echo
echo "The node starts at half voting weight and earns full weight through"
echo "sustained correct submissions."
