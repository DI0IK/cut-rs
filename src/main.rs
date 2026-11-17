use clap::Parser;
use cut_rs::api::build_router;
use cut_rs::cli::{
    Cli, Commands, agy::run_agy_cli, claude::run_claude_cli, codex::run_codex_cli,
    init::run_init, status::run_status, waybar::run_waybar,
};
use cut_rs::config::Config;
use cut_rs::init_app_state;
use cut_rs::worker::spawn_worker;
use std::net::SocketAddr;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init(args) => {
            return run_init(args).await;
        }
        Commands::Status(args) => {
            run_status(args).await;
            return Ok(());
        }
        Commands::AgyCli(args) => {
            run_agy_cli(args).await;
            return Ok(());
        }
        Commands::ClaudeCli(args) => {
            run_claude_cli(args).await;
            return Ok(());
        }
        Commands::CodexCli(args) => {
            run_codex_cli(args).await;
            return Ok(());
        }
        Commands::Waybar(args) => {
            run_waybar(args).await;
            return Ok(());
        }
        Commands::Serve => {}
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("cut_rs=info,tower_http=info")),
        )
        .init();

    let config = Config::from_env();
    let addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    let state = init_app_state(config).await?;

    spawn_worker(
        state.repo.clone(),
        state.cache.clone(),
        state.config.clone(),
        state.worker_notify.clone(),
    );

    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "cut-rs server listening");

    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}
