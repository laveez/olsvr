//! macOS backend: a borderless screen-saver-level window per display rendered by the
//! portable `Compositor`, driven by the shared `Engine`. Idle is detected by polling
//! `CGEventSourceSecondsSinceLastEventType`; the display is kept awake with an
//! `IOPMAssertion` (the OLED-stays-awake / HDMI-redetect fix), except while the session
//! is locked.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use core_foundation::base::{CFGetTypeID, CFRange, TCFType};
use core_foundation::boolean::{CFBoolean, CFBooleanRef};
use core_foundation::number::{CFNumber, CFNumberRef};
use core_foundation::string::{CFString, CFStringGetCharacters, CFStringGetLength, CFStringRef};
use objc2_app_kit::{
    NSColor, NSCursor, NSScreen, NSView, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::monitor::MonitorHandle;
use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS, MonitorHandleExtMacOS};
use winit::window::{Window, WindowId, WindowLevel};

use crate::RunArgs;
use crate::compositor::{Compositor, FrameOutcome};
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
    /// A CFDictionary the caller releases, or null outside a GUI session.
    fn CGSessionCopyCurrentDictionary() -> *const c_void;
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

const KCF_NUMBER_SINT32_TYPE: isize = 3;

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

const KEEP_AWAKE_NAME: &str = "olsvr OLED screensaver";

/// Acquire a "prevent the display from idle-sleeping" assertion.
fn acquire_keep_awake() -> Option<u32> {
    let assertion_type = CFString::new("PreventUserIdleDisplaySleep");
    let name = CFString::new(KEEP_AWAKE_NAME);
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

/// The CFString under `key` in `dict`, or `None` if the key is missing or holds another
/// type. `dict` must be a valid CFDictionary.
unsafe fn string_value(dict: *const c_void, key: &str) -> Option<CFString> {
    let key = CFString::new(key);
    // SAFETY: `dict` is valid by contract. The value is borrowed (get rule), so the
    // wrapper retains it and its drop balances that.
    unsafe {
        let value = CFDictionaryGetValue(dict, key.as_concrete_TypeRef() as *const c_void);
        if value.is_null() || CFGetTypeID(value) != CFString::type_id() {
            return None;
        }
        Some(CFString::wrap_under_get_rule(value as CFStringRef))
    }
}

/// True if `assertion`, one of the dictionaries `IOPMCopyAssertionsByProcess` lists, keeps
/// the display awake. Chrome, Firefox and Electron create the deprecated
/// `NoDisplaySleepAssertion`; `AssertType` keeps that spelling, while the private
/// `AssertionTrueType` holds the canonical type when powerd adds it. `assertion` must be a
/// valid CFDictionary.
unsafe fn is_display_sleep_assertion(assertion: *const c_void) -> bool {
    // SAFETY: `assertion` is valid by contract; every value read from it is type-checked
    // before use.
    unsafe {
        let Some(assertion_type) = string_value(assertion, "AssertionTrueType")
            .or_else(|| string_value(assertion, "AssertType"))
        else {
            return false;
        };
        if assertion_type != "PreventUserIdleDisplaySleep"
            && assertion_type != "NoDisplaySleepAssertion"
        {
            return false;
        }

        let level_key = CFString::new("AssertLevel");
        let level_ref =
            CFDictionaryGetValue(assertion, level_key.as_concrete_TypeRef() as *const c_void);
        if level_ref.is_null() || CFGetTypeID(level_ref) != CFNumber::type_id() {
            return true;
        }
        let mut level: i32 = 0;
        let read = CFNumberGetValue(
            level_ref,
            KCF_NUMBER_SINT32_TYPE,
            &mut level as *mut i32 as *mut c_void,
        );
        read != 0 && level == 255 // kIOPMAssertionLevelOn
    }
}

/// `string` as UTF-8, with unpaired surrogates replaced by U+FFFD. `CFString`'s own
/// `Display` panics on a string that does not convert to UTF-8, and the names we log
/// come from other processes.
fn lossy_string(string: &CFString) -> String {
    let string_ref = string.as_concrete_TypeRef();
    // SAFETY: `string_ref` is a live CFString, and `units` has room for the `length`
    // UTF-16 units CFStringGetCharacters writes for the full range.
    let units = unsafe {
        let length = CFStringGetLength(string_ref).max(0);
        let mut units = vec![0; length as usize];
        let range = CFRange {
            location: 0,
            length,
        };
        CFStringGetCharacters(string_ref, range, units.as_mut_ptr());
        units
    };
    String::from_utf16_lossy(&units)
}

/// The owning PID and `AssertName` (empty if it has none) of the first display-sleep
/// assertion another app holds, e.g. a video player or presentation keeping the screen on.
/// The macOS analog of the Wayland `IsInhibited` check, so the saver doesn't cover playing
/// video. Our own keep-awake assertion (held under some `keep_awake` policies) is ignored
/// by owning PID and by name, so a second olsvr instance doesn't count either.
fn display_sleep_inhibited() -> Option<(i32, String)> {
    let own_pid = std::process::id() as i32;

    let mut by_pid: *const c_void = std::ptr::null();
    // SAFETY: IOKit fills in a CFDictionary we own (create rule); freed below.
    if unsafe { IOPMCopyAssertionsByProcess(&mut by_pid) } != 0 || by_pid.is_null() {
        return None; // can't determine, so don't suppress activation
    }

    let mut inhibitor = None;

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
                if !is_display_sleep_assertion(assertion) {
                    continue;
                }
                let name = string_value(assertion, "AssertName");
                if name.as_ref().is_some_and(|name| *name == KEEP_AWAKE_NAME) {
                    continue;
                }
                let name = name.map(|name| lossy_string(&name)).unwrap_or_default();
                inhibitor = Some((pid, name));
                break 'scan;
            }
        }
        CFRelease(by_pid);
    }
    inhibitor
}

/// The flag under `key` in `dict`: a CFBoolean's value, or whether a CFNumber holding an
/// integer is non-zero. `None` for a missing key or any other value. `dict` must be a valid
/// CFDictionary.
unsafe fn flag_value(dict: *const c_void, key: &str) -> Option<bool> {
    let key = CFString::new(key);
    // SAFETY: `dict` is valid by contract. The value is borrowed (get rule), so the wrapper
    // retains it and its drop balances that; its type is checked before it is wrapped.
    unsafe {
        let value = CFDictionaryGetValue(dict, key.as_concrete_TypeRef() as *const c_void);
        if value.is_null() {
            return None;
        }
        let type_id = CFGetTypeID(value);
        if type_id == CFBoolean::type_id() {
            Some(CFBoolean::wrap_under_get_rule(value as CFBooleanRef).into())
        } else if type_id == CFNumber::type_id() {
            CFNumber::wrap_under_get_rule(value as CFNumberRef)
                .to_i64()
                .map(|number| number != 0)
        } else {
            None
        }
    }
}

/// True while the login session is locked, or switched out by fast user switching.
fn session_locked() -> bool {
    // SAFETY: CoreGraphics hands us a dictionary we own (create rule), released below;
    // its values are borrowed (get rule) until then.
    unsafe {
        let session = CGSessionCopyCurrentDictionary();
        if session.is_null() {
            return false;
        }
        // `CGSSessionScreenIsLocked` is undocumented: usually absent while unlocked, and its
        // CF type is not guaranteed. CGSession.h defines kCGSessionOnConsoleKey as
        // "kCGSSessionOnConsoleKey".
        let locked = flag_value(session, "CGSSessionScreenIsLocked") == Some(true)
            || flag_value(session, "kCGSSessionOnConsoleKey") == Some(false);
        CFRelease(session);
        locked
    }
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

/// If an armed watchdog sees no beat for this long, it force-exits to release the
/// screen.
const WATCHDOG_STALL: Duration = Duration::from_secs(5);

/// Safety net against a trapped screen. The saver window sits at screen-saver
/// level above everything, so if the render/event loop ever stalls (e.g. a GPU
/// surface call blocking on a display glitch) the black window would be left with
/// no way to dismiss it. This separate thread force-exits when no beat arrives for
/// `WATCHDOG_STALL` while armed. Beats come from a completed frame (presented or
/// skipped), from each `about_to_wait` pass while every saver window is occluded,
/// and from `show()` after each display's renderer init.
///
/// It exits with status 1, so launchd records a failed exit for the job; KeepAlive
/// respawns the agent after any exit, clean or not. `ThrottleInterval` only spaces
/// spawns, so a long-running agent is respawned at once. What keeps the new process
/// from showing at once is the `hidden_at` seed in `run()`: the next show needs a
/// full idle timeout from process start. The trade: a crash leaves the panel
/// uncovered longer, but watchdog restarts are spaced a full idle timeout apart.
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

    /// Record that the main thread just made progress.
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
                        "watchdog: render stalled {since_beat}ms while active — exiting to release the display (launchd will restart)"
                    );
                    let _ = std::fs::remove_file(crate::pid_path());
                    // exit() can hang in atexit and framework handlers with the main thread wedged.
                    unsafe { libc::_exit(1) }
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

    // winit's default menu binds Cmd+Q to terminate:, which ends the agent with exit 0.
    // Accessory also covers a bare-binary run, where winit would pick Regular.
    let mut builder = EventLoop::builder();
    builder
        .with_default_menu(false)
        .with_activate_ignoring_other_apps(false)
        .with_activation_policy(ActivationPolicy::Accessory);
    let event_loop = builder.build().expect("failed to create winit event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = MacApp {
        config,
        engine,
        data_cache: Arc::new(RwLock::new(DataCache::default())),
        targets: Vec::new(),
        keep_awake_id: None,
        wants_keep_awake: false,
        locked: false,
        last_poll: Instant::now(),
        timeout,
        weather_started: false,
        started: false,
        show_now: args.now,
        debug: args.debug,
        cursor_hidden: false,
        watchdog,
        // As if just dismissed, so a restarted agent waits a full idle timeout before
        // its first show; the Watchdog's relaunch bound depends on it.
        hidden_at: Some(Instant::now()),
        show_failures: 0,
        display_inhibitor: None,
    };
    event_loop
        .run_app(&mut app)
        .expect("winit event loop error");
}

/// One fullscreen window + renderer per display.
struct DisplayTarget {
    window: Arc<Window>,
    compositor: Compositor,
    occluded: bool,
    /// The last frame was skipped; `about_to_wait` requests the retry.
    needs_retry: bool,
}

struct MacApp {
    config: Config,
    engine: Engine,
    data_cache: Arc<RwLock<DataCache>>,
    targets: Vec<DisplayTarget>,
    keep_awake_id: Option<u32>,
    wants_keep_awake: bool,
    locked: bool,
    /// The last session-lock check and the retry of skipped frames and occluded windows, both
    /// paced to `POLL_INTERVAL` because `about_to_wait` also runs after every frame.
    last_poll: Instant,
    timeout: Duration,
    weather_started: bool,
    started: bool,
    show_now: bool,
    debug: bool,
    cursor_hidden: bool,
    watchdog: Arc<Watchdog>,
    hidden_at: Option<Instant>,
    /// Consecutive failed show attempts, for log throttling.
    show_failures: u32,
    /// The other app found holding a display-sleep assertion at the last idle check, so
    /// the log shows when a hold starts and ends rather than every poll.
    display_inhibitor: Option<(i32, String)>,
}

impl MacApp {
    /// Execute the commands the Engine produced.
    fn apply(&mut self, cmds: Vec<EngineCommand>, event_loop: &ActiveEventLoop) {
        let mut show_failed = false;
        for cmd in cmds {
            match cmd {
                EngineCommand::Show { .. } => show_failed |= !self.show(event_loop),
                EngineCommand::Hide => self.hide(),
                EngineCommand::SetKeepAwake(on) => self.wants_keep_awake = on,
                EngineCommand::Quit => event_loop.exit(),
            }
        }
        // Reported after the batch so the Engine's rollback (Hide, SetKeepAwake(false))
        // has the last word on `wants_keep_awake`; mid-batch, the SetKeepAwake(true)
        // after the Show would overrule it. Without the report the Engine latches Active
        // with no windows (and no input path that could ever dismiss it). The resulting
        // Hide sets `hidden_at`, pacing retries to one per idle cycle.
        if show_failed {
            self.on_event(BackendEvent::ShowFailed, event_loop);
        }
        // The hold follows the final `wants_keep_awake`, so a failed show never takes it.
        self.reconcile_keep_awake();
    }

    /// Match the display-sleep assertion to what the Engine wants, but never hold it
    /// while the session is locked: the saver cannot draw over the lock screen, so the
    /// hold would only keep the lock screen lit.
    fn reconcile_keep_awake(&mut self) {
        let hold = self.wants_keep_awake && !self.locked;
        if hold && self.keep_awake_id.is_none() {
            self.keep_awake_id = acquire_keep_awake();
            if self.keep_awake_id.is_none() {
                log::warn!("Failed to acquire display-sleep assertion");
            }
        } else if !hold && let Some(id) = self.keep_awake_id.take() {
            release_keep_awake(id);
        }
    }

    fn set_locked(&mut self, locked: bool, event_loop: &ActiveEventLoop) {
        if locked == self.locked {
            return;
        }
        self.locked = locked;
        log::info!("Session {}", if locked { "locked" } else { "unlocked" });
        self.reconcile_keep_awake();
        if locked && !self.targets.is_empty() {
            // No grace, unlike DismissInput: the saver cannot draw over the lock screen,
            // even when the lock comes right after activation.
            self.on_event(BackendEvent::SurfaceClosed, event_loop);
        }
    }

    fn on_event(&mut self, ev: BackendEvent, event_loop: &ActiveEventLoop) {
        let cmds = self.engine.handle(ev.clone(), Instant::now());
        if cmds.contains(&EngineCommand::Hide) {
            log::info!("Hide via {ev:?}");
        }
        self.apply(cmds, event_loop);
    }

    /// Returns false when no saver window could be created on any display.
    fn show(&mut self, event_loop: &ActiveEventLoop) -> bool {
        if !self.targets.is_empty() {
            return true;
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
            let compositor = Compositor::new(
                &instance,
                surface,
                size.width.max(1),
                size.height.max(1),
                layers,
                &layer_configs,
                self.data_cache.clone(),
            );
            // Pass or fail, a slow init on one display must not eat the next one's stall window.
            self.watchdog.beat();
            let Some(compositor) = compositor else {
                log::error!("Failed to init renderer, skipping display");
                continue;
            };

            window.request_redraw();
            self.targets.push(DisplayTarget {
                window,
                compositor,
                occluded: false,
                needs_retry: false,
            });
        }

        if self.targets.is_empty() {
            // Nothing came up (e.g. zero attached displays) — disarm so the watchdog
            // doesn't fire while dormant. First failure at error, repeats at debug so
            // an unattended weekend doesn't fill the log.
            if self.show_failures == 0 {
                log::error!("No saver windows could be created; will retry next idle cycle");
            } else {
                log::debug!(
                    "Saver window creation failed again ({} consecutive)",
                    self.show_failures + 1
                );
            }
            self.show_failures += 1;
            self.watchdog.set_active(false);
            return false;
        }
        if self.show_failures > 0 {
            log::info!(
                "Saver windows created again after {} failed attempts",
                self.show_failures
            );
            self.show_failures = 0;
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
        true
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
        self.hidden_at = Some(Instant::now());
    }

    /// True once a full idle period has passed since the saver was last dismissed
    /// (startup counts as a dismiss, see `run()`).
    ///
    /// `CGEventSourceSecondsSinceLastEventType` trails the input winit delivers by
    /// up to a second, so the reading taken right after a dismiss is still the
    /// stale pre-input value — well past the timeout. Without this gate the very
    /// next poll re-shows the saver milliseconds after the dismiss, and only a
    /// second mouse move (by which time the counter has caught up) gets rid of it.
    fn idle_cycle_elapsed(&self) -> bool {
        self.hidden_at.is_none_or(|t| t.elapsed() >= self.timeout)
    }

    /// Whether another app's display-sleep assertion should keep the saver away. Logs
    /// when a hold starts and ends. Only the idle check calls it, so an end is logged
    /// at the first idle check after the hold is gone, not when it is released.
    fn display_inhibited(&mut self) -> bool {
        let inhibitor = display_sleep_inhibited();
        match (&self.display_inhibitor, &inhibitor) {
            (None, Some((pid, name))) => log::info!(
                "Not activating while pid {pid} holds a display-sleep assertion named {name:?}"
            ),
            (Some((pid, name)), None) => log::info!(
                "Display-sleep assertion of pid {pid} named {name:?} released, activation resumes"
            ),
            _ => {}
        }
        self.display_inhibitor = inhibitor;
        self.display_inhibitor.is_some()
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
        // Ahead of init, so "always" takes no hold when starting behind a lock screen.
        self.set_locked(session_locked(), event_loop);
        // Startup commands (holds keep-awake under the "always" policy).
        let cmds = self.engine.init();
        self.apply(cmds, event_loop);
        if self.show_now {
            if self.locked {
                log::info!("Session locked, not activating");
            } else {
                self.on_event(BackendEvent::ActivateSignal, event_loop);
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let poll_due = self.last_poll.elapsed() >= POLL_INTERVAL;
        if poll_due {
            self.last_poll = Instant::now();
            self.set_locked(session_locked(), event_loop);
        }

        // Drive the Engine from the system-wide idle timer. This is the reliable
        // dismiss path: the saver windows are borderless and the app is an
        // accessory, so they don't always get key/mouse events themselves.
        let idle = idle_seconds();
        let showing = !self.targets.is_empty();
        if !showing
            && !self.locked
            && idle >= self.timeout.as_secs_f64()
            && self.idle_cycle_elapsed()
            && !self.display_inhibited()
        {
            // Skip activation while the lock screen would cover the saver, and while
            // another app holds a display-sleep assertion (video playback,
            // presentations) so the saver doesn't cover it.
            self.on_event(BackendEvent::Idle, event_loop);
        } else if showing && idle < DISMISS_IDLE_SECS {
            // Fresh input while showing — dismiss. DismissInput (1s grace), not
            // Resume (5s), so it's responsive even if you return right away.
            self.on_event(BackendEvent::DismissInput, event_loop);
        }

        if poll_due {
            // Occluded windows too: a stale flag would freeze a full-brightness frame on an
            // OLED until input, and an occluded retry is one CPU-only frame per second.
            for t in self
                .targets
                .iter_mut()
                .filter(|t| t.needs_retry || t.occluded)
            {
                t.needs_retry = false;
                t.window.request_redraw();
            }
        }
        // While every window is occluded (a sleeping display) nothing renders, so
        // beat here. A visible window still has to complete its frames.
        if !self.targets.is_empty() && self.targets.iter().all(|t| t.occluded) {
            self.watchdog.beat();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL_INTERVAL));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => self.on_event(BackendEvent::SurfaceClosed, event_loop),
            WindowEvent::Resized(size) => {
                if let Some(t) = self.target_mut(id) {
                    t.compositor.resize(size.width.max(1), size.height.max(1));
                }
            }
            WindowEvent::Occluded(occluded) => {
                if let Some(t) = self.target_mut(id) {
                    log::debug!("Saver window {id:?} occluded: {occluded}");
                    t.occluded = occluded;
                    if !occluded {
                        t.window.request_redraw();
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                let Some(t) = self.target_mut(id) else {
                    return;
                };
                match t.compositor.render_frame(None) {
                    FrameOutcome::Broken => {
                        self.on_event(BackendEvent::ShowFailed, event_loop);
                        return;
                    }
                    FrameOutcome::Presented => {
                        t.needs_retry = false;
                        // wgpu's Metal backend returns Occluded instead of a drawable while AppKit
                        // reports the window occluded, so a present proves it visible, even when
                        // the `Occluded(false)` that clears the flag never arrived.
                        t.occluded = false;
                        t.window.request_redraw();
                    }
                    // An occluded surface skips without blocking, so re-requesting here
                    // would spin; `about_to_wait` retries at the poll rate instead.
                    FrameOutcome::Skipped => t.needs_retry = true,
                }
                // Any completed render is a heartbeat: the watchdog exists to catch
                // a hung loop, and a skipped frame (occluded surface, sleeping
                // display) means there is nothing to draw, not that a black window
                // is trapping the screen.
                self.watchdog.beat();
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

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        log::warn!("Event loop exiting");
    }
}
