use std::ops::RangeInclusive;
use std::path::PathBuf;

use chrono::format::StrftimeItems;
use serde::{Deserialize, Serialize};

use crate::layer::Layer;
use crate::layers;

/// Which displays the screensaver activates on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DisplayScope {
    /// Only the primary display (preserves the original single-window behavior).
    #[default]
    Primary,
    /// Every connected display on macOS. Wayland renders one surface on the output the
    /// compositor chooses.
    All,
}

/// When the backend holds a "keep the display awake" assertion/inhibitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum KeepAwake {
    /// Only while the screensaver is showing. On GNOME this also stops the auto-lock and the
    /// screen blank while the saver is visible; on macOS, a lock that follows display sleep.
    #[default]
    WhileActive,
    /// For the whole session, released while it is locked (macOS and GNOME). On GNOME this also
    /// stops the auto-lock; on macOS, a lock that follows display sleep.
    Always,
    /// Never inhibit display sleep. The desktop's own idle lock then runs over the saver.
    Off,
}

/// Activation + keep-awake policy, configured via the `[activation]` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Activation {
    pub displays: DisplayScope,
    pub keep_awake: KeepAwake,
}

impl Default for Activation {
    fn default() -> Self {
        Self {
            displays: DisplayScope::Primary,
            keep_awake: KeepAwake::WhileActive,
        }
    }
}

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
    pub activation: Activation,
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
            activation: Activation::default(),
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
        Self::try_load().unwrap_or_else(|e| {
            log::error!("Using the default settings: {e}");
            Self::default()
        })
    }

    /// Like `load`, but a file that cannot be read or parsed is an error instead of the defaults.
    pub fn try_load() -> Result<Self, String> {
        let path = Self::path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        toml::from_str(&contents).map_err(|e| format!("Failed to parse {}: {e}", path.display()))
    }

    /// Replaces each value the layers cannot use, in the flat fields and in every `[[layers]]`
    /// table, with its default, and describes each replacement.
    pub fn validate(&mut self) -> Vec<String> {
        let defaults = Self::default();
        let mut problems = Vec::new();
        // At 0 the macOS saver comes back at the first poll after every dismiss, and the GNOME
        // idle monitor activates once and then never again.
        if self.timeout == 0 {
            problems.push(format!(
                "timeout = 0 is not at least 1 minute, using {}",
                defaults.timeout
            ));
            self.timeout = defaults.timeout;
        }
        if !EDGE_PADDINGS.contains(&i64::from(self.edge_padding)) {
            problems.push(format!(
                "edge_padding = {} is not {EDGE_PADDING_RULE}, using {}",
                self.edge_padding, defaults.edge_padding
            ));
            self.edge_padding = defaults.edge_padding;
        }
        if !FONT_SIZES.contains(&i64::from(self.font_size)) {
            problems.push(format!(
                "font_size = {} is not {FONT_SIZE_RULE}, using {}",
                self.font_size, defaults.font_size
            ));
            self.font_size = defaults.font_size;
        }
        if !is_time_format(&self.time_format) {
            problems.push(format!(
                "time_format = {:?} is not {TIME_FORMAT_RULE}, using {:?}",
                self.time_format, defaults.time_format
            ));
            self.time_format = defaults.time_format;
        }
        if !is_date_format(&self.date_format) {
            problems.push(format!(
                "date_format = {:?} is not {DATE_FORMAT_RULE}, using {:?}",
                self.date_format, defaults.date_format
            ));
            self.date_format = defaults.date_format;
        }
        for (i, table) in self.layers.iter_mut().flatten().enumerate() {
            table.retain(|key, value| match broken_layer_rule(key, value) {
                Some(rule) => {
                    problems.push(format!(
                        "layer {}: {key} = {value} is not {rule}, using the default",
                        i + 1
                    ));
                    false
                }
                None => true,
            });
        }
        problems
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
            for (i, table) in layers_cfg.iter().enumerate() {
                let layer_type = layer_type(table);
                let Some(layer) = layers::create_layer(layer_type) else {
                    log::error!(
                        "Config: layer {}: type = {layer_type:?} is unknown, skipping the layer",
                        i + 1
                    );
                    continue;
                };
                list.push(layer);
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
            (
                layers::create_layer("clock").into_iter().collect(),
                vec![table],
            )
        };

        if debug {
            layer_list.extend(layers::create_layer("debug"));
            layer_configs.push(toml::value::Table::new());
        }

        (layer_list, layer_configs)
    }

    /// Return the weather location if a weather layer is configured, along with its update
    /// interval in seconds, raised to `MIN_WEATHER_INTERVAL` when the configured one is lower.
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
                        .unwrap_or(600);
                    let floor = i64::from(MIN_WEATHER_INTERVAL);
                    if interval < floor {
                        log::warn!(
                            "Config: update_interval = {interval} is under {floor} seconds, using {floor}"
                        );
                    }
                    return Some((location, interval.max(floor) as u64));
                }
            }
        }
        None
    }
}

/// Shortest weather `update_interval` in seconds: FMI's forecast changes hourly.
pub const MIN_WEATHER_INTERVAL: u32 = 300;
/// The type `build_layers` gives a table: a missing or non-string `type` is a clock.
pub fn layer_type(table: &toml::value::Table) -> &str {
    table
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("clock")
}

const FONT_SIZES: RangeInclusive<i64> = 1..=2000;
const FONT_SIZE_RULE: &str = "a whole number from 1 to 2000";
/// Wider than an 8K display (7680 px). From about 2^24, `pad + 1.0 == pad` in f32, and the empty
/// range panics `Animation::random_position`.
const EDGE_PADDINGS: RangeInclusive<i64> = 0..=10000;
const EDGE_PADDING_RULE: &str = "a whole number from 0 to 10000";
const TIME_FORMAT_RULE: &str = r#""24h" or "12h""#;
const DATE_FORMAT_RULE: &str = "a valid chrono date format";

fn is_time_format(format: &str) -> bool {
    matches!(format, "24h" | "12h")
}

fn is_date_format(format: &str) -> bool {
    StrftimeItems::new(format).parse().is_ok()
}

/// The rule `value` breaks, for the `[[layers]]` keys whose values the layers convert.
fn broken_layer_rule(key: &str, value: &toml::Value) -> Option<&'static str> {
    let (valid, rule) = match key {
        "font_size" => (
            value.as_integer().is_some_and(|n| FONT_SIZES.contains(&n)),
            FONT_SIZE_RULE,
        ),
        "hold" | "fade_duration" => (
            value.as_integer().is_some_and(|n| u32::try_from(n).is_ok()),
            "a whole number from 0 to 4294967295",
        ),
        "edge_padding" => (
            value
                .as_integer()
                .is_some_and(|n| EDGE_PADDINGS.contains(&n)),
            EDGE_PADDING_RULE,
        ),
        "color" => (
            value.as_array().is_some_and(|rgb| {
                rgb.len() == 3
                    && rgb
                        .iter()
                        .all(|c| c.as_integer().is_some_and(|n| u8::try_from(n).is_ok()))
            }),
            "three whole numbers from 0 to 255",
        ),
        "time_format" => (value.as_str().is_some_and(is_time_format), TIME_FORMAT_RULE),
        "date_format" => (value.as_str().is_some_and(is_date_format), DATE_FORMAT_RULE),
        _ => return None,
    };
    (!valid).then_some(rule)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_defaults_preserve_linux_behavior() {
        let a = Activation::default();
        assert_eq!(a.displays, DisplayScope::Primary);
        assert_eq!(a.keep_awake, KeepAwake::WhileActive);
    }

    #[test]
    fn config_without_activation_table_uses_defaults() {
        let cfg: Config = toml::from_str("timeout = 5").unwrap();
        assert_eq!(cfg.activation, Activation::default());
    }

    #[test]
    fn activation_parses_all_and_always() {
        let cfg: Config =
            toml::from_str("[activation]\ndisplays = \"all\"\nkeep_awake = \"always\"").unwrap();
        assert_eq!(cfg.activation.displays, DisplayScope::All);
        assert_eq!(cfg.activation.keep_awake, KeepAwake::Always);
    }
}
