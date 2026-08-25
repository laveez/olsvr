//! Platform backends. Each backend owns the platform event loop and renders the
//! screensaver: the Wayland backend on Linux, the macOS backend (winit/Cocoa) on
//! macOS. `main.rs` selects the active one by `cfg`.

#[cfg(target_os = "linux")]
pub mod wayland;

#[cfg(target_os = "macos")]
pub mod macos;
