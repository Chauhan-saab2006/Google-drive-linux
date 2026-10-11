use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Failed to determine standard system directories")]
    PathResolutionError,
    #[error("IO error accessing config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("Failed to parse TOML configuration: {0}")]
    ParseError(#[from] toml::de::Error),
    #[error("Failed to serialize TOML configuration: {0}")]
    SerializeError(#[from] toml::ser::Error),
}

/// Persistent configuration for Google Drive Linux.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    /// Filesystem mount target (defaults to ~/GoogleDrive).
    pub mount_point: PathBuf,
    /// Path to local cache storage.
    pub cache_dir: PathBuf,
    /// Whether to start the application/daemon on desktop login.
    pub auto_start: bool,
    /// Whether to automatically mount Google Drive when daemon starts.
    pub auto_mount: bool,
    /// Cache size limit in megabytes.
    pub cache_size_mb: u64,
    /// Background synchronization and remote change polling interval in seconds.
    #[serde(default = "default_sync_interval")]
    pub sync_interval_secs: u64,
    /// Logging level (trace, debug, info, warn, error).
    pub log_level: String,
    /// Optional Google Cloud OAuth 2.0 Client ID.
    pub client_id: Option<String>,
    /// Optional Google Cloud OAuth 2.0 Client Secret.
    pub client_secret: Option<String>,
}

fn default_sync_interval() -> u64 {
    30
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            mount_point: Self::default_mount_point(),
            cache_dir: Self::default_cache_dir(),
            auto_start: false,
            auto_mount: true,
            cache_size_mb: 5120, // 5 GB
            sync_interval_secs: 30,
            log_level: "info".to_string(),
            client_id: None,
            client_secret: None,
        }
    }
}

impl AppConfig {
    /// Return the default mount point (`~/GoogleDrive`).
    pub fn default_mount_point() -> PathBuf {
        if let Some(home) = dirs::home_dir() {
            home.join("GoogleDrive")
        } else {
            PathBuf::from("/tmp/GoogleDrive")
        }
    }

    /// Return the default cache directory (`~/.cache/google-drive-linux`).
    pub fn default_cache_dir() -> PathBuf {
        if let Some(cache_dir) = dirs::cache_dir() {
            cache_dir.join("google-drive-linux")
        } else {
            PathBuf::from("/tmp/google-drive-linux/cache")
        }
    }

    /// Return the default data directory (`~/.local/share/google-drive-linux`).
    pub fn default_data_dir() -> Result<PathBuf, ConfigError> {
        let data_dir = dirs::data_dir()
            .ok_or(ConfigError::PathResolutionError)?
            .join("google-drive-linux");
        Ok(data_dir)
    }

    /// Return the standard credentials file path (`~/.local/share/google-drive-linux/credentials.json`).
    pub fn default_credentials_path() -> Result<PathBuf, ConfigError> {
        Ok(Self::default_data_dir()?.join("credentials.json"))
    }

    /// Return the standard config file path (`~/.config/google-drive-linux/config.toml`).
    pub fn default_config_path() -> Result<PathBuf, ConfigError> {
        let config_dir = dirs::config_dir()
            .ok_or(ConfigError::PathResolutionError)?
            .join("google-drive-linux");
        Ok(config_dir.join("config.toml"))
    }

    /// Load configuration from the default system location, or initialize with defaults if not present.
    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::default_config_path()?;
        if path.exists() {
            Self::load_from_path(&path)
        } else {
            let config = Self::default();
            // Try creating parent directory and saving defaults
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = config.save_to_path(&path);
            Ok(config)
        }
    }

    /// Load configuration from a specific file path.
    pub fn load_from_path(path: &Path) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
    }

    /// Save configuration to the default system location.
    pub fn save(&self) -> Result<(), ConfigError> {
        let path = Self::default_config_path()?;
        self.save_to_path(&path)
    }

    /// Save configuration to a specific file path.
    pub fn save_to_path(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let serialized = toml::to_string_pretty(self)?;
        fs::write(path, serialized)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert!(config.auto_mount);
        assert!(!config.auto_start);
        assert_eq!(config.cache_size_mb, 5120);
        assert_eq!(config.log_level, "info");
        assert!(config
            .mount_point
            .to_string_lossy()
            .ends_with("GoogleDrive"));
    }

    #[test]
    fn test_save_and_load_config() {
        let dir = tempdir().expect("Failed to create temporary directory");
        let config_file = dir.path().join("config.toml");

        let mut original = AppConfig::default();
        original.cache_size_mb = 10240;
        original.auto_start = true;
        original.log_level = "debug".to_string();

        original
            .save_to_path(&config_file)
            .expect("Failed to save config");

        let loaded = AppConfig::load_from_path(&config_file).expect("Failed to load config");
        assert_eq!(original, loaded);
    }
}
