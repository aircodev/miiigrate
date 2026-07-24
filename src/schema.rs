//! Structured schema introspection for `migrate::schema`.
//!
//! Where `codegen` reduces the schema to a TypeScript-oriented IR, this
//! module reports what is actually in the database — ordered columns with
//! defaults, primary keys, foreign keys, indexes, triggers — so a caller can
//! verify what a migration did without hand-writing `information_schema`
//! queries. Everything is pure: `database::query` rows in, [`SchemaReport`]
//! out, unit-tested from JSON fixtures without a database.

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::codegen::{int_field, str_field};
use crate::TRACKING_TABLE;

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct SchemaReport {
    pub tables: Vec<TableSchema>,
    /// Postgres only; always empty on SQLite.
    pub enums: Vec<EnumSchema>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct TableSchema {
    pub name: String,
    /// Columns in ordinal (DDL) order.
    pub columns: Vec<ColumnSchema>,
    /// Primary-key column names in key order. Empty when the table has none.
    pub primary_key: Vec<String>,
    pub foreign_keys: Vec<ForeignKey>,
    pub indexes: Vec<Index>,
    pub triggers: Vec<Trigger>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct ColumnSchema {
    pub name: String,
    /// Dialect-native type (information_schema `data_type` on Postgres, the
    /// declared type on SQLite).
    pub data_type: String,
    pub nullable: bool,
    /// Default expression as the catalog reports it (`now()`,
    /// `nextval('users_id_seq'::regclass)`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// 1-based ordinal position.
    pub position: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct ForeignKey {
    /// Constraint name on Postgres; SQLite foreign keys are unnamed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub references_table: String,
    /// Empty on SQLite when the key targets the referenced table's implicit
    /// primary key.
    pub references_columns: Vec<String>,
    pub on_delete: String,
    pub on_update: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Index {
    pub name: String,
    pub unique: bool,
    /// Column names, best-effort; empty for expression indexes — read
    /// `definition` there.
    pub columns: Vec<String>,
    /// Full DDL when the catalog has it (`pg_indexes.indexdef`,
    /// `sqlite_master.sql`); absent for SQLite auto-indexes backing
    /// PRIMARY KEY / UNIQUE constraints.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Trigger {
    pub name: String,
    /// `BEFORE`, `AFTER`, or `INSTEAD OF`.
    pub timing: String,
    /// `INSERT` / `UPDATE` / `DELETE`, one trigger may fire on several.
    pub events: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct EnumSchema {
    pub name: String,
    pub labels: Vec<String>,
}

// ---------------------------------------------------------------------------
// Postgres rows → report
// ---------------------------------------------------------------------------

/// Columns of the current schema's base tables, with defaults and positions.
pub const PG_SCHEMA_COLUMNS_SQL: &str = "\
SELECT c.table_name, c.column_name, c.data_type, c.is_nullable, \
       c.column_default, c.ordinal_position \
FROM information_schema.columns c \
JOIN information_schema.tables t \
  ON t.table_schema = c.table_schema AND t.table_name = c.table_name \
WHERE c.table_schema = current_schema() AND t.table_type = 'BASE TABLE' \
ORDER BY c.table_name, c.ordinal_position";

/// Primary-key columns in key order.
pub const PG_PRIMARY_KEYS_SQL: &str = "\
SELECT tc.table_name, kcu.column_name, kcu.ordinal_position \
FROM information_schema.table_constraints tc \
JOIN information_schema.key_column_usage kcu \
  ON kcu.constraint_name = tc.constraint_name \
 AND kcu.constraint_schema = tc.constraint_schema \
WHERE tc.table_schema = current_schema() AND tc.constraint_type = 'PRIMARY KEY' \
ORDER BY tc.table_name, kcu.ordinal_position";

/// Foreign keys with correct multi-column pairing. `information_schema`
/// cannot express which local column maps to which referenced column, so
/// this goes through `pg_constraint` and pairs `conkey`/`confkey` with
/// `unnest … WITH ORDINALITY`.
pub const PG_FOREIGN_KEYS_SQL: &str = "\
SELECT rel.relname AS table_name, con.conname AS constraint_name, \
       att.attname AS column_name, frel.relname AS references_table, \
       fatt.attname AS references_column, ord.n AS position, \
       CASE con.confdeltype WHEN 'a' THEN 'NO ACTION' WHEN 'r' THEN 'RESTRICT' \
            WHEN 'c' THEN 'CASCADE' WHEN 'n' THEN 'SET NULL' WHEN 'd' THEN 'SET DEFAULT' END AS on_delete, \
       CASE con.confupdtype WHEN 'a' THEN 'NO ACTION' WHEN 'r' THEN 'RESTRICT' \
            WHEN 'c' THEN 'CASCADE' WHEN 'n' THEN 'SET NULL' WHEN 'd' THEN 'SET DEFAULT' END AS on_update \
FROM pg_constraint con \
JOIN pg_class rel ON rel.oid = con.conrelid \
JOIN pg_namespace nsp ON nsp.oid = rel.relnamespace \
JOIN pg_class frel ON frel.oid = con.confrelid \
CROSS JOIN LATERAL unnest(con.conkey, con.confkey) WITH ORDINALITY AS ord(attnum, fattnum, n) \
JOIN pg_attribute att  ON att.attrelid = con.conrelid  AND att.attnum = ord.attnum \
JOIN pg_attribute fatt ON fatt.attrelid = con.confrelid AND fatt.attnum = ord.fattnum \
WHERE con.contype = 'f' AND nsp.nspname = current_schema() \
ORDER BY rel.relname, con.conname, ord.n";

/// Index definitions of the current schema.
pub const PG_INDEXES_SQL: &str = "\
SELECT tablename AS table_name, indexname AS index_name, indexdef AS definition \
FROM pg_indexes WHERE schemaname = current_schema() \
ORDER BY tablename, indexname";

/// Triggers, one row per event (aggregated in Rust).
pub const PG_TRIGGERS_SQL: &str = "\
SELECT event_object_table AS table_name, trigger_name, \
       action_timing AS timing, event_manipulation AS event, action_statement \
FROM information_schema.triggers \
WHERE trigger_schema = current_schema() \
ORDER BY event_object_table, trigger_name, event_manipulation";

/// Build the report from `database::query` rows of the five `PG_*_SQL`
/// queries above plus [`crate::codegen::PG_ENUMS_SQL`]. The tracking table
/// is excluded everywhere.
pub fn pg_schema_from_rows(
    columns: &[Map<String, Value>],
    pks: &[Map<String, Value>],
    fks: &[Map<String, Value>],
    indexes: &[Map<String, Value>],
    triggers: &[Map<String, Value>],
    enums: &[Map<String, Value>],
) -> Result<SchemaReport, String> {
    let mut tables: Vec<TableSchema> = Vec::new();
    for row in columns {
        let table = str_field(row, "table_name")?;
        if table == TRACKING_TABLE {
            continue;
        }
        let column = ColumnSchema {
            name: str_field(row, "column_name")?,
            data_type: str_field(row, "data_type")?,
            nullable: str_field(row, "is_nullable")? == "YES",
            default: opt_str_field(row, "column_default"),
            position: int_field(row, "ordinal_position").unwrap_or(0) as u32,
        };
        match tables.last_mut() {
            Some(t) if t.name == table => t.columns.push(column),
            _ => tables.push(TableSchema {
                name: table,
                columns: vec![column],
                primary_key: Vec::new(),
                foreign_keys: Vec::new(),
                indexes: Vec::new(),
                triggers: Vec::new(),
            }),
        }
    }

    // Rows for the tracking table (or anything else not in `tables`) fall
    // through the lookups below and are dropped, which is the point.
    for row in pks {
        let table = str_field(row, "table_name")?;
        if let Some(t) = tables.iter_mut().find(|t| t.name == table) {
            t.primary_key.push(str_field(row, "column_name")?);
        }
    }

    for row in fks {
        let table = str_field(row, "table_name")?;
        let Some(t) = tables.iter_mut().find(|t| t.name == table) else {
            continue;
        };
        let name = str_field(row, "constraint_name")?;
        let column = str_field(row, "column_name")?;
        let ref_column = str_field(row, "references_column")?;
        match t
            .foreign_keys
            .last_mut()
            .filter(|fk| fk.name.as_deref() == Some(name.as_str()))
        {
            Some(fk) => {
                fk.columns.push(column);
                fk.references_columns.push(ref_column);
            }
            None => t.foreign_keys.push(ForeignKey {
                name: Some(name),
                columns: vec![column],
                references_table: str_field(row, "references_table")?,
                references_columns: vec![ref_column],
                on_delete: str_field(row, "on_delete")?,
                on_update: str_field(row, "on_update")?,
            }),
        }
    }

    for row in indexes {
        let table = str_field(row, "table_name")?;
        let Some(t) = tables.iter_mut().find(|t| t.name == table) else {
            continue;
        };
        let definition = str_field(row, "definition")?;
        t.indexes.push(Index {
            name: str_field(row, "index_name")?,
            unique: definition.starts_with("CREATE UNIQUE INDEX"),
            columns: index_columns_from_def(&definition),
            definition: Some(definition),
        });
    }

    for row in triggers {
        let table = str_field(row, "table_name")?;
        let Some(t) = tables.iter_mut().find(|t| t.name == table) else {
            continue;
        };
        let name = str_field(row, "trigger_name")?;
        let event = str_field(row, "event")?;
        match t.triggers.last_mut().filter(|tr| tr.name == name) {
            Some(tr) => tr.events.push(event),
            None => t.triggers.push(Trigger {
                name,
                timing: str_field(row, "timing")?,
                events: vec![event],
                definition: opt_str_field(row, "action_statement"),
            }),
        }
    }

    let mut enum_schemas: Vec<EnumSchema> = Vec::new();
    for row in enums {
        let name = str_field(row, "enum_name")?;
        let label = str_field(row, "label")?;
        match enum_schemas.last_mut() {
            Some(e) if e.name == name => e.labels.push(label),
            _ => enum_schemas.push(EnumSchema {
                name,
                labels: vec![label],
            }),
        }
    }

    Ok(SchemaReport {
        tables,
        enums: enum_schemas,
    })
}

/// Best-effort column extraction from a `pg_indexes.indexdef`:
/// `CREATE [UNIQUE] INDEX name ON table USING btree (a, b DESC)` → `[a, b]`.
/// Expression indexes return an empty list — the definition is the truth.
fn index_columns_from_def(def: &str) -> Vec<String> {
    let Some(open) = def.find('(') else {
        return Vec::new();
    };
    let mut depth = 0usize;
    let mut end = None;
    for (i, c) in def[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(open + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(end) = end else {
        return Vec::new();
    };
    let inner = &def[open + 1..end];
    let mut columns = Vec::new();
    for part in inner.split(',') {
        let part = part.trim();
        if part.contains('(') {
            return Vec::new(); // expression index
        }
        // Strip ordering qualifiers; the first token is the column name.
        let first = part.split_whitespace().next().unwrap_or_default();
        let rest_ok = part
            .split_whitespace()
            .skip(1)
            .all(|t| matches!(t, "ASC" | "DESC" | "NULLS" | "FIRST" | "LAST"));
        if first.is_empty() || !rest_ok {
            return Vec::new();
        }
        columns.push(first.trim_matches('"').to_string());
    }
    columns
}

// ---------------------------------------------------------------------------
// SQLite rows → report
// ---------------------------------------------------------------------------

/// One table's columns, with defaults and PK ordinals.
pub const SQLITE_SCHEMA_COLUMNS_SQL: &str = "SELECT cid, name, type, \"notnull\", dflt_value, pk \
     FROM pragma_table_info(?) ORDER BY cid";

/// One table's foreign keys; group rows by `id`, order columns by `seq`.
pub const SQLITE_FOREIGN_KEYS_SQL: &str =
    "SELECT id, seq, \"table\", \"from\", \"to\", on_update, on_delete \
     FROM pragma_foreign_key_list(?) ORDER BY id, seq";

/// One table's indexes (auto-indexes included, `origin` says which).
pub const SQLITE_INDEX_LIST_SQL: &str =
    "SELECT name, \"unique\", origin FROM pragma_index_list(?) ORDER BY name";

/// One index's columns; `name` is NULL for expression members.
pub const SQLITE_INDEX_INFO_SQL: &str =
    "SELECT seqno, name FROM pragma_index_info(?) ORDER BY seqno";

/// DDL of a named index (NULL for constraint-backed auto-indexes).
pub const SQLITE_INDEX_SQL_SQL: &str =
    "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?";

/// Triggers attached to one table.
pub const SQLITE_TRIGGERS_SQL: &str =
    "SELECT name, sql FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ? ORDER BY name";

/// Raw introspection rows for one SQLite index.
pub struct SqliteIndexRaw {
    /// Row of [`SQLITE_INDEX_LIST_SQL`].
    pub list_row: Map<String, Value>,
    /// Rows of [`SQLITE_INDEX_INFO_SQL`].
    pub columns: Vec<Map<String, Value>>,
    /// `sql` of [`SQLITE_INDEX_SQL_SQL`], when present.
    pub sql: Option<String>,
}

/// Raw introspection rows for one SQLite table.
pub struct SqliteTableRaw {
    pub name: String,
    pub columns: Vec<Map<String, Value>>,
    pub foreign_keys: Vec<Map<String, Value>>,
    pub indexes: Vec<SqliteIndexRaw>,
    pub triggers: Vec<Map<String, Value>>,
}

/// Build the report from per-table pragma rows. The tracking table is
/// excluded; SQLite has no enums.
pub fn sqlite_schema_from_rows(tables: &[SqliteTableRaw]) -> Result<SchemaReport, String> {
    let mut out = Vec::new();
    for raw in tables {
        if raw.name == TRACKING_TABLE {
            continue;
        }

        let mut columns = Vec::new();
        // (pk ordinal, column name) — pragma reports composite keys as
        // pk=1,2,… in declaration order of the key, not of the table.
        let mut pk_members: Vec<(i64, String)> = Vec::new();
        for row in &raw.columns {
            let name = str_field(row, "name")?;
            let notnull = int_field(row, "notnull").unwrap_or(0) != 0;
            let pk = int_field(row, "pk").unwrap_or(0);
            if pk > 0 {
                pk_members.push((pk, name.clone()));
            }
            columns.push(ColumnSchema {
                name,
                data_type: str_field(row, "type").unwrap_or_default(),
                // `INTEGER PRIMARY KEY` (rowid alias) reports notnull=0 but
                // can never be null in practice — same call as codegen.
                nullable: !notnull && pk == 0,
                default: opt_str_field(row, "dflt_value"),
                position: int_field(row, "cid").map(|c| c + 1).unwrap_or(0) as u32,
            });
        }
        pk_members.sort();
        let primary_key = pk_members.into_iter().map(|(_, name)| name).collect();

        let mut foreign_keys: Vec<ForeignKey> = Vec::new();
        let mut last_id: Option<i64> = None;
        for row in &raw.foreign_keys {
            let id = int_field(row, "id").unwrap_or(0);
            let column = str_field(row, "from")?;
            // `to` is NULL when the key targets the implicit primary key.
            let ref_column = opt_str_field(row, "to");
            match foreign_keys.last_mut() {
                Some(fk) if last_id == Some(id) => {
                    fk.columns.push(column);
                    if let Some(c) = ref_column {
                        fk.references_columns.push(c);
                    }
                }
                _ => foreign_keys.push(ForeignKey {
                    name: None,
                    columns: vec![column],
                    references_table: str_field(row, "table")?,
                    references_columns: ref_column.into_iter().collect(),
                    on_delete: str_field(row, "on_delete")?,
                    on_update: str_field(row, "on_update")?,
                }),
            }
            last_id = Some(id);
        }

        let mut indexes = Vec::new();
        for idx in &raw.indexes {
            indexes.push(Index {
                name: str_field(&idx.list_row, "name")?,
                unique: int_field(&idx.list_row, "unique").unwrap_or(0) != 0,
                columns: idx
                    .columns
                    .iter()
                    .filter_map(|r| opt_str_field(r, "name"))
                    .collect(),
                definition: idx.sql.clone(),
            });
        }

        let mut triggers = Vec::new();
        for row in &raw.triggers {
            let sql = opt_str_field(row, "sql");
            let (timing, events) = parse_sqlite_trigger(sql.as_deref().unwrap_or_default());
            triggers.push(Trigger {
                name: str_field(row, "name")?,
                timing,
                events,
                definition: sql,
            });
        }

        out.push(TableSchema {
            name: raw.name.clone(),
            columns,
            primary_key,
            foreign_keys,
            indexes,
            triggers,
        });
    }
    Ok(SchemaReport {
        tables: out,
        enums: Vec::new(),
    })
}

/// Best-effort timing/events extraction from a `CREATE TRIGGER` statement.
/// Only the header (before the `ON <table>` clause) is scanned, so keywords
/// inside the trigger body cannot leak into the result.
fn parse_sqlite_trigger(sql: &str) -> (String, Vec<String>) {
    let upper = sql.to_ascii_uppercase();
    let header = upper
        .find(" ON ")
        .map(|i| &upper[..i])
        .unwrap_or(upper.as_str());
    let timing = if header.contains("INSTEAD OF") {
        "INSTEAD OF"
    } else if header.contains("BEFORE") {
        "BEFORE"
    } else {
        // AFTER is SQLite's default when the keyword is omitted.
        "AFTER"
    };
    let events = ["DELETE", "INSERT", "UPDATE"]
        .iter()
        .filter(|e| header.contains(*e))
        .map(|e| e.to_string())
        .collect();
    (timing.to_string(), events)
}

fn opt_str_field(row: &Map<String, Value>, key: &str) -> Option<String> {
    match row.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        // pragma defaults can surface as numbers; report them as written.
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_columns_are_extracted_from_plain_defs() {
        assert_eq!(
            index_columns_from_def(
                "CREATE INDEX idx_users_mood ON public.users USING btree (mood)"
            ),
            vec!["mood"]
        );
        assert_eq!(
            index_columns_from_def(
                "CREATE UNIQUE INDEX u ON t USING btree (a, b DESC, c NULLS LAST)"
            ),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn expression_indexes_yield_no_columns() {
        assert_eq!(
            index_columns_from_def("CREATE INDEX i ON t USING btree (lower(email))"),
            Vec::<String>::new()
        );
        assert_eq!(index_columns_from_def("no parens"), Vec::<String>::new());
    }

    #[test]
    fn sqlite_trigger_header_is_parsed() {
        let (timing, events) = parse_sqlite_trigger(
            "CREATE TRIGGER audit AFTER INSERT ON users BEGIN INSERT INTO log VALUES (1); END",
        );
        assert_eq!(timing, "AFTER");
        assert_eq!(events, vec!["INSERT"]);

        let (timing, events) =
            parse_sqlite_trigger("CREATE TRIGGER t BEFORE UPDATE OR DELETE ON x BEGIN END");
        assert_eq!(timing, "BEFORE");
        assert_eq!(events, vec!["DELETE", "UPDATE"]);

        // Body keywords (that INSERT above) must not leak: header stops at ON.
        let (_, events) = parse_sqlite_trigger(
            "CREATE TRIGGER t AFTER UPDATE ON x BEGIN INSERT INTO y VALUES (1); DELETE FROM z; END",
        );
        assert_eq!(events, vec!["UPDATE"]);
    }
}
