<div align="center">

# olsvr

**OLED screensaver for Wayland and macOS — fade-animated clock on pure black**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2024-orange?style=flat-square)](https://www.rust-lang.org)
[![Wayland](https://img.shields.io/badge/Wayland-native-blueviolet?style=flat-square)](#)
[![macOS](https://img.shields.io/badge/macOS-Metal-black?style=flat-square)](#)

</div>

---

olsvr prevents OLED burn-in by displaying a fading clock that periodically repositions itself on a pure black background. It activates after a configurable idle timeout, renders with GPU acceleration via wgpu, and dismisses on any keyboard or mouse input. It runs natively on Linux (Wayland, via Vulkan) and macOS (via Metal) behind a shared, platform-agnostic core.

![Demo](docs/demo.webp)

### Contents

- [Features](#features) · [Quick Start](#quick-start) · [Setup Wizard](#setup-wizard) · [Configuration](#configuration)
- [How It Works](#how-it-works) · [Requirements](#requirements) · [Contributing](#contributing)

---

## Features

- **Cross-platform** — native Linux (Wayland) and macOS backends behind a shared core
- **Idle detection** — `ext-idle-notify-v1` or D-Bus (GNOME) on Wayland, CoreGraphics event idle on macOS, after a configurable timeout
- **Fade animation** — clock fades in, holds, fades out, then teleports to a new position
- **Quadrant-aware repositioning** — never lands in the same screen quadrant twice in a row
- **Multi-display** — cover just the primary display or every connected display
- **Weather overlay** — colored Nerd Font weather icons with temperature and wind speed via [FMI Open Data](https://en.ilmatieteenlaitos.fi/open-data)
- **GPU-accelerated** — wgpu + glyphon text rendering (Vulkan on Wayland, Metal on macOS)
- **Configurable** — font, size, color, timing, date/time format, activation policy via `~/.olsvr.toml`
- **Interactive setup wizard** — `olsvr setup` walks through configuration with a live preview
- **Autostart** — installs a systemd user service (Linux) or a launchd login agent with no Dock icon (macOS)
- **Keep-awake** — holds a display-sleep inhibitor while active, or for the whole session (Wayland idle-inhibit / macOS `IOPMAssertion`)
- **Manual trigger** — `olsvr run --activate` signals a running instance to show now (Wayland)
- **Video-aware** — respects screensaver inhibitors such as video playback (Wayland)
- **Input dismissal** — any key press or mouse movement deactivates immediately

---

## Quick Start

```bash
# Build and install
cargo install --path .

# Run the setup wizard (first-run default)
olsvr setup

# Or run directly with defaults
olsvr run

# Trigger a running instance to show immediately
olsvr run --activate

# Preview the screensaver (shows instantly, dismiss with any input)
olsvr run --now

# Stop a running instance
olsvr stop
```

> **macOS:** if `olsvr` isn't found after `cargo install`, add `~/.cargo/bin` to your `PATH` — rustup does this automatically, but Homebrew's cargo does not.

---

## Setup Wizard

Running `olsvr setup` (or just `olsvr` on first run) starts an interactive wizard that guides you through:

1. **Timing** — idle timeout, hold duration, fade speed
2. **Display** — font size, family, time/date format, brightness, padding
3. **Weather** — enable/disable weather overlay, location, icon font
4. **Preview** — launch a live preview to see your settings
5. **Autostart** — optionally install a systemd user service (Linux) or a launchd login agent (macOS) that starts on login

The wizard writes `~/.olsvr.toml` and can be re-run at any time to adjust settings.

---

## Configuration

Settings live in `~/.olsvr.toml`. All fields are optional — defaults are used for anything omitted. The setup wizard (`olsvr setup`) is the easiest way to configure everything.

### Clock

| Field | Description | Default |
|---|---|---|
| `timeout` | Idle timeout in minutes | `5` |
| `hold` | Hold duration between fades (seconds) | `10` |
| `fade_duration` | Fade in/out duration (milliseconds) | `1500` |
| `font_size` | Clock font size (pixels) | `200` |
| `font_family` | Font family (`sans-serif`, `serif`, `monospace`, or a font name) | `sans-serif` |
| `time_format` | `24h` or `12h` | `24h` |
| `date_format` | [chrono format string](https://docs.rs/chrono/latest/chrono/format/strftime/) | `%a %d %b` |
| `color` | RGB brightness `[r, g, b]` | `[255, 255, 255]` |
| `edge_padding` | Minimum distance from screen edges (pixels) | `50` |

### Weather

The weather overlay shows a colored icon, temperature, and wind speed below the clock using [FMI Open Data](https://en.ilmatieteenlaitos.fi/open-data). Icons are Nerd Font weather glyphs — install a [Nerd Font](https://www.nerdfonts.com/) with weather icons (e.g. MesloLGS NF).

| Field | Description | Default |
|---|---|---|
| `location` | City name for FMI weather data | `helsinki` |
| `update_interval` | Weather fetch interval (seconds) | `600` |
| `font_size` | Weather text size (pixels) | `28` |
| `icon_font` | Nerd Font family for weather icons | `MesloLGS NF` |

### Activation

The `[activation]` table controls which displays the screensaver covers and how it keeps the panel awake. The defaults preserve the original single-display, inhibit-while-active behavior.

| Field | Description | Default |
|---|---|---|
| `displays` | `primary` (primary display only) or `all` (every connected display). Multi-display is currently macOS; the Wayland backend renders a single fullscreen surface. | `primary` |
| `keep_awake` | `while-active` (inhibit display sleep only while showing), `always` (keep the panel awake for the whole session), or `off` | `while-active` |

### Example

```toml
timeout = 5

[[layers]]
type = "clock"
font_size = 200
font_family = "sans-serif"
time_format = "24h"
date_format = "%a %d %b"
color = [200, 200, 200]
hold = 10
fade_duration = 1500
edge_padding = 50

[[layers]]
type = "weather"
location = "helsinki"
update_interval = 600
font_size = 28
icon_font = "MesloLGS NF"

[activation]
displays = "all"
keep_awake = "always"
```

### CLI flags

CLI flags override config file values for the current run.

| Flag | Description |
|---|---|
| `--timeout <MINUTES>` | Idle timeout before activation |
| `--font-size <PIXELS>` | Clock font size |
| `--hold <SECONDS>` | Hold duration between fades |
| `--fade-duration <MS>` | Fade in/out duration |
| `--edge-padding <PIXELS>` | Minimum distance from screen edges |
| `--now` | Show immediately, skip idle wait |
| `--activate` | Send SIGUSR1 to a running instance |
| `--debug` | Show debug overlay (frame count, timing) |

---

## How It Works

```mermaid
stateDiagram-v2
    [*] --> Idle: Launch
    Idle --> FadeIn: Idle timeout / SIGUSR1
    FadeIn --> Hold: Fade complete
    Hold --> FadeOut: Hold duration elapsed
    FadeOut --> FadeIn: Repositioned
    FadeIn --> [*]: Key / mouse input
    Hold --> [*]: Key / mouse input
    FadeOut --> [*]: Key / mouse input
```

The animation cycle runs continuously while the screensaver is active:

1. **Fade in** — clock appears at a random position, opacity rises from 0 to 255
2. **Hold** — clock stays visible at full brightness
3. **Fade out** — opacity drops back to 0
4. **Reposition** — clock teleports to a new quadrant (never the same one twice in a row)

The screensaver creates a fullscreen black surface on each target display — a Wayland window on Linux, or a borderless `NSWindow` at screen-saver level on macOS. A display-sleep inhibitor keeps the panel awake while active. Any keyboard or mouse input dismisses it immediately.

---

## Requirements

- **Rust toolchain** for building from source
- **Nerd Font** (optional, for weather icons) — install from [nerdfonts.com](https://www.nerdfonts.com/)

**Linux (Wayland):**

- A Wayland compositor — GNOME, Sway, Hyprland, etc.
- A GPU with Vulkan support (wgpu backend)
- **libdbus** (for the GNOME D-Bus idle fallback) — `libdbus-1-dev` on Debian/Ubuntu, `dbus-devel` on Fedora

**macOS:**

- macOS 11 (Big Sur) or newer — renders with Metal via wgpu
- No extra dependencies

> On Wayland, idle detection uses `ext-idle-notify-v1` (Sway, Hyprland) with an automatic D-Bus fallback for GNOME/Mutter, plus a manual `olsvr run --activate` trigger. On macOS, idle is polled from CoreGraphics and the login agent runs without a Dock icon.

---

## Contributing

Contributions are welcome! Open an issue or submit a PR.

```bash
git clone https://github.com/laveez/olsvr.git
cd olsvr
git config core.hooksPath .githooks  # Enable pre-push fmt/clippy checks
cargo build
cargo run -- run --now               # Preview the screensaver
cargo run -- run --now --debug       # Preview with debug overlay
RUST_LOG=info cargo run -- run       # Run with logging
```

## License

[MIT](LICENSE)
