//! Minimal Soroban JSON-RPC client.
//!
//! Only the calls that need no transaction construction live here — health and
//! liveness. Anything that builds, signs or simulates a transaction goes
//! through [`super::cli`], which delegates to the `stellar` CLI rather than
//! reimplementing XDR assembly.

use serde_json::json;

use crate::error::{NodeError, Result};

#[derive(Clone)]
pub struct RpcClient {
    client: reqwest::Client,
    url: String,
}

#[derive(Debug, Clone)]
pub struct LedgerInfo {
    pub sequence: u32,
    pub protocol_version: u32,
}

impl RpcClient {
    pub fn new(url: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .user_agent(concat!("aphelion-node/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|e| NodeError::Config(format!("cannot build RPC client: {e}")))?,
            url: url.into(),
        })
    }

    /// `getLatestLedger`. Used as the RPC reachability probe in `/health`:
    /// it is cheap, unauthenticated, and fails in exactly the ways a broken
    /// endpoint does.
    pub async fn latest_ledger(&self) -> Result<LedgerInfo> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getLatestLedger"
        });

        let resp = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| NodeError::Chain(format!("RPC request failed: {e}")))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| NodeError::Chain(format!("RPC body unreadable: {e}")))?;

        if !status.is_success() {
            let snippet: String = text.chars().take(200).collect();
            return Err(NodeError::Chain(format!("RPC HTTP {status}: {snippet}")));
        }

        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| NodeError::Chain(format!("RPC returned invalid JSON: {e}")))?;

        if let Some(err) = value.get("error") {
            return Err(NodeError::Chain(format!("RPC error: {err}")));
        }

        let result = value
            .get("result")
            .ok_or_else(|| NodeError::Chain("RPC response has no result".into()))?;

        Ok(LedgerInfo {
            sequence: result
                .get("sequence")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| NodeError::Chain("getLatestLedger has no sequence".into()))?
                as u32,
            protocol_version: result
                .get("protocolVersion")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}
