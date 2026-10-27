use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const FMI_URL: &str = "https://opendata.fmi.fi/wfs?request=getFeature&storedquery_id=fmi::forecast::harmonie::surface::point::simple&parameters=Temperature,WeatherSymbol3,WindSpeedMS&timestep=60";
/// Without a limit, a connection that dies silently (a laptop lid closed mid-fetch) blocks the
/// only fetch thread for good.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Weather data older than this many update intervals is no longer shown.
const STALE_AFTER_INTERVALS: u32 = 3;

#[allow(dead_code)]
pub struct WeatherData {
    pub temperature: f32,
    pub weather_symbol: u8,
    pub wind_speed: f32,
    pub location: String,
    pub fetched_at: Instant,
    pub max_age: Duration,
}

impl WeatherData {
    pub fn is_fresh(&self) -> bool {
        self.fetched_at.elapsed() <= self.max_age
    }
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

enum FetchError {
    /// FMI's own explanation of why it refused the request, e.g. an unknown place.
    Rejected(String),
    Failed(String),
}

fn fetch_fmi_weather(agent: &ureq::Agent, location: &str) -> Result<(f32, u8, f32), FetchError> {
    // FMI's CDN caches each URL for up to 6 h, so without an explicit window the first step
    // can be hours old. Asking for the nearest full hour changes the URL every hour.
    let start = (chrono::Utc::now() + chrono::TimeDelta::minutes(30))
        .format("%Y-%m-%dT%H:00:00Z")
        .to_string();
    let mut response = agent
        .get(FMI_URL)
        .query("starttime", &start)
        .query("endtime", &start)
        .query("place", location)
        .call()
        .map_err(|e| FetchError::Failed(format!("HTTP request failed: {e}")))?;
    let status = response.status();
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| FetchError::Failed(format!("Failed to read response: {e}")))?;

    let forecast = if status.is_success() {
        parse_fmi_xml(&body)
    } else {
        Err(format!("HTTP status {status}"))
    };
    forecast.map_err(|e| match exception_text(&body) {
        Some(text) => FetchError::Rejected(text),
        None => FetchError::Failed(e),
    })
}

/// The first `ExceptionText` of an OGC ExceptionReport, which is what FMI answers a refused
/// request with.
fn exception_text(xml: &str) -> Option<String> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event().ok()? {
            Event::Start(e) if e.name().into_inner().ends_with("ExceptionText") => {
                let raw = reader.read_text(e.name()).ok()?;
                let text = quick_xml::escape::unescape(&raw).ok()?;
                return Some(text.trim().to_string());
            }
            Event::Eof => return None,
            _ => {}
        }
    }
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
                let name = e.name().into_inner();
                if name.ends_with("ParameterName") {
                    in_param_name = true;
                } else if name.ends_with("ParameterValue") {
                    in_param_value = true;
                }
            }
            Ok(Event::Text(e)) => {
                let text = e.xml10_content().trim().to_string();
                if in_param_name {
                    current_param = text;
                    in_param_name = false;
                } else if in_param_value {
                    match current_param.as_str() {
                        // Forecast endpoint uses capitalized names;
                        // take first (nearest future) value only. Parsing an f32 also
                        // accepts "NaN" and "inf", which are skipped.
                        "Temperature" => {
                            if temperature.is_none()
                                && let Ok(v) = text.parse::<f32>()
                                && v.is_finite()
                            {
                                temperature = Some(v);
                            }
                        }
                        "WeatherSymbol3" => {
                            if weather_symbol.is_none()
                                && let Ok(v) = text.parse::<f32>()
                                && v.is_finite()
                            {
                                weather_symbol = Some(v as u8);
                            }
                        }
                        "WindSpeedMS" => {
                            if wind_speed.is_none()
                                && let Ok(v) = text.parse::<f32>()
                                && v.is_finite()
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
        // FMI explains a refused request in the body of its 400, which ureq drops by default.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(FETCH_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        let display_name = capitalize(&location);
        let max_age = interval.saturating_mul(STALE_AFTER_INTERVALS);
        let mut last_rejection: Option<String> = None;
        loop {
            match fetch_fmi_weather(&agent, &location) {
                Ok((temp, symbol, wind)) => {
                    last_rejection = None;
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
                            max_age,
                        });
                    }
                }
                Err(FetchError::Rejected(text)) => {
                    if last_rejection.as_ref() != Some(&text) {
                        log::error!("Weather: FMI refused the request for {location:?}: {text}");
                        last_rejection = Some(text);
                    }
                }
                Err(FetchError::Failed(e)) => {
                    log::warn!("Weather fetch failed: {e}");
                }
            }
            std::thread::sleep(interval);
        }
    })
}
