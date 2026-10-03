//! Settings window — a second gpui window so config.toml never needs editing.
//! Every change saves immediately and hot-reloads the running app.

use crate::audio;
use crate::config::Config;
use crate::hotkey;
use crate::ipc::Command;
use crate::{niri, tray};
use anyhow::{Context as _, Result};
use gpui::{
    AnyElement, App, Bounds, Context, Div, FocusHandle, Focusable, FontWeight, Global,
    KeyDownEvent, MouseButton, MouseDownEvent, SharedString, Stateful, Subscription,
    TitlebarOptions, Window, WindowBounds, WindowHandle, WindowOptions, div, prelude::*, px, rgb,
    rgba,
};
use std::sync::mpsc::{Sender, TryRecvError};
use std::time::{Duration, Instant};

/// Speech model presets as (label, OpenRouter slug); anything else is Custom.
const MODELS: &[(&str, &str)] = &[
    ("Fish Audio", "fish-audio/transcribe-1"),
    ("Whisper v3", "openai/whisper-large-v3"),
    ("Whisper", "openai/whisper-1"),
];

/// How long "Press a key…" waits before giving up.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

const KEYS_URL: &str = "https://openrouter.ai/keys";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    ApiKey,
    Model,
    Language,
}

#[derive(Clone, Copy)]
enum Flag {
    TypeText,
    Beeps,
    Cleanup,
}

/// The open settings window, so the tray and `--settings` bring it forward
/// instead of stacking duplicates.
struct OpenSettings(WindowHandle<SettingsView>);
impl Global for OpenSettings {}

/// A pending "press a key" request; dropping it withdraws the request, so
/// closing the window or clicking away can't leave the next key captured.
struct Capture {
    key: hotkey::KeyCapture,
    started: Instant,
}

pub struct SettingsView {
    focus_handle: FocusHandle,
    tx: Sender<Command>,
    /// The editable config — file values only (see `Config::load_file`).
    cfg: Config,
    /// OPENROUTER_API_KEY overrides the file's key; shown read-only.
    env_key: bool,
    custom_model: bool,
    mics: audio::InputDevices,
    reveal_key: bool,
    active: Option<Field>,
    capture: Option<Capture>,
    /// Why the last capture didn't change the key.
    capture_note: Option<&'static str>,
    /// config.toml stopped reading: saving waits until it reads again rather
    /// than replace it with values from before.
    load_error: Option<String>,
    save_error: Option<String>,
    _activation: Subscription,
}

fn delete_previous_word(value: &mut String) {
    let is_break = |ch: char| ch.is_whitespace() || matches!(ch, '/' | '_' | '-');
    // Trailing breaks go with the word, so repeated presses keep deleting.
    let cut = value
        .trim_end_matches(is_break)
        .char_indices()
        .rev()
        .find(|&(_, ch)| is_break(ch))
        .map(|(index, ch)| index + ch.len_utf8())
        .unwrap_or(0);
    value.truncate(cut);
}

/// Open the settings window, or bring the open one forward. `tx` lets every
/// change tell the running app to reload config.
pub fn open<V: 'static>(cx: &mut Context<V>, tx: Sender<Command>) -> Result<()> {
    if let Some(&OpenSettings(handle)) = cx.try_global::<OpenSettings>()
        && handle
            .update(cx, |view, window, cx| {
                view.reread_file();
                cx.notify();
                window.activate_window();
            })
            .is_ok()
    {
        niri::focus_settings();
        return Ok(());
    }
    // A failed read must not become an editable default that a save would
    // persist. File values only: saving never copies OPENROUTER_API_KEY in.
    let config = Config::load_file().context("unable to load settings")?;
    let dims = gpui::size(px(460.0), px(760.0));
    let bounds = Bounds::centered(None, dims, cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Dictation Settings".into()),
                appears_transparent: false,
                traffic_light_position: None,
            }),
            app_id: Some("dictationapp".to_string()),
            focus: true,
            is_resizable: false,
            window_min_size: Some(dims),
            ..Default::default()
        },
        move |window, cx| {
            let view = cx.new(|cx| SettingsView::new(config, tx, window, cx));
            window.set_window_title("Dictation Settings");
            window.focus(&view.read(cx).focus_handle);
            view
        },
    );
    let handle = opened.context("unable to open the settings window")?;
    cx.set_global(OpenSettings(handle));
    Ok(())
}

impl SettingsView {
    fn new(cfg: Config, tx: Sender<Command>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Coming back picks up edits made elsewhere meanwhile. Leaving
        // abandons a pending capture, so the next key typed elsewhere can't
        // become the hotkey.
        let activation = cx.observe_window_activation(window, |view, window, cx| {
            if window.is_window_active() {
                view.reread_file();
                cx.notify();
            } else {
                view.cancel_capture(cx);
            }
        });
        Self {
            focus_handle: cx.focus_handle(),
            tx,
            env_key: std::env::var("OPENROUTER_API_KEY").is_ok_and(|k| !k.trim().is_empty()),
            custom_model: !MODELS.iter().any(|(_, slug)| *slug == cfg.model),
            mics: audio::input_devices(),
            cfg,
            reveal_key: false,
            active: None,
            capture: None,
            capture_note: None,
            load_error: None,
            save_error: None,
            _activation: activation,
        }
    }

    /// Re-read config.toml, so edits made outside this window (an editor and
    /// `--reload`) aren't overwritten by the next change here.
    fn reread_file(&mut self) {
        match self.cfg.reread() {
            Ok(cfg) => {
                self.custom_model = !MODELS.iter().any(|(_, slug)| *slug == cfg.model);
                self.cfg = cfg;
                self.load_error = None;
            }
            Err(e) => self.load_error = Some(format!("{e:#}")),
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
    }

    fn text(&self, f: Field) -> &str {
        match f {
            Field::ApiKey => self.cfg.api_key.as_deref().unwrap_or(""),
            Field::Model => &self.cfg.model,
            Field::Language => self.cfg.language.as_deref().unwrap_or(""),
        }
    }

    fn edit(&mut self, f: Field, change: impl FnOnce(&mut String)) {
        match f {
            Field::ApiKey => change(self.cfg.api_key.get_or_insert_with(String::new)),
            Field::Model => change(&mut self.cfg.model),
            Field::Language => change(self.cfg.language.get_or_insert_with(String::new)),
        }
        self.persist();
    }

    /// Text fields reachable with Tab, in visual order.
    fn fields(&self) -> Vec<Field> {
        let mut order = Vec::new();
        if !self.env_key {
            order.push(Field::ApiKey);
        }
        if self.custom_model {
            order.push(Field::Model);
        }
        order.push(Field::Language);
        order
    }

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        if ks.key == "escape" {
            if self.capture.is_some() {
                self.cancel_capture(cx);
            } else if self.active.is_some() {
                self.active = None;
            } else {
                window.remove_window();
            }
            cx.notify();
            return;
        }
        // The watcher reads the captured key itself; don't also type it.
        if self.capture.is_some() {
            return;
        }
        if ks.key == "tab" {
            let order = self.fields();
            let next = match self.active.and_then(|f| order.iter().position(|o| *o == f)) {
                Some(i) => (i + 1) % order.len(),
                None => 0,
            };
            self.active = order.get(next).copied();
            cx.notify();
            return;
        }
        let Some(field) = self.active else { return };
        if ks.key == "enter" {
            self.active = None;
        } else if ks.key == "backspace" {
            let word = ks.modifiers.control;
            self.edit(field, |v| {
                if word {
                    delete_previous_word(v);
                } else {
                    v.pop();
                }
            });
        } else if ks.modifiers.control && ks.key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                let clean: String = text.chars().filter(|c| !c.is_control()).collect();
                self.edit(field, |v| v.push_str(clean.trim()));
            }
        } else if let Some(ch) = &ks.key_char
            && !ks.modifiers.control
            && !ks.modifiers.platform
        {
            self.edit(field, |v| v.push_str(ch));
        }
        cx.notify();
    }

    fn unfocus(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.active = None;
        self.cancel_capture(cx);
        cx.notify();
    }

    fn start_capture(&mut self, cx: &mut Context<Self>) {
        self.active = None;
        self.capture_note = None;
        self.capture = Some(Capture {
            key: hotkey::KeyCapture::start(),
            started: Instant::now(),
        });
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(30))
                    .await;
                let pending = this.update(cx, |view, cx| view.poll_capture(cx));
                if !pending.unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// Returns whether the capture is still waiting.
    fn poll_capture(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(capture) = &self.capture else {
            return false;
        };
        let key = match capture.key.try_recv() {
            Ok(key) => key,
            Err(TryRecvError::Empty) if capture.started.elapsed() < CAPTURE_TIMEOUT => {
                return true;
            }
            Err(TryRecvError::Empty) => {
                self.cancel_capture(cx);
                self.capture_note = Some("No key detected. Is /dev/input readable?");
                cx.notify();
                return false;
            }
            Err(TryRecvError::Disconnected) => {
                self.capture = None;
                cx.notify();
                return false;
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
        cx.notify();
        false
    }

    fn cancel_capture(&mut self, cx: &mut Context<Self>) {
        if self.capture.take().is_some() {
            cx.notify();
        }
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
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// ── styling ──────────────────────────────────────────────────────────────────

const BG: u32 = 0x0e1117;
const CARD: u32 = 0x161a22;
const CARD_BORDER: u32 = 0x232835;
const DIVIDER: u32 = 0x1f2430;
const RAISED: u32 = 0x262c3a;
const HOVER: u32 = 0x1a1f29;
const INPUT_BORDER: u32 = 0x2c3242;
/// The pill's accent, so both windows read as one app.
const ACCENT: u32 = 0x8da2fb;
const TEXT: u32 = 0xe6e9ef;
const MUTED: u32 = 0x8a92a3;
const FAINT: u32 = 0x5b6375;
const RED: u32 = 0xf87171;

type Handler = Box<dyn Fn(&mut SettingsView, &mut Window, &mut Context<SettingsView>)>;

/// A control's mouse-down listener: runs `f` and redraws, and keeps the root
/// from treating the press as a click-away (which unfocuses fields and
/// cancels a capture).
fn on_press(
    cx: &mut Context<SettingsView>,
    f: impl Fn(&mut SettingsView, &mut Window, &mut Context<SettingsView>) + 'static,
) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
    cx.listener(move |view, _: &MouseDownEvent, window, cx| {
        f(view, window, cx);
        cx.stop_propagation();
        cx.notify();
    })
}

fn section(title: &'static str, rows: Vec<AnyElement>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .pl(px(4.0))
                .text_size(px(11.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(MUTED))
                .child(title),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .rounded(px(12.0))
                .bg(rgb(CARD))
                .border_1()
                .border_color(rgb(CARD_BORDER))
                .overflow_hidden()
                .children(rows.into_iter().enumerate().map(move |(i, row)| {
                    div()
                        .when(i > 0, |d| d.border_t_1().border_color(rgb(DIVIDER)))
                        .child(row)
                })),
        )
}

/// A settings row: label (see `label_block`) on the left, control on the right.
fn row(label: Div, control: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .min_h(px(40.0))
        .px(px(14.0))
        .py(px(6.0))
        .child(label)
        .child(div().flex_none().child(control))
}

/// A row's label with an optional second line in `detail_color`.
fn label_block(
    label: impl Into<SharedString>,
    detail: Option<SharedString>,
    detail_color: u32,
) -> Div {
    div()
        .flex()
        .flex_col()
        .min_w_0()
        .gap(px(2.0))
        .child(
            div()
                .text_size(px(13.5))
                .text_color(rgb(TEXT))
                .truncate()
                .child(label.into()),
        )
        .when_some(detail, |d, detail| {
            d.child(
                div()
                    .text_size(px(12.0))
                    .line_height(px(16.0))
                    .text_color(rgb(detail_color))
                    .child(detail),
            )
        })
}

fn switch(on: bool) -> Div {
    div()
        .flex_none()
        .w(px(34.0))
        .h(px(20.0))
        .p(px(2.0))
        .rounded_full()
        .bg(rgb(if on { ACCENT } else { 0x343a49 }))
        .child(
            div()
                .size(px(16.0))
                .rounded_full()
                .bg(rgb(if on { 0x0e1117 } else { 0xc9ced8 }))
                .when(on, |d| d.ml(px(14.0))),
        )
}

fn radio(selected: bool) -> Div {
    div()
        .flex_none()
        .size(px(16.0))
        .rounded_full()
        .border_1()
        .border_color(rgb(if selected { ACCENT } else { 0x4a5163 }))
        .flex()
        .items_center()
        .justify_center()
        .when(selected, |d| {
            d.child(div().size(px(8.0)).rounded_full().bg(rgb(ACCENT)))
        })
}

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

fn segmented(
    id: &'static str,
    options: Vec<(SharedString, bool, Handler)>,
    cx: &mut Context<SettingsView>,
) -> Div {
    div()
        .flex()
        .flex_row()
        .p(px(2.0))
        .gap(px(2.0))
        .rounded(px(8.0))
        .bg(rgb(BG))
        .border_1()
        .border_color(rgb(INPUT_BORDER))
        .children(
            options
                .into_iter()
                .enumerate()
                .map(|(i, (label, selected, handler))| {
                    div()
                        .id(SharedString::from(format!("{id}-{i}")))
                        .flex_1()
                        .h(px(26.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .text_size(px(12.5))
                        .whitespace_nowrap()
                        .cursor_pointer()
                        .when(selected, |d| {
                            d.bg(rgb(RAISED))
                                .text_color(rgb(TEXT))
                                .font_weight(FontWeight::MEDIUM)
                        })
                        .when(!selected, |d| {
                            d.text_color(rgb(MUTED)).hover(|s| s.text_color(rgb(TEXT)))
                        })
                        .on_mouse_down(MouseButton::Left, on_press(cx, handler))
                        .child(label)
                }),
        )
}

impl SettingsView {
    fn text_input(
        &self,
        f: Field,
        placeholder: &'static str,
        width: Option<f32>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let active = self.active == Some(f);
        let masked = f == Field::ApiKey && !self.reveal_key;
        let value = self.text(f);
        let shown = if masked {
            "•".repeat(value.chars().count().min(16))
        } else {
            value.to_string()
        };
        div()
            .id(match f {
                Field::ApiKey => "f-key",
                Field::Model => "f-model",
                Field::Language => "f-lang",
            })
            .map(|d| match width {
                Some(w) => d.w(px(w)),
                None => d.w_full(),
            })
            .h(px(30.0))
            .px(px(10.0))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .rounded(px(8.0))
            .bg(rgb(BG))
            .border_1()
            .border_color(rgb(if active { ACCENT } else { INPUT_BORDER }))
            .text_size(px(13.0))
            .cursor_text()
            .on_mouse_down(
                MouseButton::Left,
                on_press(cx, move |view, _, cx| {
                    view.active = Some(f);
                    view.cancel_capture(cx);
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .when(shown.is_empty() && !active, |d| {
                        d.child(div().text_color(rgb(FAINT)).child(placeholder))
                    })
                    .when(!shown.is_empty(), |d| {
                        d.child(
                            div()
                                .text_color(rgb(TEXT))
                                .whitespace_nowrap()
                                .child(SharedString::from(shown)),
                        )
                    })
                    .when(active, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .w(px(1.5))
                                .h(px(16.0))
                                .ml(px(1.0))
                                .bg(rgb(ACCENT)),
                        )
                    }),
            )
            .when(f == Field::ApiKey && !value.is_empty(), |d| {
                d.child(
                    div()
                        .id("reveal")
                        .flex_none()
                        .text_size(px(12.0))
                        .text_color(rgb(MUTED))
                        .cursor_pointer()
                        .hover(|s| s.text_color(rgb(TEXT)))
                        .on_mouse_down(
                            MouseButton::Left,
                            on_press(cx, |view, _, _| view.reveal_key = !view.reveal_key),
                        )
                        .child(if self.reveal_key { "Hide" } else { "Show" }),
                )
            })
    }

    fn keycap(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let capturing = self.capture.is_some();
        div()
            .id("keycap")
            .h(px(30.0))
            .min_w(px(96.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(8.0))
            .border_1()
            .border_b_2()
            .text_size(px(13.0))
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .when(capturing, |d| {
                d.bg(rgba(0x8da2fb1f))
                    .border_color(rgb(ACCENT))
                    .text_color(rgb(ACCENT))
                    .child("Press a key…")
            })
            .when(!capturing, |d| {
                d.bg(rgb(RAISED))
                    .border_color(rgb(0x363d4e))
                    .text_color(rgb(TEXT))
                    .hover(|s| s.border_color(rgb(ACCENT)))
                    .child(SharedString::from(hotkey::key_label(&self.cfg.hotkey)))
            })
            .on_mouse_down(
                MouseButton::Left,
                on_press(cx, |view, _, cx| {
                    if view.capture.is_some() {
                        view.cancel_capture(cx);
                    } else {
                        view.start_capture(cx);
                    }
                }),
            )
    }

    /// A whole-row toggle: click anywhere on it.
    fn switch_row(
        &self,
        label: &'static str,
        detail: Option<&'static str>,
        on: bool,
        flag: Flag,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        row(
            label_block(label, detail.map(SharedString::from), MUTED),
            switch(on),
        )
        .id(label)
        .cursor_pointer()
        .hover(|s| s.bg(rgb(HOVER)))
        .on_mouse_down(
            MouseButton::Left,
            on_press(cx, move |view, _, _| view.set_flag(flag)),
        )
        .into_any_element()
    }

    fn shortcut_section(&self, cx: &mut Context<Self>) -> Div {
        let hold = self.cfg.mode != "toggle";
        let key_detail = match (self.capture.is_some(), self.capture_note) {
            (true, _) => Some("Esc to cancel".into()),
            (false, Some(note)) => Some(note.into()),
            (false, None) => None,
        };
        let note_color = if self.capture.is_none() && self.capture_note.is_some() {
            RED
        } else {
            MUTED
        };
        let key_row = row(label_block("Key", key_detail, note_color), self.keycap(cx));
        let mode = segmented(
            "mode",
            vec![
                (
                    "Hold".into(),
                    hold,
                    Box::new(|v: &mut Self, _: &mut Window, _: &mut Context<Self>| {
                        v.cfg.mode = "hold".into();
                        v.persist();
                    }),
                ),
                (
                    "Toggle".into(),
                    !hold,
                    Box::new(|v: &mut Self, _: &mut Window, _: &mut Context<Self>| {
                        v.cfg.mode = "toggle".into();
                        v.persist();
                    }),
                ),
            ],
            cx,
        );
        section(
            "SHORTCUT",
            vec![
                key_row.into_any_element(),
                row(
                    label_block("Mode", None, MUTED),
                    div().w(px(160.0)).child(mode),
                )
                .into_any_element(),
            ],
        )
    }

    fn mic_section(&self, cx: &mut Context<Self>) -> Div {
        let rows = mic_rows(self.cfg.mic.as_deref(), &self.mics)
            .into_iter()
            .enumerate()
            .map(|(i, row)| {
                let MicRow {
                    value,
                    label,
                    detail,
                    selected,
                } = row;
                div()
                    .id(SharedString::from(format!("mic-{i}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .h(px(38.0))
                    .px(px(14.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(HOVER)))
                    .on_mouse_down(
                        MouseButton::Left,
                        on_press(cx, move |view, _, _| {
                            view.cfg.mic = value.clone();
                            view.persist();
                        }),
                    )
                    .child(radio(selected))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(13.5))
                            .text_color(rgb(TEXT))
                            .child(label),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .text_size(px(12.5))
                            .text_color(rgb(MUTED))
                            .truncate()
                            .child(detail),
                    )
                    .into_any_element()
            })
            .collect();
        section("MICROPHONE", rows)
    }

    fn transcription_section(&self, cx: &mut Context<Self>) -> Div {
        let key_row = if self.env_key {
            row(
                label_block("API key", None, MUTED),
                div()
                    .text_size(px(12.5))
                    .text_color(rgb(MUTED))
                    .child("From OPENROUTER_API_KEY"),
            )
        } else {
            let missing = self.text(Field::ApiKey).trim().is_empty();
            row(
                label_block("API key", None, MUTED).when(missing, |d| {
                    d.child(
                        div()
                            .id("get-key")
                            .text_size(px(12.0))
                            .text_color(rgb(ACCENT))
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .on_mouse_down(
                                MouseButton::Left,
                                on_press(cx, |_, _, cx| cx.open_url(KEYS_URL)),
                            )
                            .child("Get a key ↗"),
                    )
                }),
                self.text_input(Field::ApiKey, "sk-or-…", Some(220.0), cx),
            )
        };

        let mut model_options: Vec<(SharedString, bool, Handler)> = MODELS
            .iter()
            .map(|&(label, slug)| {
                let selected = !self.custom_model && self.cfg.model == slug;
                let handler: Handler =
                    Box::new(move |v: &mut Self, _: &mut Window, _: &mut Context<Self>| {
                        v.custom_model = false;
                        v.active = None;
                        v.cfg.model = slug.to_string();
                        v.persist();
                    });
                (label.into(), selected, handler)
            })
            .collect();
        model_options.push((
            "Custom".into(),
            self.custom_model,
            Box::new(|v: &mut Self, _: &mut Window, _: &mut Context<Self>| {
                v.custom_model = true;
                v.active = Some(Field::Model);
            }),
        ));
        let model_row = div()
            .flex()
            .flex_col()
            .child(row(
                label_block("Model", None, MUTED),
                div()
                    .w(px(300.0))
                    .child(segmented("model", model_options, cx)),
            ))
            .when(self.custom_model, |d| {
                d.child(div().px(px(14.0)).pb(px(10.0)).child(self.text_input(
                    Field::Model,
                    "provider/model",
                    None,
                    cx,
                )))
            });

        section(
            "TRANSCRIPTION",
            vec![
                key_row.into_any_element(),
                model_row.into_any_element(),
                row(
                    label_block("Language", None, MUTED),
                    self.text_input(Field::Language, "Auto-detect", Some(120.0), cx),
                )
                .into_any_element(),
                self.switch_row(
                    "Clean up transcript",
                    Some("Fix punctuation, filler words, and misheard terms"),
                    self.cfg.cleanup,
                    Flag::Cleanup,
                    cx,
                ),
            ],
        )
    }

    fn output_section(&self, cx: &mut Context<Self>) -> Div {
        section(
            "OUTPUT",
            vec![
                self.switch_row(
                    "Type into focused window",
                    Some("The transcript is always copied to the clipboard too"),
                    self.cfg.type_text,
                    Flag::TypeText,
                    cx,
                ),
                self.switch_row("Sound cues", None, self.cfg.beeps, Flag::Beeps, cx),
            ],
        )
    }

    fn header(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(px(17.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child("Dictation"),
                    )
                    .child(div().text_size(px(12.5)).text_color(rgb(MUTED)).child(
                        SharedString::from(tray::hint(&self.cfg.hotkey, &self.cfg.mode)),
                    )),
            )
            .child(
                div()
                    .id("done")
                    .h(px(30.0))
                    .px(px(14.0))
                    .flex()
                    .items_center()
                    .rounded(px(8.0))
                    .bg(rgb(ACCENT))
                    .text_size(px(13.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(BG))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0xa3b4fc)))
                    .on_mouse_down(
                        MouseButton::Left,
                        on_press(cx, |_, window, _| window.remove_window()),
                    )
                    .child("Done"),
            )
    }

    /// An unreadable config.toml or a failed save, until it's resolved.
    fn banner(&self) -> Option<Div> {
        let text = match (&self.load_error, &self.save_error) {
            (Some(e), _) => format!(
                "Can't read config.toml, so changes aren't saved. Fix it, then come back to this window.\n{e}"
            ),
            (None, Some(e)) => e.clone(),
            (None, None) => return None,
        };
        Some(
            div()
                .px(px(12.0))
                .py(px(9.0))
                .rounded(px(10.0))
                .bg(rgba(0xf8717126))
                .border_1()
                .border_color(rgba(0xf8717166))
                .text_size(px(12.5))
                .line_height(px(17.0))
                .text_color(rgb(0xfecaca))
                .child(SharedString::from(text)),
        )
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Settings")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::unfocus))
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .child(
                div()
                    .id("settings-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .px(px(20.0))
                            .pt(px(14.0))
                            .pb(px(14.0))
                            .child(self.header(cx))
                            .children(self.banner())
                            .child(self.shortcut_section(cx))
                            .child(self.mic_section(cx))
                            .child(self.transcription_section(cx))
                            .child(self.output_section(cx)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evdev::KeyCode;
    use gpui::{TestAppContext, VisualTestContext};
    use std::path::PathBuf;
    use std::sync::mpsc::channel;

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
    #[gpui::test]
    fn coming_back_picks_up_outside_edits(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_at(dir.path().join("config.toml"));
        cfg.save().unwrap();
        let (tx, _reloads) = channel();
        let (view, cx) =
            cx.add_window_view(|window, cx| SettingsView::new(cfg.clone(), tx, window, cx));
        let come_back = |cx: &mut VisualTestContext| {
            cx.update(|window, _| window.activate_window());
            cx.run_until_parked();
        };
        come_back(cx);

        cx.deactivate_window();
        Config {
            model: "custom/model".into(),
            ..cfg.clone()
        }
        .save()
        .unwrap();
        come_back(cx);
        view.update(cx, |view, _| view.set_flag(Flag::Beeps));
        let saved = cfg.reread().unwrap();
        assert_eq!((saved.model.as_str(), saved.beeps), ("custom/model", false));

        cx.deactivate_window();
        std::fs::write(&cfg.path, "beeps = [").unwrap();
        come_back(cx);
        view.update(cx, |view, _| view.set_flag(Flag::Beeps));
        assert_eq!(std::fs::read_to_string(&cfg.path).unwrap(), "beeps = [");
        assert!(view.read_with(cx, |view, _| view.banner().is_some()));
    }

    /// A pressed key becomes the hotkey (saved and reloaded at once) unless it
    /// types text; silence ends the wait.
    #[gpui::test]
    fn captured_key_is_saved_unless_it_types(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = config_at(path.clone());
        let (tx, reloads) = channel();
        let window = cx.add_window(|window, cx| SettingsView::new(cfg, tx, window, cx));
        // Poll once with `key` pressed (or none) after `waited`.
        let poll = |cx: &mut TestAppContext, key: Option<KeyCode>, waited: Duration| {
            let (press, key_capture) = hotkey::KeyCapture::detached();
            if let Some(key) = key {
                press.send(key).unwrap();
            }
            window
                .update(cx, |view, _, cx| {
                    view.capture = Some(Capture {
                        key: key_capture,
                        started: Instant::now() - waited,
                    });
                    assert!(!view.poll_capture(cx), "capture should be over");
                    assert!(view.capture.is_none());
                    (view.cfg.hotkey.clone(), view.capture_note)
                })
                .unwrap()
        };

        let (hotkey, note) = poll(cx, Some(KeyCode::KEY_A), Duration::ZERO);
        assert_eq!(hotkey, "KEY_RIGHTCTRL");
        assert!(note.is_some_and(|n| n.contains("types text")));
        assert!(!path.exists() && reloads.try_recv().is_err());

        let (_, note) = poll(cx, None, CAPTURE_TIMEOUT);
        assert!(note.is_some_and(|n| n.contains("No key")));
        assert!(!path.exists() && reloads.try_recv().is_err());

        let (hotkey, note) = poll(cx, Some(KeyCode::KEY_F13), Duration::ZERO);
        assert_eq!((hotkey.as_str(), note), ("KEY_F13", None));
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains(r#"hotkey = "KEY_F13""#), "{saved}");
        assert_eq!(reloads.try_recv(), Ok(Command::Reload));
    }

    #[test]
    fn ctrl_backspace_handles_unicode_word_boundaries() {
        for (before, after) in [
            ("foo\u{2003}bar", "foo\u{2003}"),
            ("foo\u{a0}bar  ", "foo\u{a0}"),
            ("中文\u{3000}🙂", "中文\u{3000}"),
            ("provider/model", "provider/"),
            ("provider/", ""),
            ("org/provider/", "org/"),
            ("KEY_RIGHTCTRL", "KEY_"),
            ("🙂", ""),
            ("   ", ""),
        ] {
            let mut value = before.to_string();
            delete_previous_word(&mut value);
            assert_eq!(value, after, "input: {before:?}");
        }
    }
}
