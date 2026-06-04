//! macOS backend: a winit fullscreen window per display, each rendered by the
//! portable `Compositor`. Idle detection (CGEventSource), the display-sleep
//! assertion (IOPMAssertion), and full Engine wiring are layered on next; this
//! stage shows the saver on all displays with weather.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Fullscreen, Window, WindowId, WindowLevel};

use crate::RunArgs;
use crate::compositor::Compositor;
use crate::config::Config;
use crate::data::{self, DataCache};

pub fn run(args: RunArgs) {
    let mut config = Config::load();
    crate::merge_cli(&mut config, &args);

    let event_loop = EventLoop::new().expect("failed to create winit event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = MacApp {
        config,
        data_cache: Arc::new(RwLock::new(DataCache::default())),
        targets: Vec::new(),
        debug: args.debug,
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
    data_cache: Arc<RwLock<DataCache>>,
    targets: Vec<DisplayTarget>,
    debug: bool,
}

impl MacApp {
    fn show(&mut self, event_loop: &ActiveEventLoop) {
        if !self.targets.is_empty() {
            return;
        }

        // Start the weather fetch thread if a weather layer is configured.
        if let Some((location, interval_secs)) = self.config.weather_config() {
            data::start_fetch_thread(
                self.data_cache.clone(),
                location,
                Duration::from_secs(interval_secs),
            );
        }

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        let monitors: Vec<_> = event_loop.available_monitors().collect();
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
            window.set_cursor_visible(false);

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
    }

    fn target_mut(&mut self, id: WindowId) -> Option<&mut DisplayTarget> {
        self.targets.iter_mut().find(|t| t.window.id() == id)
    }
}

impl ApplicationHandler for MacApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.show(event_loop);
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
                // Skeleton: any input exits. The Engine + idle loop replace this next.
                event_loop.exit();
            }
            _ => {}
        }
    }
}
