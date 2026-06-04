//! Platform backends. Each backend owns the platform event loop and renders the
//! screensaver. The Wayland backend is Linux-only; a macOS backend (winit/Cocoa)
//! is added in a later phase.

#[cfg(target_os = "linux")]
pub mod wayland;
