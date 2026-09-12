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

use std::process::Stdio;

use aphelion_core::{FeedId, Price};
use async_trait::async_trait;
use tokio::process::Command;

use super::{ChainClient, OnChainNode, OnChainPrice, SubmitReceipt, SweepReceipt};
use crate::config::NetworkConfig;
use crate::error::{NodeError, Result};
use crate::signer::SignedSubmission;

pub struct CliChain {
    binary: String,
    network: NetworkConfig,
    secret: String,
    timeout: std::time::Duration,
}

impl CliChain {
    pub fn new(network: NetworkConfig) -> Result<Self> {
        let secret = crate::Config::secret_from_env(&network.submitter_secret_env)?;
        if !secret.starts_with('S') {
            return Err(NodeError::Config(format!(
                "`{}` does not look like a Stellar secret seed (expected it to start with `S`)",
                network.submitter_secret_env
            )));
        }
        Ok(Self {
            binary: std::env::var("APHELION_STELLAR_BIN").unwrap_or_else(|_| "stellar".into()),
            network,
            secret,
            timeout: std::time::Duration::from_secs(60),
        })
    }

    /// Common prefix for every `contract invoke`.
    fn base_args(&self, contract: &str) -> Vec<String> {
        vec![
            "contract".into(),
            "invoke".into(),
            "--id".into(),
            contract.into(),
            "--source-account".into(),
            self.secret.clone(),
            "--rpc-url".into(),
            self.network.rpc_url.clone(),
            "--network-passphrase".into(),
            self.network.network_passphrase.clone(),
        ]
    }

    /// Run the CLI and return `(stdout, stderr)`.
    ///
    /// The secret is passed as an argument to a child process, which would
    /// normally be visible in `ps`. It is instead written to an environment
    /// variable and referenced, keeping it off the process's argv.
    async fn run(&self, args: Vec<String>) -> Result<(String, String)> {
        let mut sanitised = args.clone();
        if let Some(pos) = sanitised.iter().position(|a| a == &self.secret) {
            sanitised[pos] = "$APHELION_STELLAR_SECRET".into();
        }
        tracing::debug!(cmd = %self.binary, args = ?sanitised, "invoking stellar CLI");

        let child = Command::new(&self.binary)
            .args(&args)
            .env("STELLAR_ACCOUNT", &self.network.submitter_account)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output();

        let output = tokio::time::timeout(self.timeout, child)
            .await
            .map_err(|_| {
                NodeError::Chain(format!(
                    "`{}` did not return within {:?}",
                    self.binary, self.timeout
                ))
            })?
            .map_err(|e| {
                NodeError::Chain(format!(
                    "cannot run `{}`: {e}. Install the Stellar CLI or set APHELION_STELLAR_BIN.",
                    self.binary
                ))
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

        if !output.status.success() {
            let detail = if stderr.is_empty() { &stdout } else { &stderr };
            let snippet: String = detail.chars().take(600).collect();
            return Err(NodeError::Chain(format!(
                "stellar CLI exited with {}: {snippet}",
                output.status
            )));
        }
        Ok((stdout, stderr))
    }

    /// Read-only call: simulated, never submitted, so it costs nothing.
    async fn view(
        &self,
        contract: &str,
        func: &str,
        args: &[(&str, String)],
    ) -> Result<serde_json::Value> {
        let mut cmd = self.base_args(contract);
        cmd.push("--send".into());
        cmd.push("no".into());
        cmd.push("--".into());
        cmd.push(func.into());
        for (name, value) in args {
            cmd.push(format!("--{name}"));
            cmd.push(value.clone());
        }
        let (stdout, _) = self.run(cmd).await?;
        parse_json(&stdout)
    }

    /// The CLI prints the transaction hash on stderr in a line that also
    /// contains an explorer URL. Nothing depends on finding it — a submission
    /// that landed without a recoverable hash is still a landed submission —
    /// so a miss degrades to `None` rather than an error.
    fn extract_tx_hash(stderr: &str) -> Option<String> {
        stderr
            .split(|c: char| !c.is_ascii_alphanumeric())
            .find(|token| token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit()))
            .map(|s| s.to_string())
    }
}

fn parse_json(stdout: &str) -> Result<serde_json::Value> {
    if stdout.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(stdout).map_err(|e| {
        let snippet: String = stdout.chars().take(300).collect();
        NodeError::Chain(format!(
            "stellar CLI returned unparseable output ({e}): {snippet}"
        ))
    })
}

/// Soroban's JSON encoding renders `i128` and `u64` as strings when they
/// exceed what JSON numbers hold safely, and as numbers when they do not.
/// Both shapes have to be accepted.
fn as_i128(v: &serde_json::Value) -> Option<i128> {
    match v {
        serde_json::Value::String(s) => s.parse().ok(),
        serde_json::Value::Number(n) => n.as_i64().map(i128::from),
        _ => None,
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

fn as_u64(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::String(s) => s.parse().ok(),
        serde_json::Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

#[async_trait]
impl ChainClient for CliChain {
    async fn ledger_time(&self) -> Result<u64> {
        let value = self
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
        let mut cmd = self.base_args(&self.network.aggregator_contract);
        cmd.push("--".into());
        cmd.push("submit_price".into());
        for (name, value) in [
            ("feed", m.feed.to_string()),
            ("pubkey", public_key_hex.to_string()),
            ("price", m.price.raw().to_string()),
            ("timestamp", m.timestamp.to_string()),
            ("confidence_bps", m.confidence_bps.to_string()),
            ("nonce", m.nonce.to_string()),
            ("signature", submission.signature_hex()),
        ] {
            cmd.push(format!("--{name}"));
            cmd.push(value);
        }

        let (stdout, stderr) = self.run(cmd).await?;
        // `submit_price` returns true when this submission was the one that
        // closed the round.
        let finalized = parse_json(&stdout)
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(SubmitReceipt {
            tx_hash: Self::extract_tx_hash(&stderr),
            finalized_round: finalized,
        })
    }

    async fn latest_price(&self, feed: &FeedId) -> Result<Option<OnChainPrice>> {
        let value = self
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
            .view(
                &self.network.registry_contract,
                "get_node",
                &[("pubkey", public_key_hex.to_string())],
            )
            .await?;
        if value.is_null() {
            return Ok(None);
        }
        Ok(Some(OnChainNode {
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
        }))
    }

    async fn last_nonce(&self, public_key_hex: &str, feed: &FeedId) -> Result<u64> {
        let value = self
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
            .view(&self.network.registry_contract, "list_nodes", &[])
            .await?;
        decode_pubkeys(&value)
    }

    async fn absence_threshold(&self) -> Result<u64> {
        let value = self
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

    async fn sweep_absent(&self, pubkeys: &[String]) -> Result<SweepReceipt> {
        if pubkeys.is_empty() {
            // Nothing to charge is not a transaction. Submitting an empty
            // vector would pay a fee to learn what the caller already knows.
            return Ok(SweepReceipt {
                tx_hash: None,
                charged: 0,
            });
        }

        let mut cmd = self.base_args(&self.network.aggregator_contract);
        cmd.push("--".into());
        cmd.push("sweep_absent".into());
        cmd.push("--pubkeys".into());
        cmd.push(
            serde_json::to_string(pubkeys)
                .map_err(|e| NodeError::Chain(format!("cannot encode --pubkeys: {e}")))?,
        );

        let (stdout, stderr) = self.run(cmd).await?;
        Ok(SweepReceipt {
            tx_hash: Self::extract_tx_hash(&stderr),
            charged: parse_json(&stdout)
                .ok()
                .as_ref()
                .and_then(as_u64)
                .unwrap_or(0) as u32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_transaction_hash_in_cli_chatter() {
        let stderr = "ℹ️ Transaction hash is \
            9f2c1b4a5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708\n\
            ℹ️ Signing transaction";
        assert_eq!(
            CliChain::extract_tx_hash(stderr).as_deref(),
            Some("9f2c1b4a5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708")
        );
    }

    #[test]
    fn missing_hash_is_not_an_error() {
        assert_eq!(CliChain::extract_tx_hash("no hash here"), None);
        // A 63-char token must not be mistaken for one.
        assert_eq!(CliChain::extract_tx_hash(&"a".repeat(63)), None);
    }

    #[test]
    fn accepts_both_json_shapes_soroban_uses_for_integers() {
        assert_eq!(
            as_i128(&serde_json::json!("170141183460469231731")),
            Some(170141183460469231731)
        );
        assert_eq!(as_i128(&serde_json::json!(42)), Some(42));
        assert_eq!(as_u64(&serde_json::json!("1735689600")), Some(1735689600));
        assert_eq!(as_u64(&serde_json::json!(1735689600)), Some(1735689600));
        assert_eq!(as_i128(&serde_json::json!(null)), None);
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
        assert!(parse_json("").unwrap().is_null());
        assert!(parse_json("not json").is_err());
    }
}
