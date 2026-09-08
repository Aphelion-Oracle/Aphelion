//! Node-wide error type.
//!
//! The variants are split by *who has to act*: a `Source` error means an
//! exchange is misbehaving and the node should carry on with its other
//! sources; a `Config` or `Signing` error means the operator has to intervene
//! and the process should refuse to start rather than publish garbage.

use aphelion_core::FeedId;

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("signing key error: {0}")]
    Signing(String),

    // The field is `venue` rather than `source` because `thiserror` reserves
    // that name for an error's underlying cause.
    #[error("data source `{venue}` failed for feed `{feed}`: {detail}")]
    Source {
        venue: &'static str,
        feed: FeedId,
        detail: String,
    },

    #[error("feed `{feed}` has {available} usable source(s), need at least {required}")]
    InsufficientSources {
        feed: FeedId,
        available: usize,
        required: usize,
    },

    #[error("feed `{feed}` sources disagree by {observed_bps} bps (limit {limit_bps})")]
    SourceDisagreement {
        feed: FeedId,
        observed_bps: u32,
        limit_bps: u32,
    },

    #[error("chain interaction failed: {0}")]
    Chain(String),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl NodeError {
    /// Whether the round loop should keep going after seeing this error.
    ///
    /// Data problems are expected in normal operation — exchanges rate-limit,
    /// feeds go quiet. Configuration and key problems are not, and a node that
    /// keeps looping through them just fills the log while publishing nothing.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            NodeError::Source { .. }
                | NodeError::InsufficientSources { .. }
                | NodeError::SourceDisagreement { .. }
                | NodeError::Chain(_)
                | NodeError::Http(_)
                | NodeError::Database(_)
        )
    }

    /// Short, low-cardinality label for the `aphelion_round_errors_total` metric.
    pub fn kind(&self) -> &'static str {
        match self {
            NodeError::Config(_) => "config",
            NodeError::Signing(_) => "signing",
            NodeError::Source { .. } => "source",
            NodeError::InsufficientSources { .. } => "insufficient_sources",
            NodeError::SourceDisagreement { .. } => "source_disagreement",
            NodeError::Chain(_) => "chain",
            NodeError::Database(_) => "database",
            NodeError::Http(_) => "http",
            NodeError::Other(_) => "other",
        }
    }
}

pub type Result<T, E = NodeError> = std::result::Result<T, E>;
