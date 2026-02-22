use std::sync::{Arc, RwLock};

use glyphon::{FontSystem, TextArea};

use crate::data::DataCache;

#[allow(dead_code)]
pub struct GpuContext<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub surface_format: wgpu::TextureFormat,
    pub width: u32,
    pub height: u32,
}

pub trait Layer {
    #[allow(dead_code)]
    fn name(&self) -> &'static str;

    fn init(
        &mut self,
        ctx: &GpuContext<'_>,
        font_system: &mut FontSystem,
        config: &toml::value::Table,
        data_cache: Arc<RwLock<DataCache>>,
    );

    fn update(&mut self, dt_secs: f32, screen_w: f32, screen_h: f32);

    #[allow(dead_code)]
    fn alpha(&self) -> u8;

    fn prepare_text(&mut self, _font_system: &mut FontSystem) {}

    fn text_areas(&self) -> Vec<TextArea<'_>> {
        vec![]
    }

    fn render_custom(&self, _pass: &mut wgpu::RenderPass<'_>) {}

    fn resize(&mut self, _width: u32, _height: u32) {}

    #[allow(dead_code)]
    fn destroy(&mut self) {}
}
