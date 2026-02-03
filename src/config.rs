use anyhow::Result;
use log::debug;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Application configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Volume level (0.0 to 1.0)
    #[serde(default = "default_volume")]
    pub volume: f32,
}

fn default_volume() -> f32 {
    0.8
}

impl Default for Config {
    fn default() -> Self {
        Self {
            volume: default_volume(),
        }
    }
}

impl Config {
    /// Get config directory path
    pub fn dir() -> PathBuf {
        // Use /etc/lego-radio on Linux, current dir elsewhere (for testing)
        #[cfg(target_os = "linux")]
        {
            PathBuf::from("/etc/lego-radio")
        }
        #[cfg(not(target_os = "linux"))]
        {
            // For development/testing on Mac, use current directory
            PathBuf::from(".")
        }
    }

    /// Get config file path
    pub fn path() -> PathBuf {
        Self::dir().join("config.json")
    }

    /// Load config from file, or create default if not exists
    pub fn load() -> Self {
        let path = Self::path();

        if path.exists() {
            match fs::read_to_string(&path) {
                Ok(contents) => match serde_json::from_str(&contents) {
                    Ok(config) => {
                        debug!("Loaded config from {:?}", path);
                        return config;
                    }
                    Err(e) => {
                        log::warn!("Failed to parse config: {}, using defaults", e);
                    }
                },
                Err(e) => {
                    log::warn!("Failed to read config: {}, using defaults", e);
                }
            }
        }

        Self::default()
    }

    /// Save config to file
    pub fn save(&self) -> Result<()> {
        let dir = Self::dir();
        if !dir.exists() {
            fs::create_dir_all(&dir)?;
        }

        let path = Self::path();
        let contents = serde_json::to_string_pretty(self)?;
        fs::write(&path, contents)?;

        debug!("Saved config to {:?}", path);
        Ok(())
    }

    /// Set volume (clamped to 0.0-1.0)
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_volume() {
        let config = Config::default();
        assert_eq!(config.volume, 0.8);
    }

    #[test]
    fn test_volume_clamping() {
        let mut config = Config::default();

        config.set_volume(1.5);
        assert_eq!(config.volume, 1.0);

        config.set_volume(-0.5);
        assert_eq!(config.volume, 0.0);

        config.set_volume(0.5);
        assert_eq!(config.volume, 0.5);
    }

    #[test]
    fn test_config_serialization() {
        let config = Config { volume: 0.75 };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.volume, 0.75);
    }
}
