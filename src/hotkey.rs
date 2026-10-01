use anyhow::{Context, Result};
use evdev::{Device, EventType, InputEvent, KeyCode};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::ipc::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Hold the key to record, release to transcribe.
    Hold,
    /// Tap the key to toggle recording on/off.
    Toggle,
}

/// One listener event, reduced to what the hotkey logic needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Input {
    HotkeyDown,
    HotkeyUp,
    /// Any other keyboard key pressed.
    OtherKey,
    /// The device vanished (unplug, suspend) — a hotkey held on it will
    /// never be released.
    Lost,
}

fn classify(ev: &InputEvent, key: KeyCode) -> Option<Input> {
    if ev.event_type() != EventType::KEY {
        return None;
    }
    // value: 0 = release, 1 = press, 2 = autorepeat
    match (ev.code() == key.code(), ev.value()) {
        (true, 1) => Some(Input::HotkeyDown),
        (true, 0) => Some(Input::HotkeyUp),
        // Keyboard keys only; mouse and touchpad buttons start at BTN_0.
        (false, 1) if ev.code() < KeyCode::BTN_0.code() => Some(Input::OtherKey),
        _ => None,
    }
}

/// Hotkey state shared by every device listener, so overlapping holds from
/// several devices count as one gesture and a chord on any keyboard counts.
#[derive(Default)]
struct Hold {
    /// Devices currently holding the hotkey down.
    down: HashSet<PathBuf>,
    /// Another key was pressed during this hold: it's a shortcut like
    /// RightCtrl+C, not dictation.
    chorded: bool,
}

impl Hold {
    /// Apply one input from `dev`; returns the command to send, if any.
    /// Hold mode starts on press so no speech is lost, and a chord cancels.
    /// Toggle mode waits for the release so a chord never toggles.
    fn apply(&mut self, dev: &Path, input: Input, mode: Mode) -> Option<Command> {
        match input {
            Input::HotkeyDown => {
                let fresh = self.down.is_empty();
                self.down.insert(dev.to_path_buf());
                if !fresh {
                    return None;
                }
                self.chorded = false;
                (mode == Mode::Hold).then_some(Command::Start)
            }
            // A release we never saw pressed (key already down when the
            // listener attached) is ignored.
            Input::HotkeyUp => {
                if !self.down.remove(dev) || !self.down.is_empty() || self.chorded {
                    return None;
                }
                Some(match mode {
                    Mode::Hold => Command::Stop,
                    Mode::Toggle => Command::Toggle,
                })
            }
            Input::OtherKey => {
                if self.down.is_empty() || self.chorded {
                    return None;
                }
                self.chorded = true;
                (mode == Mode::Hold).then_some(Command::Cancel)
            }
            // Nothing will release this hold, so discard rather than transcribe.
            Input::Lost => {
                if !self.down.remove(dev) || !self.down.is_empty() || self.chorded {
                    return None;
                }
                (mode == Mode::Hold).then_some(Command::Cancel)
            }
        }
    }
}

/// Look up a key by name like "KEY_RIGHTCTRL".
pub fn parse_key(name: &str) -> Result<KeyCode> {
    KeyCode::from_str(name).with_context(|| format!("unknown key name '{name}'"))
}

fn is_keyboard(dev: &Device) -> bool {
    dev.supported_keys()
        .map(|keys| keys.contains(KeyCode::KEY_A) && keys.contains(KeyCode::KEY_Z))
        .unwrap_or(false)
}

/// How often to rescan /dev/input for keyboards that appeared after startup
/// (hot-plug, suspend/resume) or became readable once udev applied their ACL.
const RESCAN_INTERVAL: Duration = Duration::from_secs(1);

/// A running set of per-device listener threads, plus a supervisor thread that
/// rescans /dev/input so keyboards appearing later — or re-appearing after
/// removal — are picked up. Drop-style stop via a shared flag so the hotkey
/// can be reconfigured at runtime without restarting the app.
pub struct Watcher {
    stop: Arc<AtomicBool>,
}

impl Watcher {
    /// Spawn a listener thread per keyboard device and keep the set current as
    /// devices come and go. Sends commands per `Hold::apply`.
    pub fn start(key: KeyCode, mode: Mode, tx: Sender<Command>) -> Result<Watcher> {
        let stop = Arc::new(AtomicBool::new(false));
        let watched = Arc::new(Mutex::new(HashSet::new()));
        let hold = Arc::new(Mutex::new(Hold::default()));
        let mut failed = HashSet::new();
        scan_devices(key, mode, &tx, &watched, &hold, &mut failed, &stop);
        eprintln!(
            "hotkey: {key:?} ({mode:?}) on {} device(s)",
            watched.lock().unwrap().len()
        );
        let watched = Arc::clone(&watched);
        let stop2 = Arc::clone(&stop);
        thread::spawn(move || {
            loop {
                thread::sleep(RESCAN_INTERVAL);
                if stop2.load(Ordering::Relaxed) {
                    return;
                }
                scan_devices(key, mode, &tx, &watched, &hold, &mut failed, &stop2);
            }
        });
        Ok(Watcher { stop })
    }

    /// Stop the current listeners and start fresh ones for a new key/mode.
    pub fn restart(&mut self, key: KeyCode, mode: Mode, tx: Sender<Command>) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        // Let in-flight polls observe the flag before we re-enumerate devices.
        thread::sleep(Duration::from_millis(50));
        *self = Self::start(key, mode, tx)?;
        Ok(())
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Open every /dev/input/event* that isn't already watched and spawn a
/// listener for each keyboard found. Devices that can't be opened yet — e.g.
/// udev hasn't applied the user ACL on a fresh node — are retried on the next
/// scan; `failed` keeps that log from repeating every interval. Non-keyboards
/// are re-probed each scan so a node re-created at the same path is classified
/// fresh.
fn scan_devices(
    key: KeyCode,
    mode: Mode,
    tx: &Sender<Command>,
    watched: &Arc<Mutex<HashSet<PathBuf>>>,
    hold: &Arc<Mutex<Hold>>,
    failed: &mut HashSet<PathBuf>,
    stop: &Arc<AtomicBool>,
) {
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return;
    };
    for path in entries.flatten().map(|e| e.path()) {
        let is_event_node = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("event"));
        if !is_event_node || watched.lock().unwrap().contains(&path) {
            continue;
        }
        let dev = match Device::open(&path) {
            Ok(d) => d,
            Err(e) => {
                if failed.insert(path.clone()) {
                    eprintln!("hotkey: {} unreadable ({e}); will retry", path.display());
                }
                continue;
            }
        };
        failed.remove(&path);
        if !is_keyboard(&dev) {
            continue;
        }
        watched.lock().unwrap().insert(path.clone());
        let tx = tx.clone();
        let stop = stop.clone();
        let watched = Arc::clone(watched);
        let hold = Arc::clone(hold);
        // Poll with a timeout so the stop flag is honored quickly.
        dev.set_nonblocking(true).ok();
        thread::spawn(move || {
            watch_device(&path, dev, key, mode, tx, &hold, stop);
            watched.lock().unwrap().remove(&path);
        });
    }
}

fn watch_device(
    path: &Path,
    mut dev: Device,
    key: KeyCode,
    mode: Mode,
    tx: Sender<Command>,
    hold: &Mutex<Hold>,
    stop: Arc<AtomicBool>,
) {
    let name = dev.name().unwrap_or("?").to_string();
    // Send while holding the lock so commands keep the order of the
    // transitions that produced them across device threads.
    let apply = |input| {
        let mut hold = hold.lock().unwrap();
        if let Some(cmd) = hold.apply(path, input, mode) {
            let _ = tx.send(cmd);
        }
    };
    eprintln!("hotkey: watching {} ({})", path.display(), name);
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match dev.fetch_events() {
            Ok(events) => {
                for input in events.filter_map(|ev| classify(&ev, key)) {
                    apply(input);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(8));
            }
            Err(e) => {
                // Device removed or unplugged — stop watching it.
                eprintln!("hotkey: {} ({}) unreadable: {e}", path.display(), name);
                apply(Input::Lost);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typer::Typer;
    use std::sync::mpsc::channel;
    use std::time::Instant;

    /// A quick tap on the virtual uinput keyboard must reach the listener as a
    /// full Start/Stop pair — proves the whole hotkey path works on this
    /// machine, and regresses the debounce that dropped releases under 80ms,
    /// leaving a recording running until the next press.
    /// F24 is used so a running instance's configured hotkey can't fire.
    #[test]
    fn quick_tap_reaches_listener_as_start_and_stop() {
        let mut typer = Typer::new().expect("uinput must be writable");
        thread::sleep(Duration::from_millis(500));

        let (tx, rx) = channel();
        let watcher = Watcher::start(KeyCode::KEY_F24, Mode::Hold, tx).expect("enumerate");

        typer.emit(KeyCode::KEY_F24, 1).unwrap();
        thread::sleep(Duration::from_millis(10));
        typer.emit(KeyCode::KEY_F24, 0).unwrap();

        for expected in [Command::Start, Command::Stop] {
            let cmd = rx
                .recv_timeout(Duration::from_secs(3))
                .unwrap_or_else(|e| panic!("expected {expected:?}: {e}"));
            assert_eq!(cmd, expected);
        }
        drop(watcher);
    }

    /// A keyboard appearing after Watcher::start — hot-plug, suspend/resume —
    /// must be picked up by the rescan. Regresses the one-shot-enumeration bug
    /// where a re-enumerated keyboard left the hotkey dead until app restart.
    #[test]
    fn hotplugged_keyboard_is_watched() {
        let (tx, rx) = channel();
        let watcher = Watcher::start(KeyCode::KEY_F24, Mode::Toggle, tx).expect("enumerate");

        // The "keyboard" only shows up now — after the watcher started.
        let mut typer = Typer::new().expect("uinput must be writable");

        // Re-emit until a rescan attaches a listener (or the deadline fails).
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            typer.emit(KeyCode::KEY_F24, 1).unwrap();
            thread::sleep(Duration::from_millis(30));
            typer.emit(KeyCode::KEY_F24, 0).unwrap();
            match rx.recv_timeout(Duration::from_millis(400)) {
                Ok(cmd) => {
                    assert_eq!(cmd, Command::Toggle);
                    break;
                }
                Err(_) if Instant::now() < deadline => continue,
                Err(e) => panic!("expected a Toggle command: {e}"),
            }
        }
        drop(watcher);
    }

    fn run(mode: Mode, inputs: &[(&str, Input)]) -> Vec<Command> {
        let mut hold = Hold::default();
        inputs
            .iter()
            .filter_map(|(dev, input)| hold.apply(Path::new(dev), *input, mode))
            .collect()
    }

    const KBD: &str = "/dev/input/event1";
    const OTHER: &str = "/dev/input/event2";

    #[test]
    fn shortcut_cancels_hold_instead_of_transcribing() {
        let cmds = run(
            Mode::Hold,
            &[
                (KBD, Input::HotkeyDown),
                (KBD, Input::OtherKey),
                (KBD, Input::OtherKey),
                (KBD, Input::HotkeyUp),
            ],
        );
        assert_eq!(cmds, [Command::Start, Command::Cancel]);
    }

    #[test]
    fn shortcut_never_toggles() {
        let chord = [
            (KBD, Input::HotkeyDown),
            (OTHER, Input::OtherKey),
            (KBD, Input::HotkeyUp),
        ];
        assert_eq!(run(Mode::Toggle, &chord), []);
        let tap = [(KBD, Input::HotkeyDown), (KBD, Input::HotkeyUp)];
        assert_eq!(run(Mode::Toggle, &tap), [Command::Toggle]);
    }

    #[test]
    fn overlapping_devices_make_one_gesture() {
        let cmds = run(
            Mode::Toggle,
            &[
                (KBD, Input::HotkeyDown),
                (OTHER, Input::HotkeyDown),
                (KBD, Input::HotkeyUp),
                (OTHER, Input::HotkeyUp),
            ],
        );
        assert_eq!(cmds, [Command::Toggle]);
    }

    #[test]
    fn losing_the_holding_device_cancels() {
        let cmds = run(Mode::Hold, &[(KBD, Input::HotkeyDown), (KBD, Input::Lost)]);
        assert_eq!(cmds, [Command::Start, Command::Cancel]);
        // A release from a hold this listener never saw does nothing.
        assert_eq!(run(Mode::Hold, &[(KBD, Input::HotkeyUp)]), []);
    }
}
