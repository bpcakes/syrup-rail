#!/usr/bin/env python3
"""Launcher lifecycle tests using a fake Docker CLI, not live database proof."""

import json
import os
from pathlib import Path
import runpy
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
if operation == "image":
    if mode.startswith("pull_"): sys.exit(1)
    print("sha256:" + "b" * 64)
elif operation == "pull":
    (root / "pulling").touch()
    if mode == "pull_slow": time.sleep(0.6)
    if mode == "pull_interrupt": time.sleep(30)
    if mode == "pull_failure": sys.exit(20)
elif operation == "create":
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
        self.environment.pop("RUST_TEST_THREADS", None)
        self.environment.update(
            PATH=str(self.root) + os.pathsep + os.environ["PATH"],
            LAUNCHER_FIXTURE_ROOT=str(self.root),
        )
        self.child = self.root / "child.py"
        self.child.write_text(
            "import json, os, pathlib, sys, time\n"
            "root = pathlib.Path(os.environ['LAUNCHER_FIXTURE_ROOT'])\n"
            "(root / 'child').write_text(json.dumps([\n"
            "    os.environ['POSTGRES_TEST_ADMIN_URL'], sys.argv[2:], os.environ.get('RUST_TEST_THREADS')]))\n"
            "if sys.argv[1] == 'wait': time.sleep(60)\n"
            "else: sys.exit(int(sys.argv[1]))\n"
        )

    def argv(self, status=0):
        return [sys.executable, str(LAUNCHER), sys.executable, str(self.child), str(status), "a b;$value"]

    def calls(self):
        path = self.root / "calls"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def create_call(self):
        return next(call for call in self.calls() if call[0] == "create")

    def run_launcher(self, status=0, mode="", preexec_fn=None):
        self.environment["LAUNCHER_FIXTURE_MODE"] = mode
        return subprocess.run(
            self.argv(status), env=self.environment, capture_output=True, text=True, timeout=10,
            preexec_fn=preexec_fn,
        )

    def child_record(self):
        return json.loads((self.root / "child").read_text())

    def assert_removed(self):
        self.assertTrue((self.root / "removed").exists())
        self.assertEqual(self.calls()[-1], ["rm", "--force", "--volumes", CONTAINER_ID])

    def test_owned_success_and_exact_arguments(self):
        result = self.run_launcher()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_removed()
        url, args, _threads = self.child_record()
        self.assertTrue(url.startswith("postgres://postgres:"))
        self.assertTrue(url.endswith("@127.0.0.1:54321/postgres?sslmode=disable"))
        self.assertEqual(args, ["a b;$value"])
        self.assertNotIn(url, result.stdout + result.stderr)
        self.assertEqual(self.calls()[0][:2], ["image", "inspect"])
        self.assertFalse(any(call[0] == "pull" for call in self.calls()))
        create = self.create_call()
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
        # The caller owns this server's capacity, so no thread default is imposed.
        self.assertEqual(self.child_record(), [explicit, ["a b;$value"], None])
        self.assertEqual(self.calls(), [])
        self.assertNotIn(explicit, result.stdout + result.stderr)

    def test_owned_server_bounds_default_test_threads(self):
        self.assertEqual(self.run_launcher().returncode, 0)
        threads = int(self.child_record()[2])
        try:
            available = len(os.sched_getaffinity(0))
        except AttributeError:
            available = os.cpu_count() or 1
        self.assertGreaterEqual(threads, 1)
        self.assertLessEqual(threads, min(32, available))
        self.assert_removed()

    def test_default_test_threads_never_exceed_the_cap(self):
        result = self.run_with_globals(MAX_DEFAULT_TEST_THREADS=1)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.child_record()[2], "1")
        self.assert_removed()

    @unittest.skipUnless(hasattr(os, "sched_setaffinity"), "needs CPU affinity control")
    def test_default_test_threads_never_exceed_available_cpus(self):
        cpu = min(os.sched_getaffinity(0))
        result = self.run_launcher(preexec_fn=lambda: os.sched_setaffinity(0, {cpu}))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.child_record()[2], "1")

    def test_caller_test_threads_are_preserved(self):
        self.environment["RUST_TEST_THREADS"] = "48"
        self.assertEqual(self.run_launcher().returncode, 0)
        self.assertEqual(self.child_record()[2], "48")
        self.assert_removed()

    def cgroup_limit(self):
        # Point the std-equivalent quota lookup at a fake procfs and cgroupfs.
        self.cgroups = self.root / "cgroup"
        self.membership = self.root / "membership"
        self.mountinfo = self.root / "mountinfo"
        limit = runpy.run_path(str(LAUNCHER))["cgroup_cpu_limit"]
        limit.__globals__.update(
            PROC_CGROUP=str(self.membership), PROC_MOUNTINFO=str(self.mountinfo),
            CGROUP_ROOT=str(self.cgroups),
        )
        return limit

    def assert_cgroup_limits(self, limit, cases):
        for membership, expected in cases:
            with self.subTest(membership=membership):
                self.membership.write_text(membership)
                self.assertEqual(limit(), expected)

    @staticmethod
    def write_v1_quota(directory, quota):
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "cpu.cfs_quota_us").write_text(quota + "\n")
        (directory / "cpu.cfs_period_us").write_text("100000\n")

    def test_cgroup_v2_quota_uses_the_tightest_ancestor(self):
        limit = self.cgroup_limit()
        tests = self.cgroups / "jobs" / "tests"
        tests.mkdir(parents=True)
        for directory, quota in ((self.cgroups, "max"), (tests.parent, "300000"), (tests, "250000")):
            (directory / "cpu.max").write_text(quota + " 100000\n")
            (directory / "cgroup.controllers").write_text("cpu memory\n")
        self.assert_cgroup_limits(limit, (
            ("0::/jobs/tests\n", 2),
            ("0::/jobs\n", 3),
            ("0::/\n", None),
            ("0::/../escape\n", None),
            ("4:memory:/jobs/tests\n", None),
        ))
        (tests.parent / "cpu.max").write_text("50000 100000\n")
        self.assert_cgroup_limits(limit, (("0::/jobs/tests\n", 1),))
        # Like std, a group without cgroup.controllers is not read as cgroup2.
        (tests / "cgroup.controllers").unlink()
        self.assert_cgroup_limits(limit, (("0::/jobs/tests\n", None),))
        self.membership.unlink()
        self.assertIsNone(limit())

    def test_cgroup_v1_cpu_quota_takes_precedence_over_v2(self):
        limit = self.cgroup_limit()
        v1 = self.cgroups / "cpu,cpuacct"
        for directory, quota in ((v1, "-1"), (v1 / "jobs", "200000"), (v1 / "jobs" / "tests", "-1")):
            self.write_v1_quota(directory, quota)
        unified = self.cgroups / "jobs"
        unified.mkdir(parents=True)
        (unified / "cpu.max").write_text("800000 100000\n")
        (unified / "cgroup.controllers").write_text("cpu\n")
        self.assert_cgroup_limits(limit, (
            ("4:cpu,cpuacct:/jobs/tests\n0::/jobs\n", 2),
            ("0::/jobs\n4:cpu,cpuacct:/jobs/tests\n", 2),
            ("0::/jobs\n", 8),
            ("4:cpu,cpuacct:/elsewhere\n", None),
        ))
        # The /sys/fs/cgroup/cpu location is tried before cpu,cpuacct.
        self.write_v1_quota(self.cgroups / "cpu" / "jobs" / "tests", "100000")
        self.assert_cgroup_limits(limit, (("4:cpu,cpuacct:/jobs/tests\n", 1),))

    def test_cgroup_v1_quota_follows_a_bind_mounted_cpu_hierarchy(self):
        limit = self.cgroup_limit()
        mount = self.root / "container-cpu"
        self.write_v1_quota(mount, "300000")
        self.write_v1_quota(mount / "tests", "-1")
        self.mountinfo.write_text(
            "30 25 0:26 / /sys/fs/cgroup/memory rw - cgroup cgroup rw,memory\n"
            f"31 25 0:27 /docker {mount} rw,nosuid shared:9 - cgroup cgroup rw,cpu,cpuacct\n"
        )
        self.assert_cgroup_limits(limit, (
            ("5:cpu,cpuacct:/docker/tests\n", 3),
            ("5:cpu,cpuacct:/elsewhere/tests\n", None),
        ))

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

    def run_with_globals(self, mode="", **overrides):
        self.environment["LAUNCHER_FIXTURE_MODE"] = mode
        # Override production module settings for this process-level test.
        code = (
            "import json, runpy, sys; m=runpy.run_path(sys.argv[1]); "
            "m['main'].__globals__.update(json.loads(sys.argv[2])); "
            "sys.exit(m['main'](sys.argv[3:]))"
        )
        argv = [sys.executable, "-c", code, str(LAUNCHER), json.dumps(overrides), *self.argv()[2:]]
        return subprocess.run(argv, env=self.environment, capture_output=True, text=True, timeout=5)

    def run_with_short_budget(self, mode):
        # Shorten the same production deadline for this process-level test.
        return self.run_with_globals(mode, STARTUP_SECONDS=0.3)

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

    def test_uncached_image_pull_has_its_own_budget(self):
        # The pull outlasts the whole (shortened) startup budget.
        result = self.run_with_short_budget("pull_slow")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Pulling", result.stderr)
        operations = [call[0] for call in self.calls()]
        self.assertEqual(operations[:3], ["image", "pull", "create"])
        self.assertEqual(self.calls()[1], ["pull", "--quiet", "--", "postgres:18"])
        self.assert_removed()

    def test_image_pull_failure_is_named_and_creates_nothing(self):
        result = self.run_launcher(mode="pull_failure")
        self.assertEqual(result.returncode, 1)
        self.assertIn("image pull failed", result.stderr)
        self.assertFalse((self.root / "child").exists())
        self.assertEqual([call[0] for call in self.calls()], ["image", "pull"])

    def test_interruption_during_image_pull_stops_promptly(self):
        self.interrupt_at("pulling", "pull_interrupt", removed=False)
        self.assertEqual([call[0] for call in self.calls()], ["image", "pull"])

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
        create = self.create_call()
        self.assertEqual(create[create.index("--") + 1], "local/postgres:18-custom")
        self.assert_removed()

    def test_empty_image_override_does_not_create_server(self):
        self.environment["POSTGRES_TEST_IMAGE"] = " "
        self.assertEqual(self.run_launcher().returncode, 2)
        self.assertEqual(self.calls(), [])

    def interrupt_at(self, marker, mode="", removed=True):
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
            if removed:
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
