pub mod agy;
pub mod common;
pub mod init;
pub mod status;
pub mod waybar;
pub mod webserver;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "cut-rs", about = "Local Antigravity AI quota tracker & cost estimator")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Auto-detect Antigravity CLI & Waybar and configure cut-rs hooks safely
    Init(init::InitArgs),
    /// Check hook configuration, local SQLite DB, webserver, and current quota
    Status(status::StatusArgs),
    /// Process Antigravity CLI statusLine JSON from stdin and record quota/token intervals locally
    AgyCli(agy::AgyCliArgs),
    /// Output Waybar custom module JSON from local Antigravity quota state
    Waybar(waybar::WaybarArgs),
    /// Manage the on-demand local web dashboard server (start, stop, status)
    Webserver(webserver::WebserverArgs),
}
