//! Settings window — a second gpui window so config.toml never needs editing.

use crate::audio;
use crate::config::Config;
use crate::hotkey;
use crate::ipc::Command;
use gpui::{
    App, Bounds, Context, FocusHandle, Focusable, KeyDownEvent, MouseButton, MouseDownEvent,
    SharedString, TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb,
    rgba,
};
use std::sync::mpsc::Sender;

const MODEL_PRESETS: &[&str] = &[
    "fish-audio/transcribe-1",
    "openai/whisper-large-v3",
    "openai/whisper-1",
];

const HOTKEY_PRESETS: &[&str] = &[
    "KEY_RIGHTCTRL",
    "KEY_LEFTCTRL",
    "KEY_CAPSLOCK",
    "KEY_SCROLLLOCK",
    "KEY_F13",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    ApiKey,
    Model,
    Language,
    Hotkey,
}

pub struct SettingsView {
    focus_handle: FocusHandle,
    tx: Sender<Command>,
    // Editable working copy of the config.
    api_key: String,
    model: String,
    language: String,
    mode_hold: bool,
    hotkey: String,
    mic: Option<String>,
    mic_devices: Vec<String>,
    type_text: bool,
    beeps: bool,
    cleanup: bool,
    cleanup_model: String,
    reveal_key: bool,
    active: Option<Field>,
    message: Option<(String, bool)>, // (text, is_error)
}

/// Open the settings window. `tx` lets Save notify the pill to reload config.
pub fn open<V: 'static + Render>(cx: &mut Context<V>, tx: Sender<Command>) {
    let dims = gpui::size(px(460.0), px(760.0));
    let bounds = Bounds::centered(None, dims, cx);
    let tx2 = tx;
    cx.open_window(
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
            let view = cx.new(|cx| SettingsView::new(tx2, cx));
            window.set_window_title("Dictation Settings");
            window.focus(&view.read(cx).focus_handle);
            view
        },
    )
    .ok();
}

impl SettingsView {
    fn new(tx: Sender<Command>, cx: &mut Context<Self>) -> Self {
        let cfg = Config::load().unwrap_or_else(|_| Config {
            api_key: None,
            model: crate::config::DEFAULT_MODEL.into(),
            language: None,
            mode: "hold".into(),
            hotkey: crate::config::DEFAULT_HOTKEY.into(),
            mic: None,
            type_text: true,
            beeps: true,
            cleanup: true,
            cleanup_model: crate::config::DEFAULT_CLEANUP_MODEL.into(),
            path: crate::config::Config::default_path(),
        });
        Self {
            focus_handle: cx.focus_handle(),
            tx,
            api_key: cfg.api_key.unwrap_or_default(),
            model: cfg.model,
            language: cfg.language.unwrap_or_default(),
            mode_hold: cfg.mode != "toggle",
            hotkey: cfg.hotkey,
            mic: cfg.mic,
            mic_devices: audio::input_device_names(),
            type_text: cfg.type_text,
            beeps: cfg.beeps,
            cleanup: cfg.cleanup,
            cleanup_model: cfg.cleanup_model,
            reveal_key: false,
            active: None,
            message: None,
        }
    }

    fn field_mut(&mut self, f: Field) -> &mut String {
        match f {
            Field::ApiKey => &mut self.api_key,
            Field::Model => &mut self.model,
            Field::Language => &mut self.language,
            Field::Hotkey => &mut self.hotkey,
        }
    }

    fn field(&self, f: Field) -> &String {
        match f {
            Field::ApiKey => &self.api_key,
            Field::Model => &self.model,
            Field::Language => &self.language,
            Field::Hotkey => &self.hotkey,
        }
    }

    fn activate(
        f: Field,
    ) -> impl Fn(&mut Self, &MouseDownEvent, &mut Window, &mut Context<Self>) + 'static {
        move |view, _, _, cx| {
            view.active = Some(f);
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn unfocus(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.active = None;
        cx.notify();
    }

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "escape" => {
                if self.active.is_some() {
                    self.active = None;
                    cx.notify();
                } else {
                    window.remove_window();
                }
                return;
            }
            "tab" => {
                let order = [Field::ApiKey, Field::Model, Field::Language, Field::Hotkey];
                self.active = Some(match self.active {
                    None => order[0],
                    Some(cur) => {
                        let i = order.iter().position(|f| *f == cur).unwrap_or(0);
                        order[(i + 1) % order.len()]
                    }
                });
                cx.notify();
                return;
            }
            _ => {}
        }
        let Some(field) = self.active else { return };
        if ks.key == "backspace" {
            if ks.modifiers.control {
                // ctrl+backspace: delete to previous word boundary
                let v = self.field_mut(field);
                let trimmed = v.trim_end();
                let cut = trimmed
                    .rfind(|c: char| c.is_whitespace() || c == '/' || c == '_' || c == '-')
                    .map(|i| i + 1)
                    .unwrap_or(0);
                v.truncate(cut);
            } else {
                self.field_mut(field).pop();
            }
        } else if ks.modifiers.control && ks.key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                let clean: String = text.chars().filter(|c| !c.is_control()).collect();
                self.field_mut(field).push_str(clean.trim());
            }
        } else if let Some(ch) = &ks.key_char {
            if !ks.modifiers.control && !ks.modifiers.platform {
                self.field_mut(field).push_str(ch);
            }
        }
        cx.notify();
    }

    fn pick_model(
        m: &'static str,
    ) -> impl Fn(&mut Self, &MouseDownEvent, &mut Window, &mut Context<Self>) + 'static {
        move |view, _, _, cx| {
            view.model = m.to_string();
            view.active = None;
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn pick_hotkey(
        k: &'static str,
    ) -> impl Fn(&mut Self, &MouseDownEvent, &mut Window, &mut Context<Self>) + 'static {
        move |view, _, _, cx| {
            view.hotkey = k.to_string();
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn pick_mic(
        m: Option<String>,
    ) -> impl Fn(&mut Self, &MouseDownEvent, &mut Window, &mut Context<Self>) + 'static {
        move |view, _, _, cx| {
            view.mic = m.clone();
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn set_mode(
        hold: bool,
    ) -> impl Fn(&mut Self, &MouseDownEvent, &mut Window, &mut Context<Self>) + 'static {
        move |view, _, _, cx| {
            view.mode_hold = hold;
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn toggle_flag(
        which: u8,
    ) -> impl Fn(&mut Self, &MouseDownEvent, &mut Window, &mut Context<Self>) + 'static {
        move |view, _, _, cx| {
            match which {
                0 => view.type_text = !view.type_text,
                1 => view.beeps = !view.beeps,
                2 => view.cleanup = !view.cleanup,
                _ => {}
            }
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn toggle_reveal(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.reveal_key = !self.reveal_key;
        cx.stop_propagation();
        cx.notify();
    }

    fn save(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        // Validate the hotkey before persisting.
        if let Err(e) = hotkey::parse_key(self.hotkey.trim()) {
            self.message = Some((format!("{e:#}"), true));
            cx.notify();
            return;
        }
        let cfg = Config {
            api_key: Some(self.api_key.trim().to_string()).filter(|s| !s.is_empty()),
            model: if self.model.trim().is_empty() {
                crate::config::DEFAULT_MODEL.to_string()
            } else {
                self.model.trim().to_string()
            },
            language: Some(self.language.trim().to_string()).filter(|s| !s.is_empty()),
            mode: if self.mode_hold {
                "hold".into()
            } else {
                "toggle".into()
            },
            hotkey: self.hotkey.trim().to_string(),
            mic: self.mic.clone(),
            type_text: self.type_text,
            beeps: self.beeps,
            cleanup: self.cleanup,
            cleanup_model: self.cleanup_model.clone(),
            path: Config::default_path(),
        };
        match cfg.save() {
            Ok(()) => {
                let _ = self.tx.send(Command::Reload);
                self.message = Some(("Saved.".into(), false));
            }
            Err(e) => self.message = Some((format!("{e:#}"), true)),
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn close(&mut self, _: &MouseDownEvent, window: &mut Window, _: &mut Context<Self>) {
        window.remove_window();
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// ── styling helpers ──────────────────────────────────────────────────────────

const BG: u32 = 0x10141c;
const SURFACE: u32 = 0x171c27;
const BORDER: u32 = 0x2a3040;
const ACCENT: u32 = 0x6e8cff;
const TEXT: u32 = 0xe2e6ee;
const DIM: u32 = 0x8b93a3;
const GREEN: u32 = 0x4ade80;
const RED: u32 = 0xef4444;

fn section_label(text: &'static str) -> gpui::Div {
    div().text_xs().text_color(rgb(DIM)).mb_1().child(text)
}

/// Device names can be long — cap chip labels so they don't overflow.
fn truncate_label(s: &str) -> String {
    const MAX: usize = 40;
    if s.chars().count() > MAX {
        format!("{}…", s.chars().take(MAX - 1).collect::<String>())
    } else {
        s.to_string()
    }
}

impl SettingsView {
    fn text_field(
        &self,
        f: Field,
        placeholder: &'static str,
        masked: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let active = self.active == Some(f);
        let value = self.field(f);
        let shown = if masked && !self.reveal_key {
            if value.is_empty() {
                String::new()
            } else {
                "•".repeat(value.len().min(24))
            }
        } else {
            value.clone()
        };
        let mut el = div()
            .id(match f {
                Field::ApiKey => "f-key",
                Field::Model => "f-model",
                Field::Language => "f-lang",
                Field::Hotkey => "f-hotkey",
            })
            .flex()
            .flex_row()
            .items_center()
            .h(px(32.0))
            .px_3()
            .rounded_lg()
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(if active { rgb(ACCENT) } else { rgb(BORDER) })
            .cursor_text()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::activate(f)))
            .child(if shown.is_empty() {
                div().text_color(rgb(0x4b5265)).child(placeholder)
            } else {
                div().text_color(rgb(TEXT)).child(SharedString::from(shown))
            });
        if active {
            el = el.child(div().w(px(1.5)).h(px(18.0)).ml(px(1.0)).bg(rgb(ACCENT)));
        }
        if masked {
            el = el.child(
                div()
                    .id("reveal")
                    .ml_auto()
                    .pl_2()
                    .text_xs()
                    .text_color(rgb(DIM))
                    .cursor_pointer()
                    .hover(|s| s.text_color(rgb(TEXT)))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::toggle_reveal))
                    .child(if self.reveal_key { "hide" } else { "show" }),
            );
        }
        el
    }

    fn chip(
        &self,
        id: &str,
        label: &str,
        selected: bool,
        handler: impl Fn(&mut Self, &MouseDownEvent, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(SharedString::from(format!("chip-{id}")))
            .px_2p5()
            .h(px(24.0))
            .flex()
            .items_center()
            .rounded_md()
            .text_xs()
            .cursor_pointer()
            .border_1()
            .border_color(if selected { rgb(ACCENT) } else { rgb(BORDER) })
            .bg(if selected {
                rgba(0x6e8cff22)
            } else {
                rgba(0x00000000)
            })
            .text_color(if selected { rgb(ACCENT) } else { rgb(DIM) })
            .hover(|s| s.border_color(rgb(ACCENT)))
            .on_mouse_down(MouseButton::Left, cx.listener(handler))
            .child(SharedString::from(label.to_string()))
    }

    fn toggle(
        &self,
        label: &'static str,
        desc: &'static str,
        on: bool,
        which: u8,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(SharedString::from(format!("tog-{label}")))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .py_0p5()
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::toggle_flag(which)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_sm().text_color(rgb(TEXT)).child(label))
                    .child(div().text_xs().text_color(rgb(DIM)).child(desc)),
            )
            .child(
                div()
                    .w(px(36.0))
                    .h(px(20.0))
                    .rounded_full()
                    .bg(if on { rgb(ACCENT) } else { rgb(0x3a4152) })
                    .p_px()
                    .child(
                        div()
                            .size(px(18.0))
                            .rounded_full()
                            .bg(rgb(0xffffff))
                            .ml(if on { px(16.0) } else { px(0.0) }),
                    ),
            )
    }
}

impl SettingsView {
    /// Scrollable body — everything above the pinned footer.
    fn scroll_content(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        // Keep a configured-but-currently-unplugged device selectable.
        let mut mic_names = self.mic_devices.clone();
        if let Some(m) = &self.mic {
            if !mic_names.contains(m) {
                mic_names.push(m.clone());
            }
        }
        div()
            .id("settings-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .p_4()
            .gap_2()
            .child(
                div()
                    .text_lg()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Dictation Settings"),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .child(section_label("OPENROUTER"))
                    .child(div().text_xs().text_color(rgb(DIM)).child("API key"))
                    .child(self.text_field(Field::ApiKey, "sk-or-…", true, cx))
                    .child(div().text_xs().text_color(rgb(DIM)).child("Model"))
                    .child(self.text_field(Field::Model, "provider/model", false, cx))
                    .child(
                        div().flex().flex_row().flex_wrap().gap_1p5().children(
                            MODEL_PRESETS.iter().map(|m| {
                                self.chip(m, m, self.model == *m, Self::pick_model(m), cx)
                            }),
                        ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(DIM))
                            .child("Language (blank = auto)"),
                    )
                    .child(self.text_field(Field::Language, "en", false, cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .child(section_label("DICTATION"))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_1p5()
                            .child(self.chip(
                                "hold",
                                "Hold to talk",
                                self.mode_hold,
                                Self::set_mode(true),
                                cx,
                            ))
                            .child(self.chip(
                                "toggle",
                                "Toggle",
                                !self.mode_hold,
                                Self::set_mode(false),
                                cx,
                            )),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(DIM))
                            .child("Hotkey (evdev name)"),
                    )
                    .child(self.text_field(Field::Hotkey, "KEY_RIGHTCTRL", false, cx))
                    .child(
                        div().flex().flex_row().flex_wrap().gap_1p5().children(
                            HOTKEY_PRESETS.iter().map(|k| {
                                self.chip(k, k, self.hotkey == *k, Self::pick_hotkey(k), cx)
                            }),
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .child(section_label("MICROPHONE"))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap_1p5()
                            .child(self.chip(
                                "default",
                                "System default",
                                self.mic.is_none(),
                                Self::pick_mic(None),
                                cx,
                            ))
                            .children(mic_names.iter().map(|name| {
                                self.chip(
                                    name,
                                    &truncate_label(name),
                                    self.mic.as_deref() == Some(name.as_str()),
                                    Self::pick_mic(Some(name.clone())),
                                    cx,
                                )
                            })),
                    )
                    .when(self.mic_devices.is_empty(), |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(rgb(DIM))
                                .child("No input devices detected."),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(section_label("OUTPUT"))
                    .child(self.toggle(
                        "Type into focused window",
                        "Virtual keyboard via uinput; falls back to clipboard",
                        self.type_text,
                        0,
                        cx,
                    ))
                    .child(self.toggle(
                        "Sound cues",
                        "Short beeps on record start/stop",
                        self.beeps,
                        1,
                        cx,
                    ))
                    .child(self.toggle(
                        "Refine transcript",
                        "Correct likely recognition errors, punctuation, and filler words",
                        self.cleanup,
                        2,
                        cx,
                    )),
            )
    }

    /// Pinned footer — status message plus Close/Save, always visible.
    fn footer(&self, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_4()
            .pb_4()
            .pt_3()
            .border_t_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(match &self.message {
                        Some((_, true)) => RED,
                        Some((_, false)) => GREEN,
                        None => DIM,
                    }))
                    .child(SharedString::from(match &self.message {
                        Some((m, _)) => m.clone(),
                        None => "Ctrl+V to paste into fields.".to_string(),
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        div()
                            .id("close")
                            .px_4()
                            .h(px(32.0))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .text_sm()
                            .text_color(rgb(DIM))
                            .cursor_pointer()
                            .hover(|s| s.text_color(rgb(TEXT)))
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::close))
                            .child("Close"),
                    )
                    .child(
                        div()
                            .id("save")
                            .px_4()
                            .h(px(32.0))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .text_sm()
                            .bg(rgb(ACCENT))
                            .text_color(rgb(0xffffff))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0x7d9bff)))
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::save))
                            .child("Save"),
                    ),
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
            .flex()
            .flex_col()
            .child(self.scroll_content(cx))
            .child(self.footer(cx))
    }
}
