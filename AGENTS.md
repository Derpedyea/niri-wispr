# dictationapp

Wispr Flow-style dictation for Linux/Wayland (niri). Hold Right Ctrl → speak → release →
transcript is typed into the focused window (and copied to clipboard).

## Build / run / test

```bash
cargo build                    # debug binary at target/debug/dictationapp
cargo install --path .         # release install to ~/.cargo/bin/dictationapp
cargo test                     # deterministic tests; no desktop input
cargo test -- --ignored        # opt-in hardware/input + network tests
cd ui && bun install           # once; debug builds then run ui/src with bun
cd ui && bun run typecheck
cd ui && bun run build         # target/release/dictationapp-ui, beside the release binary
cd ui && bun run install-local # ~/.cargo/bin/dictationapp-ui, beside cargo install
python3 scripts/check-window-lifecycle.py target/debug/dictationapp  # headless niri
```

Windows are found as `$DICTATIONAPP_UI`, else `dictationapp-ui` beside the
executable, else `/usr/lib/dictationapp/dictationapp-ui`; debug builds fall back
to `bun ui/src/main.tsx`. Never `strip` it: a stripped Bun executable is plain bun.

## Releasing

Push a `v*` tag matching Cargo.toml's version → `.github/workflows/release.yml`
builds on ubuntu-22.04 (cargo + `bun run build` in `ui/`), creates a GitHub
release (x86_64 tarball with `dictationapp` + `dictationapp-ui`, checksums),
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

- `main.rs` — CLI dispatch (`--toggle/--start/--stop/--cancel/--quit`), wires channel →
  IPC listener + evdev hotkey + Wayland typer + tray icon, then runs `Dictation` on the
  main thread until `Quit`. No GUI toolkit in the daemon.
- `app.rs` — the command loop (`recv_timeout` on the `Command` channel, 40ms tick for cues,
  waveform, message expiry) + state machine Idle → Recording → Transcribing → Cleaning →
  Typing. Network/clipboard/typing work runs on threads that hand an `Internal` result
  back and send `Command::Wake`. Capture starts on Start, but the beep and pill — or a
  start failure (no API key, no mic) — wait `hotkey::MIN_HOLD` (250ms), so taps and
  shortcuts the hotkey cancels before then stay invisible. The pill process starts warm
  with capture and opens its window on the first view at the cue; it's killed when idle.
  A start during Transcribing/Cleaning/Typing is held and records once delivery ends,
  unless its gesture (release, `--stop`, second toggle) ends first.
  Active recording shows a 21-sample waveform from measured input levels.
- `ui.rs` — window processes: `dictationapp-ui <pill|settings>` gets one JSON view per line
  on stdin and answers JSON events on stdout (non-JSON stdout lines are logged). A writer
  thread with a small queue keeps a stalled window from blocking the loop; unchanged views
  aren't resent. Dropping a `Window` kills its process.
- `ui/` — Bun + [GPUIX](https://github.com/remorses/gpuix) (React → Zed's GPUI) windows:
  `pill.tsx` (layer-shell overlay, namespace `dictationapp-pill`, anchored bottom with an
  80px margin, keyboard interactivity none), `settings.tsx` (a view of `settings.rs`; text
  fields keep local state while focused so late views can't rewind typing), `link.ts`
  (pipe protocol; reroutes `console.log` to stderr), `compiled.ts` + `native.ts` (the
  `--compile` entry embeds the renderer addon, which napi-rs's dynamic require hides from
  the bundler; running sources, Bun refuses that import, so `main.tsx` stays without it). GPUIX is one
  window per process and exits when its window closes — the reason for the process model.
- `audio.rs` — cpal capture to mono f32 + hound WAV encode. `mic` config selects the input
  device by name (exact, else case-insensitive substring; unset = system default) through
  `match_mic`, which the settings picker shares so both agree on which device "USB" means.
  `input_device_names()` lists sound-card capture PCMs (`hw:`/`plughw:`/`front:`/`sysdefault:`)
  for the settings picker — not ALSA plugins — without test-opening them: a mic the
  sound server is briefly holding (right after a dictation) must not vanish from the list.
- `api.rs` — OpenRouter transcription (`POST /api/v1/audio/transcriptions`) followed optionally
  by conservative text cleanup (`POST /api/v1/chat/completions`); cleanup failure falls back to
  the raw transcript.
- `ipc.rs` — Unix socket at `$XDG_RUNTIME_DIR/dictationapp-$USER.sock`; ordered,
  bounded command dispatch acknowledges enqueue before the CLI returns. A retained
  `Server` owns the worker and a persistent sibling `.lock` inode through shutdown;
  stale sockets are recovered under that lock. Never unlink the lock file.
- `hotkey.rs` — evdev: watches every readable `/dev/input/event*` supporting the hotkey.
  One hold state is shared across devices. Hold mode: press→Start, release→Stop (Cancel if
  held under `MIN_HOLD` — timed here from key events, since the app sees commands late), any other
  key during the hold→Cancel (it's a shortcut like RightCtrl+C); a Ctrl/Shift/Alt/Super already
  held at press makes it a shortcut too (Ctrl+C with hotkey C). Toggle mode: release of a
  lone tap→Toggle. No time debounce — one dropped a quick tap's release and left recordings running.
  `KeyCapture` (Settings' "press a key") is answered by the watcher from the same event
  stream, and the captured press counts as a chord, so pressing the current hotkey there
  never starts a dictation. Esc stays pending for Settings to cancel, but can't drive the
  hotkey while a capture waits. Dropping the handle withdraws the request.
- `typer.rs` — in-process Wayland virtual keyboard (`zwp_virtual_keyboard_v1`) with
  per-transcript keymaps: independent of physical layout/CapsLock and invisible to evdev.
  Text only borrows printable keycodes — Chromium/Electron/terminals act on Backspace,
  Escape, F-keys etc. by scancode whatever the keysym (why not wtype: atx/wtype#71).
  Keys are paced: niri disconnects a client whose socket fills. Verify against a real
  client with `scripts/check-typing.py` (headless niri + Chrome).
- `beep.rs` — start/stop/error audio cues; WAVs generated once into `dirs::cache_dir()/dictationapp`,
  played via pw-play/paplay/aplay. `beeps = false` in config disables.
- `niri.rs` — focuses the open Settings window by pid + title (a second open has no input
  serial, so niri would ignore its activation) and reports the focused window's pid, which
  a pending key capture checks. The pill needs nothing: niri puts a new layer surface on
  the focused output. A pill kept visible across recordings (a notice, then a new
  recording) stays on the output it opened on.
- `settings.rs` — Settings model + `Session` (`dictationapp --settings`, tray click): shortcut (set by
  pressing the key; letters/Space are rejected), mode, mic, API key, model presets + Custom,
  language, cleanup, typing, sounds. Every change saves at once and sends `Command::Reload`;
  there is no Save button. Edits file values only (`Config::load_file`), so an
  `OPENROUTER_API_KEY` is shown read-only and never written into the file. GPUIX reports no
  window activation, so: rereads the file when its mtime moves away from the last read/save
  here (checked each second) or on reopen, and re-lists mics every 2s on a thread — outside
  edits aren't overwritten and new mics appear; an unreadable file pauses saving. A text
  field taking focus (`editing` event) ends any pending key capture, and so does niri
  focusing another window. One window: a second open focuses it via `niri msg`.
- `tray.rs` — StatusNotifierItem via `ksni` (blocking API on the async-io zbus already in the
  tree). Symbolic mic icon, which Noctalia tints to its bar color; while a cued recording
  runs, status NeedsAttention + a procedurally drawn red-mic *pixmap* — Noctalia tints every
  symbolic SVG, so only a pixmap can be red there. Click → Settings, menu → Settings/Quit.
  `assume_sni_available` so it waits for the bar instead of failing at niri startup.
- `config.rs` — `~/.config/dictationapp/config.toml` (+ `OPENROUTER_API_KEY` env wins).
  `cleanup` defaults to true and `cleanup_model` defaults to `inclusionai/ling-3.0-flash`.
  Note: honors XDG_CONFIG_HOME first, falls back to `~/.config` — T3 Code shells override XDG.

## System integration (niri)

`~/.config/niri/cfg/rules.kdl` — one window-rule, `app-id="dictationapp"`
`title="Dictation Settings"`: `open-floating true`, `min/max 460x760` (floats open centered).
The layout is sized to fit 760px with ~3 mics; more scroll. The pill is a layer surface
(`layer-rule` matches `namespace="dictationapp-pill"` if effects ever need overriding); old
`title="^Dictation$"` window-rules are now inert.

`~/.config/niri/cfg/keybinds.kdl` — no dictation bind (a `Super+D` toggle was removed: stray
toggles caused invisible recordings). Re-add a `spawn ".../dictationapp" "--toggle"` inside the
`binds {}` block if wanted — trailing `}` is the block's close.

`~/.config/niri/cfg/autostart.kdl` — `spawn-at-startup` the installed binary.

## Gotchas

- Hotkey events pass through to the focused app too (no device grab — grabbing would block
  normal typing). RightCtrl is inert in practice.
- If several devices report the same key, overlapping holds count as one gesture.
- `/dev/input/event*` needs read access (user ACL); typing needs the compositor's
  Wayland virtual keyboard protocol, else clipboard-only. `/dev/uinput` is only used
  by opt-in hardware tests.
- Chromium truncates keysyms past U+FFFF, so typed emoji arrive mangled there
  (any keyboard-based typer does this); the clipboard copy is exact.
- Never leave an idle surface mapped: an alpha-zero toplevel blocked underlying buttons and
  stole focus on niri, even with an empty input region. Idle must mean no pill process.
  See `docs/window-lifecycle.md` before changing window lifetime.
- The pill never takes keyboard focus so typed text lands in the previously focused app.
- `niri msg layers` lists a layer surface before its first frame; wait a frame before
  screenshotting one.
- Headless checks isolate `XDG_RUNTIME_DIR`, which hides PipeWire; set
  `PIPEWIRE_RUNTIME_DIR`/`PULSE_RUNTIME_PATH` to the real one to record there.
- `type_text = false` in config → clipboard-only mode.
- The hotkey watcher rescans /dev/input every second, so hot-plugged or
  suspend/resume-recreated keyboards are picked up within ~1s; devices that fail
  to open (e.g. ACL not yet applied) are retried each scan.
