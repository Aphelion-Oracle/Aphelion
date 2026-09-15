#!/usr/bin/env python3
"""Regenerate the shared test vectors under tests/vectors/.

Three files, all asserted from the off-chain Rust and from the Soroban
contracts:

  price_message.json      the canonical signing payload
  aggregation.json        the consensus arithmetic
  beacon_commitment.json  the randomness beacon's two payloads

This script is a third, independent implementation of both. Writing it in
another language is the point: two implementations that agree may simply
share a misreading of the specification, and the specification here is a byte
layout and an integer rounding rule -- exactly the kind of thing that reads
the same to two people and compiles differently.

A drift in price_message.json means no signature verifies on chain. A drift in
aggregation.json means a node can predict one round outcome while the contract
computes another, and be slashed for the difference. A drift in
beacon_commitment.json is the nastiest of the three: a node computes a
commitment the contract will not reproduce, so its reveal is rejected as a bad
one and it is penalised for withholding a secret it did in fact publish. All
three are consensus bugs, not flaky tests.

Usage: python3 scripts/gen_test_vectors.py
"""
import hashlib
import json
import pathlib

DOMAIN = b"APHELION_PRICE_V1"
MESSAGE_LEN = 117

COMMITMENT_DOMAIN = b"APHELION_RANDOM_V1"
COMMIT_SIGNATURE_DOMAIN = b"APHELION_COMMIT_V1"
COMMITMENT_PREIMAGE_LEN = 122
COMMIT_MESSAGE_LEN = 90

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


# --- aggregation ------------------------------------------------------------
#
# Mirrors of aphelion-core::math and contracts/aggregator/src/math.rs. Every
# division truncates toward zero, as Rust's `/` does on integers, and the
# midpoint uses the same bitwise form so that a tie between two enormous values
# cannot differ by one unit between implementations.


def trunc_div(a: int, b: int) -> int:
    q = abs(a) // abs(b)
    return -q if (a < 0) != (b < 0) else q


def midpoint(a: int, b: int) -> int:
    return (a & b) + ((a ^ b) >> 1)


def weighted_median(samples):
    if not samples:
        return None
    total = sum(w for _, w in samples)
    if total == 0:
        return None
    ordered = sorted(samples, key=lambda s: s[0])
    half = total // 2
    cumulative = 0
    for i, (value, weight) in enumerate(ordered):
        cumulative += weight
        if total % 2 == 0 and cumulative == half:
            nxt = ordered[i + 1][0] if i + 1 < len(ordered) else value
            return midpoint(value, nxt)
        if cumulative * 2 > total:
            return value
    return ordered[-1][0]


def isqrt(n: int) -> int:
    if n <= 0:
        return 0
    if n < 4:
        return 1
    x, y = n, (n + 1) // 2
    while y < x:
        x = y
        y = (x + n // x) // 2
    return x


def stddev(values):
    if len(values) < 2:
        return 0
    mu = trunc_div(sum(values), len(values))
    sum_sq = sum((v - mu) ** 2 for v in values)
    return isqrt(trunc_div(sum_sq, len(values)))


def deviation_bps(value: int, reference: int) -> int:
    if reference == 0:
        return 2**32 - 1
    bps = trunc_div(abs(value - reference) * 10_000, abs(reference))
    return min(bps, 2**32 - 1)


def time_weighted_average(observations, window_start: int, now: int):
    if not observations or now <= window_start:
        return None
    weighted = 0
    total_time = 0
    for i, (ts, price) in enumerate(observations):
        end = min(observations[i + 1][0], now) if i + 1 < len(observations) else now
        start = max(ts, window_start)
        if end <= start:
            continue
        weighted += price * (end - start)
        total_time += end - start
    if total_time == 0:
        return observations[-1][1]
    return trunc_div(weighted, total_time)


SCALE = 100_000_000
BTC = 64_231_55 * SCALE // 100

MEDIAN_CASES = [
    ("odd count, equal weight", [(100, 10_000), (300, 10_000), (200, 10_000)]),
    ("even count averages the middle pair", [(100, 10_000), (200, 10_000), (300, 10_000), (400, 10_000)]),
    ("one outlier cannot move it", [(1000, 10_000), (1001, 10_000), (999, 10_000), (1000, 10_000), (999_999, 10_000)]),
    (
        "three proven nodes outvote four fresh ones",
        [(1000, 10_000)] * 3 + [(5000, 5_000)] * 4,
    ),
    ("a single sample is its own median", [(BTC, 5_000)]),
    ("zero weight has no median", [(1000, 0)]),
    ("no samples has no median", []),
    (
        "realistic btc round",
        [(BTC - 200, 10_000), (BTC, 10_000), (BTC + 150, 5_000), (BTC + 50, 10_000), (BTC - 50, 5_000)],
    ),
    (
        "a full-weight vote outbids two half-weight ones",
        [(BTC, 10_000), (BTC, 5_000), (BTC * 102 // 100, 5_000), (BTC * 102 // 100, 5_000)],
    ),
    ("exact tie between two values", [(1000, 5_000), (2000, 5_000)]),
]

STDDEV_CASES = [
    ("textbook", [2, 4, 4, 4, 5, 5, 7, 9]),
    ("single value", [5]),
    ("empty", []),
    ("identical values", [BTC, BTC, BTC]),
    ("cents apart at oracle magnitudes", [6_423_150 * 1_000_000, 6_423_152 * 1_000_000, 6_423_155 * 1_000_000, 6_423_158 * 1_000_000, 6_423_160 * 1_000_000]),
    ("one wild outlier", [BTC, BTC, BTC, 300 * SCALE]),
]

DEVIATION_CASES = [
    ("ten percent up", 110, 100),
    ("ten percent down", 90, 100),
    ("exact match", 100, 100),
    ("zero reference saturates", 100, 0),
    ("one basis point", 10_001, 10_000),
    ("rounding truncates rather than rounds", 10_000_9, 10_000_0),
    ("a wild submission against a real price", 300 * SCALE, BTC),
]

TWAP_CASES = [
    ("a one second spike in a sixty second window", [(0, 1000), (59, 10_000)], 0, 60),
    ("flat", [(0, BTC), (30, BTC), (60, BTC)], 0, 90),
    ("everything predates the window", [(0, 1000)], 500, 600),
    ("a step half way through", [(0, 1000), (50, 2000)], 0, 100),
    ("observations before the window are clipped", [(0, 1000), (100, 2000)], 100, 200),
    ("an empty window has no average", [(0, 1000)], 100, 100),
    ("no observations", [], 0, 100),
]


def aggregation_vectors() -> dict:
    return {
        "_comment": [
            "Canonical Aphelion consensus arithmetic. Asserted from both the off-chain",
            "implementation (crates/aphelion-core/src/math.rs) and the on-chain mirror",
            "(contracts/aggregator/src/math.rs). A node predicts a round with the first",
            "and is rewarded or penalised by the second: a difference of one unit lets an",
            "honest node be slashed for arithmetic it had no way to see.",
            "Regenerate with: python3 scripts/gen_test_vectors.py",
        ],
        "weighted_median": [
            {
                "name": name,
                "samples": [[str(v), w] for v, w in samples],
                "expected": None if weighted_median(samples) is None else str(weighted_median(samples)),
            }
            for name, samples in MEDIAN_CASES
        ],
        "stddev": [
            {
                "name": name,
                "values": [str(v) for v in values],
                "expected": str(stddev(values)) if values else None,
            }
            for name, values in STDDEV_CASES
        ],
        "deviation_bps": [
            {
                "name": name,
                "value": str(value),
                "reference": str(reference),
                "expected": deviation_bps(value, reference),
            }
            for name, value, reference in DEVIATION_CASES
        ],
        "twap": [
            {
                "name": name,
                "observations": [[ts, str(p)] for ts, p in obs],
                "window_start": start,
                "now": now,
                "expected": None
                if time_weighted_average(obs, start, now) is None
                else str(time_weighted_average(obs, start, now)),
            }
            for name, obs, start, now in TWAP_CASES
        ],
    }


# --- the beacon -------------------------------------------------------------
#
# Mirrors of aphelion-core::message and contracts/randomness/src/message.rs.
#
# Two payloads, and they are easy to confuse: both begin "APHELION_", both are
# 18-byte domains, both carry the contract id and the round id. The preimage
# then carries the node's key and the secret; the signed message carries the
# commitment instead. Swapping them produces a commitment that hashes fine and
# opens nothing, so the vectors pin both and assert the domains differ.


def commitment_preimage(contract_hex: str, round_id: int, pubkey_hex: str, secret_hex: str) -> str:
    contract = bytes.fromhex(contract_hex)
    pubkey = bytes.fromhex(pubkey_hex)
    secret = bytes.fromhex(secret_hex)
    assert len(contract) == 32, "contract id must be 32 bytes"
    assert len(pubkey) == 32, "public key must be 32 bytes"
    assert len(secret) == 32, "secret must be 32 bytes"
    buf = COMMITMENT_DOMAIN + contract + round_id.to_bytes(8, "big") + pubkey + secret
    assert len(buf) == COMMITMENT_PREIMAGE_LEN, len(buf)
    return buf.hex()


def commit_message(contract_hex: str, round_id: int, commitment_hex: str) -> str:
    contract = bytes.fromhex(contract_hex)
    commitment = bytes.fromhex(commitment_hex)
    assert len(contract) == 32, "contract id must be 32 bytes"
    assert len(commitment) == 32, "commitment must be 32 bytes"
    buf = COMMIT_SIGNATURE_DOMAIN + contract + round_id.to_bytes(8, "big") + commitment
    assert len(buf) == COMMIT_MESSAGE_LEN, len(buf)
    return buf.hex()


BEACON_CASES = [
    ("typical", "11" * 32, 42, "22" * 32, "33" * 32),
    ("round_zero", "00" * 32, 0, "01" * 32, "02" * 32),
    (
        "max_round_id",
        "ff" * 32,
        18446744073709551615,
        "fe" * 32,
        "fd" * 32,
    ),
    (
        "realistic",
        "3f9a2b7c81d045e6aa1234567890abcdef0011223344556677889900aabbccdd",
        7,
        "565cfc4e2239fcabea063061080790d58324fdd4a984c87c87055a08eff7dd62",
        "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
    ),
]


def beacon_vectors():
    cases = []
    for name, contract, round_id, pubkey, secret in BEACON_CASES:
        preimage = commitment_preimage(contract, round_id, pubkey, secret)
        commitment = hashlib.sha256(bytes.fromhex(preimage)).hexdigest()
        cases.append(
            {
                "name": name,
                "contract_hex": contract,
                "round_id": round_id,
                "pubkey_hex": pubkey,
                "secret_hex": secret,
                "preimage_hex": preimage,
                # What the node stores and the contract recomputes. A node whose
                # SHA-256 of the preimage differs from this publishes a
                # commitment its own reveal will not open.
                "commitment_hex": commitment,
                "commit_message_hex": commit_message(contract, round_id, commitment),
            }
        )
    return {
        "_comment": [
            "Canonical Aphelion beacon payloads: the commitment preimage and the message",
            "signed alongside it. Asserted from the off-chain implementation",
            "(crates/aphelion-core/src/message.rs) and the on-chain mirror",
            "(contracts/randomness/src/message.rs).",
            "A drift here does not merely fail: the node publishes a commitment the",
            "contract cannot reproduce, its reveal is rejected as a bad one, and it is",
            "penalised for withholding a secret it did publish.",
            "Regenerate with: python3 scripts/gen_test_vectors.py",
        ],
        "commitment_domain": COMMITMENT_DOMAIN.decode(),
        "signature_domain": COMMIT_SIGNATURE_DOMAIN.decode(),
        "preimage_len": COMMITMENT_PREIMAGE_LEN,
        "commit_message_len": COMMIT_MESSAGE_LEN,
        "cases": cases,
    }


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

    aggregation = aggregation_vectors()
    path = ROOT / "tests" / "vectors" / "aggregation.json"
    path.write_text(json.dumps(aggregation, indent=2) + "\n")
    count = sum(len(v) for k, v in aggregation.items() if k != "_comment")
    print(f"wrote {count} vectors to {path.relative_to(ROOT)}")

    beacon = beacon_vectors()
    path = ROOT / "tests" / "vectors" / "beacon_commitment.json"
    path.write_text(json.dumps(beacon, indent=2) + "\n")
    print(f"wrote {len(beacon['cases'])} vectors to {path.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
