#!/usr/bin/env python3
"""Exercise policy behavior against real Git baselines and renames."""

import contextlib
import importlib.util
import io
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("policy", Path(__file__).resolve().parents[1] / "check-rust-policy.py")
policy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(policy)


class RustPolicyTests(unittest.TestCase):
    def setUp(self):
        self.original = Path.cwd()
        self.temp = tempfile.TemporaryDirectory()
        os.chdir(self.temp.name)
        self.addCleanup(self.temp.cleanup)
        self.addCleanup(os.chdir, self.original)
        self.git("init", "-q")
        self.git("config", "user.name", "Policy Test")
        self.git("config", "user.email", "policy@example.invalid")
        Path("crates").mkdir()
        Path("tools").mkdir()

    def git(self, *args):
        return subprocess.check_output(["git", *args], stderr=subprocess.DEVNULL).decode().strip()

    def source(self, path, lines, annotation=""):
        Path(path).write_text((annotation + "\n" if annotation else "") + "// source\n" * (lines - bool(annotation)))

    def commit(self):
        self.git("add", ".")
        self.git("commit", "-qm", "fixture")
        return self.git("rev-parse", "HEAD")

    def check(self, name, base=None):
        with contextlib.redirect_stdout(io.StringIO()):
            return policy.check(["crates"], name, base)

    def test_size_limits_annotations_and_root_scope(self):
        self.source("crates/file.rs", 1)
        base = self.commit()
        for lines, annotation, fails in (
            (800, "", False),
            (801, "", True),
            (1000, "// agentic-loc-exception: retained policy", False),
            (1001, "// agentic-loc-exception: retained policy", True),
            (801, "// @generated", False),
        ):
            with self.subTest(lines=lines, annotation=annotation):
                self.source("crates/file.rs", lines, annotation)
                self.source("tools/ignored.rs", 1200)
                self.commit()
                self.assertEqual(self.check("rust-file-loc", base), fails)

    def test_legacy_debt_and_renames_cannot_grow(self):
        self.source("crates/old.rs", 1100)
        base = self.commit()
        self.git("mv", "crates/old.rs", "crates/new.rs")
        self.commit()
        self.assertFalse(self.check("rust-file-loc", base))
        self.source("crates/new.rs", 1099)
        self.commit()
        self.assertFalse(self.check("rust-file-loc", base))
        self.source("crates/new.rs", 1101)
        self.commit()
        self.assertTrue(self.check("rust-file-loc", base))

    def test_mod_rs_is_scoped_to_tracked_crate_files(self):
        Path("tools/mod.rs").write_text("")
        self.commit()
        Path("crates/mod.rs").write_text("")
        self.assertFalse(self.check("no-mod-rs"))
        self.git("add", "crates/mod.rs")
        self.assertTrue(self.check("no-mod-rs"))

    def test_multi_commit_comparison_covers_earlier_growth(self):
        self.source("crates/file.rs", 800)
        base = self.commit()
        self.source("crates/file.rs", 801)
        self.commit()
        Path("README.md").write_text("later unrelated change\n")
        self.commit()
        self.assertTrue(self.check("rust-file-loc", base))


if __name__ == "__main__":
    unittest.main()
