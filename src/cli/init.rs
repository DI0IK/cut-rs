use clap::{Args, ValueEnum};
use serde_json::{Map, Value};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InitTool {
    All,
    Agy,
    Waybar,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Tools to configure (default: auto-detect Antigravity and Waybar)
    #[arg(long = "tool", value_enum, default_value = "all")]
    pub tools: Vec<InitTool>,

    /// Show planned changes without modifying any files
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// Overwrite existing cut-rs or preexisting statusLine commands instead of chaining
    #[arg(long, default_value_t = false)]
    pub force: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PatchOutcome {
    Created,
    Updated { chained: bool },
    AlreadyConfigured,
}

pub async fn run_init(args: InitArgs) -> anyhow::Result<()> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "cut-rs".to_string());

    let selected = resolve_selected_tools(&args.tools, &home);
    if selected.is_empty() {
        println!(
            "No supported tools detected in {} (.gemini/antigravity-cli or .config/waybar).",
            home.display()
        );
        println!("Pass --tool agy or --tool waybar to configure a specific tool.");
        return Ok(());
    }

    println!("cut-rs setup (binary: {exe})");
    if args.dry_run {
        println!("[dry-run] No files will be modified.");
    }
    println!();

    for tool in &selected {
        match tool {
            InitTool::Agy => {
                let path = home
                    .join(".gemini")
                    .join("antigravity-cli")
                    .join("settings.json");
                let base_cmd = format!("{} agy-cli", quote_shell_arg(&exe));
                match apply_json_status_line(&path, &base_cmd, true, args.force, args.dry_run) {
                    Ok(outcome) => print_outcome("Antigravity CLI", &path, outcome, args.dry_run),
                    Err(e) => eprintln!("  [!] Antigravity CLI ({}): {e:#}", path.display()),
                }
            }
            InitTool::Waybar => {
                print_waybar_snippet(&exe);
            }
            InitTool::All => {}
        }
    }

    println!();
    println!("Done! Run `cut-rs status` to check local quota & database state, or `cut-rs webserver start --open` to view the dashboard.");
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
    if home.join(".gemini").join("antigravity-cli").exists() {
        detected.push(InitTool::Agy);
    }
    if home.join(".config").join("waybar").exists() {
        detected.push(InitTool::Waybar);
    }
    detected
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
        }
        _ => {
            outcome = if existed {
                PatchOutcome::Updated { chained: false }
            } else {
                PatchOutcome::Created
            };
            let mut sl_obj = Map::new();
            sl_obj.insert("type".to_string(), Value::String("command".to_string()));
            let final_cmd = if stack_with_default && !force {
                format!("{base_cmd} --chain default")
            } else {
                base_cmd.to_string()
            };
            sl_obj.insert("command".to_string(), Value::String(final_cmd));
            root_obj.insert("statusLine".to_string(), Value::Object(sl_obj));
        }
    }

    Ok((serde_json::to_string_pretty(&root)? + "\n", outcome))
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

    let (updated, outcome) =
        patch_json_status_line_str(existing.as_deref(), base_cmd, stack_with_default, force)?;

    if !dry_run && outcome != PatchOutcome::AlreadyConfigured {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, updated)?;
    }

    Ok(outcome)
}

fn print_outcome(name: &str, path: &Path, outcome: PatchOutcome, dry_run: bool) {
    let prefix = if dry_run { "[dry-run] " } else { "" };
    match outcome {
        PatchOutcome::Created => {
            println!("  [+] {prefix}{name}: created {}", path.display());
        }
        PatchOutcome::Updated { chained: true } => {
            println!(
                "  [+] {prefix}{name}: updated {} (chained existing statusLine command)",
                path.display()
            );
        }
        PatchOutcome::Updated { chained: false } => {
            println!("  [+] {prefix}{name}: updated {}", path.display());
        }
        PatchOutcome::AlreadyConfigured => {
            println!("  [=] {name}: already configured ({})", path.display());
        }
    }
}

fn print_waybar_snippet(exe: &str) {
    let cmd = format!("{} waybar", quote_shell_arg(exe));
    println!("  [*] Waybar: add this custom module to ~/.config/waybar/config.jsonc:");
    println!(
        r#"      "custom/cut-rs": {{
        "exec": "{cmd}",
        "return-type": "json",
        "interval": 15,
        "format": "⚡ {{text}}"
      }}"#
    );
}
