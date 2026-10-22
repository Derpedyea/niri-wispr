use crate::audio::Recording;
use crate::config::Config;
use crate::hotkey::Watcher;
use crate::ipc::Command;
use crate::tray::{self, Tray};
use crate::typer::Typer;
use crate::{api, audio, beep, clipboard, hotkey, settings, ui};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub enum Status {
    Idle,
    /// Starting failed, but `error` waits like a recording's cue — see
    /// `cue_if_due` — so a tap or shortcut the hotkey cancels stays silent.
    StartFailed {
        started: Instant,
        error: &'static str,
    },
    /// `cued` once the beep played and the pill appeared — see `cue_if_due`.
    Recording {
        started: Instant,
        cued: bool,
    },
    Transcribing,
    Cleaning,
    Copying,
    Typing,
}

impl Status {
    fn is_active(&self) -> bool {
        !matches!(self, Status::Idle)
    }

    fn is_delivering(&self) -> bool {
        matches!(
            self,
            Status::Transcribing | Status::Cleaning | Status::Copying | Status::Typing
        )
    }
}

const BAR_COUNT: usize = 21;
const ERROR_DURATION: Duration = Duration::from_secs(4);
const NOTICE_DURATION: Duration = Duration::from_millis(2500);
/// How often capture, cues, the waveform and messages are checked.
const TICK: Duration = Duration::from_millis(40);

fn normalize_level(level: f32) -> f32 {
    ((level - 0.02) / 0.55).clamp(0.0, 1.0).powf(0.7)
}

fn push_waveform(history: &mut [f32; BAR_COUNT], smoothed: &mut f32, level: f32) {
    let target = normalize_level(level);
    *smoothed = if target > *smoothed {
        target
    } else {
        *smoothed * 0.78 + target * 0.22
    };
    history.rotate_left(1);
    history[BAR_COUNT - 1] = *smoothed;
}

/// Results of work done off the command loop. Each is followed by a
/// `Command::Wake` so the loop handles it at once.
enum Internal {
    Transcribed {
        result: anyhow::Result<String>,
        cleanup: Option<Cleanup>,
    },
    /// The text to deliver, cleaned up or not.
    Text(String),
    Copied {
        result: anyhow::Result<()>,
        text: String,
    },
    Typed {
        outcome: anyhow::Result<()>,
        cancelled: Arc<AtomicBool>,
        chars: usize,
    },
}

/// Cleanup settings as they were when the recording stopped.
struct Cleanup {
    api_key: String,
    model: String,
    language: Option<String>,
}

/// What the pill window draws; see `ui/src/pill.tsx`.
#[derive(Serialize)]
struct PillView {
    dot: String,
    kind: &'static str,
    bars: Vec<f32>,
    label: String,
    message: String,
}

pub struct Dictation {
    /// The pill's window process, while one is wanted.
    pill: Option<ui::Window>,
    /// The pill process failed or quit while wanted; retried once idle.
    pill_failed: bool,
    settings: Option<settings::Session>,
    config: Config,
    status: Status,
    recording: Option<Recording>,
    error: Option<String>,
    notice: Option<String>,
    rx: Receiver<Command>,
    tx: Sender<Command>,
    internal_rx: Receiver<Internal>,
    internal_tx: Sender<Internal>,
    typer: Option<Arc<Mutex<Typer>>>,
    watcher: Option<Watcher>,
    /// Shared with the delivery child; settings changes and every shutdown stop it.
    typing_cancel: Option<Arc<AtomicBool>>,
    copy_cancel: Option<Arc<AtomicBool>>,
    /// A start (hold press, `--start`, or toggle) that arrived while the last
    /// transcript was still being delivered. It records once idle, unless its
    /// gesture ends first — dropping it would lose speech; replaying it alone
    /// would outlive the release that already went by.
    start_pending: bool,
    shutting_down: bool,
    tray: Option<Tray>,
    /// What the tray icon currently shows, so it's only updated on change.
    tray_recording: bool,
    waveform: [f32; BAR_COUNT],
    smoothed_level: f32,
    message_expires_at: Option<Instant>,
}

struct Delivery {
    typer: Arc<Mutex<Typer>>,
    cancelled: Arc<AtomicBool>,
}

impl Dictation {
    pub fn new(
        config: Config,
        rx: Receiver<Command>,
        tx: Sender<Command>,
        typer: Option<Typer>,
        watcher: Option<Watcher>,
        tray: Option<Tray>,
    ) -> Self {
        let (internal_tx, internal_rx) = channel();
        Self {
            pill: None,
            pill_failed: false,
            settings: None,
            config,
            status: Status::Idle,
            recording: None,
            error: None,
            notice: None,
            rx,
            tx,
            internal_rx,
            internal_tx,
            typer: typer.map(|t| Arc::new(Mutex::new(t))),
            watcher,
            typing_cancel: None,
            copy_cancel: None,
            start_pending: false,
            shutting_down: false,
            tray,
            tray_recording: false,
            waveform: [0.0; BAR_COUNT],
            smoothed_level: 0.0,
            message_expires_at: None,
        }
    }

    /// Handle commands until `Quit`.
    pub fn run(mut self) {
        let mut next_tick = Instant::now();
        while !self.shutting_down {
            match self
                .rx
                .recv_timeout(next_tick.saturating_duration_since(Instant::now()))
            {
                Ok(command) => self.handle_command(command),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.drain_commands();
            // Settings first: a change saved there queues the Reload that a
            // finishing delivery drains before choosing typing or clipboard.
            if let Some(session) = &mut self.settings
                && !session.tick()
            {
                self.settings = None;
            }
            while !self.shutting_down
                && let Ok(event) = self.internal_rx.try_recv()
            {
                self.handle_internal(event);
            }
            if Instant::now() >= next_tick {
                self.tick();
                next_tick = Instant::now() + TICK;
            }
            self.sync_pill();
        }
    }

    fn tick(&mut self) {
        self.fail_recording_if_needed();
        self.cue_if_due();
        self.sync_tray();
        if let Some(level) = self.recording.as_ref().map(|rec| rec.capture.level()) {
            push_waveform(&mut self.waveform, &mut self.smoothed_level, level);
        }
        if let Some(deadline) = self.message_expires_at
            && Instant::now() >= deadline
        {
            self.clear_message();
        }
    }

    /// Run `work` on its own thread and hand its result back to the loop.
    fn spawn_work(&self, work: impl FnOnce() -> Internal + Send + 'static) {
        let internal = self.internal_tx.clone();
        let wake = self.tx.clone();
        std::thread::spawn(move || {
            if internal.send(work()).is_ok() {
                let _ = wake.send(Command::Wake);
            }
        });
    }

    fn handle_command(&mut self, cmd: Command) {
        if self.shutting_down || cmd == Command::Wake {
            return;
        }
        eprintln!("handling {cmd:?}");
        match cmd {
            Command::Hotkey { generation, action } => {
                // Stop/join cannot retract commands already queued by an old listener.
                if self.watcher.as_ref().map(Watcher::generation) == Some(generation) {
                    self.handle_command(action.into());
                }
            }
            Command::Toggle => match self.status {
                Status::Recording { .. } => self.stop_and_transcribe(),
                Status::Idle => self.start_recording(),
                Status::StartFailed { error, .. } => self.show_start_failure(error),
                Status::Transcribing | Status::Cleaning | Status::Copying | Status::Typing => {
                    self.start_pending = !self.start_pending;
                }
            },
            Command::Start => match self.status {
                Status::Idle => self.start_recording(),
                _ if self.status.is_delivering() => self.start_pending = true,
                _ => {}
            },
            Command::Stop => match self.status {
                Status::Recording { .. } => self.stop_and_transcribe(),
                Status::StartFailed { error, .. } => self.show_start_failure(error),
                _ => self.start_pending = false,
            },
            Command::Cancel => {
                self.cancel_recording();
            }
            Command::Quit => self.prepare_quit(),
            Command::Settings => self.open_settings(),
            Command::Reload => self.reload_config(),
            Command::Wake => {}
        }
    }

    /// The tray mic goes red with the pill: only once a recording is cued, so
    /// taps and shortcuts stay invisible there too.
    fn sync_tray(&mut self) {
        let recording = matches!(self.status, Status::Recording { cued: true, .. });
        if recording != self.tray_recording {
            self.tray_recording = recording;
            if let Some(tray) = &self.tray {
                tray.set_recording(recording);
            }
        }
    }

    fn drain_commands(&mut self) {
        while let Ok(command) = self.rx.try_recv() {
            self.handle_command(command);
        }
    }

    /// Open Settings, or bring the open window forward with fresh values.
    fn open_settings(&mut self) {
        if let Some(session) = &mut self.settings {
            session.reopen();
            return;
        }
        // A failed read must not become an editable default that a save would
        // persist. File values only: saving never copies OPENROUTER_API_KEY in.
        let config = match Config::load_file() {
            Ok(config) => config,
            Err(error) => {
                eprintln!("settings: {error:#}");
                self.show_error("Unable to load settings. Check config.toml.");
                return;
            }
        };
        match settings::Session::open(config, self.tx.clone()) {
            Ok(session) => self.settings = Some(session),
            Err(error) => {
                eprintln!("settings: {error:#}");
                self.show_error("Unable to open settings.");
            }
        }
    }

    /// Apply output changes before any pending transcript can choose a delivery target.
    fn reload_config(&mut self) {
        let Ok(new) = Config::load() else {
            self.show_error("Unable to reload settings. Check config.toml.");
            return;
        };
        self.apply_config(new, Typer::new);
    }

    fn apply_config(&mut self, new: Config, create_typer: impl FnOnce() -> anyhow::Result<Typer>) {
        self.apply_config_with(new, create_typer, Watcher::start);
    }

    /// Resource factories keep reload regressions independent of desktop devices.
    fn apply_config_with(
        &mut self,
        new: Config,
        create_typer: impl FnOnce() -> anyhow::Result<Typer>,
        create_watcher: impl FnOnce(
            evdev::KeyCode,
            hotkey::Mode,
            Sender<Command>,
        ) -> anyhow::Result<Watcher>,
    ) {
        self.clear_message();
        if !new.type_text {
            self.cancel_typing();
            self.typer = None;
        } else if self.typer.is_none() {
            match create_typer() {
                Ok(typer) => self.typer = Some(Arc::new(Mutex::new(typer))),
                Err(error) => {
                    self.show_error(format!("Typing unavailable ({error:#}) — clipboard only"))
                }
            }
        }

        let restart_hotkey = new.hotkey != self.config.hotkey
            || new.mode != self.config.mode
            || self.watcher.is_none();
        if restart_hotkey {
            // Quiesce the old producer before cancelling its capture. The new
            // generation filters its queued commands without discarding IPC.
            // With no watcher, no gesture is in flight: keep IPC recordings.
            let stopped = match self.watcher.take() {
                Some(mut watcher) => {
                    let stopped = watcher.stop();
                    self.cancel_recording();
                    stopped
                }
                None => Ok(()),
            };
            let mode = if new.mode == "toggle" {
                hotkey::Mode::Toggle
            } else {
                hotkey::Mode::Hold
            };
            match stopped
                .and_then(|()| hotkey::parse_key(&new.hotkey))
                .and_then(|key| create_watcher(key, mode, self.tx.clone()))
            {
                Ok(watcher) => {
                    self.watcher = Some(watcher);
                    eprintln!("hotkey: reloaded");
                }
                Err(error) => self.show_error(format!("hotkey: {error:#}")),
            }
        }
        if let Some(tray) = &self.tray
            && (new.hotkey != self.config.hotkey || new.mode != self.config.mode)
        {
            tray.set_hint(tray::hint(&new.hotkey, &new.mode));
        }
        self.config = new;
    }

    fn cancel_recording(&mut self) {
        self.start_pending = false;
        if matches!(
            self.status,
            Status::Recording { .. } | Status::StartFailed { .. }
        ) {
            self.recording = None;
            self.status = Status::Idle;
            self.reset_waveform();
        }
    }

    fn cancel_typing(&mut self) {
        if let Some(cancelled) = &self.typing_cancel {
            cancelled.store(true, Ordering::Release);
        }
    }

    fn cancel_copy(&mut self) {
        if let Some(cancelled) = &self.copy_cancel {
            cancelled.store(true, Ordering::Release);
        }
    }

    fn prepare_quit(&mut self) {
        self.shutting_down = true;
        self.cancel_recording();
        self.cancel_typing();
        self.cancel_copy();
        self.typer = None;
        if let Some(mut watcher) = self.watcher.take()
            && let Err(error) = watcher.stop()
        {
            eprintln!("hotkey shutdown: {error:#}");
        }
    }

    fn fail_recording_if_needed(&mut self) {
        let Some(error) = self
            .recording
            .as_ref()
            .and_then(|rec| rec.capture.failure())
        else {
            return;
        };
        eprintln!("recording failed: {error}");
        self.recording = None;
        self.reset_waveform();
        if let Status::Recording { started, .. } = self.status {
            // Keep short gestures invisible even if the stream fails during startup.
            self.status = Status::StartFailed {
                started,
                error: "Microphone unavailable. Check your input device.",
            };
        }
    }

    fn start_recording(&mut self) {
        // Before opening the mic, so its latency counts toward the cue delay.
        let started = Instant::now();
        let error = if self.config.api_key.is_none() {
            eprintln!("no api key configured");
            "Add an OpenRouter API key in Settings."
        } else {
            match audio::start(self.config.mic.as_deref()) {
                Ok(rec) => {
                    eprintln!("recording started ({} Hz)", rec.sample_rate);
                    self.recording = Some(rec);
                    self.status = Status::Recording {
                        started,
                        cued: false,
                    };
                    self.reset_waveform();
                    return;
                }
                Err(e) => {
                    eprintln!("audio start failed: {e:#}");
                    if self.config.mic.is_some() {
                        "Microphone unavailable — check Settings → Microphone."
                    } else {
                        "Microphone unavailable. Check your input device."
                    }
                }
            }
        };
        self.status = Status::StartFailed { started, error };
    }

    fn show_start_failure(&mut self, error: &'static str) {
        self.status = Status::Idle;
        if self.config.beeps {
            beep::error();
        }
        self.show_error(error);
    }

    /// Capture starts at once so no speech is lost, but the beep and pill — or
    /// a start failure — wait until `MIN_HOLD` has passed, so the taps and
    /// shortcuts the hotkey cancels before then stay invisible.
    fn cue_if_due(&mut self) {
        match self.status {
            Status::StartFailed { started, error } if started.elapsed() >= hotkey::MIN_HOLD => {
                self.show_start_failure(error);
            }
            Status::Recording {
                started,
                cued: false,
            } if started.elapsed() >= hotkey::MIN_HOLD => {
                self.status = Status::Recording {
                    started,
                    cued: true,
                };
                if self.config.beeps {
                    beep::start();
                }
                self.clear_message();
            }
            _ => {}
        }
    }

    fn stop_and_transcribe(&mut self) {
        let Some(rec) = self.recording.take() else {
            return;
        };
        let (samples, sample_rate) = match rec.finish() {
            Ok(recording) => recording,
            Err(error) => {
                eprintln!("recording failed: {error:#}");
                self.reset_waveform();
                self.show_start_failure("Microphone unavailable. Check your input device.");
                return;
            }
        };
        self.reset_waveform();
        let peak = samples.iter().fold(0.0f32, |a, &s| a.max(s.abs()));
        eprintln!(
            "recorded {:.1}s, peak {:.3}",
            samples.len() as f32 / sample_rate as f32,
            peak
        );

        if samples.len() < sample_rate as usize / 4 {
            self.status = Status::Idle;
            self.show_notice("Keep holding while you speak.");
            return;
        }

        let wav = match audio::encode_wav(&samples, sample_rate) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("wav encode failed: {e:#}");
                self.status = Status::Idle;
                self.show_error("Unable to prepare the recording. Try again.");
                return;
            }
        };

        self.status = Status::Transcribing;
        if self.config.beeps {
            beep::stop();
        }
        let api_key = self.config.api_key.clone().unwrap_or_default();
        let model = self.config.model.clone();
        let language = self.config.language.clone();
        let cleanup = self.config.cleanup.then(|| Cleanup {
            api_key: api_key.clone(),
            model: self.config.cleanup_model.clone(),
            language: language.clone(),
        });
        self.spawn_work(move || {
            eprintln!(
                "transcribe task running ({} bytes wav, model {model})",
                wav.len()
            );
            let result = api::transcribe(&api_key, &model, &wav, language.as_deref());
            eprintln!(
                "transcribe returned: {:?}",
                result.as_ref().map(|t| t.len())
            );
            Internal::Transcribed { result, cleanup }
        });
    }

    fn handle_internal(&mut self, event: Internal) {
        match event {
            Internal::Transcribed { result, cleanup } => self.transcribed(result, cleanup),
            Internal::Text(text) => self.deliver_transcript(text),
            Internal::Copied { result, text } => {
                if let Some(Delivery { typer, cancelled }) = self.prepare_delivery(result) {
                    let chars = text.chars().count();
                    let child_cancel = cancelled.clone();
                    self.spawn_work(move || {
                        let outcome = match typer.lock() {
                            Ok(mut typer) => typer.type_str(&text, &child_cancel),
                            Err(_) => Err(anyhow::anyhow!("typing state poisoned")),
                        };
                        Internal::Typed {
                            outcome,
                            cancelled,
                            chars,
                        }
                    });
                }
            }
            Internal::Typed {
                outcome,
                cancelled,
                chars,
            } => {
                self.typing_cancel = None;
                self.drain_commands();
                self.finish_delivery();
                if cancelled.load(Ordering::Acquire) {
                    self.clear_message();
                } else {
                    match outcome {
                        Ok(()) => {
                            eprintln!("typed {chars} chars");
                            self.clear_message();
                        }
                        Err(e) => {
                            eprintln!("typing: {e:#}");
                            self.show_error("Typing failed. Full text copied.");
                        }
                    }
                }
            }
        }
    }

    fn transcribed(&mut self, result: anyhow::Result<String>, cleanup: Option<Cleanup>) {
        let raw = match result {
            Ok(t) if !t.is_empty() => {
                eprintln!("transcript: {t:?}");
                t
            }
            Ok(_) => {
                self.drain_commands();
                self.finish_delivery();
                self.show_notice("No speech detected. Try again.");
                return;
            }
            Err(e) => {
                eprintln!("transcription error: {e:#}");
                self.drain_commands();
                self.finish_delivery();
                self.show_error("Transcription failed. Check your connection and speech model.");
                return;
            }
        };
        let Some(cleanup) = cleanup else {
            return self.deliver_transcript(raw);
        };
        self.status = Status::Cleaning;
        self.spawn_work(move || {
            let language = cleanup.language.as_deref();
            Internal::Text(
                match api::cleanup(&cleanup.api_key, &cleanup.model, &raw, language) {
                    Ok(cleaned) => {
                        eprintln!("cleanup applied ({} -> {} chars)", raw.len(), cleaned.len());
                        cleaned
                    }
                    Err(e) => {
                        eprintln!("cleanup failed, using raw transcript: {e:#}");
                        raw
                    }
                },
            )
        });
    }

    /// Publish before insertion. Commands remain live while wl-copy is waiting
    /// on the compositor; output changes are applied again after publication.
    fn deliver_transcript(&mut self, text: String) {
        self.drain_commands();
        if self.shutting_down {
            return;
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        self.copy_cancel = Some(cancelled.clone());
        self.status = Status::Copying;
        self.spawn_work(move || Internal::Copied {
            result: clipboard::copy(&text, &cancelled),
            text,
        });
    }

    fn prepare_delivery(&mut self, copied: anyhow::Result<()>) -> Option<Delivery> {
        // Save may have queued Reload that the loop hasn't handled yet. Apply
        // it before choosing clipboard-only vs typing.
        self.drain_commands();
        if self.shutting_down {
            return None;
        }
        self.copy_cancel = None;
        match copied {
            Ok(()) => self.begin_delivery(),
            Err(error) => {
                eprintln!("clipboard: {error:#}");
                self.finish_delivery();
                self.show_error("Copy failed. Check wl-clipboard.");
                None
            }
        }
    }

    fn begin_delivery(&mut self) -> Option<Delivery> {
        let typer = self.config.type_text.then(|| self.typer.clone()).flatten();
        if let Some(typer) = typer {
            let cancelled = Arc::new(AtomicBool::new(false));
            self.typing_cancel = Some(cancelled.clone());
            self.status = Status::Typing;
            Some(Delivery { typer, cancelled })
        } else {
            self.show_notice("Copied. Paste to insert.");
            self.finish_delivery();
            None
        }
    }

    /// Delivery is over. Callers first drain commands sent meanwhile, applied
    /// as still busy, so a start whose gesture already ended stays dropped and
    /// only one still pending records.
    fn finish_delivery(&mut self) {
        self.status = Status::Idle;
        if std::mem::take(&mut self.start_pending) && !self.shutting_down {
            self.start_recording();
        }
    }

    fn reset_waveform(&mut self) {
        self.waveform = [0.0; BAR_COUNT];
        self.smoothed_level = 0.0;
    }

    fn clear_message(&mut self) {
        self.error = None;
        self.notice = None;
        self.message_expires_at = None;
    }

    fn show_error(&mut self, text: impl Into<String>) {
        self.error = Some(text.into());
        self.notice = None;
        self.message_expires_at = Some(Instant::now() + ERROR_DURATION);
    }

    fn show_notice(&mut self, text: impl Into<String>) {
        self.notice = Some(text.into());
        self.error = None;
        self.message_expires_at = Some(Instant::now() + NOTICE_DURATION);
    }

    /// Whether the pill shows the status rather than a message.
    fn shows_status(&self) -> bool {
        match self.status {
            Status::Recording { cued, .. } => cued,
            Status::StartFailed { .. } => false,
            _ => self.status.is_active(),
        }
    }

    fn pill_visible(&self) -> bool {
        self.shows_status() || self.error.is_some() || self.notice.is_some()
    }

    /// Idle means no pill process at all: an invisible surface can still take
    /// clicks or focus. The process starts with capture, so its window is ready
    /// by the time the recording is cued, and opens with the first view.
    fn sync_pill(&mut self) {
        let visible = self.pill_visible();
        let warm = matches!(
            self.status,
            Status::Recording { cued: false, .. } | Status::StartFailed { .. }
        );
        if !visible && !warm {
            self.pill = None;
            self.pill_failed = false;
            return;
        }
        if self.pill.as_mut().is_some_and(|pill| !pill.is_open()) {
            eprintln!("pill: window process exited");
            self.pill = None;
            self.pill_failed = true;
        }
        if self.pill.is_none() && !self.pill_failed {
            let tx = self.tx.clone();
            match ui::Window::spawn("pill", move |event| {
                if event["event"] == "click" {
                    let _ = tx.send(Command::Toggle);
                }
            }) {
                Ok(pill) => self.pill = Some(pill),
                Err(error) => {
                    eprintln!("pill: {error:#}");
                    self.pill_failed = true;
                }
            }
        }
        if visible {
            let view = self.pill_view();
            if let Some(pill) = &mut self.pill {
                pill.show(&view);
            }
        }
    }

    fn pill_view(&self) -> PillView {
        let recording = matches!(self.status, Status::Recording { .. });
        let kind = match (self.shows_status(), recording) {
            (false, _) => "message",
            (true, true) => "recording",
            (true, false) => "busy",
        };
        PillView {
            dot: format!("#{:06x}", self.dot_color()),
            kind,
            bars: if recording {
                self.waveform
                    .iter()
                    .map(|l| (l * 100.0).round() / 100.0)
                    .collect()
            } else {
                Vec::new()
            },
            label: self.status_label(),
            message: self
                .error
                .clone()
                .or_else(|| self.notice.clone())
                .unwrap_or_default(),
        }
    }

    fn status_label(&self) -> String {
        match &self.status {
            Status::Recording { started, .. } => {
                let secs = started.elapsed().as_secs();
                format!("{}:{:02}", secs / 60, secs % 60)
            }
            Status::Transcribing => "Transcribing".to_string(),
            Status::Cleaning => "Refining".to_string(),
            Status::Copying => "Copying".to_string(),
            Status::Typing => "Inserting".to_string(),
            Status::Idle | Status::StartFailed { .. } => String::new(),
        }
    }

    fn dot_color(&self) -> u32 {
        match &self.status {
            Status::Idle | Status::StartFailed { .. } => {
                if self.error.is_some() {
                    0xff6b6b
                } else {
                    0xf6c85f
                }
            }
            Status::Recording { .. } => 0xff5f57,
            Status::Transcribing | Status::Cleaning | Status::Copying | Status::Typing => 0x8da2fb,
        }
    }
}

impl Drop for Dictation {
    fn drop(&mut self) {
        self.cancel_typing();
        self.cancel_copy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn config(type_text: bool) -> Config {
        Config {
            api_key: None,
            model: crate::config::DEFAULT_MODEL.into(),
            language: None,
            mode: "hold".into(),
            hotkey: crate::config::DEFAULT_HOTKEY.into(),
            mic: None,
            type_text,
            beeps: false,
            cleanup: false,
            cleanup_model: crate::config::DEFAULT_CLEANUP_MODEL.into(),
            path: PathBuf::from("/unused-test-config"),
        }
    }

    fn typer() -> Typer {
        // Delivery preparation never connects; typing is tested in typer.rs.
        Typer::for_test()
    }

    fn watcher(_: evdev::KeyCode, _: hotkey::Mode, _: Sender<Command>) -> anyhow::Result<Watcher> {
        Ok(Watcher::for_test())
    }

    fn view(type_text: bool) -> Dictation {
        let (tx, rx) = channel();
        Dictation::new(
            config(type_text),
            rx,
            tx,
            type_text.then(typer),
            Some(Watcher::for_test()),
            None,
        )
    }

    /// Runs `test` in a child test process whose XDG_CONFIG_HOME holds
    /// `config_toml`, never the user's configuration.
    fn in_isolated_config(name: &str, child: &str, config_toml: &str) -> bool {
        if std::env::var_os(child).is_some() {
            return true;
        }
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("dictationapp")).unwrap();
        std::fs::write(
            directory.path().join("dictationapp/config.toml"),
            config_toml,
        )
        .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(child, "1")
            .env("XDG_CONFIG_HOME", directory.path())
            .env_remove("OPENROUTER_API_KEY")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        false
    }

    #[test]
    fn clipboard_only_completion_is_visible_and_failed_copy_never_types() {
        let mut view = view(true);
        view.status = Status::Copying;
        assert!(
            view.prepare_delivery(Err(anyhow::anyhow!("publisher failed")))
                .is_none()
        );
        assert!(matches!(view.status, Status::Idle));
        assert!(view.typing_cancel.is_none());
        assert_eq!(
            view.error.as_deref(),
            Some("Copy failed. Check wl-clipboard.")
        );
        assert!(view.notice.is_none());

        for type_text in [false, true] {
            view.config.type_text = type_text;
            view.typer = None;
            view.status = Status::Copying;
            assert!(view.prepare_delivery(Ok(())).is_none());
            assert!(matches!(view.status, Status::Idle));
            assert_eq!(view.notice.as_deref(), Some("Copied. Paste to insert."));
            assert!(view.error.is_none());
            assert!(view.pill_visible());
            view.clear_message();
            assert!(!view.pill_visible());
        }
    }

    #[test]
    fn shutdown_and_drop_cancel_the_clipboard_publisher() {
        for shutdown in [false, true] {
            let mut view = view(false);
            let cancelled = Arc::new(AtomicBool::new(false));
            view.copy_cancel = Some(cancelled.clone());
            view.status = Status::Copying;
            if shutdown {
                view.prepare_quit();
                assert!(view.prepare_delivery(Ok(())).is_none());
                assert!(view.notice.is_none());
            }
            drop(view);
            assert!(cancelled.load(Ordering::Acquire));
        }
    }

    /// Real Wayland fixture, driven only by the isolated niri script.
    /// Injects a completed transcript instead of microphone/network I/O.
    #[test]
    #[ignore]
    fn native_delivery_fixture() {
        eprintln!("fixture: reliable clipboard publication");
        let text = std::env::var("DICTATION_TEST_TRANSCRIPT").expect("use check-delivery.py");
        let trigger = PathBuf::from(std::env::var_os("DICTATION_TEST_TRIGGER").unwrap());
        let config = Config::load().unwrap();
        let (tx, rx) = channel();
        let server = crate::ipc::listen(tx.clone()).unwrap();
        let typer = config.type_text.then(|| Typer::new().unwrap());
        let view = Dictation::new(config, rx, tx, typer, None, None);
        let internal = view.internal_tx.clone();
        let wake = view.tx.clone();
        std::thread::spawn(move || {
            while !trigger.exists() {
                std::thread::sleep(Duration::from_millis(20));
            }
            internal.send(Internal::Text(text)).unwrap();
            wake.send(Command::Wake).unwrap();
        });
        view.run();
        drop(server);
    }

    #[test]
    fn output_reload_applies_to_pending_and_active_delivery() {
        let mut view = view(true);
        view.status = Status::Transcribing;
        view.apply_config_with(
            config(false),
            || panic!("disabled output created a typer"),
            watcher,
        );
        assert!(matches!(view.status, Status::Transcribing));
        assert!(view.begin_delivery().is_none());
        assert!(matches!(view.status, Status::Idle));

        view.apply_config_with(config(true), || Ok(typer()), watcher);
        let Delivery { cancelled, .. } = view
            .begin_delivery()
            .expect("enabling output must create delivery");
        view.apply_config_with(
            config(false),
            || panic!("disabled output created a typer"),
            watcher,
        );
        assert!(cancelled.load(Ordering::Acquire));
        assert!(!view.config.type_text);
        assert!(view.typer.is_none());
    }

    #[test]
    fn unavailable_delivery_fails_closed_and_can_retry() {
        let mut view = view(false);
        view.apply_config_with(
            config(true),
            || anyhow::bail!("no virtual keyboard"),
            watcher,
        );
        assert!(
            view.error
                .as_ref()
                .is_some_and(|message| message.contains("no virtual keyboard"))
        );
        assert!(view.begin_delivery().is_none());
        view.apply_config_with(config(true), || Ok(typer()), watcher);
        assert!(view.begin_delivery().is_some());
    }

    #[test]
    fn hotkey_reload_cancels_capture_and_rejects_queued_old_gestures() {
        let mut view = view(false);
        let old = view.watcher.as_ref().unwrap().generation();
        view.status = Status::Recording {
            started: Instant::now(),
            cued: false,
        };
        let mut new = config(false);
        new.hotkey = "KEY_CAPSLOCK".into();
        view.apply_config_with(new, || panic!("disabled output created a typer"), watcher);
        let current = view.watcher.as_ref().unwrap().generation();
        assert_ne!(current, old);
        assert!(matches!(view.status, Status::Idle));
        for action in [
            crate::ipc::HotkeyCommand::Start,
            crate::ipc::HotkeyCommand::Stop,
            crate::ipc::HotkeyCommand::Toggle,
        ] {
            view.handle_command(Command::Hotkey {
                generation: old,
                action,
            });
            assert!(matches!(view.status, Status::Idle));
        }
        // IPC remains independent of watcher generations, and a current
        // gesture still reaches the normal missing-key path without a mic.
        view.handle_command(Command::Start);
        assert!(matches!(view.status, Status::StartFailed { .. }));
        view.handle_command(Command::Cancel);
        view.handle_command(Command::Hotkey {
            generation: current,
            action: crate::ipc::HotkeyCommand::Start,
        });
        assert!(matches!(view.status, Status::StartFailed { .. }));
    }

    #[test]
    fn correcting_initial_hotkey_failure_creates_a_watcher() {
        let mut view = view(false);
        view.watcher = None;
        view.config.hotkey = "INVALID_TEST_KEY".into();
        // Started over IPC; no hotkey gesture can be in flight.
        view.status = Status::Recording {
            started: Instant::now(),
            cued: true,
        };
        view.apply_config_with(
            config(false),
            || panic!("disabled output created a typer"),
            watcher,
        );
        assert!(view.watcher.is_some());
        assert!(matches!(view.status, Status::Recording { .. }));
        view.status = Status::Idle;

        let mut invalid = config(false);
        invalid.hotkey = "INVALID_TEST_KEY".into();
        view.apply_config_with(
            invalid,
            || panic!("disabled output created a typer"),
            watcher,
        );
        assert!(view.watcher.is_none());
        assert!(view.error.is_some());
        view.apply_config_with(
            config(false),
            || panic!("disabled output created a typer"),
            watcher,
        );
        assert!(view.watcher.is_some());
    }

    #[test]
    fn start_queued_as_a_transcript_lands_is_not_dropped() {
        let mut view = view(false);
        view.status = Status::Transcribing;
        view.tx.send(Command::Start).unwrap();
        assert!(view.prepare_delivery(Ok(())).is_none());
        // No API key in tests, so a handled Start reaches StartFailed.
        assert!(matches!(view.status, Status::StartFailed { .. }));
    }

    #[test]
    fn start_during_typing_records_once_delivery_finishes() {
        let mut view = view(true);
        view.status = Status::Transcribing;
        view.tx.send(Command::Start).unwrap();
        assert!(view.prepare_delivery(Ok(())).is_some());
        assert!(matches!(view.status, Status::Typing));
        view.drain_commands();
        view.finish_delivery();
        // No API key in tests, so the started recording reaches StartFailed.
        assert!(matches!(view.status, Status::StartFailed { .. }));

        // A gesture that ends while still busy records nothing afterwards,
        // even when its end is still queued as delivery finishes.
        for (start, end) in [
            (Command::Start, Command::Stop),
            (Command::Start, Command::Cancel),
            (Command::Toggle, Command::Toggle),
        ] {
            view.status = Status::Typing;
            view.handle_command(start);
            view.tx.send(end).unwrap();
            view.drain_commands();
            view.finish_delivery();
            assert!(matches!(view.status, Status::Idle), "{start:?} {end:?}");
        }
    }

    /// Typing reports back through the loop: the outcome lands, and a start
    /// held during delivery records once it finished.
    #[test]
    fn typed_result_finishes_delivery_through_the_loop() {
        let mut view = view(true);
        view.status = Status::Typing;
        view.typing_cancel = Some(Arc::new(AtomicBool::new(false)));
        view.handle_command(Command::Start);
        view.handle_internal(Internal::Typed {
            outcome: Err(anyhow::anyhow!("compositor went away")),
            cancelled: Arc::new(AtomicBool::new(false)),
            chars: 3,
        });
        assert!(view.typing_cancel.is_none());
        assert_eq!(
            view.error.as_deref(),
            Some("Typing failed. Full text copied.")
        );
        assert!(matches!(view.status, Status::StartFailed { .. }));
    }

    #[test]
    fn shutdown_cancels_delivery_and_blocks_later_commands() {
        let mut view = view(true);
        let Delivery { cancelled, .. } = view.begin_delivery().unwrap();
        view.prepare_quit();
        assert!(cancelled.load(Ordering::Acquire));
        assert!(view.typer.is_none());
        assert!(view.watcher.is_none());
        view.status = Status::Idle;
        view.handle_command(Command::Start);
        assert!(matches!(view.status, Status::Idle));
    }

    #[test]
    fn dropping_the_view_cancels_inflight_delivery() {
        let mut view = view(true);
        let cancelled = view.begin_delivery().unwrap().cancelled;
        drop(view);
        assert!(cancelled.load(Ordering::Acquire));
    }

    #[test]
    fn queued_output_reload_precedes_delivery() {
        if !in_isolated_config(
            "app::tests::queued_output_reload_precedes_delivery",
            "DICTATION_TEST_QUEUED_OUTPUT_RELOAD",
            "type_text = false\nbeeps = false\ncleanup = false\n",
        ) {
            return;
        }
        let mut view = view(true);
        view.status = Status::Transcribing;
        view.tx.send(Command::Reload).unwrap();
        // Completion arrives before the loop handled the queued Reload.
        assert!(view.prepare_delivery(Ok(())).is_none());
        assert!(!view.config.type_text);
        assert_eq!(view.notice.as_deref(), Some("Copied. Paste to insert."));
    }

    #[test]
    fn malformed_config_never_opens_writable_settings() {
        if !in_isolated_config(
            "app::tests::malformed_config_never_opens_writable_settings",
            "DICTATION_TEST_SETTINGS_LOAD_FAILURE",
            "api_key = [malformed\n",
        ) {
            return;
        }
        let path = Config::default_path();
        let original = std::fs::read(&path).unwrap();
        let mut view = view(false);
        view.handle_command(Command::Settings);
        assert!(view.error.is_some());
        assert!(view.settings.is_none());
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn idle_is_the_only_inactive_status() {
        assert!(!Status::Idle.is_active());
        assert!(
            Status::Recording {
                started: Instant::now(),
                cued: false,
            }
            .is_active()
        );
        assert!(Status::Transcribing.is_active());
        assert!(Status::Cleaning.is_active());
        assert!(Status::Typing.is_active());
    }

    #[test]
    fn waveform_keeps_real_recent_samples() {
        let mut history = [0.0; BAR_COUNT];
        let mut smoothed = 0.0;
        push_waveform(&mut history, &mut smoothed, 0.4);
        assert!(history[BAR_COUNT - 1] > 0.0);
        assert!(history[..BAR_COUNT - 1].iter().all(|level| *level == 0.0));
        let first = history[BAR_COUNT - 1];
        push_waveform(&mut history, &mut smoothed, 0.0);
        assert_eq!(history[BAR_COUNT - 2], first);
        assert!(history[BAR_COUNT - 1] < first);
    }
}
