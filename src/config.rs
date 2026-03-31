use anyhow::{Context, Result};
use dirs::home_dir;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Which screen edge the bar docks to.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BarPosition {
    Top,
    Bottom,
}

impl Default for BarPosition {
    fn default() -> Self {
        Self::Bottom
    }
}

/// RGBA color stored as a hex string (e.g. `"#1e1e2e"` or `"#1e1e2eff"`).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Color(pub String);

impl Color {
    /// Parse into `(r, g, b, a)` bytes.
    pub fn to_rgba(&self) -> (u8, u8, u8, u8) {
        let s = self.0.trim_start_matches('#');
        let n = u32::from_str_radix(s, 16).unwrap_or(0);
        match s.len() {
            6 => {
                let r = ((n >> 16) & 0xFF) as u8;
                let g = ((n >> 8) & 0xFF) as u8;
                let b = (n & 0xFF) as u8;
                (r, g, b, 255)
            }
            8 => {
                let r = ((n >> 24) & 0xFF) as u8;
                let g = ((n >> 16) & 0xFF) as u8;
                let b = ((n >> 8) & 0xFF) as u8;
                let a = (n & 0xFF) as u8;
                (r, g, b, a)
            }
            _ => (0, 0, 0, 255),
        }
    }

    /// Convert to a `tiny_skia::Color`.
    pub fn to_skia(&self) -> tiny_skia::Color {
        let (r, g, b, a) = self.to_rgba();
        tiny_skia::Color::from_rgba8(r, g, b, a)
    }
}

/// Top-level config file schema.
///
/// Loaded from `%APPDATA%\glazeid\config.toml` on Windows, with sane defaults
/// when the file is absent.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Which screen edge the bar docks to (`"top"` or `"bottom"`).
    pub position: BarPosition,

    /// How far along the edge to place the bar, as a percentage of the
    /// monitor's width (for top/bottom) in the range `0.0`–`100.0`.
    ///
    /// `0.0` = left-most (default), `50.0` = centred, `100.0` = right-most
    /// (bar would be flush with the right edge).
    pub offset_percent: f32,

    /// GlazeWM IPC port.
    pub glazewm_port: u16,

    /// Milliseconds to wait before retrying a failed IPC connection.
    pub reconnect_delay_ms: u64,

    /// Background color of the bar.
    pub background: Color,

    /// Text color for inactive workspaces.
    pub foreground: Color,

    /// Background color of the active workspace pill.
    pub active_bg: Color,

    /// Text color of the active workspace pill.
    pub active_fg: Color,

    /// Font size in logical pixels.
    pub font_size: f32,

    /// Horizontal padding inside each workspace label, in logical pixels.
    pub label_padding_x: f32,

    /// Vertical padding above and below the text inside each pill, in logical
    /// pixels.  The total bar height = font cap-height + 2 × label_padding_y.
    pub label_padding_y: f32,

    /// Corner radius of the active workspace pill, in logical pixels.
    pub pill_radius: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            position: BarPosition::Bottom,
            offset_percent: 0.0,
            glazewm_port: 6123,
            reconnect_delay_ms: 2000,
            background: Color("#00000000".into()),
            foreground: Color("#ffffff".into()),
            active_bg: Color("#DA3B01".into()),
            active_fg: Color("#000000".into()),
            font_size: 13.0,
            label_padding_x: 10.0,
            label_padding_y: 4.0,
            pill_radius: 4.0,
        }
    }
}

impl Config {
    /// Load the config from disk, falling back to defaults if the file does not
    /// exist. Returns an error only if the file exists but cannot be parsed.
    pub fn load() -> Result<Self> {
        let path = config_path();

        if !path.exists() {
            tracing::debug!(
                path = %path.display(),
                "Config file not found, using defaults."
            );
            return Ok(Self::default());
        }

        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config at {}", path.display()))?;

        serde_yaml::from_str(&raw)
            .with_context(|| format!("Failed to parse config at {}", path.display()))
    }
}

/// Returns the platform-appropriate config file path.
///
/// macOS:   ~/.config/.glzr/glazeid/config.yaml
/// Windows: %USERPROFILE%\.glzr\glazeid\config.yaml
pub fn config_path() -> PathBuf {
    let home = home_dir().unwrap_or_else(|| PathBuf::from("."));

    #[cfg(target_os = "windows")]
    return home.join(".glzr").join("glazeid").join("config.yaml");

    #[cfg(not(target_os = "windows"))]
    return home
        .join(".config")
        .join(".glzr")
        .join("glazeid")
        .join("config.yaml");
}
