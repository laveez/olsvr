use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::layer::Layer;
use crate::layers;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub timeout: u32,
    pub hold: u32,
    pub fade_duration: u32,
    pub font_size: u32,
    pub font_family: String,
    pub time_format: String,
    pub date_format: String,
    pub color: [u8; 3],
    pub edge_padding: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layers: Option<Vec<toml::value::Table>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timeout: 5,
            hold: 10,
            fade_duration: 1500,
            font_size: 200,
            font_family: "sans-serif".into(),
            time_format: "24h".into(),
            date_format: "%a %d %b".into(),
            color: [255, 255, 255],
            edge_padding: 50,
            layers: None,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        let home = std::env::var("HOME").expect("HOME not set");
        PathBuf::from(home).join(".olsvr.toml")
    }

    pub fn exists() -> bool {
        Self::path().exists()
    }

    pub fn load() -> Self {
        let path = Self::path();
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(contents) => match toml::from_str(&contents) {
                    Ok(config) => return config,
                    Err(e) => log::warn!("Failed to parse {}: {e}, using defaults", path.display()),
                },
                Err(e) => log::warn!("Failed to read {}: {e}, using defaults", path.display()),
            }
        }
        Self::default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let contents = toml::to_string_pretty(self).expect("Failed to serialize config");
        std::fs::write(Self::path(), contents)
    }

    /// Build layer instances and their config tables.
    /// If `[[layers]]` is defined, use those. Otherwise, wrap flat config as a single clock layer.
    pub fn build_layers(&self, debug: bool) -> (Vec<Box<dyn Layer>>, Vec<toml::value::Table>) {
        let (mut layer_list, mut layer_configs) = if let Some(ref layers_cfg) = self.layers {
            let mut list = Vec::new();
            let mut cfgs = Vec::new();
            for table in layers_cfg {
                let layer_type = table
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("clock");
                list.push(layers::create_layer(layer_type));
                cfgs.push(table.clone());
            }
            (list, cfgs)
        } else {
            // Backward compat: wrap flat config fields as a single clock layer
            let mut table = toml::value::Table::new();
            table.insert("type".into(), toml::Value::String("clock".into()));
            table.insert(
                "font_size".into(),
                toml::Value::Integer(self.font_size as i64),
            );
            table.insert(
                "font_family".into(),
                toml::Value::String(self.font_family.clone()),
            );
            table.insert(
                "time_format".into(),
                toml::Value::String(self.time_format.clone()),
            );
            table.insert(
                "date_format".into(),
                toml::Value::String(self.date_format.clone()),
            );
            table.insert(
                "color".into(),
                toml::Value::Array(
                    self.color
                        .iter()
                        .map(|&c| toml::Value::Integer(c as i64))
                        .collect(),
                ),
            );
            table.insert("hold".into(), toml::Value::Integer(self.hold as i64));
            table.insert(
                "fade_duration".into(),
                toml::Value::Integer(self.fade_duration as i64),
            );
            table.insert(
                "edge_padding".into(),
                toml::Value::Integer(self.edge_padding as i64),
            );
            (vec![layers::create_layer("clock")], vec![table])
        };

        if debug {
            layer_list.push(layers::create_layer("debug"));
            layer_configs.push(toml::value::Table::new());
        }

        (layer_list, layer_configs)
    }

    /// Return the weather location if a weather layer is configured, along with its update interval.
    pub fn weather_config(&self) -> Option<(String, u64)> {
        if let Some(ref layers_cfg) = self.layers {
            for table in layers_cfg {
                if table.get("type").and_then(|v| v.as_str()) == Some("weather") {
                    let location = table
                        .get("location")
                        .and_then(|v| v.as_str())
                        .unwrap_or("helsinki")
                        .to_string();
                    let interval = table
                        .get("update_interval")
                        .and_then(|v| v.as_integer())
                        .unwrap_or(600) as u64;
                    return Some((location, interval));
                }
            }
        }
        None
    }
}
