//! The `stellar` CLI, as a thing two clients can share.
//!
//! [`super::cli::CliChain`] drove the CLI first, and for a while it was the
//! only caller, so the process plumbing lived inside it. [`super::committee`]
//! needs exactly the same plumbing against a different contract and, in the
//! general case, a different signing account — so it lives here instead of
//! being written twice.
//!
//! The argument for shelling out at all is in [`super::cli`]; it is unchanged
//! by the move. What changes is that the secret handling, the timeout, the
//! error text and the two shapes Soroban's JSON uses for integers now have one
//! implementation rather than one per caller, which is what keeps a fix to any
//! of them from reaching only half of the calls.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::error::{NodeError, Result};

/// One configured way of invoking the `stellar` binary: an endpoint, a
/// network, and an account that signs.
pub struct StellarCli {
    binary: String,
    rpc_url: String,
    network_passphrase: String,
    account: String,
    secret: String,
    timeout: Duration,
}

impl StellarCli {
    /// `secret_env` names the environment variable holding the seed; the seed
    /// itself is never taken as an argument, so a config file that is read
    /// aloud in a support channel is not a credential.
    pub fn new(
        rpc_url: impl Into<String>,
        network_passphrase: impl Into<String>,
        account: impl Into<String>,
        secret_env: &str,
    ) -> Result<Self> {
        let secret = crate::Config::secret_from_env(secret_env)?;
        if !secret.starts_with('S') {
            return Err(NodeError::Config(format!(
                "`{secret_env}` does not look like a Stellar secret seed \
                 (expected it to start with `S`)"
            )));
        }
        Ok(Self {
            binary: std::env::var("APHELION_STELLAR_BIN").unwrap_or_else(|_| "stellar".into()),
            rpc_url: rpc_url.into(),
            network_passphrase: network_passphrase.into(),
            account: account.into(),
            secret,
            timeout: Duration::from_secs(60),
        })
    }

    /// The account whose authorisation these invocations carry.
    ///
    /// Not cosmetic: `require_auth` in the contracts is checked against this,
    /// so an operator whose committee actions are refused needs to be able to
    /// see which address was actually signing.
    pub fn account(&self) -> &str {
        &self.account
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
            self.rpc_url.clone(),
            "--network-passphrase".into(),
            self.network_passphrase.clone(),
        ]
    }

    /// Run the CLI and return `(stdout, stderr)`.
    ///
    /// The secret is redacted from the logged argv. It is still passed to the
    /// child, which is the cost of driving a CLI; what is avoided is writing
    /// it into this process's own logs.
    async fn run(&self, args: Vec<String>) -> Result<(String, String)> {
        let mut sanitised = args.clone();
        if let Some(pos) = sanitised.iter().position(|a| a == &self.secret) {
            sanitised[pos] = "$APHELION_STELLAR_SECRET".into();
        }
        tracing::debug!(cmd = %self.binary, args = ?sanitised, "invoking stellar CLI");

        let child = Command::new(&self.binary)
            .args(&args)
            .env("STELLAR_ACCOUNT", &self.account)
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
    pub async fn view(
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

    /// A call that is signed and submitted. Returns what the function returned
    /// and, where it could be recovered, the transaction hash.
    pub async fn invoke(
        &self,
        contract: &str,
        func: &str,
        args: &[(&str, String)],
    ) -> Result<(serde_json::Value, Option<String>)> {
        let mut cmd = self.base_args(contract);
        cmd.push("--".into());
        cmd.push(func.into());
        for (name, value) in args {
            cmd.push(format!("--{name}"));
            cmd.push(value.clone());
        }
        let (stdout, stderr) = self.run(cmd).await?;
        Ok((parse_json(&stdout)?, extract_tx_hash(&stderr)))
    }
}

/// The CLI prints the transaction hash on stderr in a line that also contains
/// an explorer URL. Nothing depends on finding it — a call that landed without
/// a recoverable hash is still a landed call — so a miss degrades to `None`
/// rather than an error.
pub fn extract_tx_hash(stderr: &str) -> Option<String> {
    stderr
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find(|token| token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|s| s.to_string())
}

pub fn parse_json(stdout: &str) -> Result<serde_json::Value> {
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
pub fn as_i128(v: &serde_json::Value) -> Option<i128> {
    match v {
        serde_json::Value::String(s) => s.parse().ok(),
        serde_json::Value::Number(n) => n.as_i64().map(i128::from),
        _ => None,
    }
}

pub fn as_u64(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::String(s) => s.parse().ok(),
        serde_json::Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

pub fn as_u32(v: &serde_json::Value) -> Option<u32> {
    as_u64(v).and_then(|n| u32::try_from(n).ok())
}

/// Soroban renders a fieldless enum variant as a bare string and a variant
/// with fields as a single-key object. An enum the node only reads the name of
/// has to accept both, or a contract gaining a payload becomes a parse error
/// here rather than a new case to handle.
pub fn as_variant(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(map) if map.len() == 1 => map.keys().next().cloned(),
        _ => None,
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
            extract_tx_hash(stderr).as_deref(),
            Some("9f2c1b4a5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708")
        );
    }

    #[test]
    fn missing_hash_is_not_an_error() {
        assert_eq!(extract_tx_hash("no hash here"), None);
        // A 63-char token must not be mistaken for one.
        assert_eq!(extract_tx_hash(&"a".repeat(63)), None);
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
    fn a_u32_field_that_does_not_fit_is_absent_rather_than_wrapped() {
        assert_eq!(as_u32(&serde_json::json!(10_000)), Some(10_000));
        // Silently truncating would turn an impossible weight into a
        // plausible one, which is the failure that is hard to notice.
        assert_eq!(as_u32(&serde_json::json!(u64::MAX)), None);
    }

    #[test]
    fn accepts_both_json_shapes_soroban_uses_for_enums() {
        assert_eq!(
            as_variant(&serde_json::json!("Voting")).as_deref(),
            Some("Voting")
        );
        assert_eq!(
            as_variant(&serde_json::json!({ "Upheld": [] })).as_deref(),
            Some("Upheld")
        );
        assert_eq!(as_variant(&serde_json::json!(7)), None);
        assert_eq!(as_variant(&serde_json::json!({ "a": 1, "b": 2 })), None);
    }

    #[test]
    fn empty_cli_output_decodes_as_null_not_an_error() {
        assert!(parse_json("").unwrap().is_null());
        assert!(parse_json("not json").is_err());
    }
}
