# dictationapp

[![release](https://github.com/Derpedyea/niri-wispr/actions/workflows/release.yml/badge.svg)](https://github.com/Derpedyea/niri-wispr/actions/workflows/release.yml)
[![AUR](https://img.shields.io/aur/version/dictationapp)](https://aur.archlinux.org/packages/dictationapp)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

Push-to-talk dictation for Wayland, in the spirit of Wispr Flow. Hold a key,
say what you mean, let go — the words land in whatever window you were using.
Built for [niri](https://github.com/YaLTeR/niri); powered by
[OpenRouter](https://openrouter.ai).

A small pill tracks the state — recording (with a live waveform),
transcribing, cleaning up, typing — and fades to fully transparent when idle,
so it costs nothing to leave running.

## How it works

1. **Hotkey down** — recording starts, with a subtle beep.
2. **Hotkey up** — the audio is encoded to WAV and sent to OpenRouter's
   transcription endpoint.
3. **Cleanup pass** *(optional)* — a second model pass fixes punctuation,
   capitalization, and obvious recognition errors without touching your
   meaning. If it fails, the raw transcript is used instead.
4. **Delivery** — the result is typed into the focused window through a
   `uinput` virtual keyboard and also placed on the clipboard, so nothing is
   lost if a field refuses synthetic input.

## Install

### Arch Linux (AUR)

```bash
paru -S dictationapp-bin   # prebuilt binary (fast)
paru -S dictationapp       # build from source
```

### From source

```bash
git clone https://github.com/Derpedyea/niri-wispr
cd niri-wispr
cargo install --path .     # installs to ~/.cargo/bin/dictationapp
```

To start it with niri:

```kdl
spawn-at-startup "~/.cargo/bin/dictationapp"
```

## Requirements

- **Wayland** — developed on niri; the pill positions itself via `niri msg`
  (a graceful no-op on other compositors)
- **An OpenRouter API key** — set it in the settings window, in
  `config.toml`, or via `OPENROUTER_API_KEY`
- **Read access to `/dev/input/event*`** — for the global hotkey
- **Write access to `/dev/uinput`** — for typing; without it the app falls
  back to clipboard-only
- **`pw-play`, `paplay`, or `aplay`** — for the start/stop cues (any one of
  them; `beeps = false` silences this entirely)
- A microphone reachable through cpal's default input device

Granting `/dev/input` and `/dev/uinput` access is distro-specific — typically
the `input` group plus a udev rule, or logind ACLs.

## Usage

Run `dictationapp` once — it stays resident, shows the pill, and listens for
the hotkey and for commands on a Unix socket
(`$XDG_RUNTIME_DIR/dictationapp-$USER.sock`):

```bash
dictationapp --toggle     # start/stop dictation
dictationapp --start      # start recording
dictationapp --stop       # stop and transcribe
dictationapp --cancel     # throw the current recording away
dictationapp --settings   # open the settings window
dictationapp --reload     # re-read config.toml
dictationapp --quit       # exit
dictationapp --record <out.wav> [secs]   # save mic audio to a WAV (debug)
```

Clicking the pill also toggles recording. When the pill has focus:
`Space`/`Enter` toggles, `Esc` cancels, `Ctrl-Q` quits.

## Configuration

`dictationapp --settings` opens a GUI that edits the config and hot-reloads
the running app — you never have to touch the file. It lives at
`~/.config/dictationapp/config.toml`:

```toml
api_key = "sk-or-..."            # or set OPENROUTER_API_KEY (env wins)
model = "fish-audio/transcribe-1"
language = "en"                  # optional; omit to auto-detect
mode = "hold"                    # "hold" or "toggle"
hotkey = "KEY_RIGHTCTRL"         # any evdev key name, e.g. KEY_CAPSLOCK
type_text = true                 # false = clipboard only
beeps = true                     # start/stop/error cues
cleanup = true                   # LLM pass that tidies the transcript
cleanup_model = "inclusionai/ling-3.0-flash"
```

- **`hold`** — press and hold the hotkey; release to transcribe.
- **`toggle`** — press once to start, again to stop.

## niri setup

Two window rules make the pill behave like an overlay. They're matched by
`app-id` and title (both set in code, so rules only need to match):

```kdl
// Pill — invisible when idle, never takes focus
window-rule {
    match app-id="dictationapp" title="^Dictation$"
    open-floating true
    open-focused false
    min-width 320
    max-width 320
    min-height 64
    max-height 64
    // disable compositor effects that would reveal the transparent surface:
    // blur off, focus ring / border off
}

// Settings — an ordinary centered floating window
window-rule {
    match app-id="dictationapp" title="Dictation Settings"
    open-floating true
    min-width 460
    max-width 460
    min-height 760
    max-height 760
}
```

The min/max pinning is load-bearing: niri ignores the client's requested size
for floating windows, and without the blur/focus-ring overrides the
compositor paints effects behind the transparent idle window.

An optional compositor-level toggle:

```kdl
binds {
    Super+D { spawn "dictationapp" "--toggle"; }
}
```

## Troubleshooting

- **Nothing types** — check `/dev/uinput` write access. Transcripts still
  reach the clipboard; `type_text = false` silences the startup warning.
- **Hotkey does nothing** — check read access to `/dev/input/event*`. IPC
  (`--toggle`, clicking the pill) works regardless. Devices plugged in after
  startup aren't picked up; restart the app.
- **The hotkey isn't swallowed** — there's no device grab (grabbing would
  break normal typing), so the key also reaches the focused app. Pick an
  inert key like Right Ctrl.
- **Toggle mode double-fires** — several devices can report the same key.
  Set `hotkey` to a key only one device emits.
- **Hear what the model hears** — `dictationapp --record /tmp/speech.wav 5`
  and play it back.
- Runtime logs go to stderr: `recording started`, `transcript: ...`,
  `typed N chars`, errors.

## Releasing

The tag is the release — CI does the rest:

```bash
# bump `version` in Cargo.toml first, then:
git tag v0.2.0
git push origin v0.2.0
```

`.github/workflows/release.yml` builds the binary on Ubuntu, verifies the tag
matches `Cargo.toml`, and publishes a GitHub release with a
`x86_64-linux-gnu` tarball and checksums.

### Auto-publishing to the AUR

The same workflow then updates two AUR packages — `dictationapp` (built from
source) and `dictationapp-bin` (the release tarball) — via
`packaging/aur/publish.sh`. One-time setup:

1. Create an account at <https://aur.archlinux.org/register>.
2. Generate a dedicated key pair:

   ```bash
   ssh-keygen -t ed25519 -f ~/.ssh/aur
   ```

3. In your AUR account settings, add `~/.ssh/aur.pub` under **SSH Public
   Key**.
4. In this repo's **Settings → Secrets and variables → Actions**:
   - Secret `AUR_SSH_PRIVATE_KEY` — contents of `~/.ssh/aur` (required)
   - Secret `AUR_USERNAME` — your AUR username (optional; commit author)
   - Variable `AUR_EMAIL` — your AUR email (optional; commit author)
5. Push a tag. On the first run the packages are created automatically
   (pushing to a nonexistent pkgbase creates it), so `dictationapp` and
   `dictationapp-bin` get claimed under your account.

If `AUR_SSH_PRIVATE_KEY` isn't set, the AUR job skips itself — the GitHub
release still happens.

## Development

```bash
cargo build                    # debug binary at target/debug/dictationapp
cargo test                     # unit tests
cargo test -- --ignored        # network test (needs OPENROUTER_API_KEY + /tmp/speech.wav)
```

Rust + [gpui](https://github.com/zed-industries/zed) (UI), cpal (audio),
hound (WAV), evdev/uinput (hotkey + typing), ureq (HTTP).

| File | Role |
| --- | --- |
| `main.rs` | CLI dispatch, GPUI bootstrap, wiring |
| `app.rs` | pill UI, state machine (Idle → Recording → Transcribing → Cleaning → Typing) |
| `audio.rs` | mic capture + WAV encode |
| `api.rs` | OpenRouter transcription + cleanup pass |
| `hotkey.rs` | evdev global hotkey watcher |
| `typer.rs` | uinput virtual keyboard |
| `ipc.rs` | Unix socket for CLI commands |
| `settings.rs` | settings window |
| `beep.rs` | audio cues |
| `niri.rs` | pill positioning via `niri msg` |
| `config.rs` | config.toml load/save |
| `packaging/aur/` | PKGBUILDs + the AUR publish script |

## License

[MIT](LICENSE) © Derpedyea
