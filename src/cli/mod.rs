pub mod agy;
pub mod claude;
pub mod codex;
pub mod common;
pub mod init;
pub mod status;
pub mod waybar;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "cut-rs", about = "Crowdsourced AI quota telemetry server & CLI")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Auto-detect installed AI coding tools and configure cut-rs hooks safely
    Init(init::InitArgs),
    /// Check server connectivity, hook configuration, pending telemetry, and local quota
    Status(status::StatusArgs),
    /// Run the HTTP telemetry ingestion and estimation server
    #[command(alias = "server")]
    Serve,
    /// Process Antigravity CLI statusLine JSON from stdin and log quota telemetry
    AgyCli(agy::AgyCliArgs),
    /// Process Claude Code statusLine JSON from stdin and log quota telemetry
    ClaudeCli(claude::ClaudeCliArgs),
    /// Incrementally scan local Codex CLI rollout logs (~/.codex/sessions) and log telemetry
    CodexCli(codex::CodexCliArgs),
    /// Output Waybar custom module JSON aggregated from local tool quota states
    Waybar(waybar::WaybarArgs),
}
