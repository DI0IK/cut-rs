use crate::api::build_router;
use crate::cli::common::{default_webserver_log_path, default_webserver_pid_path};
use crate::config::Config;
use crate::init_app_state;
use clap::{Args, Subcommand};
use std::fs::OpenOptions;
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Args)]
pub struct WebserverArgs {
    #[command(subcommand)]
    pub action: WebserverAction,
}

#[derive(Debug, Subcommand)]
pub enum WebserverAction {
    /// Start the local web dashboard server
    Start(StartArgs),
    /// Stop the running background web dashboard server
    Stop,
    /// Check if the local web dashboard server is running
    Status,
}

#[derive(Debug, Args)]
pub struct StartArgs {
    /// Host address to bind to
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Port to listen on
    #[arg(long, default_value_t = 8080)]
    pub port: u16,

    /// Run in the foreground instead of daemonizing
    #[arg(long, default_value_t = false)]
    pub foreground: bool,

    /// Open the dashboard URL in your default browser after starting
    #[arg(long, default_value_t = false)]
    pub open: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PidFileInfo {
    pid: u32,
    host: String,
    port: u16,
}

pub async fn run_webserver(args: WebserverArgs) -> anyhow::Result<()> {
    match args.action {
        WebserverAction::Start(start) => run_start(start).await,
        WebserverAction::Stop => run_stop(),
        WebserverAction::Status => run_status(),
    }
}

async fn run_start(args: StartArgs) -> anyhow::Result<()> {
    let pid_path = default_webserver_pid_path();

    if args.foreground {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("cut_rs=info,tower_http=info")),
            )
            .init();

        let mut config = Config::from_env();
        config.host = args.host.clone();
        config.port = args.port;
        let addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
        let state = init_app_state(config).await?;
        let router = build_router(state);
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!(%addr, "cut-rs webserver listening");
        if args.open {
            open_browser(&format!("http://{addr}"));
        }
        axum::serve(listener, router).await?;
        return Ok(());
    }

    if let Some(info) = read_running_pid(&pid_path) {
        let url = format!("http://{}:{}", info.host, info.port);
        println!("cut-rs webserver is already running (pid={}, url={url})", info.pid);
        if args.open {
            open_browser(&url);
        }
        return Ok(());
    }

    let exe = std::env::current_exe()?;
    let log_path = default_webserver_log_path();
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_err = log_file.try_clone()?;

    let child = Command::new(exe)
        .args([
            "webserver",
            "start",
            "--foreground",
            "--host",
            &args.host,
            "--port",
            &args.port.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err))
        .spawn()?;

    let pid = child.id();
    let info = PidFileInfo {
        pid,
        host: args.host.clone(),
        port: args.port,
    };
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&pid_path, serde_json::to_string_pretty(&info)?)?;

    // Brief wait to confirm the port is bound
    let addr = format!("{}:{}", args.host, args.port);
    let mut ready = false;
    for _ in 0..20 {
        if std::net::TcpStream::connect(&addr).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let url = format!("http://{addr}");
    if ready {
        println!("cut-rs webserver started (pid={pid}, url={url})");
        if args.open {
            open_browser(&url);
        }
    } else {
        println!(
            "cut-rs webserver spawned (pid={pid}, url={url}); check {} if not reachable",
            log_path.display()
        );
    }

    Ok(())
}

fn run_stop() -> anyhow::Result<()> {
    let pid_path = default_webserver_pid_path();
    let Some(info) = read_pid_file(&pid_path) else {
        println!("cut-rs webserver is not running.");
        return Ok(());
    };

    if is_pid_alive(info.pid) {
        let _ = Command::new("kill")
            .args(["-TERM", &info.pid.to_string()])
            .status();
        for _ in 0..20 {
            if !is_pid_alive(info.pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        println!("cut-rs webserver stopped (pid={}).", info.pid);
    } else {
        println!("cut-rs webserver was not running (cleaned up stale pid file).");
    }

    let _ = std::fs::remove_file(&pid_path);
    Ok(())
}

fn run_status() -> anyhow::Result<()> {
    let pid_path = default_webserver_pid_path();
    if let Some(info) = read_running_pid(&pid_path) {
        println!(
            "cut-rs webserver: running (pid={}, url=http://{}:{})",
            info.pid, info.host, info.port
        );
    } else {
        if pid_path.exists() {
            let _ = std::fs::remove_file(&pid_path);
        }
        println!("cut-rs webserver: stopped");
    }
    Ok(())
}

pub fn get_running_webserver_url() -> Option<(u32, String)> {
    let pid_path = default_webserver_pid_path();
    read_running_pid(&pid_path).map(|info| (info.pid, format!("http://{}:{}", info.host, info.port)))
}

fn read_pid_file(path: &Path) -> Option<PidFileInfo> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn read_running_pid(path: &Path) -> Option<PidFileInfo> {
    let info = read_pid_file(path)?;
    if is_pid_alive(info.pid) {
        Some(info)
    } else {
        None
    }
}

fn is_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn open_browser(url: &str) {
    #[cfg(target_os = "linux")]
    let _ = Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let _ = url;
}
