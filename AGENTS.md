# dictationapp

Wispr Flow-style dictation for Linux/Wayland (niri). Hold Right Ctrl → speak → release →
transcript is typed into the focused window (and copied to clipboard).

## Build / run / test

```bash
cargo build                    # debug binary at target/debug/dictationapp
cargo install --path .         # release install to ~/.cargo/bin/dictationapp
cargo test                     # unit tests (evdev round-trip needs /dev/uinput + readable /dev/input)
cargo test -- --ignored        # network test (needs OPENROUTER_API_KEY + /tmp/speech.wav)
```

## Releasing

Push a `v*` tag matching Cargo.toml's version → `.github/workflows/release.yml`
builds on ubuntu-22.04, creates a GitHub release (x86_64 tarball + checksums),
then the `aur` job runs `packaging/aur/publish.sh` inside `archlinux:base-devel`
to update the `dictationapp` + `dictationapp-bin` AUR packages (clone or init
repo, fill pkgver/sha256, `makepkg --printsrcinfo` as unprivileged `builder`,
push to master). Skipped unless `AUR_SSH_PRIVATE_KEY` secret is set; optional
`AUR_USERNAME`/`AUR_EMAIL` set the AUR commit identity. PKGBUILD templates live
in `packaging/aur/` (MIT licensed — LICENSE ships in both packages and in the
release tarball). `packaging/dictationapp-settings.desktop` ships in the tarball
and both packages install it — it's how the settings window surfaces in app
launchers (Exec runs `dictationapp --settings`, i.e. IPC to the running instance).

Runtime logs go to stderr (`recording started`, `transcript:`, `typed N chars`, errors).

## Architecture

- `main.rs` — CLI dispatch (`--toggle/--start/--stop/--cancel/--quit`), GPUI app bootstrap,
  wires channel → IPC listener + evdev hotkey + uinput typer → transparent pill window.
- `app.rs` — GPUI pill UI + command pump (`cx.spawn` + `timer` poll of `mpsc::Receiver`),
  state machine Idle → Recording → Transcribing → Cleaning → Typing. Idle renders fully
  transparent; active recording shows a 21-sample waveform from measured input levels.
- `audio.rs` — cpal capture to mono f32 + hound WAV encode. `mic` config selects the input
  device by name (exact, else case-insensitive substring; unset = system default).
  `input_device_names()` lists streamable devices for the settings picker.
- `api.rs` — OpenRouter transcription (`POST /api/v1/audio/transcriptions`) followed optionally
  by conservative text cleanup (`POST /api/v1/chat/completions`); cleanup failure falls back to
  the raw transcript.
- `ipc.rs` — Unix socket at `$XDG_RUNTIME_DIR/dictationapp-$USER.sock`; stale socket takeover on bind.
- `hotkey.rs` — evdev: watches every readable `/dev/input/event*` supporting the hotkey.
  Hold mode: press→Start, release→Stop. Toggle mode: press→Toggle.
- `typer.rs` — evdev uinput virtual keyboard; types text into whatever window is focused.
- `beep.rs` — start/stop/error audio cues; WAVs generated once into `dirs::cache_dir()/dictationapp`,
  played via pw-play/paplay/aplay. `beeps = false` in config disables.
- `niri.rs` — moves the pill to bottom-center of its output via `niri msg`
  (niri 26.04's `default-floating-position` doesn't honor edge anchors here — verified visually).
- `settings.rs` — second gpui window (`dictationapp --settings`) with fields for API key,
  speech model, language, hotkey, mode, output, sound, and transcript cleanup. Save writes
  config.toml and sends `Command::Reload` — the pill hot-reloads.
- `config.rs` — `~/.config/dictationapp/config.toml` (+ `OPENROUTER_API_KEY` env wins).
  `cleanup` defaults to true and `cleanup_model` defaults to `inclusionai/ling-3.0-flash`.
  Note: honors XDG_CONFIG_HOME first, falls back to `~/.config` — T3 Code shells override XDG.

## System integration (niri)

`~/.config/niri/cfg/rules.kdl` — two window-rules on `app-id="dictationapp"`, distinguished
by **title** (`window.set_window_title` is required — `titlebar.title` alone never reaches
the toplevel on Wayland):
- `title="^Dictation$"` (the pill): `open-floating true`, `open-focused false`,
  `min/max-width 320`, `min/max-height 64`, plus `blur false` and disabled focus ring/border.
  The effect overrides the global blur rule: otherwise niri renders a noisy rectangle behind the
  fully transparent idle toplevel. Min/max pinning is required because niri does not honor the
  client's requested size for floating windows.
- `title="Dictation Settings"`: `open-floating true`, `min/max 460x760` (floats open centered).

`~/.config/niri/cfg/keybinds.kdl` — no dictation bind (a `Super+D` toggle was removed: stray
toggles caused invisible recordings). Re-add a `spawn ".../dictationapp" "--toggle"` inside the
`binds {}` block if wanted — trailing `}` is the block's close.

`~/.config/niri/cfg/autostart.kdl` — `spawn-at-startup` the installed binary.

## Gotchas

- Hotkey events pass through to the focused app too (no device grab — grabbing would block
  normal typing). RightCtrl is inert in practice.
- If several devices report the same key, Hold mode dedupes naturally (Start is a no-op while
  recording). Toggle mode could double-fire — set `hotkey` to a key unique to one device.
- `/dev/input/event*` needs read access (user ACL); `/dev/uinput` needs write access.
  Without them: IPC + pill-click still work; typing degrades to clipboard-only.
- The idle pill remains a mapped 320x64 Wayland toplevel but renders alpha-zero; keep its niri
  blur/focus-ring/border overrides or compositor effects reveal the invisible surface.
- The pill opens without focus so typed text lands in the previously focused app. Closing the
  settings window can focus the idle toplevel; refocus the destination app before dictating.
- `type_text = false` in config → clipboard-only mode.
- The hotkey watcher rescans /dev/input every second, so hot-plugged or
  suspend/resume-recreated keyboards are picked up within ~1s; devices that fail
  to open (e.g. ACL not yet applied) are retried each scan.
