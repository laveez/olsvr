use std::ptr::NonNull;

use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Proxy};

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    // Text rendering
    font_system: FontSystem,
    swash_cache: SwashCache,
    text_atlas: TextAtlas,
    text_renderer: TextRenderer,
    cache: Cache,
    viewport: Viewport,
    time_buffer: Buffer,
    date_buffer: Buffer,
    font_size: f32,
}

impl Renderer {
    pub fn new(
        conn: &Connection,
        wl_surface: &WlSurface,
        width: u32,
        height: u32,
        font_size: u32,
    ) -> Self {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..Default::default()
        });

        let display_ptr = conn.backend().display_ptr();
        let surface_ptr = wl_surface.id().as_ptr();

        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
                    NonNull::new_unchecked(display_ptr as *mut _),
                )),
                raw_window_handle: RawWindowHandle::Wayland(WaylandWindowHandle::new(
                    NonNull::new_unchecked(surface_ptr as *mut _),
                )),
            })
        }
        .expect("Failed to create wgpu surface");

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .expect("No suitable GPU adapter found");

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("olsvr"),
                ..Default::default()
            },
        ))
        .expect("Failed to create device");

        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats[0];

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        // Text rendering setup
        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let text_atlas = TextAtlas::new(&device, &queue, &cache, format);
        let mut text_atlas = text_atlas;
        let text_renderer =
            TextRenderer::new(&mut text_atlas, &device, wgpu::MultisampleState::default(), None);

        let fs = font_size as f32;

        // Time buffer (large): "23:47"
        let mut time_buffer = Buffer::new(&mut font_system, Metrics::new(fs, fs * 1.2));
        time_buffer.set_size(&mut font_system, Some(width as f32), Some(fs * 1.5));
        time_buffer.set_text(
            &mut font_system,
            "00:00",
            &Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
            None,
        );
        time_buffer.shape_until_scroll(&mut font_system, false);

        // Date buffer (smaller): "Fri 21 Feb"
        let date_fs = fs * 0.3;
        let mut date_buffer = Buffer::new(&mut font_system, Metrics::new(date_fs, date_fs * 1.2));
        date_buffer.set_size(&mut font_system, Some(width as f32), Some(date_fs * 1.5));
        date_buffer.set_text(
            &mut font_system,
            "Mon 01 Jan",
            &Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
            None,
        );
        date_buffer.shape_until_scroll(&mut font_system, false);

        Self {
            surface,
            device,
            queue,
            config,
            font_system,
            swash_cache,
            text_atlas,
            text_renderer,
            cache,
            viewport,
            time_buffer,
            date_buffer,
            font_size: fs,
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Update text content and render a frame.
    /// `x`, `y` are the top-left position of the time text.
    /// `alpha` is 0-255 for fade effect.
    pub fn render_frame(&mut self, x: f32, y: f32, alpha: u8) {
        let now = chrono::Local::now();
        let time_str = now.format("%H:%M").to_string();
        let date_str = now.format("%a %d %b").to_string();

        self.time_buffer.set_text(
            &mut self.font_system,
            &time_str,
            &Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
            None,
        );
        self.time_buffer
            .shape_until_scroll(&mut self.font_system, false);

        self.date_buffer.set_text(
            &mut self.font_system,
            &date_str,
            &Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
            None,
        );
        self.date_buffer
            .shape_until_scroll(&mut self.font_system, false);

        let w = self.config.width;
        let h = self.config.height;
        let color = Color::rgba(255, 255, 255, alpha);

        // Date sits below the time
        let date_y = y + self.font_size * 1.2;

        self.viewport.update(
            &self.queue,
            Resolution {
                width: w,
                height: h,
            },
        );

        let text_areas = [
            TextArea {
                buffer: &self.time_buffer,
                left: x,
                top: y,
                scale: 1.0,
                bounds: TextBounds {
                    left: 0,
                    top: 0,
                    right: w as i32,
                    bottom: h as i32,
                },
                default_color: color,
                custom_glyphs: &[],
            },
            TextArea {
                buffer: &self.date_buffer,
                left: x,
                top: date_y,
                scale: 1.0,
                bounds: TextBounds {
                    left: 0,
                    top: 0,
                    right: w as i32,
                    bottom: h as i32,
                },
                default_color: color,
                custom_glyphs: &[],
            },
        ];

        self.text_renderer
            .prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.text_atlas,
                &self.viewport,
                text_areas,
                &mut self.swash_cache,
            )
            .expect("Failed to prepare text");

        let output = match self.surface.get_current_texture() {
            Ok(t) => t,
            Err(e) => {
                log::warn!("Failed to get surface texture: {e}");
                return;
            }
        };
        let view = output.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("render"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                ..Default::default()
            });
            self.text_renderer
                .render(&self.text_atlas, &self.viewport, &mut pass)
                .expect("Failed to render text");
        }
        self.queue.submit(Some(encoder.finish()));
        output.present();

        self.text_atlas.trim();
    }

    /// Approximate width of the rendered time text block.
    pub fn text_block_width(&self) -> f32 {
        // "HH:MM" is roughly 3 characters wide at font_size
        self.font_size * 3.0
    }

    /// Approximate height of time + date text block.
    pub fn text_block_height(&self) -> f32 {
        // Time line + date line
        self.font_size * 1.2 + self.font_size * 0.3 * 1.2
    }

    pub fn width(&self) -> u32 {
        self.config.width
    }

    pub fn height(&self) -> u32 {
        self.config.height
    }
}
