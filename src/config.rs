use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub opacity: f64,
    pub width: i32,
    pub height: i32,
    pub font_size: u32,
    pub theme: Theme,
    /// API key for the Groq speech-to-text endpoint
    /// (https://console.groq.com/keys).
    #[serde(alias = "soniox_api_key")]
    pub groq_api_key: String,
    /// Optional ISO-639-1 language hint (e.g. "pt", "en"). Empty string
    /// means let Whisper auto-detect.
    pub transcribe_language: String,
    /// GIF recorder frame rate. Valid set: 12 / 15 / 20 / 24 / 30.
    pub gif_fps: u32,
    /// GIF recorder quality preset. Drives both the libx264 CRF on
    /// capture and the ffmpeg palette/dither on conversion.
    pub gif_quality: GifQuality,
    /// Saved top-left position for the GIF recorder overlay (global
    /// screen pixels). First-time setup runs the placement picker;
    /// subsequent sessions reuse this. None = ask again next time.
    pub recorder_overlay_x: Option<i32>,
    pub recorder_overlay_y: Option<i32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GifQuality {
    /// CRF 17, full-frame palette stats, Sierra2_4a dither. Largest
    /// file, sharpest motion and gradients.
    High,
    /// CRF 22, diff-mode palette, Sierra2_4a dither. The default —
    /// barely-perceptible quality drop in exchange for ~half the size.
    Balanced,
    /// CRF 28, diff-mode palette, Bayer dither. Smallest file; visible
    /// banding on gradients. Good for code-only screencasts.
    Compact,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    Dark,
    Light,
    System,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            opacity: 0.94,
            width: 820,
            height: 620,
            font_size: 15,
            theme: Theme::Dark,
            groq_api_key: String::new(),
            transcribe_language: String::new(),
            gif_fps: 24,
            gif_quality: GifQuality::Balanced,
            recorder_overlay_x: None,
            recorder_overlay_y: None,
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let Ok(path) = config_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => toml::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

fn config_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("no XDG config dir")?
        .join("jot")
        .join("config.toml"))
}
