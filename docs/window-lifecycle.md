# Idle windows and native verification

Idle Dictation must have **no pill surface**. Transparency is not enough:
the old 320×64 floating toplevel covered other apps' controls, particularly
after maximizing them in niri. Even an empty Wayland input region left niri's
tile activation rectangle able to take focus and consume clicks.

Windows are separate GPUIX processes (`ui/`), one window each, so a window
exists exactly as long as its process. The daemon starts the pill process
when capture starts (to hide its ~100ms startup), sends the first view when
the recording is cued, and kills it once idle. The pill is a layer-shell
overlay with no keyboard interactivity, placed by the compositor. Settings is
an ordinary toplevel; closing it ends only its process. Explicit `--quit`
stops the daemon, which kills any window processes.

## Automated native check

Requires Python 3, niri, gamescope, Bun (`cd ui && bun install`) and a working
Vulkan driver:

```sh
cargo build --locked
python3 scripts/check-window-lifecycle.py target/debug/dictationapp
# the compiled windows instead of ui/src:
(cd ui && bun run build)
DICTATIONAPP_UI=$PWD/target/release/dictationapp-ui python3 scripts/check-window-lifecycle.py target/debug/dictationapp
```

This starts a separate niri inside headless gamescope. It uses temporary IPC
and configuration directories, disables the hotkey, sound and text injection,
and clears the API key. `--start` therefore shows the real missing-key message
without recording or contacting a service. The check verifies no idle surface,
repeated pill creation/removal, timed dismissal, one reused Settings window,
survival after Settings closes, and explicit quit. It never moves the real
desktop pointer or changes the user's settings.

The correct seam is the compositor's actual window and layer lists and
process lifetime, not a unit test of `pill_visible()`.
