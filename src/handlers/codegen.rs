//! `migrate::codegen` — introspect the schema through `database::query` and
//! write a TypeScript definition file.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;

use super::AppState;
use crate::codegen::{
    emit_typescript, pg_ir_from_rows, sqlite_ir_from_rows, SchemaIr, PG_COLUMNS_SQL, PG_ENUMS_SQL,
    SQLITE_COLUMNS_SQL, SQLITE_TABLES_SQL,
};
use crate::config::Dialect;
use crate::db::{self, DbCallError};
use crate::error::MigrateError;

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct CodegenReq {
    /// Output path override. Defaults to the config's `types_out`.
    #[serde(default)]
    pub out: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CodegenResp {
    pub path: String,
    pub tables: usize,
    pub enums: usize,
}

pub async fn handle(state: &AppState, req: CodegenReq) -> Result<CodegenResp, MigrateError> {
    let out = req
        .out
        .or_else(|| state.config.types_out.clone())
        .ok_or_else(|| MigrateError::ConfigError {
            message: "no output path: set `types_out` in the miiigrate configuration or pass `out` in the payload".into(),
        })?;

    let dialect = state.dialect().await?;
    let ir = introspect(state, dialect).await?;
    let ts = emit_typescript(&ir, &dialect.to_string());

    let path = Path::new(&out);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| MigrateError::ConfigError {
                message: format!("creating {}: {e}", parent.display()),
            })?;
        }
    }
    std::fs::write(path, &ts).map_err(|e| MigrateError::ConfigError {
        message: format!("writing {}: {e}", path.display()),
    })?;

    tracing::info!(
        path = %path.display(),
        tables = ir.tables.len(),
        enums = ir.enums.len(),
        "wrote TypeScript schema types"
    );
    Ok(CodegenResp {
        path: path.display().to_string(),
        tables: ir.tables.len(),
        enums: ir.enums.len(),
    })
}

async fn introspect(state: &AppState, dialect: Dialect) -> Result<SchemaIr, MigrateError> {
    let db = &state.config.db;
    match dialect {
        Dialect::Postgres => {
            let columns = db::query(&state.iii, db, PG_COLUMNS_SQL, vec![])
                .await
                .map_err(DbCallError::into_migrate_error)?;
            let enums = db::query(&state.iii, db, PG_ENUMS_SQL, vec![])
                .await
                .map_err(DbCallError::into_migrate_error)?;
            pg_ir_from_rows(&columns.rows, &enums.rows)
                .map_err(|message| MigrateError::ConfigError { message })
        }
        Dialect::Sqlite => {
            let tables = db::query(&state.iii, db, SQLITE_TABLES_SQL, vec![])
                .await
                .map_err(DbCallError::into_migrate_error)?;
            let mut per_table = Vec::new();
            for row in tables.rows {
                let Some(name) = row.get("name").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let cols = db::query(&state.iii, db, SQLITE_COLUMNS_SQL, vec![json!(name)])
                    .await
                    .map_err(DbCallError::into_migrate_error)?;
                per_table.push((name.to_string(), cols.rows));
            }
            sqlite_ir_from_rows(&per_table).map_err(|message| MigrateError::ConfigError { message })
        }
    }
}
