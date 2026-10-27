mod animation;
mod backend;
mod compositor;
mod config;
mod data;
mod engine;
mod layer;
mod layers;
mod pidfile;
mod setup;

use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::pidfile::StopOutcome;

#[derive(Parser)]
#[command(
    name = "olsvr",
    version,
    about = "OLED screensaver for Wayland and macOS"
)]
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
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub timeout: Option<u32>,

    /// Send SIGUSR1 to a running instance to activate it (Linux only)
    #[arg(long)]
    pub activate: bool,

    /// Show the screensaver now: a preview that ends at the first input, or on Linux the
    /// running instance's saver
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

/// Exit status when there is no compositor to connect to. It differs from the 1 of a failure,
/// so the autostart unit can leave a start without a session alone (`RestartPreventExitStatus`).
#[cfg(target_os = "linux")]
pub(crate) const EXIT_NO_COMPOSITOR: i32 = 3;

/// Exit status when another instance holds the PID-file lock. It differs from the 1 of a
/// failure, so the autostart unit can leave a refused start alone (`RestartPreventExitStatus`).
pub(crate) const EXIT_ALREADY_RUNNING: i32 = 4;

/// Applies the command-line overrides, then validates the result, so a bad value from either
/// source is replaced before any layer reads it.
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
    for problem in config.validate() {
        log::error!("Config: {problem}");
    }
}

#[cfg(target_os = "linux")]
fn stop() {
    match pidfile::stop_running() {
        Ok(StopOutcome::Stopped(pid)) => println!("Stopped olsvr (PID {pid})"),
        Ok(StopOutcome::StillRunning(pid)) => {
            eprintln!("olsvr (PID {pid}) got SIGTERM but has not exited yet");
        }
        Ok(StopOutcome::NotRunning) => {
            eprintln!("No running olsvr instance found");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Cannot signal the running olsvr instance: {e}");
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
    let killed = match pidfile::stop_running() {
        Ok(StopOutcome::NotRunning) => None,
        Ok(StopOutcome::Stopped(pid)) => Some(pid),
        Ok(StopOutcome::StillRunning(pid)) => {
            eprintln!("olsvr (PID {pid}) got SIGTERM but has not exited yet");
            return;
        }
        Err(e) => {
            eprintln!("Cannot signal the running olsvr instance: {e}");
            None
        }
    };
    match killed {
        Some(pid) => println!("Stopped olsvr (PID {pid})"),
        None if booted_out => println!("Stopped olsvr"),
        None => {
            eprintln!("No running olsvr instance found");
            std::process::exit(1);
        }
    }
}

fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("error,olsvr=info,wgpu=warn"),
    )
    .init();
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

/// A main-thread panic ends the process at the panic site instead of unwinding: winit catches
/// it on macOS and leaves the process alive without a saver, and on Linux the unwind drops the
/// Wayland connection before the swapchain, which can segfault.
fn exit_on_main_thread_panic() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        if std::thread::current().name() == Some("main") {
            pidfile::remove_if_ours(&pidfile::path());
            unsafe { libc::_exit(1) }
        }
    }));
}

fn log_version() {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("path unknown: {e}"));
    log::info!("olsvr {} ({exe})", env!("CARGO_PKG_VERSION"));
}

#[cfg(target_os = "linux")]
fn run_screensaver(args: RunArgs) {
    exit_on_main_thread_panic();
    log_version();
    backend::wayland::run(args);
}

#[cfg(target_os = "macos")]
fn run_screensaver(args: RunArgs) {
    exit_on_main_thread_panic();
    log_version();
    backend::macos::run(args);
}
