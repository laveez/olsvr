mod clock;
mod debug;
mod weather;

use crate::layer::Layer;

pub fn create_layer(layer_type: &str) -> Option<Box<dyn Layer>> {
    match layer_type {
        "clock" => Some(Box::new(clock::ClockLayer::default())),
        "debug" => Some(Box::new(debug::DebugLayer::default())),
        "weather" => Some(Box::new(weather::WeatherLayer::default())),
        _ => None,
    }
}
