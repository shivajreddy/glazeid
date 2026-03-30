use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Position of the bar on the screen edge.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BarPosition {
    Top,
    Bottom,
    Left,
    Right,
}

impl Default for BarPosition {
    fn default() -> Self {
        Self::Top
    }
}

/// RGBA color stored as hex string (e.g. `"#1e1e2e"` or `"#1e1e2eff"`).
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
/// Loaded from `~/.config/glazeid/config.toml` (or
/// `%APPDATA%\glazeid\config.toml` on Windows) with sane defaults when the
/// file is absent.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Which edge of each monitor the bar attaches to.
    pub position: BarPosition,
    /// Height (or width when position is Left/Right) of the bar in logical pixels.
    pub bar_size: u32,
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
    /// Horizontal padding around each workspace label (logical pixels).
    pub label_padding_x: f32,
    /// Vertical padding inside each workspace pill (logical pixels).
    pub label_padding_y: f32,
    /// Corner radius of the active workspace pill.
    pub pill_radius: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            position: BarPosition::Top,
            bar_size: 28,
            glazewm_port: 6123,
            reconnect_delay_ms: 2000,
            background: Color("#1e1e2e".into()),
            foreground: Color("#cdd6f4".into()),
            active_bg: Color("#89b4fa".into()),
            active_fg: Color("#1e1e2e".into()),
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

        toml::from_str(&raw)
            .with_context(|| format!("Failed to parse config at {}", path.display()))
    }
}

/// Returns the platform-appropriate config file path.
pub fn config_path() -> PathBuf {
    // Use `dirs::config_dir()` which maps to:
    //   Windows: %APPDATA%  (e.g. C:\Users\<user>\AppData\Roaming)
    //   macOS:   ~/Library/Application Support
    //   Linux:   ~/.config
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("glazeid").join("config.toml")
}
