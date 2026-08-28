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
    /// Run the HTTP server and collect on an interval.
    Run,
    /// Collect once and exit.
    Collect,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let cli = Cli::parse();
    let config = kartero::config::Config::from_env()?;
    match cli.command {
        Command::Run => kartero::http::serve(config).await,
        Command::Collect => kartero::collect::collect_once(&config).await,
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
