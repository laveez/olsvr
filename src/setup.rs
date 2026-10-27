use std::process::Command;

use dialoguer::{Input, Select, theme::ColorfulTheme};
use serde::de::DeserializeOwned;

use crate::config::{Config, MIN_WEATHER_INTERVAL};
use crate::pidfile::{self, StopOutcome};

fn stop_existing() {
    stop_autostart_service();

    // Also an instance started outside autostart, e.g. `olsvr run` in a terminal.
    match pidfile::stop_running() {
        Ok(StopOutcome::NotRunning | StopOutcome::Stopped(_)) => {}
        Ok(StopOutcome::StillRunning(pid)) => {
            eprintln!("olsvr (PID {pid}) got SIGTERM but has not exited yet");
        }
        Err(e) => eprintln!("Cannot stop the running olsvr instance: {e}"),
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
        let Some(mut config) = load_or_back_up() else {
            return;
        };
        for problem in config.validate() {
            eprintln!("  {problem}");
        }
        seed_from_clock_layer(&mut config);
        config
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

/// The existing config, or the defaults once a file that does not load has been copied to
/// `~/.olsvr.toml.bak`. `None` when that copy fails, because saving would then destroy the file.
fn load_or_back_up() -> Option<Config> {
    let error = match Config::try_load() {
        Ok(config) => return Some(config),
        Err(e) => e,
    };
    eprintln!("  {error}");
    let backup = Config::path().with_extension("toml.bak");
    if let Err(e) = std::fs::copy(Config::path(), &backup) {
        eprintln!(
            "  Setup stopped: the file could not be copied to {}: {e}",
            backup.display()
        );
        return None;
    }
    eprintln!(
        "  Copied the file to {}. Setup starts from the defaults.",
        backup.display()
    );
    Some(Config::default())
}

/// Sets the flat fields to what the first clock layer renders, so the prompts default to the
/// values in use and `rebuild_layers` writes them back unchanged. A key the layer lacks, or sets
/// to a value that does not fit its field, gets the default the saver falls back to.
fn seed_from_clock_layer(config: &mut Config) {
    let Some(clock) = config
        .layers
        .iter()
        .flatten()
        .find(|t| crate::config::layer_type(t) == "clock")
    else {
        return;
    };
    let defaults = Config::default();
    config.font_size = layer_value(clock, "font_size").unwrap_or(defaults.font_size);
    config.font_family = layer_value(clock, "font_family").unwrap_or(defaults.font_family);
    config.time_format = layer_value(clock, "time_format").unwrap_or(defaults.time_format);
    config.date_format = layer_value(clock, "date_format").unwrap_or(defaults.date_format);
    config.color = layer_value(clock, "color").unwrap_or(defaults.color);
    config.hold = layer_value(clock, "hold").unwrap_or(defaults.hold);
    config.fade_duration = layer_value(clock, "fade_duration").unwrap_or(defaults.fade_duration);
    config.edge_padding = layer_value(clock, "edge_padding").unwrap_or(defaults.edge_padding);
}

fn layer_value<T: DeserializeOwned>(layer: &toml::value::Table, key: &str) -> Option<T> {
    layer.get(key)?.clone().try_into().ok()
}

fn prompt_timing(theme: &ColorfulTheme, config: &mut Config) {
    println!();
    println!("  -- Timing --");
    println!();

    config.timeout = Input::with_theme(theme)
        .with_prompt("Idle timeout (minutes)")
        .default(config.timeout)
        .validate_with(|minutes: &u32| {
            if *minutes >= 1 {
                Ok(())
            } else {
                Err("The timeout must be at least 1 minute")
            }
        })
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

    let mut families = vec!["sans-serif", "serif", "monospace", "cursive", "fantasy"];
    // A family outside the generic ones is offered too, so that Enter keeps it
    if !families.contains(&config.font_family.as_str()) {
        families.push(&config.font_family);
    }
    let current_idx = families
        .iter()
        .position(|f| *f == config.font_family)
        .unwrap_or(0);
    let family_idx = Select::with_theme(theme)
        .with_prompt("Font family")
        .items(&families)
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

    let [r, g, b] = config.color;
    let brightest = r.max(g).max(b);
    let brightness: u8 = Input::with_theme(theme)
        .with_prompt("Brightness (0-255)")
        .default(brightest)
        .interact_text()
        .unwrap();
    // Every component scales with the brightest, so a tint survives and the default changes
    // nothing; black has no tint, so it becomes grey
    config.color = if brightest == 0 {
        [brightness; 3]
    } else {
        config
            .color
            .map(|c| (f32::from(c) * f32::from(brightness) / f32::from(brightest)).round() as u8)
    };

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
        .map_or(600, |seconds| {
            seconds.clamp(MIN_WEATHER_INTERVAL.into(), u32::MAX.into()) as u32
        });
    let interval: u32 = Input::with_theme(theme)
        .with_prompt("Update interval (seconds)")
        .default(default_interval)
        .validate_with(|seconds: &u32| {
            if *seconds >= MIN_WEATHER_INTERVAL {
                Ok(())
            } else {
                Err(format!(
                    "The interval must be at least {MIN_WEATHER_INTERVAL} seconds"
                ))
            }
        })
        .interact_text()
        .unwrap();

    let default_font_size = existing
        .as_ref()
        .and_then(|t| t.get("font_size").and_then(|v| v.as_integer()))
        .unwrap_or(24) as u32;
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
        existing.extend(weather);
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

    let has_clock = config
        .layers
        .as_ref()
        .is_some_and(|l| l.iter().any(|t| crate::config::layer_type(t) == "clock"));

    if !has_weather && !has_clock {
        // Neither layer exists: the flat fields render as the single clock
        config.layers = None;
        return;
    }

    let layers = config.layers.get_or_insert_with(Vec::new);

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
            .find(|t| crate::config::layer_type(t) == "clock")
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
        install_systemd_service(theme);
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
fn install_systemd_service(theme: &ColorfulTheme) {
    let home = std::env::var("HOME").expect("HOME not set");
    let service_dir = format!("{home}/.config/systemd/user");
    let service_path = format!("{service_dir}/olsvr.service");

    let exe = match std::env::current_exe() {
        Ok(exe) => exe.to_string_lossy().into_owned(),
        Err(e) => {
            eprintln!("Cannot find the path of the olsvr binary: {e}");
            return;
        }
    };
    if exe.contains("/target/") {
        eprintln!("  Warning: {exe} is inside a cargo target directory.");
        eprintln!(
            "  The service would run that build, and fail to start once the directory is cleaned."
        );
        eprintln!("  Run cargo install and set up from the installed olsvr to avoid that.");
        let proceed = Select::with_theme(theme)
            .with_prompt("Install the service with this binary anyway?")
            .items(["No", "Yes"])
            .default(0)
            .interact()
            .unwrap();
        if proceed == 0 {
            println!("  Service not installed.");
            return;
        }
    }

    if let Err(e) = std::fs::create_dir_all(&service_dir) {
        eprintln!("Failed to create {service_dir}: {e}");
        return;
    }

    // ExecStart expands the specifier % even inside quotes.
    let exec_start = exe.replace('%', "%%");

    // Keep olsvr.service, the documentation copy of this unit, in sync. RestartPreventExitStatus
    // lists the exits a restart cannot fix: no Wayland compositor, and already running.
    let unit = format!(
        "[Unit]\n\
         Description=OLED Screensaver\n\
         After=graphical-session.target\n\
         PartOf=graphical-session.target\n\
         StartLimitIntervalSec=60\n\
         StartLimitBurst=5\n\
         \n\
         [Service]\n\
         Type=exec\n\
         Environment=RUST_LOG=info\n\
         UnsetEnvironment=DISPLAY XAUTHORITY\n\
         ExecStart=\"{exec_start}\" run\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         RestartPreventExitStatus={} {}\n\
         \n\
         [Install]\n\
         WantedBy=graphical-session.target\n",
        crate::EXIT_NO_COMPOSITOR,
        crate::EXIT_ALREADY_RUNNING
    );
    if let Err(e) = std::fs::write(&service_path, unit) {
        eprintln!("Failed to write service file: {e}");
        return;
    }
    println!("  Installed service to {service_path}");

    enable_and_restart_service();
    print_session_note();
}

#[cfg(target_os = "linux")]
fn enable_and_restart_service() {
    let steps: [&[&str]; 3] = [
        &["daemon-reload"],
        &["enable", "olsvr.service"],
        &["restart", "olsvr.service"],
    ];
    for args in steps {
        let failure = match Command::new("systemctl").arg("--user").args(args).status() {
            Ok(status) if status.success() => continue,
            Ok(status) => status.to_string(),
            Err(e) => e.to_string(),
        };
        eprintln!("  systemctl --user {} failed: {failure}", args.join(" "));
        eprintln!("  The service was not started. Fix the error above and run setup again.");
        return;
    }
    report_service_state();
}

/// With `Type=exec` the restart returns as soon as the binary has started, so a daemon that
/// exits at once only shows after a short wait.
#[cfg(target_os = "linux")]
fn report_service_state() {
    std::thread::sleep(std::time::Duration::from_secs(2));
    let shown = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "-p",
            "ActiveState,Result,NRestarts",
            "olsvr.service",
        ])
        .output();
    let Some(shown) = shown.ok().filter(|output| output.status.success()) else {
        eprintln!(
            "  Could not read the service state. Check it with: systemctl --user status olsvr.service"
        );
        return;
    };
    let shown = String::from_utf8_lossy(&shown.stdout);
    let property = |name: &str| {
        shown
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
            .unwrap_or("unknown")
    };
    let (active, result, restarts) = (
        property("ActiveState"),
        property("Result"),
        property("NRestarts"),
    );
    if active == "active" && result == "success" && restarts == "0" {
        println!("  Service enabled and running.");
    } else {
        eprintln!(
            "  The service did not stay up: ActiveState={active}, Result={result}, NRestarts={restarts}"
        );
        eprintln!("  See why with: journalctl --user -u olsvr.service -e");
    }
}

/// GNOME and KDE bring up graphical-session.target, which the unit is bound to; a bare
/// compositor does not.
#[cfg(target_os = "linux")]
fn print_session_note() {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_lowercase();
    let is = |name: &str| desktop.split(':').any(|part| part == name);
    if is("gnome") || is("kde") {
        return;
    }
    println!();
    println!(
        "  The service starts with graphical-session.target, which this desktop may not bring up."
    );
    if is("sway") || std::env::var_os("SWAYSOCK").is_some() {
        println!("  Start olsvr from your sway config instead:");
        println!("      exec olsvr run");
    } else if is("hyprland") || std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        println!("  Start olsvr from your Hyprland config instead:");
        println!("      exec-once = olsvr run");
    } else {
        println!("  Starting your compositor through uwsm brings it up.");
    }
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
  <key>CFBundleShortVersionString</key>
  <string>{version}</string>
  <key>CFBundleVersion</key>
  <string>{version}</string>
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
        version = env!("CARGO_PKG_VERSION"),
    );
    if let Err(e) = std::fs::write(&info_dst, info) {
        eprintln!("Failed to write app Info.plist: {e}");
        return None;
    }

    // Ad-hoc codesign the bundle. The linker's signature on the bare binary fails
    // validation once it is a bundle's main executable, so the agent gets
    // AMFI-killed at launch (SIGKILL, OS_REASON_CODESIGNING) unless re-signed in
    // the bundle context.
    let signed = Command::new("codesign")
        .args(["--force", "--sign", "-", &app_dir])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !signed {
        eprintln!(
            "Warning: codesign failed for {app_dir}; the login agent may be killed at launch"
        );
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

    // KeepAlive true: launchd respawns the agent after any exit, so a quit event cannot
    // leave it dead; respawns are at least ThrottleInterval apart. `olsvr stop` and
    // setup still stop it because both run `launchctl bootout` first.
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
  <true/>
  <key>ProcessType</key>
  <string>Interactive</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>RUST_LOG</key>
    <string>info</string>
  </dict>
  <key>StandardOutPath</key>
  <string>{home}/Library/Logs/olsvr.log</string>
  <key>StandardErrorPath</key>
  <string>{home}/Library/Logs/olsvr.log</string>
  <key>ThrottleInterval</key>
  <integer>30</integer>
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
