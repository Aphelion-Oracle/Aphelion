//! The node's Ed25519 identity.
//!
//! This key *is* the node's identity on chain: the registry stores its public
//! half, and the aggregator will only accept a submission whose signature
//! verifies against it. It is deliberately separate from the Stellar account
//! that pays for transactions, so that the paying account can be rotated,
//! kept in a different vault, or shared with other automation without ever
//! being able to sign a price.

use std::fs;
use std::path::Path;

use aphelion_core::{FeedId, Price, PriceMessage};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

use crate::error::{NodeError, Result};

/// On-disk key material. Written with mode 0600.
#[derive(Serialize, Deserialize)]
struct KeyFile {
    /// Always "ed25519"; present so a future format can be detected.
    algorithm: String,
    public_key: String,
    secret_key: String,
    created_at: String,
}

pub struct NodeSigner {
    signing_key: SigningKey,
    aggregator_id: [u8; 32],
}

/// Hand-written so that no code path — a debug log, an `unwrap` panic, an
/// error report — can print the secret half of the key.
impl std::fmt::Debug for NodeSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeSigner")
            .field("public_key", &self.public_key_hex())
            .field("aggregator", &hex::encode(self.aggregator_id))
            .field("secret_key", &"<redacted>")
            .finish()
    }
}

impl NodeSigner {
    /// Generate a fresh key and persist it, refusing to clobber an existing
    /// file — overwriting a node key means losing the on-chain identity and
    /// the stake bonded to it.
    pub fn generate(path: &Path) -> Result<VerifyingKey> {
        if path.exists() {
            return Err(NodeError::Signing(format!(
                "`{}` already exists; refusing to overwrite an existing node key",
                path.display()
            )));
        }
        let signing_key = SigningKey::generate(&mut OsRng);
        let key_file = KeyFile {
            algorithm: "ed25519".into(),
            public_key: hex::encode(signing_key.verifying_key().to_bytes()),
            secret_key: hex::encode(signing_key.to_bytes()),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        let json = serde_json::to_string_pretty(&key_file)
            .map_err(|e| NodeError::Signing(e.to_string()))?;

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| NodeError::Signing(e.to_string()))?;
            }
        }
        fs::write(path, json).map_err(|e| NodeError::Signing(e.to_string()))?;
        restrict_permissions(path)?;
        Ok(signing_key.verifying_key())
    }

    /// Load a key from disk, binding it to the aggregator it may sign for.
    pub fn load(path: &Path, aggregator_contract_id: [u8; 32]) -> Result<Self> {
        let raw = fs::read_to_string(path).map_err(|e| {
            NodeError::Signing(format!(
                "cannot read node key `{}`: {e}. Run `aphelion-node keygen` first.",
                path.display()
            ))
        })?;
        let key_file: KeyFile =
            serde_json::from_str(&raw).map_err(|e| NodeError::Signing(e.to_string()))?;
        if key_file.algorithm != "ed25519" {
            return Err(NodeError::Signing(format!(
                "unsupported key algorithm `{}`",
                key_file.algorithm
            )));
        }
        let secret = hex::decode(&key_file.secret_key)
            .map_err(|e| NodeError::Signing(format!("secret_key is not hex: {e}")))?;
        let secret: [u8; 32] = secret
            .try_into()
            .map_err(|_| NodeError::Signing("secret_key must be 32 bytes".into()))?;

        let signing_key = SigningKey::from_bytes(&secret);
        // Catch a hand-edited or truncated file before it silently produces
        // signatures the contract will reject.
        let derived = hex::encode(signing_key.verifying_key().to_bytes());
        if derived != key_file.public_key {
            return Err(NodeError::Signing(format!(
                "key file is inconsistent: stored public key {} does not match the \
                 secret key, which derives {derived}",
                key_file.public_key
            )));
        }
        warn_if_world_readable(path);

        Ok(Self {
            signing_key,
            aggregator_id: aggregator_contract_id,
        })
    }

    pub fn public_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    pub fn public_key_hex(&self) -> String {
        hex::encode(self.public_key().to_bytes())
    }

    /// Build and sign a price submission.
    pub fn sign_price(
        &self,
        feed: &FeedId,
        price: Price,
        timestamp: u64,
        confidence_bps: u32,
        nonce: u64,
    ) -> SignedSubmission {
        let message = PriceMessage {
            aggregator: self.aggregator_id,
            feed: feed.clone(),
            price,
            timestamp,
            confidence_bps,
            nonce,
        };
        let signature: Signature = self.signing_key.sign(&message.to_bytes());
        SignedSubmission {
            message,
            signature: signature.to_bytes(),
        }
    }
}

/// A price observation plus the signature the aggregator will verify.
#[derive(Debug, Clone)]
pub struct SignedSubmission {
    pub message: PriceMessage,
    pub signature: [u8; 64],
}

impl SignedSubmission {
    pub fn signature_hex(&self) -> String {
        hex::encode(self.signature)
    }

    /// Verify locally before spending a transaction fee on it. Cheap insurance
    /// against a corrupted key or a layout change that slipped through.
    pub fn verify(&self, public_key: &VerifyingKey) -> bool {
        use ed25519_dalek::Verifier;
        let sig = Signature::from_bytes(&self.signature);
        public_key.verify(&self.message.to_bytes(), &sig).is_ok()
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .map_err(|e| NodeError::Signing(e.to_string()))?
        .permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms).map_err(|e| NodeError::Signing(e.to_string()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn warn_if_world_readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path) {
        let mode = meta.permissions().mode() & 0o077;
        if mode != 0 {
            tracing::warn!(
                path = %path.display(),
                "node key is readable by other users; run `chmod 600` on it"
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_world_readable(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("aphelion-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn generated_key_round_trips_and_signs_verifiably() {
        let path = tmp("node-key.json");
        let public = NodeSigner::generate(&path).unwrap();

        let signer = NodeSigner::load(&path, [7u8; 32]).unwrap();
        assert_eq!(signer.public_key(), public);

        let submission = signer.sign_price(
            &FeedId::new("BTC_USD").unwrap(),
            Price::parse_decimal("64231.55").unwrap(),
            1_735_689_600,
            25,
            1,
        );
        assert!(submission.verify(&public));
    }

    #[test]
    fn refuses_to_overwrite_an_existing_key() {
        let path = tmp("node-key.json");
        NodeSigner::generate(&path).unwrap();
        let err = NodeSigner::generate(&path).unwrap_err().to_string();
        assert!(err.contains("refusing to overwrite"), "{err}");
    }

    #[test]
    fn detects_a_tampered_key_file() {
        let path = tmp("node-key.json");
        NodeSigner::generate(&path).unwrap();
        let mut file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        file["public_key"] = serde_json::json!(hex::encode([9u8; 32]));
        std::fs::write(&path, file.to_string()).unwrap();

        let err = NodeSigner::load(&path, [0u8; 32]).unwrap_err().to_string();
        assert!(err.contains("inconsistent"), "{err}");
    }

    #[test]
    fn a_signature_does_not_transfer_between_aggregators() {
        let path = tmp("node-key.json");
        let public = NodeSigner::generate(&path).unwrap();
        let testnet = NodeSigner::load(&path, [1u8; 32]).unwrap();
        let mainnet = NodeSigner::load(&path, [2u8; 32]).unwrap();

        let feed = FeedId::new("BTC_USD").unwrap();
        let price = Price::parse_decimal("100").unwrap();
        let on_testnet = testnet.sign_price(&feed, price, 100, 10, 1);

        // Replay the testnet signature against a mainnet-bound payload.
        let replayed = SignedSubmission {
            message: mainnet.sign_price(&feed, price, 100, 10, 1).message,
            signature: on_testnet.signature,
        };
        assert!(
            !replayed.verify(&public),
            "contract id must be bound into the signature"
        );
    }
}
