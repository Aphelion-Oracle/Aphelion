//! Postgres access.
//!
//! Queries are written out longhand rather than with `sqlx::query!` so that the
//! crate builds without a live database — an operator cloning the repo should
//! be able to `cargo build` before they have provisioned anything.

pub mod models;
pub mod repo;

use std::time::Duration;

use sqlx::postgres::{PgPool, PgPoolOptions};

use crate::config::DatabaseConfig;
use crate::error::{NodeError, Result};

pub use models::*;
pub use repo::Repo;

/// Connect and run migrations.
pub async fn connect(cfg: &DatabaseConfig) -> Result<PgPool> {
    let url = crate::Config::secret_from_env(&cfg.url_env)?;
    let pool = PgPoolOptions::new()
        .max_connections(cfg.max_connections)
        .acquire_timeout(Duration::from_secs(10))
        // Recycle idle connections so a long-lived node does not hold a pool
        // full of sockets a restarted Postgres has already forgotten about.
        .idle_timeout(Duration::from_secs(600))
        .connect(&url)
        .await?;

    sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .map_err(|e| NodeError::Config(format!("migrations failed: {e}")))?;

    Ok(pool)
}
