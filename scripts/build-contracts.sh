#!/usr/bin/env bash
#
# Build every Aphelion contract for the ledger.
#
# Usage: scripts/build-contracts.sh
#
# The target is `wasm32v1-none`, not `wasm32-unknown-unknown`. Since Rust 1.82
# the latter enables reference-types and multi-value, which the Soroban
# environment does not accept and which cannot easily be turned off; soroban-sdk
# refuses to build for it. If you have a build script or a CI job still naming
# the old target, that is the reason it stopped working.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="wasm32v1-none"
OUT="$ROOT/contracts/target/$TARGET/release"

command -v cargo >/dev/null 2>&1 || {
    echo "error: cargo is not installed." >&2
    exit 69
}

if ! rustup target list --installed 2>/dev/null | grep -qx "$TARGET"; then
    echo "Installing the $TARGET target..."
    rustup target add "$TARGET"
fi

echo "Building contracts for $TARGET..."
cargo build \
    --manifest-path "$ROOT/contracts/Cargo.toml" \
    --target "$TARGET" \
    --release

echo
printf '%-34s %10s\n' "CONTRACT" "BYTES"
for wasm in "$OUT"/*.wasm; do
    printf '%-34s %10s\n' "$(basename "$wasm")" "$(stat -c%s "$wasm" 2>/dev/null || stat -f%z "$wasm")"
done

echo
echo "Artefacts in $OUT"
echo "Deploy them with scripts/deploy.sh"
