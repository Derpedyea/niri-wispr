#!/usr/bin/env python3
"""Check publication, paste, focus and feedback in an isolated headless niri.

cargo test --no-run --message-format=json
python3 scripts/check-delivery.py <test-binary> <app-binary> --evidence .t3/pr-evidence

The fixture injects a completed transcript; microphone and API I/O are not run.
--before accepts the baseline fixture and proves the old clipboard survives.
"""

import argparse
import ctypes
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import traceback

TEXT = "Café — exact words.\nSecond line, ready to paste."
PREVIOUS = "Previous copy."
PAGE = """<!doctype html><meta charset=utf-8><title>Delivery check</title>
<style>body{background:#171b23;color:#edf0f7;font:18px sans-serif;margin:40px}
h1{font-size:24px;font-weight:500}textarea{box-sizing:border-box;width:100%;height:270px;
background:#202631;color:inherit;border:1px solid #68748a;border-radius:8px;padding:20px;
font:20px/1.5 sans-serif}p{color:#aeb9cc}pre{font:18px/1.5 sans-serif;white-space:pre-wrap}</style>
<h1>Draft</h1><textarea id=t autofocus aria-label=Draft></textarea>
<p>Clipboard</p><pre id=c></pre><script>
let gen=null; const t=document.getElementById('t');
(async function poll(){const s=await(await fetch('/state')).json();
if(s.gen!==gen){gen=s.gen;t.value='';t.focus()}
document.getElementById('c').textContent=s.clipboard;
await fetch('/report',{method:'POST',body:JSON.stringify({gen,value:t.value})});
setTimeout(poll,30)})();</script>"""


def compositor_window(display):
    """Find niri's X11 surface on gamescope's private display for recording."""
    x11 = ctypes.CDLL("libX11.so.6")
    window = ctypes.c_ulong
    x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
    x11.XOpenDisplay.restype = ctypes.c_void_p
    x11.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
    x11.XDefaultRootWindow.restype = window
    x11.XQueryTree.argtypes = [ctypes.c_void_p, window, ctypes.POINTER(window), ctypes.POINTER(window),
                              ctypes.POINTER(ctypes.POINTER(window)), ctypes.POINTER(ctypes.c_uint)]
    x11.XGetGeometry.argtypes = [ctypes.c_void_p, window, ctypes.POINTER(window),
        ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_uint),
        ctypes.POINTER(ctypes.c_uint), ctypes.POINTER(ctypes.c_uint), ctypes.POINTER(ctypes.c_uint)]
    x11.XFree.argtypes = [ctypes.c_void_p]
    x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
    connection = x11.XOpenDisplay(display.encode())
    assert connection, "Cannot open isolated gamescope display"
    try:
        root, parent, count = window(), window(), ctypes.c_uint()
        children = ctypes.POINTER(window)()
        assert x11.XQueryTree(connection, x11.XDefaultRootWindow(connection),
                             ctypes.byref(root), ctypes.byref(parent), ctypes.byref(children), ctypes.byref(count))
        candidates = []
        try:
            for ident in children[:count.value]:
                x, y = ctypes.c_int(), ctypes.c_int()
                width, height, border, depth = (ctypes.c_uint() for _ in range(4))
                assert x11.XGetGeometry(connection, ident, ctypes.byref(root), ctypes.byref(x), ctypes.byref(y),
                    ctypes.byref(width), ctypes.byref(height), ctypes.byref(border), ctypes.byref(depth))
                candidates.append((ident, width.value, height.value))
        finally:
            x11.XFree(children)
        assert candidates, "No compositor surface on isolated display"
        return max(candidates, key=lambda entry: entry[1] * entry[2])
    finally:
        x11.XCloseDisplay(connection)


def nested(test_binary, app_binary, state, evidence, before):
    assert os.environ["NIRI_SOCKET"] != os.environ["DICTATION_TEST_PARENT_NIRI"]
    reports, current = {}, {"gen": 0, "clipboard": PREVIOUS}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            body = PAGE if self.path == "/" else json.dumps(current)
            self.send_response(200)
            self.end_headers()
            self.wfile.write(body.encode())

        def do_POST(self):
            report = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            reports[report["gen"]] = report["value"]
            self.send_response(204)
            self.end_headers()

    def report(message):
        with (state / "checks.log").open("a") as log:
            print(message, file=log)

    def wait_for(description, predicate, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if predicate():
                report("PASS " + description)
                return
            time.sleep(.02)
        raise AssertionError(description)

    def niri(*args):
        return subprocess.check_output(["niri", "msg", *args], text=True)

    def windows():
        return json.loads(niri("-j", "windows"))

    def page_focused():
        window = json.loads(niri("-j", "focused-window"))
        return window and window["title"] == "Delivery check"

    def clipboard():
        return subprocess.check_output(["wl-paste", "--no-newline"], env=env, timeout=5).decode()

    def snapshot(name):
        path = evidence / name
        niri("action", "screenshot-screen", "--show-pointer", "false", "--path", str(path))
        wait_for("screenshot saved", path.is_file)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    socket = Path(os.environ["WAYLAND_DISPLAY"])
    if not socket.is_absolute():
        socket = Path(os.environ["XDG_RUNTIME_DIR"]) / socket
    env = dict(os.environ, WAYLAND_DISPLAY=str(socket))
    env.pop("OPENROUTER_API_KEY", None)
    chrome = app = recorder = None
    (state / "chrome").mkdir()
    (state / "chrome/First Run").touch()
    try:
        chrome = subprocess.Popen([
            "google-chrome-stable", "--ozone-platform=wayland", "--disable-gpu",
            "--no-first-run", "--no-default-browser-check", "--password-store=basic",
            "--disable-background-networking", f"--user-data-dir={state / 'chrome'}",
            f"--app=http://127.0.0.1:{server.server_port}/"], env=env,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        wait_for("target browser ready", lambda: 0 in reports and any(
            w["title"] == "Delivery check" for w in windows()))
        niri("action", "close-overview")
        cases = [("clipboard-only", False, TEXT), ("typing", True, TEXT),
                 ("typing-failure", True, "before\x1bafter"), ("copy-failure", True, TEXT)]
        for index, (name, typing, text) in enumerate(cases[:1] if before else cases):
            case = state / name
            (case / "runtime").mkdir(parents=True, mode=0o700)
            (case / "config/dictationapp").mkdir(parents=True)
            (case / "config/dictationapp/config.toml").write_text(
                f'type_text = {str(typing).lower()}\nbeeps = false\ncleanup = false\nhotkey = "INVALID_TEST_KEY"\n')
            trigger = case / "complete-transcript"
            app_env = dict(env, XDG_RUNTIME_DIR=str(case / "runtime"),
                           XDG_CONFIG_HOME=str(case / "config"),
                           DICTATION_TEST_TRANSCRIPT=text, DICTATION_TEST_TRIGGER=str(trigger))
            if name == "copy-failure":
                (case / "bin").mkdir()
                publisher = case / "bin/wl-copy"
                publisher.write_text("#!/bin/sh\nexit 1\n")
                publisher.chmod(0o700)
                app_env["PATH"] = f"{case / 'bin'}:{env['PATH']}"
            current.update(gen=index + 1, clipboard=PREVIOUS)
            subprocess.run(["wl-copy"], input=PREVIOUS.encode(), env=env, check=True, timeout=5)
            page = next(w for w in windows() if w["title"] == "Delivery check")
            niri("action", "focus-window", "--id", str(page["id"]))
            wait_for("target field reset", lambda: reports.get(index + 1) == "" and page_focused())
            log_path = case / "app.log"
            with log_path.open("w") as log:
                app = subprocess.Popen([test_binary, "--ignored", "--exact",
                    "app::tests::native_delivery_fixture", "--nocapture"],
                    env=app_env, stdout=log, stderr=log)
            wait_for("fixture IPC ready", lambda: bool(list((case / "runtime").glob("*.sock"))))
            running = Path(f"/proc/{app.pid}/exe")
            assert running.resolve() == Path(test_binary).resolve(), "Wrong fixture binary"
            digest = hashlib.sha256(running.read_bytes()).hexdigest()
            assert digest == (state / "binary.sha256").read_text(), "Fixture binary changed"
            report("PASS executing fixture sha256=" + digest)
            subprocess.run([app_binary, "--stop"], env=app_env, check=True, timeout=5)
            wait_for("fixture command pump ready", lambda: "handling Stop" in log_path.read_text())
            assert page_focused(), "Fixture stole focus"
            if name == "clipboard-only" and not before and os.environ.get("DICTATION_TEST_RECORD"):
                display = os.environ["DICTATION_TEST_OUTER_DISPLAY"]
                assert display != os.environ["DICTATION_TEST_PARENT_DISPLAY"], "Recorder would use desktop display"
                window_id, width, height = compositor_window(display)
                progress = case / "recording.progress"
                recorder_log = case / "recorder.log"
                with recorder_log.open("w") as log:
                    recorder = subprocess.Popen(["ffmpeg", "-y", "-f", "x11grab", "-framerate", "12",
                        "-window_id", str(window_id), "-video_size", f"{width}x{height}", "-draw_mouse", "0", "-i", display,
                        "-c:v", "libx264", "-pix_fmt", "yuv420p",
                        "-stats_period", "0.1", "-progress", str(progress),
                        str(evidence / "delivery.mp4")], stdin=subprocess.PIPE,
                        stdout=subprocess.DEVNULL, stderr=log)
                def recorded_frames():
                    assert recorder.poll() is None, recorder_log.read_text()
                    if not progress.exists():
                        return 0
                    frames = [int(line[6:]) for line in progress.read_text().splitlines() if line.startswith("frame=")]
                    return max(frames, default=0)
                wait_for("recording target ready", lambda: recorded_frames() > 0)
            trigger.touch()
            if before:
                wait_for("baseline delivery completed", lambda: "fixture delivered" in log_path.read_text())
                assert clipboard() == PREVIOUS, "Baseline unexpectedly exported transcript"
                assert not any(w["title"] == "Dictation" for w in windows())
                report("PASS reproduced baseline: clipboard unchanged, no completion notice")
            elif name == "copy-failure":
                wait_for("copy failure reported", lambda: "clipboard:" in log_path.read_text())
                wait_for("copy error visible", lambda: any(w["title"] == "Dictation" for w in windows()))
                assert clipboard() == PREVIOUS
                assert reports[index + 1] == "", "Failed copy typed text"
            else:
                wait_for("exact transcript published", lambda: clipboard() == text)
                current["clipboard"] = text
                if name == "typing":
                    wait_for("transcript inserted", lambda: reports[index + 1] == text)
                elif name == "typing-failure":
                    wait_for("typing failure reached", lambda: "typing:" in log_path.read_text()
                             and any(w["title"] == "Dictation" for w in windows()))
                    assert reports[index + 1] == ""
                    report("PASS full transcript survives typing failure")
                else:
                    wait_for("copied notice visible", lambda: any(w["title"] == "Dictation" for w in windows()))
            assert page_focused(), "Delivery stole target focus"
            assert all(w["title"] in {"Delivery check", "Dictation"} for w in windows()), "Helper window mapped"
            report(f"PASS {name}: target retains focus, no helper window")
            if name == "clipboard-only":
                subprocess.run([test_binary, "--ignored", "--exact",
                    "typer::tests::pastes_into_focused_window"],
                    env=dict(env, DICTATION_TEST_PASTE="1"), check=True, timeout=10,
                    stdout=subprocess.DEVNULL)
                expected = PREVIOUS if before else text
                wait_for("browser pastes clipboard", lambda: reports[index + 1] == expected)
                if not before:
                    wait_for("notice placed at bottom", lambda: any(
                        w["title"] == "Dictation" and
                        w["layout"]["tile_pos_in_workspace_view"][1] > 500 for w in windows()))
                # niri's screenshot action replaces the clipboard with its PNG.
                # Capture only after proving native clipboard read and paste.
                snapshot("before.png" if before else "after.png")
            if name == "copy-failure":
                wait_for("error placed at bottom", lambda: any(
                    w["title"] == "Dictation" and
                    w["layout"]["tile_pos_in_workspace_view"][1] > 500 for w in windows()))
                snapshot("copy-failure.png")
            if name == "typing-failure":
                wait_for("typing error placed at bottom", lambda: any(
                    w["title"] == "Dictation" and
                    w["layout"]["tile_pos_in_workspace_view"][1] > 500 for w in windows()))
                snapshot("typing-failure.png")
            wait_for("idle removes native pill", lambda: not any(w["title"] == "Dictation" for w in windows()))
            if recorder is not None:
                frame = recorded_frames()
                wait_for("idle frame recorded", lambda: recorded_frames() > frame + 3)
                recorder.stdin.write(b"q")
                recorder.stdin.flush()
                assert recorder.wait(timeout=10) == 0, "Native recording failed"
                recorder = None
            replacement = "New user copy."
            subprocess.run(["wl-copy"], input=replacement.encode(), env=env, check=True, timeout=5)
            subprocess.run([app_binary, "--quit"], env=app_env, check=True, timeout=5)
            assert app.wait(timeout=5) == 0
            app = None
            assert clipboard() == replacement, "Exit republished or cleared newer clipboard"
            report("PASS shutdown preserves replacement clipboard")
        (state / "passed").touch()
    except Exception:
        report(traceback.format_exc())
        raise
    finally:
        for process in (recorder, app, chrome):
            if process is not None and process.poll() is None:
                process.terminate()
                process.wait(timeout=5)
        server.shutdown()
        subprocess.run(["niri", "msg", "action", "quit", "--skip-confirmation"], timeout=5, check=False)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("test_binary", type=Path)
    parser.add_argument("app_binary", type=Path)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--before", action="store_true")
    parser.add_argument("--nested", type=Path)
    args = parser.parse_args()
    binary, app_binary, evidence = args.test_binary.resolve(), args.app_binary.resolve(), args.evidence.resolve()
    if args.nested:
        nested(str(binary), str(app_binary), args.nested, evidence, args.before)
        return
    dependencies = ["niri", "gamescope", "google-chrome-stable", "wl-copy", "wl-paste"]
    if os.environ.get("DICTATION_TEST_RECORD"):
        dependencies += ["ffmpeg"]
    for tool in dependencies:
        assert shutil.which(tool), f"Missing test dependency: {tool}"
    evidence.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="dictation-delivery-", dir="/tmp") as directory:
        state = Path(directory)
        (state / "binary.sha256").write_text(hashlib.sha256(binary.read_bytes()).hexdigest())
        config = state / "niri.kdl"
        config.write_text('''hotkey-overlay { skip-at-startup; }
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
                   DICTATION_TEST_PARENT_DISPLAY=os.environ.get("DISPLAY", ""),
                   DISABLE_GAMESCOPE_WSI="1", WINIT_UNIX_BACKEND="x11")
        command = ["gamescope", "--backend", "headless", "-W1280", "-H900", "--",
                   "sh", "-c", 'export DICTATION_TEST_OUTER_DISPLAY="$DISPLAY"; unset WAYLAND_DISPLAY; exec "$@"', "test-niri",
                   "niri", "--config", str(config), "--", sys.executable,
                   str(Path(__file__).resolve()), str(binary), str(app_binary),
                   "--evidence", str(evidence), "--nested", str(state)]
        if args.before:
            command.append("--before")
        with (state / "session.log").open("w") as log:
            subprocess.run(command, env=env, stdout=log, stderr=log, timeout=90, check=False)
        checks = (state / "checks.log").read_text() if (state / "checks.log").exists() else ""
        print(checks, end="")
        (evidence / ("before-checks.log" if args.before else "after-checks.log")).write_text(checks)
        if not (state / "passed").exists():
            print((state / "session.log").read_text()[-6000:])
            for log in state.glob("*/app.log"):
                print(log.read_text()[-3000:])
            for log in state.glob("*/recorder.log"):
                print(log.read_text()[-3000:])
            raise SystemExit("FAIL native transcript delivery")


if __name__ == "__main__":
    main()
