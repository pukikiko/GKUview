use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::endpoints::Endpoints;

pub const DEFAULT_HOST: &str = "192.168.0.1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub host: String,
    pub endpoints: Endpoints,
    pub output_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_string(),
            endpoints: Endpoints::default(),
            output_dir: None,
        }
    }
}

impl Config {
    pub fn default_path() -> PathBuf {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                return PathBuf::from(xdg).join("gkuview").join("config.json");
            }
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join(".config")
            .join("gkuview")
            .join("config.json")
    }

    pub fn load(path: Option<&Path>) -> Self {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => Self::default_path(),
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: Option<&Path>) -> crate::Result<()> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => Self::default_path(),
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn output_dir(&self) -> PathBuf {
        self.output_dir.clone().unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join("GKUview")
        })
    }

    pub fn cache_dir() -> PathBuf {
        if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
            if !xdg.is_empty() {
                return PathBuf::from(xdg).join("gkuview");
            }
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".cache").join("gkuview")
    }
}

/// Appends a line to `~/.cache/gkuview/gkuview.log` so future runs leave a
/// diagnostic trail.
pub fn log_line(message: &str) {
    use std::io::Write;
    let dir = Config::cache_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("gkuview.log"))
    {
        let _ = writeln!(file, "[{stamp}] {message}");
    }
}
