use clap::Parser;
use smithay_client_toolkit::{
    reexports::calloop::EventLoop,
    reexports::calloop_wayland_source::WaylandSource,
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_output, delegate_registry, delegate_seat,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{Capability, SeatHandler, SeatState},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_seat, wl_surface},
    Connection, QueueHandle,
};

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
    registry_state: RegistryState,
    compositor_state: CompositorState,
    output_state: OutputState,
    seat_state: SeatState,
    width: u32,
    height: u32,
    running: bool,
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

fn main() {
    env_logger::init();
    let args = Args::parse();
    log::info!(
        "olsvr: timeout={}m, font_size={}px, hold={}s",
        args.timeout,
        args.font_size,
        args.hold
    );

    let conn = Connection::connect_to_env().expect("Failed to connect to Wayland");
    let (globals, event_queue) = registry_queue_init(&conn).expect("Failed to init registry");
    let qh = event_queue.handle();

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        compositor_state: CompositorState::bind(&globals, &qh)
            .expect("wl_compositor not available"),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        width: 0,
        height: 0,
        running: true,
    };

    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().expect("Failed to create event loop");

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .expect("Failed to insert Wayland source");

    // Do one roundtrip to get output info, then exit for now
    event_loop
        .dispatch(std::time::Duration::from_millis(100), &mut app)
        .expect("Event loop dispatch failed");

    if app.width > 0 {
        println!("Detected output: {}x{}", app.width, app.height);
    } else {
        println!("No output resolution detected yet (will be available after configure)");
    }
}
