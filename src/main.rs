mod animation;
mod renderer;

use std::num::NonZeroU32;

use clap::Parser;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_output, delegate_registry, delegate_seat, delegate_xdg_shell,
    delegate_xdg_window,
    output::{OutputHandler, OutputState},
    reexports::calloop::EventLoop,
    reexports::calloop_wayland_source::WaylandSource,
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{Capability, SeatHandler, SeatState},
    shell::WaylandSurface,
    shell::xdg::{
        window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
        XdgShell,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_seat, wl_surface},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::{self, ExtIdleNotifierV1},
};

use crate::animation::Animation;
use crate::renderer::Renderer;

#[derive(Parser)]
#[command(name = "olsvr", about = "OLED screensaver for Wayland")]
struct Args {
    /// Idle timeout in minutes before activation
    #[arg(long, default_value_t = 5)]
    timeout: u32,

    /// Immediately activate the screensaver
    #[arg(long)]
    activate: bool,

    /// Clock font size in pixels
    #[arg(long, default_value_t = 200)]
    font_size: u32,

    /// Hold duration between fades in seconds
    #[arg(long, default_value_t = 10)]
    hold: u32,
}

struct Screensaver {
    window: Window,
    renderer: Option<Renderer>,
    animation: Option<Animation>,
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
    seat: Option<wl_seat::WlSeat>,
    font_size: u32,
    hold: u32,
    width: u32,
    height: u32,
    running: bool,
}

impl App {
    fn activate(&mut self, qh: &QueueHandle<Self>) {
        if self.screensaver.is_some() {
            return;
        }
        log::info!("Activating screensaver");

        let surface = self.compositor_state.create_surface(qh);
        let window = self.xdg_shell.create_window(surface, WindowDecorations::None, qh);
        window.set_fullscreen(None);
        window.set_title("olsvr");
        window.set_app_id("olsvr");
        window.commit();

        // Renderer/animation are created in configure callback when we know the size
        self.screensaver = Some(Screensaver {
            window,
            renderer: None,
            animation: None,
        });
    }

    fn deactivate(&mut self) {
        if self.screensaver.is_none() {
            return;
        }
        log::info!("Deactivating screensaver");
        // Drop destroys the window and its Wayland resources
        self.screensaver = None;
    }

    fn draw(&mut self) {
        if let Some(ref mut ss) = self.screensaver {
            if let (Some(anim), Some(renderer)) =
                (&mut ss.animation, &mut ss.renderer)
            {
                anim.tick();
                let (x, y) = anim.position();
                let alpha = anim.alpha();
                renderer.render_frame(x, y, alpha);
            }
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
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.draw();
        surface.frame(qh, surface.clone());
        surface.commit();
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
        if let Some(info) = self.output_state.info(&output) {
            if let Some(mode) = info.modes.iter().find(|m| m.current) {
                self.width = mode.dimensions.0 as u32;
                self.height = mode.dimensions.1 as u32;
                log::info!("Output: {}x{}", self.width, self.height);
            }
        }
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Some(info) = self.output_state.info(&output) {
            if let Some(mode) = info.modes.iter().find(|m| m.current) {
                self.width = mode.dimensions.0 as u32;
                self.height = mode.dimensions.1 as u32;
            }
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

    fn new_seat(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
    ) {
        if self.seat.is_none() {
            self.seat = Some(seat);
        }
    }

    fn new_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        _capability: Capability,
    ) {
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        _capability: Capability,
    ) {
    }

    fn remove_seat(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
    ) {
    }
}

impl WindowHandler for App {
    fn request_close(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &Window,
    ) {
        self.deactivate();
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

            if ss.renderer.is_none() {
                log::info!("Window configured: {w}x{h}, initializing renderer");
                let renderer = Renderer::new(
                    &self.conn,
                    window.wl_surface(),
                    w,
                    h,
                    self.font_size,
                );
                let animation = Animation::new(
                    w as f32,
                    h as f32,
                    renderer.text_block_width(),
                    renderer.text_block_height(),
                    self.hold,
                );
                ss.renderer = Some(renderer);
                ss.animation = Some(animation);

                self.draw();
                let wl_surface = window.wl_surface();
                wl_surface.frame(qh, wl_surface.clone());
                wl_surface.commit();
            } else if let Some(ref mut renderer) = ss.renderer {
                renderer.resize(w, h);
                if let Some(ref mut anim) = ss.animation {
                    anim.update_screen_size(
                        w as f32,
                        h as f32,
                        renderer.text_block_width(),
                        renderer.text_block_height(),
                    );
                }
            }
        }
    }
}

// Idle notification dispatch
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

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers!(OutputState, SeatState);
}

delegate_compositor!(App);
delegate_output!(App);
delegate_registry!(App);
delegate_seat!(App);
delegate_xdg_shell!(App);
delegate_xdg_window!(App);

fn main() {
    env_logger::init();
    let args = Args::parse();

    let conn = Connection::connect_to_env().expect("Failed to connect to Wayland");
    let (globals, event_queue) = registry_queue_init(&conn).expect("Failed to init registry");
    let qh = event_queue.handle();

    let compositor_state =
        CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg_wm_base not available");

    // Bind idle notifier (optional — not all compositors support it)
    let idle_notifier: Option<ExtIdleNotifierV1> = globals.bind(&qh, 1..=1, ()).ok();
    if idle_notifier.is_none() {
        log::warn!("ext_idle_notifier_v1 not available — idle detection disabled");
    }

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
        seat: None,
        font_size: args.font_size,
        hold: args.hold,
        width: 0,
        height: 0,
        running: true,
    };

    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().expect("Failed to create event loop");

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .expect("Failed to insert Wayland source");

    // Do initial roundtrip to get seat
    event_loop
        .dispatch(std::time::Duration::from_millis(100), &mut app)
        .expect("Initial dispatch failed");

    if args.activate {
        // Immediate activation mode
        app.activate(&qh);
    } else if let (Some(notifier), Some(seat)) = (&app.idle_notifier, &app.seat) {
        // Set up idle notification
        let timeout_ms = args.timeout * 60 * 1000;
        log::info!("Setting up idle notification: {}ms", timeout_ms);
        let notification = notifier.get_idle_notification(timeout_ms, seat, &qh, ());
        app.idle_notification = Some(notification);
    }

    while app.running {
        event_loop
            .dispatch(std::time::Duration::from_millis(16), &mut app)
            .expect("Event loop dispatch failed");
    }
}
