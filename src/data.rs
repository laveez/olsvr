use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

#[allow(dead_code)]
pub struct WeatherData {
    pub temperature: f32,
    pub weather_symbol: u8,
    pub wind_speed: f32,
    pub location: String,
    pub fetched_at: Instant,
}

pub struct DebugData {
    pub frame_count: u64,
    pub frame_ms: u64,
    pub from_callback: bool,
    pub uptime_secs: u64,
}

pub struct ClockPosition {
    pub x: f32,
    pub y: f32,
    pub alpha: u8,
    /// Total height of clock text block (time + date)
    pub block_height: f32,
    pub block_width: f32,
}

#[derive(Default)]
pub struct DataCache {
    pub weather: Option<WeatherData>,
    pub debug: Option<DebugData>,
    pub clock_pos: Option<ClockPosition>,
}

fn fetch_fmi_weather(location: &str) -> Result<(f32, u8, f32), String> {
    let url = format!(
        "https://opendata.fmi.fi/wfs?request=getFeature&storedquery_id=fmi::forecast::harmonie::surface::point::simple&place={}&parameters=Temperature,WeatherSymbol3,WindSpeedMS&timestep=60",
        location
    );

    let body = ureq::get(&url)
        .call()
        .map_err(|e| format!("HTTP request failed: {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("Failed to read response: {e}"))?;

    parse_fmi_xml(&body)
}

fn parse_fmi_xml(xml: &str) -> Result<(f32, u8, f32), String> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(xml);
    let mut temperature: Option<f32> = None;
    let mut weather_symbol: Option<u8> = None;
    let mut wind_speed: Option<f32> = None;
    let mut current_param = String::new();
    let mut in_param_name = false;
    let mut in_param_value = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name.ends_with("ParameterName") {
                    in_param_name = true;
                } else if name.ends_with("ParameterValue") {
                    in_param_value = true;
                }
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().trim().to_string();
                if in_param_name {
                    current_param = text;
                    in_param_name = false;
                } else if in_param_value {
                    match current_param.as_str() {
                        // Forecast endpoint uses capitalized names;
                        // take first (nearest future) value only
                        "Temperature" => {
                            if temperature.is_none()
                                && let Ok(v) = text.parse::<f32>()
                            {
                                temperature = Some(v);
                            }
                        }
                        "WeatherSymbol3" => {
                            if weather_symbol.is_none()
                                && let Ok(v) = text.parse::<f32>()
                            {
                                weather_symbol = Some(v as u8);
                            }
                        }
                        "WindSpeedMS" => {
                            if wind_speed.is_none()
                                && let Ok(v) = text.parse::<f32>()
                            {
                                wind_speed = Some(v);
                            }
                        }
                        _ => {}
                    }
                    in_param_value = false;
                }
            }
            Ok(Event::End(_)) => {
                in_param_name = false;
                in_param_value = false;
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {e}")),
            _ => {}
        }
    }

    match (temperature, weather_symbol) {
        (Some(t), Some(s)) => Ok((t, s, wind_speed.unwrap_or(0.0))),
        (Some(t), None) => Ok((t, 0, wind_speed.unwrap_or(0.0))),
        _ => Err("No temperature data found in FMI response".into()),
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

pub fn start_fetch_thread(
    cache: Arc<RwLock<DataCache>>,
    location: String,
    interval: Duration,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let display_name = capitalize(&location);
        loop {
            match fetch_fmi_weather(&location) {
                Ok((temp, symbol, wind)) => {
                    log::info!(
                        "Weather: {temp:.1}°C, symbol={symbol}, wind={wind:.1}m/s ({display_name})"
                    );
                    if let Ok(mut c) = cache.write() {
                        c.weather = Some(WeatherData {
                            temperature: temp,
                            weather_symbol: symbol,
                            wind_speed: wind,
                            location: display_name.clone(),
                            fetched_at: Instant::now(),
                        });
                    }
                }
                Err(e) => {
                    log::warn!("Weather fetch failed: {e}");
                }
            }
            std::thread::sleep(interval);
        }
    })
}
