//! Discriminated error codes returned to the engine.
//!
//! Same convention as the `database` worker: the `code` field is stable and
//! clients should match on it; the remaining fields are diagnostic. Errors
//! cross the wire as `Error::Handler` bodies (JSON string), so callers see
//! wire code `invocation_failed` with this JSON as the message.

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error, Serialize)]
#[serde(tag = "code")]
pub enum MigrateError {
    #[serde(rename = "CONFIG_ERROR")]
    #[error("config error: {message}")]
    ConfigError { message: String },

    #[serde(rename = "DIR_NOT_FOUND")]
    #[error("migrations directory not found: {dir}")]
    DirNotFound { dir: String },

    #[serde(rename = "INVALID_MIGRATION_NAME")]
    #[error("invalid migration file name `{file}`: {reason}")]
    InvalidMigrationName { file: String, reason: String },

    #[serde(rename = "CHECKSUM_MISMATCH")]
    #[error(
        "checksum mismatch for applied migration `{name}`: applied={applied_checksum} file={file_checksum}"
    )]
    ChecksumMismatch {
        name: String,
        applied_checksum: String,
        file_checksum: String,
    },

    #[serde(rename = "MIGRATION_FAILED")]
    #[error("migration `{name}` failed: {message}")]
    MigrationFailed {
        name: String,
        message: String,
        /// 0-based index of the failing statement within the migration file
        /// (the advisory-lock preamble is not counted).
        #[serde(skip_serializing_if = "Option::is_none")]
        statement_index: Option<usize>,
        /// Structured error body from the `database` worker (stable `code`
        /// field inside, e.g. `DRIVER_ERROR` with a SQLSTATE `inner_code`).
        #[serde(skip_serializing_if = "Option::is_none")]
        database_error: Option<Value>,
    },

    #[serde(rename = "DATABASE_WORKER_UNAVAILABLE")]
    #[error("database worker unavailable: {message}")]
    DatabaseWorkerUnavailable { message: String },

    #[serde(rename = "UNSUPPORTED_DIALECT")]
    #[error("dialect `{dialect}` is not supported in v1 (postgres and sqlite only)")]
    UnsupportedDialect { dialect: String },

    #[serde(rename = "ADOPT_SOURCE_INVALID")]
    #[error("cannot adopt from `{path}`: {reason}")]
    AdoptSourceInvalid { path: String, reason: String },

    #[serde(rename = "ADOPT_CONFLICT")]
    #[error("adopt conflict on `{file}`: {reason}")]
    AdoptConflict { file: String, reason: String },
}

impl From<MigrateError> for iii_sdk::errors::Error {
    fn from(e: MigrateError) -> Self {
        let body = serde_json::to_string(&e)
            .unwrap_or_else(|_| format!("{{\"code\":\"CONFIG_ERROR\",\"message\":\"{e}\"}}"));
        iii_sdk::errors::Error::Handler(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_mismatch_serializes_with_stable_code() {
        let e = MigrateError::ChecksumMismatch {
            name: "20260719120000_add_users.sql".into(),
            applied_checksum: "aaa".into(),
            file_checksum: "bbb".into(),
        };
        let v: Value = serde_json::to_value(&e).unwrap();
        assert_eq!(v["code"], "CHECKSUM_MISMATCH");
        assert_eq!(v["name"], "20260719120000_add_users.sql");
    }

    #[test]
    fn migration_failed_carries_database_error() {
        let e = MigrateError::MigrationFailed {
            name: "20260719120000_add_users.sql".into(),
            message: "syntax error".into(),
            statement_index: Some(2),
            database_error: Some(serde_json::json!({"code": "DRIVER_ERROR"})),
        };
        let v: Value = serde_json::to_value(&e).unwrap();
        assert_eq!(v["code"], "MIGRATION_FAILED");
        assert_eq!(v["statement_index"], 2);
        assert_eq!(v["database_error"]["code"], "DRIVER_ERROR");
    }

    #[test]
    fn optional_fields_are_absent_when_none() {
        let e = MigrateError::MigrationFailed {
            name: "x.sql".into(),
            message: "boom".into(),
            statement_index: None,
            database_error: None,
        };
        let v: Value = serde_json::to_value(&e).unwrap();
        assert!(v.get("statement_index").is_none());
        assert!(v.get("database_error").is_none());
    }
}
