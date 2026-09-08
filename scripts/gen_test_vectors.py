#!/usr/bin/env python3
"""Regenerate tests/vectors/price_message.json.

This is the third, independent implementation of the signing layout (after the
Rust off-chain encoder and the Soroban on-chain mirror). Writing it in another
language on purpose: if all three agree, the layout documentation in
crates/aphelion-core/src/message.rs is almost certainly right.

Usage: python3 scripts/gen_test_vectors.py
"""
import json
import pathlib

DOMAIN = b"APHELION_PRICE_V1"
MESSAGE_LEN = 117
ROOT = pathlib.Path(__file__).resolve().parent.parent


def encode(agg_hex: str, feed: str, price_raw: str, ts: int, conf: int, nonce: int) -> str:
    agg = bytes.fromhex(agg_hex)
    assert len(agg) == 32, "contract id must be 32 bytes"
    f = feed.encode()
    assert 0 < len(f) <= 32, "feed id must be 1..=32 ascii bytes"
    buf = DOMAIN + agg + f + b"\x00" * (32 - len(f))
    buf += int(price_raw).to_bytes(16, "big", signed=True)
    buf += int(ts).to_bytes(8, "big")
    buf += int(conf).to_bytes(4, "big")
    buf += int(nonce).to_bytes(8, "big")
    assert len(buf) == MESSAGE_LEN, len(buf)
    return buf.hex()


CASES = [
    ("btc_usd_typical", "11" * 32, "BTC_USD", "6423155000000", 1735689600, 25, 42),
    (
        "xlm_usd_sub_dollar",
        "3f9a2b7c81d045e6aa1234567890abcdef0011223344556677889900aabbccdd",
        "XLM_USD",
        "12500000",
        1735689660,
        200,
        1,
    ),
    ("max_length_feed_id", "00" * 32, "A" * 32, "100000000", 0, 0, 0),
    (
        "large_price_and_nonce",
        "ff" * 32,
        "ETH_USD",
        "999999999999999999",
        4102444800,
        10000,
        18446744073709551615,
    ),
]


def main() -> None:
    out = {
        "_comment": [
            "Canonical Aphelion price-signing payloads. Asserted from both the off-chain",
            "implementation (crates/aphelion-core/src/message.rs) and the on-chain mirror",
            "(contracts/aggregator/src/message.rs). If these two ever disagree, every",
            "submission is rejected by ed25519_verify, so treat a failure here as a",
            "consensus-breaking bug, not a flaky test.",
            "Regenerate with: python3 scripts/gen_test_vectors.py",
        ],
        "domain_separator": DOMAIN.decode(),
        "message_len": MESSAGE_LEN,
        "cases": [
            {
                "name": n,
                "aggregator_hex": a,
                "feed": f,
                "price_raw": p,
                "timestamp": t,
                "confidence_bps": c,
                "nonce": no,
                "message_hex": encode(a, f, p, t, c, no),
            }
            for (n, a, f, p, t, c, no) in CASES
        ],
    }
    path = ROOT / "tests" / "vectors" / "price_message.json"
    path.write_text(json.dumps(out, indent=2) + "\n")
    print(f"wrote {len(CASES)} vectors to {path.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
