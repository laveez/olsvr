//! macOS backend (skeleton stage): a winit fullscreen window rendered by the
//! portable `Compositor`. Idle detection (CGEventSource), the display-sleep
//! assertion (IOPMAssertion), and Engine wiring are layered on after this proves
//! the renderer works on macOS.

use std::sync::{Arc, RwLock};

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Fullscreen, Window, WindowId, WindowLevel};

use crate::RunArgs;
use crate::compositor::Compositor;
use crate::config::Config;
use crate::data::DataCache;

pub fn run(args: RunArgs) {
    let mut config = Config::load();
    crate::merge_cli(&mut config, &args);

    let event_loop = EventLoop::new().expect("failed to create winit event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = MacApp {
        config,
        data_cache: Arc::new(RwLock::new(DataCache::default())),
        window: None,
        compositor: None,
        debug: args.debug,
    };
    event_loop
        .run_app(&mut app)
        .expect("winit event loop error");
}

struct MacApp {
    config: Config,
    data_cache: Arc<RwLock<DataCache>>,
    window: Option<Arc<Window>>,
    compositor: Option<Compositor>,
    debug: bool,
}

impl MacApp {
    fn show(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("olsvr")
            .with_fullscreen(Some(Fullscreen::Borderless(None)));
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create window"),
        );
        // Raise above other windows so the saver is visible and the surface is
        // not occluded (a proper NSScreenSaverWindowLevel via objc2 comes later).
        window.set_window_level(WindowLevel::AlwaysOnTop);
        let size = window.inner_size();

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let surface = instance
            .create_surface(window.clone())
            .expect("failed to create surface");

        let (layers, layer_configs) = self.config.build_layers(self.debug);
        let comp = Compositor::new(
            &instance,
            surface,
            size.width.max(1),
            size.height.max(1),
            layers,
            &layer_configs,
            self.data_cache.clone(),
        );

        self.compositor = Some(comp);
        self.window = Some(window.clone());
        window.request_redraw();
    }
}

impl ApplicationHandler for MacApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            self.show(event_loop);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(comp) = &mut self.compositor {
                    comp.resize(size.width.max(1), size.height.max(1));
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(comp) = &mut self.compositor {
                    comp.render_frame(None);
                }
                // Continuous animation: request the next frame.
                if let Some(window) = &self.window {
                    window.request_redraw();
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
