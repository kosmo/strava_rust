use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Strava API app credentials – baked into the binary at compile time from .env.
/// The build fails if these are missing (see build.rs).
pub const CLIENT_ID: &str = env!("STRAVA_CLIENT_ID");
pub const CLIENT_SECRET: &str = env!("STRAVA_CLIENT_SECRET");

/// Per-user OAuth tokens. Stored in config.json in the app data directory.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Config {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
}

fn config_path(data_dir: &PathBuf) -> PathBuf {
    data_dir.join("config.json")
}

/// Load tokens from config.json. Returns an empty Config if the file is missing.
pub fn load(data_dir: &PathBuf) -> Config {
    std::fs::read_to_string(config_path(data_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist tokens to config.json.
pub fn save(data_dir: &PathBuf, cfg: &Config) -> std::io::Result<()> {
    let content = serde_json::to_string_pretty(cfg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(config_path(data_dir), content)
}
