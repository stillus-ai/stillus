#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Exercise opt-in registration in an isolated user data directory."""

from pathlib import Path
import platform
import re
import runpy
import shutil
import tempfile
import unittest
from unittest.mock import patch

from register_linux import APP_ID, TEXT_MIME_TYPES, desktop_entry, exec_argument, register
from package_macos import TEXT_EXTENSIONS


class RegistrationTests(unittest.TestCase):
    def test_text_extensions_match_windows_registration(self):
        script = Path(__file__).with_name("register_windows.ps1").read_text()
        declaration = script.split("$extensions = @(\n", 1)[1].split("\n)", 1)[0]
        windows_extensions = re.findall(r"'\.([a-z0-9]+)'", declaration)
        self.assertEqual(windows_extensions, TEXT_EXTENSIONS)
        self.assertEqual(len(TEXT_EXTENSIONS), len(set(TEXT_EXTENSIONS)))
        self.assertTrue({"md", "markdown", "txt", "log", "json", "csv", "tsv", "php", "js", "html"}.issubset(TEXT_EXTENSIONS))
        self.assertEqual(script.count("foreach ($extension in $extensions)"), 2)

    def test_linux_package_and_registration_declare_the_same_text_types(self):
        self.assertEqual(len(TEXT_MIME_TYPES), len(set(TEXT_MIME_TYPES)))
        self.assertTrue({"text/plain", "application/json", "text/csv", "text/tab-separated-values", "application/x-php", "text/javascript", "text/html"}.issubset(TEXT_MIME_TYPES))
        source = Path(__file__).resolve().parent
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "tools").mkdir()
            for name in ("package_linux.py", "register_linux.py"):
                shutil.copyfile(source / name, root / "tools" / name)
            assets = root / "app/stillus/assets"
            assets.mkdir(parents=True)
            (assets / "stillus-app-icon.svg").write_text("<svg/>")
            (root / "LICENSE").write_text("test license")
            destination = root / "dist/linux" / platform.machine()
            destination.mkdir(parents=True)
            runpy.run_path(str(root / "tools/package_linux.py"), run_name="__main__")
            packaged = (destination / f"{APP_ID}.desktop").read_text()
            registered = desktop_entry(destination / "stillus")
            expected = "MimeType=" + ";".join(TEXT_MIME_TYPES) + ";"
            self.assertIn(expected, packaged.splitlines())
            self.assertIn(expected, registered.splitlines())
            self.assertEqual((destination / "Register.py").read_bytes(), (source / "register_linux.py").read_bytes())

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
