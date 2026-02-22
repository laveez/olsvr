use std::sync::{Arc, RwLock};

use glyphon::{
    Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, TextArea, TextBounds,
    cosmic_text::Align,
};

use crate::data::DataCache;
use crate::layer::{GpuContext, Layer};

/// Returns (nerd font icon char, RGB color) for an FMI WeatherSymbol3 code.
fn weather_icon(code: u8) -> (char, [u8; 3]) {
    match code {
        1 => ('\u{E30D}', [0xFF, 0xD7, 0x00]),  // day_sunny — yellow
        2 => ('\u{E302}', [0x87, 0xCE, 0xEB]),  // day_cloudy — light blue
        3 => ('\u{E312}', [0xA0, 0xA0, 0xA0]),  // cloudy — gray
        21 => ('\u{E319}', [0x6C, 0xB4, 0xEE]), // showers — light blue
        22 => ('\u{E318}', [0x4A, 0x90, 0xD9]), // rain — blue
        23 => ('\u{E318}', [0x2E, 0x5C, 0xA8]), // heavy rain — deep blue
        31 => ('\u{E31A}', [0xE0, 0xE0, 0xE0]), // light snow — white
        32 => ('\u{E31A}', [0xFF, 0xFF, 0xFF]), // snow — white
        33 => ('\u{E35E}', [0xFF, 0xFF, 0xFF]), // snow_wind — bright white
        41 => ('\u{E3AD}', [0x5F, 0x9E, 0xA0]), // light sleet — teal
        42 => ('\u{E3AD}', [0x5F, 0x9E, 0xA0]), // sleet — teal
        43 => ('\u{E3AD}', [0x4A, 0x8A, 0x8C]), // heavy sleet — dark teal
        51..=53 => ('\u{E309}', [0x4A, 0x90, 0xD9]), // showers — blue
        61..=64 => ('\u{E31D}', [0x93, 0x70, 0xDB]), // thunderstorm — purple
        _ => ('\u{E33D}', [0x80, 0x80, 0x80]),  // cloud — gray
    }
}

/// Weather layer that follows clock position — renders below the clock text block.
pub struct WeatherLayer {
    buffer: Option<Buffer>,
    data_cache: Option<Arc<RwLock<DataCache>>>,
    font_size: f32,
    icon_font: String,
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
            icon_font: "MesloLGS NF".into(),
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
        if let Some(v) = config.get("icon_font").and_then(|v| v.as_str()) {
            self.icon_font = v.to_string();
        }

        self.screen_w = ctx.width;
        self.screen_h = ctx.height;
        self.data_cache = Some(data_cache);

        let fs = self.font_size;
        let mut buffer = Buffer::new(font_system, Metrics::new(fs, fs * 1.4));
        buffer.set_size(font_system, Some(fs * 14.0), Some(fs * 2.0));
        buffer.set_rich_text(
            font_system,
            [("", Attrs::new().family(Family::SansSerif))],
            &Attrs::new(),
            Shaping::Advanced,
            Some(Align::Center),
        );
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
            self.x = pos.x + (pos.block_width - self.font_size * 14.0) / 2.0;
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

        let a = self.current_alpha;
        let (icon_str, temp_str, wind_str);

        let spans: Vec<(&str, Attrs<'_>)> = if let Ok(cache) = cache.read()
            && let Some(ref weather) = cache.weather
        {
            let (icon_char, [ir, ig, ib]) = weather_icon(weather.weather_symbol);
            icon_str = icon_char.to_string();
            temp_str = format!("  {:.0}°C  ", weather.temperature);
            wind_str = format!("{:.0} m/s", weather.wind_speed);

            vec![
                (
                    &icon_str,
                    Attrs::new()
                        .family(Family::Name(&self.icon_font))
                        .color(Color::rgba(ir, ig, ib, a)),
                ),
                (
                    &temp_str,
                    Attrs::new()
                        .family(Family::SansSerif)
                        .color(Color::rgba(0xE0, 0xE0, 0xE0, a)),
                ),
                (
                    &wind_str,
                    Attrs::new()
                        .family(Family::SansSerif)
                        .color(Color::rgba(0xA0, 0xA0, 0xA0, a)),
                ),
            ]
        } else {
            vec![]
        };

        buffer.set_rich_text(
            font_system,
            spans,
            &Attrs::new(),
            Shaping::Advanced,
            Some(Align::Center),
        );
        buffer.shape_until_scroll(font_system, false);
    }

    fn text_areas(&self) -> Vec<TextArea<'_>> {
        if self.current_alpha == 0 {
            return vec![];
        }
        let Some(ref buf) = self.buffer else {
            return vec![];
        };
        let color = Color::rgba(0xA0, 0xA0, 0xA0, self.current_alpha);

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
