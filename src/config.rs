//! Configuration for the miiigrate worker.
//!
//! Runtime config is stored in the `configuration` worker under id
//! `miiigrate`. An optional YAML seed file (`--config`, delivered by the
//! engine from the inline `config:` block) overrides `initial_value` on first
//! register. There is no hot-reload: a running migration must not change
//! target database or directory mid-flight; restart the worker to pick up
//! changes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::MigrateError;

/// SQL dialect of the target database. v1 supports postgres and sqlite;
/// mysql is detected but rejected with `UNSUPPORTED_DIALECT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    Postgres,
    Sqlite,
}

impl Dialect {
    /// Map the `driver` string reported by `database::listDatabases`
    /// ("postgres" | "mysql" | "sqlite").
    pub fn from_driver(driver: &str) -> Result<Self, MigrateError> {
        match driver {
            "postgres" => Ok(Dialect::Postgres),
            "sqlite" => Ok(Dialect::Sqlite),
            other => Err(MigrateError::UnsupportedDialect {
                dialect: other.to_string(),
            }),
        }
    }
}

impl std::fmt::Display for Dialect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Dialect::Postgres => write!(f, "postgres"),
            Dialect::Sqlite => write!(f, "sqlite"),
        }
    }
}

/// Top-level worker config registered with the `configuration` worker.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct WorkerConfig {
    /// Logical database name in the `database` worker's configuration
    /// (e.g. `primary`). Every SQL operation is routed to this entry.
    #[serde(default = "default_db")]
    pub db: String,

    /// Directory containing the migration files (`YYYYMMDDHHMMSS_slug.sql`),
    /// resolved against the worker process working directory.
    #[serde(default = "default_dir")]
    pub dir: String,

    /// When true, run `migrate::up` once at worker startup.
    #[serde(default)]
    pub auto: bool,

    /// Output path for `migrate::codegen` TypeScript types. When omitted,
    /// `migrate::codegen` requires an explicit `out` in its payload.
    #[serde(default)]
    pub types_out: Option<String>,

    /// SQL dialect override. Default: auto-detect via `database::listDatabases`.
    #[serde(default)]
    pub dialect: Option<Dialect>,

    /// Run `migrate::codegen` automatically after a `migrate::up` that
    /// applied at least one migration. Default: enabled when `types_out` is
    /// set, disabled otherwise. Codegen failures never fail the up run.
    #[serde(default)]
    pub codegen_on_up: Option<bool>,
}

fn default_db() -> String {
    "primary".to_string()
}

fn default_dir() -> String {
    "./migrations".to_string()
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            db: default_db(),
            dir: default_dir(),
            auto: false,
            types_out: None,
            dialect: None,
            codegen_on_up: None,
        }
    }
}

impl WorkerConfig {
    /// Effective `codegen_on_up`: explicit value wins, otherwise types are
    /// regenerated whenever a `types_out` destination is configured.
    pub fn codegen_on_up_enabled(&self) -> bool {
        self.codegen_on_up.unwrap_or(self.types_out.is_some())
    }

    pub fn from_yaml(yaml: &str) -> Result<Self, String> {
        serde_yml::from_str(yaml).map_err(|e| format!("yaml parse: {e}"))
    }

    pub fn from_json(value: &Value) -> Result<Self, String> {
        serde_json::from_value(value.clone()).map_err(|e| format!("json parse: {e}"))
    }

    pub fn from_file(path: &str) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
        Self::from_yaml(&raw)
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("WorkerConfig serializes")
    }

    pub fn json_schema() -> Value {
        let root = schemars::schema_for!(WorkerConfig);
        let mut schema =
            serde_json::to_value(&root.schema).expect("WorkerConfig JSON Schema serializes");
        if let Some(obj) = schema.as_object_mut() {
            if !root.definitions.is_empty() {
                obj.insert(
                    "definitions".into(),
                    serde_json::to_value(&root.definitions).expect("definitions serialize"),
                );
            }
            obj.insert(
                "example".into(),
                json!({
                    "db": "primary",
                    "dir": "./migrations",
                    "auto": false,
                    "types_out": "./db.types.ts",
                }),
            );
        }
        schema
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_documented_values() {
        let cfg = WorkerConfig::default();
        assert_eq!(cfg.db, "primary");
        assert_eq!(cfg.dir, "./migrations");
        assert!(!cfg.auto);
        assert!(cfg.types_out.is_none());
        assert!(cfg.dialect.is_none());
        assert!(cfg.codegen_on_up.is_none());
    }

    #[test]
    fn codegen_on_up_defaults_follow_types_out() {
        let mut cfg = WorkerConfig::default();
        assert!(!cfg.codegen_on_up_enabled()); // no types_out, no codegen

        cfg.types_out = Some("./db.types.ts".into());
        assert!(cfg.codegen_on_up_enabled()); // types_out set: on by default

        cfg.codegen_on_up = Some(false); // explicit opt-out wins
        assert!(!cfg.codegen_on_up_enabled());

        cfg.types_out = None;
        cfg.codegen_on_up = Some(true); // explicit opt-in without types_out:
        assert!(cfg.codegen_on_up_enabled()); // codegen will fail loudly (no out)
    }

    #[test]
    fn parses_full_yaml_seed() {
        let cfg = WorkerConfig::from_yaml(
            "db: analytics\ndir: ./db/migrations\nauto: true\ntypes_out: ./db.types.ts\ndialect: postgres\n",
        )
        .unwrap();
        assert_eq!(cfg.db, "analytics");
        assert_eq!(cfg.dir, "./db/migrations");
        assert!(cfg.auto);
        assert_eq!(cfg.types_out.as_deref(), Some("./db.types.ts"));
        assert_eq!(cfg.dialect, Some(Dialect::Postgres));
    }

    #[test]
    fn empty_yaml_yields_defaults() {
        let cfg = WorkerConfig::from_yaml("{}").unwrap();
        assert_eq!(cfg.db, "primary");
    }

    #[test]
    fn dialect_from_driver_rejects_mysql() {
        assert!(matches!(
            Dialect::from_driver("mysql"),
            Err(MigrateError::UnsupportedDialect { .. })
        ));
        assert_eq!(Dialect::from_driver("postgres").unwrap(), Dialect::Postgres);
        assert_eq!(Dialect::from_driver("sqlite").unwrap(), Dialect::Sqlite);
    }
}
