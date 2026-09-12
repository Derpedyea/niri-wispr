use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode, SynchronizationCode};
use std::thread;
use std::time::Duration;

/// A virtual keyboard that types into whatever window is focused.
pub struct Typer {
    dev: VirtualDevice,
}

fn key_table() -> &'static [(char, KeyCode, bool)] {
    &[
        ('a', KeyCode::KEY_A, false),
        ('b', KeyCode::KEY_B, false),
        ('c', KeyCode::KEY_C, false),
        ('d', KeyCode::KEY_D, false),
        ('e', KeyCode::KEY_E, false),
        ('f', KeyCode::KEY_F, false),
        ('g', KeyCode::KEY_G, false),
        ('h', KeyCode::KEY_H, false),
        ('i', KeyCode::KEY_I, false),
        ('j', KeyCode::KEY_J, false),
        ('k', KeyCode::KEY_K, false),
        ('l', KeyCode::KEY_L, false),
        ('m', KeyCode::KEY_M, false),
        ('n', KeyCode::KEY_N, false),
        ('o', KeyCode::KEY_O, false),
        ('p', KeyCode::KEY_P, false),
        ('q', KeyCode::KEY_Q, false),
        ('r', KeyCode::KEY_R, false),
        ('s', KeyCode::KEY_S, false),
        ('t', KeyCode::KEY_T, false),
        ('u', KeyCode::KEY_U, false),
        ('v', KeyCode::KEY_V, false),
        ('w', KeyCode::KEY_W, false),
        ('x', KeyCode::KEY_X, false),
        ('y', KeyCode::KEY_Y, false),
        ('z', KeyCode::KEY_Z, false),
        ('A', KeyCode::KEY_A, true),
        ('B', KeyCode::KEY_B, true),
        ('C', KeyCode::KEY_C, true),
        ('D', KeyCode::KEY_D, true),
        ('E', KeyCode::KEY_E, true),
        ('F', KeyCode::KEY_F, true),
        ('G', KeyCode::KEY_G, true),
        ('H', KeyCode::KEY_H, true),
        ('I', KeyCode::KEY_I, true),
        ('J', KeyCode::KEY_J, true),
        ('K', KeyCode::KEY_K, true),
        ('L', KeyCode::KEY_L, true),
        ('M', KeyCode::KEY_M, true),
        ('N', KeyCode::KEY_N, true),
        ('O', KeyCode::KEY_O, true),
        ('P', KeyCode::KEY_P, true),
        ('Q', KeyCode::KEY_Q, true),
        ('R', KeyCode::KEY_R, true),
        ('S', KeyCode::KEY_S, true),
        ('T', KeyCode::KEY_T, true),
        ('U', KeyCode::KEY_U, true),
        ('V', KeyCode::KEY_V, true),
        ('W', KeyCode::KEY_W, true),
        ('X', KeyCode::KEY_X, true),
        ('Y', KeyCode::KEY_Y, true),
        ('Z', KeyCode::KEY_Z, true),
        ('1', KeyCode::KEY_1, false),
        ('2', KeyCode::KEY_2, false),
        ('3', KeyCode::KEY_3, false),
        ('4', KeyCode::KEY_4, false),
        ('5', KeyCode::KEY_5, false),
        ('6', KeyCode::KEY_6, false),
        ('7', KeyCode::KEY_7, false),
        ('8', KeyCode::KEY_8, false),
        ('9', KeyCode::KEY_9, false),
        ('0', KeyCode::KEY_0, false),
        ('!', KeyCode::KEY_1, true),
        ('@', KeyCode::KEY_2, true),
        ('#', KeyCode::KEY_3, true),
        ('$', KeyCode::KEY_4, true),
        ('%', KeyCode::KEY_5, true),
        ('^', KeyCode::KEY_6, true),
        ('&', KeyCode::KEY_7, true),
        ('*', KeyCode::KEY_8, true),
        ('(', KeyCode::KEY_9, true),
        (')', KeyCode::KEY_0, true),
        ('-', KeyCode::KEY_MINUS, false),
        ('_', KeyCode::KEY_MINUS, true),
        ('=', KeyCode::KEY_EQUAL, false),
        ('+', KeyCode::KEY_EQUAL, true),
        ('[', KeyCode::KEY_LEFTBRACE, false),
        ('{', KeyCode::KEY_LEFTBRACE, true),
        (']', KeyCode::KEY_RIGHTBRACE, false),
        ('}', KeyCode::KEY_RIGHTBRACE, true),
        (';', KeyCode::KEY_SEMICOLON, false),
        (':', KeyCode::KEY_SEMICOLON, true),
        ('\'', KeyCode::KEY_APOSTROPHE, false),
        ('"', KeyCode::KEY_APOSTROPHE, true),
        ('`', KeyCode::KEY_GRAVE, false),
        ('~', KeyCode::KEY_GRAVE, true),
        ('\\', KeyCode::KEY_BACKSLASH, false),
        ('|', KeyCode::KEY_BACKSLASH, true),
        (',', KeyCode::KEY_COMMA, false),
        ('<', KeyCode::KEY_COMMA, true),
        ('.', KeyCode::KEY_DOT, false),
        ('>', KeyCode::KEY_DOT, true),
        ('/', KeyCode::KEY_SLASH, false),
        ('?', KeyCode::KEY_SLASH, true),
        (' ', KeyCode::KEY_SPACE, false),
        ('\n', KeyCode::KEY_ENTER, false),
        ('\t', KeyCode::KEY_TAB, false),
    ]
}

impl Typer {
    pub fn new() -> Result<Typer> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for k in [
            KeyCode::KEY_LEFTSHIFT,
            KeyCode::KEY_RIGHTSHIFT,
            KeyCode::KEY_LEFTCTRL,
            KeyCode::KEY_RIGHTCTRL,
            KeyCode::KEY_LEFTALT,
            KeyCode::KEY_RIGHTALT,
            KeyCode::KEY_BACKSPACE,
            KeyCode::KEY_ESC,
        ] {
            keys.insert(k);
        }
        for &(_, key, _) in key_table() {
            keys.insert(key);
        }

        let mut dev = VirtualDevice::builder()
            .context("failed to open /dev/uinput")?
            .name("dictationapp virtual keyboard")
            .with_keys(&keys)
            .context("failed to register keys on virtual keyboard")?
            .build()
            .context("failed to create virtual keyboard")?;

        // Wait until the device node is fully registered so the compositor sees it.
        let _ = dev.enumerate_dev_nodes_blocking();
        Ok(Typer { dev })
    }

    /// Press (value=1) or release (value=0) a key on the virtual keyboard.
    pub fn emit(&mut self, key: KeyCode, value: i32) -> Result<()> {
        self.dev.emit(&[
            *evdev::KeyEvent::new(key, value),
            InputEvent::new(
                EventType::SYNCHRONIZATION.0,
                SynchronizationCode::SYN_REPORT.0,
                0,
            ),
        ])?;
        Ok(())
    }

    fn tap(&mut self, key: KeyCode, shift: bool) -> Result<()> {
        if shift {
            self.emit(KeyCode::KEY_LEFTSHIFT, 1)?;
        }
        self.emit(key, 1)?;
        thread::sleep(Duration::from_millis(4));
        self.emit(key, 0)?;
        if shift {
            self.emit(KeyCode::KEY_LEFTSHIFT, 0)?;
        }
        thread::sleep(Duration::from_millis(2));
        Ok(())
    }

    /// Type a string into the currently focused window. Unmapped chars are skipped.
    pub fn type_str(&mut self, text: &str) -> Result<()> {
        for ch in text.chars() {
            if let Some(&(_, key, shift)) = key_table().iter().find(|(c, _, _)| *c == ch) {
                self.tap(key, shift)
                    .with_context(|| format!("failed to type '{ch}'"))?;
            }
        }
        Ok(())
    }
}
