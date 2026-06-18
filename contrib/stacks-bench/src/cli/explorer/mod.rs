use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use crate::cli::common::{CliContext, ExecCommand};

const EXPLORER_REL_PATH: &str = "tools/profiler-explorer";
const SERVER_FILE: &str = "server.ts";
const DIST_DIR_NAME: &str = "dist";
const NODE_MODULES_DIR_NAME: &str = "node_modules";
const PID_FILE_NAME: &str = "profiler-explorer.pid";
const LOG_FILE_NAME: &str = "profiler-explorer.log";
const SENTINEL_FILE_NAME: &str = ".node-deps-installed";

#[derive(Args, Debug)]
pub struct ExplorerArgs {
    #[command(subcommand)]
    command: ExplorerCommand,
}

#[derive(Subcommand, Debug)]
pub enum ExplorerCommand {
    /// Setup (if needed) and start the profiler explorer
    Start(ExplorerStartArgs),
    /// Stop the profiler explorer
    Stop(ExplorerStopArgs),
    /// Show explorer status
    Status(ExplorerStatusArgs),
}

#[derive(Args, Debug)]
pub struct ExplorerStartArgs {
    /// Port to expose the explorer on (default: 8800)
    #[arg(long, default_value = "8800")]
    pub port: u16,

    /// Host/IP to bind (default: 127.0.0.1)
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Rebuild the UI before starting
    #[arg(long)]
    pub refresh: bool,

    /// Stop a running explorer before starting
    #[arg(long)]
    pub restart: bool,
}

#[derive(Args, Debug)]
pub struct ExplorerStopArgs {}

#[derive(Args, Debug)]
pub struct ExplorerStatusArgs {}

impl ExecCommand for ExplorerArgs {
    type Output = serde_json::Value;

    async fn exec(&self, ctx: &CliContext) -> Result<Self::Output> {
        let interactive = ctx.interactive();
        match &self.command {
            ExplorerCommand::Start(args) => {
                let (pid, db_path, log_path) = start_explorer(ctx, args, interactive).await?;
                if interactive {
                    cliclack::note(
                        "Explorer started",
                        format!(
                            "URL:    http://{}:{}\n\
                             PID:    {pid}\n\
                             App DB: {}\n\
                             Log:    {}",
                            args.host,
                            args.port,
                            db_path.display(),
                            log_path.display(),
                        ),
                    )?;
                }
                Ok(serde_json::json!({
                    "message": "Explorer started",
                    "host": args.host,
                    "port": args.port,
                    "url": format!("http://{}:{}", args.host, args.port),
                    "pid": pid,
                }))
            }
            ExplorerCommand::Stop(_) => {
                let pid = stop_explorer(ctx, interactive).await?;
                Ok(serde_json::json!({"message": "Explorer stopped", "pid": pid}))
            }
            ExplorerCommand::Status(_) => {
                let app_data = ctx.app_data_dir();
                let pid_path = app_data.path().join(PID_FILE_NAME);
                if let Some(pid) = read_pid(&pid_path)? {
                    if pid_is_running(pid) {
                        if interactive {
                            cliclack::log::info(format!("Explorer is running (pid {pid})"))?;
                        }
                        return Ok(serde_json::json!({"running": true, "pid": pid}));
                    }
                    if interactive {
                        cliclack::log::warning(format!(
                            "Explorer is not running (stale pid {pid})"
                        ))?;
                    }
                    return Ok(serde_json::json!({"running": false, "stale_pid": pid}));
                }
                if interactive {
                    cliclack::log::info("Explorer is not running")?;
                }
                Ok(serde_json::json!({"running": false}))
            }
        }
    }
}

/// Start the explorer process. Returns (pid, db_path, log_path) on success.
async fn start_explorer(
    ctx: &CliContext,
    args: &ExplorerStartArgs,
    interactive: bool,
) -> Result<(u32, PathBuf, PathBuf)> {
    let app_data = ctx.app_data_dir();
    let db_path = app_data.app_db_path();
    if !db_path.exists() {
        bail!(
            "Database not found at {:?}. Run a benchmark first.",
            db_path
        );
    }

    let explorer_root = explorer_root()?;
    if args.restart {
        let _ = stop_explorer(ctx, interactive).await;
    }

    if args.refresh {
        build_ui(&explorer_root, interactive)?;
    }
    ensure_ui_build(&explorer_root)?;
    ensure_node_deps(&explorer_root, interactive)?;

    let pid_path = app_data.path().join(PID_FILE_NAME);
    if let Some(pid) = read_pid(&pid_path)? {
        if pid_is_running(pid) {
            bail!("Explorer already running (pid {pid}).");
        }
        let _ = fs::remove_file(&pid_path);
    }

    let log_path = app_data.path().join(LOG_FILE_NAME);
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Failed to open log file at {log_path:?}"))?;

    let child = Command::new("node")
        .arg("--experimental-strip-types")
        .arg(SERVER_FILE)
        .arg("--db")
        .arg(&db_path)
        .arg("--port")
        .arg(args.port.to_string())
        .arg("--host")
        .arg(&args.host)
        .current_dir(&explorer_root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file.try_clone()?))
        .stderr(Stdio::from(log_file))
        .spawn()
        .context("Failed to start profiler explorer")?;

    let pid = child.id();
    fs::write(&pid_path, pid.to_string())
        .with_context(|| format!("Failed to write pid file at {pid_path:?}"))?;

    Ok((pid, db_path, log_path))
}

/// Stop the explorer process. Returns the stopped PID.
async fn stop_explorer(ctx: &CliContext, interactive: bool) -> Result<u32> {
    let app_data = ctx.app_data_dir();
    let pid_path = app_data.path().join(PID_FILE_NAME);
    let pid = read_pid(&pid_path)?.ok_or_else(|| anyhow::anyhow!("Explorer is not running"))?;

    if pid_is_running(pid) {
        Command::new("kill")
            .arg(pid.to_string())
            .status()
            .context("Failed to stop profiler explorer")?;
    }

    let _ = fs::remove_file(&pid_path);
    if interactive {
        cliclack::log::info(format!("Explorer stopped (pid {pid})"))?;
    }
    Ok(pid)
}

fn explorer_root() -> Result<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(EXPLORER_REL_PATH);
    if !root.exists() {
        bail!("Explorer directory not found at {:?}", root);
    }
    Ok(root)
}

fn ensure_node_deps(explorer_root: &Path, interactive: bool) -> Result<()> {
    let node_modules_dir = explorer_root.join(NODE_MODULES_DIR_NAME);
    let package_json = explorer_root.join("package.json");
    let sentinel = explorer_root.join(SENTINEL_FILE_NAME);

    let node_major = Command::new("node")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|v| {
            v.trim()
                .strip_prefix('v')?
                .split('.')
                .next()
                .map(String::from)
        })
        .unwrap_or_default();

    let package_json_changed = match (package_json.metadata(), sentinel.metadata()) {
        (Ok(pkg_meta), Ok(sent_meta)) => {
            let pkg_time = pkg_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let sent_time = sent_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            pkg_time > sent_time
        }
        (Ok(_), Err(_)) => true,
        (Err(_), _) => false,
    };

    let node_version_changed = fs::read_to_string(&sentinel)
        .map(|s| s.trim() != node_major)
        .unwrap_or(false);

    if package_json_changed || node_version_changed || !node_modules_dir.exists() {
        let spinner = if interactive {
            let s = cliclack::spinner();
            s.start("Installing Node dependencies...");
            Some(s)
        } else {
            None
        };

        let status = Command::new("npm")
            .arg("install")
            .current_dir(explorer_root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("Failed to install profiler explorer Node dependencies")?;

        if !status.success() {
            if let Some(s) = spinner {
                s.error("npm install failed");
            }
            bail!("npm install failed with status {:?}", status.code());
        }

        fs::write(&sentinel, &node_major)
            .with_context(|| format!("Failed to write sentinel file at {sentinel:?}"))?;

        if let Some(s) = spinner {
            s.stop(fmt_success!("Node dependencies installed"));
        }
    }

    Ok(())
}

fn ensure_ui_build(explorer_root: &Path) -> Result<()> {
    let dist_dir = explorer_root.join(DIST_DIR_NAME);
    let index_file = dist_dir.join("index.html");
    if index_file.exists() {
        return Ok(());
    }

    bail!(
        "Explorer UI build not found at {:?}. Run 'npm install' and 'npm run build' in {}/web.",
        dist_dir,
        explorer_root.display()
    );
}

fn build_ui(explorer_root: &Path, interactive: bool) -> Result<()> {
    let web_dir = explorer_root.join("web");
    if !web_dir.exists() {
        bail!("Explorer web directory not found at {:?}", web_dir);
    }

    let spinner = if interactive {
        let s = cliclack::spinner();
        s.start("Building explorer UI...");
        Some(s)
    } else {
        None
    };

    let status = Command::new("npm")
        .arg("run")
        .arg("build")
        .current_dir(&web_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("Failed to run npm build for profiler explorer")?;

    if !status.success() {
        if let Some(s) = spinner {
            s.error("UI build failed");
        }
        bail!("npm run build failed with status {:?}", status.code());
    }

    if let Some(s) = spinner {
        s.stop(fmt_success!("Explorer UI built"));
    }
    Ok(())
}

fn read_pid(pid_path: &Path) -> Result<Option<u32>> {
    if !pid_path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(pid_path)
        .with_context(|| format!("Failed to read pid file at {pid_path:?}"))?;
    let pid = contents.trim().parse::<u32>().ok();
    Ok(pid)
}

fn pid_is_running(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
