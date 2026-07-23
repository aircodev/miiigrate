//! Fixture tests for `migrate::schema`'s pure introspection pipeline —
//! JSON rows shaped like `database::query` output, no database involved.

use serde_json::{Map, Value};

use miiigrate::schema::{
    pg_schema_from_rows, sqlite_schema_from_rows, SqliteIndexRaw, SqliteTableRaw,
};

fn rows(json: &str) -> Vec<Map<String, Value>> {
    serde_json::from_str(json).expect("fixture parses")
}

#[test]
fn pg_fixture_produces_expected_report() {
    let report = pg_schema_from_rows(
        &rows(include_str!("fixtures/pg_schema_columns.json")),
        &rows(include_str!("fixtures/pg_schema_pks.json")),
        &rows(include_str!("fixtures/pg_schema_fks.json")),
        &rows(include_str!("fixtures/pg_schema_indexes.json")),
        &rows(include_str!("fixtures/pg_schema_triggers.json")),
        &rows(include_str!("fixtures/pg_enums.json")),
    )
    .unwrap();

    // The tracking table is excluded everywhere, even though the fixtures
    // carry its columns, primary key, and backing index.
    let names: Vec<_> = report.tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["events", "reservations"]);

    let events = &report.tables[0];
    assert_eq!(
        events
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "title", "created_at"]
    );
    assert_eq!(events.columns[0].position, 1);
    assert_eq!(
        events.columns[0].default.as_deref(),
        Some("nextval('events_id_seq'::regclass)")
    );
    assert_eq!(events.columns[2].default.as_deref(), Some("now()"));
    assert_eq!(events.primary_key, ["id"]);
    // One trigger aggregated from two event rows.
    assert_eq!(events.triggers.len(), 1);
    assert_eq!(events.triggers[0].timing, "AFTER");
    assert_eq!(events.triggers[0].events, ["INSERT", "UPDATE"]);

    let reservations = &report.tables[1];
    // ordinal_position arrived as strings (int8 wire format) — tolerated.
    assert_eq!(reservations.columns[0].position, 1);
    // Composite PK in key order, not table order.
    assert_eq!(reservations.primary_key, ["user_id", "event_id"]);
    // Multi-column FK paired column-by-column.
    assert_eq!(reservations.foreign_keys.len(), 1);
    let fk = &reservations.foreign_keys[0];
    assert_eq!(fk.columns, ["event_id", "user_id"]);
    assert_eq!(fk.references_table, "events");
    assert_eq!(fk.references_columns, ["id", "owner_id"]);
    assert_eq!(fk.on_delete, "CASCADE");
    // Partial index keeps its columns; expression index yields none.
    let partial = reservations
        .indexes
        .iter()
        .find(|i| i.name == "idx_reservations_active")
        .unwrap();
    assert_eq!(partial.columns, ["event_id"]);
    assert!(!partial.unique);
    let expr = reservations
        .indexes
        .iter()
        .find(|i| i.name == "idx_reservations_lower")
        .unwrap();
    assert!(expr.columns.is_empty());
    assert!(expr
        .definition
        .as_deref()
        .unwrap()
        .contains("lower(status)"));

    // Enums ride along unchanged (shared fixture with codegen).
    assert!(report.enums.iter().any(|e| e.name == "mood"));
}

#[test]
fn sqlite_fixture_produces_expected_report() {
    let all: Map<String, Value> =
        serde_json::from_str(include_str!("fixtures/sqlite_schema.json")).unwrap();
    let tables: Vec<SqliteTableRaw> = all
        .iter()
        .map(|(name, spec)| SqliteTableRaw {
            name: name.clone(),
            columns: spec["columns"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_object().unwrap().clone())
                .collect(),
            foreign_keys: spec["foreign_keys"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_object().unwrap().clone())
                .collect(),
            indexes: spec["indexes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|idx| SqliteIndexRaw {
                    list_row: idx["list_row"].as_object().unwrap().clone(),
                    columns: idx["columns"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_object().unwrap().clone())
                        .collect(),
                    sql: idx["sql"].as_str().map(String::from),
                })
                .collect(),
            triggers: spec["triggers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_object().unwrap().clone())
                .collect(),
        })
        .collect();

    let report = sqlite_schema_from_rows(&tables).unwrap();

    // Tracking table excluded; no enums on SQLite.
    assert_eq!(report.tables.len(), 1);
    assert!(report.enums.is_empty());

    let orders = &report.tables[0];
    assert_eq!(orders.name, "orders");
    // Positions are cid + 1, columns in DDL order.
    assert_eq!(
        orders
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.position))
            .collect::<Vec<_>>(),
        [("tenant", 1), ("id", 2), ("qty", 3), ("user_ref", 4)]
    );
    // pk members ordered by pk ordinal (id=1, tenant=2), not table order.
    assert_eq!(orders.primary_key, ["id", "tenant"]);
    // PK column reports nullable=false even with notnull=0 (rowid alias).
    assert!(!orders.columns[1].nullable);
    assert_eq!(orders.columns[2].default.as_deref(), Some("1"));

    // FK targeting the implicit PK: `to` is NULL → no referenced columns.
    assert_eq!(orders.foreign_keys.len(), 1);
    assert_eq!(orders.foreign_keys[0].columns, ["user_ref"]);
    assert!(orders.foreign_keys[0].references_columns.is_empty());
    assert_eq!(orders.foreign_keys[0].on_delete, "CASCADE");
    assert!(orders.foreign_keys[0].name.is_none());

    // Auto-index has no DDL; the explicit one keeps its sql.
    let auto = orders
        .indexes
        .iter()
        .find(|i| i.name.starts_with("sqlite_autoindex"))
        .unwrap();
    assert!(auto.unique && auto.definition.is_none());
    assert_eq!(auto.columns, ["id", "tenant"]);
    let explicit = orders
        .indexes
        .iter()
        .find(|i| i.name == "idx_orders_qty")
        .unwrap();
    assert_eq!(explicit.columns, ["qty"]);
    assert!(explicit.definition.is_some());

    assert_eq!(orders.triggers.len(), 1);
    assert_eq!(orders.triggers[0].timing, "AFTER");
    assert_eq!(orders.triggers[0].events, ["INSERT"]);
}
