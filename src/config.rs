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
