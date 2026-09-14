//! Reading and driving the randomness contract.
//!
//! A third trait rather than more methods on [`ChainClient`], for the reason
//! [`super::committee`] gives for being a second: the round loop has no
//! business with any of this, and a trait it does not use is a trait that
//! cannot break it.
//!
//! Unlike the committee's calls, these are authorised by the **node's own
//! Ed25519 key** rather than by an account. A commitment carries a signature
//! over the canonical payload in [`aphelion_core::message`], exactly as a
//! price submission does, so the account that pays the fee and the key that
//! authorises the contents stay separable.

use async_trait::async_trait;
use serde::Serialize;

use super::stellar::{as_u64, StellarCli};
use crate::config::NetworkConfig;
use crate::engine::beacon::{OurPart, RoundStatus};
use crate::error::{NodeError, Result};

/// A round as the contract reports it, decoded.
#[derive(Debug, Clone, Serialize)]
pub struct ChainRound {
    pub id: u64,
    pub status: RoundStatus,
    pub commit_deadline: u64,
    pub reveal_deadline: u64,
    pub committed: Vec<String>,
    pub revealed: Vec<String>,
    /// All zeroes until the round finalizes, and forever if it failed.
    pub output_hex: String,
}

impl ChainRound {
    /// Whether this node's key appears among the commitments.
    pub fn has_committed(&self, pubkey_hex: &str) -> bool {
        self.committed
            .iter()
            .any(|k| k.eq_ignore_ascii_case(pubkey_hex))
    }

    pub fn has_revealed(&self, pubkey_hex: &str) -> bool {
        self.revealed
            .iter()
            .any(|k| k.eq_ignore_ascii_case(pubkey_hex))
    }

    /// What this node's part in the round is, given whether a secret is on
    /// disk for it.
    ///
    /// The combination is the interesting one: committed on chain with no
    /// secret here is [`OurPart::SecretLost`], which is a different thing from
    /// anything else this can return and the only one that costs money.
    pub fn our_part(&self, pubkey_hex: &str, secret_on_disk: bool) -> OurPart {
        match (self.has_committed(pubkey_hex), secret_on_disk) {
            (true, _) if self.has_revealed(pubkey_hex) => OurPart::Revealed,
            (true, true) => OurPart::Committed,
            (true, false) => OurPart::SecretLost,
            (false, true) => OurPart::SecretStored,
            (false, false) => OurPart::None,
        }
    }
}

/// The contract's parameters.
#[derive(Debug, Clone, Serialize)]
pub struct BeaconParams {
    pub commit_window: u64,
    pub reveal_window: u64,
    pub min_participants: u32,
    pub min_round_interval: u64,
    pub no_show_rep_penalty: u32,
    pub no_show_slash: i128,
}

/// What a write returned.
#[derive(Debug, Clone, Serialize)]
pub struct Landed {
    pub tx_hash: Option<String>,
}

#[async_trait]
pub trait BeaconClient: Send + Sync {
    async fn params(&self) -> Result<BeaconParams>;
    async fn round_count(&self) -> Result<u64>;
    async fn round(&self, id: u64) -> Result<Option<ChainRound>>;
    /// The most recently finalized round and its beacon, if there is one.
    async fn latest(&self) -> Result<Option<(u64, String)>>;

    async fn open_round(&self) -> Result<Landed>;
    async fn commit(
        &self,
        pubkey_hex: &str,
        commitment_hex: &str,
        signature_hex: &str,
    ) -> Result<Landed>;
    async fn reveal(&self, pubkey_hex: &str, secret_hex: &str) -> Result<Landed>;
    async fn finalize(&self, round_id: u64) -> Result<Landed>;
}

pub struct CliBeacon {
    cli: StellarCli,
    randomness: String,
}

impl CliBeacon {
    /// Fails with a pointer rather than a contract error when no randomness
    /// contract is configured, the same as [`super::committee::CliCommittee`].
    pub fn new(network: &NetworkConfig) -> Result<Self> {
        let randomness = network.randomness_contract.clone().ok_or_else(|| {
            NodeError::Config(
                "no `randomness_contract` in the [network] section. Beacon commands \
                 need it; copy the `randomness` id out of the deployment record \
                 written by scripts/deploy.sh. A deployment may not run a beacon at \
                 all, in which case there is nothing to configure."
                    .into(),
            )
        })?;
        // The submitter, not the operator account: a commitment is authorised
        // by the node's Ed25519 signature, and the transaction underneath is
        // an ordinary one that merely has to be paid for.
        let cli = StellarCli::new(
            &network.rpc_url,
            &network.network_passphrase,
            &network.submitter_account,
            &network.submitter_secret_env,
        )?;
        Ok(Self { cli, randomness })
    }

    /// The raw 32-byte contract id, for the payloads a commitment binds to.
    pub fn contract_id(&self) -> Result<[u8; 32]> {
        crate::strkey::contract_id_bytes(&self.randomness)
    }
}

fn decode_status(v: &serde_json::Value) -> Result<RoundStatus> {
    // A status this build cannot name is an error rather than a default, the
    // same rule the dispute decoder follows: the action derived from it is
    // whether to reveal, and guessing wrong in the reassuring direction is a
    // slash.
    let raw = v
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| v.to_string());
    match raw.trim_matches('"').to_ascii_lowercase().as_str() {
        "committing" => Ok(RoundStatus::Committing),
        "revealing" => Ok(RoundStatus::Revealing),
        "finalized" => Ok(RoundStatus::Finalized),
        "failed" => Ok(RoundStatus::Failed),
        other => Err(NodeError::Chain(format!(
            "randomness contract reported a round status this build does not know: `{other}`. \
             Refusing to guess -- the decision that depends on it is whether to reveal, and \
             stake rides on getting it right."
        ))),
    }
}

fn key_list(v: Option<&serde_json::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .map(|k| {
                    k.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| k.to_string())
                })
                .map(|k| k.trim_matches('"').to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn decode_round(v: &serde_json::Value) -> Result<ChainRound> {
    let field = |name: &str| -> Result<u64> {
        v.get(name)
            .and_then(as_u64)
            .ok_or_else(|| NodeError::Chain(format!("round has no `{name}`: {v}")))
    };
    Ok(ChainRound {
        id: field("id")?,
        status: decode_status(
            v.get("status")
                .ok_or_else(|| NodeError::Chain(format!("round has no `status`: {v}")))?,
        )?,
        commit_deadline: field("commit_deadline")?,
        reveal_deadline: field("reveal_deadline")?,
        committed: key_list(v.get("committed")),
        revealed: key_list(v.get("revealed")),
        output_hex: v
            .get("output")
            .and_then(|o| o.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

#[async_trait]
impl BeaconClient for CliBeacon {
    async fn params(&self) -> Result<BeaconParams> {
        let v = self.cli.view(&self.randomness, "get_config", &[]).await?;
        let u = |name: &str| -> Result<u64> {
            v.get(name)
                .and_then(as_u64)
                .ok_or_else(|| NodeError::Chain(format!("randomness config has no `{name}`")))
        };
        Ok(BeaconParams {
            commit_window: u("commit_window")?,
            reveal_window: u("reveal_window")?,
            min_participants: u("min_participants")? as u32,
            min_round_interval: u("min_round_interval")?,
            no_show_rep_penalty: u("no_show_rep_penalty")? as u32,
            no_show_slash: v
                .get("no_show_slash")
                .and_then(super::stellar::as_i128)
                .unwrap_or(0),
        })
    }

    async fn round_count(&self) -> Result<u64> {
        let v = self.cli.view(&self.randomness, "round_count", &[]).await?;
        Ok(as_u64(&v).unwrap_or(0))
    }

    async fn round(&self, id: u64) -> Result<Option<ChainRound>> {
        let v = self
            .cli
            .view(
                &self.randomness,
                "get_round",
                &[("round_id", id.to_string())],
            )
            .await?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(Some(decode_round(&v)?))
    }

    async fn latest(&self) -> Result<Option<(u64, String)>> {
        let v = self.cli.view(&self.randomness, "latest", &[]).await?;
        if v.is_null() {
            return Ok(None);
        }
        let pair = v
            .as_array()
            .ok_or_else(|| NodeError::Chain(format!("latest is not a pair: {v}")))?;
        let id = pair.first().and_then(as_u64).unwrap_or(0);
        let output = pair
            .get(1)
            .and_then(|o| o.as_str())
            .unwrap_or_default()
            .to_string();
        Ok(Some((id, output)))
    }

    async fn open_round(&self) -> Result<Landed> {
        let (_v, tx_hash) = self.cli.invoke(&self.randomness, "open_round", &[]).await?;
        Ok(Landed { tx_hash })
    }

    async fn commit(
        &self,
        pubkey_hex: &str,
        commitment_hex: &str,
        signature_hex: &str,
    ) -> Result<Landed> {
        let (_v, tx_hash) = self
            .cli
            .invoke(
                &self.randomness,
                "commit",
                &[
                    ("pubkey", pubkey_hex.to_string()),
                    ("commitment", commitment_hex.to_string()),
                    ("signature", signature_hex.to_string()),
                ],
            )
            .await?;
        Ok(Landed { tx_hash })
    }

    async fn reveal(&self, pubkey_hex: &str, secret_hex: &str) -> Result<Landed> {
        let (_v, tx_hash) = self
            .cli
            .invoke(
                &self.randomness,
                "reveal",
                &[
                    ("pubkey", pubkey_hex.to_string()),
                    ("secret", secret_hex.to_string()),
                ],
            )
            .await?;
        Ok(Landed { tx_hash })
    }

    async fn finalize(&self, round_id: u64) -> Result<Landed> {
        let (_v, tx_hash) = self
            .cli
            .invoke(
                &self.randomness,
                "finalize",
                &[("round_id", round_id.to_string())],
            )
            .await?;
        Ok(Landed { tx_hash })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_json() -> serde_json::Value {
        serde_json::json!({
            "id": 7,
            "status": "Revealing",
            "commit_deadline": 1_735_689_700u64,
            "reveal_deadline": 1_735_690_000u64,
            "committed": ["aa", "bb"],
            "revealed": ["aa"],
            "output": "00".repeat(32),
        })
    }

    #[test]
    fn decodes_a_round() {
        let r = decode_round(&round_json()).unwrap();
        assert_eq!(r.id, 7);
        assert_eq!(r.status, RoundStatus::Revealing);
        assert_eq!(r.committed.len(), 2);
        assert_eq!(r.revealed, vec!["aa".to_string()]);
    }

    #[test]
    fn a_status_this_build_does_not_know_is_an_error() {
        // Not a default. The decision downstream is whether to reveal, and a
        // reassuring guess costs stake.
        let mut v = round_json();
        v["status"] = serde_json::json!("Adjudicating");
        let err = decode_round(&v).unwrap_err().to_string();
        assert!(err.contains("does not know"), "{err}");
    }

    #[test]
    fn status_decoding_is_case_insensitive() {
        // The CLI spells the contract enum `Revealing`; a mock or a future
        // encoder may not.
        for spelling in ["Revealing", "revealing", "REVEALING", "\"Revealing\""] {
            let mut v = round_json();
            v["status"] = serde_json::json!(spelling);
            assert_eq!(decode_round(&v).unwrap().status, RoundStatus::Revealing);
        }
    }

    #[test]
    fn our_part_reads_committed_without_a_secret_as_lost() {
        let r = decode_round(&round_json()).unwrap();
        assert_eq!(r.our_part("bb", true), OurPart::Committed);
        assert_eq!(r.our_part("bb", false), OurPart::SecretLost);
        assert_eq!(r.our_part("aa", true), OurPart::Revealed);
        assert_eq!(r.our_part("cc", true), OurPart::SecretStored);
        assert_eq!(r.our_part("cc", false), OurPart::None);
    }

    #[test]
    fn a_revealed_key_reads_as_revealed_even_with_no_secret_on_disk() {
        // The reveal already landed, so the secret is no longer needed and its
        // absence is not a problem to report.
        let r = decode_round(&round_json()).unwrap();
        assert_eq!(r.our_part("aa", false), OurPart::Revealed);
    }

    #[test]
    fn keys_are_matched_without_regard_to_case() {
        let r = decode_round(&round_json()).unwrap();
        assert!(r.has_committed("AA"));
        assert!(r.has_revealed("Aa"));
    }

    #[test]
    fn a_missing_field_is_an_error_rather_than_a_zero() {
        let mut v = round_json();
        v.as_object_mut().unwrap().remove("reveal_deadline");
        let err = decode_round(&v).unwrap_err().to_string();
        assert!(err.contains("reveal_deadline"), "{err}");
    }
}
