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
    Connection, QueueHandle,
};

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

struct App {
    conn: Connection,
    registry_state: RegistryState,
    compositor_state: CompositorState,
    output_state: OutputState,
    seat_state: SeatState,
    xdg_shell: XdgShell,
    window: Window,
    renderer: Option<Renderer>,
    font_size: u32,
    width: u32,
    height: u32,
    configured: bool,
    running: bool,
}

impl App {
    fn draw(&mut self) {
        if let Some(ref mut renderer) = self.renderer {
            // Center the clock for now (animation will change this later)
            let x = (renderer.width() as f32 - renderer.text_block_width()) / 2.0;
            let y = (renderer.height() as f32 - renderer.text_block_height()) / 2.0;
            renderer.render_frame(x, y, 255);
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
                log::info!("Output updated: {}x{}", self.width, self.height);
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
        _seat: wl_seat::WlSeat,
    ) {
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
        self.running = false;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _window: &Window,
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
        log::info!("Window configured: {w}x{h}");
        self.width = w;
        self.height = h;

        if self.renderer.is_none() && w > 0 && h > 0 {
            self.renderer = Some(Renderer::new(
                &self.conn,
                self.window.wl_surface(),
                w,
                h,
                self.font_size,
            ));
            self.configured = true;

            self.draw();
            let wl_surface = self.window.wl_surface();
            wl_surface.frame(qh, wl_surface.clone());
            wl_surface.commit();
        } else if let Some(ref mut renderer) = self.renderer {
            renderer.resize(w, h);
        }
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

    let surface = compositor_state.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::None, &qh);
    window.set_fullscreen(None);
    window.set_title("olsvr");
    window.set_app_id("olsvr");
    window.commit();

    let mut app = App {
        conn: conn.clone(),
        registry_state: RegistryState::new(&globals),
        compositor_state,
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        xdg_shell,
        window,
        renderer: None,
        font_size: args.font_size,
        width: 0,
        height: 0,
        configured: false,
        running: true,
    };

    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().expect("Failed to create event loop");

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .expect("Failed to insert Wayland source");

    while app.running {
        event_loop
            .dispatch(std::time::Duration::from_millis(16), &mut app)
            .expect("Event loop dispatch failed");
    }
}
