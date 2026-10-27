//! Wayland (Linux) backend, extracted from the original `main.rs` and driven through
//! the `Engine`. The shared CLI and helpers (`Cli`, `RunArgs`, `pid_path`, `merge_cli`,
//! `stop`, `main`) live in the crate root (`src/main.rs`). Compiled only on Linux.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, mpsc};
use std::time::{Duration, Instant};

use dbus::message::MatchRule;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    reexports::calloop::{
        EventLoop,
        signals::{Signal, Signals},
    },
    reexports::calloop_wayland_source::WaylandSource,
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::WaylandSurface,
    shell::xdg::{
        XdgShell,
        window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
    },
};
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface},
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::{self, ExtIdleNotifierV1},
};
use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1, zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
};

use smithay_client_toolkit::reexports::calloop::channel::{self, Sender};

use crate::compositor::{Compositor, DebugInfo, FrameOutcome, warning_due};
use crate::config::{Config, KeepAwake};
use crate::data::{self, DataCache};
use crate::engine::{BackendEvent, Engine, EngineCommand};
use crate::{RunArgs, merge_cli, pid_path};

enum DbusIdleEvent {
    Idle,
    Resumed,
    Locked,
    Unlocked,
}

enum GnomeCommand {
    SetKeepAwake(bool),
    Rearm,
}

/// gnome-session's `GSM_INHIBITOR_FLAG_IDLE`.
const IDLE_INHIBIT_FLAG: u32 = 8;
/// The app id olsvr's own `always` inhibitor carries, which the idle gate skips.
const INHIBIT_APP_ID: &str = "olsvr";
const CALL_TIMEOUT: Duration = Duration::from_secs(2);
/// Consecutive failures after which the monitor replaces its bus connection.
const RECONNECT_AFTER: u32 = 3;
/// GetActive retries after a read that got no reply, before the monitor assumes unlocked. The
/// bound matters because gnome-shell's D-Bus-activated `org.gnome.ScreenSaver` helper exits
/// without replying wherever gnome-shell is installed but not running.
const LOCK_READ_RETRIES: u32 = 3;

/// The wait after `failures` failures in a row: 1, 2, 4 and so on, up to 30 s.
fn retry_delay(failures: u32) -> Duration {
    Duration::from_secs(1 << failures.saturating_sub(1).min(5)).min(Duration::from_secs(30))
}

/// libdbus reports a call that timed out this way, and the callee may still act on it later.
fn is_no_reply(e: &dbus::Error) -> bool {
    e.name() == Some("org.freedesktop.DBus.Error.NoReply")
}

/// The main thread's handle on the GNOME D-Bus monitor thread, which owns the session bus
/// connection.
struct GnomeSession {
    tx: mpsc::Sender<GnomeCommand>,
    locked: Arc<AtomicBool>,
}

impl GnomeSession {
    fn spawn(events: Sender<DbusIdleEvent>, timeout_ms: u64) -> Self {
        let (tx, commands) = mpsc::channel();
        let locked = Arc::new(AtomicBool::new(false));
        let thread_locked = Arc::clone(&locked);
        std::thread::spawn(move || {
            dbus_idle_monitor_loop(events, commands, thread_locked, timeout_ms)
        });
        Self { tx, locked }
    }

    fn set_keep_awake(&self, on: bool) {
        if self.tx.send(GnomeCommand::SetKeepAwake(on)).is_err() {
            log::error!("D-Bus monitor has stopped, so keep_awake cannot be applied");
        }
    }

    /// A hide without user input can leave GNOME's idle time past the timeout (under `always`
    /// olsvr's own inhibitor keeps mutter from resetting it), and the monitor would then wait
    /// for a resume that never comes. Re-armed, it activates again one timeout later if the
    /// user is still idle.
    fn rearm(&self) {
        // The monitor stops only by panicking, which the closed event channel reports.
        let _ = self.tx.send(GnomeCommand::Rearm);
    }

    fn locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }
}

/// The `always` keep-awake hold: a gnome-session idle inhibitor. gnome-session drops it when
/// the connection that took it closes, so neither a reconnect nor an exiting thread leaves it
/// behind.
#[derive(Default)]
struct KeepAwakeHold {
    wanted: bool,
    cookie: Option<u32>,
    /// gnome-session answered the last Inhibit with an error.
    refused: bool,
    /// Inhibit calls in a row that got no reply.
    timeouts: u32,
    /// After an Inhibit without a reply, the next one waits until then.
    retry_at: Option<Instant>,
}

impl KeepAwakeHold {
    /// Returns true when the connection must be replaced.
    fn reconcile(
        &mut self,
        session: &dbus::blocking::Proxy<&dbus::blocking::Connection>,
        locked: bool,
    ) -> bool {
        if !self.wanted || locked {
            self.refused = false;
            self.timeouts = 0;
            self.retry_at = None;
            if let Some(cookie) = self.cookie.take() {
                let released: Result<(), dbus::Error> =
                    session.method_call("org.gnome.SessionManager", "Uninhibit", (cookie,));
                match released {
                    Ok(()) => log::info!("Keep-awake: released GNOME idle inhibitor {cookie}"),
                    Err(e) => log::warn!("Keep-awake: Uninhibit of cookie {cookie} failed: {e}"),
                }
            }
            return false;
        }
        if self.cookie.is_some()
            || self.refused
            || self.retry_at.is_some_and(|t| Instant::now() < t)
        {
            return false;
        }
        let taken: Result<(u32,), dbus::Error> = session.method_call(
            "org.gnome.SessionManager",
            "Inhibit",
            (INHIBIT_APP_ID, 0u32, "OLED screensaver", IDLE_INHIBIT_FLAG),
        );
        match taken {
            Ok((cookie,)) => {
                log::info!("Keep-awake: holding GNOME idle inhibitor {cookie}");
                self.cookie = Some(cookie);
                self.timeouts = 0;
                false
            }
            // gnome-session can still create this inhibitor once it gets to the call, with no
            // cookie to release it. Closing the connection makes gnome-session drop it.
            Err(e) if is_no_reply(&e) => {
                self.timeouts += 1;
                let delay = retry_delay(self.timeouts);
                log::warn!(
                    "Keep-awake: SessionManager.Inhibit failed: {e} (reconnecting, retried in {}s)",
                    delay.as_secs()
                );
                self.retry_at = Some(Instant::now() + delay);
                true
            }
            Err(e) => {
                log::warn!(
                    "Keep-awake: SessionManager.Inhibit failed: {e} (retried after the next lock, release or reconnect)"
                );
                self.refused = true;
                false
            }
        }
    }

    /// The cookie belonged to the old connection, and gnome-session dropped it along with it.
    fn forget_connection(&mut self) {
        if let Some(cookie) = self.cookie.take() {
            log::info!(
                "Keep-awake: GNOME idle inhibitor {cookie} ended with the old D-Bus connection"
            );
        }
        self.refused = false;
    }
}

fn get_mutter_idletime(conn: &dbus::blocking::Connection) -> Result<u64, dbus::Error> {
    let monitor = conn.with_proxy(
        "org.gnome.Mutter.IdleMonitor",
        "/org/gnome/Mutter/IdleMonitor/Core",
        CALL_TIMEOUT,
    );
    let (ms,): (u64,) = monitor.method_call("org.gnome.Mutter.IdleMonitor", "GetIdletime", ())?;
    Ok(ms)
}

/// Whether an app other than olsvr holds a gnome-session idle inhibitor. Inhibitors relayed by
/// mutter count: mpv and SDL inhibit through that relay, and olsvr's own relayed inhibitor exists
/// only while the saver shows, when activation is moot. No session manager on the bus counts as
/// not inhibited.
fn idle_inhibited_by_others(
    session: &dbus::blocking::Proxy<&dbus::blocking::Connection>,
) -> Result<bool, dbus::Error> {
    const NO_SESSION_MANAGER: [&str; 2] = [
        "org.freedesktop.DBus.Error.ServiceUnknown",
        "org.freedesktop.DBus.Error.NameHasNoOwner",
    ];
    let conn = session.connection;
    let find_other = || -> Result<bool, dbus::Error> {
        let (paths,): (Vec<dbus::Path<'static>>,) =
            session.method_call("org.gnome.SessionManager", "GetInhibitors", ())?;
        for path in paths {
            let inhibitor = conn.with_proxy("org.gnome.SessionManager", path, session.timeout);
            let (flags,): (u32,) =
                inhibitor.method_call("org.gnome.SessionManager.Inhibitor", "GetFlags", ())?;
            if flags & IDLE_INHIBIT_FLAG == 0 {
                continue;
            }
            let (app_id,): (String,) =
                inhibitor.method_call("org.gnome.SessionManager.Inhibitor", "GetAppId", ())?;
            if app_id != INHIBIT_APP_ID {
                return Ok(true);
            }
        }
        Ok(false)
    };
    match find_other() {
        Err(e) if e.name().is_some_and(|n| NO_SESSION_MANAGER.contains(&n)) => Ok(false),
        result => result,
    }
}

/// Keeps `locked` equal to GNOME's lock screen state. The match callback runs inside
/// `Connection::process`.
fn watch_lock_screen(conn: &dbus::blocking::Connection, locked: &Arc<AtomicBool>) {
    let flag = Arc::clone(locked);
    let watch = conn.add_match(
        MatchRule::new_signal("org.gnome.ScreenSaver", "ActiveChanged"),
        move |(active,): (bool,), _: &dbus::blocking::Connection, _: &dbus::Message| {
            flag.store(active, Ordering::Relaxed);
            true
        },
    );
    if let Err(e) = watch {
        log::warn!("D-Bus: cannot watch the GNOME lock screen: {e}");
    }
}

/// Stores GNOME's lock screen state into `locked`. Only a missing reply is returned, because
/// then a lock screen service exists but did not answer, as when gnome-shell stalls. Any other
/// failure counts as unlocked, as on desktops without GNOME's lock screen.
fn read_lock_screen(
    conn: &dbus::blocking::Connection,
    locked: &AtomicBool,
) -> Result<(), dbus::Error> {
    let screensaver = conn.with_proxy(
        "org.gnome.ScreenSaver",
        "/org/gnome/ScreenSaver",
        CALL_TIMEOUT,
    );
    let active: Result<(bool,), dbus::Error> =
        screensaver.method_call("org.gnome.ScreenSaver", "GetActive", ());
    match active {
        Ok((active,)) => locked.store(active, Ordering::Relaxed),
        Err(e) => {
            locked.store(false, Ordering::Relaxed);
            if is_no_reply(&e) {
                return Err(e);
            }
            log::warn!("D-Bus: GetActive failed, assuming unlocked: {e}");
        }
    }
    Ok(())
}

/// Why the monitor stops using its connection.
enum Exit {
    Reconnect,
    /// The main loop has gone.
    Stop,
}

/// The monitor thread's state, kept across connections.
struct IdleMonitor {
    events: Sender<DbusIdleEvent>,
    commands: mpsc::Receiver<GnomeCommand>,
    /// Shared with `GnomeSession::locked`. False whenever the state cannot be read, so a stale
    /// true cannot refuse activations for good.
    locked: Arc<AtomicBool>,
    timeout_ms: u64,
    hold: KeepAwakeHold,
    /// GetActive retries left after the read at connect got no reply, one per tick in which
    /// GetIdletime answers. While any are left the session may be locked, so the monitor
    /// neither activates nor takes the hold; after the last one it assumes unlocked.
    lock_retries: u32,
    was_locked: bool,
    was_idle: bool,
    /// One timeout after a hide without input; no activation before it.
    activate_after: Option<Instant>,
    /// Failed attempts in a row to read the idle time, connection failures included.
    failures: u32,
    next_poll: Instant,
    gate_warned_at: Option<Instant>,
}

impl Drop for IdleMonitor {
    fn drop(&mut self) {
        self.locked.store(false, Ordering::Relaxed);
    }
}

impl IdleMonitor {
    /// Retries until the session bus accepts a connection.
    fn connect(&mut self) -> dbus::blocking::Connection {
        loop {
            match dbus::blocking::Connection::new_session() {
                Ok(conn) => {
                    watch_lock_screen(&conn, &self.locked);
                    self.lock_retries = match read_lock_screen(&conn, &self.locked) {
                        Ok(()) => 0,
                        Err(e) => {
                            log::warn!(
                                "D-Bus: GetActive got no reply, so activation waits for a retry: {e}"
                            );
                            LOCK_READ_RETRIES
                        }
                    };
                    self.hold.forget_connection();
                    return conn;
                }
                Err(e) => {
                    self.locked.store(false, Ordering::Relaxed);
                    self.record_failure("cannot connect to the session bus", &e);
                    std::thread::sleep(self.next_poll.saturating_duration_since(Instant::now()));
                }
            }
        }
    }

    fn tick(&mut self, conn: &dbus::blocking::Connection) -> Result<(), Exit> {
        // `process` handles at most one message per call and returns false once `wait` passes
        // without one. It fails only once the connection is gone.
        let mut wait = Duration::from_secs(1);
        loop {
            match conn.process(wait) {
                Ok(true) => wait = Duration::ZERO,
                Ok(false) => break,
                Err(e) => {
                    self.record_failure("lost the session bus connection", &e);
                    std::thread::sleep(self.next_poll.saturating_duration_since(Instant::now()));
                    return Err(Exit::Reconnect);
                }
            }
        }

        while let Ok(command) = self.commands.try_recv() {
            match command {
                GnomeCommand::SetKeepAwake(on) => self.hold.wanted = on,
                GnomeCommand::Rearm => {
                    self.was_idle = false;
                    self.activate_after =
                        Some(Instant::now() + Duration::from_millis(self.timeout_ms));
                }
            }
        }

        let session = conn.with_proxy(
            "org.gnome.SessionManager",
            "/org/gnome/SessionManager",
            CALL_TIMEOUT,
        );
        if self.lock_retries == 0 {
            let locked = self.locked.load(Ordering::Relaxed);
            if locked != self.was_locked {
                self.was_locked = locked;
                self.send(if locked {
                    DbusIdleEvent::Locked
                } else {
                    DbusIdleEvent::Unlocked
                })?;
            }
            if self.hold.reconcile(&session, locked) {
                return Err(Exit::Reconnect);
            }
        }

        if Instant::now() < self.next_poll {
            return Ok(());
        }
        let idle_ms = match get_mutter_idletime(conn) {
            Ok(ms) => ms,
            Err(e) => {
                self.record_failure("GetIdletime failed", &e);
                return match self.failures {
                    RECONNECT_AFTER => Err(Exit::Reconnect),
                    _ => Ok(()),
                };
            }
        };
        if self.failures > 0 {
            log::info!(
                "D-Bus: GetIdletime works again after {} failures",
                self.failures
            );
            self.failures = 0;
        }

        if self.lock_retries > 0 {
            match read_lock_screen(conn, &self.locked) {
                Ok(()) => self.lock_retries = 0,
                Err(e) => {
                    self.lock_retries -= 1;
                    if self.lock_retries > 0 {
                        log::debug!("D-Bus: GetActive still gets no reply: {e}");
                        return Ok(());
                    }
                    log::warn!("D-Bus: GetActive failed, assuming unlocked: {e}");
                }
            }
        }
        if self.locked.load(Ordering::Relaxed) {
            return Ok(());
        }
        if !self.was_idle && idle_ms >= self.timeout_ms && self.activation_allowed(&session) {
            log::info!("D-Bus: user idle ({idle_ms}ms) — activating screensaver");
            self.send(DbusIdleEvent::Idle)?;
            self.was_idle = true;
        } else if self.was_idle && idle_ms < self.timeout_ms {
            log::info!("D-Bus: user resumed — deactivating screensaver");
            self.send(DbusIdleEvent::Resumed)?;
            self.was_idle = false;
        }
        Ok(())
    }

    /// Not within one timeout of a hide without input, and not while another app inhibits idle.
    fn activation_allowed(
        &mut self,
        session: &dbus::blocking::Proxy<&dbus::blocking::Connection>,
    ) -> bool {
        if self.activate_after.is_some_and(|t| Instant::now() < t) {
            return false;
        }
        match idle_inhibited_by_others(session) {
            Ok(false) => true,
            Ok(true) => {
                log::debug!("D-Bus: idle inhibited — skipping activation");
                false
            }
            Err(e) => {
                if self
                    .gate_warned_at
                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(60))
                {
                    log::warn!("D-Bus: cannot read idle inhibitors, skipping activation: {e}");
                    self.gate_warned_at = Some(Instant::now());
                }
                false
            }
        }
    }

    /// Counts a failed attempt to read the idle time, connection failures included, and
    /// schedules the next one.
    fn record_failure(&mut self, what: &str, e: &dbus::Error) {
        self.failures += 1;
        let delay = retry_delay(self.failures);
        self.next_poll = Instant::now() + delay;
        let (n, secs) = (self.failures, delay.as_secs());
        match n {
            1 => log::warn!("D-Bus: {what}, retrying in {secs}s: {e}"),
            n if n < RECONNECT_AFTER => {
                log::info!("D-Bus: {what} ({n} failures in a row), retrying in {secs}s: {e}")
            }
            RECONNECT_AFTER => log::error!(
                "D-Bus: {what} ({n} failures in a row), so no idle source works; reconnecting, then retrying in {secs}s: {e}"
            ),
            _ => log::debug!("D-Bus: {what} ({n} failures in a row), retrying in {secs}s: {e}"),
        }
    }

    fn send(&self, event: DbusIdleEvent) -> Result<(), Exit> {
        self.events.send(event).map_err(|_| Exit::Stop)
    }
}

/// Never returns while the main loop runs. Failed calls are retried with a growing delay, and
/// the connection is replaced when it drops, after `RECONNECT_AFTER` failures in a row, and
/// after an Inhibit that got no reply.
fn dbus_idle_monitor_loop(
    events: Sender<DbusIdleEvent>,
    commands: mpsc::Receiver<GnomeCommand>,
    locked: Arc<AtomicBool>,
    timeout_ms: u64,
) {
    let mut monitor = IdleMonitor {
        events,
        commands,
        locked,
        timeout_ms,
        hold: KeepAwakeHold::default(),
        lock_retries: 0,
        was_locked: false,
        was_idle: false,
        activate_after: None,
        failures: 0,
        next_poll: Instant::now(),
        gate_warned_at: None,
    };
    loop {
        let conn = monitor.connect();
        loop {
            match monitor.tick(&conn) {
                Ok(()) => {}
                Err(Exit::Reconnect) => break,
                Err(Exit::Stop) => return,
            }
        }
    }
}

struct Screensaver {
    #[allow(dead_code)]
    window: Window,
    compositor: Option<Compositor>,
    inhibitor: Option<ZwpIdleInhibitorV1>,
    activated_at: Instant,
}

struct App {
    /// One instance per process, created at startup: a new instance loads the Vulkan drivers
    /// from disk, and after an NVIDIA update that has not been rebooted into, the new driver
    /// no longer matches the loaded kernel module. `None` from a failed show until the next.
    gpu_instance: Option<wgpu::Instance>,
    conn: Connection,
    registry_state: RegistryState,
    compositor_state: CompositorState,
    output_state: OutputState,
    seat_state: SeatState,
    xdg_shell: XdgShell,
    screensaver: Option<Screensaver>,
    idle_notifier: Option<ExtIdleNotifierV1>,
    idle_notification: Option<ExtIdleNotificationV1>,
    idle_inhibit_manager: Option<ZwpIdleInhibitManagerV1>,
    gnome: Option<GnomeSession>,
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    config: Config,
    engine: Engine,
    data_cache: Arc<RwLock<DataCache>>,
    weather_started: bool,
    width: u32,
    height: u32,
    running: bool,
    signal_activate: bool,
    signal_deactivate: bool,
    signal_locked: bool,
    ready_to_draw: bool,
    last_draw: Option<Instant>,
    debug: bool,
    frame_count: u64,
}

impl App {
    fn activate(&mut self, qh: &QueueHandle<Self>) {
        if self.screensaver.is_some() {
            return;
        }
        log::info!("Activating screensaver");

        let surface = self.compositor_state.create_surface(qh);
        let window = self
            .xdg_shell
            .create_window(surface, WindowDecorations::None, qh);
        window.set_fullscreen(None);
        window.set_title("olsvr");
        window.set_app_id("olsvr");
        window.commit();

        let inhibitor = self
            .idle_inhibit_manager
            .as_ref()
            .filter(|_| self.config.activation.keep_awake != KeepAwake::Off)
            .map(|mgr| mgr.create_inhibitor(window.wl_surface(), qh, ()));

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

        self.screensaver = Some(Screensaver {
            window,
            compositor: None,
            inhibitor,
            activated_at: Instant::now(),
        });
    }

    /// Execute the commands the `Engine` produced for an event.
    fn apply(&mut self, cmds: Vec<EngineCommand>, qh: &QueueHandle<Self>) {
        for cmd in cmds {
            match cmd {
                EngineCommand::Show { .. } => self.activate(qh),
                EngineCommand::Hide => self.deactivate(),
                EngineCommand::SetKeepAwake(on) => self.set_keep_awake(on),
                EngineCommand::Quit => self.running = false,
            }
        }
    }

    /// Only `always` reaches GNOME. Under `while-active` the saver surface's zwp inhibitor is
    /// the hold, and mutter relays it to gnome-session while the saver is visible.
    fn set_keep_awake(&self, on: bool) {
        if self.config.activation.keep_awake != KeepAwake::Always {
            return;
        }
        match &self.gnome {
            Some(gnome) => gnome.set_keep_awake(on),
            None if on => log::error!(
                "keep_awake = \"always\" is not supported on this desktop (Wayland: GNOME only)"
            ),
            None => {}
        }
    }

    fn on_event(&mut self, ev: BackendEvent, qh: &QueueHandle<Self>) {
        let cmds = self.engine.handle(ev.clone(), Instant::now());
        if cmds.contains(&EngineCommand::Hide) {
            log::info!("Hide via {ev:?}");
        }
        self.apply(cmds, qh);
    }

    fn dismiss(&mut self, qh: &QueueHandle<Self>) {
        self.on_event(BackendEvent::DismissInput, qh);
    }

    /// A hide the user did not cause (a failed show, or the compositor closing the window),
    /// after which the GNOME monitor has to re-arm.
    fn hide_without_input(&mut self, ev: BackendEvent, qh: &QueueHandle<Self>) {
        self.on_event(ev, qh);
        if let Some(gnome) = &self.gnome {
            gnome.rearm();
        }
    }

    fn deactivate(&mut self) {
        if let Some(ss) = self.screensaver.take() {
            log::info!("Deactivating screensaver");
            if let Some(inhibitor) = ss.inhibitor {
                inhibitor.destroy();
            }
        }
    }

    fn draw(&mut self, from_callback: bool) -> FrameOutcome {
        let debug_info = if self.debug {
            let frame_ms = self
                .last_draw
                .map(|t| t.elapsed().as_millis() as u64)
                .unwrap_or(0);
            Some(DebugInfo {
                frame_count: self.frame_count,
                frame_ms,
                from_callback,
                uptime_secs: self
                    .screensaver
                    .as_ref()
                    .map(|ss| ss.activated_at.elapsed().as_secs())
                    .unwrap_or(0),
            })
        } else {
            None
        };
        self.frame_count += 1;

        if let Some(ref mut ss) = self.screensaver
            && let Some(ref mut comp) = ss.compositor
        {
            comp.render_frame(debug_info.as_ref())
        } else {
            FrameOutcome::Skipped
        }
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        log::debug!("Frame callback received");
        self.ready_to_draw = true;
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Some(info) = self.output_state.info(&output)
            && let Some(mode) = info.modes.iter().find(|m| m.current)
        {
            self.width = mode.dimensions.0 as u32;
            self.height = mode.dimensions.1 as u32;
            log::info!("Output: {}x{}", self.width, self.height);
        }
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Some(info) = self.output_state.info(&output)
            && let Some(mode) = info.modes.iter().find(|m| m.current)
        {
            self.width = mode.dimensions.0 as u32;
            self.height = mode.dimensions.1 as u32;
        }
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.seat.is_none() {
            self.seat = Some(seat);
        }
    }

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let keyboard = self
                .seat_state
                .get_keyboard(qh, &seat, None)
                .expect("Failed to create keyboard");
            self.keyboard = Some(keyboard);
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            let pointer = self
                .seat_state
                .get_pointer(qh, &seat)
                .expect("Failed to create pointer");
            self.pointer = Some(pointer);
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && let Some(kb) = self.keyboard.take()
        {
            kb.release();
        }
        if capability == Capability::Pointer
            && let Some(ptr) = self.pointer.take()
        {
            ptr.release();
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {
    }
}

impl WindowHandler for App {
    fn request_close(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, _window: &Window) {
        self.hide_without_input(BackendEvent::SurfaceClosed, qh);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let (w, h) = (
            configure
                .new_size
                .0
                .map(NonZeroU32::get)
                .unwrap_or(self.width),
            configure
                .new_size
                .1
                .map(NonZeroU32::get)
                .unwrap_or(self.height),
        );

        if let Some(ref mut ss) = self.screensaver {
            if w == 0 || h == 0 {
                return;
            }

            if ss.compositor.is_none() {
                log::info!("Window configured: {w}x{h}, initializing compositor");
                let (layer_list, layer_configs) = self.config.build_layers(self.debug);
                let instance = self.gpu_instance.get_or_insert_with(vulkan_instance);
                ss.compositor = Compositor::new_wayland(
                    instance,
                    &self.conn,
                    window.wl_surface(),
                    w,
                    h,
                    layer_list,
                    &layer_configs,
                    self.data_cache.clone(),
                );
                if ss.compositor.is_none() {
                    // A GPU reset can leave the instance itself unusable, so the next show
                    // starts from a fresh one.
                    self.gpu_instance = None;
                    self.hide_without_input(BackendEvent::ShowFailed, qh);
                }
            } else if let Some(ref mut comp) = ss.compositor {
                comp.resize(w, h);
            }
        }
    }
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
        self.dismiss(qh);
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: Modifiers,
        _raw_modifiers: RawModifiers,
        _layout: u32,
    ) {
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    pointer.set_cursor(serial, None, 0, 0);
                }
                PointerEventKind::Leave { .. } => {}
                _ => {
                    self.dismiss(qh);
                    return;
                }
            }
        }
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for App {
    fn event(
        state: &mut Self,
        _notification: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => {
                log::info!("User idle — activating screensaver");
                state.on_event(BackendEvent::Idle, qh);
            }
            ext_idle_notification_v1::Event::Resumed => {
                log::info!("User resumed — deactivating screensaver");
                state.on_event(BackendEvent::Resume, qh);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for App {
    fn event(
        _state: &mut Self,
        _notifier: &ExtIdleNotifierV1,
        _event: ext_idle_notifier_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpIdleInhibitManagerV1, ()> for App {
    fn event(
        _state: &mut Self,
        _manager: &ZwpIdleInhibitManagerV1,
        _event: <ZwpIdleInhibitManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpIdleInhibitorV1, ()> for App {
    fn event(
        _state: &mut Self,
        _inhibitor: &ZwpIdleInhibitorV1,
        _event: <ZwpIdleInhibitorV1 as wayland_client::Proxy>::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers!(OutputState, SeatState);
}

delegate_compositor!(App);
delegate_keyboard!(App);
delegate_output!(App);
delegate_pointer!(App);
delegate_registry!(App);
delegate_seat!(App);
delegate_xdg_shell!(App);
delegate_xdg_window!(App);

fn vulkan_instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    })
}

pub fn run(args: RunArgs) {
    // With DISPLAY set, NVIDIA's Vulkan driver connects to X11 and leaks a /dev/nvidiactl and
    // renderD fd pair per destroyed instance (NVIDIA bug 6446591, fixed in 615.71.09).
    // SAFETY: no thread has been started yet, so nothing can read the environment concurrently.
    unsafe {
        std::env::remove_var("DISPLAY");
        std::env::remove_var("XAUTHORITY");
    }

    // --activate: send SIGUSR1 to running instance and exit
    if args.activate {
        let path = pid_path();
        if let Ok(contents) = std::fs::read_to_string(&path)
            && let Ok(pid) = contents.trim().parse::<i32>()
        {
            log::info!("Sending SIGUSR1 to PID {pid}");
            unsafe { libc::kill(pid, libc::SIGUSR1) };
            return;
        }
        eprintln!(
            "No running olsvr instance found (no PID file at {})",
            path.display()
        );
        std::process::exit(1);
    }

    let mut config = Config::load();
    merge_cli(&mut config, &args);

    // Write PID file
    let path = pid_path();
    std::fs::write(&path, std::process::id().to_string()).expect("Failed to write PID file");

    let conn = Connection::connect_to_env().expect("Failed to connect to Wayland");
    let (globals, event_queue) = registry_queue_init(&conn).expect("Failed to init registry");
    let qh = event_queue.handle();

    let compositor_state =
        CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg_wm_base not available");

    let idle_notifier: Option<ExtIdleNotifierV1> = globals.bind(&qh, 1..=1, ()).ok();
    if idle_notifier.is_none() {
        log::info!("ext_idle_notifier_v1 not available — trying D-Bus fallback");
    }

    let idle_inhibit_manager: Option<ZwpIdleInhibitManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    if idle_inhibit_manager.is_none() {
        log::warn!("zwp_idle_inhibit_manager_v1 not available — idle inhibition disabled");
    }

    let data_cache = Arc::new(RwLock::new(DataCache::default()));

    // The Engine owns the activate/deactivate decisions and grace periods. On
    // Wayland a single fullscreen surface is used regardless of the display set,
    // so a one-element display list is sufficient.
    let engine = Engine::new(config.activation, vec![0]);

    // Blocks these signals for this thread only, so it comes before the Vulkan instance below:
    // threads the drivers start inherit the mask, and a signal one of them took would kill
    // the process instead of reaching the event loop.
    let signals = Signals::new(&[Signal::SIGUSR1, Signal::SIGTERM, Signal::SIGINT])
        .expect("Failed to create signal source");

    let mut app = App {
        gpu_instance: Some(vulkan_instance()),
        conn: conn.clone(),
        registry_state: RegistryState::new(&globals),
        compositor_state,
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        xdg_shell,
        screensaver: None,
        idle_notifier,
        idle_notification: None,
        idle_inhibit_manager,
        gnome: None,
        seat: None,
        keyboard: None,
        pointer: None,
        config,
        engine,
        data_cache,
        weather_started: false,
        width: 0,
        height: 0,
        running: true,
        signal_activate: args.now,
        signal_deactivate: false,
        signal_locked: false,
        ready_to_draw: false,
        last_draw: None,
        debug: args.debug,
        frame_count: 0,
    };

    let mut event_loop: EventLoop<App> = EventLoop::try_new().expect("Failed to create event loop");

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .expect("Failed to insert Wayland source");

    event_loop
        .handle()
        .insert_source(signals, |event, _, app| match event.signal() {
            Signal::SIGUSR1 => {
                log::info!("Received SIGUSR1 — activating screensaver");
                app.signal_activate = true;
            }
            Signal::SIGTERM | Signal::SIGINT => {
                log::info!("Received shutdown signal");
                app.running = false;
            }
            _ => {}
        })
        .expect("Failed to insert signal source");

    event_loop
        .dispatch(std::time::Duration::from_millis(100), &mut app)
        .expect("Initial dispatch failed");

    if let (Some(notifier), Some(seat)) = (&app.idle_notifier, &app.seat) {
        let timeout_ms =
            u32::try_from(u64::from(app.config.timeout).saturating_mul(60_000)).unwrap_or(u32::MAX);
        log::info!("Setting up Wayland idle notification: {}ms", timeout_ms);
        let notification = notifier.get_idle_notification(timeout_ms, seat, &qh, ());
        app.idle_notification = Some(notification);
    } else {
        if app.idle_notifier.is_some() {
            log::error!(
                "ext_idle_notifier_v1 is available but has no seat to watch, so idle detection falls back to the GNOME D-Bus monitor"
            );
        }
        // Started without a probe: mutter claims its D-Bus name asynchronously, and the monitor
        // retries until it answers.
        let timeout_ms = app.config.timeout as u64 * 60 * 1000;
        log::info!("Using D-Bus Mutter IdleMonitor fallback: {}ms", timeout_ms);
        let (tx, rx) = channel::channel::<DbusIdleEvent>();
        app.gnome = Some(GnomeSession::spawn(tx, timeout_ms));
        event_loop
            .handle()
            .insert_source(rx, |event, _, app: &mut App| {
                let channel::Event::Msg(msg) = event else {
                    log::error!(
                        "D-Bus monitor has stopped, so the saver no longer activates on idle"
                    );
                    return;
                };
                match msg {
                    DbusIdleEvent::Idle => app.signal_activate = true,
                    DbusIdleEvent::Resumed => app.signal_deactivate = true,
                    DbusIdleEvent::Locked => {
                        log::info!("Session locked");
                        app.signal_locked = true;
                    }
                    DbusIdleEvent::Unlocked => log::info!("Session unlocked"),
                }
            })
            .expect("Failed to insert D-Bus channel source");
    }

    // Apply any startup commands (e.g. hold keep-awake under the "always" policy). This comes
    // after the D-Bus fallback above, which `always` needs on GNOME.
    let init_cmds = app.engine.init();
    app.apply(init_cmds, &qh);

    let has_compositor = |app: &App| {
        app.screensaver
            .as_ref()
            .is_some_and(|ss| ss.compositor.is_some())
    };
    let mut slow_frame_warned_at: Option<Instant> = None;
    let mut dispatch_blocked_warned_at: Option<Instant> = None;
    while app.running {
        let had_compositor = has_compositor(&app);
        let t0 = Instant::now();
        event_loop
            .dispatch(std::time::Duration::from_millis(16), &mut app)
            .expect("Event loop dispatch failed");
        let dispatch_ms = t0.elapsed().as_millis();
        // The dispatch that creates the compositor builds the GPU device, so every show takes
        // that long and it is not a stall.
        let first_frame = !had_compositor && has_compositor(&app);

        if app.signal_activate {
            app.signal_activate = false;
            if app.gnome.as_ref().is_some_and(GnomeSession::locked) {
                log::info!("Session locked, not activating");
            } else {
                app.on_event(BackendEvent::ActivateSignal, &qh);
            }
        }
        if app.signal_deactivate {
            app.signal_deactivate = false;
            // The Engine owns the resume grace period now.
            app.on_event(BackendEvent::Resume, &qh);
        }
        if app.signal_locked {
            app.signal_locked = false;
            // No grace: the saver cannot draw over the lock screen, and its zwp inhibitor must
            // go so that gsd-power can blank the locked panel.
            app.on_event(BackendEvent::SurfaceClosed, &qh);
        }

        // Draw when the compositor signals readiness (frame callback), or
        // after 100ms as a fallback if the callback chain breaks.
        let should_draw = app.screensaver.is_some()
            && (app.ready_to_draw
                || app
                    .last_draw
                    .is_none_or(|t| t.elapsed() > std::time::Duration::from_millis(100)));
        if should_draw {
            let from_callback = app.ready_to_draw;
            let surface = app
                .screensaver
                .as_ref()
                .unwrap()
                .window
                .wl_surface()
                .clone();
            let t1 = Instant::now();
            let outcome = app.draw(from_callback);
            let draw_ms = t1.elapsed().as_millis();
            match outcome {
                FrameOutcome::Presented => {
                    surface.frame(&qh, surface.clone());
                    surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
                    surface.commit();
                }
                // No frame callback is requested, so the 100ms fallback retries.
                FrameOutcome::Skipped => {}
                FrameOutcome::Broken => app.hide_without_input(BackendEvent::ShowFailed, &qh),
            }

            if !first_frame
                && (dispatch_ms > 50 || draw_ms > 50)
                && warning_due(&mut slow_frame_warned_at)
            {
                log::warn!("Slow frame: dispatch={dispatch_ms}ms draw={draw_ms}ms");
            }
            app.ready_to_draw = false;
            app.last_draw = Some(Instant::now());
        } else if app.screensaver.is_some()
            && !first_frame
            && dispatch_ms > 50
            && warning_due(&mut dispatch_blocked_warned_at)
        {
            log::warn!("Dispatch blocked {dispatch_ms}ms (no draw)");
        }
    }

    let _ = std::fs::remove_file(pid_path());
}

// `main`, `stop`, the CLI types, `pid_path`, and `merge_cli` now live in the
// crate root (`src/main.rs`). This module exposes `pub fn run` as the backend.
