use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

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

/// Longest animation step one frame advances, so the first frame after a long
/// occlusion does not jump.
const MAX_FRAME_DT_SECS: f32 = 0.25;
/// Validation errors in a row, with no frame presented between them, after which
/// the surface counts as broken.
const MAX_VALIDATION_ERRORS: u32 = 3;
const WARN_INTERVAL: Duration = Duration::from_secs(10);

pub struct DebugInfo {
    pub frame_count: u64,
    pub frame_ms: u64,
    pub from_callback: bool,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    Presented,
    /// Nothing was drawn (timeout, occlusion, a transient validation error). Not a
    /// failure: try again later.
    Skipped,
    /// The surface or device can no longer render. Backends answer with `ShowFailed`.
    Broken,
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
    skips: u32,
    last_skip_log: Instant,
    validation_errors: u32,
    last_validation_warn: Option<Instant>,
    last_prepare_warn: Option<Instant>,
    last_render_warn: Option<Instant>,
    last_text_render_warn: Option<Instant>,
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

/// Rate limit for a warning that can repeat every frame: true at most once per
/// `WARN_INTERVAL`, and then records the time in `last`.
pub(crate) fn warning_due(last: &mut Option<Instant>) -> bool {
    if last.is_some_and(|t| t.elapsed() < WARN_INTERVAL) {
        return false;
    }
    *last = Some(Instant::now());
    true
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
    ) -> Option<Self> {
        // Fallible so a backend can recover (the macOS saver skips the display)
        // instead of aborting the whole process on a GPU init failure.
        let adapter =
            match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })) {
                Ok(a) => a,
                Err(e) => {
                    log::error!("No suitable GPU adapter found: {e}");
                    return None;
                }
            };

        let device_queue = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("olsvr"),
            // Use the adapter's real limits so large HiDPI surfaces (e.g. a 2x
            // 5120x1440 panel = 10240x2880) fit; the default caps texture size at 8192.
            required_limits: adapter.limits(),
            ..Default::default()
        }));
        let (device, queue) = match device_queue {
            Ok(dq) => dq,
            Err(e) => {
                log::error!("Failed to create GPU device: {e}");
                return None;
            }
        };

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

        Some(Self {
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
            skips: 0,
            last_skip_log: Instant::now(),
            validation_errors: 0,
            last_validation_warn: None,
            last_prepare_warn: None,
            last_render_warn: None,
            last_text_render_warn: None,
        })
    }

    /// Linux constructor: create a surface on the backend's process-wide `instance`
    /// from the Wayland display + surface, then build the portable renderer. `None`
    /// when the surface, adapter or device cannot be created.
    #[cfg(target_os = "linux")]
    #[allow(clippy::too_many_arguments)]
    pub fn new_wayland(
        instance: &wgpu::Instance,
        conn: &Connection,
        wl_surface: &WlSurface,
        width: u32,
        height: u32,
        layers: Vec<Box<dyn Layer>>,
        layer_configs: &[toml::value::Table],
        data_cache: Arc<RwLock<DataCache>>,
    ) -> Option<Self> {
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
        };
        let surface = match surface {
            Ok(s) => s,
            Err(e) => {
                log::error!("Failed to create wgpu surface: {e}");
                return None;
            }
        };

        Self::new(
            instance,
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
        if (width, height) == (self.config.width, self.config.height) {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        for layer in &mut self.layers {
            layer.resize(width, height);
        }
    }

    /// Render one frame; skipped frames (timeout/occluded) are counted and logged at debug.
    #[must_use]
    pub fn render_frame(&mut self, debug_info: Option<&DebugInfo>) -> FrameOutcome {
        // Summarize skipped frames at most once per second (a brief burst at show time is normal).
        if self.skips > 0 && self.last_skip_log.elapsed().as_secs() >= 1 {
            log::debug!(
                "surface {}x{}: {} frame(s) skipped (timeout/occluded) in the last ~1s",
                self.config.width,
                self.config.height,
                self.skips
            );
            self.skips = 0;
        }
        let now = Instant::now();
        let dt = now
            .duration_since(self.last_frame)
            .as_secs_f32()
            .min(MAX_FRAME_DT_SECS);
        self.last_frame = now;
        self.update_layers(dt, debug_info);

        // Nothing may be written to the queue before the acquire: wgpu holds each write's
        // staging buffer until the next submit, and a skipped frame never submits.
        let t0 = Instant::now();
        let (output, suboptimal) = match self.acquire() {
            Ok(acquired) => acquired,
            Err(outcome) => return outcome,
        };
        let texture_ms = t0.elapsed().as_millis();

        let text_ready = self.upload_text();
        let commands = self.encode_frame(&output.texture, text_ready);

        let t1 = Instant::now();
        self.queue.submit(Some(commands));
        let submit_ms = t1.elapsed().as_millis();

        let t2 = Instant::now();
        output.present();
        let present_ms = t2.elapsed().as_millis();

        // Configuring while a texture acquired from this surface is alive panics in
        // wgpu, so a suboptimal surface is reconfigured only after its frame is presented.
        if suboptimal {
            self.surface.configure(&self.device, &self.config);
        }

        if (texture_ms > 50 || submit_ms > 50 || present_ms > 50)
            && warning_due(&mut self.last_render_warn)
        {
            log::warn!(
                "Render: texture={texture_ms}ms submit={submit_ms}ms present={present_ms}ms"
            );
        }

        self.validation_errors = 0;
        self.text_atlas.trim();
        FrameOutcome::Presented
    }

    /// Advance the layers and shape their text. CPU only: `render_frame` relies on
    /// nothing here reaching the GPU queue.
    fn update_layers(&mut self, dt: f32, debug_info: Option<&DebugInfo>) {
        let (w, h) = (self.config.width as f32, self.config.height as f32);
        for layer in &mut self.layers {
            layer.update(dt, w, h);
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

        for layer in &mut self.layers {
            layer.prepare_text(&mut self.font_system);
        }
    }

    /// The frame's texture, and whether the surface wants a reconfigure once the
    /// texture is presented. `Err` carries the outcome of a frame that cannot be drawn.
    fn acquire(&mut self) -> Result<(wgpu::SurfaceTexture, bool), FrameOutcome> {
        let mut acquired = self.surface.get_current_texture();
        if matches!(acquired, wgpu::CurrentSurfaceTexture::Outdated) {
            self.surface.configure(&self.device, &self.config);
            acquired = self.surface.get_current_texture();
        }
        match acquired {
            wgpu::CurrentSurfaceTexture::Success(texture) => Ok((texture, false)),
            wgpu::CurrentSurfaceTexture::Suboptimal(texture) => Ok((texture, true)),
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                if self.skips == 0 {
                    self.last_skip_log = Instant::now();
                }
                self.skips += 1;
                Err(FrameOutcome::Skipped)
            }
            wgpu::CurrentSurfaceTexture::Validation => Err(self.validation_error()),
            wgpu::CurrentSurfaceTexture::Lost => {
                log::error!("Surface lost");
                Err(FrameOutcome::Broken)
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                log::error!("Surface still outdated after a reconfigure");
                Err(FrameOutcome::Broken)
            }
        }
    }

    fn validation_error(&mut self) -> FrameOutcome {
        self.validation_errors += 1;
        if self.validation_errors >= MAX_VALIDATION_ERRORS {
            log::error!(
                "Surface texture validation failed {} times in a row",
                self.validation_errors
            );
            return FrameOutcome::Broken;
        }
        if warning_due(&mut self.last_validation_warn) {
            log::warn!(
                "Surface texture validation error ({} in a row), skipping frame",
                self.validation_errors
            );
        }
        FrameOutcome::Skipped
    }

    /// Upload the viewport and the layers' glyphs. False when the text cannot be drawn
    /// this frame.
    fn upload_text(&mut self) -> bool {
        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );
        let text_areas: Vec<_> = self.layers.iter().flat_map(|l| l.text_areas()).collect();
        match self.text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.text_atlas,
            &self.viewport,
            text_areas,
            &mut self.swash_cache,
        ) {
            Ok(()) => true,
            Err(e) => {
                if warning_due(&mut self.last_prepare_warn) {
                    log::warn!("Text prepare failed, presenting a black frame: {e}");
                }
                false
            }
        }
    }

    /// A black clear, with the layers drawn over it when `draw_layers` is set.
    fn encode_frame(&mut self, target: &wgpu::Texture, draw_layers: bool) -> wgpu::CommandBuffer {
        let view = target.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
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
        if draw_layers {
            for layer in &self.layers {
                layer.render_custom(&mut pass);
            }
            if let Err(e) = self
                .text_renderer
                .render(&self.text_atlas, &self.viewport, &mut pass)
                && warning_due(&mut self.last_text_render_warn)
            {
                log::warn!("Text render failed, presenting without it: {e}");
            }
        }
        drop(pass);
        encoder.finish()
    }
}
