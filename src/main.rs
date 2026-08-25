mod animation;
mod backend;
mod compositor;
mod config;
mod data;
mod engine;
mod layer;
mod layers;
mod setup;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::config::Config;

#[derive(Parser)]
#[command(name = "olsvr", about = "OLED screensaver for Wayland and macOS")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the screensaver (default)
    Run(RunArgs),
    /// Stop a running instance
    Stop,
    /// Interactive setup wizard
    Setup,
}

#[derive(Parser)]
pub(crate) struct RunArgs {
    /// Idle timeout in minutes before activation
    #[arg(long)]
    pub timeout: Option<u32>,

    /// Send SIGUSR1 to a running instance to activate it
    #[arg(long)]
    pub activate: bool,

    /// Start and immediately show the screensaver (skip idle wait)
    #[arg(long)]
    pub now: bool,

    /// Clock font size in pixels
    #[arg(long)]
    pub font_size: Option<u32>,

    /// Hold duration between fades in seconds
    #[arg(long)]
    pub hold: Option<u32>,

    /// Fade duration in milliseconds
    #[arg(long)]
    pub fade_duration: Option<u32>,

    /// Edge padding in pixels
    #[arg(long)]
    pub edge_padding: Option<u32>,

    /// Show debug overlay (frame count, timing, source)
    #[arg(long)]
    pub debug: bool,
}

/// launchd agent label (and plist filename stem) on macOS.
#[cfg(target_os = "macos")]
pub(crate) const LAUNCHD_LABEL: &str = "io.github.laveez.olsvr";

#[cfg(target_os = "linux")]
pub(crate) fn pid_path() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/run/user/{uid}/olsvr.pid"))
}

#[cfg(target_os = "macos")]
pub(crate) fn pid_path() -> PathBuf {
    // $TMPDIR is a per-user directory on macOS (no /run/user equivalent).
    std::env::temp_dir().join("olsvr.pid")
}

pub(crate) fn merge_cli(config: &mut Config, args: &RunArgs) {
    if let Some(v) = args.timeout {
        config.timeout = v;
    }
    if let Some(v) = args.font_size {
        config.font_size = v;
    }
    if let Some(v) = args.hold {
        config.hold = v;
    }
    if let Some(v) = args.fade_duration {
        config.fade_duration = v;
    }
    if let Some(v) = args.edge_padding {
        config.edge_padding = v;
    }
}

/// SIGTERM the PID recorded in the pidfile, if present. Returns the PID signaled.
fn kill_pidfile() -> Option<i32> {
    let path = pid_path();
    let contents = std::fs::read_to_string(&path).ok()?;
    let Ok(pid) = contents.trim().parse::<i32>() else {
        eprintln!("Invalid PID file at {}", path.display());
        return None;
    };
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let _ = std::fs::remove_file(&path);
    Some(pid)
}

#[cfg(target_os = "linux")]
fn stop() {
    match kill_pidfile() {
        Some(pid) => println!("Stopped olsvr (PID {pid})"),
        None => {
            eprintln!("No running olsvr instance found");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "macos")]
fn stop() {
    // Boot out the launchd agent first so KeepAlive can't restart it, then also
    // kill a foreground instance via its pidfile. Either succeeding = stopped.
    let uid = unsafe { libc::getuid() };
    let booted_out = std::process::Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{LAUNCHD_LABEL}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let killed = kill_pidfile().is_some();
    if booted_out || killed {
        println!("Stopped olsvr");
    } else {
        eprintln!("No running olsvr instance found");
        std::process::exit(1);
    }
}

fn main() {
    env_logger::init();
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Setup) => setup::run(),
        Some(Command::Stop) => stop(),
        Some(Command::Run(args)) => run_screensaver(args),
        None => {
            // First-run detection: no config file → launch wizard
            if !Config::exists() {
                println!("  No config file found. Starting setup wizard...");
                println!("  (Run `olsvr run` to skip setup and use defaults)");
                println!();
                setup::run();
            } else {
                run_screensaver(RunArgs {
                    timeout: None,
                    activate: false,
                    now: false,
                    font_size: None,
                    hold: None,
                    fade_duration: None,
                    edge_padding: None,
                    debug: false,
                });
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn run_screensaver(args: RunArgs) {
    backend::wayland::run(args);
}

#[cfg(target_os = "macos")]
fn run_screensaver(args: RunArgs) {
    backend::macos::run(args);
}
