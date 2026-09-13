use anyhow::{Context, Result};
use evdev::{Device, EventType, KeyCode};
use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::ipc::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Hold the key to record, release to transcribe.
    Hold,
    /// Press the key to toggle recording on/off.
    Toggle,
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
    /// devices come and go. Sends Command::Start/Stop (hold) or Command::Toggle
    /// on each press.
    pub fn start(key: KeyCode, mode: Mode, tx: Sender<Command>) -> Result<Watcher> {
        let stop = Arc::new(AtomicBool::new(false));
        let watched = Arc::new(Mutex::new(HashSet::new()));
        let mut failed = HashSet::new();
        scan_devices(key, mode, &tx, &watched, &mut failed, &stop);
        eprintln!(
            "hotkey: {key:?} ({mode:?}) on {} device(s)",
            watched.lock().unwrap().len()
        );
        let watched = Arc::clone(&watched);
        let stop2 = Arc::clone(&stop);
        thread::spawn(move || loop {
            thread::sleep(RESCAN_INTERVAL);
            if stop2.load(Ordering::Relaxed) {
                return;
            }
            scan_devices(key, mode, &tx, &watched, &mut failed, &stop2);
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
        let name = dev.name().unwrap_or("?").to_string();
        // Poll with a timeout so the stop flag is honored quickly.
        dev.set_nonblocking(true).ok();
        thread::spawn(move || {
            watch_device(path.clone(), dev, key, mode, tx, name, stop);
            watched.lock().unwrap().remove(&path);
        });
    }
}

fn watch_device(
    path: std::path::PathBuf,
    mut dev: Device,
    key: KeyCode,
    mode: Mode,
    tx: Sender<Command>,
    name: String,
    stop: Arc<AtomicBool>,
) {
    let mut held = false;
    let mut last_change = Instant::now() - Duration::from_secs(1);
    eprintln!("hotkey: watching {} ({})", path.display(), name);
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match dev.fetch_events() {
            Ok(events) => {
                for ev in events {
                    if ev.event_type() != EventType::KEY || ev.code() != key.code() {
                        continue;
                    }
                    // value: 0 = release, 1 = press, 2 = autorepeat
                    if ev.value() == 2 {
                        continue;
                    }
                    let now = Instant::now();
                    if now.duration_since(last_change) < Duration::from_millis(80) {
                        continue;
                    }
                    last_change = now;
                    match mode {
                        Mode::Hold => {
                            if ev.value() == 1 && !held {
                                held = true;
                                let _ = tx.send(Command::Start);
                            } else if ev.value() == 0 && held {
                                held = false;
                                let _ = tx.send(Command::Stop);
                            }
                        }
                        Mode::Toggle => {
                            if ev.value() == 1 {
                                let _ = tx.send(Command::Toggle);
                            }
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(8));
            }
            Err(e) => {
                // Device removed or unplugged — stop watching it.
                eprintln!("hotkey: {} ({}) unreadable: {e}", path.display(), name);
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

    /// Emitting a key on the virtual uinput keyboard must be observed by the
    /// evdev listener — proves the whole hotkey path works on this machine.
    /// F24 is used so a running instance's configured hotkey can't fire.
    #[test]
    fn virtual_keypress_reaches_listener() {
        let mut typer = Typer::new().expect("uinput must be writable");
        thread::sleep(Duration::from_millis(500));

        let (tx, rx) = channel();
        let watcher = Watcher::start(KeyCode::KEY_F24, Mode::Toggle, tx).expect("enumerate");

        typer.emit(KeyCode::KEY_F24, 1).unwrap();
        thread::sleep(Duration::from_millis(30));
        typer.emit(KeyCode::KEY_F24, 0).unwrap();

        let cmd = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("expected a Toggle command");
        assert_eq!(cmd, Command::Toggle);
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
}
