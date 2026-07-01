//! macOS backend: a borderless screen-saver-level window per display rendered by the
//! portable `Compositor`, driven by the shared `Engine`. Idle is detected by polling
//! `CGEventSourceSecondsSinceLastEventType`; the display is kept awake with an
//! `IOPMAssertion` (the OLED-stays-awake / HDMI-redetect fix).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};
use objc2_app_kit::{
    NSColor, NSCursor, NSScreen, NSView, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::monitor::MonitorHandle;
use winit::platform::macos::MonitorHandleExtMacOS;
use winit::window::{Window, WindowId, WindowLevel};

use crate::RunArgs;
use crate::compositor::Compositor;
use crate::config::{Config, DisplayScope};
use crate::data::{self, DataCache};
use crate::engine::{BackendEvent, Engine, EngineCommand};

// ---- FFI: idle time (CoreGraphics) + display-sleep assertion (IOKit) ----

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(state_id: i32, event_type: u32) -> f64;
    fn CGDisplayHideCursor(display: u32) -> i32;
    fn CGDisplayShowCursor(display: u32) -> i32;
    fn CGMainDisplayID() -> u32;
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
    /// Fills `*out` with a CFDictionary `{pid: [assertion dicts]}`; caller releases it.
    fn IOPMCopyAssertionsByProcess(out: *mut *const c_void) -> i32;
}

// CoreFoundation accessors for walking the IOPMCopyAssertionsByProcess result.
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
    fn CFEqual(a: *const c_void, b: *const c_void) -> u8;
    fn CFDictionaryGetCount(dict: *const c_void) -> isize;
    fn CFDictionaryGetKeysAndValues(
        dict: *const c_void,
        keys: *mut *const c_void,
        values: *mut *const c_void,
    );
    fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFNumberGetValue(number: *const c_void, number_type: isize, value: *mut c_void) -> u8;
}

/// Seconds since the last HID input, system-wide.
fn idle_seconds() -> f64 {
    // kCGEventSourceStateCombinedSessionState = 0, kCGAnyInputEventType = !0
    unsafe { CGEventSourceSecondsSinceLastEventType(0, u32::MAX) }
}

/// Hide the cursor everywhere. `NSCursor::hide()` only takes effect where the
/// (accessory) app holds focus, so pair it with CoreGraphics' system-wide hide.
fn hide_cursor() {
    unsafe {
        NSCursor::hide();
        CGDisplayHideCursor(CGMainDisplayID());
    }
}

fn show_cursor() {
    unsafe {
        NSCursor::unhide();
        CGDisplayShowCursor(CGMainDisplayID());
    }
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

/// True if a process *other than us* currently holds a `PreventUserIdleDisplaySleep`
/// assertion — e.g. a video player or presentation keeping the screen on. The macOS
/// analog of the Wayland `IsInhibited` check, so the saver doesn't cover playing
/// video. Our own keep-awake assertion (held under some `keep_awake` policies) is
/// ignored by comparing the owning PID against ours.
fn display_sleep_inhibited() -> bool {
    const KCF_NUMBER_SINT32_TYPE: isize = 3;
    let own_pid = std::process::id() as i32;

    let mut by_pid: *const c_void = std::ptr::null();
    // SAFETY: IOKit fills in a CFDictionary we own (create rule); freed below.
    if unsafe { IOPMCopyAssertionsByProcess(&mut by_pid) } != 0 || by_pid.is_null() {
        return false; // can't determine — don't suppress activation
    }

    let display_type = CFString::new("PreventUserIdleDisplaySleep");
    let type_key = CFString::new("AssertType");
    let mut inhibited = false;

    // SAFETY: walk the documented `{pid: [{AssertType: ...}, ...]}` structure, then
    // release the dictionary. Inner keys/values are borrowed (get rule).
    unsafe {
        let count = CFDictionaryGetCount(by_pid).max(0) as usize;
        let mut pids = vec![std::ptr::null::<c_void>(); count];
        let mut lists = vec![std::ptr::null::<c_void>(); count];
        CFDictionaryGetKeysAndValues(by_pid, pids.as_mut_ptr(), lists.as_mut_ptr());

        'scan: for i in 0..count {
            let mut pid: i32 = 0;
            let read = CFNumberGetValue(
                pids[i],
                KCF_NUMBER_SINT32_TYPE,
                &mut pid as *mut i32 as *mut c_void,
            );
            if read == 0 || pid == own_pid {
                continue;
            }
            let list = lists[i];
            for j in 0..CFArrayGetCount(list) {
                let assertion = CFArrayGetValueAtIndex(list, j);
                let ty = CFDictionaryGetValue(
                    assertion,
                    type_key.as_concrete_TypeRef() as *const c_void,
                );
                if !ty.is_null()
                    && CFEqual(ty, display_type.as_concrete_TypeRef() as *const c_void) != 0
                {
                    inhibited = true;
                    break 'scan;
                }
            }
        }
        CFRelease(by_pid);
    }
    inhibited
}

/// Turn the freshly-created winit window into a full-display screen-saver overlay via
/// the underlying NSWindow: borderless, sized to the target display's whole frame at
/// screen-saver level (drawing over the menu bar and Dock), visible on all Spaces,
/// opaque black. Returns the display's physical pixel size for the GPU surface.
///
/// We place and size the window ourselves from the monitor's `NSScreen` rather than
/// using winit fullscreen: `Fullscreen::Borderless` routes through
/// `-[NSWindow toggleFullScreen:]`, entering a native fullscreen Space whose
/// `_NSFullScreenSpace` segfaults on teardown once the window is re-styled (winit
/// #2645). A plain borderless window at screen-saver level never creates one.
fn configure_saver_window(window: &Window, monitor: &MonitorHandle) -> Option<PhysicalSize<u32>> {
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return None;
    };
    // SAFETY: winit hands us a live NSView pointer for this window, and the monitor's
    // NSScreen pointer is valid for this synchronous use.
    unsafe {
        let ns_view: &NSView = &*appkit.ns_view.as_ptr().cast::<NSView>();
        let ns_window = ns_view.window()?;
        // Borderless first: removes the title bar and lifts AppKit's clamp of a
        // titled window to the visibleFrame, so the frame below can cover the menu
        // bar and Dock.
        ns_window.setStyleMask(NSWindowStyleMask::Borderless);
        ns_window.setLevel(1000); // NSScreenSaverWindowLevel (above the menu bar/Dock)
        ns_window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        // Place/size the window on the target display from its NSScreen frame (global
        // AppKit points, bottom-left origin — DPI-independent). winit's `with_position`
        // is unreliable across mixed-DPI displays, so we go straight to the NSScreen.
        // Overscan a couple points so HiDPI rounding can't leave a seam; the excess is
        // clipped off-screen.
        let size = monitor.ns_screen().map(|ptr| {
            let ns_screen: &NSScreen = &*ptr.cast::<NSScreen>();
            let mut frame = ns_screen.frame();
            let scale = ns_screen.backingScaleFactor();
            let (w, h) = (frame.size.width, frame.size.height);
            frame.origin.x -= 2.0;
            frame.origin.y -= 2.0;
            frame.size.width += 4.0;
            frame.size.height += 4.0;
            ns_window.setFrame_display(frame, true);
            PhysicalSize::new((w * scale).round() as u32, (h * scale).round() as u32)
        });
        // Paint the window black and drop the shadow — the shadow is the thin grey
        // edge line on modern macOS, and black covers any margin the GPU misses.
        ns_window.setOpaque(true);
        ns_window.setHasShadow(false);
        ns_window.setBackgroundColor(Some(&*NSColor::blackColor()));
        ns_window.orderFrontRegardless();
        size
    }
}

/// How often to poll the idle timer while waiting.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// While the saver is showing, a system idle time below this means fresh user
/// input, so dismiss. The borderless accessory windows don't reliably receive
/// key/mouse events themselves, so the system-wide idle timer is the reliable
/// dismiss signal. Kept below the Engine's 1s dismiss-grace so a `--now` preview
/// isn't dismissed by the keystroke that launched it.
const DISMISS_IDLE_SECS: f64 = 0.5;

/// If the render loop goes this long without drawing a frame while the saver is
/// active, the watchdog force-exits to release the screen.
const WATCHDOG_STALL: Duration = Duration::from_secs(5);

/// Safety net against a trapped screen. The saver window sits at screen-saver
/// level above everything, so if the render/event loop ever stalls (e.g. a GPU
/// surface call blocking on a display glitch) the black window would be left with
/// no way to dismiss it. This separate thread force-exits if no frame is drawn
/// for `WATCHDOG_STALL` while active. A clean `exit(0)` lets the display recover
/// and, because the launchd agent's KeepAlive only restarts on failure, does not
/// relaunch into another black-out.
struct Watchdog {
    start: Instant,
    last_beat_ms: AtomicU64,
    active: AtomicBool,
}

impl Watchdog {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            start: Instant::now(),
            last_beat_ms: AtomicU64::new(0),
            active: AtomicBool::new(false),
        })
    }

    /// Record that a frame was just drawn.
    fn beat(&self) {
        self.last_beat_ms
            .store(self.start.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// Arm (true) or disarm (false) staleness checking. Arming also beats, so the
    /// stall window starts fresh from `show()` (covers a hang during GPU init).
    fn set_active(&self, active: bool) {
        if active {
            self.beat();
        }
        self.active.store(active, Ordering::Relaxed);
    }

    fn spawn(self: &Arc<Self>) {
        let wd = Arc::clone(self);
        let stall_ms = WATCHDOG_STALL.as_millis() as u64;
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                if !wd.active.load(Ordering::Relaxed) {
                    continue;
                }
                let elapsed = wd.start.elapsed().as_millis() as u64;
                let since_beat = elapsed.saturating_sub(wd.last_beat_ms.load(Ordering::Relaxed));
                if since_beat > stall_ms {
                    log::error!(
                        "watchdog: render stalled {since_beat}ms while active — exiting to release the display"
                    );
                    let _ = std::fs::remove_file(crate::pid_path());
                    std::process::exit(0);
                }
            }
        });
    }
}

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

    // Watchdog guards against a trapped screen if the render loop ever stalls.
    let watchdog = Watchdog::new();
    watchdog.spawn();

    let event_loop = EventLoop::new().expect("failed to create winit event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = MacApp {
        config,
        engine,
        data_cache: Arc::new(RwLock::new(DataCache::default())),
        targets: Vec::new(),
        keep_awake_id: None,
        timeout,
        weather_started: false,
        started: false,
        show_now: args.now,
        debug: args.debug,
        cursor_hidden: false,
        watchdog,
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
    weather_started: bool,
    started: bool,
    show_now: bool,
    debug: bool,
    cursor_hidden: bool,
    watchdog: Arc<Watchdog>,
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

        // Arm the watchdog for the whole show + render path (covers a hang during
        // GPU init too); disarmed again below if no window could be created.
        self.watchdog.set_active(true);

        let mut monitors: Vec<_> = event_loop.available_monitors().collect();
        if self.config.activation.displays == DisplayScope::Primary {
            monitors.truncate(1);
        }

        for monitor in monitors {
            // Plain undecorated window — NOT winit fullscreen. `Fullscreen::Borderless`
            // enters a native fullscreen Space whose `_NSFullScreenSpace` segfaults on
            // teardown once we re-style the window (see configure_saver_window).
            // configure_saver_window then makes it a borderless screen-saver-level
            // window placed on this monitor's NSScreen. Failures skip the display
            // instead of aborting the process.
            let attrs = Window::default_attributes()
                .with_title("olsvr")
                .with_decorations(false);
            let window = match event_loop.create_window(attrs) {
                Ok(w) => Arc::new(w),
                Err(e) => {
                    log::error!("Failed to create saver window, skipping display: {e}");
                    continue;
                }
            };
            window.set_window_level(WindowLevel::AlwaysOnTop);
            let size =
                configure_saver_window(&window, &monitor).unwrap_or_else(|| window.inner_size());

            let surface = match instance.create_surface(window.clone()) {
                Ok(s) => s,
                Err(e) => {
                    log::error!("Failed to create surface, skipping display: {e}");
                    continue;
                }
            };

            let (layers, layer_configs) = self.config.build_layers(self.debug);
            let Some(compositor) = Compositor::new(
                &instance,
                surface,
                size.width.max(1),
                size.height.max(1),
                layers,
                &layer_configs,
                self.data_cache.clone(),
            ) else {
                log::error!("Failed to init renderer, skipping display");
                continue;
            };

            window.request_redraw();
            self.targets.push(DisplayTarget { window, compositor });
        }

        if self.targets.is_empty() {
            // Nothing came up — disarm so the watchdog doesn't fire while dormant.
            log::error!("No saver windows could be created; staying dormant");
            self.watchdog.set_active(false);
            return;
        }

        // Activate the app so the cursor hide reaches every display — cursor
        // hiding only takes effect while the app holds focus. An LSUIElement app
        // has no Dock icon, so activating costs nothing visible. (The earlier
        // "activation breaks multi-display" was really the with_position bug,
        // since fixed by placing each window on its monitor's NSScreen.)
        // SAFETY: main thread; NSApplication is the live singleton.
        unsafe {
            let app: *mut objc2::runtime::AnyObject =
                objc2::msg_send![objc2::class!(NSApplication), sharedApplication];
            if !app.is_null() {
                let _: () = objc2::msg_send![app, activateIgnoringOtherApps: true];
            }
        }

        if !self.cursor_hidden {
            hide_cursor();
            self.cursor_hidden = true;
        }
    }

    fn target_mut(&mut self, id: WindowId) -> Option<&mut DisplayTarget> {
        self.targets.iter_mut().find(|t| t.window.id() == id)
    }

    /// Hide the saver windows and restore the cursor.
    fn hide(&mut self) {
        self.targets.clear(); // drops windows + renderers
        self.watchdog.set_active(false);
        if self.cursor_hidden {
            show_cursor();
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
            show_cursor();
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
        // Drive the Engine from the system-wide idle timer. This is the reliable
        // dismiss path: the saver windows are borderless and the app is an
        // accessory, so they don't always get key/mouse events themselves.
        let idle = idle_seconds();
        let showing = !self.targets.is_empty();
        if !showing && idle >= self.timeout.as_secs_f64() && !display_sleep_inhibited() {
            // Skip activation while another app holds a display-sleep assertion
            // (video playback, presentations) so the saver doesn't cover it.
            self.on_event(BackendEvent::Idle, event_loop);
        } else if showing && idle < DISMISS_IDLE_SECS {
            // Fresh input while showing — dismiss. DismissInput (1s grace), not
            // Resume (5s), so it's responsive even if you return right away.
            self.on_event(BackendEvent::DismissInput, event_loop);
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
                let mut drew = false;
                if let Some(t) = self.target_mut(id) {
                    drew = t.compositor.render_frame(None);
                    // Continuous animation: request the next frame for this window.
                    t.window.request_redraw();
                }
                if drew {
                    self.watchdog.beat();
                }
            }
            WindowEvent::KeyboardInput { .. }
            | WindowEvent::MouseInput { .. }
            | WindowEvent::CursorMoved { .. } => {
                // Fast path when the window does receive input; the idle poll in
                // about_to_wait is the reliable fallback. Keeps the process running.
                self.on_event(BackendEvent::DismissInput, event_loop);
            }
            _ => {}
        }
    }
}
