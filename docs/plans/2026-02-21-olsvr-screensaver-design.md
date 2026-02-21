# olsvr — OLED Screensaver Design

## Overview

A Rust screensaver for Wayland that displays a fading clock (time + date) on a pure black background to prevent OLED burn-in. Activates automatically after a configurable idle timeout via the ext-idle-notify-v1 Wayland protocol. Deactivates on any input. Prevents the system from sleeping while active.

## Target Environment

- Ubuntu 25.10, GNOME Shell 49, Mutter 49.0
- Wayland (no X11 fallback needed)
- Display: 5120x1440 @ 240Hz (Samsung G9-class ultrawide OLED)
- Single monitor on DP-1

## Tech Stack

- **Language:** Rust
- **Wayland integration:** smithay-client-toolkit (SCTK)
- **GPU rendering:** wgpu
- **Text rendering:** glyphon or wgpu_text
- **Idle detection:** ext-idle-notify-v1 (Wayland protocol, supported by Mutter 49+)
- **Sleep prevention:** zwp_idle_inhibit_manager_v1 (Wayland protocol)

## Application States

```
             idle timeout
  Hidden ──────────────────► Visible (fullscreen, black + clock)
    ▲                              │
    │         any input            │
    └──────────────────────────────┘
```

- **Hidden:** No window/surface. App is an idle listener in the SCTK event loop.
- **Visible:** Fullscreen black window with fading clock. Idle inhibitor active.

## Rendering

- **Background:** Pure black (#000000) — OLED pixels off
- **Clock text:** HH:MM in large sans-serif (~200px), white on black
- **Date text:** Fri 21 Feb below the time (~60px)

### Animation Cycle

1. **Fade in** at a random position (0% → 100% opacity over ~1.5s)
2. **Hold** fully visible for ~8-10s
3. **Fade out** (100% → 0% opacity over ~1.5s)
4. **Teleport** to a new random position (while invisible)
5. Repeat

### Position Selection

Random x,y ensuring clock stays within screen bounds with padding. Avoid repeating the same quadrant twice in a row for better burn-in distribution.

### Frame Rate

~30fps during fade transitions. Can drop to ~1fps during hold phase (only update if minute changes).

## Idle Detection

- Register ext-idle-notify-v1 notification with configurable timeout (default 5 min)
- On `idled` event → create fullscreen surface, start rendering, create idle inhibitor
- On `resumed` event → destroy surface and inhibitor, return to idle listening

## Sleep Prevention

- Use zwp_idle_inhibit_manager_v1 to create an idle inhibitor on the screensaver surface
- Created when screensaver activates, destroyed when it deactivates
- Prevents compositor from triggering screen-off or suspend

## Configuration

CLI arguments (no config file):

| Argument | Default | Description |
|----------|---------|-------------|
| `--timeout <min>` | 5 | Idle timeout before activation |
| `--activate` | - | Immediately activate (manual trigger) |
| `--font-size <px>` | 200 | Clock font size |
| `--speed <sec>` | 10 | Hold duration between fades |

## Process Model

Single long-running process:
1. Connect to Wayland compositor via SCTK
2. Register ext-idle-notify-v1 watch
3. Enter event loop (idle, low CPU)
4. On idle → create fullscreen surface, start rendering, create idle inhibitor
5. On input → destroy surface and inhibitor, return to step 3

## Manual Trigger

`olsvr --activate` sends a Unix signal (SIGUSR1) to the running instance to force activation. The running process writes its PID to a known location (e.g., `/run/user/$UID/olsvr.pid`).

## Autostart

Installed as a systemd user service (`olsvr.service`) that starts on login.

## Dependencies

Rust crates:
- `smithay-client-toolkit` — Wayland client with SCTK abstractions
- `wgpu` — GPU rendering
- `glyphon` — GPU text rendering
- `clap` — CLI argument parsing
- `chrono` — time/date formatting
- `rand` — random position generation
