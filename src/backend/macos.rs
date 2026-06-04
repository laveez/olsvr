//! macOS backend: a winit fullscreen window per display rendered by the portable
//! `Compositor`, driven by the shared `Engine`. Idle is detected by polling
//! `CGEventSourceSecondsSinceLastEventType`; the display is kept awake with an
//! `IOPMAssertion` (the OLED-stays-awake / HDMI-redetect fix).

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};
use objc2_app_kit::{NSCursor, NSView, NSWindowCollectionBehavior};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Fullscreen, Window, WindowId, WindowLevel};

use crate::RunArgs;
use crate::compositor::Compositor;
use crate::config::{Config, DisplayScope};
use crate::data::{self, DataCache};
use crate::engine::{BackendEvent, Engine, EngineCommand};

// ---- FFI: idle time (CoreGraphics) + display-sleep assertion (IOKit) ----

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(state_id: i32, event_type: u32) -> f64;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: CFStringRef,
        assertion_level: u32,
        assertion_name: CFStringRef,
        assertion_id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(assertion_id: u32) -> i32;
}

/// Seconds since the last HID input, system-wide.
fn idle_seconds() -> f64 {
    // kCGEventSourceStateCombinedSessionState = 0, kCGAnyInputEventType = !0
    unsafe { CGEventSourceSecondsSinceLastEventType(0, u32::MAX) }
}

/// Acquire a "prevent the display from idle-sleeping" assertion.
fn acquire_keep_awake() -> Option<u32> {
    let assertion_type = CFString::new("PreventUserIdleDisplaySleep");
    let name = CFString::new("olsvr OLED screensaver");
    let mut id: u32 = 0;
    let ok = unsafe {
        IOPMAssertionCreateWithName(
            assertion_type.as_concrete_TypeRef(),
            255, // kIOPMAssertionLevelOn
            name.as_concrete_TypeRef(),
            &mut id,
        )
    };
    (ok == 0).then_some(id)
}

fn release_keep_awake(id: u32) {
    unsafe {
        IOPMAssertionRelease(id);
    }
}

/// Raise a winit window to the macOS screen-saver level and make it span all
/// Spaces / show over fullscreen apps, via the underlying NSWindow. Best-effort.
fn raise_to_screensaver_level(window: &Window) {
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return;
    };
    // SAFETY: winit hands us a live NSView pointer for this window.
    unsafe {
        let ns_view: &NSView = &*appkit.ns_view.as_ptr().cast::<NSView>();
        if let Some(ns_window) = ns_view.window() {
            ns_window.setLevel(1000); // NSScreenSaverWindowLevel
            ns_window.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::Stationary
                    | NSWindowCollectionBehavior::FullScreenAuxiliary,
            );
        }
    }
}

/// How often to poll the idle timer while waiting.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

pub fn run(args: RunArgs) {
    if args.activate {
        eprintln!("--activate is not supported on macOS; the screensaver activates on idle.");
        return;
    }

    let mut config = Config::load();
    crate::merge_cli(&mut config, &args);

    // Record our PID so `olsvr stop` and the setup wizard can find this instance.
    let pid_file = crate::pid_path();
    if let Err(e) = std::fs::write(&pid_file, std::process::id().to_string()) {
        log::warn!("Failed to write PID file {}: {e}", pid_file.display());
    }

    let timeout = Duration::from_secs(config.timeout as u64 * 60);
    let engine = Engine::new(config.activation, vec![0]);

    let event_loop = EventLoop::new().expect("failed to create winit event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = MacApp {
        config,
        engine,
        data_cache: Arc::new(RwLock::new(DataCache::default())),
        targets: Vec::new(),
        keep_awake_id: None,
        timeout,
        was_idle: false,
        weather_started: false,
        started: false,
        show_now: args.now,
        debug: args.debug,
        cursor_hidden: false,
    };
    event_loop
        .run_app(&mut app)
        .expect("winit event loop error");
}

/// One fullscreen window + renderer per display.
struct DisplayTarget {
    window: Arc<Window>,
    compositor: Compositor,
}

struct MacApp {
    config: Config,
    engine: Engine,
    data_cache: Arc<RwLock<DataCache>>,
    targets: Vec<DisplayTarget>,
    keep_awake_id: Option<u32>,
    timeout: Duration,
    was_idle: bool,
    weather_started: bool,
    started: bool,
    show_now: bool,
    debug: bool,
    cursor_hidden: bool,
}

impl MacApp {
    /// Execute the commands the Engine produced.
    fn apply(&mut self, cmds: Vec<EngineCommand>, event_loop: &ActiveEventLoop) {
        for cmd in cmds {
            match cmd {
                EngineCommand::Show { .. } => self.show(event_loop),
                EngineCommand::Hide => self.hide(),
                EngineCommand::SetKeepAwake(true) => {
                    if self.keep_awake_id.is_none() {
                        self.keep_awake_id = acquire_keep_awake();
                        if self.keep_awake_id.is_none() {
                            log::warn!("Failed to acquire display-sleep assertion");
                        }
                    }
                }
                EngineCommand::SetKeepAwake(false) => {
                    if let Some(id) = self.keep_awake_id.take() {
                        release_keep_awake(id);
                    }
                }
                EngineCommand::Quit => event_loop.exit(),
            }
        }
    }

    fn on_event(&mut self, ev: BackendEvent, event_loop: &ActiveEventLoop) {
        let cmds = self.engine.handle(ev, Instant::now());
        self.apply(cmds, event_loop);
    }

    fn show(&mut self, event_loop: &ActiveEventLoop) {
        if !self.targets.is_empty() {
            return;
        }

        // Start the weather fetch thread once if a weather layer is configured.
        if !self.weather_started
            && let Some((location, interval_secs)) = self.config.weather_config()
        {
            data::start_fetch_thread(
                self.data_cache.clone(),
                location,
                Duration::from_secs(interval_secs),
            );
            self.weather_started = true;
        }

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        let mut monitors: Vec<_> = event_loop.available_monitors().collect();
        if self.config.activation.displays == DisplayScope::Primary {
            monitors.truncate(1);
        }

        for monitor in monitors {
            let attrs = Window::default_attributes()
                .with_title("olsvr")
                .with_fullscreen(Some(Fullscreen::Borderless(Some(monitor))));
            let window = Arc::new(
                event_loop
                    .create_window(attrs)
                    .expect("failed to create window"),
            );
            window.set_window_level(WindowLevel::AlwaysOnTop);
            raise_to_screensaver_level(&window);

            let size = window.inner_size();
            let surface = instance
                .create_surface(window.clone())
                .expect("failed to create surface");

            let (layers, layer_configs) = self.config.build_layers(self.debug);
            let compositor = Compositor::new(
                &instance,
                surface,
                size.width.max(1),
                size.height.max(1),
                layers,
                &layer_configs,
                self.data_cache.clone(),
            );

            window.request_redraw();
            self.targets.push(DisplayTarget { window, compositor });
        }

        if !self.cursor_hidden {
            unsafe { NSCursor::hide() };
            self.cursor_hidden = true;
        }
    }

    fn target_mut(&mut self, id: WindowId) -> Option<&mut DisplayTarget> {
        self.targets.iter_mut().find(|t| t.window.id() == id)
    }

    /// Hide the saver windows and restore the cursor.
    fn hide(&mut self) {
        self.targets.clear(); // drops windows + renderers
        if self.cursor_hidden {
            unsafe { NSCursor::unhide() };
            self.cursor_hidden = false;
        }
    }
}

impl Drop for MacApp {
    fn drop(&mut self) {
        if let Some(id) = self.keep_awake_id.take() {
            release_keep_awake(id);
        }
        if self.cursor_hidden {
            unsafe { NSCursor::unhide() };
        }
        let _ = std::fs::remove_file(crate::pid_path());
    }
}

impl ApplicationHandler for MacApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.started {
            return;
        }
        self.started = true;
        // Startup commands (holds keep-awake under the "always" policy).
        let cmds = self.engine.init();
        self.apply(cmds, event_loop);
        if self.show_now {
            self.on_event(BackendEvent::ActivateSignal, event_loop);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Drive the Engine from the idle timer on idle/active transitions.
        let is_idle = idle_seconds() >= self.timeout.as_secs_f64();
        if is_idle && !self.was_idle {
            self.was_idle = true;
            self.on_event(BackendEvent::Idle, event_loop);
        } else if !is_idle && self.was_idle {
            self.was_idle = false;
            self.on_event(BackendEvent::Resume, event_loop);
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL_INTERVAL));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(t) = self.target_mut(id) {
                    t.compositor.resize(size.width.max(1), size.height.max(1));
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(t) = self.target_mut(id) {
                    t.compositor.render_frame(None);
                    // Continuous animation: request the next frame for this window.
                    t.window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { .. }
            | WindowEvent::MouseInput { .. }
            | WindowEvent::CursorMoved { .. } => {
                // Input dismisses the saver but keeps the process running.
                self.was_idle = false;
                self.on_event(BackendEvent::DismissInput, event_loop);
            }
            _ => {}
        }
    }
}
