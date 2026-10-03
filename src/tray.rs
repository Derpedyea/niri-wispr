//! StatusNotifierItem tray icon — how Dictation shows up in Noctalia's bar
//! (and Waybar, KDE, …). Click opens Settings; the menu adds Quit. While
//! recording the item asks for attention and shows a red mic.

use crate::ipc::Command;
use ksni::blocking::{Handle, TrayMethods};
use std::sync::mpsc::Sender;

pub struct Tray {
    handle: Handle<Item>,
}

struct Item {
    tx: Sender<Command>,
    recording: bool,
    /// e.g. "Hold Right Ctrl to dictate".
    hint: String,
}

impl Item {
    fn send(&self, cmd: Command) {
        if let Err(e) = self.tx.send(cmd) {
            eprintln!("tray: send {cmd:?} failed: {e}");
        }
    }
}

impl ksni::Tray for Item {
    fn id(&self) -> String {
        "dictationapp".into()
    }

    fn title(&self) -> String {
        "Dictation".into()
    }

    // Symbolic, so Noctalia tints it like its own bar glyphs.
    fn icon_name(&self) -> String {
        "audio-input-microphone-symbolic".into()
    }

    // Hosts show this instead of the icon while the status needs attention.
    // A pixmap, not a name: Noctalia tints every symbolic icon to its bar
    // color, so a themed icon couldn't be red.
    fn attention_icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![red_mic(64)]
    }

    fn status(&self) -> ksni::Status {
        if self.recording {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: if self.recording {
                "Dictation — listening"
            } else {
                "Dictation"
            }
            .into(),
            description: self.hint.clone(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Command::Settings);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Settings…".into(),
                activate: Box::new(|item: &mut Self| item.send(Command::Settings)),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Quit Dictation".into(),
                activate: Box::new(|item: &mut Self| item.send(Command::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

impl Tray {
    /// Register the icon. It waits for a tray host rather than failing, since
    /// niri may start us before the bar is up, and re-registers whenever the
    /// bar restarts. Without any host the icon simply never shows.
    pub fn spawn(tx: Sender<Command>, hint: String) -> anyhow::Result<Tray> {
        let item = Item {
            tx,
            recording: false,
            hint,
        };
        let handle = item.assume_sni_available(true).spawn()?;
        Ok(Tray { handle })
    }

    pub fn set_recording(&self, recording: bool) {
        self.update(|item| item.recording = recording);
    }

    pub fn set_hint(&self, hint: String) {
        self.update(|item| item.hint = hint);
    }

    fn update(&self, change: impl FnOnce(&mut Item)) {
        if self.handle.update(change).is_none() {
            eprintln!("tray: service stopped; icon not updated");
        }
    }
}

/// What the tray tooltip and Settings say about using the hotkey.
pub fn hint(hotkey: &str, mode: &str) -> String {
    let key = crate::hotkey::key_label(hotkey);
    if mode == "toggle" {
        format!("Tap {key} to start and stop dictating")
    } else {
        format!("Hold {key} and speak")
    }
}

/// The symbolic mic, drawn in red at `size`×`size` (ARGB32, as SNI wants).
/// Shapes are signed distances on a 16-unit grid like the symbolic icon's.
fn red_mic(size: i32) -> ksni::Icon {
    let segment = |(px, py): (f32, f32), (ax, ay): (f32, f32), (bx, by): (f32, f32)| {
        let (dx, dy) = (bx - ax, by - ay);
        let t = (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
        (px - ax - t * dx).hypot(py - ay - t * dy)
    };
    let distance = |p: (f32, f32)| {
        let capsule = segment(p, (8.0, 3.75), (8.0, 7.25)) - 2.75;
        // The U around the capsule: its lower half, with round ends.
        let (cx, cy, r) = (8.0, 7.5, 4.75);
        let arc = if p.1 >= cy {
            ((p.0 - cx).hypot(p.1 - cy) - r).abs()
        } else {
            (p.0 - (cx - r))
                .hypot(p.1 - cy)
                .min((p.0 - (cx + r)).hypot(p.1 - cy))
        } - 0.75;
        let stem = segment(p, (8.0, 12.25), (8.0, 14.25)) - 0.75;
        let base = segment(p, (5.5, 14.25), (10.5, 14.25)) - 0.75;
        capsule.min(arc).min(stem).min(base)
    };
    let scale = size as f32 / 16.0;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let p = ((x as f32 + 0.5) / scale, (y as f32 + 0.5) / scale);
            // One pixel of anti-aliasing along the edge.
            let alpha = (0.5 - distance(p) * scale).clamp(0.0, 1.0);
            data.extend([(alpha * 255.0).round() as u8, 0xef, 0x44, 0x44]);
        }
    }
    ksni::Icon {
        width: size,
        height: size,
        data,
    }
}
