pub mod agy;
pub mod claude;
pub mod codex;
pub mod common;
pub mod waybar;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "cut-rs", about = "Crowdsourced AI quota telemetry server & CLI")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Run the HTTP telemetry ingestion and estimation server (default)
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
