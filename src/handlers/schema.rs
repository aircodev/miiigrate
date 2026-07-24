//! `migrate::schema` — read-only structured report of the live schema.
//!
//! The verification companion of `migrate::up`: ordered columns with
//! defaults, primary keys, foreign keys, indexes, triggers, and Postgres
//! enums, without hand-writing `information_schema` queries through the CLI.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::AppState;
use crate::codegen::{PG_ENUMS_SQL, SQLITE_TABLES_SQL};
use crate::config::Dialect;
use crate::db::{self, DbCallError};
use crate::error::MigrateError;
use crate::schema::{
    pg_schema_from_rows, sqlite_schema_from_rows, EnumSchema, SchemaReport, SqliteIndexRaw,
    SqliteTableRaw, TableSchema, PG_FOREIGN_KEYS_SQL, PG_INDEXES_SQL, PG_PRIMARY_KEYS_SQL,
    PG_SCHEMA_COLUMNS_SQL, PG_TRIGGERS_SQL, SQLITE_FOREIGN_KEYS_SQL, SQLITE_INDEX_INFO_SQL,
    SQLITE_INDEX_LIST_SQL, SQLITE_INDEX_SQL_SQL, SQLITE_SCHEMA_COLUMNS_SQL, SQLITE_TRIGGERS_SQL,
};
use crate::TRACKING_TABLE;

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct SchemaReq {
    /// Restrict the report to one table (exact name). An unknown table
    /// yields an empty `tables` list, not an error.
    #[serde(default)]
    pub table: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SchemaResp {
    pub db: String,
    pub dialect: String,
    pub tables: Vec<TableSchema>,
    /// Postgres only; always empty on SQLite.
    pub enums: Vec<EnumSchema>,
}

pub async fn handle(state: &AppState, req: SchemaReq) -> Result<SchemaResp, MigrateError> {
    let dialect = state.dialect().await?;
    let report = match dialect {
        Dialect::Postgres => introspect_postgres(state).await?,
        Dialect::Sqlite => introspect_sqlite(state, req.table.as_deref()).await?,
    };

    let tables = match &req.table {
        Some(name) => report
            .tables
            .into_iter()
            .filter(|t| &t.name == name)
            .collect(),
        None => report.tables,
    };

    Ok(SchemaResp {
        db: state.config.db.clone(),
        dialect: dialect.to_string(),
        tables,
        enums: report.enums,
    })
}

async fn introspect_postgres(state: &AppState) -> Result<SchemaReport, MigrateError> {
    let db = &state.config.db;
    let mut rows = Vec::with_capacity(6);
    for sql in [
        PG_SCHEMA_COLUMNS_SQL,
        PG_PRIMARY_KEYS_SQL,
        PG_FOREIGN_KEYS_SQL,
        PG_INDEXES_SQL,
        PG_TRIGGERS_SQL,
        PG_ENUMS_SQL,
    ] {
        rows.push(
            db::query(&state.iii, db, sql, vec![])
                .await
                .map_err(DbCallError::into_migrate_error)?
                .rows,
        );
    }
    pg_schema_from_rows(&rows[0], &rows[1], &rows[2], &rows[3], &rows[4], &rows[5])
        .map_err(|message| MigrateError::ConfigError { message })
}

async fn introspect_sqlite(
    state: &AppState,
    only_table: Option<&str>,
) -> Result<SchemaReport, MigrateError> {
    let db = &state.config.db;
    let tables = db::query(&state.iii, db, SQLITE_TABLES_SQL, vec![])
        .await
        .map_err(DbCallError::into_migrate_error)?;

    let mut raws = Vec::new();
    for row in &tables.rows {
        let Some(name) = row.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        if name == TRACKING_TABLE || only_table.is_some_and(|t| t != name) {
            continue;
        }

        let columns = db::query(&state.iii, db, SQLITE_SCHEMA_COLUMNS_SQL, vec![json!(name)])
            .await
            .map_err(DbCallError::into_migrate_error)?
            .rows;
        let foreign_keys = db::query(&state.iii, db, SQLITE_FOREIGN_KEYS_SQL, vec![json!(name)])
            .await
            .map_err(DbCallError::into_migrate_error)?
            .rows;
        let triggers = db::query(&state.iii, db, SQLITE_TRIGGERS_SQL, vec![json!(name)])
            .await
            .map_err(DbCallError::into_migrate_error)?
            .rows;

        let index_list = db::query(&state.iii, db, SQLITE_INDEX_LIST_SQL, vec![json!(name)])
            .await
            .map_err(DbCallError::into_migrate_error)?
            .rows;
        let mut indexes = Vec::new();
        for list_row in index_list {
            let Some(index_name) = list_row.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let index_name = index_name.to_string();
            let columns = db::query(
                &state.iii,
                db,
                SQLITE_INDEX_INFO_SQL,
                vec![json!(index_name)],
            )
            .await
            .map_err(DbCallError::into_migrate_error)?
            .rows;
            let sql = db::query(
                &state.iii,
                db,
                SQLITE_INDEX_SQL_SQL,
                vec![json!(index_name)],
            )
            .await
            .map_err(DbCallError::into_migrate_error)?
            .rows
            .first()
            .and_then(|r| r.get("sql"))
            .and_then(|v| v.as_str())
            .map(String::from);
            indexes.push(SqliteIndexRaw {
                list_row,
                columns,
                sql,
            });
        }

        raws.push(SqliteTableRaw {
            name: name.to_string(),
            columns,
            foreign_keys,
            indexes,
            triggers,
        });
    }

    sqlite_schema_from_rows(&raws).map_err(|message| MigrateError::ConfigError { message })
}
