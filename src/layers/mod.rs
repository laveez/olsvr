mod clock;
mod debug;
mod weather;

use crate::layer::Layer;

pub fn create_layer(layer_type: &str) -> Box<dyn Layer> {
    match layer_type {
        "clock" => Box::new(clock::ClockLayer::default()),
        "debug" => Box::new(debug::DebugLayer::default()),
        "weather" => Box::new(weather::WeatherLayer::default()),
        _ => panic!("Unknown layer type: {layer_type}"),
    }
}
