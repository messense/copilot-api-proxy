//! copilot-api-proxy - A reverse proxy server for GitHub Copilot API

use anyhow::Result;
use clap::{Parser, Subcommand};
use copilot_api_proxy::{auth, config, server, web_backend::SearchProvider};
use service_manager::{
    RestartPolicy, ServiceInstallCtx, ServiceLabel, ServiceLevel, ServiceManager,
    ServiceUninstallCtx,
};
use std::ffi::OsString;
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
        /// Handle Amp management APIs locally instead of proxying to ampcode.com.
        /// Serves thread search, markdown export, telemetry, etc. from
        /// local ~/.local/share/amp/threads/ data.
        #[arg(long)]
        amp_local: bool,
        /// Search backend for web search and page extraction in --amp-local mode.
        /// Requires --amp-local. Some backends need env vars for API keys.
        #[arg(long, default_value = "jina", value_parser = clap::value_parser!(SearchProvider))]
        search_provider: SearchProvider,
    },
    /// Manage the system service
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install the service daemon
    Install {
        #[arg(short, long, default_value = "9876")]
        port: u16,
    },
    /// Uninstall the service daemon
    Uninstall,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    match cli.command {
        Commands::Auth => run_auth().await,
        Commands::Server {
            port,
            log_level,
            amp_local,
            search_provider,
        } => run_server(port, &log_level, amp_local, search_provider).await,
        Commands::Service { action } => match action {
            ServiceAction::Install { port } => install_service(port),
            ServiceAction::Uninstall => uninstall_service(),
        },
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

async fn run_server(port: u16, log_level: &str, amp_local: bool, search_provider: SearchProvider) -> Result<()> {
    let filter = format!("copilot_api_proxy={},tower_http={}", log_level, log_level);
    init_tracing(&filter);

    let state = server::AppState::new(amp_local, search_provider).await?;
    let app = server::create_router(state);

    if amp_local {
        tracing::info!("Amp local mode enabled — management APIs served from local thread data");
    }

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    tracing::info!("Server listening on http://0.0.0.0:{}", port);

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
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

const SERVICE_LABEL: &str = "me.messense.copilot-api-proxy";

fn install_service(port: u16) -> Result<()> {
    let label: ServiceLabel = SERVICE_LABEL.parse()?;
    let mut manager = <dyn ServiceManager>::native()?;

    let program = std::env::current_exe()?;
    let args = vec![
        OsString::from("server"),
        OsString::from("--port"),
        OsString::from(port.to_string()),
    ];

    manager.set_level(ServiceLevel::User)?;
    manager.install(ServiceInstallCtx {
        label: label.clone(),
        program,
        args,
        contents: None,
        username: None,
        working_directory: None,
        environment: None,
        autostart: true,
        restart_policy: RestartPolicy::OnFailure {
            delay_secs: Some(10),
        },
    })?;

    println!("Service installed successfully as '{}'", SERVICE_LABEL);
    println!("The service will start automatically on system boot.");
    println!("\nTo start the service now, run:");
    #[cfg(target_os = "macos")]
    println!(
        "  launchctl load ~/Library/LaunchAgents/{}.plist",
        SERVICE_LABEL
    );
    #[cfg(target_os = "linux")]
    println!("  systemctl --user start {}", SERVICE_LABEL);

    Ok(())
}

fn uninstall_service() -> Result<()> {
    let label: ServiceLabel = SERVICE_LABEL.parse()?;
    let mut manager = <dyn ServiceManager>::native()?;
    manager.set_level(ServiceLevel::User)?;

    manager.uninstall(ServiceUninstallCtx { label })?;

    println!("Service '{}' uninstalled successfully.", SERVICE_LABEL);

    Ok(())
}
