use std::fmt::Write;
use std::sync::{Arc, RwLock};

use glyphon::{
    Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, TextArea, TextBounds, Wrap,
    cosmic_text::Align,
};

use crate::animation::Animation;
use crate::data::DataCache;
use crate::layer::{GpuContext, Layer};

const DEFAULT_DATE_FORMAT: &str = "%a %d %b";

pub struct ClockLayer {
    time_buffer: Option<Buffer>,
    date_buffer: Option<Buffer>,
    animation: Option<Animation>,
    data_cache: Option<Arc<RwLock<DataCache>>>,
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
    block_w: f32,
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
            data_cache: None,
            font_size: 200.0,
            font_family: "sans-serif".into(),
            time_format: "24h".into(),
            date_format: DEFAULT_DATE_FORMAT.into(),
            text_color: [255, 255, 255],
            hold: 10,
            fade_duration: 1500,
            edge_padding: 50,
            x: 0.0,
            y: 0.0,
            block_w: 0.0,
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

fn format_date(date_format: &str, at: &chrono::DateTime<chrono::Local>) -> String {
    let mut date = String::new();
    if write!(date, "{}", at.format(date_format)).is_err() {
        date = at.format(DEFAULT_DATE_FORMAT).to_string();
    }
    date
}

fn widest_line_w(
    font_system: &mut FontSystem,
    family: Family<'_>,
    font_size: f32,
    samples: impl IntoIterator<Item = String>,
) -> f32 {
    let mut buffer = Buffer::new(font_system, Metrics::new(font_size, font_size * 1.2));
    let attrs = Attrs::new().family(family);
    let mut widest = 0.0_f32;
    for text in samples {
        buffer.set_text(&text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(font_system, false);
        for run in buffer.layout_runs() {
            widest = widest.max(run.line_w);
        }
    }
    widest
}

impl ClockLayer {
    fn text_block_width(&self) -> f32 {
        self.block_w
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
        data_cache: Arc<RwLock<DataCache>>,
    ) {
        self.data_cache = Some(data_cache);
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
        let date_fs = fs * 0.3;
        let family = parse_family(&self.font_family);

        // Sized to the widest text the clock can show, not the current text, so centring,
        // placement and the weather layer keep one fixed width as the digits change.
        let meridiems: &[&str] = if self.time_format == "12h" {
            &[" AM", " PM"]
        } else {
            &[""]
        };
        let times =
            ('0'..='9').flat_map(|d| meridiems.iter().map(move |m| format!("{d}{d}:{d}{d}{m}")));
        let today = chrono::Local::now();
        let year_of_dates = (0..366)
            .map(|day| format_date(&self.date_format, &(today + chrono::Duration::days(day))));
        let time_w = widest_line_w(font_system, family, fs, times);
        let date_w = widest_line_w(font_system, family, date_fs, year_of_dates);
        self.block_w = time_w.max(date_w);

        let mut time_buffer = Buffer::new(font_system, Metrics::new(fs, fs * 1.2));
        time_buffer.set_wrap(Wrap::None);
        time_buffer.set_size(Some(self.block_w), Some(fs * 1.5));
        time_buffer.set_text(
            "00:00",
            &Attrs::new().family(family),
            Shaping::Advanced,
            None,
        );
        for line in time_buffer.lines.iter_mut() {
            line.set_align(Some(Align::Center));
        }
        time_buffer.shape_until_scroll(font_system, false);

        let mut date_buffer = Buffer::new(font_system, Metrics::new(date_fs, date_fs * 1.2));
        date_buffer.set_wrap(Wrap::None);
        date_buffer.set_size(Some(self.block_w), Some(date_fs * 1.5));
        date_buffer.set_text(
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

        // Publish position so other layers (weather) can follow
        if let Some(ref cache) = self.data_cache
            && let Ok(mut c) = cache.write()
        {
            c.clock_pos = Some(crate::data::ClockPosition {
                x: self.x,
                y: self.y,
                alpha: self.current_alpha,
                block_height: self.text_block_height(),
                block_width: self.text_block_width(),
            });
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
        let date_str = format_date(&self.date_format, &now);

        if let Some(ref mut buf) = self.time_buffer {
            let family = parse_family(&self.font_family);
            buf.set_text(
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
