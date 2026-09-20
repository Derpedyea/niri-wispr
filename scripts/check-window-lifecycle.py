#!/usr/bin/env python3
"""Check real window lifetime in an isolated headless niri session."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import traceback


def nested(binary, state):
    assert os.environ["NIRI_SOCKET"] != os.environ["DICTATION_TEST_PARENT_NIRI"]

    def niri(*args):
        return subprocess.check_output(["niri", "msg", *args], text=True)

    def report(message):
        with (state / "checks.log").open("a") as log:
            print(message, file=log)

    runtime = state / "runtime"
    config = state / "config"
    runtime.mkdir(mode=0o700)
    (config / "dictationapp").mkdir(parents=True)
    (config / "dictationapp/config.toml").write_text(
        'api_key = ""\nhotkey = "INVALID_TEST_KEY"\ntype_text = false\nbeeps = false\n'
    )
    socket = Path(os.environ["WAYLAND_DISPLAY"])
    if not socket.is_absolute():
        socket = Path(os.environ["XDG_RUNTIME_DIR"]) / socket
    env = dict(os.environ, WAYLAND_DISPLAY=str(socket),
               XDG_RUNTIME_DIR=str(runtime), XDG_CONFIG_HOME=str(config))
    env.pop("OPENROUTER_API_KEY", None)
    app = None
    log_path = state / "app.log"

    try:
        with log_path.open("w") as log:
            app = subprocess.Popen([binary], env=env, stdout=log, stderr=log)

        def wait_for(description, predicate, timeout=8):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                assert app.poll() is None, "Dictation exited unexpectedly"
                if predicate():
                    report("PASS " + description)
                    return
                time.sleep(.05)
            raise AssertionError(description)

        def command(flag):
            subprocess.run([binary, flag], env=env, check=True, timeout=5)

        def windows(title):
            return [w for w in json.loads(niri("-j", "windows"))
                    if w["pid"] == app.pid and w["title"] == title]

        wait_for("IPC ready", lambda: bool(list(runtime.glob("dictationapp-*.sock"))))
        command("--reload")
        wait_for("command pump ready", lambda: "handling Reload" in log_path.read_text())
        assert not windows("Dictation"), "Idle Dictation leaves a native window mapped"
        report("PASS no idle window")

        for dismissal in ("reload", "expiry"):
            command("--start")
            wait_for("message opens a pill", lambda: bool(windows("Dictation")))
            if dismissal == "reload":
                command("--reload")
            wait_for(f"{dismissal} removes the pill", lambda: not windows("Dictation"))

        command("--settings")
        wait_for("settings opens without a pill", lambda: bool(windows("Dictation Settings")))
        window_id = windows("Dictation Settings")[0]["id"]
        niri("action", "close-window", "--id", str(window_id))
        wait_for("settings closes", lambda: not windows("Dictation Settings"))
        command("--start")
        wait_for("daemon reopens pill after its last window closes",
                 lambda: bool(windows("Dictation")))
        command("--reload")
        wait_for("idle removes the reopened pill", lambda: not windows("Dictation"))
        command("--quit")
        assert app.wait(timeout=5) == 0, "Explicit quit failed"
        report("PASS explicit quit")
        (state / "passed").touch()
    except Exception:
        report(traceback.format_exc())
        raise
    finally:
        if app is not None and app.poll() is None:
            app.terminate()
            app.wait(timeout=5)
        # This socket belongs only to the nested compositor (asserted above).
        subprocess.run(["niri", "msg", "action", "quit", "--skip-confirmation"],
                       check=False, timeout=5)


def main():
    if len(sys.argv) == 4 and sys.argv[1] == "--nested":
        nested(sys.argv[2], Path(sys.argv[3]))
        return
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/dictationapp").resolve()
    assert binary.is_file(), f"Build the binary first: {binary}"
    for tool in ("niri", "gamescope"):
        assert shutil.which(tool), f"Missing test dependency: {tool}"
    with tempfile.TemporaryDirectory(prefix="dictation-window-check-") as directory:
        state = Path(directory)
        config = state / "niri.kdl"
        config.write_text('''
hotkey-overlay { skip-at-startup; }
animations { off; }
window-rule {
    match app-id="dictationapp" title="^Dictation$"
    open-floating true
    open-focused false
    min-width 320
    max-width 320
    min-height 64
    max-height 64
}
''')
        env = dict(os.environ, DICTATION_TEST_PARENT_NIRI=os.environ.get("NIRI_SOCKET", ""),
                   DISABLE_GAMESCOPE_WSI="1", WINIT_UNIX_BACKEND="x11")
        command = ["gamescope", "--backend", "headless", "-W1280", "-H900", "--",
                   "sh", "-c", 'unset WAYLAND_DISPLAY; exec "$@"', "test-niri",
                   "niri", "--config", str(config), "--", sys.executable,
                   str(Path(__file__).resolve()), "--nested", str(binary), str(state)]
        with (state / "session.log").open("w") as log:
            subprocess.run(command, env=env, stdout=log, stderr=log, timeout=40, check=False)
        output = (state / "session.log").read_text()
        if (state / "checks.log").exists():
            print((state / "checks.log").read_text(), end="")
        if not (state / "passed").exists():
            print(output)
            if (state / "app.log").exists():
                print((state / "app.log").read_text())
            raise SystemExit("FAIL native window lifecycle")


if __name__ == "__main__":
    main()
