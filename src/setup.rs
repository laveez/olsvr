use std::process::Command;

use dialoguer::{Input, Select, theme::ColorfulTheme};
use serde::de::DeserializeOwned;

use crate::config::{Config, MIN_WEATHER_INTERVAL};
use crate::pidfile::{self, StopOutcome};

/// An instance that still holds the PID lock would show its own saver in place of the preview
/// (Linux) or refuse the preview (macOS). `false` when it does not stop.
fn stop_running_instance(autostart_was_active: bool) -> bool {
    match pidfile::stop_running() {
        Ok(StopOutcome::NotRunning) => true,
        Ok(StopOutcome::Stopped(pid)) => {
            // Right after the autostart service stopped, this is that instance still exiting
            if !autostart_was_active {
                println!(
                    "  Stopped olsvr (PID {pid}), which ran outside autostart. Setup does not start it again."
                );
            }
            true
        }
        Ok(StopOutcome::StillRunning(pid)) => {
            eprintln!("  olsvr (PID {pid}) got SIGTERM but has not exited yet, so no preview.");
            false
        }
        Err(e) => {
            eprintln!("  Cannot stop the running olsvr instance, so no preview: {e}");
            false
        }
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
    let _ = Command::new("launchctl")
        .args(["bootout", &launchd_target()])
        .output();
    // `bootout` can return while launchd is still unloading the job, and `bootstrap` fails then
    for _ in 0..20 {
        if !autostart_active() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

#[cfg(target_os = "linux")]
fn autostart_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "olsvr.service"])
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(target_os = "macos")]
fn autostart_active() -> bool {
    launchctl(&["print", &launchd_target()]).is_ok()
}

#[cfg(target_os = "linux")]
fn start_autostart_service() -> bool {
    if systemctl_user(&["start", "olsvr.service"]) {
        return true;
    }
    eprintln!("  The service is stopped.");
    false
}

#[cfg(target_os = "macos")]
fn start_autostart_service() -> bool {
    let domain = launchd_domain();
    let plist_path = launchd_plist_path();
    let mut failure = String::new();
    // A job that is already loaded gives the same "Input/output error" as one still unloading
    for _ in 0..5 {
        match launchctl(&["bootstrap", &domain, &plist_path]) {
            Ok(()) => return true,
            Err(e) => failure = e,
        }
        if autostart_active() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    // Older macOS without `bootstrap`: fall back to legacy load.
    if launchctl(&["load", "-w", &plist_path]).is_ok() {
        return true;
    }
    eprintln!("  launchctl bootstrap failed: {failure}");
    eprintln!(
        "  The login agent is not loaded. Load it with: launchctl bootstrap {domain} {plist_path}"
    );
    false
}

/// Runs `systemctl --user` and prints what failed.
#[cfg(target_os = "linux")]
fn systemctl_user(args: &[&str]) -> bool {
    let failure = match Command::new("systemctl").arg("--user").args(args).status() {
        Ok(status) if status.success() => return true,
        Ok(status) => status.to_string(),
        Err(e) => e.to_string(),
    };
    eprintln!("  systemctl --user {} failed: {failure}", args.join(" "));
    false
}

#[cfg(target_os = "macos")]
fn launchctl(args: &[&str]) -> Result<(), String> {
    match Command::new("launchctl").args(args).output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(target_os = "macos")]
fn launchd_domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

#[cfg(target_os = "macos")]
fn launchd_target() -> String {
    format!("{}/{}", launchd_domain(), crate::LAUNCHD_LABEL)
}

#[cfg(target_os = "macos")]
fn launchd_plist_path() -> String {
    let home = std::env::var("HOME").expect("HOME not set");
    format!("{home}/Library/LaunchAgents/{}.plist", crate::LAUNCHD_LABEL)
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
    if !wizard() {
        std::process::exit(1);
    }
}

/// `false` when a step failed, which the step has already said.
fn wizard() -> bool {
    let theme = ColorfulTheme::default();

    println!();
    println!("  olsvr setup");
    println!("  OLED screensaver");
    println!();

    check_prerequisites();

    let mut config = if Config::exists() {
        println!("  Found existing config at {}", Config::path().display());
        println!();
        let Some(mut config) = load_or_back_up() else {
            return false;
        };
        for problem in config.validate() {
            eprintln!("  {problem}");
        }
        seed_from_clock_layer(&mut config);
        config
    } else {
        Config::default()
    };

    let mut ok = true;
    loop {
        prompt_timing(&theme, &mut config);
        prompt_display(&theme, &mut config);
        prompt_weather(&theme, &mut config);
        rebuild_layers(&mut config);

        if let Err(e) = config.save() {
            eprintln!("Failed to save config: {e}");
            return false;
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
            ok &= run_preview();
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
                    return false;
                }
                continue;
            }
            3 => {
                prompt_weather(&theme, &mut config);
                rebuild_layers(&mut config);
                if let Err(e) = config.save() {
                    eprintln!("Failed to save config: {e}");
                    return false;
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

    ok &= offer_autostart_install(&theme);
    if ok {
        println!();
        println!("  Setup complete!");
        println!();
    }
    ok
}

/// Shows the preview with the autostart service stopped, and starts the service again after.
/// `false` when the service does not come back.
fn run_preview() -> bool {
    let _interrupts = IgnoreInterrupts::new();
    let stopped = StoppedAutostart::stop();
    if stop_running_instance(stopped.was_active) {
        show_preview();
    }
    stopped.restart()
}

fn show_preview() {
    let exe = std::env::current_exe().unwrap_or_else(|_| "olsvr".into());
    let mut child = match Command::new(exe).args(["run", "--now"]).spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("  Cannot start the preview: {e}");
            return;
        }
    };
    println!("  Preview running. Press Enter here to end the preview.");
    println!();
    let _ = std::io::stdin().read_line(&mut String::new());
    let _ = child.kill();
    let _ = child.wait();
}

extern "C" fn ignore_signal(_: libc::c_int) {}

const INTERRUPTS: [libc::c_int; 2] = [libc::SIGINT, libc::SIGHUP];

/// Ctrl-C and a closing terminal reach the wizard and the preview together. While the autostart
/// service is stopped, only the preview may end on them, so that the wizard can start the
/// service again; after a hangup the blocked terminal read fails at once. A handler rather than
/// SIG_IGN, because exec resets a handler and the preview keeps the default.
struct IgnoreInterrupts([libc::sighandler_t; 2]);

impl IgnoreInterrupts {
    fn new() -> Self {
        Self(INTERRUPTS.map(|signal| unsafe {
            libc::signal(signal, ignore_signal as *const () as libc::sighandler_t)
        }))
    }
}

impl Drop for IgnoreInterrupts {
    fn drop(&mut self) {
        for (signal, previous) in INTERRUPTS.into_iter().zip(self.0) {
            unsafe { libc::signal(signal, previous) };
        }
    }
}

/// The autostart service, stopped if it ran. Dropping this starts it again, also when setup
/// unwinds from a panic, such as a print to a terminal that has closed.
struct StoppedAutostart {
    was_active: bool,
}

impl StoppedAutostart {
    fn stop() -> Self {
        let was_active = autostart_active();
        if was_active {
            stop_autostart_service();
        }
        Self { was_active }
    }

    /// `false` when the service ran and does not come back.
    fn restart(mut self) -> bool {
        let was_active = std::mem::take(&mut self.was_active);
        !was_active || start_autostart_service()
    }

    /// Leaves starting the service to the caller.
    #[cfg(target_os = "macos")]
    fn disarm(mut self) {
        self.was_active = false;
    }
}

impl Drop for StoppedAutostart {
    fn drop(&mut self) {
        if self.was_active {
            start_autostart_service();
        }
    }
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

/// Restarts the autostart service, so that it reads the saved config.
fn restart_autostart() -> bool {
    let _interrupts = IgnoreInterrupts::new();
    stop_autostart_service();
    // An instance still exiting holds the PID lock, and the new one would be refused
    let _ = pidfile::stop_running();
    start_autostart_service()
}

/// What an autostart that is not reinstalled needs: a running service restarts to read the saved
/// config, any other state stays as it is and is reported.
fn refresh_autostart() -> bool {
    if !autostart_active() {
        return report_autostart_state();
    }
    println!("  Restarting autostart to apply the new config.");
    if !restart_autostart() || !report_service_state() {
        return false;
    }
    println!("  Autostart is running with the new config.");
    true
}

/// `false` when the state cannot be read.
#[cfg(target_os = "linux")]
fn report_autostart_state() -> bool {
    let loaded = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "-p",
            "LoadState",
            "--value",
            "olsvr.service",
        ])
        .output()
        .ok()
        .filter(|output| output.status.success());
    match loaded
        .as_ref()
        .map(|output| String::from_utf8_lossy(&output.stdout))
    {
        Some(state) if state.trim() == "not-found" => println!("  Autostart is not installed."),
        Some(_) => println!(
            "  Autostart is installed but not running. Start it with: systemctl --user start olsvr.service"
        ),
        None => {
            eprintln!(
                "  Could not read the service state. Check it with: systemctl --user status olsvr.service"
            );
            return false;
        }
    }
    true
}

#[cfg(target_os = "macos")]
fn report_autostart_state() -> bool {
    let plist_path = launchd_plist_path();
    if std::path::Path::new(&plist_path).exists() {
        println!("  The login agent is installed but not loaded, so it starts at the next login.");
        println!(
            "  Load it now with: launchctl bootstrap {} {plist_path}",
            launchd_domain()
        );
    } else {
        println!("  Autostart is not installed.");
    }
    true
}

/// Writes `contents` to a file beside `path` and renames it over `path`, so that a failed write
/// leaves the old file whole. A symlink at `path`, such as one from a dotfiles manager, stays,
/// and the file it points to is replaced.
fn write_replacing(path: &str, contents: &str) -> std::io::Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.into());
    let mut staged = target.clone().into_os_string();
    staged.push(".new");
    let written =
        std::fs::write(&staged, contents).and_then(|()| std::fs::rename(&staged, &target));
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    written
}

#[cfg(target_os = "linux")]
fn offer_autostart_install(theme: &ColorfulTheme) -> bool {
    println!();
    let install = Select::with_theme(theme)
        .with_prompt("Install or refresh autostart (systemd user service)?")
        .items(["Yes", "No"])
        .default(0)
        .interact()
        .unwrap();
    if install == 0 {
        install_systemd_service(theme)
    } else {
        refresh_autostart()
    }
}

#[cfg(target_os = "macos")]
fn offer_autostart_install(theme: &ColorfulTheme) -> bool {
    println!();
    let install = Select::with_theme(theme)
        .with_prompt("Install or refresh autostart (login agent, no Dock icon)?")
        .items(["Yes", "No"])
        .default(0)
        .interact()
        .unwrap();
    if install == 0 {
        install_launchd_agent()
    } else {
        refresh_autostart()
    }
}

#[cfg(target_os = "linux")]
fn install_systemd_service(theme: &ColorfulTheme) -> bool {
    let home = std::env::var("HOME").expect("HOME not set");
    let service_dir = format!("{home}/.config/systemd/user");
    let service_path = format!("{service_dir}/olsvr.service");

    let exe = match std::env::current_exe() {
        Ok(exe) => exe.to_string_lossy().into_owned(),
        Err(e) => {
            eprintln!("Cannot find the path of the olsvr binary: {e}");
            return false;
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
            return refresh_autostart();
        }
    }

    if let Err(e) = std::fs::create_dir_all(&service_dir) {
        eprintln!("Failed to create {service_dir}: {e}");
        return false;
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
    if let Err(e) = write_replacing(&service_path, &unit) {
        eprintln!("Failed to write service file: {e}");
        return false;
    }
    println!("  Installed service to {service_path}");

    // The service's instance exits while another one holds the PID lock
    if !autostart_active()
        && let Ok(StopOutcome::Stopped(pid)) = pidfile::stop_running()
    {
        println!(
            "  Stopped olsvr (PID {pid}), which ran outside autostart; the service takes over."
        );
    }

    let running = enable_and_restart_service();
    print_session_note();
    running
}

#[cfg(target_os = "linux")]
fn enable_and_restart_service() -> bool {
    let steps: [&[&str]; 3] = [
        &["daemon-reload"],
        &["enable", "olsvr.service"],
        &["restart", "olsvr.service"],
    ];
    if !steps.iter().all(|args| systemctl_user(args)) {
        eprintln!("  The service was not started. Fix the error above and run setup again.");
        return false;
    }
    let running = report_service_state();
    if running {
        println!("  Service enabled and running.");
    }
    running
}

/// With `Type=exec` the restart returns as soon as the binary has started, so a daemon that
/// exits at once only shows after a short wait.
#[cfg(target_os = "linux")]
fn report_service_state() -> bool {
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
        return false;
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
        return true;
    }
    eprintln!(
        "  The service did not stay up: ActiveState={active}, Result={result}, NRestarts={restarts}"
    );
    eprintln!("  See why with: journalctl --user -u olsvr.service -e");
    false
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
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::process::CommandExt;

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
    // its Info.plist; re-run setup after upgrading to refresh the copy. The copy is renamed
    // over the executable instead of written onto it: a write rewrites the same inode in place,
    // under the code signature macOS has cached for it.
    let src = std::env::current_exe().ok()?;
    let same_file = match (std::fs::metadata(&src), std::fs::metadata(&exe_dst)) {
        (Ok(from), Ok(to)) => from.dev() == to.dev() && from.ino() == to.ino(),
        _ => false,
    };
    if !same_file {
        let staged = format!("{exe_dst}.new");
        let _ = std::fs::remove_file(&staged);
        let copied = std::fs::copy(&src, &staged).and_then(|_| std::fs::rename(&staged, &exe_dst));
        if let Err(e) = copied {
            let _ = std::fs::remove_file(&staged);
            eprintln!("Failed to copy binary into app bundle: {e}");
            return None;
        }
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
    // the bundle context. It runs in its own process group, so that a Ctrl-C, which the wizard
    // ignores here, cannot cut the signing short.
    let signed = Command::new("codesign")
        .args(["--force", "--sign", "-", &app_dir])
        .process_group(0)
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
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(target_os = "macos")]
fn install_launchd_agent() -> bool {
    let home = std::env::var("HOME").expect("HOME not set");
    let label = crate::LAUNCHD_LABEL;
    let agents_dir = format!("{home}/Library/LaunchAgents");
    let plist_path = launchd_plist_path();

    if let Err(e) = std::fs::create_dir_all(&agents_dir) {
        eprintln!("Failed to create {agents_dir}: {e}");
        return false;
    }

    // The agent runs from the bundle, so it stops before the bundle changes
    let _interrupts = IgnoreInterrupts::new();
    let stopped = StoppedAutostart::stop();
    if autostart_active() {
        eprintln!("  The login agent is still loaded, so setup left the app bundle as it was.");
        eprintln!("  Unload it with: launchctl bootout {}", launchd_target());
        return false;
    }

    // Launch from the LSUIElement app bundle so the agent has no Dock icon; fall
    // back to the bare installed binary if the bundle couldn't be created.
    let exe = create_app_bundle().unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{home}/.cargo/bin/olsvr"))
    });

    let exe = xml_escape(&exe);
    let log_path = xml_escape(&format!("{home}/Library/Logs/olsvr.log"));

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
  <string>{log_path}</string>
  <key>StandardErrorPath</key>
  <string>{log_path}</string>
  <key>ThrottleInterval</key>
  <integer>30</integer>
</dict>
</plist>
"#
    );

    if let Err(e) = write_replacing(&plist_path, &plist) {
        eprintln!("Failed to write launchd plist: {e}");
        return false;
    }
    println!("  Installed agent to {plist_path}");

    // Reload: bootout any previous instance, then bootstrap into the GUI domain, whether or not
    // an agent ran before.
    stopped.disarm();
    if !restart_autostart() || !report_service_state() {
        return false;
    }
    println!("  Agent enabled and started.");
    true
}

/// Waits for the agent to run. The first launch of a freshly signed bundle can take a few seconds.
#[cfg(target_os = "macos")]
fn report_service_state() -> bool {
    for _ in 0..20 {
        if agent_running() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    eprintln!("  The login agent is loaded, but olsvr is not running.");
    eprintln!(
        "  See ~/Library/Logs/olsvr.log, or: launchctl print {}",
        launchd_target()
    );
    false
}

/// launchd shows a `pid` line for a job it has started. The PID lock is the second opinion,
/// which an agent that still runs a build from before the lock does not give. Its holder counts
/// only when launchd, PID 1, is its parent, because an `olsvr run` started by hand holds the lock
/// too.
#[cfg(target_os = "macos")]
fn agent_running() -> bool {
    let printed = Command::new("launchctl")
        .args(["print", &launchd_target()])
        .output();
    let has_pid = printed.is_ok_and(|output| {
        String::from_utf8_lossy(&output.stdout).lines().any(|line| {
            line.trim()
                .strip_prefix("pid = ")
                .is_some_and(|pid| pid.parse::<u32>().is_ok())
        })
    });
    has_pid
        || matches!(pidfile::running_pid(&pidfile::path()), Ok(Some(pid)) if parent_pid(pid) == Some(1))
}

/// The parent of process `pid`, or `None` if it cannot be read, e.g. because the process has
/// exited.
#[cfg(target_os = "macos")]
fn parent_pid(pid: libc::pid_t) -> Option<libc::pid_t> {
    // SAFETY: proc_bsdinfo holds only integers and byte arrays, so all zeros is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is writable for the `size` bytes passed.
    let written =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    (written == size).then_some(info.pbi_ppid as libc::pid_t)
}
