#!/usr/bin/env python3
"""Type into Chrome inside an isolated headless niri and check what arrives.

Chromium acts on some keys by scancode whatever the keymap says, so this is
the client that exposes keycode collisions (https://github.com/atx/wtype/issues/71).

    cargo test --no-run --message-format=json  # prints the test binary path
    scripts/check-typing.py <test-binary>
"""

import argparse
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

CASES = [
    "abcdefghijklm?XYZ",
    "let's just keep it open.",
    "абвгдеёжзийклмнопрстуфхцчшщъыьэюя",
    # Over 47 distinct characters, so typing switches keymaps mid-text. No Tab:
    # in a textarea it moves focus, as typing one would. No emoji: Chromium
    # truncates keysyms past U+FFFF to 16 bits, whatever types them.
    "Quick, Zoë! Jump over 12 lazy dogs (and 3 foxes) — 50% faster; "
    "then e-mail “Ålesund” at 9:45? Yes/no: #1 [ok] {done} <π≈3.14> ~\n"
    "Second line, ünïcödé — and «quotes» & ½ more.",
]
# Besides Key*/Digit*: printable punctuation keys, and whitespace's real keys.
TEXT_CODES = {"Minus", "Equal", "BracketLeft", "BracketRight", "Semicolon", "Quote",
              "Backquote", "Backslash", "Comma", "Period", "Slash", "Enter", "Space"}

PAGE = """<!doctype html><meta charset=utf-8>
<textarea id=t autofocus style="width:100%;height:90vh"></textarea>
<script>
// One request at a time, so a long transcript can't queue thousands of them.
const t = document.getElementById('t');
let gen = null, codes = [], sent = null;
t.addEventListener('keydown', e => { codes.push(e.code); });
(async function poll() {
  const next = await (await fetch('/gen')).text();
  if (next !== gen) { gen = next; codes = []; t.value = ''; t.focus(); sent = null; }
  const report = JSON.stringify({gen, value: t.value, codes});
  if (report !== sent) {
    await fetch('/report', {method: 'POST', body: report});
    sent = report;
  }
  setTimeout(poll, 50);
})();
</script>"""


def nested(binary, state):
    assert os.environ["NIRI_SOCKET"] != os.environ["DICTATION_TEST_PARENT_NIRI"]
    reports, gen = {}, ["0"]

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            body = (PAGE if self.path == "/" else gen[0]).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            report = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            reports[report["gen"]] = report
            self.send_response(204)
            self.end_headers()

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    socket = Path(os.environ["WAYLAND_DISPLAY"])
    if not socket.is_absolute():
        socket = Path(os.environ["XDG_RUNTIME_DIR"]) / socket
    env = dict(os.environ, WAYLAND_DISPLAY=str(socket))
    results = state / "results.log"
    chrome = None
    # The sentinel stops Chrome opening its welcome window mid-typing.
    (state / "chrome").mkdir()
    (state / "chrome" / "First Run").touch()
    try:
        chrome = subprocess.Popen(
            ["google-chrome-stable", "--ozone-platform=wayland", "--disable-gpu",
             "--no-first-run", "--no-default-browser-check", "--password-store=basic",
             "--disable-background-networking",
             f"--user-data-dir={state / 'chrome'}",
             f"--app=http://127.0.0.1:{server.server_port}/"],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

        def wait_for(predicate, timeout=15):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                if predicate():
                    return True
                time.sleep(.05)
            return False

        def niri(*args):
            return subprocess.check_output(["niri", "msg", *args], text=True)

        def is_page(window):
            return "chrome-127.0.0.1" in ((window or {}).get("app_id") or "")

        def focus_page():
            # Chrome opens promo windows a few seconds after launch; close them.
            for window in json.loads(niri("-j", "windows")):
                if not is_page(window):
                    niri("action", "close-window", "--id", str(window["id"]))
            page = next(w for w in json.loads(niri("-j", "windows")) if is_page(w))
            niri("action", "focus-window", "--id", str(page["id"]))
            assert wait_for(lambda: is_page(json.loads(niri("-j", "focused-window")))), \
                "page window never took focus"

        assert wait_for(lambda: "0" in reports and any(
            is_page(w) for w in json.loads(niri("-j", "windows")))), "Chrome never opened"
        # A nested niri opens the overview at startup, which holds keyboard focus.
        niri("action", "close-overview")

        def type_text(text):
            subprocess.run(
                [binary, "--ignored", "--exact", "typer::tests::types_into_focused_window"],
                env=dict(env, DICTATION_TEST_TYPE_TEXT=text), check=True,
                stdout=subprocess.DEVNULL, timeout=60)

        failures = 0
        for case in CASES:
            focus_page()
            gen[0] = str(int(gen[0]) + 1)
            assert wait_for(lambda: gen[0] in reports), "page did not reset"
            type_text(case)
            wait_for(lambda: reports[gen[0]]["value"] == case, timeout=5)
            assert is_page(json.loads(niri("-j", "focused-window"))), \
                "focus moved during typing; rerun"
            report = reports[gen[0]]
            special = sorted({code for code in report["codes"]
                              if not code.startswith(("Key", "Digit"))} - TEXT_CODES)
            ok = report["value"] == case and not special
            failures += not ok
            with results.open("a") as log:
                print(f"{'PASS' if ok else 'FAIL'} sent {case!r}", file=log)
                if not ok:
                    print(f"    got  {report['value']!r}", file=log)
                    print(f"    non-text key codes: {special}", file=log)
        if failures == 0:
            (state / "passed").touch()
    except Exception:
        with results.open("a") as log:
            print(traceback.format_exc(), file=log)
        raise
    finally:
        if chrome is not None:
            chrome.terminate()
            chrome.wait(timeout=5)
        server.shutdown()
        subprocess.run(["niri", "msg", "action", "quit", "--skip-confirmation"],
                       check=False, timeout=5)


def main():
    if len(sys.argv) == 4 and sys.argv[1] == "--nested":
        nested(sys.argv[2], Path(sys.argv[3]))
        return
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path, help="dictationapp test binary")
    args = parser.parse_args()
    binary = args.binary.resolve()
    assert binary.is_file(), f"Build the test binary first: {binary}"
    for tool in ("niri", "gamescope", "google-chrome-stable"):
        assert shutil.which(tool), f"Missing test dependency: {tool}"
    with tempfile.TemporaryDirectory(prefix="dictation-typing-check-", dir="/tmp") as directory:
        state = Path(directory)
        config = state / "niri.kdl"
        config.write_text("hotkey-overlay { skip-at-startup; }\nanimations { off; }\n")
        env = dict(os.environ, DICTATION_TEST_PARENT_NIRI=os.environ.get("NIRI_SOCKET", ""),
                   DISABLE_GAMESCOPE_WSI="1", WINIT_UNIX_BACKEND="x11")
        command = ["gamescope", "--backend", "headless", "-W1280", "-H900", "--",
                   "sh", "-c", 'unset WAYLAND_DISPLAY; exec "$@"', "test-niri",
                   "niri", "--config", str(config), "--", sys.executable,
                   str(Path(__file__).resolve()), "--nested", str(binary), str(state)]
        with (state / "session.log").open("w") as log:
            subprocess.run(command, env=env, stdout=log, stderr=log, timeout=180, check=False)
        if (state / "results.log").exists():
            print((state / "results.log").read_text(), end="")
        if not (state / "passed").exists():
            print((state / "session.log").read_text()[-4000:])
            raise SystemExit("FAIL typing")


if __name__ == "__main__":
    main()
