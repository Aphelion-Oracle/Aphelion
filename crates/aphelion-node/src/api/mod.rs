//! The node's HTTP surface.
//!
//! Deliberately read-only. Everything here answers a question an operator or a
//! monitoring system asks; nothing here changes what the node publishes. That
//! constraint is what makes it safe to expose the port inside a cluster
//! without an auth layer, and it should not be relaxed casually — an endpoint
//! that could nudge a price would make the HTTP port as sensitive as the
//! signing key.

mod routes;
mod state;

pub use routes::router;
pub use state::AppState;

use std::net::SocketAddr;

use crate::error::{NodeError, Result};

/// Serve until `shutdown` flips to true.
pub async fn serve(
    state: AppState,
    bind: &str,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let addr: SocketAddr = bind
        .parse()
        .map_err(|e| NodeError::Config(format!("invalid api.bind `{bind}`: {e}")))?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| NodeError::Config(format!("cannot bind {addr}: {e}")))?;

    tracing::info!(%addr, "http api listening");

    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        })
        .await
        .map_err(|e| NodeError::Other(anyhow::anyhow!("http server failed: {e}")))
}
