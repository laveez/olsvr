use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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

    pub fn font_family_enum(&self) -> glyphon::Family<'_> {
        match self.font_family.as_str() {
            "sans-serif" => glyphon::Family::SansSerif,
            "serif" => glyphon::Family::Serif,
            "monospace" => glyphon::Family::Monospace,
            "cursive" => glyphon::Family::Cursive,
            "fantasy" => glyphon::Family::Fantasy,
            name => glyphon::Family::Name(name),
        }
    }
}
