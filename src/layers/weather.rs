use std::sync::{Arc, RwLock};

use glyphon::{
    Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, TextArea, TextBounds,
    cosmic_text::Align,
};

use crate::data::DataCache;
use crate::layer::{GpuContext, Layer};

fn weather_symbol_text(code: u8) -> &'static str {
    match code {
        1 => "Clear",
        2 => "Partly cloudy",
        3 => "Cloudy",
        21 => "Light rain",
        22 => "Rain",
        23 => "Heavy rain",
        31 => "Light snow",
        32 => "Snow",
        33 => "Heavy snow",
        41 => "Light sleet",
        42 => "Sleet",
        43 => "Heavy sleet",
        51 => "Light rain",
        52 => "Rain",
        53 => "Heavy rain",
        61 => "Thunder",
        62 => "Heavy thunder",
        63 => "Thunder",
        64 => "Thunder + snow",
        _ => "",
    }
}

/// Weather layer that follows clock position — renders below the clock text block.
pub struct WeatherLayer {
    buffer: Option<Buffer>,
    data_cache: Option<Arc<RwLock<DataCache>>>,
    font_size: f32,
    text_color: [u8; 3],
    // Cached from clock position each frame
    x: f32,
    y: f32,
    current_alpha: u8,
    screen_w: u32,
    screen_h: u32,
}

impl Default for WeatherLayer {
    fn default() -> Self {
        Self {
            buffer: None,
            data_cache: None,
            font_size: 24.0,
            text_color: [150, 150, 150],
            x: 0.0,
            y: 0.0,
            current_alpha: 0,
            screen_w: 0,
            screen_h: 0,
        }
    }
}

impl Layer for WeatherLayer {
    fn name(&self) -> &'static str {
        "weather"
    }

    fn init(
        &mut self,
        ctx: &GpuContext<'_>,
        font_system: &mut FontSystem,
        config: &toml::value::Table,
        data_cache: Arc<RwLock<DataCache>>,
    ) {
        if let Some(v) = config.get("font_size").and_then(|v| v.as_integer()) {
            self.font_size = v as f32;
        }
        if let Some(arr) = config.get("color").and_then(|v| v.as_array())
            && arr.len() == 3
            && let (Some(r), Some(g), Some(b)) = (
                arr[0].as_integer(),
                arr[1].as_integer(),
                arr[2].as_integer(),
            )
        {
            self.text_color = [r as u8, g as u8, b as u8];
        }

        self.screen_w = ctx.width;
        self.screen_h = ctx.height;
        self.data_cache = Some(data_cache);

        let fs = self.font_size;
        let mut buffer = Buffer::new(font_system, Metrics::new(fs, fs * 1.4));
        buffer.set_size(font_system, Some(fs * 12.0), Some(fs * 2.0));
        buffer.set_text(
            font_system,
            "",
            &Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
            None,
        );
        for line in buffer.lines.iter_mut() {
            line.set_align(Some(Align::Center));
        }
        buffer.shape_until_scroll(font_system, false);
        self.buffer = Some(buffer);
    }

    fn update(&mut self, _dt_secs: f32, _screen_w: f32, _screen_h: f32) {
        // Follow clock position
        if let Some(ref cache) = self.data_cache
            && let Ok(c) = cache.read()
            && let Some(ref pos) = c.clock_pos
        {
            // Center weather text below the clock block, offset by a small gap
            let gap = self.font_size * 0.5;
            self.x = pos.x + (pos.block_width - self.font_size * 12.0) / 2.0;
            self.y = pos.y + pos.block_height + gap;
            self.current_alpha = pos.alpha;
        }
    }

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

        let text = if let Ok(cache) = cache.read()
            && let Some(ref weather) = cache.weather
        {
            let symbol = weather_symbol_text(weather.weather_symbol);
            let wind = format!("{:.0} m/s", weather.wind_speed);
            if symbol.is_empty() {
                format!("{:.0}°C  {}", weather.temperature, wind)
            } else {
                format!("{:.0}°C  {}  {}", weather.temperature, symbol, wind)
            }
        } else {
            String::new()
        };

        buffer.set_text(
            font_system,
            &text,
            &Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
            None,
        );
        for line in buffer.lines.iter_mut() {
            line.set_align(Some(Align::Center));
        }
        buffer.shape_until_scroll(font_system, false);
    }

    fn text_areas(&self) -> Vec<TextArea<'_>> {
        if self.current_alpha == 0 {
            return vec![];
        }
        let Some(ref buf) = self.buffer else {
            return vec![];
        };
        let [r, g, b] = self.text_color;
        let color = Color::rgba(r, g, b, self.current_alpha);

        vec![TextArea {
            buffer: buf,
            left: self.x,
            top: self.y,
            scale: 1.0,
            bounds: TextBounds {
                left: 0,
                top: 0,
                right: self.screen_w as i32,
                bottom: self.screen_h as i32,
            },
            default_color: color,
            custom_glyphs: &[],
        }]
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.screen_w = width;
        self.screen_h = height;
    }
}
