#!/usr/bin/env python3
"""Run argv against an explicit test server or one owned disposable PostgreSQL 18."""

import os
import re
import secrets
import signal
import subprocess
import sys
import time
import uuid


ADMIN_URL = "POSTGRES_TEST_ADMIN_URL"
OWNER_LABEL = "local.test-postgres.owner"
PULL_SECONDS = 600
STARTUP_SECONDS = 60
STOP_SECONDS = 10
# Every live test fixture shares the owned server: each copies its own
# database onto the 1 GiB tmpfs and holds harness sessions plus up to four
# pool connections out of 300. libtest otherwise runs one test per CPU, and
# 128 concurrent fixtures exhaust that server. An explicit RUST_TEST_THREADS
# or --test-threads still takes precedence.
TEST_THREADS = "RUST_TEST_THREADS"
MAX_DEFAULT_TEST_THREADS = 32
PROC_CGROUP = "/proc/self/cgroup"
PROC_MOUNTINFO = "/proc/self/mountinfo"
CGROUP_ROOT = "/sys/fs/cgroup"


class Interrupted(Exception):
    pass


def cgroup_cpu_limit():
    """Return the cgroup CPU quota in whole CPUs (at least one), or None.

    This mirrors Rust std's available_parallelism, which sets libtest's
    default thread count, so the launcher's default never exceeds libtest's
    own choice. As in std, a v1 entry with the cpu controller takes
    precedence over the v2 entry; v1 reads the CFS quota under the cgroups(7)
    mounts or the first matching cpu mount in mountinfo; v2 reads cpu.max
    only under the standard mount; the tightest ancestor quota applies.
    """
    try:
        with open(PROC_CGROUP) as entries:
            lines = entries.read().splitlines()
    except OSError:
        return None
    selected = None
    for line in lines:
        fields = line.split(":", 2)
        if len(fields) != 3:
            continue
        if fields[1] == "":
            selected = selected or (cgroup_v2_quota, fields[2])
        elif "cpu" in fields[1].split(","):
            selected = (cgroup_v1_quota, fields[2])
    if selected is None:
        return None
    reader, path = selected
    parts = [part for part in path.split("/") if part]
    if ".." in parts:
        return None
    quota = reader(parts)
    return None if quota is None else max(1, quota)


def cgroup_v2_quota(parts):
    if not os.path.exists(os.path.join(CGROUP_ROOT, *parts, "cgroup.controllers")):
        return None

    def read(directory):
        text = read_text(os.path.join(directory, "cpu.max"))
        fields = text.splitlines()[0].split(" ") if text and text.strip() else []
        return whole_cpus(*fields[:2]) if len(fields) >= 2 else None

    return tightest_quota(CGROUP_ROOT, parts, read)


def cgroup_v1_quota(parts):
    def read(directory):
        limit = read_text(os.path.join(directory, "cpu.cfs_quota_us"))
        period = read_text(os.path.join(directory, "cpu.cfs_period_us"))
        return None if limit is None or period is None else whole_cpus(limit.strip(), period.strip())

    for mount, relative in cgroup_v1_cpu_mounts(parts):
        if os.path.exists(os.path.join(mount, *relative)):
            return tightest_quota(mount, relative, read)
    return None


def cgroup_v1_cpu_mounts(parts):
    yield os.path.join(CGROUP_ROOT, "cpu"), parts
    yield os.path.join(CGROUP_ROOT, "cpu,cpuacct"), parts
    # Then the first v1 cpu mount whose bind root contains this group.
    try:
        with open(PROC_MOUNTINFO) as mounts:
            for line in mounts:
                fields = line.strip().split(" ")
                if len(fields) < 7 or fields[-3] != "cgroup" or "cpu" not in fields[-1].split(","):
                    continue
                root = [part for part in fields[3].split("/") if part]
                if parts[:len(root)] == root:
                    yield fields[4], parts[len(root):]
                    return
    except OSError:
        return


def tightest_quota(mount, parts, read):
    # Every ancestor's quota applies, up to and including the mount root.
    quotas = (read(os.path.join(mount, *parts[:depth])) for depth in range(len(parts), -1, -1))
    return min((quota for quota in quotas if quota is not None), default=None)


def whole_cpus(limit, period):
    # A quota of "max" or -1 means unlimited and parses as no quota.
    if not re.fullmatch(r"[0-9]+", limit) or not re.fullmatch(r"[0-9]+", period) or int(period) == 0:
        return None
    return int(limit) // int(period)


def read_text(path):
    try:
        with open(path) as handle:
            return handle.read()
    except OSError:
        return None


def default_test_threads():
    try:
        cpus = len(os.sched_getaffinity(0))
    except AttributeError:
        cpus = os.cpu_count() or 1
    quota = cgroup_cpu_limit()
    return max(1, min(MAX_DEFAULT_TEST_THREADS, cpus, quota or cpus))


class Launcher:
    def __init__(self):
        self.owner = uuid.uuid4().hex
        self.name = "test-postgres-" + self.owner
        self.container_id = None
        self.create_attempted = False
        self.interrupted = None
        self.deadline = time.monotonic() + STARTUP_SECONDS
        self.environment = os.environ.copy()
        self.password = secrets.token_hex(24)
        self.image = os.environ.get("POSTGRES_TEST_IMAGE", "postgres:18")
        self.environment.update(POSTGRES_PASSWORD=self.password, PGPASSWORD=self.password)

    def on_signal(self, number, _frame):
        self.interrupted = number

    def check_interruption(self):
        if self.interrupted is not None:
            raise Interrupted()

    def docker(self, *args, cleanup=False):
        if not cleanup:
            self.check_interruption()
        remaining = 30 if cleanup else min(30, self.deadline - time.monotonic())
        if remaining <= 0:
            raise RuntimeError(f"PostgreSQL startup exceeded its {STARTUP_SECONDS} second budget")
        return subprocess.run(
            ["docker", *args], env=self.environment, text=True,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=remaining,
            check=False,
        )

    @staticmethod
    def require_success(result, operation):
        if result.returncode:
            # Docker arguments and environment may contain credentials; don't
            # render them or arbitrary Docker output in launcher errors.
            raise RuntimeError(f"Docker {operation} failed (exit {result.returncode})")
        return result.stdout.strip()

    def ensure_image(self):
        # `docker create` pulls a missing image implicitly, and a cold pull
        # cannot finish inside the startup budget. Pull it under its own.
        present = self.docker("image", "inspect", "--format", "{{.Id}}", "--", self.image)
        if present.returncode == 0:
            return
        print("Pulling the test PostgreSQL image (uncached)...", file=sys.stderr, flush=True)
        self.check_interruption()
        pull = subprocess.Popen(
            ["docker", "pull", "--quiet", "--", self.image], env=self.environment,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        deadline = time.monotonic() + PULL_SECONDS
        try:
            # Poll so a signal stops a long pull promptly.
            while pull.poll() is None:
                self.check_interruption()
                if time.monotonic() >= deadline:
                    raise RuntimeError(f"Docker image pull exceeded its {PULL_SECONDS} second budget")
                time.sleep(0.05)
        finally:
            if pull.poll() is None:
                pull.kill()
            pull.wait()
        if pull.returncode:
            raise RuntimeError(f"Docker image pull failed (exit {pull.returncode})")

    def start(self):
        self.ensure_image()
        self.deadline = time.monotonic() + STARTUP_SECONDS
        self.create_attempted = True
        result = self.docker(
            "create", "--name", self.name, "--label", f"{OWNER_LABEL}={self.owner}",
            "--publish", "127.0.0.1::5432", "--env", "POSTGRES_PASSWORD",
            "--env", "POSTGRES_INITDB_ARGS=--no-sync",
            "--tmpfs", "/var/lib/postgresql:rw,nosuid,size=1g", "--", self.image,
            "postgres", "-c", "max_connections=300", "-c", "shared_buffers=128MB",
            "-c", "fsync=off", "-c", "synchronous_commit=off", "-c", "full_page_writes=off",
        )
        self.container_id = self.require_success(result, "create")
        if not re.fullmatch(r"[0-9a-f]{64}", self.container_id):
            self.container_id = None
            raise RuntimeError("Docker create did not return a container ID")
        self.require_success(self.docker("start", self.container_id), "start")
        binding = self.require_success(self.docker("port", self.container_id, "5432/tcp"), "port")
        if not re.fullmatch(r"127\.0\.0\.1:[0-9]+", binding):
            raise RuntimeError("PostgreSQL must expose exactly one loopback TCP port")
        while True:
            self.check_interruption()
            # The image's temporary initialization server uses a Unix socket.
            # TCP avoids accepting that server before final startup completes.
            ready = self.docker(
                "exec", "--env", "PGPASSWORD", self.container_id,
                "psql", "-X", "-w", "-h", "127.0.0.1", "-U", "postgres", "-d", "postgres",
                "-Atqc", "SHOW server_version_num",
            )
            if ready.returncode == 0:
                version = ready.stdout.strip()
                if not version.isdecimal() or not 180000 <= int(version) < 190000:
                    raise RuntimeError("Disposable test server must run PostgreSQL 18")
                return f"postgres://postgres:{self.password}@{binding}/postgres?sslmode=disable"
            time.sleep(0.1)

    def run_child(self, argv, url):
        self.check_interruption()
        environment = os.environ.copy()
        environment[ADMIN_URL] = url
        environment.setdefault(TEST_THREADS, str(default_test_threads()))
        child = subprocess.Popen(argv, env=environment, start_new_session=True)
        forwarded = False
        stop_deadline = None
        try:
            while child.poll() is None:
                if self.interrupted is not None and not forwarded:
                    self.signal_child_group(child, self.interrupted)
                    forwarded = True
                    stop_deadline = time.monotonic() + STOP_SECONDS
                if stop_deadline is not None and time.monotonic() >= stop_deadline:
                    self.signal_child_group(child, signal.SIGKILL)
                    break
                time.sleep(0.05)
            status = child.wait()
            return 128 - status if status < 0 else status
        finally:
            # A leader may exit on TERM while a descendant ignores it. The
            # process group belongs to this invocation even after leader exit.
            if child.poll() is None or self.interrupted is not None:
                self.signal_child_group(child, signal.SIGKILL)
            child.wait()

    @staticmethod
    def signal_child_group(child, number):
        try:
            os.killpg(child.pid, number)
        except ProcessLookupError:
            pass

    def cleanup(self):
        if not self.create_attempted:
            return
        if self.container_id is None:
            # A failed/interrupted create can have reached the daemon. Recover
            # only this random name with the matching per-invocation label.
            listing = self.docker(
                "ps", "--all", "--quiet", "--no-trunc",
                "--filter", f"name=^/{self.name}$",
                "--filter", f"label={OWNER_LABEL}={self.owner}", cleanup=True,
            )
            matches = self.require_success(listing, "cleanup ownership lookup").split()
            if not matches:
                return
            if len(matches) != 1 or not re.fullmatch(r"[0-9a-f]{64}", matches[0]):
                raise RuntimeError("Ambiguous cleanup ownership lookup")
            found = self.docker(
                "inspect", "--format", '{{.Id}} {{index .Config.Labels "' + OWNER_LABEL + '"}}',
                matches[0], cleanup=True,
            )
            parts = self.require_success(found, "cleanup ownership inspection").split()
            if len(parts) != 2 or parts[1] != self.owner or parts[0] != matches[0]:
                raise RuntimeError("Refusing cleanup of a container with different ownership")
            self.container_id = parts[0]
        self.require_success(self.docker("rm", "--force", "--volumes", self.container_id, cleanup=True), "cleanup")


def main(argv):
    if not argv:
        print("usage: with-test-postgres.py COMMAND [ARG ...]", file=sys.stderr)
        return 2
    if ADMIN_URL in os.environ:
        if not os.environ[ADMIN_URL].strip():
            print(f"{ADMIN_URL} must not be empty", file=sys.stderr)
            return 2
        # The caller owns this server. The harness still validates PostgreSQL
        # 18 and owns its database leases; the launcher never touches Docker.
        os.execvpe(argv[0], argv, os.environ)
    if not os.environ.get("POSTGRES_TEST_IMAGE", "postgres:18").strip():
        print("POSTGRES_TEST_IMAGE must not be empty", file=sys.stderr)
        return 2
    launcher = Launcher()
    for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(number, launcher.on_signal)
    status = 1
    try:
        status = launcher.run_child(argv, launcher.start())
    except Interrupted:
        pass
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        # TimeoutExpired/OSError can include command arguments. Keep the
        # failure class visible without rendering arbitrary child arguments.
        detail = str(error) if isinstance(error, RuntimeError) else type(error).__name__
        print(f"Test PostgreSQL launcher failed: {detail}", file=sys.stderr)
    finally:
        try:
            launcher.cleanup()
        except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
            print(f"Test PostgreSQL cleanup failed: {type(error).__name__}", file=sys.stderr)
            status = status or 1
    return 128 + launcher.interrupted if launcher.interrupted is not None else status


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
