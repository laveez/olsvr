use std::process::Command;

use dialoguer::{Input, Select, theme::ColorfulTheme};

use crate::config::Config;
use crate::pid_path;

fn stop_existing() {
    // Stop systemd service if running
    let _ = Command::new("systemctl")
        .args(["--user", "stop", "olsvr.service"])
        .output();

    // Kill any process from the PID file
    let path = pid_path();
    if let Ok(contents) = std::fs::read_to_string(&path)
        && let Ok(pid) = contents.trim().parse::<i32>()
    {
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let _ = std::fs::remove_file(&path);
    }
}

pub fn run() {
    let theme = ColorfulTheme::default();

    println!();
    println!("  olsvr setup");
    println!("  OLED screensaver for Wayland");
    println!();

    stop_existing();

    // Check prerequisites
    if std::env::var("WAYLAND_DISPLAY").is_err() {
        eprintln!("Warning: WAYLAND_DISPLAY not set. olsvr requires a Wayland session.");
        println!();
    }

    let mut config = if Config::exists() {
        println!("  Found existing config at {}", Config::path().display());
        println!();
        Config::load()
    } else {
        Config::default()
    };

    loop {
        prompt_timing(&theme, &mut config);
        prompt_display(&theme, &mut config);

        if let Err(e) = config.save() {
            eprintln!("Failed to save config: {e}");
            return;
        }
        println!();
        println!("  Config saved to {}", Config::path().display());

        // Preview
        println!();
        let do_preview = Select::with_theme(&theme)
            .with_prompt("Launch a live preview?")
            .items(["Yes", "No"])
            .default(0)
            .interact()
            .unwrap();

        if do_preview == 0 {
            let exe = std::env::current_exe().unwrap_or_else(|_| "olsvr".into());
            if let Ok(mut child) = Command::new(exe).args(["run", "--now"]).spawn() {
                println!("  Preview running. Press Enter here to stop.");
                println!();
                let _ = std::io::stdin().read_line(&mut String::new());
                let _ = child.kill();
                let _ = child.wait();
            }
        }

        // Satisfaction check
        println!();
        let choice = Select::with_theme(&theme)
            .with_prompt("Happy with this configuration?")
            .items([
                "Yes, continue",
                "Change timing",
                "Change display",
                "Start over",
            ])
            .default(0)
            .interact()
            .unwrap();

        match choice {
            0 => break,
            1 => continue, // loop will re-prompt timing then display
            2 => {
                prompt_display(&theme, &mut config);
                if let Err(e) = config.save() {
                    eprintln!("Failed to save config: {e}");
                    return;
                }
                // Re-check satisfaction
                continue;
            }
            3 => {
                config = Config::default();
                continue;
            }
            _ => break,
        }
    }

    // Systemd install
    println!();
    let install = Select::with_theme(&theme)
        .with_prompt("Install as systemd user service?")
        .items(["Yes", "No"])
        .default(0)
        .interact()
        .unwrap();

    if install == 0 {
        install_systemd_service();
    }

    println!();
    println!("  Setup complete!");
    println!();
}

fn prompt_timing(theme: &ColorfulTheme, config: &mut Config) {
    println!();
    println!("  -- Timing --");
    println!();

    config.timeout = Input::with_theme(theme)
        .with_prompt("Idle timeout (minutes)")
        .default(config.timeout)
        .interact_text()
        .unwrap();

    config.hold = Input::with_theme(theme)
        .with_prompt("Hold duration between fades (seconds)")
        .default(config.hold)
        .interact_text()
        .unwrap();

    config.fade_duration = Input::with_theme(theme)
        .with_prompt("Fade duration (milliseconds)")
        .default(config.fade_duration)
        .interact_text()
        .unwrap();
}

fn prompt_display(theme: &ColorfulTheme, config: &mut Config) {
    println!();
    println!("  -- Display --");
    println!();

    config.font_size = Input::with_theme(theme)
        .with_prompt("Font size (pixels)")
        .default(config.font_size)
        .interact_text()
        .unwrap();

    let families = ["sans-serif", "serif", "monospace", "cursive", "fantasy"];
    let current_idx = families
        .iter()
        .position(|f| *f == config.font_family)
        .unwrap_or(0);
    let family_idx = Select::with_theme(theme)
        .with_prompt("Font family")
        .items(families)
        .default(current_idx)
        .interact()
        .unwrap();
    config.font_family = families[family_idx].into();

    let formats = ["24h", "12h"];
    let current_fmt = if config.time_format == "12h" { 1 } else { 0 };
    let fmt_idx = Select::with_theme(theme)
        .with_prompt("Time format")
        .items(formats)
        .default(current_fmt)
        .interact()
        .unwrap();
    config.time_format = formats[fmt_idx].into();

    config.date_format = Input::with_theme(theme)
        .with_prompt("Date format (chrono)")
        .default(config.date_format.clone())
        .interact_text()
        .unwrap();

    let brightness: u8 = Input::with_theme(theme)
        .with_prompt("Brightness (0-255)")
        .default(config.color[0])
        .interact_text()
        .unwrap();
    config.color = [brightness, brightness, brightness];

    config.edge_padding = Input::with_theme(theme)
        .with_prompt("Edge padding (pixels)")
        .default(config.edge_padding)
        .interact_text()
        .unwrap();
}

fn install_systemd_service() {
    let home = std::env::var("HOME").expect("HOME not set");
    let service_dir = format!("{home}/.config/systemd/user");
    let service_path = format!("{service_dir}/olsvr.service");

    if let Err(e) = std::fs::create_dir_all(&service_dir) {
        eprintln!("Failed to create {service_dir}: {e}");
        return;
    }

    // Find the service file — check next to the binary, then cwd
    let service_source = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("olsvr.service")))
        .filter(|p| p.exists())
        .or_else(|| {
            let cwd = std::env::current_dir().ok()?.join("olsvr.service");
            cwd.exists().then_some(cwd)
        });

    match service_source {
        Some(src) => {
            if let Err(e) = std::fs::copy(&src, &service_path) {
                eprintln!("Failed to copy service file: {e}");
                return;
            }
        }
        None => {
            // Generate a minimal service file
            let contents = format!(
                "[Unit]\n\
                 Description=OLED Screensaver\n\
                 After=graphical-session.target\n\
                 \n\
                 [Service]\n\
                 Type=simple\n\
                 ExecStart={home}/.cargo/bin/olsvr\n\
                 Restart=on-failure\n\
                 RestartSec=5\n\
                 \n\
                 [Install]\n\
                 WantedBy=graphical-session.target\n"
            );
            if let Err(e) = std::fs::write(&service_path, contents) {
                eprintln!("Failed to write service file: {e}");
                return;
            }
        }
    }

    println!("  Installed service to {service_path}");

    // Reload, enable, and (re)start
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let _ = Command::new("systemctl")
        .args(["--user", "enable", "olsvr.service"])
        .status();
    let _ = Command::new("systemctl")
        .args(["--user", "restart", "olsvr.service"])
        .status();

    println!("  Service enabled and started.");
}
