use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "kartero", version = kartero::VERSION, about = "Pull OTLP JSON from CI artifacts and deliver it")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// In-cluster process: HTTP /metrics plus collect (and archive if configured).
    Run,
    /// One collect pass, then exit. For local debug; the Deployment uses `run`.
    Collect,
    /// One archive pass, then exit. For local debug; production is Helm `archive.enabled` plus a volume.
    Archive,
    /// Load the configuration, print the sources it resolved, and exit.
    /// Contacts nothing. Answers "will this Deployment start" without
    /// starting it, which is what makes a rendered chart testable.
    ConfigCheck,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let cli = Cli::parse();
    let config = kartero::config::Config::from_env()?;
    match cli.command {
        Command::Run => kartero::http::serve(config).await,
        Command::Collect => kartero::collect::collect_once(&config).await,
        Command::Archive => kartero::archive::archive_once(&config).await,
        Command::ConfigCheck => {
            // Tokens are never printed, only whether one resolved.
            for source in &config.sources {
                println!(
                    "source {} branch={} workflows={} token={} derives={}",
                    source.slug(),
                    source.trusted_branch,
                    source.workflows.join(","),
                    if source.token.is_empty() {
                        "missing"
                    } else {
                        "present"
                    },
                    if source.actions.is_some() {
                        "yes"
                    } else {
                        "no"
                    }
                );
            }
            println!(
                "otlp={} allowlist={} ledger={} prefix={} archive={}",
                config.otlp_endpoint,
                config.allowlist_path.display(),
                config.ledger_path.display(),
                config.artifact_prefix,
                config.archive.is_some()
            );
            Ok(())
        }
    }
}

fn init_logging() {
    let filter = std::env::var("KARTERO_LOG")
        .ok()
        .and_then(|value| EnvFilter::try_new(value).ok())
        .unwrap_or_else(|| EnvFilter::new("kartero=info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .init();
}
