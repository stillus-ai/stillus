#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only

from __future__ import annotations

import hashlib
import json
import plistlib
import stat
import struct
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import call, patch

import package_macos
import generate_app_icon


def thin_macho(cpu_type: int) -> bytes:
    return struct.pack(
        "<IIIIIIII",
        0xFEEDFACF,
        cpu_type,
        0,
        2,
        0,
        0,
        0,
        0,
    ) + b"stillus-package-smoke"


def universal_macho() -> bytes:
    table_end = 8 + (2 * 20)
    return (
        struct.pack(">II", 0xCAFEBABE, 2)
        + struct.pack(
            ">IIIII", package_macos.CPU_TYPE_X86_64, 0, table_end, 1, 0
        )
        + struct.pack(
            ">IIIII", package_macos.CPU_TYPE_ARM64, 0, table_end + 1, 1, 0
        )
        + b"xa"
    )


def icon_pixels(path: Path) -> bytes:
    """Decode every validated representation, independent of PNG compression."""
    package_macos.inspect_icns(path)
    data = path.read_bytes()
    pixels = []
    offset = 8
    while offset < len(data):
        size = int.from_bytes(data[offset + 4 : offset + 8], "big")
        decoded = subprocess.run(
            ["convert", "png:-", "-depth", "8", "rgba:-"],
            input=data[offset + 8 : offset + size],
            capture_output=True,
            check=True,
        )
        pixels.append(decoded.stdout)
        offset += size
    return b"".join(pixels)


def assert_icon_pixels_match(case: unittest.TestCase, expected: bytes, actual: bytes) -> None:
    case.assertEqual(len(actual), len(expected))
    differences = [abs(left - right) for left, right in zip(expected, actual) if left != right]
    # ImageMagick Q16 resizing on arm64/x64 differs by one 8-bit level in three
    # channels across the 512/1024 representations. Bound that rounding error
    # across the entire icon; larger color changes and accumulated changes fail.
    case.assertLessEqual(max(differences, default=0), 1, "icon channel changed beyond rounding")
    case.assertLessEqual(len(differences), 4, "icon changed beyond isolated rounding")


class PackageMacosTests(unittest.TestCase):
    def test_adhoc_sign_seals_and_strictly_verifies_complete_bundle(self) -> None:
        bundle = Path("/tmp/Stillus.app")
        with patch.object(package_macos.subprocess, "run") as execute:
            package_macos.sign_adhoc_bundle(bundle)

        options = {
            "check": True,
            "stdout": subprocess.PIPE,
            "stderr": subprocess.PIPE,
            "text": True,
        }
        self.assertEqual(
            execute.call_args_list,
            [
                call(
                    [
                        "/usr/bin/codesign",
                        "--force",
                        "--sign",
                        "-",
                        "--timestamp=none",
                        str(bundle),
                    ],
                    **options,
                ),
                call(
                    [
                        "/usr/bin/codesign",
                        "--verify",
                        "--deep",
                        "--strict",
                        "--verbose=2",
                        str(bundle),
                    ],
                    **options,
                ),
            ],
        )

    def test_builds_arm64_bundle_with_provenance(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            binary = root / "stillus-app"
            binary.write_bytes(thin_macho(package_macos.CPU_TYPE_ARM64))
            output = root / "dist" / "Stillus.app"

            package_macos.build_bundle(binary, output, "0123456789abcdef")

            executable = output / "Contents" / "MacOS" / "Stillus"
            info_path = output / "Contents" / "Info.plist"
            icon_path = output / "Contents" / "Resources" / "Stillus.icns"
            manifest_path = output / "Contents" / "Resources" / "release.json"
            with info_path.open("rb") as source:
                info = plistlib.load(source)
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))

            self.assertEqual(info["CFBundleExecutable"], "Stillus")
            self.assertEqual(info["CFBundleIdentifier"], "org.stillus.Stillus")
            self.assertEqual(info["CFBundleIconFile"], "Stillus.icns")
            self.assertEqual(info["CFBundleShortVersionString"], package_macos.APP_VERSION)
            self.assertEqual(info["CFBundleVersion"], package_macos.APP_VERSION)
            self.assertEqual(manifest["version"], package_macos.APP_VERSION)
            self.assertEqual(
                info["CFBundleDocumentTypes"],
                [
                    {
                        "CFBundleTypeExtensions": ["md", "markdown", "txt"],
                        "CFBundleTypeName": "Markdown and Text",
                        "CFBundleTypeRole": "Editor",
                        "LSHandlerRank": "Owner",
                    }
                ],
            )
            self.assertEqual(icon_path.read_bytes(), package_macos.ICON_SOURCE.read_bytes())
            self.assertEqual(
                package_macos.inspect_icns(icon_path),
                tuple(kind for kind, _ in package_macos.ICON_REPRESENTATIONS),
            )
            self.assertEqual(manifest["architectures"], ["arm64"])
            self.assertEqual(manifest["icon_bytes"], icon_path.stat().st_size)
            self.assertEqual(
                manifest["icon_representations"],
                [kind for kind, _ in package_macos.ICON_REPRESENTATIONS],
            )
            self.assertEqual(
                manifest["icon_sha256"], hashlib.sha256(icon_path.read_bytes()).hexdigest()
            )
            self.assertEqual(manifest["source_revision"], "0123456789abcdef")
            self.assertEqual(
                manifest["binary_sha256"], hashlib.sha256(binary.read_bytes()).hexdigest()
            )
            self.assertTrue(executable.stat().st_mode & stat.S_IXUSR)

    def test_committed_icon_matches_vector_source(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            generated = Path(temp) / "Stillus.icns"
            source = package_macos.ICON_SOURCE.with_name("stillus-app-icon.svg")

            generate_app_icon.build_icns(source, generated)

            assert_icon_pixels_match(
                self, icon_pixels(package_macos.ICON_SOURCE), icon_pixels(generated)
            )

    def test_icon_comparison_rejects_color_damage_and_accumulated_differences(self) -> None:
        reference = bytes([100] * 32)
        for changed in (bytes([102]) + reference[1:], bytes([101] * 5) + reference[5:]):
            with self.subTest(changed_channels=sum(a != b for a, b in zip(reference, changed))):
                with self.assertRaises(AssertionError):
                    assert_icon_pixels_match(self, reference, changed)

    def test_rejects_malformed_icns_resource(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            malformed = Path(temp) / "Stillus.icns"
            malformed.write_bytes(b"icns\x00\x00\x00\x08")

            with self.assertRaisesRegex(
                package_macos.PackageError, "required representations"
            ):
                package_macos.inspect_icns(malformed)

    def test_rejects_binary_without_arm64_slice(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            binary = root / "stillus-app"
            binary.write_bytes(thin_macho(package_macos.CPU_TYPE_X86_64))

            with self.assertRaisesRegex(package_macos.PackageError, "arm64"):
                package_macos.build_bundle(binary, root / "Stillus.app", "revision")

    def test_accepts_universal_binary_with_arm64_slice(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            binary = Path(temp) / "stillus-app"
            binary.write_bytes(universal_macho())

            self.assertEqual(
                package_macos.inspect_macho(binary), ("x86_64", "arm64")
            )

    def test_refuses_to_overwrite_existing_bundle(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            binary = root / "stillus-app"
            binary.write_bytes(thin_macho(package_macos.CPU_TYPE_ARM64))
            output = root / "Stillus.app"
            output.mkdir()

            with self.assertRaisesRegex(package_macos.PackageError, "overwrite"):
                package_macos.build_bundle(binary, output, "revision")

    def test_replaces_only_a_recognized_stillus_bundle(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            first_binary = root / "stillus-app-first"
            first_binary.write_bytes(
                thin_macho(package_macos.CPU_TYPE_ARM64) + b"first"
            )
            second_binary = root / "stillus-app-second"
            second_binary.write_bytes(
                thin_macho(package_macos.CPU_TYPE_ARM64) + b"second"
            )
            output = root / "Stillus.app"
            package_macos.build_bundle(first_binary, output, "first-revision")

            package_macos.build_bundle(
                second_binary,
                output,
                "second-revision",
                replace_existing=True,
            )

            executable = output / "Contents" / "MacOS" / "Stillus"
            manifest = json.loads(
                (output / "Contents" / "Resources" / "release.json").read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(executable.read_bytes(), second_binary.read_bytes())
            self.assertEqual(manifest["source_revision"], "second-revision")

    def test_signing_failure_preserves_previous_bundle(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            first_binary = root / "stillus-app-first"
            first_binary.write_bytes(
                thin_macho(package_macos.CPU_TYPE_ARM64) + b"first"
            )
            second_binary = root / "stillus-app-second"
            second_binary.write_bytes(
                thin_macho(package_macos.CPU_TYPE_ARM64) + b"second"
            )
            output = root / "Stillus.app"
            package_macos.build_bundle(first_binary, output, "first-revision")

            with patch.object(
                package_macos,
                "sign_adhoc_bundle",
                side_effect=package_macos.PackageError("signing failed"),
            ), self.assertRaisesRegex(package_macos.PackageError, "signing failed"):
                package_macos.build_bundle(
                    second_binary,
                    output,
                    "second-revision",
                    adhoc_sign=True,
                    replace_existing=True,
                )

            executable = output / "Contents" / "MacOS" / "Stillus"
            metadata = json.loads(
                (output / "Contents" / "Resources" / "release.json").read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(executable.read_bytes(), first_binary.read_bytes())
            self.assertEqual(metadata["source_revision"], "first-revision")

    def test_refuses_to_replace_unrecognized_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            binary = root / "stillus-app"
            binary.write_bytes(thin_macho(package_macos.CPU_TYPE_ARM64))
            output = root / "Stillus.app"
            output.mkdir()
            (output / "user-data.txt").write_text("keep", encoding="utf-8")

            with self.assertRaisesRegex(package_macos.PackageError, "unrecognized"):
                package_macos.build_bundle(
                    binary, output, "revision", replace_existing=True
                )
            self.assertEqual(
                (output / "user-data.txt").read_text(encoding="utf-8"), "keep"
            )

    def test_refuses_to_replace_file_or_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            binary = root / "stillus-app"
            binary.write_bytes(thin_macho(package_macos.CPU_TYPE_ARM64))
            output = root / "Stillus.app"
            output.write_text("keep", encoding="utf-8")

            with self.assertRaisesRegex(package_macos.PackageError, "unrecognized"):
                package_macos.build_bundle(
                    binary, output, "revision", replace_existing=True
                )
            self.assertEqual(output.read_text(encoding="utf-8"), "keep")

            output.unlink()
            target = root / "elsewhere"
            target.mkdir()
            (target / "user-data.txt").write_text("keep", encoding="utf-8")
            output.symlink_to(target, target_is_directory=True)
            with self.assertRaisesRegex(package_macos.PackageError, "unrecognized"):
                package_macos.build_bundle(
                    binary, output, "revision", replace_existing=True
                )
            self.assertTrue(output.is_symlink())
            self.assertEqual(
                (target / "user-data.txt").read_text(encoding="utf-8"), "keep"
            )

    def test_rejects_symlinked_input_binary(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            binary = root / "stillus-app-real"
            binary.write_bytes(thin_macho(package_macos.CPU_TYPE_ARM64))
            symlink = root / "stillus-app"
            symlink.symlink_to(binary)

            with self.assertRaisesRegex(package_macos.PackageError, "symlink"):
                package_macos.build_bundle(symlink, root / "Stillus.app", "revision")


if __name__ == "__main__":
    unittest.main()
