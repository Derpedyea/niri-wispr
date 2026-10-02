use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, Permissions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
    /// input device name to record from; unset = system default
    mic: Option<String>,
    /// type the transcript into the focused window via Wayland's virtual keyboard
    type_text: Option<bool>,
    /// play subtle start/stop audio cues
    beeps: Option<bool>,
    cleanup: Option<bool>,
    cleanup_model: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Config {
    pub api_key: Option<String>,
    pub model: String,
    pub language: Option<String>,
    pub mode: String,
    pub hotkey: String,
    pub mic: Option<String>,
    pub type_text: bool,
    pub beeps: bool,
    pub cleanup: bool,
    pub cleanup_model: String,
    #[serde(skip)]
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
        let text = toml::to_string(self).context("failed to serialize settings")?;
        replace_config(&self.path, |file| file.write_all(text.as_bytes()))
    }
}

const TEMP_PREFIX: &str = ".dictationapp-config-";
const TEMP_SUFFIX: &str = ".tmp";
const TEMP_RANDOM_LEN: usize = 6;
static CONFIG_SAVE_LOCK: Mutex<()> = Mutex::new(());

fn remove_temp(path: &Path) -> Result<()> {
    let mut retries = 0;
    loop {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) if retries < 2 => retries += 1,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to remove temporary config {}", path.display())
                });
            }
        }
    }
}

fn close_temp(temp: tempfile::NamedTempFile) -> Result<()> {
    let path = temp.path().to_path_buf();
    match temp.close() {
        Ok(()) => Ok(()),
        Err(error) => remove_temp(&path).with_context(|| {
            format!(
                "initial temporary config cleanup failed for {}: {error}",
                path.display()
            )
        }),
    }
}

fn cleanup_interrupted_saves(parent: &Path) -> Result<()> {
    let uid = std::fs::metadata("/proc/self")
        .context("failed to identify the config owner")?
        .uid();
    for entry in std::fs::read_dir(parent).context("failed to inspect interrupted config saves")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(name) = name
            .strip_prefix(TEMP_PREFIX)
            .and_then(|name| name.strip_suffix(TEMP_SUFFIX))
        else {
            continue;
        };
        let Some((pid, random)) = name.split_once('-') else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if pid == 0
            || random.len() != TEMP_RANDOM_LEN
            || !random.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            continue;
        }
        let path = entry.path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("failed to inspect a temporary config"),
        };
        if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o777 != 0o600 {
            continue;
        }
        // Saves are serialized in this process, so our own retained staging
        // files can also be retried after a previous cleanup failed.
        if pid == std::process::id()
            || !Path::new(&format!("/proc/{pid}"))
                .try_exists()
                .context("failed to check an interrupted config save's owner")?
        {
            remove_temp(&path)?;
        }
    }
    Ok(())
}

/// The write callback makes partial I/O failures testable without filling a disk.
fn replace_config(path: &Path, write: impl FnOnce(&mut File) -> std::io::Result<()>) -> Result<()> {
    let _guard = CONFIG_SAVE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("config save lock was poisoned"))?;
    // Preserve a user's dotfile symlink: replace its target, not the link itself.
    let resolved;
    let path = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            resolved =
                std::fs::canonicalize(path).context("failed to resolve the config symlink")?;
            resolved.as_path()
        }
        Ok(_) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path,
        Err(error) => return Err(error).context("failed to inspect the config destination"),
    };
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    cleanup_interrupted_saves(parent)?;
    let directory = File::open(parent).context("failed to open the config directory")?;
    let prefix = format!("{TEMP_PREFIX}{}-", std::process::id());
    let mut temp = tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(TEMP_SUFFIX)
        .rand_bytes(TEMP_RANDOM_LEN)
        .tempfile_in(parent)
        .context("failed to create a temporary config")?;
    let prepared = (|| -> Result<()> {
        temp.as_file()
            .set_permissions(Permissions::from_mode(0o600))?;
        write(temp.as_file_mut()).context("failed to write the temporary config")?;
        temp.as_file()
            .sync_all()
            .context("failed to sync the temporary config")?;
        Ok(())
    })();
    if let Err(error) = prepared {
        close_temp(temp).with_context(|| format!("config save failed: {error:#}"))?;
        return Err(error);
    }
    match temp.persist(path) {
        Ok(_) => directory
            .sync_all()
            .context("config replaced, but its directory could not be synced"),
        Err(error) => {
            let tempfile::PersistError { error, file } = error;
            close_temp(file).with_context(|| format!("config replacement failed: {error}"))?;
            Err(error).with_context(|| format!("failed to replace {}", path.display()))
        }
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
        mic: file.mic.filter(|m| !m.trim().is_empty()),
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

    #[test]
    fn mic_defaults_to_none_and_blank_is_ignored() {
        assert!(
            from_file(PathBuf::from("/tmp/x"), FileConfig::default(), None)
                .mic
                .is_none()
        );
        let file = FileConfig {
            mic: Some("   ".into()),
            ..Default::default()
        };
        assert!(from_file(PathBuf::from("/tmp/x"), file, None).mic.is_none());
        let file = FileConfig {
            mic: Some("USB Mic".into()),
            ..Default::default()
        };
        assert_eq!(
            from_file(PathBuf::from("/tmp/x"), file, None)
                .mic
                .as_deref(),
            Some("USB Mic")
        );
    }

    #[test]
    fn saved_unicode_and_escaped_values_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = from_file(path.clone(), FileConfig::default(), None);
        config.api_key = Some("quote\" slash\\ nul\0 tab\t".into());
        config.mic = Some("USB\u{a0}Microphone\u{200b}".into());
        config.save().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let file: FileConfig = toml::from_str(&text).unwrap();
        assert_eq!(file.api_key, config.api_key);
        assert_eq!(file.mic, config.mic);
        assert_eq!(file.model.as_deref(), Some(DEFAULT_MODEL));
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn partial_write_failure_preserves_existing_config() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = "model = \"previous/model\"\n";
        std::fs::write(&path, original).unwrap();

        let result = replace_config(&path, |file| {
            file.write_all(b"incomplete")?;
            Err(std::io::Error::other("simulated disk full"))
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn rename_failure_cleans_up_the_staged_config() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::create_dir(&path).unwrap();

        assert!(replace_config(&path, |file| file.write_all(b"complete")).is_err());
        assert!(path.is_dir());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn config_symlink_is_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("actual.toml");
        let path = directory.path().join("config.toml");
        std::fs::write(&target, "old").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();

        replace_config(&path, |file| file.write_all(b"new")).unwrap();
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }

    #[test]
    fn interrupted_save_preserves_config_and_is_reclaimed_on_retry() {
        const CRASH_PATH: &str = "DICTATIONAPP_CONFIG_TEST_CRASH_PATH";
        if let Some(path) = std::env::var_os(CRASH_PATH) {
            replace_config(Path::new(&path), |file| {
                file.write_all(b"interrupted")?;
                // Simulate process death while the staging file is incomplete.
                std::process::exit(77);
            })
            .unwrap();
            unreachable!();
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = "model = \"previous/model\"\n";
        std::fs::write(&path, original).unwrap();
        let active = directory
            .path()
            .join(format!("{TEMP_PREFIX}1-ABC123{TEMP_SUFFIX}"));
        std::fs::write(&active, "another live process's staging file").unwrap();
        std::fs::set_permissions(&active, Permissions::from_mode(0o600)).unwrap();

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "config::tests::interrupted_save_preserves_config_and_is_reclaimed_on_retry",
            ])
            .env(CRASH_PATH, &path)
            .output()
            .unwrap()
            .status;
        assert_eq!(status.code(), Some(77));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 3);

        replace_config(&path, |file| file.write_all(b"model = \"next/model\"\n")).unwrap();
        let file: FileConfig = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(file.model.as_deref(), Some("next/model"));
        assert!(active.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }
}
