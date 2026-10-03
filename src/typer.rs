use anyhow::{Context, Result, bail, ensure};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_keyboard::KeymapFormat;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, backend::WaylandError, delegate_noop,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use xkbcommon::xkb;

/// Evdev codes of the US layout's printable keys (1…=, q…], a…`, \…/).
/// Chromium, Electron and terminals act on some keys by scancode whatever
/// keysym the keymap gives them — Escape, Backspace, Tab, modifiers, F-keys,
/// media keys — so transcript characters only ever borrow these codes. wtype
/// assigns codes in order from Escape, which turns the 14th distinct character
/// into Backspace (https://github.com/atx/wtype/issues/71).
const TEXT_KEYS: [std::ops::RangeInclusive<u32>; 4] = [2..=13, 16..=27, 30..=41, 43..=53];

/// Whitespace keeps its real key, so it behaves exactly like typing it.
fn whitespace_key(ch: char) -> Option<(u32, xkb::Keysym)> {
    match ch {
        '\t' => Some((15, xkb::Keysym::Tab)),
        '\n' => Some((28, xkb::Keysym::Return)),
        ' ' => Some((57, xkb::Keysym::space)),
        _ => None,
    }
}

/// The compositor disconnects a client whose socket fills, so keys are paced
/// rather than sent at once, never outrunning a briefly busy focused app.
/// Settling every batch bounds our own buffering and notices a hung compositor.
const BATCH: usize = 16;
const KEY_INTERVAL: Duration = Duration::from_millis(1);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Types through Wayland's virtual keyboard protocol with keymaps built per
/// transcript, so physical layout and lock state cannot reinterpret the text
/// and the events never reach the evdev hotkey listener.
pub struct Typer {
    _private: (),
}

impl Typer {
    #[cfg(test)]
    pub fn for_test() -> Self {
        Self { _private: () }
    }

    /// Fails when the compositor lacks the virtual keyboard protocol, so the
    /// app reports clipboard-only up front instead of on every dictation.
    pub fn new() -> Result<Typer> {
        Session::connect()?;
        Ok(Typer { _private: () })
    }

    /// Each call owns its connection, so a cancelled or failed transcript
    /// leaves nothing behind. Keys are sent as whole press/release pairs and
    /// flushed before returning, so no key is ever left held down.
    pub fn type_str(&mut self, text: &str, cancelled: &AtomicBool) -> Result<()> {
        let chunks = plan(text)?;
        ensure!(!cancelled.load(Ordering::SeqCst), "typing cancelled");
        if chunks.is_empty() {
            return Ok(());
        }
        let mut session = Session::connect()?;
        let mut unsettled = 0;
        'typing: for chunk in &chunks {
            session.keymap(&chunk.keymap)?;
            for &key in &chunk.keys {
                if cancelled.load(Ordering::SeqCst) {
                    break 'typing;
                }
                session.tap(key)?;
                unsettled += 1;
                if unsettled == BATCH {
                    session.settle()?;
                    unsettled = 0;
                }
                thread::sleep(KEY_INTERVAL);
            }
        }
        session.settle()?;
        ensure!(!cancelled.load(Ordering::SeqCst), "typing cancelled");
        Ok(())
    }
}

/// One keymap and the keys typed with it.
struct Chunk {
    keymap: String,
    keys: Vec<u32>,
}

/// Splits the text into keymaps of at most one character per text key.
fn plan(text: &str) -> Result<Vec<Chunk>> {
    let text_keys = TEXT_KEYS.into_iter().flatten().collect::<Vec<_>>();
    let mut chunks = Vec::new();
    let mut assigned = HashMap::new();
    let mut symbols = Vec::new();
    let mut keys = Vec::new();
    for ch in text.chars() {
        if let Some((key, _)) = whitespace_key(ch) {
            keys.push(key);
            continue;
        }
        if let Some(&key) = assigned.get(&ch) {
            keys.push(key);
            continue;
        }
        let keysym = xkb::utf32_to_keysym(u32::from(ch));
        if ch.is_control() || keysym == xkb::Keysym::NoSymbol {
            bail!(
                "transcript contains untypeable character U+{:04X}",
                u32::from(ch)
            );
        }
        if symbols.len() == text_keys.len() {
            chunks.push(Chunk::new(&symbols, std::mem::take(&mut keys))?);
            assigned.clear();
            symbols.clear();
        }
        let key = text_keys[symbols.len()];
        assigned.insert(ch, key);
        symbols.push((key, keysym));
        keys.push(key);
    }
    if !keys.is_empty() {
        chunks.push(Chunk::new(&symbols, keys)?);
    }
    Ok(chunks)
}

impl Chunk {
    fn new(symbols: &[(u32, xkb::Keysym)], keys: Vec<u32>) -> Result<Self> {
        let all = [' ', '\t', '\n']
            .into_iter()
            .filter_map(whitespace_key)
            .chain(symbols.iter().copied())
            .collect::<Vec<_>>();
        let mut codes = String::new();
        let mut syms = String::new();
        for (key, keysym) in &all {
            // Names stay within X11's four characters for Xwayland.
            writeln!(codes, "<K{key}> = {};", key + 8)?;
            writeln!(
                syms,
                "key <K{key}> {{ [ {} ] }};",
                xkb::keysym_get_name(*keysym)
            )?;
        }
        let keymap = format!(
            "xkb_keymap {{\n\
             xkb_keycodes \"dictationapp\" {{\nminimum = 8;\nmaximum = 255;\n{codes}}};\n\
             xkb_types \"dictationapp\" {{ include \"complete\" }};\n\
             xkb_compatibility \"dictationapp\" {{ include \"complete\" }};\n\
             xkb_symbols \"dictationapp\" {{\n{syms}}};\n\
             }};\n"
        );
        verify(&keymap, &all)?;
        Ok(Chunk { keymap, keys })
    }
}

/// The compositor silently keeps the previous keymap when a new one fails to
/// compile, which would type the wrong characters, so prove each key first.
fn verify(keymap: &str, symbols: &[(u32, xkb::Keysym)]) -> Result<()> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let compiled = xkb::Keymap::new_from_string(
        &context,
        keymap.to_owned(),
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .context("typing keymap failed to compile")?;
    let state = xkb::State::new(&compiled);
    for &(key, keysym) in symbols {
        let actual = state.key_get_one_sym(xkb::Keycode::new(key + 8));
        ensure!(
            actual == keysym,
            "typing keymap maps key {key} to {actual:?} instead of {keysym:?}"
        );
    }
    Ok(())
}

#[derive(Default)]
struct Events {
    settled: bool,
}

impl Dispatch<WlRegistry, GlobalListContents> for Events {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlCallback, ()> for Events {
    fn event(
        events: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            events.settled = true;
        }
    }
}

delegate_noop!(Events: ignore WlSeat);
delegate_noop!(Events: ZwpVirtualKeyboardManagerV1);
delegate_noop!(Events: ZwpVirtualKeyboardV1);

struct Session {
    connection: Connection,
    queue: EventQueue<Events>,
    events: Events,
    keyboard: ZwpVirtualKeyboardV1,
    started: Instant,
}

impl Session {
    fn connect() -> Result<Self> {
        let connection = Connection::connect_to_env().context("no Wayland session to type into")?;
        let (globals, queue) =
            registry_queue_init::<Events>(&connection).context("unable to list Wayland globals")?;
        let handle = queue.handle();
        let seat: WlSeat = globals
            .bind(&handle, 1..=1, ())
            .context("compositor has no seat to type into")?;
        let manager: ZwpVirtualKeyboardManagerV1 = globals
            .bind(&handle, 1..=1, ())
            .context("compositor lacks the Wayland virtual keyboard protocol")?;
        let keyboard = manager.create_virtual_keyboard(&seat, &handle, ());
        Ok(Self {
            connection,
            queue,
            events: Events::default(),
            keyboard,
            started: Instant::now(),
        })
    }

    fn keymap(&mut self, keymap: &str) -> Result<()> {
        let mut file = tempfile::tempfile().context("unable to create typing keymap")?;
        // NUL-terminated, like every wl_keyboard keymap.
        file.write_all(keymap.as_bytes())
            .and_then(|()| file.write_all(&[0]))
            .context("unable to write typing keymap")?;
        let size = u32::try_from(keymap.len() + 1).context("typing keymap is too large")?;
        self.keyboard
            .keymap(KeymapFormat::XkbV1.into(), file.as_fd(), size);
        Ok(())
    }

    fn tap(&mut self, key: u32) -> Result<()> {
        let time = u32::try_from(self.started.elapsed().as_millis())
            .context("typing session ran too long")?;
        self.keyboard.key(time, key, 1);
        self.keyboard.key(time, key, 0);
        self.flush(Instant::now() + RESPONSE_TIMEOUT)
    }

    /// Waits until the compositor has handled every request sent so far.
    fn settle(&mut self) -> Result<()> {
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        self.events.settled = false;
        self.connection.display().sync(&self.queue.handle(), ());
        loop {
            self.queue
                .dispatch_pending(&mut self.events)
                .context("Wayland connection failed while typing")?;
            if self.events.settled {
                return Ok(());
            }
            self.flush(deadline)?;
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            wait(guard.connection_fd(), libc::POLLIN, deadline)?;
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error).context("Wayland connection failed while typing"),
            }
        }
    }

    fn flush(&self, deadline: Instant) -> Result<()> {
        loop {
            match self.connection.flush() {
                Ok(()) => return Ok(()),
                Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    wait(self.connection.backend().poll_fd(), libc::POLLOUT, deadline)?;
                }
                Err(error) => return Err(error).context("Wayland connection failed while typing"),
            }
        }
    }
}

/// Polls the socket until it is ready (or hung up — the next I/O reports that).
fn wait(fd: BorrowedFd<'_>, events: libc::c_short, deadline: Instant) -> Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(
            !remaining.is_zero(),
            "compositor stopped responding while typing"
        );
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events,
            revents: 0,
        };
        let timeout =
            libc::c_int::try_from(remaining.as_millis().max(1)).unwrap_or(libc::c_int::MAX);
        // SAFETY: one valid pollfd, borrowed for the duration of the call.
        match unsafe { libc::poll(&mut poll, 1, timeout) } {
            0 => {}
            ready if ready > 0 => return Ok(()),
            _ => {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error).context("unable to wait for the compositor");
                }
            }
        }
    }
}

/// Explicit opt-in hardware test seam; production never creates a uinput device.
#[cfg(test)]
pub struct VirtualKeyboard {
    dev: evdev::uinput::VirtualDevice,
}

#[cfg(test)]
impl VirtualKeyboard {
    pub fn new() -> Result<Self> {
        let mut keys = evdev::AttributeSet::new();
        for key in [
            evdev::KeyCode::KEY_A,
            evdev::KeyCode::KEY_Z,
            evdev::KeyCode::KEY_F24,
        ] {
            keys.insert(key);
        }
        let dev = evdev::uinput::VirtualDevice::builder()?
            .name("dictationapp test keyboard")
            .with_keys(&keys)?
            .build()?;
        Ok(Self { dev })
    }
    pub fn emit(&mut self, key: evdev::KeyCode, value: i32) -> Result<()> {
        self.dev.emit(&[*evdev::KeyEvent::new(key, value)])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes typed keys through each chunk's own compiled keymap.
    fn typed(chunks: &[Chunk]) -> String {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let mut out = String::new();
        for chunk in chunks {
            let keymap = xkb::Keymap::new_from_string(
                &context,
                chunk.keymap.clone(),
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .unwrap();
            let state = xkb::State::new(&keymap);
            for &key in &chunk.keys {
                match state.key_get_one_sym(xkb::Keycode::new(key + 8)) {
                    xkb::Keysym::Return => out.push('\n'),
                    keysym => out.push_str(&xkb::keysym_to_utf8(keysym)),
                }
            }
        }
        out
    }

    #[test]
    fn text_round_trips_through_printable_keys_only() {
        // Positions 13/14 are the upstream Backspace/Tab collisions, and well
        // over 47 distinct characters force further keymaps.
        let text = "abcdefghijklm?XYZ абвгдеёжзийклмнопрстуфхцчшщъыьэюя \
                    αβγδεζηθικλμνξοπρστυφχψωΑΒΓΔΕΖΗΘΙ Café — 👩🏽‍💻 !*\n\t-- -M ctrl";
        let chunks = plan(text).unwrap();
        assert!(chunks.len() > 2);
        let printable = TEXT_KEYS.into_iter().flatten().collect::<Vec<_>>();
        for key in chunks.iter().flat_map(|chunk| &chunk.keys) {
            assert!(
                printable.contains(key) || [15, 28, 57].contains(key),
                "key {key}"
            );
        }
        assert_eq!(typed(&chunks), text);
    }

    #[test]
    fn untypeable_characters_fail_closed() {
        for text in ["before\0after", "before\x1bafter", "before\rafter"] {
            assert!(plan(text).is_err(), "{text:?}");
        }
        assert!(plan("").unwrap().is_empty());
    }

    /// Run by scripts/check-typing.py inside an isolated compositor; it types
    /// into whatever window has focus, so it never runs on its own.
    #[test]
    #[ignore = "types into the focused window; run via scripts/check-typing.py"]
    fn types_into_focused_window() {
        let text = std::env::var("DICTATION_TEST_TYPE_TEXT")
            .expect("run via scripts/check-typing.py, which supplies the text");
        Typer::new()
            .unwrap()
            .type_str(&text, &AtomicBool::new(false))
            .unwrap();
    }
}
