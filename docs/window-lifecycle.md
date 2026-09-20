# Idle windows and native verification

Idle Dictation must have **no native pill window**. Transparency is not enough:
the old 320×64 floating toplevel covered other apps' controls, particularly
after maximizing them in niri. Even an empty Wayland input region left niri's
tile activation rectangle able to take focus and consume clicks.

The command pump keeps the model alive independently of windows. It opens a
pill for recording, processing, or a timed message, then removes that window
when idle. Window creation happens outside the entity update because GPUI
immediately renders the root view. The small vendored GPUI lifetime opt-out
keeps the Linux event loop alive after closing the last window. Explicit quit
and manually closing the visible pill still exit; closing Settings does not.

## Automated native check

Requires Python 3, niri, gamescope, and a working Vulkan driver:

```sh
cargo build --locked
python3 scripts/check-window-lifecycle.py target/debug/dictationapp
```

This starts a separate niri inside headless gamescope. It uses temporary IPC
and configuration directories, disables the hotkey, sound and text injection,
and clears the API key. `--start` therefore shows the real missing-key message
without recording or contacting a service. The check verifies no idle window,
repeated message-window creation/removal, timed dismissal, survival after the
last Settings window closes, and explicit quit. It never moves the real
desktop pointer or changes the user's settings.

The installed pre-fix binary fails the initial no-window assertion. The
correct seam is the actual compositor window list and process lifetime, not
a unit test of `pill_visible()`.

## Original click reproduction

The fix was also checked with Lords' actual Build/Zoning controls inside the
isolated compositor, using native Wayland virtual-pointer presses. The game
was maximized from a half-width column with 125% monitor scaling, 70% UI scale,
and 50% render scale. The old binary captured clicks and took focus over the
toolbar while idle. With the fix, six idle-state toolbar clicks across three
idle periods reached the game and retained focus; two visible-pill clicks
correctly reached Dictation. Closing the last Settings window left the daemon
running, and `--quit` terminated it. Result: eight native clicks, zero failures.
