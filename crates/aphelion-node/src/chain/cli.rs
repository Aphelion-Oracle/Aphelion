//! `ChainClient` backed by the `stellar` CLI.
//!
//! # Why shell out
//!
//! Submitting to Soroban means building a transaction, simulating it to
//! discover its footprint and resource fees, rebuilding it with those values,
//! signing, submitting, and polling for the result. The `stellar` CLI already
//! does all of that, is maintained in lockstep with the protocol, and is the
//! same binary an operator uses to deploy the contracts. Reimplementing it
//! here would add a large XDR surface to audit in exchange for saving a
//! process spawn once a minute.
//!
//! The cost is real and worth naming: one fork/exec per call, and the secret
//! key is passed through the environment of the child process. Both are
//! acceptable at a 60-second cadence; neither would be at 60 per second, which
//! is when to reach for a native client (see the note on [`super::ChainClient`]).
//!
//! The process plumbing itself lives in [`super::stellar`], shared with the
//! committee client.

use aphelion_core::{FeedId, Price};
use async_trait::async_trait;

use super::stellar::{as_i128, as_u64, StellarCli};
use super::{ChainClient, OnChainNode, OnChainPrice, SubmitReceipt, SweepReceipt};
use crate::config::NetworkConfig;
use crate::error::{NodeError, Result};
use crate::signer::SignedSubmission;

pub struct CliChain {
    cli: StellarCli,
    network: NetworkConfig,
}

impl CliChain {
    pub fn new(network: NetworkConfig) -> Result<Self> {
        let cli = StellarCli::new(
            &network.rpc_url,
            &network.network_passphrase,
            &network.submitter_account,
            &network.submitter_secret_env,
        )?;
        Ok(Self { cli, network })
    }
}

/// Decode `registry.list_nodes` into hex public keys.
///
/// A malformed entry is dropped rather than failing the whole read: the caller
/// is doing upkeep on the keys it can identify, and one unreadable entry should
/// not stop it from reaching the rest. Anything that is not 32 bytes of hex is
/// not a key this node could sweep anyway — it would be rejected by the
/// contract, at the caller's expense.
fn decode_pubkeys(value: &serde_json::Value) -> Result<Vec<String>> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let items = value.as_array().ok_or_else(|| {
        NodeError::Chain(format!(
            "registry.list_nodes returned {value}, expected an array of public keys"
        ))
    })?;
    Ok(items
        .iter()
        .filter_map(|v| v.as_str())
        .map(|s| s.trim_start_matches("0x").to_ascii_lowercase())
        .filter(|s| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()))
        .collect())
}

#[async_trait]
impl ChainClient for CliChain {
    async fn ledger_time(&self) -> Result<u64> {
        let value = self
            .cli
            .view(&self.network.aggregator_contract, "ledger_time", &[])
            .await?;
        as_u64(&value).ok_or_else(|| {
            NodeError::Chain(format!(
                "aggregator.ledger_time returned {value}, expected a u64"
            ))
        })
    }

    async fn submit_price(
        &self,
        public_key_hex: &str,
        submission: &SignedSubmission,
    ) -> Result<SubmitReceipt> {
        let m = &submission.message;
        let (value, tx_hash) = self
            .cli
            .invoke(
                &self.network.aggregator_contract,
                "submit_price",
                &[
                    ("feed", m.feed.to_string()),
                    ("pubkey", public_key_hex.to_string()),
                    ("price", m.price.raw().to_string()),
                    ("timestamp", m.timestamp.to_string()),
                    ("confidence_bps", m.confidence_bps.to_string()),
                    ("nonce", m.nonce.to_string()),
                    ("signature", submission.signature_hex()),
                ],
            )
            .await?;

        Ok(SubmitReceipt {
            tx_hash,
            // `submit_price` returns true when this submission was the one
            // that closed the round.
            finalized_round: value.as_bool().unwrap_or(false),
        })
    }

    async fn latest_price(&self, feed: &FeedId) -> Result<Option<OnChainPrice>> {
        let value = self
            .cli
            .view(
                &self.network.aggregator_contract,
                "get_price",
                &[("feed", feed.to_string())],
            )
            .await?;
        if value.is_null() {
            return Ok(None);
        }
        Ok(Some(OnChainPrice {
            feed: feed.clone(),
            price: value
                .get("price")
                .and_then(as_i128)
                .map(Price::from_raw)
                .ok_or_else(|| NodeError::Chain(format!("get_price has no price: {value}")))?,
            timestamp: value.get("timestamp").and_then(as_u64).unwrap_or(0),
            num_nodes: value.get("num_nodes").and_then(as_u64).unwrap_or(0) as u32,
            confidence_bps: value.get("confidence_bps").and_then(as_u64).unwrap_or(0) as u32,
            round_id: value.get("round_id").and_then(as_u64).unwrap_or(0),
        }))
    }

    async fn node_info(&self, public_key_hex: &str) -> Result<Option<OnChainNode>> {
        let value = self
            .cli
            .view(
                &self.network.registry_contract,
                "get_node",
                &[("pubkey", public_key_hex.to_string())],
            )
            .await?;
        if value.is_null() {
            return Ok(None);
        }
        Ok(Some(decode_node(public_key_hex, &value)))
    }

    async fn last_nonce(&self, public_key_hex: &str, feed: &FeedId) -> Result<u64> {
        let value = self
            .cli
            .view(
                &self.network.aggregator_contract,
                "last_nonce",
                &[
                    ("pubkey", public_key_hex.to_string()),
                    ("feed", feed.to_string()),
                ],
            )
            .await?;
        Ok(as_u64(&value).unwrap_or(0))
    }

    async fn list_nodes(&self) -> Result<Vec<String>> {
        let value = self
            .cli
            .view(&self.network.registry_contract, "list_nodes", &[])
            .await?;
        decode_pubkeys(&value)
    }

    async fn absence_threshold(&self) -> Result<u64> {
        let value = self
            .cli
            .view(&self.network.aggregator_contract, "get_config", &[])
            .await?;
        value
            .get("absence_threshold")
            .and_then(as_u64)
            .ok_or_else(|| {
                NodeError::Chain(format!(
                    "aggregator.get_config has no absence_threshold: {value}"
                ))
            })
    }

    async fn min_stake(&self) -> Result<i128> {
        let value = self
            .cli
            .view(&self.network.registry_contract, "get_config", &[])
            .await?;
        value.get("min_stake").and_then(as_i128).ok_or_else(|| {
            NodeError::Chain(format!("registry.get_config has no min_stake: {value}"))
        })
    }

    async fn sweep_absent(&self, pubkeys: &[String]) -> Result<SweepReceipt> {
        if pubkeys.is_empty() {
            // Nothing to charge is not a transaction. Submitting an empty
            // vector would pay a fee to learn what the caller already knows.
            return Ok(SweepReceipt {
                tx_hash: None,
                charged: 0,
            });
        }

        let encoded = serde_json::to_string(pubkeys)
            .map_err(|e| NodeError::Chain(format!("cannot encode --pubkeys: {e}")))?;
        let (value, tx_hash) = self
            .cli
            .invoke(
                &self.network.aggregator_contract,
                "sweep_absent",
                &[("pubkeys", encoded)],
            )
            .await?;

        Ok(SweepReceipt {
            tx_hash,
            charged: as_u64(&value).unwrap_or(0) as u32,
        })
    }
}

/// One `NodeView`, as the registry's `get_node` returns it.
///
/// A free function so the decoding can be tested without a CLI to drive. Every
/// field falls back rather than failing, because a record that decoded
/// partially still answers most of what the caller asked — but the fallbacks
/// are chosen to be the reading the ledger would have produced, never the
/// reassuring one.
fn decode_node(public_key_hex: &str, value: &serde_json::Value) -> OnChainNode {
    OnChainNode {
        public_key_hex: public_key_hex.to_string(),
        stake: value.get("stake").and_then(as_i128).unwrap_or(0),
        reputation: value.get("reputation").and_then(as_u64).unwrap_or(0) as u32,
        status: value
            .get("status")
            .and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| Some(v.to_string()))
            })
            .unwrap_or_else(|| "unknown".into()),
        weight_bps: value.get("weight_bps").and_then(as_u64).unwrap_or(0) as u32,
        last_submission: value.get("last_submission").and_then(as_u64).unwrap_or(0),
        // Zero is what the registry itself stores when the node is in neither
        // state, so a missing field and an absent deadline decode alike. The
        // judgement that reads them treats zero as "no deadline" rather than
        // as a moment long past, which is what makes that safe.
        jailed_until: value.get("jailed_until").and_then(as_u64).unwrap_or(0),
        unbonding_until: value.get("unbonding_until").and_then(as_u64).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::super::stellar::parse_json;
    use super::*;

    /// The clocks are the two fields on this record that nothing else would
    /// notice going missing: both decode to zero, and zero is a legal value
    /// meaning "not in that state". A rename on the contract side would
    /// silently turn every jailed node into one with no term to serve.
    #[test]
    fn a_node_record_decodes_its_deadlines() {
        let node = decode_node(
            "ab",
            &serde_json::json!({
                "stake": "10000000000",
                "reputation": 2_500,
                "status": "Jailed",
                "weight_bps": 0,
                "last_submission": 1_700_000_000u64,
                "jailed_until": 1_700_086_400u64,
                "unbonding_until": 0,
            }),
        );
        assert_eq!(node.jailed_until, 1_700_086_400);
        assert_eq!(node.unbonding_until, 0);
        assert_eq!(node.status, "Jailed");
        assert_eq!(node.stake, 10_000_000_000);
    }

    #[test]
    fn a_record_without_deadlines_is_a_node_in_neither_state() {
        let node = decode_node("ab", &serde_json::json!({"status": "Active"}));
        assert_eq!(node.jailed_until, 0);
        assert_eq!(node.unbonding_until, 0);
    }

    #[test]
    fn decodes_a_node_index_and_drops_what_is_not_a_key() {
        let key = "a".repeat(64);
        let other = "B".repeat(64);
        let decoded = decode_pubkeys(&serde_json::json!([
            key.clone(),
            format!("0x{other}"),
            "deadbeef", // too short to be a 32-byte key
            42,         // not a string at all
        ]))
        .unwrap();
        // The long ones survive, normalised to lower case and unprefixed; the
        // rest are dropped rather than failing the whole read.
        assert_eq!(decoded, vec![key, other.to_ascii_lowercase()]);
    }

    #[test]
    fn an_empty_registry_is_an_empty_list_not_an_error() {
        // A deployment with no operators yet. Sweeping it is a no-op, not a
        // fault, and the first thing a fresh testnet deployment looks like.
        assert!(decode_pubkeys(&serde_json::json!(null)).unwrap().is_empty());
        assert!(decode_pubkeys(&serde_json::json!([])).unwrap().is_empty());
        assert!(decode_pubkeys(&serde_json::json!({"nodes": []})).is_err());
    }

    #[test]
    fn empty_cli_output_decodes_as_null_not_an_error() {
        // Re-checked here because `submit_price` reads its return value
        // through it: a function that returned nothing must not be reported as
        // a round that closed.
        assert!(parse_json("").unwrap().is_null());
        assert!(!parse_json("").unwrap().as_bool().unwrap_or(false));
    }
}
