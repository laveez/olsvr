use std::sync::{Arc, RwLock};

use glyphon::{
    Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, TextArea, TextBounds,
    cosmic_text::Align,
};

use crate::animation::Animation;
use crate::data::DataCache;
use crate::layer::{GpuContext, Layer};

pub struct ClockLayer {
    time_buffer: Option<Buffer>,
    date_buffer: Option<Buffer>,
    animation: Option<Animation>,
    font_size: f32,
    font_family: String,
    time_format: String,
    date_format: String,
    text_color: [u8; 3],
    hold: u32,
    fade_duration: u32,
    edge_padding: u32,
    x: f32,
    y: f32,
    current_alpha: u8,
    screen_w: u32,
    screen_h: u32,
}

impl Default for ClockLayer {
    fn default() -> Self {
        Self {
            time_buffer: None,
            date_buffer: None,
            animation: None,
            font_size: 200.0,
            font_family: "sans-serif".into(),
            time_format: "24h".into(),
            date_format: "%a %d %b".into(),
            text_color: [255, 255, 255],
            hold: 10,
            fade_duration: 1500,
            edge_padding: 50,
            x: 0.0,
            y: 0.0,
            current_alpha: 0,
            screen_w: 0,
            screen_h: 0,
        }
    }
}

fn parse_family(name: &str) -> Family<'_> {
    match name {
        "sans-serif" => Family::SansSerif,
        "serif" => Family::Serif,
        "monospace" => Family::Monospace,
        "cursive" => Family::Cursive,
        "fantasy" => Family::Fantasy,
        name => Family::Name(name),
    }
}

impl ClockLayer {
    fn text_block_width(&self) -> f32 {
        self.font_size * 3.0
    }

    fn text_block_height(&self) -> f32 {
        self.font_size * 1.2 + self.font_size * 0.3 * 1.2
    }
}

impl Layer for ClockLayer {
    fn name(&self) -> &'static str {
        "clock"
    }

    fn init(
        &mut self,
        ctx: &GpuContext<'_>,
        font_system: &mut FontSystem,
        config: &toml::value::Table,
        _data_cache: Arc<RwLock<DataCache>>,
    ) {
        if let Some(v) = config.get("font_size").and_then(|v| v.as_integer()) {
            self.font_size = v as f32;
        }
        if let Some(v) = config.get("font_family").and_then(|v| v.as_str()) {
            self.font_family = v.to_string();
        }
        if let Some(v) = config.get("time_format").and_then(|v| v.as_str()) {
            self.time_format = v.to_string();
        }
        if let Some(v) = config.get("date_format").and_then(|v| v.as_str()) {
            self.date_format = v.to_string();
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
        if let Some(v) = config.get("hold").and_then(|v| v.as_integer()) {
            self.hold = v as u32;
        }
        if let Some(v) = config.get("fade_duration").and_then(|v| v.as_integer()) {
            self.fade_duration = v as u32;
        }
        if let Some(v) = config.get("edge_padding").and_then(|v| v.as_integer()) {
            self.edge_padding = v as u32;
        }

        self.screen_w = ctx.width;
        self.screen_h = ctx.height;

        let fs = self.font_size;
        let family = parse_family(&self.font_family);
        let block_w = fs * 3.0;

        let mut time_buffer = Buffer::new(font_system, Metrics::new(fs, fs * 1.2));
        time_buffer.set_size(font_system, Some(block_w), Some(fs * 1.5));
        time_buffer.set_text(
            font_system,
            "00:00",
            &Attrs::new().family(family),
            Shaping::Advanced,
            None,
        );
        for line in time_buffer.lines.iter_mut() {
            line.set_align(Some(Align::Center));
        }
        time_buffer.shape_until_scroll(font_system, false);

        let date_fs = fs * 0.3;
        let mut date_buffer = Buffer::new(font_system, Metrics::new(date_fs, date_fs * 1.2));
        date_buffer.set_size(font_system, Some(block_w), Some(date_fs * 1.5));
        date_buffer.set_text(
            font_system,
            "Mon 01 Jan",
            &Attrs::new().family(family),
            Shaping::Advanced,
            None,
        );
        for line in date_buffer.lines.iter_mut() {
            line.set_align(Some(Align::Center));
        }
        date_buffer.shape_until_scroll(font_system, false);

        self.time_buffer = Some(time_buffer);
        self.date_buffer = Some(date_buffer);

        let animation = Animation::new(
            ctx.width as f32,
            ctx.height as f32,
            self.text_block_width(),
            self.text_block_height(),
            self.hold,
            self.fade_duration,
            self.edge_padding,
        );
        self.animation = Some(animation);
    }

    fn update(&mut self, _dt_secs: f32, _screen_w: f32, _screen_h: f32) {
        if let Some(ref mut anim) = self.animation {
            anim.tick();
            let (x, y) = anim.position();
            self.x = x;
            self.y = y;
            self.current_alpha = anim.alpha();
            log::trace!(
                "clock: alpha={} pos=({:.0},{:.0}) phase={}",
                self.current_alpha,
                self.x,
                self.y,
                anim.phase_name()
            );
        }
    }

    fn alpha(&self) -> u8 {
        self.current_alpha
    }

    fn prepare_text(&mut self, font_system: &mut FontSystem) {
        let now = chrono::Local::now();
        let time_fmt = match self.time_format.as_str() {
            "12h" => "%I:%M %p",
            _ => "%H:%M",
        };
        let time_str = now.format(time_fmt).to_string();
        let date_str = now.format(&self.date_format).to_string();

        if let Some(ref mut buf) = self.time_buffer {
            let family = parse_family(&self.font_family);
            buf.set_text(
                font_system,
                &time_str,
                &Attrs::new().family(family),
                Shaping::Advanced,
                None,
            );
            for line in buf.lines.iter_mut() {
                line.set_align(Some(Align::Center));
            }
            buf.shape_until_scroll(font_system, false);
        }

        if let Some(ref mut buf) = self.date_buffer {
            let family = parse_family(&self.font_family);
            buf.set_text(
                font_system,
                &date_str,
                &Attrs::new().family(family),
                Shaping::Advanced,
                None,
            );
            for line in buf.lines.iter_mut() {
                line.set_align(Some(Align::Center));
            }
            buf.shape_until_scroll(font_system, false);
        }
    }

    fn text_areas(&self) -> Vec<TextArea<'_>> {
        let mut areas = Vec::new();
        let [r, g, b] = self.text_color;
        let color = Color::rgba(r, g, b, self.current_alpha);
        let w = self.screen_w;
        let h = self.screen_h;

        if let Some(ref buf) = self.time_buffer {
            areas.push(TextArea {
                buffer: buf,
                left: self.x,
                top: self.y,
                scale: 1.0,
                bounds: TextBounds {
                    left: 0,
                    top: 0,
                    right: w as i32,
                    bottom: h as i32,
                },
                default_color: color,
                custom_glyphs: &[],
            });
        }

        if let Some(ref buf) = self.date_buffer {
            let date_y = self.y + self.font_size * 1.2;
            areas.push(TextArea {
                buffer: buf,
                left: self.x,
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
            });
        }

        areas
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.screen_w = width;
        self.screen_h = height;
        let tw = self.text_block_width();
        let th = self.text_block_height();
        if let Some(ref mut anim) = self.animation {
            anim.update_screen_size(width as f32, height as f32, tw, th);
        }
    }
}
