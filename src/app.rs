use crate::audio::Recording;
use crate::config::Config;
use crate::hotkey::Watcher;
use crate::ipc::Command;
use crate::typer::Typer;
use crate::{api, audio, beep, settings};
use gpui::{
    App, ClipboardItem, Context, FocusHandle, Focusable, MouseButton, MouseDownEvent, SharedString,
    Window, div, prelude::*, px, rgb, rgba,
};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub enum Status {
    Idle,
    Recording { started: Instant },
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
    config: Config,
    status: Status,
    recording: Option<Recording>,
    error: Option<String>,
    notice: Option<String>,
    rx: Receiver<Command>,
    tx: Sender<Command>,
    typer: Option<Arc<Mutex<Typer>>>,
    watcher: Option<Watcher>,
    waveform: [f32; BAR_COUNT],
    smoothed_level: f32,
    message_expires_at: Option<Instant>,
    /// Free-running animation phase, advanced by the pump.
    anim: f32,
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
            config,
            status: Status::Idle,
            recording: None,
            error: None,
            notice: None,
            rx,
            tx,
            typer: typer.map(|t| Arc::new(Mutex::new(t))),
            watcher,
            waveform: [0.0; BAR_COUNT],
            smoothed_level: 0.0,
            message_expires_at: None,
            anim: 0.0,
        };
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
                    let mut changed = false;
                    while let Ok(cmd) = view.rx.try_recv() {
                        view.handle_command(cmd, cx);
                        changed = true;
                    }
                    if let Some(level) = view.recording.as_ref().map(|rec| rec.capture.level()) {
                        push_waveform(&mut view.waveform, &mut view.smoothed_level, level);
                        changed = true;
                    }
                    if let Some(deadline) = view.message_expires_at {
                        if Instant::now() >= deadline {
                            view.clear_message();
                            changed = true;
                        }
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
            }
        })
        .detach();
    }

    fn handle_command(&mut self, cmd: Command, cx: &mut Context<Self>) {
        eprintln!("handling {cmd:?}");
        match cmd {
            Command::Toggle => match self.status {
                Status::Recording { .. } => self.stop_and_transcribe(cx),
                Status::Idle => self.start_recording(cx),
                _ => {}
            },
            Command::Start => {
                if matches!(self.status, Status::Idle) {
                    self.start_recording(cx);
                }
            }
            Command::Stop => {
                if matches!(self.status, Status::Recording { .. }) {
                    self.stop_and_transcribe(cx);
                }
            }
            Command::Cancel => {
                if matches!(self.status, Status::Recording { .. }) {
                    self.recording = None;
                    self.status = Status::Idle;
                    self.reset_waveform();
                    self.clear_message();
                }
            }
            Command::Quit => cx.quit(),
            Command::Settings => settings::open(cx, self.tx.clone()),
            Command::Reload => self.reload_config(),
        }
    }

    /// Re-read config.toml; restart the hotkey watcher if the key/mode changed.
    fn reload_config(&mut self) {
        let Ok(new) = Config::load() else {
            self.show_error("Unable to reload settings. Check config.toml.");
            return;
        };
        self.clear_message();
        if new.hotkey != self.config.hotkey || new.mode != self.config.mode {
            if let Some(w) = &mut self.watcher {
                let mode = if new.mode == "toggle" {
                    crate::hotkey::Mode::Toggle
                } else {
                    crate::hotkey::Mode::Hold
                };
                match crate::hotkey::parse_key(&new.hotkey)
                    .and_then(|k| w.restart(k, mode, self.tx.clone()))
                {
                    Ok(()) => eprintln!("hotkey: reloaded"),
                    Err(e) => self.show_error(format!("hotkey: {e:#}")),
                }
            }
        }
        self.config = new;
    }

    fn start_recording(&mut self, _cx: &mut Context<Self>) {
        if self.config.api_key.is_none() {
            eprintln!("no api key configured");
            self.show_error("Add an OpenRouter API key in Settings.");
            return;
        }
        match audio::start() {
            Ok(rec) => {
                eprintln!("recording started ({} Hz)", rec.sample_rate);
                if self.config.beeps {
                    beep::start();
                }
                self.recording = Some(rec);
                self.status = Status::Recording {
                    started: Instant::now(),
                };
                self.reset_waveform();
                self.clear_message();
            }
            Err(e) => {
                eprintln!("audio start failed: {e:#}");
                if self.config.beeps {
                    beep::error();
                }
                self.show_error("Microphone unavailable. Check your input device.");
            }
        }
    }

    fn stop_and_transcribe(&mut self, cx: &mut Context<Self>) {
        let Some(rec) = self.recording.take() else {
            return;
        };
        let (samples, sample_rate) = rec.finish();
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
        let typer = self.typer.clone();

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
            let typed = this
                .update(cx, |view, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                    if typer.is_some() {
                        view.status = Status::Typing;
                    } else {
                        view.status = Status::Idle;
                        view.clear_message();
                    }
                    cx.notify();
                    typer.is_some()
                })
                .unwrap_or(false);

            if typed {
                let typer = typer.unwrap();
                let text2 = text.clone();
                let n = text2.len();
                let outcome = cx
                    .background_executor()
                    .spawn(async move { typer.lock().unwrap().type_str(&text2) })
                    .await;
                this.update(cx, |view, cx| {
                    view.status = Status::Idle;
                    match outcome {
                        Ok(()) => {
                            eprintln!("typed {n} chars");
                            view.clear_message();
                        }
                        Err(e) => view.show_error(format!(
                            "Typing failed ({e:#}) — transcript is on the clipboard"
                        )),
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
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

    fn pill_visible(&self) -> bool {
        self.status.is_active() || self.error.is_some() || self.notice.is_some()
    }

    fn status_label(&self) -> String {
        match &self.status {
            Status::Recording { started } => {
                let secs = started.elapsed().as_secs();
                format!("{}:{:02}", secs / 60, secs % 60)
            }
            Status::Transcribing => "Transcribing".to_string(),
            Status::Cleaning => "Refining".to_string(),
            Status::Typing => "Inserting".to_string(),
            Status::Idle => String::new(),
        }
    }

    fn dot_color(&self) -> u32 {
        match &self.status {
            Status::Idle => {
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

        let pill = if matches!(self.status, Status::Idle) {
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

    #[test]
    fn idle_is_the_only_inactive_status() {
        assert!(!Status::Idle.is_active());
        assert!(
            Status::Recording {
                started: Instant::now()
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
