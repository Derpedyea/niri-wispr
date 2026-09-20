# Local GPUI patch

This is the published `gpui` 0.2.2 crate from crates.io, licensed under
Apache-2.0 (see `LICENSE-APACHE`). Its `.cargo_vcs_info.json` identifies Zed
commit `69e2130295c2649963eb639fc70b4f2ee8ea1624`, `crates/gpui`, with the
upstream crate's dirty flag retained. The published crate is the baseline,
not an assumed clean checkout of that commit.

Examples and their explicit Cargo targets, the crate's lockfile, and Cargo's
registry marker are omitted. The application uses its own committed lockfile.
The source changes are recorded in `background-lifetime.patch`.

## Background application lifetime

Dictation must remove its native window when idle. A transparent window still
intercepts clicks on niri; even an empty Wayland input region leaves the tile's
activation area in the compositor. GPUI 0.2.2 otherwise stops its Linux event
loop as soon as the last window closes.

`App::set_quit_on_last_window_close(false)` opts a background app out of that
automatic shutdown on both Wayland and X11. The default remains `true`, and
explicit `App::quit()` still stops the loop. Other platforms keep their
existing behavior. Dictation retains its model separately from the window.

When updating GPUI, look for an equivalent upstream lifetime API first. If it
exists, remove this patch and the Cargo override. Otherwise reapply only this
small change and run the native window lifecycle check in
`../../docs/window-lifecycle.md`.
