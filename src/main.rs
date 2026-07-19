use anyhow::{Context, Result};
use clap::Parser;
use iii_helpers::observability::OtelConfig;
use iii_sdk::{register_worker, InitOptions, RegisterFunction};
use miiigrate::config::WorkerConfig;
use miiigrate::configuration;
use miiigrate::handlers::{status, AppState};

#[derive(Parser, Debug)]
#[command(
    name = "miiigrate",
    about = "Forward-only SQL migrations for iii, driven through the database worker"
)]
struct Cli {
    /// Optional seed config.yaml used to populate `initial_value` on first register
    #[arg(long)]
    config: Option<String>,

    /// WebSocket URL of the iii engine
    #[arg(long, default_value = "ws://127.0.0.1:49134")]
    url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    tracing::info!(
        name = miiigrate::worker_name(),
        seed_config = cli.config.as_deref().unwrap_or("(none)"),
        url = %redact_url(&cli.url),
        "starting"
    );

    let iii = register_worker(
        &cli.url,
        InitOptions {
            otel: Some(OtelConfig::default()),
            ..Default::default()
        },
    );

    let seed = match &cli.config {
        Some(path) => match WorkerConfig::from_file(path) {
            Ok(cfg) => {
                tracing::info!(path = %path, "loaded seed config for initial registration");
                Some(cfg)
            }
            Err(e) => {
                tracing::warn!(
                    path = %path,
                    error = %e,
                    "failed to load seed config; relying on existing configuration entry"
                );
                None
            }
        },
        None => None,
    };

    configuration::register_config(&iii, seed.as_ref())
        .await
        .map_err(anyhow::Error::msg)
        .context("registering miiigrate configuration schema")?;

    let cfg = configuration::fetch_config(&iii)
        .await
        .map_err(anyhow::Error::msg)
        .context("loading miiigrate configuration")?;

    tracing::info!(
        db = %cfg.db,
        dir = %cfg.dir,
        auto = cfg.auto,
        "configuration loaded"
    );
    if cfg.auto {
        // Wired to migrate::up once it exists (next milestone).
        tracing::warn!("config `auto: true` is not honored yet — migrate::up is not implemented");
    }

    let state = AppState::new(iii.clone(), cfg);

    {
        let st = state.clone();
        iii.register_function(
            "migrate::status",
            RegisterFunction::new_async(move |req: status::StatusReq| {
                let st = st.clone();
                async move {
                    status::handle(&st, req)
                        .await
                        .map_err(iii_sdk::errors::Error::from)
                }
            })
            .description("Report applied, pending, and checksum-mismatched migrations. Read-only."),
        );
    }

    tracing::info!("miiigrate worker registered 1 function, waiting for invocations");
    wait_for_shutdown_signal().await?;
    tracing::info!("miiigrate worker shutting down");
    iii.shutdown_async().await;
    Ok(())
}

/// Strip userinfo (username:password) from a URL before logging it.
fn redact_url(s: &str) -> String {
    match url::Url::parse(s) {
        Ok(mut u) => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.to_string()
        }
        Err(_) => s.to_string(),
    }
}

/// Wait for SIGINT or, on Unix, SIGTERM — so `docker stop` / k8s SIGTERM also
/// reach `iii.shutdown_async()` instead of leaving the engine connection
/// dangling.
async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => r,
            _ = sigterm.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}
