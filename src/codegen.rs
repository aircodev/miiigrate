//! Schema introspection IR and TypeScript emission for `migrate::codegen`.
//!
//! The pipeline is: `database::query` on `information_schema` / `pg_catalog`
//! (Postgres) or `pragma_table_info` (SQLite) → [`SchemaIr`] → one
//! TypeScript interface per table plus an aggregate `Database` type.
//!
//! Type mapping starts from kysely-codegen's conventions but describes what
//! **actually crosses the wire**: every read goes through `database::query`,
//! i.e. JSON — so types a JSON payload cannot carry (`Date`, `Buffer`) never
//! reach the consumer and are not emitted:
//!
//! - `numeric`/`decimal`/`int8` → `string` (loss-free)
//! - `timestamp`/`timestamptz`/`date` → `string` (the database worker
//!   serializes timestamps as RFC 3339 strings)
//! - `bytea`/`blob` → `string` (the database worker serializes bytes as
//!   base64)
//! - `json`/`jsonb` → `Json` (emitted alias — real JSON on the wire)
//! - Postgres enums → union-of-labels type aliases
//! - arrays → `T[]`
//! - nullable columns → `| null`
//!
//! Everything in this module is pure — introspection rows in, TypeScript
//! out — so it is unit-tested from JSON fixtures without a database.

use serde_json::{Map, Value};

use crate::TRACKING_TABLE;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaIr {
    pub tables: Vec<TableIr>,
    pub enums: Vec<EnumIr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableIr {
    pub name: String,
    pub columns: Vec<ColumnIr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnIr {
    pub name: String,
    /// TypeScript type, without the `| null` suffix.
    pub ts_type: String,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumIr {
    /// Postgres type name, e.g. `mood`.
    pub name: String,
    pub labels: Vec<String>,
}

// ---------------------------------------------------------------------------
// Postgres rows → IR
// ---------------------------------------------------------------------------

/// SQL for the column listing (base tables of the current schema).
pub const PG_COLUMNS_SQL: &str = "\
SELECT c.table_name, c.column_name, c.data_type, c.udt_name, c.is_nullable \
FROM information_schema.columns c \
JOIN information_schema.tables t \
  ON t.table_schema = c.table_schema AND t.table_name = c.table_name \
WHERE c.table_schema = current_schema() AND t.table_type = 'BASE TABLE' \
ORDER BY c.table_name, c.ordinal_position";

/// SQL for enum labels of the current schema, in declared order.
pub const PG_ENUMS_SQL: &str = "\
SELECT t.typname AS enum_name, e.enumlabel AS label \
FROM pg_type t \
JOIN pg_enum e ON e.enumtypid = t.oid \
JOIN pg_namespace n ON n.oid = t.typnamespace \
WHERE n.nspname = current_schema() \
ORDER BY t.typname, e.enumsortorder";

/// Build the IR from `database::query` rows of [`PG_COLUMNS_SQL`] and
/// [`PG_ENUMS_SQL`]. The tracking table is excluded.
pub fn pg_ir_from_rows(
    column_rows: &[Map<String, Value>],
    enum_rows: &[Map<String, Value>],
) -> Result<SchemaIr, String> {
    let mut enums: Vec<EnumIr> = Vec::new();
    for row in enum_rows {
        let name = str_field(row, "enum_name")?;
        let label = str_field(row, "label")?;
        match enums.last_mut() {
            Some(e) if e.name == name => e.labels.push(label),
            _ => enums.push(EnumIr {
                name,
                labels: vec![label],
            }),
        }
    }

    let mut tables: Vec<TableIr> = Vec::new();
    for row in column_rows {
        let table = str_field(row, "table_name")?;
        if table == TRACKING_TABLE {
            continue;
        }
        let column = str_field(row, "column_name")?;
        let data_type = str_field(row, "data_type")?;
        let udt_name = str_field(row, "udt_name")?;
        let nullable = str_field(row, "is_nullable")? == "YES";
        let ts_type = pg_type_to_ts(&data_type, &udt_name, &enums);

        match tables.last_mut() {
            Some(t) if t.name == table => t.columns.push(ColumnIr {
                name: column,
                ts_type,
                nullable,
            }),
            _ => tables.push(TableIr {
                name: table,
                columns: vec![ColumnIr {
                    name: column,
                    ts_type,
                    nullable,
                }],
            }),
        }
    }

    // Only emit enums actually referenced by a kept column? kysely-codegen
    // emits every enum of the schema; do the same (they are cheap and a
    // migration may add the column later).
    Ok(SchemaIr { tables, enums })
}

/// Map a Postgres column type to TypeScript.
pub fn pg_type_to_ts(data_type: &str, udt_name: &str, enums: &[EnumIr]) -> String {
    // Arrays: information_schema says data_type = 'ARRAY' and udt_name is
    // the element type with a leading underscore.
    if data_type == "ARRAY" {
        let elem = udt_name.strip_prefix('_').unwrap_or(udt_name);
        return format!("{}[]", pg_udt_to_ts(elem, enums));
    }
    if data_type == "USER-DEFINED" {
        return pg_udt_to_ts(udt_name, enums);
    }
    pg_udt_to_ts(udt_name, enums)
}

fn pg_udt_to_ts(udt: &str, enums: &[EnumIr]) -> String {
    match udt {
        "int2" | "int4" | "float4" | "float8" | "oid" => "number".into(),
        "int8" | "numeric" | "money" => "string".into(),
        "bool" => "boolean".into(),
        "text" | "varchar" | "bpchar" | "char" | "name" | "uuid" | "citext" | "inet" | "cidr"
        | "macaddr" | "interval" | "time" | "timetz" | "bit" | "varbit" | "xml" | "tsvector"
        | "tsquery" | "point" => "string".into(),
        // RFC 3339 strings on the wire (database worker's value encoder).
        "date" | "timestamp" | "timestamptz" => "string".into(),
        "json" | "jsonb" => "Json".into(),
        // base64 string on the wire.
        "bytea" => "string".into(),
        other => {
            if enums.iter().any(|e| e.name == other) {
                pascal_case(other)
            } else {
                "unknown".into()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SQLite rows → IR
// ---------------------------------------------------------------------------

/// SQL listing user tables (excluding sqlite internals and the tracking table).
pub const SQLITE_TABLES_SQL: &str = "\
SELECT name FROM sqlite_master \
WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
ORDER BY name";

/// SQL for one table's columns; bind the table name as the only parameter.
pub const SQLITE_COLUMNS_SQL: &str =
    "SELECT name, type, \"notnull\", pk FROM pragma_table_info(?) ORDER BY cid";

/// Build the IR from per-table `pragma_table_info` rows.
pub fn sqlite_ir_from_rows(
    tables: &[(String, Vec<Map<String, Value>>)],
) -> Result<SchemaIr, String> {
    let mut out = Vec::new();
    for (table, rows) in tables {
        if table == TRACKING_TABLE {
            continue;
        }
        let mut columns = Vec::new();
        for row in rows {
            let name = str_field(row, "name")?;
            let decl = str_field(row, "type").unwrap_or_default();
            let notnull = int_field(row, "notnull").unwrap_or(0) != 0;
            let pk = int_field(row, "pk").unwrap_or(0) != 0;
            columns.push(ColumnIr {
                name,
                ts_type: sqlite_type_to_ts(&decl),
                // A PRIMARY KEY column is never null in practice even though
                // pragma_table_info reports notnull=0 for `INTEGER PRIMARY
                // KEY` (rowid alias).
                nullable: !notnull && !pk,
            });
        }
        out.push(TableIr {
            name: table.clone(),
            columns,
        });
    }
    Ok(SchemaIr {
        tables: out,
        enums: Vec::new(),
    })
}

/// Map a SQLite declared type to TypeScript using SQLite's affinity rules.
pub fn sqlite_type_to_ts(decl: &str) -> String {
    let d = decl.to_ascii_uppercase();
    if d.contains("INT") {
        "number".into()
    } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
        "string".into()
    } else if d.contains("BLOB") || d.is_empty() {
        // base64 string over database::query, like Postgres bytea.
        "string".into()
    } else if d.contains("REAL")
        || d.contains("FLOA")
        || d.contains("DOUB")
        || d.contains("NUMERIC")
        || d.contains("DECIMAL")
    {
        "number".into()
    } else if d.contains("BOOL") {
        "boolean".into()
    } else if d.contains("DATE") || d.contains("TIME") {
        "string".into()
    } else {
        "unknown".into()
    }
}

// ---------------------------------------------------------------------------
// IR → TypeScript
// ---------------------------------------------------------------------------

/// Emit the TypeScript module for a schema.
pub fn emit_typescript(ir: &SchemaIr, dialect: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "/**\n * generated by miiigrate, do not edit.\n *\n * Dialect: {dialect}. Types describe values as they arrive through\n * database::query (JSON): numeric/int8 are strings, timestamps are\n * RFC 3339 strings, binary columns are base64 strings. Nullable\n * columns are `| null`.\n */\n\n"
    ));

    let uses_json = ir
        .tables
        .iter()
        .flat_map(|t| &t.columns)
        .any(|c| c.ts_type == "Json" || c.ts_type == "Json[]");
    if uses_json {
        out.push_str(
            "export type Json = { [key: string]: Json } | Json[] | string | number | boolean | null;\n\n",
        );
    }

    for e in &ir.enums {
        let labels = e
            .labels
            .iter()
            .map(|l| format!("'{}'", l.replace('\'', "\\'")))
            .collect::<Vec<_>>()
            .join(" | ");
        out.push_str(&format!(
            "export type {} = {};\n\n",
            pascal_case(&e.name),
            labels
        ));
    }

    for t in &ir.tables {
        out.push_str(&format!("export interface {} {{\n", pascal_case(&t.name)));
        for c in &t.columns {
            let ty = if c.nullable {
                // Parenthesize multi-part types: `Date | string | null` is
                // fine, but keep unions readable as emitted by kysely-codegen.
                format!("{} | null", c.ts_type)
            } else {
                c.ts_type.clone()
            };
            out.push_str(&format!("  {}: {};\n", ts_property(&c.name), ty));
        }
        out.push_str("}\n\n");
    }

    out.push_str("export interface Database {\n");
    for t in &ir.tables {
        out.push_str(&format!(
            "  {}: {};\n",
            ts_property(&t.name),
            pascal_case(&t.name)
        ));
    }
    out.push_str("}\n");
    out
}

/// `snake_case`/`kebab-case` → `PascalCase`.
pub fn pascal_case(s: &str) -> String {
    s.split(['_', '-', ' '])
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut chars = p.chars();
            match chars.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// Quote a property name when it is not a valid TS identifier.
fn ts_property(name: &str) -> String {
    let valid = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if valid {
        name.to_string()
    } else {
        format!("'{}'", name.replace('\'', "\\'"))
    }
}

fn str_field(row: &Map<String, Value>, key: &str) -> Result<String, String> {
    row.get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| format!("introspection row missing string field `{key}`: {row:?}"))
}

fn int_field(row: &Map<String, Value>, key: &str) -> Option<i64> {
    let v = row.get(key)?;
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}
