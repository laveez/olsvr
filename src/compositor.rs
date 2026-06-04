use std::sync::{Arc, RwLock};
use std::time::Instant;

use glyphon::{Cache, FontSystem, Resolution, SwashCache, TextAtlas, TextRenderer, Viewport};

use crate::data::DataCache;
use crate::layer::{GpuContext, Layer};

// Wayland surface creation lives in `new_wayland` and is Linux-only.
#[cfg(target_os = "linux")]
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
#[cfg(target_os = "linux")]
use std::ptr::NonNull;
#[cfg(target_os = "linux")]
use wayland_client::{Connection, Proxy, protocol::wl_surface::WlSurface};

pub struct DebugInfo {
    pub frame_count: u64,
    pub frame_ms: u64,
    pub from_callback: bool,
    pub uptime_secs: u64,
}

pub struct Compositor {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    font_system: FontSystem,
    swash_cache: SwashCache,
    text_atlas: TextAtlas,
    text_renderer: TextRenderer,
    #[allow(dead_code)]
    cache: Cache,
    viewport: Viewport,
    layers: Vec<Box<dyn Layer>>,
    last_frame: Instant,
    data_cache: Arc<RwLock<DataCache>>,
    max_texture_dim: u32,
}

/// Scale `(w, h)` down proportionally so neither exceeds `max` (the GPU's max
/// texture dimension). Returns at least 1x1; aspect ratio is preserved.
fn clamp_to_max(w: u32, h: u32, max: u32) -> (u32, u32) {
    let longest = w.max(h);
    if longest <= max {
        return (w.max(1), h.max(1));
    }
    let scale = max as f64 / longest as f64;
    (
        ((w as f64 * scale) as u32).clamp(1, max),
        ((h as f64 * scale) as u32).clamp(1, max),
    )
}

impl Compositor {
    /// Build the renderer from a platform-created surface and the `wgpu::Instance`
    /// it was made from. The backend owns surface creation (Wayland raw handle on
    /// Linux via `new_wayland`, winit window on macOS).
    pub fn new(
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
        width: u32,
        height: u32,
        mut layers: Vec<Box<dyn Layer>>,
        layer_configs: &[toml::value::Table],
        data_cache: Arc<RwLock<DataCache>>,
    ) -> Self {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .expect("No suitable GPU adapter found");

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("olsvr"),
            // Use the adapter's real limits so large HiDPI surfaces (e.g. a 2x
            // 5120x1440 panel = 10240x2880) fit; the default caps texture size at 8192.
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .expect("Failed to create device");

        // Some displays report a physical (HiDPI) size larger than the GPU's max
        // texture dimension; clamp the surface so configure() can't fail. The
        // presented image is scaled to fill the window.
        let max_texture_dim = adapter.limits().max_texture_dimension_2d;
        let (width, height) = clamp_to_max(width, height, max_texture_dim);

        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats[0];

        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            log::info!("Using Mailbox present mode (non-blocking)");
            wgpu::PresentMode::Mailbox
        } else {
            log::info!("Mailbox not available, falling back to Fifo");
            wgpu::PresentMode::Fifo
        };

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut text_atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer = TextRenderer::new(
            &mut text_atlas,
            &device,
            wgpu::MultisampleState::default(),
            None,
        );

        for (i, layer) in layers.iter_mut().enumerate() {
            let cfg = layer_configs.get(i).cloned().unwrap_or_default();
            let ctx = GpuContext {
                device: &device,
                queue: &queue,
                surface_format: format,
                width,
                height,
            };
            layer.init(&ctx, &mut font_system, &cfg, data_cache.clone());
        }

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
            layers,
            last_frame: Instant::now(),
            data_cache,
            max_texture_dim,
        }
    }

    /// Linux constructor: create the wgpu instance and a surface from the Wayland
    /// display + surface, then build the portable renderer.
    #[cfg(target_os = "linux")]
    pub fn new_wayland(
        conn: &Connection,
        wl_surface: &WlSurface,
        width: u32,
        height: u32,
        layers: Vec<Box<dyn Layer>>,
        layer_configs: &[toml::value::Table],
        data_cache: Arc<RwLock<DataCache>>,
    ) -> Self {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        let display_ptr = conn.backend().display_ptr();
        let surface_ptr = wl_surface.id().as_ptr();

        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
                    NonNull::new_unchecked(display_ptr as *mut _),
                ))),
                raw_window_handle: RawWindowHandle::Wayland(WaylandWindowHandle::new(
                    NonNull::new_unchecked(surface_ptr as *mut _),
                )),
            })
        }
        .expect("Failed to create wgpu surface");

        Self::new(
            &instance,
            surface,
            width,
            height,
            layers,
            layer_configs,
            data_cache,
        )
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let (width, height) = clamp_to_max(width, height, self.max_texture_dim);
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        for layer in &mut self.layers {
            layer.resize(width, height);
        }
    }

    pub fn render_frame(&mut self, debug_info: Option<&DebugInfo>) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;

        let w = self.config.width;
        let h = self.config.height;

        // 1. Update all layers
        for layer in &mut self.layers {
            layer.update(dt, w as f32, h as f32);
        }

        // Pass debug info to debug layer via data cache
        if let Some(dbg) = debug_info
            && let Ok(mut cache) = self.data_cache.write()
        {
            cache.debug = Some(crate::data::DebugData {
                frame_count: dbg.frame_count,
                frame_ms: dbg.frame_ms,
                from_callback: dbg.from_callback,
                uptime_secs: dbg.uptime_secs,
            });
        }

        // 2. Prepare text for all layers
        for layer in &mut self.layers {
            layer.prepare_text(&mut self.font_system);
        }

        // 3. Collect all text areas
        let text_areas: Vec<_> = self.layers.iter().flat_map(|l| l.text_areas()).collect();

        // 4. Prepare text renderer
        self.viewport.update(
            &self.queue,
            Resolution {
                width: w,
                height: h,
            },
        );

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

        // 5. Get surface texture
        let t0 = Instant::now();
        let output = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated
            | wgpu::CurrentSurfaceTexture::Suboptimal(_)
            | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(t) => t,
                    other => {
                        log::warn!("Failed to get surface texture after reconfigure: {other:?}");
                        return;
                    }
                }
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                log::warn!("Surface texture timeout or occluded, skipping frame");
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                log::warn!("Surface texture validation error, skipping frame");
                return;
            }
        };
        let texture_ms = t0.elapsed().as_millis();

        // 6. Render pass
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

            // 7. Custom GPU commands from layers
            for layer in &self.layers {
                layer.render_custom(&mut pass);
            }

            // 8. Text on top
            self.text_renderer
                .render(&self.text_atlas, &self.viewport, &mut pass)
                .expect("Failed to render text");
        }

        let t1 = Instant::now();
        self.queue.submit(Some(encoder.finish()));
        let submit_ms = t1.elapsed().as_millis();

        let t2 = Instant::now();
        output.present();
        let present_ms = t2.elapsed().as_millis();

        if texture_ms > 50 || submit_ms > 50 || present_ms > 50 {
            log::warn!(
                "Render: texture={texture_ms}ms submit={submit_ms}ms present={present_ms}ms"
            );
        }

        self.text_atlas.trim();
    }
}
