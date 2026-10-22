//! Settings window, so config.toml never needs editing. Every change saves
//! immediately and hot-reloads the running app. The window (`ui/src/settings.tsx`)
//! only draws `View`s and reports `Event`s; this model owns the config.

use crate::audio;
use crate::config::Config;
use crate::hotkey;
use crate::ipc::Command;
use crate::{niri, tray, ui};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant, SystemTime};

/// Speech model presets as (label, OpenRouter slug); anything else is Custom.
const MODELS: &[(&str, &str)] = &[
    ("Fish Audio", "fish-audio/transcribe-1"),
    ("Whisper v3", "openai/whisper-large-v3"),
    ("Whisper", "openai/whisper-1"),
];

/// How long "Press a key…" waits before giving up.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often a pending capture checks the window still has focus.
const FOCUS_CHECK: Duration = Duration::from_millis(300);
/// How often config.toml is checked for outside edits.
const FILE_CHECK: Duration = Duration::from_secs(1);
/// How often microphones are re-listed while the window is open.
const MIC_CHECK: Duration = Duration::from_secs(2);

const KEYS_URL: &str = "https://openrouter.ai/keys";
const TITLE: &str = "Dictation Settings";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Field {
    ApiKey,
    Model,
    Language,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Flag {
    TypeText,
    Beeps,
    Cleanup,
}

/// What the window reports.
#[derive(Debug, Deserialize)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum Event {
    Set {
        field: Field,
        value: String,
    },
    Mode {
        value: String,
    },
    Mic {
        value: Option<String>,
    },
    Preset {
        slug: String,
    },
    Custom,
    Toggle {
        flag: Flag,
    },
    Capture,
    CancelCapture,
    /// A text field took focus.
    Editing,
    OpenKeys,
}

/// What the window draws.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct View<'a> {
    hint: String,
    key_label: String,
    capturing: bool,
    key_note: Option<&'static str>,
    key_note_error: bool,
    mode: &'a str,
    mics: Vec<MicRow>,
    env_key: bool,
    api_key: &'a str,
    models: Vec<Preset>,
    model: &'a str,
    custom_model: bool,
    language: &'a str,
    cleanup: bool,
    type_text: bool,
    beeps: bool,
    banner: Option<String>,
}

#[derive(Serialize)]
struct Preset {
    label: &'static str,
    slug: &'static str,
}

/// A pending "press a key" request; dropping it withdraws the request, so
/// closing the window or clicking away can't leave the next key captured.
struct Capture {
    key: hotkey::KeyCapture,
    started: Instant,
}

pub struct Settings {
    tx: Sender<Command>,
    /// The editable config — file values only (see `Config::load_file`).
    cfg: Config,
    /// OPENROUTER_API_KEY overrides the file's key; shown read-only.
    env_key: bool,
    custom_model: bool,
    mics: audio::InputDevices,
    capture: Option<Capture>,
    /// Why the last capture didn't change the key.
    capture_note: Option<&'static str>,
    /// config.toml stopped reading: saving waits until it reads again rather
    /// than replace it with values from before.
    load_error: Option<String>,
    save_error: Option<String>,
    /// config.toml's mtime as of the last read or save here.
    seen: Option<SystemTime>,
}

fn modified(cfg: &Config) -> Option<SystemTime> {
    std::fs::metadata(&cfg.path).and_then(|m| m.modified()).ok()
}

impl Settings {
    fn new(cfg: Config, tx: Sender<Command>, mics: audio::InputDevices) -> Self {
        Self {
            tx,
            env_key: std::env::var("OPENROUTER_API_KEY").is_ok_and(|k| !k.trim().is_empty()),
            custom_model: !MODELS.iter().any(|(_, slug)| *slug == cfg.model),
            mics,
            seen: modified(&cfg),
            cfg,
            capture: None,
            capture_note: None,
            load_error: None,
            save_error: None,
        }
    }

    /// Catch up with config.toml edits made elsewhere (an editor, `--reload`),
    /// which the next change here would otherwise overwrite.
    fn refresh(&mut self) {
        self.seen = modified(&self.cfg);
        match self.cfg.reread() {
            Ok(cfg) => {
                self.custom_model = !MODELS.iter().any(|(_, slug)| *slug == cfg.model);
                self.cfg = cfg;
                self.load_error = None;
            }
            Err(e) => self.load_error = Some(format!("{e:#}")),
        }
    }

    /// Refresh if config.toml changed since it was last read or saved here.
    fn refresh_if_edited(&mut self) {
        if modified(&self.cfg) != self.seen {
            self.refresh();
        }
    }

    /// Write config.toml and have the app reload it.
    fn persist(&mut self) {
        if self.load_error.is_some() {
            return;
        }
        // Trim only what's written: the fields keep exactly what was typed.
        let trimmed = |v: &Option<String>| {
            v.as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(String::from)
        };
        let mut cfg = self.cfg.clone();
        cfg.api_key = trimmed(&self.cfg.api_key);
        cfg.language = trimmed(&self.cfg.language);
        cfg.model = match self.cfg.model.trim() {
            "" => crate::config::DEFAULT_MODEL.to_string(),
            model => model.to_string(),
        };
        self.save_error = match cfg.save() {
            Ok(()) => self
                .tx
                .send(Command::Reload)
                .err()
                .map(|e| format!("Saved, but the app didn't reload: {e}")),
            Err(e) => Some(format!("Couldn't save: {e:#}")),
        };
        self.seen = modified(&self.cfg);
    }

    fn text(&self, f: Field) -> &str {
        match f {
            Field::ApiKey => self.cfg.api_key.as_deref().unwrap_or(""),
            Field::Model => &self.cfg.model,
            Field::Language => self.cfg.language.as_deref().unwrap_or(""),
        }
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Set { field, value } => {
                // The env key is read-only, and the model field only exists as Custom.
                if (field == Field::ApiKey && self.env_key)
                    || (field == Field::Model && !self.custom_model)
                {
                    return;
                }
                let value: String = value.chars().filter(|c| !c.is_control()).collect();
                match field {
                    Field::ApiKey => self.cfg.api_key = Some(value),
                    Field::Model => self.cfg.model = value,
                    Field::Language => self.cfg.language = Some(value),
                }
                self.persist();
            }
            Event::Mode { value } if value == "hold" || value == "toggle" => {
                self.cfg.mode = value;
                self.persist();
            }
            Event::Mode { .. } => {}
            Event::Mic { value } => {
                self.cfg.mic = value;
                self.persist();
            }
            Event::Preset { slug } => {
                if let Some((_, slug)) = MODELS.iter().find(|(_, s)| *s == slug) {
                    self.custom_model = false;
                    self.cfg.model = slug.to_string();
                    self.persist();
                }
            }
            Event::Custom => {
                self.cancel_capture();
                self.custom_model = true;
            }
            Event::Toggle { flag } => self.set_flag(flag),
            Event::Capture => self.start_capture(),
            // Focusing a text field ends a capture, or it would take the keys
            // typed into the field.
            Event::CancelCapture | Event::Editing => self.cancel_capture(),
            Event::OpenKeys => open_url(KEYS_URL),
        }
    }

    fn start_capture(&mut self) {
        self.capture_note = None;
        self.capture = Some(Capture {
            key: hotkey::KeyCapture::start(),
            started: Instant::now(),
        });
    }

    /// Settle a pending capture: the pressed key, a timeout, or nothing yet.
    fn poll_capture(&mut self) {
        let Some(capture) = &self.capture else {
            return;
        };
        let key = match capture.key.try_recv() {
            Ok(key) => key,
            Err(TryRecvError::Empty) if capture.started.elapsed() < CAPTURE_TIMEOUT => return,
            Err(TryRecvError::Empty) => {
                self.cancel_capture();
                self.capture_note = Some("No key detected. Is /dev/input readable?");
                return;
            }
            Err(TryRecvError::Disconnected) => {
                self.capture = None;
                return;
            }
        };
        self.capture = None;
        let name = format!("{key:?}");
        self.capture_note = if hotkey::is_typing_key(key) {
            Some("That key types text — pick one like Right Ctrl or F13.")
        } else if hotkey::parse_key(&name).is_err() {
            Some("That key has no name. Try another.")
        } else {
            self.cfg.hotkey = name;
            self.persist();
            None
        };
    }

    fn cancel_capture(&mut self) {
        self.capture = None;
    }

    fn set_flag(&mut self, flag: Flag) {
        let slot = match flag {
            Flag::TypeText => &mut self.cfg.type_text,
            Flag::Beeps => &mut self.cfg.beeps,
            Flag::Cleanup => &mut self.cfg.cleanup,
        };
        *slot = !*slot;
        self.persist();
    }

    /// An unreadable config.toml or a failed save, until it's resolved.
    fn banner(&self) -> Option<String> {
        match (&self.load_error, &self.save_error) {
            (Some(e), _) => Some(format!(
                "Can't read config.toml, so changes aren't saved. Fix it, then reopen Settings.\n{e}"
            )),
            (None, Some(e)) => Some(e.clone()),
            (None, None) => None,
        }
    }

    fn view(&self) -> View<'_> {
        View {
            hint: tray::hint(&self.cfg.hotkey, &self.cfg.mode),
            key_label: hotkey::key_label(&self.cfg.hotkey),
            capturing: self.capture.is_some(),
            key_note: self.capture_note,
            key_note_error: self.capture_note.is_some(),
            mode: if self.cfg.mode == "toggle" {
                "toggle"
            } else {
                "hold"
            },
            mics: mic_rows(self.cfg.mic.as_deref(), &self.mics),
            env_key: self.env_key,
            api_key: self.text(Field::ApiKey),
            models: MODELS
                .iter()
                .map(|&(label, slug)| Preset { label, slug })
                .collect(),
            model: &self.cfg.model,
            custom_model: self.custom_model,
            language: self.text(Field::Language),
            cleanup: self.cfg.cleanup,
            type_text: self.cfg.type_text,
            beeps: self.cfg.beeps,
            banner: self.banner(),
        }
    }
}

fn open_url(url: &'static str) {
    std::thread::spawn(move || {
        if let Err(e) = std::process::Command::new("xdg-open").arg(url).status() {
            eprintln!("settings: could not open {url}: {e}");
        }
    });
}

/// The open settings window and its model.
pub struct Session {
    model: Settings,
    window: ui::Window,
    events: Receiver<Event>,
    mics: Receiver<audio::InputDevices>,
    next_file_check: Instant,
    next_focus_check: Instant,
}

impl Session {
    /// `cfg` holds file values only (see `Config::load_file`).
    pub fn open(cfg: Config, tx: Sender<Command>) -> Result<Session> {
        let (event_tx, events) = channel();
        let wake = tx.clone();
        let window = ui::Window::spawn("settings", move |event| {
            match serde_json::from_value::<Event>(event) {
                Ok(event) => {
                    if event_tx.send(event).is_ok() {
                        let _ = wake.send(Command::Wake);
                    }
                }
                Err(e) => eprintln!("settings: bad event: {e}"),
            }
        })?;
        // Listing can be slow, so it runs beside the command loop; it ends
        // with the session, when its receiver is dropped.
        let (mic_tx, mics) = channel();
        let wake = tx.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(MIC_CHECK);
                if mic_tx.send(audio::input_devices()).is_err() || wake.send(Command::Wake).is_err()
                {
                    return;
                }
            }
        });
        let model = Settings::new(cfg, tx, audio::input_devices());
        let mut session = Session {
            model,
            window,
            events,
            mics,
            next_file_check: Instant::now() + FILE_CHECK,
            next_focus_check: Instant::now(),
        };
        session.window.show(&session.model.view());
        Ok(session)
    }

    /// `--settings` or the tray while open: fresh values, window forward.
    pub fn reopen(&mut self) {
        self.model.refresh();
        niri::focus_window(self.window.pid(), TITLE);
    }

    /// Handle what the window sent and keep it current. False once it closed.
    pub fn tick(&mut self) -> bool {
        // Before the exit check: a change made just before closing still saves.
        while let Ok(event) = self.events.try_recv() {
            self.model.apply(event);
        }
        if !self.window.is_open() {
            return false;
        }
        while let Ok(mics) = self.mics.try_recv() {
            self.model.mics = mics;
        }
        self.model.poll_capture();
        let now = Instant::now();
        // Clicking away abandons a pending capture, so the next key typed
        // elsewhere can't become the hotkey.
        if self.model.capture.is_some() && now >= self.next_focus_check {
            self.next_focus_check = now + FOCUS_CHECK;
            if niri::focused_pid().is_some_and(|pid| pid != self.window.pid()) {
                self.model.cancel_capture();
            }
        }
        if now >= self.next_file_check {
            self.next_file_check = now + FILE_CHECK;
            self.model.refresh_if_edited();
        }
        self.window.show(&self.model.view());
        true
    }
}

#[derive(Serialize)]
struct MicRow {
    value: Option<String>,
    label: String,
    detail: String,
    selected: bool,
}

/// Picker rows: System default, each sound card, and the configured mic when
/// it isn't one of them. The setting resolves the way recording does, so a
/// substring like "USB" selects the device it actually opens.
fn mic_rows(mic: Option<&str>, mics: &audio::InputDevices) -> Vec<MicRow> {
    // (row name, whether a device by that name exists)
    let chosen = mic.map(|m| match audio::match_mic(m, &mics.all) {
        Some(index) => (mics.all[index].as_str(), true),
        None => (m, false),
    });
    let mut names: Vec<&str> = mics.cards.iter().map(String::as_str).collect();
    if let Some((name, _)) = chosen
        && !names.contains(&name)
    {
        names.push(name);
    }
    let mut rows = vec![MicRow {
        value: None,
        label: "System default".into(),
        detail: String::new(),
        selected: chosen.is_none(),
    }];
    for name in names {
        // ALSA names read "Card, Input": lead with the card.
        let (card, input) = name.split_once(", ").unwrap_or((name, ""));
        let missing = chosen == Some((name, false));
        rows.push(MicRow {
            value: Some(name.to_string()),
            label: card.to_string(),
            detail: if missing { "Not connected" } else { input }.to_string(),
            selected: chosen.is_some_and(|(chosen, _)| chosen == name),
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use evdev::KeyCode;
    use std::path::PathBuf;

    fn no_mics() -> audio::InputDevices {
        audio::InputDevices {
            all: Vec::new(),
            cards: Vec::new(),
        }
    }

    fn config_at(path: PathBuf) -> Config {
        Config {
            api_key: None,
            model: crate::config::DEFAULT_MODEL.into(),
            language: None,
            mode: "hold".into(),
            hotkey: "KEY_RIGHTCTRL".into(),
            mic: None,
            type_text: true,
            beeps: true,
            cleanup: true,
            cleanup_model: crate::config::DEFAULT_CLEANUP_MODEL.into(),
            path,
        }
    }

    fn event(json: &str) -> Event {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn configured_mic_selects_the_device_recording_opens() {
        let mics = audio::InputDevices {
            all: [
                "Default Audio Device",
                "USB Microphone, USB Audio",
                "PulseAudio Sound Server",
            ]
            .map(String::from)
            .into(),
            cards: vec!["USB Microphone, USB Audio".into()],
        };
        let rows = |mic: Option<&str>| {
            mic_rows(mic, &mics)
                .into_iter()
                .map(|row| (row.label, row.detail, row.selected))
                .collect::<Vec<_>>()
        };
        let row = |label: &str, detail: &str, selected| (label.into(), detail.into(), selected);
        // A substring picks the card it matches: no phantom "USB" row.
        assert_eq!(
            rows(Some("usb")),
            [
                row("System default", "", false),
                row("USB Microphone", "USB Audio", true)
            ]
        );
        // A sound-server device isn't a card but works: listed, not "Not connected".
        assert_eq!(
            rows(Some("pulse"))[2],
            row("PulseAudio Sound Server", "", true)
        );
        assert_eq!(
            rows(Some("Studio Mic"))[2],
            row("Studio Mic", "Not connected", true)
        );
        assert!(rows(None)[0].2);
    }

    /// Edits made elsewhere while Settings sits open survive the next change
    /// here; a file that stops parsing is left alone rather than replaced.
    #[test]
    fn outside_edits_survive_the_next_change() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_at(dir.path().join("config.toml"));
        cfg.save().unwrap();
        let (tx, _reloads) = channel();
        let mut settings = Settings::new(cfg.clone(), tx, no_mics());

        Config {
            model: "custom/model".into(),
            ..cfg.clone()
        }
        .save()
        .unwrap();
        // Coarse filesystem clocks can't hide the edit from the mtime check.
        settings.seen = None;
        settings.refresh_if_edited();
        settings.apply(event(r#"{"event":"toggle","flag":"beeps"}"#));
        let saved = cfg.reread().unwrap();
        assert_eq!((saved.model.as_str(), saved.beeps), ("custom/model", false));
        assert!(settings.custom_model);

        std::fs::write(&cfg.path, "beeps = [").unwrap();
        settings.seen = None;
        settings.refresh_if_edited();
        settings.apply(event(r#"{"event":"toggle","flag":"beeps"}"#));
        assert_eq!(std::fs::read_to_string(&cfg.path).unwrap(), "beeps = [");
        assert!(settings.banner().is_some());
    }

    /// Choosing Custom keeps the preset until a model is typed, and isn't
    /// undone by the app's own save being noticed as an outside edit.
    #[test]
    fn custom_model_survives_its_own_saves() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_at(dir.path().join("config.toml"));
        cfg.save().unwrap();
        let (tx, reloads) = channel();
        let mut settings = Settings::new(cfg.clone(), tx, no_mics());
        settings.apply(event(
            r#"{"event":"set","field":"model","value":"ignored"}"#,
        ));
        assert!(reloads.try_recv().is_err(), "model edits need Custom first");

        settings.apply(event(r#"{"event":"custom"}"#));
        settings.apply(event(r#"{"event":"toggle","flag":"cleanup"}"#));
        settings.refresh_if_edited();
        assert!(settings.custom_model);
        settings.apply(event(
            r#"{"event":"set","field":"model","value":" me/model "}"#,
        ));
        assert_eq!(cfg.reread().unwrap().model, "me/model");
        assert_eq!(settings.view().model, " me/model ");

        settings.apply(event(r#"{"event":"preset","slug":"openai/whisper-1"}"#));
        assert!(!settings.custom_model);
        assert_eq!(cfg.reread().unwrap().model, "openai/whisper-1");
        settings.apply(event(r#"{"event":"preset","slug":"not/a-preset"}"#));
        assert_eq!(cfg.reread().unwrap().model, "openai/whisper-1");
    }

    /// A pressed key becomes the hotkey (saved and reloaded at once) unless it
    /// types text; silence ends the wait.
    #[test]
    fn captured_key_is_saved_unless_it_types() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let (tx, reloads) = channel();
        let mut settings = Settings::new(config_at(path.clone()), tx, no_mics());
        // Poll once with `key` pressed (or none) after `waited`.
        let mut poll = |key: Option<KeyCode>, waited: Duration| {
            let (press, key_capture) = hotkey::KeyCapture::detached();
            if let Some(key) = key {
                press.send(key).unwrap();
            }
            settings.capture = Some(Capture {
                key: key_capture,
                started: Instant::now() - waited,
            });
            settings.poll_capture();
            assert!(settings.capture.is_none(), "capture should be over");
            (settings.cfg.hotkey.clone(), settings.capture_note)
        };

        let (hotkey, note) = poll(Some(KeyCode::KEY_A), Duration::ZERO);
        assert_eq!(hotkey, "KEY_RIGHTCTRL");
        assert!(note.is_some_and(|n| n.contains("types text")));
        assert!(!path.exists() && reloads.try_recv().is_err());

        let (_, note) = poll(None, CAPTURE_TIMEOUT);
        assert!(note.is_some_and(|n| n.contains("No key")));
        assert!(!path.exists() && reloads.try_recv().is_err());

        let (hotkey, note) = poll(Some(KeyCode::KEY_F13), Duration::ZERO);
        assert_eq!((hotkey.as_str(), note), ("KEY_F13", None));
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains(r#"hotkey = "KEY_F13""#), "{saved}");
        assert_eq!(reloads.try_recv(), Ok(Command::Reload));

        // Focusing a field ends a capture, so it can't eat what's typed there.
        settings.capture = Some(Capture {
            key: hotkey::KeyCapture::detached().1,
            started: Instant::now(),
        });
        settings.apply(event(r#"{"event":"editing"}"#));
        assert!(settings.capture.is_none());
    }
}
