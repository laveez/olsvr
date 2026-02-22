use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

pub struct WeatherData {
    pub temperature: f32,
    pub weather_symbol: u8,
    pub location: String,
    #[allow(dead_code)]
    pub fetched_at: Instant,
}

pub struct DebugData {
    pub frame_count: u64,
    pub frame_ms: u64,
    pub from_callback: bool,
    pub uptime_secs: u64,
}

#[derive(Default)]
pub struct DataCache {
    pub weather: Option<WeatherData>,
    pub debug: Option<DebugData>,
}

fn fetch_fmi_weather(location: &str) -> Result<(f32, u8), String> {
    let url = format!(
        "https://opendata.fmi.fi/wfs?request=getFeature&storedquery_id=fmi::observations::weather::simple&place={}&parameters=temperature,weathersymbol3",
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

fn parse_fmi_xml(xml: &str) -> Result<(f32, u8), String> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(xml);
    let mut temperature: Option<f32> = None;
    let mut weather_symbol: Option<u8> = None;
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
                        "temperature" => {
                            // Take the last (most recent) value
                            if let Ok(v) = text.parse::<f32>() {
                                temperature = Some(v);
                            }
                        }
                        "weathersymbol3" => {
                            if let Ok(v) = text.parse::<f32>() {
                                weather_symbol = Some(v as u8);
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
        (Some(t), Some(s)) => Ok((t, s)),
        (Some(t), None) => Ok((t, 0)),
        _ => Err("No temperature data found in FMI response".into()),
    }
}

pub fn start_fetch_thread(
    cache: Arc<RwLock<DataCache>>,
    location: String,
    interval: Duration,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        loop {
            match fetch_fmi_weather(&location) {
                Ok((temp, symbol)) => {
                    log::info!("Weather: {temp:.1}°C, symbol={symbol} ({location})");
                    if let Ok(mut c) = cache.write() {
                        c.weather = Some(WeatherData {
                            temperature: temp,
                            weather_symbol: symbol,
                            location: location.clone(),
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
