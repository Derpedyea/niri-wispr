use anyhow::{Context, Result};
use evdev::{Device, EventType, InputEvent, KeyCode};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use crate::ipc::{Command, HotkeyCommand};

/// Hold-mode releases sooner than this are taps or shortcuts, not dictation:
/// they cancel instead of stopping. Timed here from the key events because the
/// app sees commands late (pump interval, mic setup).
pub const MIN_HOLD: Duration = Duration::from_millis(250);

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
    /// Ctrl, Shift, Alt, or Super pressed or released. Tracked so one already
    /// held when the hotkey goes down (Ctrl+C with hotkey C) makes a shortcut.
    ModifierDown(KeyCode),
    ModifierUp(KeyCode),
    /// The device vanished (unplug, suspend) — a hotkey held on it will
    /// never be released.
    Lost,
}

fn classify(ev: &InputEvent, key: KeyCode) -> Option<Input> {
    if ev.event_type() != EventType::KEY {
        return None;
    }
    let code = KeyCode(ev.code());
    // value: 0 = release, 1 = press, 2 = autorepeat
    match (code == key, ev.value()) {
        (true, 1) => Some(Input::HotkeyDown),
        (true, 0) => Some(Input::HotkeyUp),
        (false, 1) if is_modifier(code) => Some(Input::ModifierDown(code)),
        (false, 0) if is_modifier(code) => Some(Input::ModifierUp(code)),
        (false, 1) if !is_button(code) => Some(Input::OtherKey),
        _ => None,
    }
}

fn is_modifier(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::KEY_LEFTCTRL
            | KeyCode::KEY_RIGHTCTRL
            | KeyCode::KEY_LEFTSHIFT
            | KeyCode::KEY_RIGHTSHIFT
            | KeyCode::KEY_LEFTALT
            | KeyCode::KEY_RIGHTALT
            | KeyCode::KEY_LEFTMETA
            | KeyCode::KEY_RIGHTMETA
    )
}

/// Pointer, touch, and gamepad buttons. Every other key code is a keyboard
/// key — including those numbered past the first button range, like KEY_OK.
fn is_button(code: KeyCode) -> bool {
    [
        KeyCode::BTN_0..=KeyCode::BTN_GEAR_UP,
        KeyCode::BTN_DPAD_UP..=KeyCode::BTN_DPAD_RIGHT,
        KeyCode::BTN_TRIGGER_HAPPY1..=KeyCode::BTN_TRIGGER_HAPPY40,
    ]
    .iter()
    .any(|buttons| buttons.contains(&code))
}

/// One state for every watched device, so overlapping holds count as one
/// gesture and a chord on any keyboard counts.
struct Hold {
    /// Devices currently holding the hotkey down.
    down: HashSet<PathBuf>,
    /// When the current hold began; only meaningful while `down` is non-empty.
    since: SystemTime,
    /// Modifiers currently held, per device. Only modifiers: a letter still
    /// down from fast typing must not swallow the dictation that follows.
    modifiers: HashSet<(PathBuf, KeyCode)>,
    /// Another key was pressed during this hold, or a modifier was already
    /// held when it began: it's a shortcut like RightCtrl+C, not dictation.
    chorded: bool,
}

impl Hold {
    fn new() -> Hold {
        Hold {
            down: HashSet::new(),
            since: SystemTime::UNIX_EPOCH,
            modifiers: HashSet::new(),
            chorded: false,
        }
    }

    /// Apply one input from `dev` at `now`; returns the command to send, if
    /// any. Hold mode starts on press so no speech is lost; a chord or a
    /// release before MIN_HOLD cancels. Toggle mode waits for the release so a
    /// chord never toggles.
    fn apply(
        &mut self,
        dev: &Path,
        input: Input,
        mode: Mode,
        now: SystemTime,
    ) -> Option<HotkeyCommand> {
        match input {
            Input::HotkeyDown => {
                let fresh = self.down.is_empty();
                self.down.insert(dev.to_path_buf());
                if !fresh {
                    return None;
                }
                self.since = now;
                self.chorded = !self.modifiers.is_empty();
                (mode == Mode::Hold && !self.chorded).then_some(HotkeyCommand::Start)
            }
            // A release we never saw pressed (key already down when the
            // listener attached) is ignored.
            Input::HotkeyUp => {
                if !self.down.remove(dev) || !self.down.is_empty() || self.chorded {
                    return None;
                }
                Some(match mode {
                    // A wall-clock rollback makes the gesture's duration unknown:
                    // discard it rather than transcribing an apparent short tap.
                    Mode::Hold
                        if !now
                            .duration_since(self.since)
                            .is_ok_and(|held| held >= MIN_HOLD) =>
                    {
                        HotkeyCommand::Cancel
                    }
                    Mode::Hold => HotkeyCommand::Stop,
                    Mode::Toggle => HotkeyCommand::Toggle,
                })
            }
            Input::OtherKey => self.chord(mode),
            Input::ModifierDown(key) => {
                self.modifiers.insert((dev.to_path_buf(), key));
                self.chord(mode)
            }
            Input::ModifierUp(key) => {
                self.modifiers.remove(&(dev.to_path_buf(), key));
                None
            }
            // Nothing will release this hold, so discard rather than transcribe.
            Input::Lost => {
                self.modifiers.retain(|(held_on, _)| held_on != dev);
                if !self.down.remove(dev) || !self.down.is_empty() || self.chorded {
                    return None;
                }
                (mode == Mode::Hold).then_some(HotkeyCommand::Cancel)
            }
        }
    }

    /// A key pressed during the hold makes it a shortcut.
    fn chord(&mut self, mode: Mode) -> Option<HotkeyCommand> {
        if self.down.is_empty() || self.chorded {
            return None;
        }
        self.chorded = true;
        (mode == Mode::Hold).then_some(HotkeyCommand::Cancel)
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

/// All devices belong to one worker. Stop joins it before returning, so reload
/// can reject its queued generation and cancel capture without any late sends.
pub struct Watcher {
    stop: Arc<StopSignal>,
    worker: Option<JoinHandle<()>>,
    generation: u64,
}

#[derive(Default)]
struct StopSignal {
    stopped: AtomicBool,
    lock: Mutex<()>,
    wake: Condvar,
}

impl StopSignal {
    fn stop(&self) {
        let _guard = self.lock.lock().unwrap();
        self.stopped.store(true, Ordering::SeqCst);
        self.wake.notify_all();
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    fn wait(&self, duration: Duration) {
        let guard = self.lock.lock().unwrap();
        let _ = self
            .wake
            .wait_timeout_while(guard, duration, |_| !self.is_stopped())
            .unwrap();
    }
}

struct Keyboard {
    path: PathBuf,
    dev: Device,
}

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
const POLL_INTERVAL: Duration = Duration::from_millis(8);
const OWN_DEVICE_NAME: &str = "dictationapp virtual keyboard";

impl Watcher {
    #[cfg(test)]
    pub fn for_test() -> Self {
        Self {
            stop: Arc::new(StopSignal::default()),
            worker: None,
            generation: NEXT_GENERATION
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |next| {
                    next.checked_add(1)
                })
                .expect("test watcher generation fits"),
        }
    }

    pub fn start(key: KeyCode, mode: Mode, tx: Sender<Command>) -> Result<Watcher> {
        let generation = NEXT_GENERATION
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |next| {
                next.checked_add(1)
            })
            .map_err(|_| anyhow::anyhow!("hotkey watcher generation exhausted"))?;
        let stop = Arc::new(StopSignal::default());
        let mut devices = Devices {
            keyboards: Vec::new(),
            key,
            mode,
            tx,
            hold: Hold::new(),
            failed: HashSet::new(),
            generation,
        };
        devices.scan()?;
        eprintln!(
            "hotkey: {key:?} ({mode:?}) on {} device(s)",
            devices.keyboards.len()
        );
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("dictation-hotkey".into())
            .spawn(move || devices.watch(worker_stop))
            .context("unable to start hotkey watcher")?;
        Ok(Watcher {
            stop,
            worker: Some(worker),
            generation,
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Quiescent on success. The caller handles active capture and queued events
    /// from this generation before starting a replacement.
    pub fn stop(&mut self) -> Result<()> {
        self.stop.stop();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("hotkey watcher panicked"))?;
        }
        Ok(())
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("hotkey shutdown failed: {error:#}");
        }
    }
}

fn send_hotkey(tx: &Sender<Command>, generation: u64, action: Option<HotkeyCommand>) -> Result<()> {
    if let Some(action) = action {
        tx.send(Command::Hotkey { generation, action })
            .context("dictation command receiver closed")?;
    }
    Ok(())
}

fn is_own_device(name: Option<&str>) -> bool {
    name == Some(OWN_DEVICE_NAME)
}

fn seed_modifiers(
    hold: &mut Hold,
    path: &Path,
    keys: &evdev::AttributeSetRef<KeyCode>,
    key: KeyCode,
    mode: Mode,
) -> Option<HotkeyCommand> {
    for modifier in keys
        .iter()
        .filter(|code| *code != key && is_modifier(*code))
    {
        hold.modifiers.insert((path.to_path_buf(), modifier));
    }
    if hold.modifiers.is_empty() {
        None
    } else {
        hold.chord(mode)
    }
}

struct Devices {
    keyboards: Vec<Keyboard>,
    key: KeyCode,
    mode: Mode,
    tx: Sender<Command>,
    hold: Hold,
    failed: HashSet<PathBuf>,
    generation: u64,
}

/// Merge keyboards by event time without reversing any device's event sequence.
/// Realtime clock rollback (or synthetic resync events) can lower a timestamp;
/// preserve the original time for Hold to reject an uncertain duration.
fn order_inputs(inputs: Vec<(SystemTime, PathBuf, Input)>) -> Vec<(SystemTime, PathBuf, Input)> {
    let mut latest = HashMap::new();
    let mut ordered = inputs
        .into_iter()
        .map(|(at, path, input)| {
            let previous = latest.entry(path.clone()).or_insert(at);
            *previous = (*previous).max(at);
            (*previous, (at, path, input))
        })
        .collect::<Vec<_>>();
    ordered.sort_by_key(|(at, _)| *at);
    ordered.into_iter().map(|(_, input)| input).collect()
}

impl Devices {
    fn scan(&mut self) -> Result<()> {
        let entries = std::fs::read_dir("/dev/input").context("unable to enumerate /dev/input")?;
        let mut paths = entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()
            .context("unable to enumerate input device nodes")?;
        paths.sort();
        self.failed.retain(|path| paths.contains(path));
        for path in paths {
            let is_event_node = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("event"));
            if !is_event_node || self.keyboards.iter().any(|keyboard| keyboard.path == path) {
                continue;
            }
            let open = || -> Result<Option<(Device, evdev::AttributeSet<KeyCode>)>> {
                let dev = Device::open(&path)?;
                // Old installations may still expose their uinput keyboard. New
                // delivery uses the Wayland protocol and has no evdev device at all.
                if is_own_device(dev.name()) || !is_keyboard(&dev) {
                    return Ok(None);
                }
                dev.set_nonblocking(true)
                    .context("unable to make input reads nonblocking")?;
                let held = dev
                    .get_key_state()
                    .context("unable to read initial keyboard state")?;
                Ok(Some((dev, held)))
            };
            match open() {
                Ok(Some((dev, held))) => {
                    self.failed.remove(&path);
                    send_hotkey(
                        &self.tx,
                        self.generation,
                        seed_modifiers(&mut self.hold, &path, &held, self.key, self.mode),
                    )?;
                    eprintln!(
                        "hotkey: watching {} ({})",
                        path.display(),
                        dev.name().unwrap_or("?")
                    );
                    self.keyboards.push(Keyboard { path, dev });
                }
                Ok(None) => {
                    self.failed.remove(&path);
                }
                Err(error) => {
                    if self.failed.insert(path.clone()) {
                        eprintln!(
                            "hotkey: {} unavailable ({error:#}); will retry",
                            path.display()
                        );
                    }
                }
            }
        }
        Ok(())
    }
    fn watch(mut self, stop: Arc<StopSignal>) {
        let mut last_scan = Instant::now();
        while !stop.is_stopped() {
            if last_scan.elapsed() >= RESCAN_INTERVAL {
                if let Err(error) = self.scan() {
                    eprintln!("hotkey rescan failed: {error:#}; will retry");
                }
                last_scan = Instant::now();
            }
            let mut inputs = Vec::new();
            let mut lost = Vec::new();
            for keyboard in &mut self.keyboards {
                match keyboard.dev.fetch_events() {
                    Ok(events) => {
                        for event in events {
                            if let Some(input) = classify(&event, self.key) {
                                // Kernel timestamps preserve the actual gesture
                                // duration even when both events arrive in one batch.
                                inputs.push((event.timestamp(), keyboard.path.clone(), input));
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => {
                        eprintln!("hotkey: {} unreadable: {error}", keyboard.path.display());
                        lost.push(keyboard.path.clone());
                    }
                }
            }
            for (at, path, input) in order_inputs(inputs) {
                if stop.is_stopped()
                    || send_hotkey(
                        &self.tx,
                        self.generation,
                        self.hold.apply(&path, input, self.mode, at),
                    )
                    .is_err()
                {
                    return;
                }
            }
            for path in lost {
                self.keyboards.retain(|keyboard| keyboard.path != path);
                if stop.is_stopped()
                    || send_hotkey(
                        &self.tx,
                        self.generation,
                        self.hold
                            .apply(&path, Input::Lost, self.mode, SystemTime::now()),
                    )
                    .is_err()
                {
                    return;
                }
            }
            stop.wait(POLL_INTERVAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typer::VirtualKeyboard;
    use std::sync::mpsc::channel;

    /// A quick tap on the virtual uinput keyboard must reach the listener as a
    /// full Start/Cancel pair — proves the whole hotkey path works on this
    /// machine, and regresses the debounce that dropped releases under 80ms,
    /// leaving a recording running until the next press.
    /// F24 is used so a running instance's configured hotkey can't fire.
    #[test]
    #[ignore = "requires /dev/uinput; emits real keyboard events"]
    fn quick_tap_reaches_listener_as_start_and_cancel() {
        let mut typer = VirtualKeyboard::new().expect("uinput must be writable");
        thread::sleep(Duration::from_millis(500));

        let (tx, rx) = channel();
        let watcher = Watcher::start(KeyCode::KEY_F24, Mode::Hold, tx).expect("enumerate");

        typer.emit(KeyCode::KEY_F24, 1).unwrap();
        thread::sleep(Duration::from_millis(10));
        typer.emit(KeyCode::KEY_F24, 0).unwrap();

        for expected in [HotkeyCommand::Start, HotkeyCommand::Cancel] {
            let cmd = rx
                .recv_timeout(Duration::from_secs(3))
                .unwrap_or_else(|e| panic!("expected {expected:?}: {e}"));
            assert_eq!(
                cmd,
                Command::Hotkey {
                    generation: watcher.generation(),
                    action: expected
                }
            );
        }
        drop(watcher);
    }

    /// A keyboard appearing after Watcher::start — hot-plug, suspend/resume —
    /// must be picked up by the rescan. Regresses the one-shot-enumeration bug
    /// where a re-enumerated keyboard left the hotkey dead until app restart.
    #[test]
    #[ignore = "requires /dev/uinput; emits real keyboard events"]
    fn hotplugged_keyboard_is_watched() {
        let (tx, rx) = channel();
        let watcher = Watcher::start(KeyCode::KEY_F24, Mode::Toggle, tx).expect("enumerate");

        // The "keyboard" only shows up now — after the watcher started.
        let mut typer = VirtualKeyboard::new().expect("uinput must be writable");

        // Re-emit until a rescan attaches a listener (or the deadline fails).
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            typer.emit(KeyCode::KEY_F24, 1).unwrap();
            thread::sleep(Duration::from_millis(30));
            typer.emit(KeyCode::KEY_F24, 0).unwrap();
            match rx.recv_timeout(Duration::from_millis(400)) {
                Ok(cmd) => {
                    assert_eq!(
                        cmd,
                        Command::Hotkey {
                            generation: watcher.generation(),
                            action: HotkeyCommand::Toggle
                        }
                    );
                    break;
                }
                Err(_) if Instant::now() < deadline => continue,
                Err(e) => panic!("expected a Toggle command: {e}"),
            }
        }
        drop(watcher);
    }

    /// Feed `(device, input, ms since the first input)` through one Hold.
    fn run(mode: Mode, inputs: &[(&str, Input, u64)]) -> Vec<HotkeyCommand> {
        let mut hold = Hold::new();
        let t0 = SystemTime::UNIX_EPOCH;
        inputs
            .iter()
            .filter_map(|&(dev, input, ms)| {
                hold.apply(Path::new(dev), input, mode, t0 + Duration::from_millis(ms))
            })
            .collect()
    }

    const KBD: &str = "/dev/input/event1";
    const OTHER: &str = "/dev/input/event2";

    #[test]
    fn initial_modifiers_block_shortcuts_but_hotkey_alone_does_not() {
        let mut keys = evdev::AttributeSet::new();
        keys.insert(KeyCode::KEY_RIGHTCTRL);
        let mut hold = Hold::new();
        let path = Path::new(KBD);
        let now = SystemTime::UNIX_EPOCH;
        assert_eq!(
            seed_modifiers(&mut hold, path, &keys, KeyCode::KEY_RIGHTCTRL, Mode::Hold),
            None
        );
        assert_eq!(
            hold.apply(path, Input::HotkeyDown, Mode::Hold, now),
            Some(HotkeyCommand::Start)
        );
        keys.insert(KeyCode::KEY_LEFTSHIFT);
        assert_eq!(
            seed_modifiers(&mut hold, path, &keys, KeyCode::KEY_RIGHTCTRL, Mode::Hold),
            Some(HotkeyCommand::Cancel)
        );
        assert_eq!(
            hold.apply(
                path,
                Input::HotkeyUp,
                Mode::Hold,
                now + Duration::from_secs(1)
            ),
            None
        );
        assert_eq!(
            hold.apply(
                path,
                Input::HotkeyDown,
                Mode::Hold,
                now + Duration::from_secs(2)
            ),
            None
        );
    }

    #[test]
    fn clock_rollback_discards_the_recording() {
        let mut hold = Hold::new();
        let inputs = vec![
            (
                SystemTime::UNIX_EPOCH + Duration::from_secs(2),
                PathBuf::from(KBD),
                Input::HotkeyDown,
            ),
            (
                SystemTime::UNIX_EPOCH + Duration::from_secs(1),
                PathBuf::from(KBD),
                Input::HotkeyUp,
            ),
        ];
        let commands = order_inputs(inputs)
            .into_iter()
            .filter_map(|(at, path, input)| hold.apply(&path, input, Mode::Hold, at))
            .collect::<Vec<_>>();
        assert_eq!(commands, [HotkeyCommand::Start, HotkeyCommand::Cancel]);
        assert!(hold.down.is_empty());
    }

    #[test]
    fn stop_wakes_and_joins_the_worker() {
        let mut watcher = Watcher::for_test();
        let stop = Arc::clone(&watcher.stop);
        let (ready_tx, ready_rx) = channel();
        let (done_tx, done_rx) = channel();
        watcher.worker = Some(thread::spawn(move || {
            ready_tx.send(()).unwrap();
            stop.wait(Duration::from_secs(86_400));
            done_tx.send(()).unwrap();
        }));
        ready_rx.recv().unwrap();
        watcher.stop().unwrap();
        assert_eq!(done_rx.try_recv(), Ok(()));
        watcher.stop().unwrap();
    }

    #[test]
    fn hold_shorter_than_min_hold_is_a_tap() {
        let hold = |ms| {
            run(
                Mode::Hold,
                &[(KBD, Input::HotkeyDown, 0), (KBD, Input::HotkeyUp, ms)],
            )
        };
        assert_eq!(hold(249), [HotkeyCommand::Start, HotkeyCommand::Cancel]);
        assert_eq!(hold(250), [HotkeyCommand::Start, HotkeyCommand::Stop]);
    }

    #[test]
    fn shortcut_cancels_hold_instead_of_transcribing() {
        let cmds = run(
            Mode::Hold,
            &[
                (KBD, Input::HotkeyDown, 0),
                (KBD, Input::OtherKey, 300),
                (KBD, Input::OtherKey, 350),
                (KBD, Input::HotkeyUp, 600),
            ],
        );
        assert_eq!(cmds, [HotkeyCommand::Start, HotkeyCommand::Cancel]);
    }

    #[test]
    fn modifier_held_before_the_hotkey_makes_a_shortcut() {
        const CTRL: Input = Input::ModifierDown(KeyCode::KEY_LEFTCTRL);
        const CTRL_UP: Input = Input::ModifierUp(KeyCode::KEY_LEFTCTRL);
        // Ctrl+hotkey neither toggles nor starts.
        let ctrl_tap = [
            (KBD, CTRL, 0),
            (KBD, Input::HotkeyDown, 50),
            (KBD, Input::HotkeyUp, 600),
            (KBD, CTRL_UP, 700),
            // Once Ctrl is up, the hotkey works again.
            (KBD, Input::HotkeyDown, 800),
            (KBD, Input::HotkeyUp, 900),
        ];
        assert_eq!(run(Mode::Toggle, &ctrl_tap), [HotkeyCommand::Toggle]);
        assert_eq!(
            run(Mode::Hold, &ctrl_tap),
            [HotkeyCommand::Start, HotkeyCommand::Cancel]
        );
        // A modifier held on a vanished device no longer counts.
        let lost = [
            (KBD, CTRL, 0),
            (KBD, Input::Lost, 10),
            (OTHER, Input::HotkeyDown, 20),
            (OTHER, Input::HotkeyUp, 100),
        ];
        assert_eq!(run(Mode::Toggle, &lost), [HotkeyCommand::Toggle]);
        // A letter still down from typing doesn't block dictation.
        let rollover = [
            (KBD, Input::OtherKey, 0),
            (KBD, Input::HotkeyDown, 30),
            (KBD, Input::HotkeyUp, 400),
        ];
        assert_eq!(
            run(Mode::Hold, &rollover),
            [HotkeyCommand::Start, HotkeyCommand::Stop]
        );
    }

    #[test]
    fn shortcut_never_toggles() {
        let chord = [
            (KBD, Input::HotkeyDown, 0),
            (OTHER, Input::OtherKey, 100),
            (KBD, Input::HotkeyUp, 200),
        ];
        assert_eq!(run(Mode::Toggle, &chord), []);
        let tap = [(KBD, Input::HotkeyDown, 0), (KBD, Input::HotkeyUp, 50)];
        assert_eq!(run(Mode::Toggle, &tap), [HotkeyCommand::Toggle]);
    }

    #[test]
    fn overlapping_devices_make_one_gesture() {
        let cmds = run(
            Mode::Toggle,
            &[
                (KBD, Input::HotkeyDown, 0),
                (OTHER, Input::HotkeyDown, 5),
                (KBD, Input::HotkeyUp, 100),
                (OTHER, Input::HotkeyUp, 105),
            ],
        );
        assert_eq!(cmds, [HotkeyCommand::Toggle]);
    }

    #[test]
    fn losing_the_holding_device_cancels() {
        let cmds = run(
            Mode::Hold,
            &[(KBD, Input::HotkeyDown, 0), (KBD, Input::Lost, 900)],
        );
        assert_eq!(cmds, [HotkeyCommand::Start, HotkeyCommand::Cancel]);
        // A release from a hold this listener never saw does nothing.
        assert_eq!(run(Mode::Hold, &[(KBD, Input::HotkeyUp, 0)]), []);
    }

    #[test]
    fn keys_past_the_button_ranges_still_chord() {
        let press = |key: KeyCode| {
            classify(
                &InputEvent::new(EventType::KEY.0, key.code(), 1),
                KeyCode::KEY_RIGHTCTRL,
            )
        };
        assert_eq!(press(KeyCode::KEY_C), Some(Input::OtherKey));
        assert_eq!(press(KeyCode::KEY_OK), Some(Input::OtherKey));
        assert_eq!(press(KeyCode::KEY_FN), Some(Input::OtherKey));
        assert_eq!(press(KeyCode::BTN_LEFT), None);
        assert_eq!(press(KeyCode::BTN_TOUCH), None);
        assert_eq!(
            press(KeyCode::KEY_LEFTSHIFT),
            Some(Input::ModifierDown(KeyCode::KEY_LEFTSHIFT))
        );
    }
}
