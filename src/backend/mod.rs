//! Platform backends. Each backend owns the platform event loop and renders the
//! screensaver. The Wayland backend is Linux-only; a macOS backend (winit/Cocoa)
//! is added in a later phase.

use crate::config::Config;

#[cfg(target_os = "linux")]
pub mod wayland;

/// A platform backend owns the event loop: it builds an `Engine` from the config,
/// translates platform events into engine events, and executes the engine's
/// commands (show/hide surfaces, keep-awake, quit). Implemented per platform.
#[allow(dead_code)]
pub trait Backend {
    fn run(self: Box<Self>, config: Config);
}
