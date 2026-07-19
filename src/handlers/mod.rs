//! Function handlers for the `migrate::*` namespace.

pub mod codegen;
pub mod create;
pub mod status;
pub mod tracking;
pub mod up;

use std::sync::Arc;

use iii_sdk::IIIClient;
use tokio::sync::OnceCell;

use crate::config::{Dialect, WorkerConfig};
use crate::error::MigrateError;

/// Shared state for all handlers. Config is read once at startup (no hot
/// reload — a migration must not change target mid-run); the dialect is
/// detected lazily on first use so the worker can boot before the database
/// worker does.
#[derive(Clone)]
pub struct AppState {
    pub iii: IIIClient,
    pub config: Arc<WorkerConfig>,
    dialect: Arc<OnceCell<Dialect>>,
}

impl AppState {
    pub fn new(iii: IIIClient, config: WorkerConfig) -> Self {
        Self {
            iii,
            config: Arc::new(config),
            dialect: Arc::new(OnceCell::new()),
        }
    }

    /// Resolve the SQL dialect: explicit `dialect` config wins, otherwise
    /// auto-detect once via `database::listDatabases` and cache the result.
    pub async fn dialect(&self) -> Result<Dialect, MigrateError> {
        if let Some(d) = self.config.dialect {
            return Ok(d);
        }
        self.dialect
            .get_or_try_init(|| async {
                let d = crate::db::detect_dialect(&self.iii, &self.config.db).await?;
                tracing::info!(db = %self.config.db, dialect = %d, "detected dialect");
                Ok(d)
            })
            .await
            .copied()
    }
}
