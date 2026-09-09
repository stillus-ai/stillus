#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Exercise opt-in registration in an isolated user data directory."""

from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from register_linux import APP_ID, desktop_entry, exec_argument, register


class RegistrationTests(unittest.TestCase):
    def test_install_repeat_remove_preserves_defaults_and_other_apps(self):
        with tempfile.TemporaryDirectory(prefix="stillus desktop 日本語 ") as temporary:
            root = Path(temporary)
            package = root / 'package $ ` " \\ % space'
            package.mkdir()
            (package / "stillus").write_text("binary")
            (package / "stillus").chmod(0o755)
            (package / "stillus.svg").write_text("<svg/>")
            data = root / "data"
            data.mkdir()
            defaults = data / "mimeapps.list"
            defaults.write_text("[Default Applications]\ntext/plain=other.desktop;\n")
            with patch("register_linux.shutil.which", return_value=None):
                register(package, data)
                register(package, data)
                desktop = data / "applications" / f"{APP_ID}.desktop"
                self.assertEqual(desktop.read_text(), desktop_entry(package / "stillus"))
                self.assertIn(" -- %F\n", desktop.read_text())
                self.assertIn("%%", desktop.read_text())
                register(root / "another package", data, remove=True)
                self.assertTrue(desktop.exists())
                register(package, data, remove=True)
                register(package, data, remove=True)
                self.assertFalse(desktop.exists())
                self.assertEqual(defaults.read_text(), "[Default Applications]\ntext/plain=other.desktop;\n")

    def test_rejects_newline_in_paths(self):
        with self.assertRaises(ValueError):
            exec_argument("/tmp/injected\nExec=other")

    def test_foreign_desktop_entry_is_preserved(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "stillus").write_text("binary")
            (root / "stillus").chmod(0o755)
            (root / "applications").mkdir()
            desktop = root / "applications" / f"{APP_ID}.desktop"
            desktop.write_text("unrelated")
            with self.assertRaises(ValueError):
                register(root, root)
            self.assertEqual(desktop.read_text(), "unrelated")


if __name__ == "__main__":
    unittest.main()
