#!/usr/bin/env python3
"""Focused publish regressions with fake AUR/download/privilege operations."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]

MOCK = r'''#!/usr/bin/python3
import json, os, pathlib, subprocess, sys, tempfile
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ["PUBLISH_TEST_LOG"], "a") as log:
    log.write(json.dumps([name, args]) + "\n")
if name == "id":
    if args == ["-u"]:
        print("0" if os.environ["PUBLISH_TEST_ROOT"] == "1" else "1000")
    sys.exit(0)
if name == "mktemp":
    print(tempfile.mkdtemp(prefix="work-", dir=os.environ["PUBLISH_TEST_DIR"]))
    sys.exit(0)
if name == "git":
    if args[0] == "clone":
        pathlib.Path(args[-1]).mkdir(mode=0o755)
    if "diff" in args:
        sys.exit(128 if os.environ.get("PUBLISH_TEST_DIFF_FAIL") == "1" else 1)
    sys.exit(0)
if name == "curl":
    if os.environ.get("PUBLISH_TEST_DOWNLOAD_FAIL") == "1":
        sys.exit(22)
    sys.stdout.buffer.write(b"release archive bytes")
    sys.exit(0)
if name == "runuser":
    # We cannot change host UIDs. Enforce the builder's required traversal
    # permission before forwarding its exact command without elevation.
    workdir = pathlib.Path(args[-1]).parent
    if not workdir.stat().st_mode & 0o001:
        print("builder cannot traverse mktemp parent", file=sys.stderr)
        sys.exit(13)
    sys.exit(subprocess.call(args[3:]))
if name == "makepkg":
    if os.environ["PUBLISH_TEST_ROOT"] == "1":
        marker = pathlib.Path(os.environ["PUBLISH_TEST_DIR"]) / "builder-owned"
        if not marker.exists() or marker.read_text() != str(pathlib.Path.cwd()):
            print("builder requires writable ownership of checkout", file=sys.stderr)
            sys.exit(11)
    if os.environ.get("PUBLISH_TEST_MAKEPKG_FAIL") == "1":
        sys.exit(35)
    pkgbuild = pathlib.Path("PKGBUILD").read_text()
    pathlib.Path(os.environ["PUBLISH_TEST_SNAPSHOT"]).write_text(pkgbuild)
    print("pkgbase = dictationapp\n\tpkgver = 0.2.0")
    sys.exit(0)
if name == "rm":
    marker = pathlib.Path(os.environ["PUBLISH_TEST_DIR"]) / "cleanup-retried"
    if os.environ.get("PUBLISH_TEST_CLEANUP_FAIL_ONCE") == "1" and not marker.exists():
        marker.touch()
        sys.exit(1)
    sys.exit(subprocess.call(["/usr/bin/rm", *args]))
if name == "useradd":
    sys.exit(0)
if name == "chown":
    if args[:2] != ["-R", "builder"]:
        raise SystemExit("unexpected ownership handoff")
    marker = pathlib.Path(os.environ["PUBLISH_TEST_DIR"]) / "builder-owned"
    marker.write_text(args[-1])
    sys.exit(0)
raise SystemExit("unexpected mocked tool: " + name)
'''


class PublishTests(unittest.TestCase):
    def run_publish(self, *, root=False, download_fail=False, makepkg_fail=False,
                    cleanup_fail_once=False, diff_fail=False):
        with tempfile.TemporaryDirectory(prefix="aur-publish-test-", dir="/tmp") as tmp:
            directory = Path(tmp)
            bins = directory / "bin"
            bins.mkdir()
            helper = bins / "mock"
            helper.write_text(MOCK)
            helper.chmod(0o755)
            for name in ["id", "mktemp", "git", "curl", "runuser", "makepkg", "rm", "useradd", "chown"]:
                (bins / name).symlink_to(helper)
            log = directory / "calls.jsonl"
            snapshot = directory / "published.PKGBUILD"
            env = dict(os.environ, PATH=f"{bins}:/usr/bin:/bin",
                       PUBLISH_TEST_ROOT=str(int(root)), PUBLISH_TEST_DIR=tmp,
                       PUBLISH_TEST_LOG=str(log), PUBLISH_TEST_SNAPSHOT=str(snapshot),
                       PUBLISH_TEST_DOWNLOAD_FAIL=str(int(download_fail)),
                       PUBLISH_TEST_MAKEPKG_FAIL=str(int(makepkg_fail)),
                       PUBLISH_TEST_DIFF_FAIL=str(int(diff_fail)),
                       PUBLISH_TEST_CLEANUP_FAIL_ONCE=str(int(cleanup_fail_once)))
            # Source functions only; SSH setup and real publication never run.
            command = ['bash', '-c',
                       'source "$1"; VERSION=0.2.0; prepare_workdir; '
                       'printf "workdir=%s\\n" "$WORK_DIR"; '
                       'publish dictationapp "$2" mock://release', '--',
                       str(ROOT / "packaging/aur/publish.sh"),
                       str(ROOT / "packaging/aur/PKGBUILD")]
            result = subprocess.run(command, env=env, capture_output=True, text=True,
                                    timeout=5)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            workdirs = [line.removeprefix("workdir=") for line in result.stdout.splitlines()
                        if line.startswith("workdir=")]
            self.assertEqual(len(workdirs), 1, result.stderr)
            self.assertFalse(Path(workdirs[0]).exists(), "owned workdir leaked")
            content = snapshot.read_text() if snapshot.exists() else None
            return result, calls, content

    def test_root_builder_can_read_package_and_publish(self):
        result, calls, content = self.run_publish(root=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(any(name == "runuser" for name, _ in calls))
        self.assertTrue(any(name == "git" and "push" in args for name, args in calls))
        digest = hashlib.sha256(b"release archive bytes").hexdigest()
        self.assertIn(f"sha256sums=('{digest}')", content)

    def test_non_root_publication_does_not_need_privilege_switch(self):
        result, calls, _ = self.run_publish()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any(name == "runuser" for name, _ in calls))

    def test_failed_download_aborts_before_commit_or_push(self):
        result, calls, content = self.run_publish(download_fail=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIsNone(content)
        self.assertFalse(any(name == "git" and ("commit" in args or "push" in args)
                             for name, args in calls))

    def test_failed_srcinfo_aborts_and_cleans_checkout(self):
        result, calls, _ = self.run_publish(root=True, makepkg_fail=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(name == "git" and ("commit" in args or "push" in args)
                             for name, args in calls))

    def test_failed_cleanup_is_retried(self):
        result, calls, _ = self.run_publish(cleanup_fail_once=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(sum(name == "rm" for name, _ in calls), 2)

    def test_failed_diff_aborts_before_commit_or_push(self):
        result, calls, _ = self.run_publish(diff_fail=True)
        self.assertEqual(result.returncode, 128)
        self.assertFalse(any(name == "git" and ("commit" in args or "push" in args)
                             for name, args in calls))


if __name__ == "__main__":
    unittest.main()
