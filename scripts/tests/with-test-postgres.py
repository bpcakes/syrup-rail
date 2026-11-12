#!/usr/bin/env python3
"""Launcher lifecycle tests using a fake Docker CLI, not live database proof."""

import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest


LAUNCHER = Path(__file__).resolve().parents[1] / "with-test-postgres.py"
CONTAINER_ID = "a" * 64
FAKE_DOCKER = r'''#!/usr/bin/env python3
import json, os, pathlib, sys, time
root = pathlib.Path(os.environ["LAUNCHER_FIXTURE_ROOT"])
args = sys.argv[1:]
mode = os.environ.get("LAUNCHER_FIXTURE_MODE", "")
with (root / "calls").open("a") as out:
    out.write(json.dumps(args) + "\n")
operation = args[0]
if operation == "create":
    if mode == "create_absent": sys.exit(17)
    owner = args[args.index("--label") + 1].split("=", 1)[1]
    if mode == "foreign_owner": owner = "someone-else"
    (root / "owner").write_text(owner)
    if mode == "create_timeout": time.sleep(5)
    if mode in ("create_failure", "foreign_owner", "lookup_failure", "inspect_failure"): sys.exit(17)
    print("a" * 64)
elif operation == "start":
    (root / "started").touch()
    if mode == "startup_interrupt": time.sleep(0.4)
    if mode == "start_failure": sys.exit(18)
elif operation == "port":
    print("127.0.0.1:54321")
elif operation == "exec":
    if mode == "not_ready": sys.exit(1)
    print("170005" if mode == "wrong_version" else "180000")
elif operation == "ps":
    if mode == "lookup_failure": sys.exit(22)
    if (root / "owner").exists() and mode != "foreign_owner": print("a" * 64)
elif operation == "inspect":
    if mode == "inspect_failure": sys.exit(23)
    owner = (root / "owner").read_text()
    print("a" * 64, owner)
elif operation == "rm":
    if mode == "cleanup_failure": sys.exit(19)
    (root / "removed").touch()
else:
    sys.exit(99)
'''


class LauncherTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        docker = self.root / "docker"
        docker.write_text(FAKE_DOCKER)
        docker.chmod(0o755)
        self.environment = os.environ.copy()
        self.environment.pop("POSTGRES_TEST_ADMIN_URL", None)
        self.environment.pop("POSTGRES_TEST_IMAGE", None)
        self.environment.update(
            PATH=str(self.root) + os.pathsep + os.environ["PATH"],
            LAUNCHER_FIXTURE_ROOT=str(self.root),
        )
        self.child = self.root / "child.py"
        self.child.write_text(
            "import json, os, pathlib, sys, time\n"
            "root = pathlib.Path(os.environ['LAUNCHER_FIXTURE_ROOT'])\n"
            "(root / 'child').write_text(json.dumps([os.environ['POSTGRES_TEST_ADMIN_URL'], sys.argv[2:]]))\n"
            "if sys.argv[1] == 'wait': time.sleep(60)\n"
            "else: sys.exit(int(sys.argv[1]))\n"
        )

    def argv(self, status=0):
        return [sys.executable, str(LAUNCHER), sys.executable, str(self.child), str(status), "a b;$value"]

    def calls(self):
        path = self.root / "calls"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def run_launcher(self, status=0, mode=""):
        self.environment["LAUNCHER_FIXTURE_MODE"] = mode
        return subprocess.run(self.argv(status), env=self.environment, capture_output=True, text=True, timeout=10)

    def assert_removed(self):
        self.assertTrue((self.root / "removed").exists())
        self.assertEqual(self.calls()[-1], ["rm", "--force", "--volumes", CONTAINER_ID])

    def test_owned_success_and_exact_arguments(self):
        result = self.run_launcher()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_removed()
        url, args = json.loads((self.root / "child").read_text())
        self.assertTrue(url.startswith("postgres://postgres:"))
        self.assertTrue(url.endswith("@127.0.0.1:54321/postgres?sslmode=disable"))
        self.assertEqual(args, ["a b;$value"])
        self.assertNotIn(url, result.stdout + result.stderr)
        create = self.calls()[0]
        self.assertIn("127.0.0.1::5432", create)
        self.assertIn("POSTGRES_INITDB_ARGS=--no-sync", create)
        self.assertIn("/var/lib/postgresql:rw,nosuid,size=1g", create)
        self.assertIn("max_connections=300", create)
        self.assertIn("fsync=off", create)
        self.assertIn("synchronous_commit=off", create)
        self.assertIn("full_page_writes=off", create)
        probe = next(call for call in self.calls() if call[0] == "exec")
        self.assertIn("SHOW server_version_num", probe)
        self.assertIn("127.0.0.1", probe)

    def test_owned_child_failure_is_preserved(self):
        self.assertEqual(self.run_launcher(23).returncode, 23)
        self.assert_removed()

    def test_cleanup_failure_fails_successful_child(self):
        result = self.run_launcher(mode="cleanup_failure")
        self.assertEqual(result.returncode, 1)
        self.assertIn("cleanup failed", result.stderr)

    def test_cleanup_failure_preserves_child_failure(self):
        result = self.run_launcher(23, "cleanup_failure")
        self.assertEqual(result.returncode, 23)
        self.assertIn("cleanup failed", result.stderr)

    def test_external_server_exec_has_no_docker_ownership(self):
        explicit = "postgres://test:private@localhost:6543/postgres"
        self.environment["POSTGRES_TEST_ADMIN_URL"] = explicit
        result = self.run_launcher(23)
        self.assertEqual(result.returncode, 23)
        self.assertEqual(json.loads((self.root / "child").read_text()), [explicit, ["a b;$value"]])
        self.assertEqual(self.calls(), [])
        self.assertNotIn(explicit, result.stdout + result.stderr)

    def test_empty_explicit_url_does_not_create_server(self):
        self.environment["POSTGRES_TEST_ADMIN_URL"] = ""
        self.assertEqual(self.run_launcher().returncode, 2)
        self.assertEqual(self.calls(), [])

    def test_startup_failures_clean_only_owned_container(self):
        for mode in ("create_failure", "start_failure", "wrong_version"):
            with self.subTest(mode=mode):
                result = self.run_launcher(mode=mode)
                self.assertEqual(result.returncode, 1)
                self.assertFalse((self.root / "child").exists())
                self.assert_removed()
                (self.root / "removed").unlink()

    def run_with_short_budget(self, mode):
        self.environment["LAUNCHER_FIXTURE_MODE"] = mode
        # Shorten the same production deadline for this process-level test.
        code = (
            "import runpy, sys; m=runpy.run_path(sys.argv[1]); "
            "m['Launcher'].__init__.__globals__['STARTUP_SECONDS']=0.3; "
            "sys.exit(m['main'](sys.argv[2:]))"
        )
        argv = [sys.executable, "-c", code, *self.argv()[1:]]
        return subprocess.run(argv, env=self.environment, capture_output=True, text=True, timeout=5)

    def test_readiness_budget_stops_before_child(self):
        result = self.run_with_short_budget("not_ready")
        self.assertEqual(result.returncode, 1)
        self.assertFalse((self.root / "child").exists())
        self.assert_removed()

    def test_create_timeout_recovers_labelled_container(self):
        result = self.run_with_short_budget("create_timeout")
        self.assertEqual(result.returncode, 1)
        self.assertFalse((self.root / "child").exists())
        self.assert_removed()

    def test_foreign_container_is_never_removed(self):
        result = self.run_launcher(mode="foreign_owner")
        self.assertEqual(result.returncode, 1)
        self.assertFalse(any(call[0] == "rm" for call in self.calls()))

    def test_failed_ownership_lookup_is_reported(self):
        for mode in ("lookup_failure", "inspect_failure"):
            with self.subTest(mode=mode):
                result = self.run_launcher(mode=mode)
                self.assertEqual(result.returncode, 1)
                self.assertIn("cleanup failed", result.stderr)
                self.assertFalse(any(call[0] == "rm" for call in self.calls()))

    def test_confirmed_absence_needs_no_cleanup(self):
        result = self.run_launcher(mode="create_absent")
        self.assertEqual(result.returncode, 1)
        self.assertNotIn("cleanup failed", result.stderr)
        self.assertFalse(any(call[0] in ("rm", "inspect") for call in self.calls()))

    def test_image_override_is_one_literal_argument(self):
        self.environment["POSTGRES_TEST_IMAGE"] = "local/postgres:18-custom"
        self.assertEqual(self.run_launcher().returncode, 0)
        create = self.calls()[0]
        self.assertEqual(create[create.index("--") + 1], "local/postgres:18-custom")
        self.assert_removed()

    def test_empty_image_override_does_not_create_server(self):
        self.environment["POSTGRES_TEST_IMAGE"] = " "
        self.assertEqual(self.run_launcher().returncode, 2)
        self.assertEqual(self.calls(), [])

    def interrupt_at(self, marker, mode=""):
        self.environment["LAUNCHER_FIXTURE_MODE"] = mode
        process = subprocess.Popen(self.argv("wait"), env=self.environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 5
            while not (self.root / marker).exists():
                self.assertIsNone(process.poll())
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.02)
            process.send_signal(signal.SIGTERM)
            process.communicate(timeout=5)
            self.assertEqual(process.returncode, 128 + signal.SIGTERM)
            self.assert_removed()
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate()

    def test_interruption_forwards_to_child_and_cleans(self):
        self.interrupt_at("child")

    def test_interruption_during_start_cleans(self):
        self.interrupt_at("started", "startup_interrupt")
        self.assertFalse((self.root / "child").exists())

    def test_interruption_reaps_ignoring_descendant_after_leader_exits(self):
        grandchild = self.root / "grandchild.py"
        grandchild.write_text(
            "import os, pathlib, signal, time\n"
            "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
            "(pathlib.Path(os.environ['LAUNCHER_FIXTURE_ROOT']) / 'grandchild').write_text(str(os.getpid()))\n"
            "time.sleep(60)\n"
        )
        self.child.write_text(
            "import os, pathlib, subprocess, sys, time\n"
            "root = pathlib.Path(os.environ['LAUNCHER_FIXTURE_ROOT'])\n"
            "subprocess.Popen([sys.executable, str(root / 'grandchild.py')], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)\n"
            "while not (root / 'grandchild').exists(): time.sleep(0.01)\n"
            "(root / 'child').touch()\n"
            "time.sleep(60)\n"
        )
        try:
            self.interrupt_at("child")
            pid = int((self.root / "grandchild").read_text())
            deadline = time.monotonic() + 1
            while time.monotonic() < deadline:
                stat = Path(f"/proc/{pid}/stat")
                if not stat.exists() or stat.read_text().split()[2] == "Z":
                    return
                time.sleep(0.02)
            self.fail("interrupted command left a running descendant")
        finally:
            if (self.root / "grandchild").exists():
                try:
                    os.kill(int((self.root / "grandchild").read_text()), signal.SIGKILL)
                except ProcessLookupError:
                    pass


if __name__ == "__main__":
    unittest.main()
