mod api;
mod booker;
mod config;
mod state;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::api::{Credentials, Playtomic};
use crate::booker::Booker;
use crate::config::Config;

/// Books free (member) padel courts on Playtomic as soon as they open up.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Poll and book according to the config.
    Run {
        /// Poll once and exit (for cron).
        #[arg(long)]
        once: bool,
        /// Never confirm a booking, whatever the config says.
        #[arg(long)]
        dry_run: bool,
    },
    /// Search clubs by name to find a tenant_id.
    Search { name: String },
    /// List the courts of the configured club.
    Courts,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "padel_autobook=info".into()))
        .init();
    let cli = Cli::parse();

    match cli.command {
        Command::Search { name } => {
            let mut api = Playtomic::new()?;
            api.ensure_login(&Credentials::from_env()?).await?;
            for t in api.search_tenants(&name).await? {
                let city = t.address.and_then(|a| a.city).unwrap_or_default();
                println!("{}  {}  {}", t.tenant_id, t.tenant_name, city);
            }
        }
        Command::Courts => {
            let cfg = Config::load(&cli.config)?;
            let mut api = Playtomic::new()?;
            api.ensure_login(&Credentials::from_env()?).await?;
            let tenant = api.tenant(&cfg.tenant_id).await?;
            println!("{}", tenant.tenant_name);
            for r in tenant.resources {
                println!("{}  {}  {}", r.resource_id, r.name, r.sport_id.unwrap_or_default());
            }
        }
        Command::Run { once, dry_run } => {
            let mut cfg = Config::load(&cli.config)?;
            cfg.dry_run |= dry_run;
            let interval = Duration::from_secs(cfg.poll_interval_minutes * 60);
            let mut booker = Booker::new(cfg, Credentials::from_env()?).await?;
            if once {
                return booker.run_once().await;
            }
            loop {
                if let Err(e) = booker.run_once().await {
                    error!("poll failed: {e:#}");
                }
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = tokio::signal::ctrl_c() => {
                        info!("shutting down");
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}
