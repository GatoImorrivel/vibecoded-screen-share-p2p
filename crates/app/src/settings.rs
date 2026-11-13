use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    Light,
    Dark,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub nickname: String,
    pub theme: ThemeChoice,
    /// Apps never included in whole-desktop audio (substring match on name or binary).
    pub exclude_apps: Vec<String>,
    pub max_fps: u32,
    /// Longest edge limit of the outgoing video, as a height (720, 1080, 1440, 2160).
    pub max_height: u32,
    pub max_bitrate_kbps: u32,
    /// Encode and decode video on the GPU when possible.
    pub hardware_video: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            nickname: String::new(),
            theme: ThemeChoice::Dark,
            exclude_apps: vec!["discord".into()],
            max_fps: 30,
            max_height: 1080,
            max_bitrate_kbps: 6000,
            hardware_video: true,
        }
    }
}

impl Settings {
    fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "screenshareapp")
            .map(|d| d.config_dir().join("settings.toml"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|err| {
                tracing::warn!("ignoring invalid settings at {}: {err}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        let result = (|| -> anyhow::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, toml::to_string_pretty(self)?)?;
            Ok(())
        })();
        if let Err(err) = result {
            tracing::warn!("saving settings failed: {err:#}");
        }
    }

    pub fn max_size(&self) -> (u32, u32) {
        let h = self.max_height.max(360);
        (h * 16 / 9, h)
    }
}
