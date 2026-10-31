#!/usr/bin/env python3
"""Preserve the contract-v4 Rust policies retired from Jig's native checks."""

import argparse
import subprocess
from pathlib import Path


def git(*args):
    return subprocess.check_output(["git", *args])


def check(roots, policy, base=None):
    if policy == "no-mod-rs":
        violations = [
            path for path in git("ls-files", "-z", "--", *roots).decode().split("\0")
            if path and Path(path).name == "mod.rs"
        ]
        for path in violations:
            print(f"{path}: mod.rs files are disallowed")
        return bool(violations)

    # CI compares the event's exact prior tree with HEAD, including all commits
    # in a push. Match the old native policy's rename and baseline-debt handling.
    entries = git("diff", "--name-status", "-z", "--diff-filter=AMRT", base, "HEAD", "--", *roots).decode().split("\0")
    failures = False
    while entries[0]:
        status = entries.pop(0)
        old = entries.pop(0)
        path = entries.pop(0) if status.startswith(("R", "C")) else old
        if not path.endswith(".rs") or not Path(path).is_file():
            continue
        current = Path(path).read_text().splitlines()
        previous = 0
        for prior_path in dict.fromkeys((path, old)):
            prior = subprocess.run(["git", "show", f"{base}:{prior_path}"], capture_output=True)
            if prior.returncode == 0:
                previous = len(prior.stdout.decode().splitlines())
                break
        count = len(current)
        exception = any("agentic-loc-exception:" in line or "@generated" in line for line in current[:40])
        if count > 800:
            if count <= previous:
                print(f"{path}: warning: {count} LOC remains above the limit without growth")
            elif count <= 1000 and exception:
                print(f"{path}: warning: {count} LOC uses an explicit exception")
            else:
                limit = 1000 if count > 1000 else 800
                print(f"{path}: {count} LOC exceeds the {limit} LOC limit")
                failures = True
        elif count > 600:
            print(f"{path}: warning: {count} LOC is approaching the hard limit")
        elif count > 500:
            print(f"{path}: warning: {count} LOC is above the soft limit")
        elif count > 400:
            print(f"{path}: notice: {count} LOC is approaching the soft limit")
    return failures


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("policy", choices=("no-mod-rs", "rust-file-loc"))
    parser.add_argument("--changed-against")
    args = parser.parse_args()
    if args.policy == "rust-file-loc" and not args.changed_against:
        parser.error("rust-file-loc requires --changed-against")
    # The repository owns its Rust crate roots; keep policy discovery aligned.
    import tomllib
    with open(".jig.toml", "rb") as config:
        roots = tomllib.load(config)["rust_crate_roots"]
    raise SystemExit(check(roots, args.policy, args.changed_against))
