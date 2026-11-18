use clap::Parser;
use cut_rs::cli::{
    Cli, Commands, agy::run_agy_cli, init::run_init, status::run_status, waybar::run_waybar,
    webserver::run_webserver,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init(args) => run_init(args).await,
        Commands::Status(args) => {
            run_status(args).await;
            Ok(())
        }
        Commands::AgyCli(args) => {
            run_agy_cli(args).await;
            Ok(())
        }
        Commands::Waybar(args) => {
            run_waybar(args).await;
            Ok(())
        }
        Commands::Webserver(args) => run_webserver(args).await,
    }
}
