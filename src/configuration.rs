//! Integration with the `configuration` worker — register and fetch the
//! `miiigrate` configuration entry.
//!
//! Same lifecycle as the `database` worker: `configuration::register` with a
//! JSON Schema (+ `initial_value` from the `--config` seed when present),
//! then `configuration::get` for the live, env-expanded value. Unlike the
//! `database` worker there is no config-change trigger: migrations must not
//! switch database or directory mid-run, so config is read once at startup.

use iii_sdk::protocol::TriggerRequest;
use iii_sdk::IIIClient;
use serde_json::{json, Value};
use std::time::Duration;

use crate::config::WorkerConfig;

pub const CONFIG_ID: &str = "miiigrate";
const CONFIG_TIMEOUT_MS: u64 = 5_000;
/// Total window during which transient failures (engine still starting:
/// timeout, not connected, function not registered yet) are retried before
/// giving up. Aligned with the auto-run retry budget in `main.rs`.
const CONFIG_RETRY_BUDGET: Duration = Duration::from_secs(120);
/// Backoff ceiling between two attempts.
const CONFIG_RETRY_MAX_DELAY: Duration = Duration::from_secs(5);

/// Register the `miiigrate` configuration schema with the configuration
/// worker. When `seed` is present, its value is installed as `initial_value`.
/// Otherwise the built-in default is seeded only when no stored value exists.
pub async fn register_config(iii: &IIIClient, seed: Option<&WorkerConfig>) -> Result<(), String> {
    let mut payload = json!({
        "id": CONFIG_ID,
        "name": "Miiigrate",
        "description": "Forward-only SQL migrations driven through the database worker.",
        "schema": WorkerConfig::json_schema(),
    });
    if let Some(seed) = seed {
        payload["initial_value"] = seed.to_json();
    } else if should_seed_default_value(iii).await? {
        payload["initial_value"] = WorkerConfig::default().to_json();
    }
    trigger_with_retry(iii, "configuration::register", payload).await?;
    Ok(())
}

/// Read the live `miiigrate` configuration (env-expanded by the configuration worker).
pub async fn fetch_config(iii: &IIIClient) -> Result<WorkerConfig, String> {
    let value = get_config_value(iii).await?;
    if value.is_null() {
        tracing::info!("no configuration value found; using built-in defaults");
        return Ok(WorkerConfig::default());
    }
    WorkerConfig::from_json(&value)
}

async fn should_seed_default_value(iii: &IIIClient) -> Result<bool, String> {
    match try_get_config_value(iii).await? {
        None => Ok(true),
        Some(value) if value.is_null() => Ok(true),
        Some(_) => Ok(false),
    }
}

async fn get_config_value(iii: &IIIClient) -> Result<Value, String> {
    try_get_config_value(iii)
        .await?
        .ok_or_else(|| format!("configuration `{CONFIG_ID}` not found"))
}

/// Returns `Ok(None)` when the entry does not exist (`NOT_FOUND`).
async fn try_get_config_value(iii: &IIIClient) -> Result<Option<Value>, String> {
    match trigger_with_retry(iii, "configuration::get", json!({ "id": CONFIG_ID })).await {
        Ok(resp) => Ok(resp.get("value").cloned()),
        Err(e) if e.contains("NOT_FOUND") => Ok(None),
        Err(e) => Err(e),
    }
}

/// True for failures that resolve on their own once the engine and its
/// built-in workers finish starting: invocation timeout, socket not yet
/// connected, or the target function not registered yet. Anything else (a
/// real error from the configuration worker, e.g. schema rejection) will not
/// improve with time and must fail fast.
fn is_transient(e: &iii_sdk::errors::Error) -> bool {
    use iii_sdk::errors::Error;
    match e {
        Error::Timeout | Error::NotConnected => true,
        Error::Remote { code, .. } => code == "function_not_found",
        _ => false,
    }
}

async fn trigger_with_retry(
    iii: &IIIClient,
    function_id: &str,
    payload: Value,
) -> Result<Value, String> {
    let started = tokio::time::Instant::now();
    let mut delay = Duration::from_millis(250);
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match iii
            .trigger(TriggerRequest {
                function_id: function_id.to_string(),
                payload: payload.clone(),
                action: None,
                timeout_ms: Some(CONFIG_TIMEOUT_MS),
            })
            .await
        {
            Ok(v) => return Ok(v),
            Err(e) if is_transient(&e) && started.elapsed() + delay <= CONFIG_RETRY_BUDGET => {
                tracing::warn!(
                    function_id,
                    attempt,
                    error = %e,
                    retry_in_ms = delay.as_millis() as u64,
                    "engine not ready for configuration RPC; retrying"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(CONFIG_RETRY_MAX_DELAY);
            }
            Err(e) => {
                return Err(format!("{function_id} failed after {attempt} attempts: {e}"));
            }
        }
    }
}
