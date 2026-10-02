use crate::audio::Recording;
use crate::config::Config;
use crate::hotkey::Watcher;
use crate::ipc::Command;
use crate::typer::Typer;
use crate::{api, audio, beep, hotkey, niri, settings};
use gpui::{
    App, Bounds, ClipboardItem, Context, Entity, FocusHandle, Focusable, MouseButton,
    MouseDownEvent, SharedString, TitlebarOptions, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowOptions, div, prelude::*, px, rgb, rgba, size,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
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
    Typing,
}

impl Status {
    fn is_active(&self) -> bool {
        !matches!(self, Status::Idle)
    }
}

const BAR_COUNT: usize = 21;
const ERROR_DURATION: Duration = Duration::from_secs(4);
const NOTICE_DURATION: Duration = Duration::from_millis(2500);

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

pub struct DictationView {
    focus_handle: FocusHandle,
    window: Option<WindowHandle<Self>>,
    config: Config,
    status: Status,
    recording: Option<Recording>,
    error: Option<String>,
    notice: Option<String>,
    rx: Receiver<Command>,
    tx: Sender<Command>,
    typer: Option<Arc<Mutex<Typer>>>,
    watcher: Option<Watcher>,
    /// Shared with the delivery child; settings changes and every shutdown stop it.
    typing_cancel: Option<Arc<AtomicBool>>,
    shutting_down: bool,
    waveform: [f32; BAR_COUNT],
    smoothed_level: f32,
    message_expires_at: Option<Instant>,
    /// Free-running animation phase, advanced by the pump.
    anim: f32,
}

struct Delivery {
    typer: Arc<Mutex<Typer>>,
    cancelled: Arc<AtomicBool>,
}

impl DictationView {
    pub fn new(
        config: Config,
        rx: Receiver<Command>,
        tx: Sender<Command>,
        typer: Option<Typer>,
        watcher: Option<Watcher>,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = Self {
            focus_handle: cx.focus_handle(),
            window: None,
            config,
            status: Status::Idle,
            recording: None,
            error: None,
            notice: None,
            rx,
            tx,
            typer: typer.map(|t| Arc::new(Mutex::new(t))),
            watcher,
            typing_cancel: None,
            shutting_down: false,
            waveform: [0.0; BAR_COUNT],
            smoothed_level: 0.0,
            message_expires_at: None,
            anim: 0.0,
        };
        cx.on_app_quit(|view, _| {
            view.prepare_quit();
            async {}
        })
        .detach();
        view.start_pump(cx);
        view
    }

    /// Poll the command channel and the mic level on a timer.
    fn start_pump(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(40))
                    .await;
                let alive = match this.update(cx, |view, cx| {
                    view.anim += 0.04;
                    let mut changed = view.drain_commands(cx);
                    changed |= view.fail_recording_if_needed();
                    changed |= view.cue_if_due();
                    if let Some(level) = view.recording.as_ref().map(|rec| rec.capture.level()) {
                        push_waveform(&mut view.waveform, &mut view.smoothed_level, level);
                        changed = true;
                    }
                    if let Some(deadline) = view.message_expires_at
                        && Instant::now() >= deadline
                    {
                        view.clear_message();
                        changed = true;
                    }
                    // Keep redrawing while animated (waveform decay, busy bars).
                    if changed || view.status.is_active() {
                        cx.notify();
                    }
                    true
                }) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("pump update failed: {e}");
                        false
                    }
                };
                if !alive {
                    break;
                }
                // Opening a window renders its root, so release the view update first.
                if let Some(view) = this.upgrade()
                    && let Err(error) = cx.update(|cx| Self::sync_window(&view, cx)).flatten()
                {
                    eprintln!("pill window: {error:#}");
                }
            }
        })
        .detach();
    }

    fn handle_command(&mut self, cmd: Command, cx: &mut Context<Self>) {
        if self.shutting_down {
            return;
        }
        eprintln!("handling {cmd:?}");
        match cmd {
            Command::Hotkey { generation, action } => {
                // Stop/join cannot retract commands already queued by an old listener.
                if self.watcher.as_ref().map(Watcher::generation) == Some(generation) {
                    self.handle_command(action.into(), cx);
                }
            }
            Command::Toggle => match self.status {
                Status::Recording { .. } => self.stop_and_transcribe(cx),
                Status::Idle => self.start_recording(cx),
                Status::StartFailed { error, .. } => self.show_start_failure(error),
                _ => {}
            },
            Command::Start => {
                if matches!(self.status, Status::Idle) {
                    self.start_recording(cx);
                }
            }
            Command::Stop => match self.status {
                Status::Recording { .. } => self.stop_and_transcribe(cx),
                Status::StartFailed { error, .. } => self.show_start_failure(error),
                _ => {}
            },
            Command::Cancel => {
                self.cancel_recording();
            }
            Command::Quit => {
                self.prepare_quit();
                cx.quit();
            }
            Command::Settings => settings::open(cx, self.tx.clone()),
            Command::Reload => self.reload_config(),
        }
    }

    fn drain_commands(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        while let Ok(command) = self.rx.try_recv() {
            self.handle_command(command, cx);
            changed = true;
        }
        changed
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
            let stopped = match self.watcher.take() {
                Some(mut watcher) => watcher.stop(),
                None => Ok(()),
            };
            self.cancel_recording();
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
        self.config = new;
    }

    fn cancel_recording(&mut self) {
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

    fn prepare_quit(&mut self) {
        self.shutting_down = true;
        self.cancel_recording();
        self.cancel_typing();
        self.typer = None;
        if let Some(mut watcher) = self.watcher.take()
            && let Err(error) = watcher.stop()
        {
            eprintln!("hotkey shutdown: {error:#}");
        }
    }

    fn fail_recording_if_needed(&mut self) -> bool {
        let Some(error) = self
            .recording
            .as_ref()
            .and_then(|rec| rec.capture.failure())
        else {
            return false;
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
        true
    }

    fn start_recording(&mut self, _cx: &mut Context<Self>) {
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
    /// shortcuts the hotkey cancels before then stay invisible. Returns
    /// whether anything was shown.
    fn cue_if_due(&mut self) -> bool {
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
                // A recent notice can keep the previous pill open between recordings.
                if self.window.is_some() {
                    niri::reposition();
                }
            }
            _ => return false,
        }
        true
    }

    fn stop_and_transcribe(&mut self, cx: &mut Context<Self>) {
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
        let cleanup_enabled = self.config.cleanup;
        let cleanup_model = self.config.cleanup_model.clone();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let api_key = api_key.clone();
                    let language = language.clone();
                    async move {
                        eprintln!(
                            "transcribe task running ({} bytes wav, model {model})",
                            wav.len()
                        );
                        let r = api::transcribe(&api_key, &model, &wav, language.as_deref());
                        eprintln!("transcribe returned: {:?}", r.as_ref().map(|t| t.len()));
                        r
                    }
                })
                .await;

            match this.update(cx, |view, _| view.shutting_down) {
                Ok(false) => {}
                Ok(true) | Err(_) => return,
            }

            let raw = match result {
                Ok(t) if !t.is_empty() => {
                    eprintln!("transcript: {t:?}");
                    t
                }
                Ok(_) => {
                    this.update(cx, |view, cx| {
                        view.status = Status::Idle;
                        view.show_notice("No speech detected. Try again.");
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                Err(e) => {
                    eprintln!("transcription error: {e:#}");
                    this.update(cx, |view, cx| {
                        view.status = Status::Idle;
                        view.show_error(
                            "Transcription failed. Check your connection and speech model.",
                        );
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };

            let text = if cleanup_enabled {
                this.update(cx, |view, cx| {
                    view.status = Status::Cleaning;
                    cx.notify();
                })
                .ok();
                let cleanup_result =
                    cx.background_executor()
                        .spawn({
                            let raw = raw.clone();
                            async move {
                                api::cleanup(&api_key, &cleanup_model, &raw, language.as_deref())
                            }
                        })
                        .await;
                match cleanup_result {
                    Ok(cleaned) => {
                        eprintln!("cleanup applied ({} -> {} chars)", raw.len(), cleaned.len());
                        cleaned
                    }
                    Err(e) => {
                        eprintln!("cleanup failed, using raw transcript: {e:#}");
                        raw
                    }
                }
            } else {
                raw
            };

            // Always stash in the clipboard as a fallback.
            let delivery = this
                .update(cx, |view, cx| view.prepare_delivery(&text, cx))
                .ok()
                .flatten();

            if let Some(Delivery { typer, cancelled }) = delivery {
                let text2 = text.clone();
                let n = text2.chars().count();
                let child_cancel = cancelled.clone();
                let outcome = cx
                    .background_executor()
                    .spawn(async move {
                        let mut typer = typer
                            .lock()
                            .map_err(|_| anyhow::anyhow!("typing state poisoned"))?;
                        typer.type_str(&text2, &child_cancel)
                    })
                    .await;
                this.update(cx, |view, cx| {
                    view.typing_cancel = None;
                    view.status = Status::Idle;
                    if cancelled.load(Ordering::Acquire) {
                        view.clear_message();
                    } else {
                        match outcome {
                            Ok(()) => {
                                eprintln!("typed {n} chars");
                                view.clear_message();
                            }
                            Err(e) => view.show_error(format!(
                                "Typing failed ({e:#}) — transcript is on the clipboard"
                            )),
                        }
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn prepare_delivery(&mut self, text: &str, cx: &mut Context<Self>) -> Option<Delivery> {
        // Save may have queued Reload while the periodic pump is still waiting
        // for its next tick. Apply it before choosing clipboard-only vs typing.
        self.drain_commands(cx);
        if self.shutting_down {
            return None;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
        let delivery = self.begin_delivery();
        cx.notify();
        delivery
    }

    fn begin_delivery(&mut self) -> Option<Delivery> {
        let typer = self.config.type_text.then(|| self.typer.clone()).flatten();
        if let Some(typer) = typer {
            let cancelled = Arc::new(AtomicBool::new(false));
            self.typing_cancel = Some(cancelled.clone());
            self.status = Status::Typing;
            Some(Delivery { typer, cancelled })
        } else {
            self.status = Status::Idle;
            self.clear_message();
            None
        }
    }

    fn toggle_action(&mut self, _: &crate::Toggle, _: &mut Window, cx: &mut Context<Self>) {
        self.handle_command(Command::Toggle, cx);
        cx.notify();
    }

    fn cancel_action(&mut self, _: &crate::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        self.handle_command(Command::Cancel, cx);
        cx.notify();
    }

    fn quit_action(&mut self, _: &crate::Quit, _: &mut Window, cx: &mut Context<Self>) {
        self.prepare_quit();
        cx.quit();
    }

    fn on_pill_click(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.handle_command(Command::Toggle, cx);
        cx.notify();
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

    /// An invisible toplevel still captures focus on niri, even with an empty
    /// Wayland input region. Only keep a real window while there is a visible pill.
    fn sync_window(view: &Entity<Self>, cx: &mut App) -> anyhow::Result<()> {
        if view.read(cx).pill_visible() && view.read(cx).window.is_none() {
            let root = view.clone();
            let pill = size(px(300.0), px(64.0));
            let bounds = Bounds::centered(None, pill, cx);
            let window = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Dictation".into()),
                        appears_transparent: true,
                        traffic_light_position: None,
                    }),
                    window_background: WindowBackgroundAppearance::Transparent,
                    app_id: Some("dictationapp".to_owned()),
                    focus: false,
                    is_resizable: false,
                    window_min_size: Some(pill),
                    ..Default::default()
                },
                move |window, cx| {
                    window.set_window_title("Dictation");
                    window.resize(pill);
                    window.on_window_should_close(cx, |_, cx| {
                        cx.quit();
                        true
                    });
                    root
                },
            )?;
            view.update(cx, |view, _| view.window = Some(window));
            niri::place_at_bottom();
        } else if !view.read(cx).pill_visible()
            && let Some(window) = view.update(cx, |view, _| view.window.take())
        {
            cx.update_window(window.into(), |_, window, _| window.remove_window())?;
        }
        Ok(())
    }

    fn status_label(&self) -> String {
        match &self.status {
            Status::Recording { started, .. } => {
                let secs = started.elapsed().as_secs();
                format!("{}:{:02}", secs / 60, secs % 60)
            }
            Status::Transcribing => "Transcribing".to_string(),
            Status::Cleaning => "Refining".to_string(),
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
            Status::Transcribing | Status::Cleaning | Status::Typing => 0x8da2fb,
        }
    }
}

impl Drop for DictationView {
    fn drop(&mut self) {
        self.cancel_typing();
    }
}

impl Focusable for DictationView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DictationView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.pill_visible() {
            return div().size_full().bg(rgba(0x00000000));
        }

        let pill = div()
            .id("pill")
            .h(px(56.0))
            .w_full()
            .rounded_full()
            .bg(rgba(0x11141bf2))
            .border_1()
            .border_color(rgba(0xffffff22))
            .px_4()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_pill_click))
            .child(div().size(px(8.0)).rounded_full().bg(rgb(self.dot_color())));

        let pill = if !self.shows_status() {
            let message = self
                .error
                .clone()
                .or_else(|| self.notice.clone())
                .unwrap_or_default();
            pill.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(rgb(0xf2f4f8))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(SharedString::from(message)),
            )
        } else {
            let recording = matches!(self.status, Status::Recording { .. });
            pill.child(
                div()
                    .flex_1()
                    .h(px(32.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .gap(px(2.0))
                    .children((0..BAR_COUNT).map(|i| {
                        let h = if recording {
                            3.0 + self.waveform[i] * 25.0
                        } else {
                            3.0 + (((self.anim * 6.0 - i as f32 * 0.55).sin() * 0.5 + 0.5)
                                .powf(4.0)
                                * 13.0)
                        };
                        div()
                            .w(px(2.0))
                            .h(px(h))
                            .rounded_full()
                            .bg(rgb(if recording { 0xf1f4fa } else { 0x8da2fb }))
                    })),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xaeb6c5))
                    .whitespace_nowrap()
                    .child(self.status_label()),
            )
        };

        div()
            .key_context("Dictation")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::toggle_action))
            .on_action(cx.listener(Self::cancel_action))
            .on_action(cx.listener(Self::quit_action))
            .flex()
            .items_center()
            .justify_center()
            .size_full()
            .bg(rgba(0x00000000))
            .p_1()
            .child(pill)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::path::PathBuf;
    use std::sync::mpsc::channel;

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
        // Delivery preparation never executes this path; child I/O is tested in typer.rs.
        Typer::for_test(PathBuf::from("/unused-test-wtype"))
    }

    fn watcher(_: evdev::KeyCode, _: hotkey::Mode, _: Sender<Command>) -> anyhow::Result<Watcher> {
        Ok(Watcher::for_test())
    }

    fn view(cx: &mut TestAppContext, type_text: bool) -> Entity<DictationView> {
        let (tx, rx) = channel();
        cx.new(|cx| {
            DictationView::new(
                config(type_text),
                rx,
                tx,
                type_text.then(typer),
                Some(Watcher::for_test()),
                cx,
            )
        })
    }

    #[gpui::test]
    fn output_reload_applies_to_pending_and_active_delivery(cx: &mut TestAppContext) {
        let view = view(cx, true);
        view.update(cx, |view, _| {
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
        });
    }

    #[gpui::test]
    fn unavailable_delivery_fails_closed_and_can_retry(cx: &mut TestAppContext) {
        let view = view(cx, false);
        view.update(cx, |view, _| {
            view.apply_config_with(config(true), || anyhow::bail!("missing wtype"), watcher);
            assert!(
                view.error
                    .as_ref()
                    .is_some_and(|message| message.contains("missing wtype"))
            );
            assert!(view.begin_delivery().is_none());
            view.apply_config_with(config(true), || Ok(typer()), watcher);
            assert!(view.begin_delivery().is_some());
        });
    }

    #[gpui::test]
    fn hotkey_reload_cancels_capture_and_rejects_queued_old_gestures(cx: &mut TestAppContext) {
        let view = view(cx, false);
        view.update(cx, |view, cx| {
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
                view.handle_command(
                    Command::Hotkey {
                        generation: old,
                        action,
                    },
                    cx,
                );
                assert!(matches!(view.status, Status::Idle));
            }
            // IPC remains independent of watcher generations, and a current
            // gesture still reaches the normal missing-key path without a mic.
            view.handle_command(Command::Start, cx);
            assert!(matches!(view.status, Status::StartFailed { .. }));
            view.handle_command(Command::Cancel, cx);
            view.handle_command(
                Command::Hotkey {
                    generation: current,
                    action: crate::ipc::HotkeyCommand::Start,
                },
                cx,
            );
            assert!(matches!(view.status, Status::StartFailed { .. }));
        });
    }

    #[gpui::test]
    fn correcting_initial_hotkey_failure_creates_a_watcher(cx: &mut TestAppContext) {
        let view = view(cx, false);
        view.update(cx, |view, _| {
            view.watcher = None;
            view.config.hotkey = "INVALID_TEST_KEY".into();
            view.apply_config_with(
                config(false),
                || panic!("disabled output created a typer"),
                watcher,
            );
            assert!(view.watcher.is_some());

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
        });
    }

    #[gpui::test]
    fn shutdown_cancels_delivery_and_blocks_later_commands(cx: &mut TestAppContext) {
        let view = view(cx, true);
        view.update(cx, |view, cx| {
            let Delivery { cancelled, .. } = view.begin_delivery().unwrap();
            view.prepare_quit();
            assert!(cancelled.load(Ordering::Acquire));
            assert!(view.typer.is_none());
            assert!(view.watcher.is_none());
            view.status = Status::Idle;
            view.handle_command(Command::Start, cx);
            assert!(matches!(view.status, Status::Idle));
        });
    }

    #[gpui::test]
    fn dropping_the_view_cancels_inflight_delivery(cx: &mut TestAppContext) {
        let view = view(cx, true);
        let cancelled = view.update(cx, |view, _| view.begin_delivery().unwrap().cancelled);
        drop(view);
        // Entity release effects run when the app context exits its update.
        cx.update(|_| {});
        assert!(cancelled.load(Ordering::Acquire));
    }

    #[gpui::test]
    fn queued_output_reload_precedes_delivery(cx: &mut TestAppContext) {
        const CHILD: &str = "DICTATION_TEST_QUEUED_OUTPUT_RELOAD";
        if std::env::var_os(CHILD).is_some() {
            let view = view(cx, true);
            view.update(cx, |view, cx| {
                view.status = Status::Transcribing;
                view.tx.send(Command::Reload).unwrap();
                // Simulate completion before the pump's next timer tick.
                let text = "Café — full pending transcript";
                assert!(view.prepare_delivery(text, cx).is_none());
                assert!(!view.config.type_text);
                assert_eq!(
                    cx.read_from_clipboard().unwrap().text().as_deref(),
                    Some(text)
                );
            });
            return;
        }

        // An isolated child supplies its own config environment without racing
        // other tests or reading/writing the user's real configuration.
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("dictationapp")).unwrap();
        std::fs::write(
            directory.path().join("dictationapp/config.toml"),
            "type_text = false\nbeeps = false\ncleanup = false\n",
        )
        .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app::tests::queued_output_reload_precedes_delivery",
                "--nocapture",
            ])
            .env(CHILD, "1")
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
