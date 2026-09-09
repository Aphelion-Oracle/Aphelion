#!/usr/bin/env python3
"""Regenerate the shared test vectors under tests/vectors/.

Two files, both asserted from the off-chain Rust and from the Soroban
contracts:

  price_message.json  the canonical signing payload
  aggregation.json    the consensus arithmetic

This script is a third, independent implementation of both. Writing it in
another language is the point: two implementations that agree may simply
share a misreading of the specification, and the specification here is a byte
layout and an integer rounding rule -- exactly the kind of thing that reads
the same to two people and compiles differently.

A drift in price_message.json means no signature verifies on chain. A drift in
aggregation.json means a node can predict one round outcome while the contract
computes another, and be slashed for the difference. Both are consensus bugs,
not flaky tests.

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


if __name__ == "__main__":
    main()
