use anyhow::{Context, Result};
use evdev::{Device, EventType, KeyCode};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
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

/// A running set of per-device listener threads. Drop-style stop via a shared
/// flag so the hotkey can be reconfigured at runtime without restarting the app.
pub struct Watcher {
    stop: Arc<AtomicBool>,
}

impl Watcher {
    /// Spawn a listener thread per keyboard device. Sends Command::Start/Stop
    /// (hold) or Command::Toggle on each press.
    pub fn start(key: KeyCode, mode: Mode, tx: Sender<Command>) -> Result<Watcher> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut count = 0;
        for (path, dev) in evdev::enumerate() {
            if !is_keyboard(&dev) {
                continue;
            }
            count += 1;
            let tx = tx.clone();
            let stop = stop.clone();
            let name = dev.name().unwrap_or("?").to_string();
            // Poll with a timeout so the stop flag is honored quickly.
            dev.set_nonblocking(true).ok();
            thread::spawn(move || watch_device(path, dev, key, mode, tx, name, stop));
        }
        eprintln!("hotkey: {key:?} ({mode:?}) on {count} device(s)");
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
    #[test]
    fn virtual_keypress_reaches_listener() {
        let mut typer = Typer::new().expect("uinput must be writable");
        thread::sleep(Duration::from_millis(500));

        let (tx, rx) = channel();
        let watcher = Watcher::start(KeyCode::KEY_RIGHTCTRL, Mode::Toggle, tx).expect("enumerate");

        typer.emit(KeyCode::KEY_RIGHTCTRL, 1).unwrap();
        thread::sleep(Duration::from_millis(30));
        typer.emit(KeyCode::KEY_RIGHTCTRL, 0).unwrap();

        let cmd = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("expected a Toggle command");
        assert_eq!(cmd, Command::Toggle);
        drop(watcher);
    }
}
