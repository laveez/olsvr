use std::sync::{Arc, RwLock};

use glyphon::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, TextArea, TextBounds};

use crate::data::DataCache;
use crate::layer::{GpuContext, Layer};

pub struct DebugLayer {
    buffer: Option<Buffer>,
    data_cache: Option<Arc<RwLock<DataCache>>>,
    screen_w: u32,
    screen_h: u32,
    current_alpha: u8,
}

impl Default for DebugLayer {
    fn default() -> Self {
        Self {
            buffer: None,
            data_cache: None,
            screen_w: 0,
            screen_h: 0,
            current_alpha: 200,
        }
    }
}

impl Layer for DebugLayer {
    fn name(&self) -> &'static str {
        "debug"
    }

    fn init(
        &mut self,
        ctx: &GpuContext<'_>,
        font_system: &mut FontSystem,
        _config: &toml::value::Table,
        data_cache: Arc<RwLock<DataCache>>,
    ) {
        self.screen_w = ctx.width;
        self.screen_h = ctx.height;
        self.data_cache = Some(data_cache);

        let debug_fs = 14.0;
        let mut buffer = Buffer::new(font_system, Metrics::new(debug_fs, debug_fs * 1.4));
        buffer.set_size(font_system, Some(500.0), Some(debug_fs * 2.0));
        buffer.set_text(
            font_system,
            "",
            &Attrs::new().family(Family::Monospace),
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(font_system, false);
        self.buffer = Some(buffer);
    }

    fn update(&mut self, _dt_secs: f32, _screen_w: f32, _screen_h: f32) {}

    fn alpha(&self) -> u8 {
        self.current_alpha
    }

    fn prepare_text(&mut self, font_system: &mut FontSystem) {
        let Some(ref cache) = self.data_cache else {
            return;
        };
        let Some(ref mut buffer) = self.buffer else {
            return;
        };

        let debug_text = if let Ok(cache) = cache.read()
            && let Some(ref dbg) = cache.debug
        {
            let src = if dbg.from_callback { "cb" } else { "fb" };
            let mins = dbg.uptime_secs / 60;
            let secs = dbg.uptime_secs % 60;
            format!(
                "#{} {}ms {} {mins}:{secs:02}",
                dbg.frame_count, dbg.frame_ms, src
            )
        } else {
            String::new()
        };

        buffer.set_text(
            font_system,
            &debug_text,
            &Attrs::new().family(Family::Monospace),
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(font_system, false);
    }

    fn text_areas(&self) -> Vec<TextArea<'_>> {
        let Some(ref buf) = self.buffer else {
            return vec![];
        };
        vec![TextArea {
            buffer: buf,
            left: 10.0,
            top: self.screen_h as f32 - 30.0,
            scale: 1.0,
            bounds: TextBounds {
                left: 0,
                top: 0,
                right: self.screen_w as i32,
                bottom: self.screen_h as i32,
            },
            default_color: Color::rgba(100, 100, 100, self.current_alpha),
            custom_glyphs: &[],
        }]
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.screen_w = width;
        self.screen_h = height;
    }
}
