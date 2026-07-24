//! Client for the `database` worker — the only path to a database.
//!
//! miiigrate opens no database connection of its own. Every operation is an
//! engine invocation of `database::query`, `database::execute`,
//! `database::transaction`, or `database::listDatabases`.

use iii_sdk::protocol::TriggerRequest;
use iii_sdk::IIIClient;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::config::Dialect;
use crate::error::MigrateError;

/// Default invocation timeout for reads and single writes.
const CALL_TIMEOUT_MS: u64 = 30_000;
/// Invocation timeout for `database::transaction` batches. Migrations can
/// legitimately hold a DDL statement for minutes; align with the database
/// worker's interactive-transaction ceiling (300 s).
const TX_TIMEOUT_MS: u64 = 300_000;

/// A statement of a `database::transaction` batch.
#[derive(Debug, Clone)]
pub struct TxStatement {
    pub sql: String,
    pub params: Vec<Value>,
}

impl TxStatement {
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            params: Vec::new(),
        }
    }

    pub fn with_params(sql: impl Into<String>, params: Vec<Value>) -> Self {
        Self {
            sql: sql.into(),
            params,
        }
    }
}

/// Row envelope returned by `database::query`.
#[derive(Debug, Deserialize)]
pub struct QueryResp {
    pub rows: Vec<Map<String, Value>>,
    #[serde(default)]
    pub row_count: usize,
}

/// Failure of a `database::*` invocation, split into transport problems
/// (worker missing/unreachable) and errors reported by the worker itself.
#[derive(Debug)]
pub enum DbCallError {
    /// Engine-level failure: timeout, not connected, `function_not_found`.
    Unavailable { message: String },
    /// The database worker executed the call and returned an error body.
    /// `code` is the worker's stable code (`DRIVER_ERROR`, `UNKNOWN_DB`, …)
    /// when the body was parseable.
    Worker {
        code: Option<String>,
        message: String,
        /// `failed_index` from `DRIVER_ERROR` during a transaction batch.
        failed_index: Option<usize>,
        /// Driver-native error code from the body (`inner_code`): the
        /// Postgres SQLSTATE, or the SQLite extended result code.
        inner_code: Option<String>,
        /// `driver` from the body (`postgres`, `sqlite`).
        driver: Option<String>,
        /// Full parsed error body, for embedding in `MIGRATION_FAILED`.
        body: Option<Value>,
    },
}

/// Build a `Worker` error from a parsed database-worker body, surfacing the
/// driver-native code (`inner_code`, e.g. a Postgres SQLSTATE) in the message
/// so callers can diagnose without digging into `database_error`.
fn worker_error_from_body(body: Value, fallback_message: &str) -> DbCallError {
    let code = body.get("code").and_then(Value::as_str).map(String::from);
    let driver = body.get("driver").and_then(Value::as_str).map(String::from);
    let inner_code = body
        .get("inner_code")
        .and_then(Value::as_str)
        .map(String::from);
    let failed_index = body
        .get("failed_index")
        .and_then(Value::as_u64)
        .map(|i| i as usize);
    let base = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or(fallback_message);
    let message = match (&inner_code, driver.as_deref()) {
        // Postgres inner codes are SQLSTATEs; name them as such — the
        // five-character code is what the docs and search engines index.
        (Some(c), Some("postgres") | None) => format!("{base} (SQLSTATE {c})"),
        (Some(c), Some(d)) => format!("{base} ({d} error code {c})"),
        (None, _) => base.to_string(),
    };
    DbCallError::Worker {
        code,
        message,
        failed_index,
        inner_code,
        driver,
        body: Some(body),
    }
}

impl DbCallError {
    /// Map to the caller-facing error for non-migration contexts (status,
    /// introspection). Migration application maps `Worker` variants to
    /// `MIGRATION_FAILED` itself to attach the file name.
    pub fn into_migrate_error(self) -> MigrateError {
        match self {
            DbCallError::Unavailable { message } => {
                MigrateError::DatabaseWorkerUnavailable { message }
            }
            DbCallError::Worker { code, message, .. } => match code.as_deref() {
                Some("UNKNOWN_DB") | Some("CONFIG_ERROR") => MigrateError::ConfigError {
                    message: format!("database worker rejected the call: {message}"),
                },
                _ => MigrateError::DatabaseWorkerUnavailable {
                    message: format!("database call failed: {message}"),
                },
            },
        }
    }
}

/// Convert an SDK invocation error into a [`DbCallError`].
///
/// The database worker returns its structured errors as `Error::Handler`
/// bodies, which reach us as `Error::Remote { code: "invocation_failed",
/// message: <JSON body> }`. The stable code lives inside that JSON body.
fn map_sdk_error(e: iii_sdk::errors::Error) -> DbCallError {
    use iii_sdk::errors::Error;
    match e {
        Error::Timeout => DbCallError::Unavailable {
            message: "invocation timed out".into(),
        },
        Error::NotConnected => DbCallError::Unavailable {
            message: "engine not connected".into(),
        },
        Error::Remote { code, message, .. } => {
            if code == "function_not_found" {
                return DbCallError::Unavailable {
                    message: format!(
                        "database worker function not registered ({message}); is the database worker running?"
                    ),
                };
            }
            // The database worker's structured body is a JSON object, but the
            // SDK dispatch prefixes it ("handler error: {...}") — parse from
            // the first `{`.
            let json_part = message.find('{').map(|i| &message[i..]).unwrap_or("");
            match serde_json::from_str::<Value>(json_part) {
                Ok(body) if body.get("code").is_some() => worker_error_from_body(body, &message),
                _ => DbCallError::Worker {
                    code: Some(code),
                    message,
                    failed_index: None,
                    inner_code: None,
                    driver: None,
                    body: None,
                },
            }
        }
        other => DbCallError::Unavailable {
            message: other.to_string(),
        },
    }
}

async fn call(
    iii: &IIIClient,
    function_id: &str,
    payload: Value,
    timeout_ms: u64,
) -> Result<Value, DbCallError> {
    iii.trigger(TriggerRequest {
        function_id: function_id.to_string(),
        payload,
        action: None,
        timeout_ms: Some(timeout_ms),
    })
    .await
    .map_err(map_sdk_error)
}

/// `database::query` — read-only.
pub async fn query(
    iii: &IIIClient,
    db: &str,
    sql: &str,
    params: Vec<Value>,
) -> Result<QueryResp, DbCallError> {
    let resp = call(
        iii,
        "database::query",
        json!({ "db": db, "sql": sql, "params": params }),
        CALL_TIMEOUT_MS,
    )
    .await?;
    serde_json::from_value(resp).map_err(|e| DbCallError::Worker {
        code: None,
        message: format!("unexpected database::query response shape: {e}"),
        failed_index: None,
        inner_code: None,
        driver: None,
        body: None,
    })
}

/// `database::execute` — single write statement.
pub async fn execute(
    iii: &IIIClient,
    db: &str,
    sql: &str,
    params: Vec<Value>,
) -> Result<Value, DbCallError> {
    call(
        iii,
        "database::execute",
        json!({ "db": db, "sql": sql, "params": params }),
        CALL_TIMEOUT_MS,
    )
    .await
}

/// `database::transaction` — one-shot atomic batch. `isolation` is passed
/// through; on sqlite `serializable` maps to `BEGIN IMMEDIATE` inside the
/// database worker, which is the serialization mechanism for concurrent
/// migrators there.
pub async fn transaction(
    iii: &IIIClient,
    db: &str,
    statements: &[TxStatement],
    isolation: Option<&str>,
) -> Result<Value, DbCallError> {
    let stmts: Vec<Value> = statements
        .iter()
        .map(|s| json!({ "sql": s.sql, "params": s.params }))
        .collect();
    let mut payload = json!({ "db": db, "statements": stmts });
    if let Some(iso) = isolation {
        payload["isolation"] = json!(iso);
    }
    let resp = call(iii, "database::transaction", payload, TX_TIMEOUT_MS).await?;
    // The worker reports batch failure both as an error and, in some rollback
    // paths, as `{ committed: false, error, failed_index }` inside an Ok
    // response. Normalize the latter to DbCallError::Worker.
    if resp.get("committed").and_then(Value::as_bool) == Some(false) {
        let outer_failed_index = resp
            .get("failed_index")
            .and_then(Value::as_u64)
            .map(|i| i as usize);
        return Err(match resp.get("error").cloned() {
            Some(body) => {
                // The batch index may live next to `error` rather than inside
                // it — keep whichever the body itself did not provide.
                match worker_error_from_body(body, "transaction rolled back") {
                    DbCallError::Worker {
                        code,
                        message,
                        failed_index,
                        inner_code,
                        driver,
                        body,
                    } => DbCallError::Worker {
                        code,
                        message,
                        failed_index: failed_index.or(outer_failed_index),
                        inner_code,
                        driver,
                        body,
                    },
                    other => other,
                }
            }
            None => DbCallError::Worker {
                code: None,
                message: "transaction rolled back".to_string(),
                failed_index: outer_failed_index,
                inner_code: None,
                driver: None,
                body: None,
            },
        });
    }
    Ok(resp)
}

/// Detect the SQL dialect of `db` via `database::listDatabases`.
pub async fn detect_dialect(iii: &IIIClient, db: &str) -> Result<Dialect, MigrateError> {
    let resp = call(iii, "database::listDatabases", json!({}), CALL_TIMEOUT_MS)
        .await
        .map_err(DbCallError::into_migrate_error)?;
    let entry = resp
        .get("databases")
        .and_then(Value::as_array)
        .and_then(|dbs| {
            dbs.iter()
                .find(|d| d.get("name").and_then(Value::as_str) == Some(db))
        })
        .ok_or_else(|| MigrateError::ConfigError {
            message: format!("db `{db}` is not configured in the database worker"),
        })?;
    let driver = entry
        .get("driver")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Dialect::from_driver(driver)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_handler_body_is_parsed_for_stable_code() {
        // Real wire shape: the SDK prefixes Handler bodies with "handler
        // error: " before they reach the caller as Remote.message.
        let e = iii_sdk::errors::Error::Remote {
            code: "invocation_failed".into(),
            message: r#"handler error: {"code":"DRIVER_ERROR","driver":"sqlite","message":"no such table","failed_index":1}"#.into(),
            stacktrace: None,
        };
        match map_sdk_error(e) {
            DbCallError::Worker {
                code,
                failed_index,
                body,
                ..
            } => {
                assert_eq!(code.as_deref(), Some("DRIVER_ERROR"));
                assert_eq!(failed_index, Some(1));
                assert!(body.is_some());
            }
            other => panic!("expected Worker, got {other:?}"),
        }
    }

    #[test]
    fn postgres_sqlstate_is_surfaced_in_the_message() {
        let e = iii_sdk::errors::Error::Remote {
            code: "invocation_failed".into(),
            message: r#"handler error: {"code":"DRIVER_ERROR","driver":"postgres","message":"db error","inner_code":"42703"}"#.into(),
            stacktrace: None,
        };
        match map_sdk_error(e) {
            DbCallError::Worker {
                message,
                inner_code,
                driver,
                body,
                ..
            } => {
                assert_eq!(message, "db error (SQLSTATE 42703)");
                assert_eq!(inner_code.as_deref(), Some("42703"));
                assert_eq!(driver.as_deref(), Some("postgres"));
                // The raw body keeps everything for MIGRATION_FAILED.
                assert_eq!(body.unwrap()["inner_code"], "42703");
            }
            other => panic!("expected Worker, got {other:?}"),
        }
    }

    #[test]
    fn non_postgres_inner_code_is_labeled_with_the_driver() {
        let e = iii_sdk::errors::Error::Remote {
            code: "invocation_failed".into(),
            message: r#"handler error: {"code":"DRIVER_ERROR","driver":"sqlite","message":"constraint failed","inner_code":"1555"}"#.into(),
            stacktrace: None,
        };
        match map_sdk_error(e) {
            DbCallError::Worker { message, .. } => {
                assert_eq!(message, "constraint failed (sqlite error code 1555)");
            }
            other => panic!("expected Worker, got {other:?}"),
        }
    }

    #[test]
    fn message_is_untouched_without_inner_code() {
        let e = iii_sdk::errors::Error::Remote {
            code: "invocation_failed".into(),
            message: r#"handler error: {"code":"UNKNOWN_DB","message":"no such db"}"#.into(),
            stacktrace: None,
        };
        match map_sdk_error(e) {
            DbCallError::Worker {
                message,
                inner_code,
                ..
            } => {
                assert_eq!(message, "no such db");
                assert!(inner_code.is_none());
            }
            other => panic!("expected Worker, got {other:?}"),
        }
    }

    #[test]
    fn function_not_found_maps_to_unavailable() {
        let e = iii_sdk::errors::Error::Remote {
            code: "function_not_found".into(),
            message: "No function registered".into(),
            stacktrace: None,
        };
        assert!(matches!(map_sdk_error(e), DbCallError::Unavailable { .. }));
    }

    #[test]
    fn timeout_maps_to_unavailable() {
        assert!(matches!(
            map_sdk_error(iii_sdk::errors::Error::Timeout),
            DbCallError::Unavailable { .. }
        ));
    }

    #[test]
    fn non_json_remote_message_falls_back_to_wire_code() {
        let e = iii_sdk::errors::Error::Remote {
            code: "some_engine_code".into(),
            message: "plain text".into(),
            stacktrace: None,
        };
        match map_sdk_error(e) {
            DbCallError::Worker { code, message, .. } => {
                assert_eq!(code.as_deref(), Some("some_engine_code"));
                assert_eq!(message, "plain text");
            }
            other => panic!("expected Worker, got {other:?}"),
        }
    }
}
