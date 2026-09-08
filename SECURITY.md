# Security Policy

## Status

Aphelion is pre-alpha and has not been audited. It runs on testnet only. Please
do not secure real value with it.

## Reporting a vulnerability

**Do not open a public issue for a security problem.**

Report it privately to **security@aphelion.network**, or through GitHub's
[private vulnerability reporting](https://github.com/aphelion-oracle/aphelion/security/advisories/new).

Please include:

- What the issue is, and which component is affected (a contract, the node
  service, the signing payload, deployment tooling)
- How to reproduce it, ideally as a failing test
- What an attacker gains, and what it would cost them

We aim to acknowledge a report within 3 working days and to give an assessment
with a remediation plan within 10.

If you would like credit in the advisory, say so; if you would prefer to remain
anonymous, that is fine too.

## Scope

In scope, and taken seriously:

- Anything that lets a price be published that the network's nodes did not agree
  on: signature verification, replay across feeds, deployments or nonces, or a
  divergence between the off-chain encoder and the on-chain mirror
- Anything that lets a minority of nodes move the published median
- Anything that lets a node avoid a penalty it has earned, or extract stake or
  rewards it has not
- Anything that lets an unregistered, jailed or exiting key be counted in a round
- Denial of service against consensus: making a healthy network unable to publish
- Node key or submitter secret disclosure through logs, errors or process state

Out of scope:

- An exchange returning wrong data. Aphelion's defence against that is
  multi-source aggregation with deviation filtering; a report is only in scope
  if that filtering can be *bypassed*.
- Attacks requiring control of a supermajority of registered nodes. The honest
  tolerance bound is documented in the README, and this is above it.
- Missing rate limits on the node's read-only HTTP port, which is not intended
  to be exposed publicly.
- Findings from automated scanners without a demonstrated impact.

## Consensus-critical areas

If you are looking for where to start, these carry the most risk per line:

| Area | Why it matters |
| --- | --- |
| `crates/aphelion-core/src/message.rs` and its on-chain mirror | The two must produce identical bytes; a divergence breaks every submission, and a *partial* divergence could be worse |
| `crates/aphelion-core/src/math.rs` | Weighted median and deviation are what make a minority harmless |
| `contracts/aggregator` submission path | Signature, nonce, staleness and weight checks, in that order |
| `contracts/registry` weight and penalty accounting | Determines who can influence a price, and what it costs to be wrong |
| `crates/aphelion-node/src/signer.rs` | Key handling, and the binding of a signature to one deployment |

## Disclosure

We will coordinate a disclosure date with you. For an issue affecting a live
deployment, we will normally patch and notify operators before publishing
details.
