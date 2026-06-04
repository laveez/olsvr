//! Wayland (Linux) backend — moved verbatim from the original `main.rs`.
//! The shared CLI and helpers (`Cli`, `RunArgs`, `pid_path`, `merge_cli`, `stop`,
//! `main`) live in the crate root (`src/main.rs`). Compiled only on Linux.

use std::num::NonZeroU32;
use std::sync::{Arc, RwLock};
use std::time::Instant;

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

use crate::compositor::{Compositor, DebugInfo};
use crate::config::Config;
use crate::data::{self, DataCache};
use crate::{RunArgs, merge_cli, pid_path};

enum DbusIdleEvent {
    Idle,
    Resumed,
}

fn get_mutter_idletime(proxy: &dbus::blocking::Proxy<&dbus::blocking::Connection>) -> Option<u64> {
    proxy
        .method_call("org.gnome.Mutter.IdleMonitor", "GetIdletime", ())
        .ok()
        .map(|(ms,): (u64,)| ms)
}

fn is_idle_inhibited(proxy: &dbus::blocking::Proxy<&dbus::blocking::Connection>) -> bool {
    proxy
        .method_call("org.gnome.SessionManager", "IsInhibited", (8u32,))
        .ok()
        .map(|(inhibited,): (bool,)| inhibited)
        .unwrap_or(false)
}

fn dbus_idle_monitor_loop(tx: Sender<DbusIdleEvent>, timeout_ms: u64) {
    let conn = match dbus::blocking::Connection::new_session() {
        Ok(c) => c,
        Err(e) => {
            log::error!("D-Bus session connection failed: {e}");
            return;
        }
    };
    let idle_proxy = conn.with_proxy(
        "org.gnome.Mutter.IdleMonitor",
        "/org/gnome/Mutter/IdleMonitor/Core",
        std::time::Duration::from_secs(2),
    );
    let session_proxy = conn.with_proxy(
        "org.gnome.SessionManager",
        "/org/gnome/SessionManager",
        std::time::Duration::from_secs(2),
    );

    let mut was_idle = false;
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));

        let Some(idle_ms) = get_mutter_idletime(&idle_proxy) else {
            log::error!("GetIdletime call failed — stopping D-Bus monitor");
            return;
        };

        if !was_idle && idle_ms >= timeout_ms {
            if is_idle_inhibited(&session_proxy) {
                log::debug!("D-Bus: idle inhibited — skipping activation");
                continue;
            }
            log::info!("D-Bus: user idle ({idle_ms}ms) — activating screensaver");
            if tx.send(DbusIdleEvent::Idle).is_err() {
                return;
            }
            was_idle = true;
        } else if was_idle && idle_ms < timeout_ms {
            log::info!("D-Bus: user resumed — deactivating screensaver");
            if tx.send(DbusIdleEvent::Resumed).is_err() {
                return;
            }
            was_idle = false;
        }
    }
}

struct Screensaver {
    #[allow(dead_code)]
    window: Window,
    compositor: Option<Compositor>,
    inhibitor: Option<ZwpIdleInhibitorV1>,
    dbus_inhibit_cookie: Option<u32>,
    activated_at: Instant,
}

struct App {
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
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    config: Config,
    data_cache: Arc<RwLock<DataCache>>,
    width: u32,
    height: u32,
    running: bool,
    signal_activate: bool,
    signal_deactivate: bool,
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
            .map(|mgr| mgr.create_inhibitor(window.wl_surface(), qh, ()));

        // D-Bus ScreenSaver inhibit as belt-and-suspenders for GNOME DPMS
        let dbus_inhibit_cookie = dbus::blocking::Connection::new_session()
            .ok()
            .and_then(|conn| {
                let proxy = conn.with_proxy(
                    "org.freedesktop.ScreenSaver",
                    "/org/freedesktop/ScreenSaver",
                    std::time::Duration::from_secs(2),
                );
                let result: Result<(u32,), _> = proxy.method_call(
                    "org.freedesktop.ScreenSaver",
                    "Inhibit",
                    ("olsvr", "Screensaver active"),
                );
                match result {
                    Ok((cookie,)) => {
                        log::info!("D-Bus ScreenSaver inhibited (cookie={cookie})");
                        Some(cookie)
                    }
                    Err(e) => {
                        log::warn!("D-Bus ScreenSaver.Inhibit failed: {e}");
                        None
                    }
                }
            });

        // Start weather fetch thread if a weather layer is configured
        if let Some((location, interval_secs)) = self.config.weather_config() {
            data::start_fetch_thread(
                self.data_cache.clone(),
                location,
                std::time::Duration::from_secs(interval_secs),
            );
        }

        self.screensaver = Some(Screensaver {
            window,
            compositor: None,
            inhibitor,
            dbus_inhibit_cookie,
            activated_at: Instant::now(),
        });
    }

    fn dismiss(&mut self) {
        if let Some(ref ss) = self.screensaver
            && ss.activated_at.elapsed() < std::time::Duration::from_secs(1)
        {
            return;
        }
        self.deactivate();
    }

    fn deactivate(&mut self) {
        if let Some(ss) = self.screensaver.take() {
            log::info!("Deactivating screensaver");
            if let Some(inhibitor) = ss.inhibitor {
                inhibitor.destroy();
            }
            if let Some(cookie) = ss.dbus_inhibit_cookie
                && let Ok(conn) = dbus::blocking::Connection::new_session()
            {
                let proxy = conn.with_proxy(
                    "org.freedesktop.ScreenSaver",
                    "/org/freedesktop/ScreenSaver",
                    std::time::Duration::from_secs(2),
                );
                let _: Result<(), _> =
                    proxy.method_call("org.freedesktop.ScreenSaver", "UnInhibit", (cookie,));
                log::info!("D-Bus ScreenSaver uninhibited (cookie={cookie})");
            }
        }
    }

    fn draw(&mut self, from_callback: bool) {
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
            comp.render_frame(debug_info.as_ref());
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
    fn request_close(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _window: &Window) {
        self.deactivate();
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
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
                let comp = Compositor::new(
                    &self.conn,
                    window.wl_surface(),
                    w,
                    h,
                    layer_list,
                    &layer_configs,
                    self.data_cache.clone(),
                );
                ss.compositor = Some(comp);
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
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
        self.dismiss();
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
        _qh: &QueueHandle<Self>,
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
                    self.dismiss();
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
                state.activate(qh);
            }
            ext_idle_notification_v1::Event::Resumed => {
                log::info!("User resumed — deactivating screensaver");
                state.deactivate();
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

pub fn run(args: RunArgs) {
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
    let mut dbus_idle = false;
    if idle_notifier.is_none() {
        log::info!("ext_idle_notifier_v1 not available — trying D-Bus fallback");
    }

    let idle_inhibit_manager: Option<ZwpIdleInhibitManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    if idle_inhibit_manager.is_none() {
        log::warn!("zwp_idle_inhibit_manager_v1 not available — idle inhibition disabled");
    }

    let data_cache = Arc::new(RwLock::new(DataCache::default()));

    let mut app = App {
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
        seat: None,
        keyboard: None,
        pointer: None,
        config,
        data_cache,
        width: 0,
        height: 0,
        running: true,
        signal_activate: args.now,
        signal_deactivate: false,
        ready_to_draw: false,
        last_draw: None,
        debug: args.debug,
        frame_count: 0,
    };

    let mut event_loop: EventLoop<App> = EventLoop::try_new().expect("Failed to create event loop");

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .expect("Failed to insert Wayland source");

    let signals = Signals::new(&[Signal::SIGUSR1, Signal::SIGTERM, Signal::SIGINT])
        .expect("Failed to create signal source");
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
        let timeout_ms = app.config.timeout * 60 * 1000;
        log::info!("Setting up Wayland idle notification: {}ms", timeout_ms);
        let notification = notifier.get_idle_notification(timeout_ms, seat, &qh, ());
        app.idle_notification = Some(notification);
    } else if app.idle_notifier.is_none() {
        let timeout_ms = app.config.timeout as u64 * 60 * 1000;
        if let Ok(conn) = dbus::blocking::Connection::new_session() {
            let proxy = conn.with_proxy(
                "org.gnome.Mutter.IdleMonitor",
                "/org/gnome/Mutter/IdleMonitor/Core",
                std::time::Duration::from_secs(2),
            );
            if get_mutter_idletime(&proxy).is_some() {
                log::info!("Using D-Bus Mutter IdleMonitor fallback: {}ms", timeout_ms);
                let (tx, rx) = channel::channel::<DbusIdleEvent>();
                std::thread::spawn(move || dbus_idle_monitor_loop(tx, timeout_ms));
                event_loop
                    .handle()
                    .insert_source(rx, |event, _, app: &mut App| {
                        if let channel::Event::Msg(msg) = event {
                            match msg {
                                DbusIdleEvent::Idle => app.signal_activate = true,
                                DbusIdleEvent::Resumed => app.signal_deactivate = true,
                            }
                        }
                    })
                    .expect("Failed to insert D-Bus channel source");
                dbus_idle = true;
            }
        }
        if !dbus_idle {
            log::warn!("No idle detection available — use --now or --activate");
        }
    }

    while app.running {
        let t0 = Instant::now();
        event_loop
            .dispatch(std::time::Duration::from_millis(16), &mut app)
            .expect("Event loop dispatch failed");
        let dispatch_ms = t0.elapsed().as_millis();

        if app.signal_activate {
            app.signal_activate = false;
            app.activate(&qh);
        }
        if app.signal_deactivate {
            app.signal_deactivate = false;
            // Grace period: ignore D-Bus resume if screensaver just activated
            // (creating the window resets Mutter's idle timer)
            let recent = app
                .screensaver
                .as_ref()
                .is_some_and(|ss| ss.activated_at.elapsed() < std::time::Duration::from_secs(5));
            if recent {
                log::info!("Ignoring D-Bus resume — screensaver activated <5s ago");
            } else {
                app.deactivate();
            }
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
            app.draw(from_callback);
            let draw_ms = t1.elapsed().as_millis();
            surface.frame(&qh, surface.clone());
            surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
            surface.commit();

            if dispatch_ms > 50 || draw_ms > 50 {
                log::warn!("Slow frame: dispatch={dispatch_ms}ms draw={draw_ms}ms");
            }
            app.ready_to_draw = false;
            app.last_draw = Some(Instant::now());
        } else if app.screensaver.is_some() && dispatch_ms > 50 {
            log::warn!("Dispatch blocked {dispatch_ms}ms (no draw)");
        }
    }

    let _ = std::fs::remove_file(pid_path());
}

// `main`, `stop`, the CLI types, `pid_path`, and `merge_cli` now live in the
// crate root (`src/main.rs`). This module exposes `pub fn run` as the backend.
