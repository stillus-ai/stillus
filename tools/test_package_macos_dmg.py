#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only

from __future__ import annotations

import os
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import package_macos
import package_macos_dmg
from test_package_macos import thin_macho


class DmgTests(unittest.TestCase):
    def setUp(self) -> None:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        binary = self.root / "stillus-app"
        binary.write_bytes(thin_macho(package_macos.CPU_TYPE_ARM64))
        self.bundle = package_macos.build_bundle(
            binary, self.root / "Stillus.app", "test-revision"
        )
        self.output = self.root / "Stillus.dmg"

    def create_image(self, command: list[str], **kwargs: object) -> None:
        if command[1] == "create":
            staging = Path(command[command.index("-srcfolder") + 1])
            self.assertEqual(
                sorted(p.name for p in staging.iterdir()), ["Applications", "Stillus.app"]
            )
            self.assertEqual(os.readlink(staging / "Applications"), "/Applications")
            self.assertEqual(
                (staging / "Stillus.app/Contents/MacOS/Stillus").read_bytes(),
                (self.bundle / "Contents/MacOS/Stillus").read_bytes(),
            )
            Path(command[-1]).write_bytes(b"new image")
        elif command[1] == "verify":
            self.assertEqual(Path(command[-1]).read_bytes(), b"new image")
        else:
            self.fail(f"unexpected command: {command}")

    def test_image_contains_app_and_installation_link(self) -> None:
        executable = self.bundle / "Contents/MacOS/Stillus"
        original = executable.read_bytes()
        with patch.object(package_macos_dmg.subprocess, "run", side_effect=self.create_image):
            package_macos_dmg.build_dmg(self.bundle, self.output)
        self.assertEqual(self.output.read_bytes(), b"new image")
        self.assertEqual(executable.read_bytes(), original)
        self.assertEqual(list(self.root.glob(".stillus-dmg-*")), [])

    def test_explicit_replacement_publishes_new_image(self) -> None:
        self.output.write_bytes(b"previous image")
        with patch.object(package_macos_dmg.subprocess, "run", side_effect=self.create_image):
            package_macos_dmg.build_dmg(self.bundle, self.output, replace_existing=True)
        self.assertEqual(self.output.read_bytes(), b"new image")

    def test_creation_and_verification_failures_preserve_previous_image(self) -> None:
        for failed_step in ("create", "verify"):
            with self.subTest(step=failed_step):
                self.output.write_bytes(b"previous image")

                def fail(command: list[str], **kwargs: object) -> None:
                    self.create_image(command, **kwargs)
                    if command[1] == failed_step:
                        raise subprocess.CalledProcessError(1, command)

                with patch.object(package_macos_dmg.subprocess, "run", side_effect=fail):
                    with self.assertRaisesRegex(package_macos.PackageError, "failed"):
                        package_macos_dmg.build_dmg(self.bundle, self.output, replace_existing=True)
                self.assertEqual(self.output.read_bytes(), b"previous image")
                self.assertEqual(list(self.root.glob(".stillus-dmg-*")), [])

    def test_existing_file_requires_explicit_replacement(self) -> None:
        self.output.write_bytes(b"keep")
        with self.assertRaisesRegex(package_macos.PackageError, "overwrite"):
            package_macos_dmg.build_dmg(self.bundle, self.output)
        self.assertEqual(self.output.read_bytes(), b"keep")

    def test_refuses_directory_and_symlink_outputs(self) -> None:
        self.output.mkdir()
        with self.assertRaisesRegex(package_macos.PackageError, "non-regular"):
            package_macos_dmg.build_dmg(self.bundle, self.output, replace_existing=True)
        self.output.rmdir()
        target = self.root / "user-file"
        target.write_bytes(b"keep")
        self.output.symlink_to(target)
        with self.assertRaisesRegex(package_macos.PackageError, "non-regular"):
            package_macos_dmg.build_dmg(self.bundle, self.output, replace_existing=True)
        self.assertEqual(target.read_bytes(), b"keep")
        self.assertTrue(self.output.is_symlink())

    def test_file_appearing_during_packaging_is_not_overwritten(self) -> None:
        def create_concurrent_file(command: list[str], **kwargs: object) -> None:
            self.create_image(command, **kwargs)
            self.output.write_bytes(b"concurrent output")

        with patch.object(package_macos_dmg.subprocess, "run", side_effect=create_concurrent_file):
            with self.assertRaisesRegex(package_macos.PackageError, "overwrite"):
                package_macos_dmg.build_dmg(self.bundle, self.output)
        self.assertEqual(self.output.read_bytes(), b"concurrent output")

    def test_rejects_unknown_bundle_and_invalid_destinations(self) -> None:
        with self.assertRaisesRegex(package_macos.PackageError, "packaged Stillus"):
            package_macos_dmg.build_dmg(self.root, self.output)
        with self.assertRaisesRegex(package_macos.PackageError, "end in .dmg"):
            package_macos_dmg.build_dmg(self.bundle, self.root / "image.zip")
        with self.assertRaisesRegex(package_macos.PackageError, "outside"):
            package_macos_dmg.build_dmg(self.bundle, self.bundle / "image.dmg")
        alias = self.root / "bundle-alias"
        alias.symlink_to(self.bundle, target_is_directory=True)
        with self.assertRaisesRegex(package_macos.PackageError, "outside"):
            package_macos_dmg.build_dmg(self.bundle, alias / "image.dmg")

    def test_missing_hdiutil_fails_explicitly(self) -> None:
        with patch.object(package_macos_dmg.subprocess, "run", side_effect=FileNotFoundError):
            with self.assertRaisesRegex(package_macos.PackageError, "requires macOS"):
                package_macos_dmg.build_dmg(self.bundle, self.output)
        self.assertFalse(self.output.exists())


class NativeDmgSmoke(unittest.TestCase):
    def test_mounted_image_preserves_bundle_and_signature(self) -> None:
        bundle = Path(__file__).resolve().parent.parent / "dist/Stillus.app"
        with tempfile.TemporaryDirectory(prefix="stillus-dmg-smoke-") as temp:
            root = Path(temp)
            output = package_macos_dmg.build_dmg(bundle, root / "Stillus.dmg")
            mount = root / "mounted"
            subprocess.run(
                ["/usr/bin/hdiutil", "attach", "-readonly", "-nobrowse", "-noautoopen",
                 "-mountpoint", str(mount), str(output)], check=True,
            )
            try:
                self.assertEqual(os.readlink(mount / "Applications"), "/Applications")
                copied = mount / "Stillus.app"
                expected = {p.relative_to(bundle) for p in bundle.rglob("*")}
                self.assertEqual({p.relative_to(copied) for p in copied.rglob("*")}, expected)
                for relative in expected:
                    source, destination = bundle / relative, copied / relative
                    if source.is_symlink():
                        self.assertEqual(os.readlink(source), os.readlink(destination))
                    elif source.is_file():
                        self.assertEqual(package_macos.sha256(source), package_macos.sha256(destination))
                        self.assertEqual(stat.S_IMODE(source.stat().st_mode),
                                         stat.S_IMODE(destination.stat().st_mode))
                subprocess.run(
                    ["/usr/bin/codesign", "--verify", "--deep", "--strict", str(copied)],
                    check=True,
                )
            finally:
                subprocess.run(["/usr/bin/hdiutil", "detach", str(mount)], check=True)


if __name__ == "__main__":
    native = "--native" in sys.argv
    if native:
        sys.argv.remove("--native")
    unittest.main(defaultTest="NativeDmgSmoke" if native else "DmgTests")
