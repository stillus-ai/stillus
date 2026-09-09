#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only

"""Exercise the Makefile cache commands inside `make test-cache`'s empty volume."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path("/var/cache/stillus/target")
PROJECT = Path(__file__).resolve().parent.parent
DEBUG_DIRS = ("debug", "aarch64-apple-darwin/debug", "x86_64-pc-windows-gnu/debug")
KEPT_FILES = (
    "release/deps/library",
    "aarch64-apple-darwin/release/application",
    "x86_64-pc-windows-gnu/release/application.exe",
    "ui-acceptance/scenario/screenshot.png",
    ".rustc_info.json",
    ".hidden/cache",
)


class CargoCacheTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # Refuse to run on an ordinary directory or a populated build volume.
        if not ROOT.is_mount() or any(ROOT.iterdir()):
            raise RuntimeError("Use make test-cache with a separate empty Docker volume")

    def setUp(self):
        self.outside = tempfile.TemporaryDirectory(prefix="stillus-cache-preserve-")
        self.addCleanup(self.outside.cleanup)
        self.addCleanup(self.run_target, "clean-all")
        self.sentinel = Path(self.outside.name) / "debug" / "keep"
        self.sentinel.parent.mkdir()
        self.sentinel.write_text("outside target")

    def run_target(self, target):
        return subprocess.run(
            ["make", "--no-print-directory", target, "RUN="],
            cwd=PROJECT,
            # Cleanup must not follow a target-dir override outside the volume.
            env={**os.environ, "CARGO_TARGET_DIR": self.outside.name},
            check=True, capture_output=True, text=True,
        ).stdout

    def seed(self):
        for relative in (*KEPT_FILES, *(f"{directory}/deps/object" for directory in DEBUG_DIRS)):
            path = ROOT / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(relative)
        link = ROOT / "outside-link"
        link.unlink(missing_ok=True)
        link.symlink_to(self.outside.name, target_is_directory=True)

    def test_cleanup_preserves_release_and_external_files(self):
        self.seed()
        report = self.run_target("cache-size")
        self.assertIn(str(ROOT), report)
        for directory in DEBUG_DIRS:
            self.assertTrue((ROOT / directory / "deps/object").is_file())
        for _ in range(2):
            self.run_target("clean")
            for directory in DEBUG_DIRS:
                self.assertFalse((ROOT / directory).exists())
            for relative in KEPT_FILES:
                self.assertEqual((ROOT / relative).read_text(), relative)
            self.assertEqual(self.sentinel.read_text(), "outside target")
        self.seed()
        for _ in range(2):
            self.run_target("clean-all")
            self.assertEqual(list(ROOT.iterdir()), [])
            self.assertTrue(ROOT.is_mount())
            self.assertEqual(self.sentinel.read_text(), "outside target")

    def test_cleanup_does_not_follow_platform_symlinks(self):
        for relative in ("debug", "aarch64-apple-darwin", "x86_64-pc-windows-gnu"):
            (ROOT / relative).symlink_to(self.outside.name, target_is_directory=True)
        self.run_target("clean")
        self.assertFalse((ROOT / "debug").is_symlink())
        self.assertTrue((ROOT / "aarch64-apple-darwin").is_symlink())
        self.assertTrue((ROOT / "x86_64-pc-windows-gnu").is_symlink())
        self.assertEqual(self.sentinel.read_text(), "outside target")
        self.run_target("clean-all")
        self.assertEqual(list(ROOT.iterdir()), [])
        self.assertEqual(self.sentinel.read_text(), "outside target")


if __name__ == "__main__":
    unittest.main()
