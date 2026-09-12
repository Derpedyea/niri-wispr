use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

pub const DEFAULT_MODEL: &str = "fish-audio/transcribe-1";
pub const DEFAULT_CLEANUP_MODEL: &str = "inclusionai/ling-3.0-flash";
pub const DEFAULT_HOTKEY: &str = "KEY_RIGHTCTRL";

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    api_key: Option<String>,
    model: Option<String>,
    language: Option<String>,
    /// "hold" or "toggle"
    mode: Option<String>,
    /// evdev key name, e.g. "KEY_RIGHTCTRL"
    hotkey: Option<String>,
    /// type the transcript into the focused window via uinput
    type_text: Option<bool>,
    /// play subtle start/stop audio cues
    beeps: Option<bool>,
    cleanup: Option<bool>,
    cleanup_model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub api_key: Option<String>,
    pub model: String,
    pub language: Option<String>,
    pub mode: String,
    pub hotkey: String,
    pub type_text: bool,
    pub beeps: bool,
    pub cleanup: bool,
    pub cleanup_model: String,
    pub path: PathBuf,
}

impl Config {
    /// Resolve the config path: XDG_CONFIG_HOME if present, else ~/.config.
    pub fn default_path() -> PathBuf {
        let xdg = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("dictationapp")
            .join("config.toml");
        let home = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("dictationapp")
            .join("config.toml");
        if xdg.exists() { xdg } else { home }
    }

    pub fn load() -> Result<Config> {
        // Honor XDG_CONFIG_HOME, but fall back to ~/.config so the app still
        // finds its config when launched from environments that override it.
        let path = Self::default_path();

        let file: FileConfig = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("failed to parse {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileConfig::default(),
            Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
        };

        let env_api_key = std::env::var("OPENROUTER_API_KEY").ok();
        Ok(from_file(path, file, env_api_key))
    }

    /// Write the config back to disk (called from the settings UI).
    pub fn save(&self) -> Result<()> {
        let mut s = String::new();
        s.push_str(&format!(
            "api_key = {:?}\n",
            self.api_key.clone().unwrap_or_default()
        ));
        s.push_str(&format!("model = {:?}\n", self.model));
        if let Some(l) = &self.language {
            s.push_str(&format!("language = {l:?}\n"));
        }
        s.push_str(&format!("cleanup = {}\n", self.cleanup));
        s.push_str(&format!("cleanup_model = {:?}\n", self.cleanup_model));
        s.push_str(&format!("mode = {:?}\n", self.mode));
        s.push_str(&format!("hotkey = {:?}\n", self.hotkey));
        s.push_str(&format!("type_text = {}\n", self.type_text));
        s.push_str(&format!("beeps = {}\n", self.beeps));
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&self.path, s)
            .with_context(|| format!("failed to write {}", self.path.display()))
    }
}

fn from_file(path: PathBuf, file: FileConfig, env_api_key: Option<String>) -> Config {
    let api_key = env_api_key
        .filter(|k| !k.trim().is_empty())
        .or_else(|| file.api_key.filter(|k| !k.trim().is_empty()));
    Config {
        api_key,
        model: file
            .model
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        language: file.language.filter(|l| !l.trim().is_empty()),
        mode: file.mode.unwrap_or_else(|| "hold".to_string()),
        hotkey: file
            .hotkey
            .filter(|h| !h.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_HOTKEY.to_string()),
        type_text: file.type_text.unwrap_or(true),
        beeps: file.beeps.unwrap_or(true),
        cleanup: file.cleanup.unwrap_or(true),
        cleanup_model: file
            .cleanup_model
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_CLEANUP_MODEL.to_string()),
        path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_config_resolves_cleanup_defaults() {
        let cfg = from_file(PathBuf::from("/tmp/x"), FileConfig::default(), None);
        assert!(cfg.cleanup);
        assert_eq!(cfg.cleanup_model, DEFAULT_CLEANUP_MODEL);
    }

    #[test]
    fn explicit_cleanup_settings_are_preserved() {
        let file = FileConfig {
            cleanup: Some(false),
            cleanup_model: Some("custom/model".into()),
            ..Default::default()
        };
        let cfg = from_file(PathBuf::from("/tmp/x"), file, None);
        assert!(!cfg.cleanup);
        assert_eq!(cfg.cleanup_model, "custom/model");
    }
}
