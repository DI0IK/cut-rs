use crate::cli::common::{load_shared_auth, register_client, save_shared_auth, shared_auth_path};
use clap::{Args, ValueEnum};
use serde_json::{Map, Value};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_SERVER: &str = "https://cut-rs.dominikstahl.dev";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InitTool {
    All,
    Claude,
    Codex,
    Agy,
    Waybar,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Tools to configure (default: auto-detect all installed tools)
    #[arg(long = "tool", value_enum, default_value = "all")]
    pub tools: Vec<InitTool>,

    /// Subscription tier for Claude Code / Codex fallback (e.g. pro, max_5x, max_20x)
    #[arg(long)]
    pub tier: Option<String>,

    /// cut-rs server base URL
    #[arg(long, env = "CUT_RS_SERVER", default_value = DEFAULT_SERVER)]
    pub server: String,

    /// Non-interactive mode: accept defaults without prompting
    #[arg(short = 'y', long, default_value_t = false)]
    pub yes: bool,

    /// Show planned changes without modifying any files
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// Overwrite existing cut-rs or preexisting statusLine/notify commands instead of chaining
    #[arg(long, default_value_t = false)]
    pub force: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PatchOutcome {
    Created,
    Updated { chained: bool },
    AlreadyConfigured,
    SkippedConflict(String),
}

pub async fn run_init(args: InitArgs) -> anyhow::Result<()> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "cut-rs".to_string());

    let selected = resolve_selected_tools(&args.tools, &home);
    if selected.is_empty() {
        println!("No supported AI coding tools detected in {}.", home.display());
        println!("Pass --tool claude|codex|agy|waybar to configure a specific tool.");
        return Ok(());
    }

    println!("cut-rs setup (binary: {exe})");
    if args.dry_run {
        println!("[dry-run] No files will be modified.");
    }
    println!();

    let server_arg = if args.server.trim_end_matches('/') != DEFAULT_SERVER {
        Some(args.server.trim_end_matches('/').to_string())
    } else {
        None
    };

    for tool in &selected {
        match tool {
            InitTool::Claude => {
                let tier = resolve_claude_tier(&args);
                let path = home.join(".claude").join("settings.json");
                let base_cmd = build_subcommand(&exe, "claude-cli", Some(&tier), server_arg.as_deref());
                match apply_json_status_line(&path, &base_cmd, false, args.force, args.dry_run) {
                    Ok(outcome) => print_outcome("Claude Code", &path, outcome, args.dry_run),
                    Err(e) => eprintln!("  [!] Claude Code ({}): {e:#}", path.display()),
                }
            }
            InitTool::Agy => {
                let path = home.join(".gemini").join("antigravity-cli").join("settings.json");
                let base_cmd = build_subcommand(&exe, "agy-cli", None, server_arg.as_deref());
                match apply_json_status_line(&path, &base_cmd, true, args.force, args.dry_run) {
                    Ok(outcome) => print_outcome("Antigravity CLI", &path, outcome, args.dry_run),
                    Err(e) => eprintln!("  [!] Antigravity CLI ({}): {e:#}", path.display()),
                }
            }
            InitTool::Codex => {
                let path = home.join(".codex").join("config.toml");
                let mut notify_arr = vec![exe.clone(), "codex-cli".to_string()];
                if let Some(s) = &server_arg {
                    notify_arr.push("--server".to_string());
                    notify_arr.push(s.clone());
                }
                match apply_codex_toml(&path, &notify_arr, args.force, args.dry_run) {
                    Ok(outcome) => {
                        print_outcome("Codex CLI", &path, outcome, args.dry_run);
                        println!(
                            "      Tip: For Codex Desktop / VS Code extension, also run: {exe} codex-cli --watch"
                        );
                    }
                    Err(e) => eprintln!("  [!] Codex CLI ({}): {e:#}", path.display()),
                }
            }
            InitTool::Waybar => {
                print_waybar_snippet(&exe, server_arg.as_deref());
            }
            InitTool::All => {}
        }
    }

    if !args.dry_run {
        println!();
        ensure_server_registration(&args.server).await;
    }

    println!();
    println!("Done! Run `cut-rs status` at any time to verify telemetry and quota sync.");
    Ok(())
}

fn resolve_selected_tools(tools: &[InitTool], home: &Path) -> Vec<InitTool> {
    if !tools.contains(&InitTool::All) {
        let mut out = Vec::new();
        for t in tools {
            if !out.contains(t) {
                out.push(*t);
            }
        }
        return out;
    }

    let mut detected = Vec::new();
    if home.join(".claude").exists() {
        detected.push(InitTool::Claude);
    }
    if home.join(".codex").exists() {
        detected.push(InitTool::Codex);
    }
    if home.join(".gemini").join("antigravity-cli").exists() {
        detected.push(InitTool::Agy);
    }
    if home.join(".config").join("waybar").exists() {
        detected.push(InitTool::Waybar);
    }
    detected
}

fn resolve_claude_tier(args: &InitArgs) -> String {
    if let Some(t) = &args.tier {
        return t.trim().to_string();
    }
    if args.yes || !std::io::stdin().is_terminal() {
        return "default".to_string();
    }
    print!("Claude plan tier [pro / max_5x / max_20x / default] (default): ");
    let _ = std::io::stdout().flush();
    let mut buf = String::new();
    if std::io::stdin().read_line(&mut buf).is_ok() {
        let trimmed = buf.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "default".to_string()
}

fn quote_shell_arg(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '\\' | '.' | '_' | '-' | ':'))
    {
        s.to_string()
    } else {
        let escaped = s.replace('"', "\\\"");
        format!("\"{escaped}\"")
    }
}

fn build_subcommand(
    exe: &str,
    subcommand: &str,
    tier: Option<&str>,
    server: Option<&str>,
) -> String {
    let mut parts = vec![quote_shell_arg(exe), subcommand.to_string()];
    if let Some(t) = tier
        && !t.is_empty()
        && t != "default"
    {
        parts.push("--tier".to_string());
        parts.push(quote_shell_arg(t));
    }
    if let Some(s) = server {
        parts.push("--server".to_string());
        parts.push(quote_shell_arg(s));
    }
    parts.join(" ")
}

pub fn patch_json_status_line_str(
    existing_json: Option<&str>,
    base_cmd: &str,
    stack_with_default: bool,
    force: bool,
) -> anyhow::Result<(String, PatchOutcome)> {
    let mut root: Value = match existing_json {
        Some(raw) if !raw.trim().is_empty() => serde_json::from_str(raw)?,
        _ => Value::Object(Map::new()),
    };

    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Expected top-level JSON object"))?;

    let existed = existing_json.is_some();
    let outcome;

    match root_obj.get_mut("statusLine") {
        Some(Value::Object(sl_obj)) => {
            let existing_cmd = sl_obj
                .get("command")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());

            if let Some(prev) = existing_cmd {
                if prev.contains("cut-rs") && !force {
                    return Ok((
                        serde_json::to_string_pretty(&root)? + "\n",
                        PatchOutcome::AlreadyConfigured,
                    ));
                }
                let final_cmd = if force {
                    outcome = PatchOutcome::Updated { chained: false };
                    base_cmd.to_string()
                } else {
                    outcome = PatchOutcome::Updated { chained: true };
                    format!("{base_cmd} --chain {}", quote_shell_arg(&prev))
                };
                sl_obj.insert("type".to_string(), Value::String("command".to_string()));
                sl_obj.insert("command".to_string(), Value::String(final_cmd));
            } else {
                outcome = if existed {
                    PatchOutcome::Updated { chained: false }
                } else {
                    PatchOutcome::Created
                };
                sl_obj.insert("type".to_string(), Value::String("command".to_string()));
                sl_obj.insert("command".to_string(), Value::String(base_cmd.to_string()));
            }
            if stack_with_default && !sl_obj.contains_key("stack_with_default") {
                sl_obj.insert("stack_with_default".to_string(), Value::Bool(true));
            }
        }
        _ => {
            outcome = if existed {
                PatchOutcome::Updated { chained: false }
            } else {
                PatchOutcome::Created
            };
            let mut sl_obj = Map::new();
            sl_obj.insert("type".to_string(), Value::String("command".to_string()));
            sl_obj.insert("command".to_string(), Value::String(base_cmd.to_string()));
            if stack_with_default {
                sl_obj.insert("stack_with_default".to_string(), Value::Bool(true));
            }
            root_obj.insert("statusLine".to_string(), Value::Object(sl_obj));
        }
    }

    let formatted = serde_json::to_string_pretty(&root)? + "\n";
    Ok((formatted, outcome))
}

fn apply_json_status_line(
    path: &Path,
    base_cmd: &str,
    stack_with_default: bool,
    force: bool,
    dry_run: bool,
) -> anyhow::Result<PatchOutcome> {
    let existing = if path.exists() {
        Some(std::fs::read_to_string(path)?)
    } else {
        None
    };

    let (new_content, outcome) = patch_json_status_line_str(
        existing.as_deref(),
        base_cmd,
        stack_with_default,
        force,
    )?;

    if matches!(outcome, PatchOutcome::AlreadyConfigured) || dry_run {
        return Ok(outcome);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if existing.is_some() {
        let bak = backup_path(path);
        std::fs::copy(path, &bak)?;
    }
    std::fs::write(path, new_content)?;
    Ok(outcome)
}

pub fn patch_codex_toml_str(
    existing_toml: Option<&str>,
    notify_cmd: &[String],
    force: bool,
) -> (String, PatchOutcome) {
    let formatted_array = notify_cmd
        .iter()
        .map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let notify_line = format!("notify = [{formatted_array}]");

    let Some(raw) = existing_toml else {
        return (format!("{notify_line}\n"), PatchOutcome::Created);
    };

    let mut lines: Vec<String> = raw.lines().map(|l| l.to_string()).collect();
    let mut in_top_level = true;
    let mut existing_idx = None;
    let mut first_table_idx = None;

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_top_level = false;
            if first_table_idx.is_none() {
                first_table_idx = Some(idx);
            }
        }
        if in_top_level
            && let Some(rest) = trimmed.strip_prefix("notify")
            && rest.trim_start().starts_with('=')
        {
            existing_idx = Some(idx);
            break;
        }
    }

    if let Some(idx) = existing_idx {
        let current = lines[idx].clone();
        if current.contains("cut-rs") && !force {
            return (raw.to_string(), PatchOutcome::AlreadyConfigured);
        }
        if !force {
            return (raw.to_string(), PatchOutcome::SkippedConflict(current));
        }
        lines[idx] = notify_line;
        let mut out = lines.join("\n");
        if raw.ends_with('\n') || !out.is_empty() {
            out.push('\n');
        }
        return (out, PatchOutcome::Updated { chained: false });
    }

    let insert_at = first_table_idx.unwrap_or(lines.len());
    lines.insert(insert_at, notify_line);
    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    (out, PatchOutcome::Updated { chained: false })
}

fn apply_codex_toml(
    path: &Path,
    notify_cmd: &[String],
    force: bool,
    dry_run: bool,
) -> anyhow::Result<PatchOutcome> {
    let existing = if path.exists() {
        Some(std::fs::read_to_string(path)?)
    } else {
        None
    };

    let (new_content, outcome) = patch_codex_toml_str(existing.as_deref(), notify_cmd, force);
    if matches!(
        outcome,
        PatchOutcome::AlreadyConfigured | PatchOutcome::SkippedConflict(_)
    ) || dry_run
    {
        return Ok(outcome);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if existing.is_some() {
        let bak = backup_path(path);
        std::fs::copy(path, &bak)?;
    }
    std::fs::write(path, new_content)?;
    Ok(outcome)
}

fn backup_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".bak");
    PathBuf::from(os)
}

fn print_outcome(label: &str, path: &Path, outcome: PatchOutcome, dry_run: bool) {
    let verb = if dry_run { "would " } else { "" };
    match outcome {
        PatchOutcome::Created => {
            println!("  [✓] {label}: {verb}create {}", path.display());
        }
        PatchOutcome::Updated { chained: true } => {
            println!(
                "  [✓] {label}: {verb}prepend cut-rs (chaining existing statusLine) in {}",
                path.display()
            );
        }
        PatchOutcome::Updated { chained: false } => {
            println!("  [✓] {label}: {verb}update {}", path.display());
        }
        PatchOutcome::AlreadyConfigured => {
            println!("  [=] {label}: already configured in {}", path.display());
        }
        PatchOutcome::SkippedConflict(existing) => {
            println!(
                "  [!] {label}: skipped {} (existing `{existing}`). Re-run with --force to replace.",
                path.display()
            );
        }
    }
}

fn print_waybar_snippet(exe: &str, server: Option<&str>) {
    let server_flag = server
        .map(|s| format!(" --server {s}"))
        .unwrap_or_default();
    println!("  [i] Waybar detected. Add this module to your Waybar config:");
    println!(
        r#"      "custom/cut-rs": {{
        "exec": "{exe} waybar{server_flag}",
        "return-type": "json",
        "interval": 5,
        "format": "󰚩 {{}}",
        "tooltip": true
      }}"#
    );
}

async fn ensure_server_registration(server: &str) {
    if let Some(auth) = load_shared_auth(None) {
        println!(
            "  [✓] Client already registered ({}) at {}",
            auth.client_id,
            shared_auth_path().display()
        );
        return;
    }

    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_millis(4000))
        .build()
    else {
        return;
    };

    match register_client(&client, server).await {
        Ok(auth) => {
            if let Err(e) = save_shared_auth(&auth) {
                eprintln!("  [!] Registered client {}, but failed to save auth.json: {e}", auth.client_id);
            } else {
                println!(
                    "  [✓] Registered client {} with {server}",
                    auth.client_id
                );
            }
        }
        Err(e) => {
            println!(
                "  [!] Could not reach {server} yet ({e}). Will auto-register on first telemetry event."
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_patch_creates_new_config() {
        let (out, outcome) = patch_json_status_line_str(
            None,
            "/usr/local/bin/cut-rs claude-cli --tier pro",
            false,
            false,
        )
        .unwrap();
        assert_eq!(outcome, PatchOutcome::Created);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["statusLine"]["command"],
            "/usr/local/bin/cut-rs claude-cli --tier pro"
        );
    }

    #[test]
    fn json_patch_prepends_and_chains_existing_status_line() {
        let existing = r#"{
  "theme": "dark",
  "statusLine": {
    "type": "command",
    "command": "npx ccstatusline",
    "padding": 1
  }
}"#;
        let (out, outcome) = patch_json_status_line_str(
            Some(existing),
            "/usr/local/bin/cut-rs claude-cli --tier max_5x",
            false,
            false,
        )
        .unwrap();
        assert_eq!(outcome, PatchOutcome::Updated { chained: true });
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["statusLine"]["padding"], 1);
        assert_eq!(
            v["statusLine"]["command"],
            "/usr/local/bin/cut-rs claude-cli --tier max_5x --chain \"npx ccstatusline\""
        );
    }

    #[test]
    fn json_patch_is_idempotent() {
        let existing = r#"{
  "statusLine": {
    "type": "command",
    "command": "/usr/local/bin/cut-rs agy-cli --chain \"my-status\""
  }
}"#;
        let (_, outcome) =
            patch_json_status_line_str(Some(existing), "/usr/local/bin/cut-rs agy-cli", true, false)
                .unwrap();
        assert_eq!(outcome, PatchOutcome::AlreadyConfigured);
    }

    #[test]
    fn codex_toml_inserts_before_tables() {
        let existing = "[model]\nname = \"gpt-5\"\n";
        let (out, outcome) = patch_codex_toml_str(
            Some(existing),
            &["/usr/bin/cut-rs".to_string(), "codex-cli".to_string()],
            false,
        );
        assert_eq!(outcome, PatchOutcome::Updated { chained: false });
        assert!(out.starts_with("notify = [\"/usr/bin/cut-rs\", \"codex-cli\"]\n[model]"));
    }

    #[test]
    fn codex_toml_skips_existing_non_cut_rs_notify_without_force() {
        let existing = "notify = [\"my-notifier\"]\n";
        let (out, outcome) = patch_codex_toml_str(
            Some(existing),
            &["/usr/bin/cut-rs".to_string(), "codex-cli".to_string()],
            false,
        );
        assert!(matches!(outcome, PatchOutcome::SkippedConflict(_)));
        assert_eq!(out, existing);
    }
}
