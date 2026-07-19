//! miiigrate — forward-only SQL migrations for iii, driven entirely through
//! the `database` worker. This crate opens no database connection of its own:
//! every read goes through `database::query`, every write through
//! `database::execute`, and every migration through one atomic
//! `database::transaction` batch.
//!
//! The worker registers under the name `miiigrate`; the functions it exposes
//! live under the `migrate::` namespace (`migrate::up`, `migrate::status`,
//! `migrate::create`, `migrate::codegen`).

pub mod config;
pub mod configuration;
pub mod db;
pub mod error;
pub mod handlers;
pub mod migrations;
pub mod splitter;

/// Worker name — also the registry package, binary, and configuration id.
pub fn worker_name() -> &'static str {
    "miiigrate"
}

/// Tracking table name, shared by every handler.
pub const TRACKING_TABLE: &str = "_iii_migrations";
