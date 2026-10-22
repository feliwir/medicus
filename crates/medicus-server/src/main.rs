//! `medicus` — the core server binary.

mod addons;
mod archive;
mod config;
mod import;
mod ingest;

use std::path::PathBuf;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::config::{Config, LogFormat};

#[derive(Debug, Parser)]
#[command(version, about = "medicus — a lightweight, modular PACS")]
struct Cli {
    /// Configuration file (TOML). Values can be overridden with MEDICUS_* environment variables.
    #[arg(short, long, env = "MEDICUS_CONFIG", global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the server (default).
    Run,
    /// Validate the configuration and print the effective result as JSON.
    CheckConfig,
    /// Import DICOM Part 10 files from a file or directory tree and print a summary as JSON.
    Import {
        path: PathBuf,
        /// Files ingested concurrently.
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
    },
}

fn init_tracing(cfg: &config::LogConfig) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.filter));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match cfg.format {
        LogFormat::Pretty => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

/// Loads the addons and wires the archive.
async fn start(cfg: &Config) -> anyhow::Result<archive::Archive> {
    let caps = addons::load(cfg, &tokio::runtime::Handle::current())?;
    archive::Archive::new(&caps, &cfg.archive).await
}

async fn run(cfg: Config) -> anyhow::Result<()> {
    tracing::info!(
        name = %cfg.server.name,
        data_dir = %cfg.server.data_dir.display(),
        http = %cfg.http.listen,
        version = env!("CARGO_PKG_VERSION"),
        "medicus starting"
    );
    let archive = start(&cfg).await?;
    tracing::info!(?archive, "archive ready");
    shutdown_signal().await;
    tracing::info!("shutting down");
    Ok(())
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting async runtime")
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(cli.config.as_deref()).context("loading configuration")?;
    match cli.command.unwrap_or(Command::Run) {
        Command::CheckConfig => {
            let out = serde_json::to_string_pretty(&cfg)?;
            #[allow(clippy::print_stdout)] // the command's output
            {
                println!("{out}");
            }
            Ok(())
        }
        Command::Run => {
            init_tracing(&cfg.log);
            runtime()?.block_on(run(cfg))
        }
        Command::Import { path, concurrency } => {
            init_tracing(&cfg.log);
            let report = runtime()?.block_on(async {
                let archive = start(&cfg).await?;
                import::import(&archive, &path, concurrency).await
            })?;
            let out = serde_json::to_string_pretty(&report)?;
            #[allow(clippy::print_stdout)] // the command's output
            {
                println!("{out}");
            }
            Ok(())
        }
    }
}
