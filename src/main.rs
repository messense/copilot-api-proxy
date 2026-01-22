//! copilot-api-proxy - A reverse proxy server for GitHub Copilot API

use anyhow::Result;
use clap::{Parser, Subcommand};
use copilot_api_proxy::{auth, config, server};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[command(name = "copilot-api-proxy")]
#[command(about = "A reverse proxy server for GitHub Copilot API")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run OAuth authentication flow
    Auth,
    /// Start the proxy server
    Server {
        #[arg(short, long, default_value = "9876")]
        port: u16,
        #[arg(long, default_value = "info")]
        log_level: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    match cli.command {
        Commands::Auth => run_auth().await,
        Commands::Server { port, log_level } => run_server(port, &log_level).await,
    }
}

async fn run_auth() -> Result<()> {
    init_tracing("copilot_api_proxy=debug");

    config::ensure_token_dir()?;

    let github_token = auth::run_device_flow().await?;

    let token_path = config::token_path();
    config::write_token(&token_path, &github_token)?;

    println!("\nAuthentication successful!");
    println!("Token saved to: {}\n", token_path.display());

    Ok(())
}

async fn run_server(port: u16, log_level: &str) -> Result<()> {
    let filter = format!("copilot_api_proxy={},tower_http={}", log_level, log_level);
    init_tracing(&filter);

    let state = server::AppState::new().await?;
    let app = server::create_router(state);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    tracing::info!("Server listening on http://0.0.0.0:{}", port);

    axum::serve(listener, app)
        .with_graceful_shutdown(async { tokio::signal::ctrl_c().await.ok(); })
        .await?;

    Ok(())
}

fn init_tracing(default_filter: &str) {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
}
