use std::process::Command;

use dialoguer::{Input, Select, theme::ColorfulTheme};

use crate::config::Config;
use crate::pid_path;

fn stop_existing() {
    stop_autostart_service();

    // Kill any process from the PID file
    let path = pid_path();
    if let Ok(contents) = std::fs::read_to_string(&path)
        && let Ok(pid) = contents.trim().parse::<i32>()
    {
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let _ = std::fs::remove_file(&path);
    }
}

/// Stop a previously-installed autostart unit so it can't restart mid-setup.
#[cfg(target_os = "linux")]
fn stop_autostart_service() {
    let _ = Command::new("systemctl")
        .args(["--user", "stop", "olsvr.service"])
        .output();
}

#[cfg(target_os = "macos")]
fn stop_autostart_service() {
    let uid = unsafe { libc::getuid() };
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{}", crate::LAUNCHD_LABEL)])
        .output();
}

/// Warn about a missing display server (Wayland-only; no-op on macOS).
#[cfg(target_os = "linux")]
fn check_prerequisites() {
    if std::env::var("WAYLAND_DISPLAY").is_err() {
        eprintln!("Warning: WAYLAND_DISPLAY not set. olsvr requires a Wayland session.");
        println!();
    }
}

#[cfg(target_os = "macos")]
fn check_prerequisites() {}

pub fn run() {
    let theme = ColorfulTheme::default();

    println!();
    println!("  olsvr setup");
    println!("  OLED screensaver");
    println!();

    stop_existing();
    check_prerequisites();

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
        prompt_weather(&theme, &mut config);
        rebuild_layers(&mut config);

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
                "Change weather",
                "Start over",
            ])
            .default(0)
            .interact()
            .unwrap();

        match choice {
            0 => break,
            1 => continue,
            2 => {
                prompt_display(&theme, &mut config);
                rebuild_layers(&mut config);
                if let Err(e) = config.save() {
                    eprintln!("Failed to save config: {e}");
                    return;
                }
                continue;
            }
            3 => {
                prompt_weather(&theme, &mut config);
                rebuild_layers(&mut config);
                if let Err(e) = config.save() {
                    eprintln!("Failed to save config: {e}");
                    return;
                }
                continue;
            }
            4 => {
                config = Config::default();
                continue;
            }
            _ => break,
        }
    }

    offer_autostart_install(&theme);

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

fn prompt_weather(theme: &ColorfulTheme, config: &mut Config) {
    println!();
    println!("  -- Weather --");
    println!();

    // Detect current weather config from layers
    let existing = config.layers.as_ref().and_then(|layers| {
        layers
            .iter()
            .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("weather"))
            .cloned()
    });
    let has_weather = existing.is_some();

    let enable_idx = Select::with_theme(theme)
        .with_prompt("Show weather overlay?")
        .items(["Yes", "No"])
        .default(if has_weather { 0 } else { 1 })
        .interact()
        .unwrap();

    if enable_idx == 1 {
        // Remove weather layer if present
        if let Some(ref mut layers) = config.layers {
            layers.retain(|t| t.get("type").and_then(|v| v.as_str()) != Some("weather"));
        }
        return;
    }

    let default_location = existing
        .as_ref()
        .and_then(|t| t.get("location").and_then(|v| v.as_str()))
        .unwrap_or("helsinki");
    let location: String = Input::with_theme(theme)
        .with_prompt("Weather location")
        .default(default_location.into())
        .interact_text()
        .unwrap();

    let default_interval = existing
        .as_ref()
        .and_then(|t| t.get("update_interval").and_then(|v| v.as_integer()))
        .unwrap_or(600) as u32;
    let interval: u32 = Input::with_theme(theme)
        .with_prompt("Update interval (seconds)")
        .default(default_interval)
        .interact_text()
        .unwrap();

    let default_font_size = existing
        .as_ref()
        .and_then(|t| t.get("font_size").and_then(|v| v.as_integer()))
        .unwrap_or(28) as u32;
    let font_size: u32 = Input::with_theme(theme)
        .with_prompt("Weather font size")
        .default(default_font_size)
        .interact_text()
        .unwrap();

    let default_icon_font = existing
        .as_ref()
        .and_then(|t| t.get("icon_font").and_then(|v| v.as_str()))
        .unwrap_or("MesloLGS NF");
    let icon_font: String = Input::with_theme(theme)
        .with_prompt("Icon font (Nerd Font with weather glyphs)")
        .default(default_icon_font.into())
        .interact_text()
        .unwrap();

    // Build weather layer table
    let mut weather = toml::value::Table::new();
    weather.insert("type".into(), toml::Value::String("weather".into()));
    weather.insert("location".into(), toml::Value::String(location));
    weather.insert(
        "update_interval".into(),
        toml::Value::Integer(interval as i64),
    );
    weather.insert("font_size".into(), toml::Value::Integer(font_size as i64));
    weather.insert("icon_font".into(), toml::Value::String(icon_font));

    // Replace or add weather layer
    let layers = config.layers.get_or_insert_with(Vec::new);
    if let Some(existing) = layers
        .iter_mut()
        .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("weather"))
    {
        *existing = weather;
    } else {
        layers.push(weather);
    }
}

/// Rebuild config.layers to ensure a clock layer exists alongside weather.
fn rebuild_layers(config: &mut Config) {
    let has_weather = config.layers.as_ref().is_some_and(|l| {
        l.iter()
            .any(|t| t.get("type").and_then(|v| v.as_str()) == Some("weather"))
    });

    if !has_weather {
        // No weather — drop layers so flat config is used for the single clock
        config.layers = None;
        return;
    }

    // Weather enabled — ensure clock layer exists in the layers array
    let layers = config.layers.get_or_insert_with(Vec::new);
    let has_clock = layers
        .iter()
        .any(|t| t.get("type").and_then(|v| v.as_str()) == Some("clock"));

    if !has_clock {
        // Build clock layer from flat config fields
        let mut clock = toml::value::Table::new();
        clock.insert("type".into(), toml::Value::String("clock".into()));
        clock.insert(
            "font_size".into(),
            toml::Value::Integer(config.font_size as i64),
        );
        clock.insert(
            "font_family".into(),
            toml::Value::String(config.font_family.clone()),
        );
        clock.insert(
            "time_format".into(),
            toml::Value::String(config.time_format.clone()),
        );
        clock.insert(
            "date_format".into(),
            toml::Value::String(config.date_format.clone()),
        );
        clock.insert(
            "color".into(),
            toml::Value::Array(
                config
                    .color
                    .iter()
                    .map(|&c| toml::Value::Integer(c as i64))
                    .collect(),
            ),
        );
        clock.insert("hold".into(), toml::Value::Integer(config.hold as i64));
        clock.insert(
            "fade_duration".into(),
            toml::Value::Integer(config.fade_duration as i64),
        );
        clock.insert(
            "edge_padding".into(),
            toml::Value::Integer(config.edge_padding as i64),
        );
        layers.insert(0, clock);
    } else {
        // Update existing clock layer from flat config fields
        let clock = layers
            .iter_mut()
            .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("clock"))
            .unwrap();
        clock.insert(
            "font_size".into(),
            toml::Value::Integer(config.font_size as i64),
        );
        clock.insert(
            "font_family".into(),
            toml::Value::String(config.font_family.clone()),
        );
        clock.insert(
            "time_format".into(),
            toml::Value::String(config.time_format.clone()),
        );
        clock.insert(
            "date_format".into(),
            toml::Value::String(config.date_format.clone()),
        );
        clock.insert(
            "color".into(),
            toml::Value::Array(
                config
                    .color
                    .iter()
                    .map(|&c| toml::Value::Integer(c as i64))
                    .collect(),
            ),
        );
        clock.insert("hold".into(), toml::Value::Integer(config.hold as i64));
        clock.insert(
            "fade_duration".into(),
            toml::Value::Integer(config.fade_duration as i64),
        );
        clock.insert(
            "edge_padding".into(),
            toml::Value::Integer(config.edge_padding as i64),
        );
    }
}

#[cfg(target_os = "linux")]
fn offer_autostart_install(theme: &ColorfulTheme) {
    println!();
    let install = Select::with_theme(theme)
        .with_prompt("Install as systemd user service?")
        .items(["Yes", "No"])
        .default(0)
        .interact()
        .unwrap();
    if install == 0 {
        install_systemd_service();
    }
}

#[cfg(target_os = "macos")]
fn offer_autostart_install(theme: &ColorfulTheme) {
    println!();
    let install = Select::with_theme(theme)
        .with_prompt("Install as a login agent (launchd, no Dock icon)?")
        .items(["Yes", "No"])
        .default(0)
        .interact()
        .unwrap();
    if install == 0 {
        install_launchd_agent();
    }
}

#[cfg(target_os = "linux")]
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

/// Wrap the installed binary in a minimal `olsvr.app` whose `Info.plist` sets
/// `LSUIElement` so the login agent runs without a Dock icon. Returns the path
/// to the executable inside the bundle, or `None` if the bundle couldn't be made
/// (the caller then falls back to the bare binary).
#[cfg(target_os = "macos")]
fn create_app_bundle() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let app_dir = format!("{home}/Applications/olsvr.app");
    let macos_dir = format!("{app_dir}/Contents/MacOS");
    let exe_dst = format!("{macos_dir}/olsvr");
    let info_dst = format!("{app_dir}/Contents/Info.plist");

    if let Err(e) = std::fs::create_dir_all(&macos_dir) {
        eprintln!("Failed to create {macos_dir}: {e}");
        return None;
    }

    // Copy (not symlink) so the bundle is self-contained and LaunchServices reads
    // its Info.plist; re-run setup after upgrading to refresh the copy.
    let src = std::env::current_exe().ok()?;
    if let Err(e) = std::fs::copy(&src, &exe_dst) {
        eprintln!("Failed to copy binary into app bundle: {e}");
        return None;
    }

    let info = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key>
  <string>{label}</string>
  <key>CFBundleName</key>
  <string>olsvr</string>
  <key>CFBundleExecutable</key>
  <string>olsvr</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>LSUIElement</key>
  <true/>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
"#,
        label = crate::LAUNCHD_LABEL,
    );
    if let Err(e) = std::fs::write(&info_dst, info) {
        eprintln!("Failed to write app Info.plist: {e}");
        return None;
    }

    println!("  Created app bundle at {app_dir}");
    Some(exe_dst)
}

#[cfg(target_os = "macos")]
fn install_launchd_agent() {
    let home = std::env::var("HOME").expect("HOME not set");
    let label = crate::LAUNCHD_LABEL;
    let agents_dir = format!("{home}/Library/LaunchAgents");
    let plist_path = format!("{agents_dir}/{label}.plist");

    if let Err(e) = std::fs::create_dir_all(&agents_dir) {
        eprintln!("Failed to create {agents_dir}: {e}");
        return;
    }

    // Launch from the LSUIElement app bundle so the agent has no Dock icon; fall
    // back to the bare installed binary if the bundle couldn't be created.
    let exe = create_app_bundle().unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{home}/.cargo/bin/olsvr"))
    });

    // KeepAlive only on crash (SuccessfulExit=false) mirrors systemd Restart=on-failure,
    // so a clean `olsvr stop` / bootout stays stopped.
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>run</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ProcessType</key>
  <string>Interactive</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>RUST_LOG</key>
    <string>info</string>
  </dict>
</dict>
</plist>
"#
    );

    if let Err(e) = std::fs::write(&plist_path, plist) {
        eprintln!("Failed to write launchd plist: {e}");
        return;
    }
    println!("  Installed agent to {plist_path}");

    // Reload: bootout any previous instance, then bootstrap into the GUI domain.
    let uid = unsafe { libc::getuid() };
    let domain = format!("gui/{uid}");
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{label}")])
        .output();
    let bootstrapped = Command::new("launchctl")
        .args(["bootstrap", &domain, &plist_path])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !bootstrapped {
        // Older macOS without `bootstrap`: fall back to legacy load.
        let _ = Command::new("launchctl")
            .args(["load", "-w", &plist_path])
            .status();
    }
    println!("  Agent enabled and started.");
}
