#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Assemble a deterministic macOS application bundle."""

from __future__ import annotations

import argparse
import hashlib
import json
import plistlib
import shutil
import stat
import subprocess
import tempfile
from pathlib import Path

from app_version import read_version

APP_NAME = "Stillus"
APP_VERSION = read_version()
BUNDLE_IDENTIFIER = "org.stillus.Stillus"
ICON_FILENAME = "Stillus.icns"
PROJECT_ROOT = Path(__file__).resolve().parent.parent
ICON_SOURCE = PROJECT_ROOT / "app" / "stillus" / "assets" / ICON_FILENAME
ICON_REPRESENTATIONS = (
    ("icp4", 16),
    ("icp5", 32),
    ("icp6", 64),
    ("ic07", 128),
    ("ic08", 256),
    ("ic09", 512),
    ("ic10", 1024),
)
PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"
CPU_TYPE_ARM64 = 0x0100000C
CPU_TYPE_X86_64 = 0x01000007


class PackageError(ValueError):
    """Raised when an input cannot safely produce the requested bundle."""


def sign_adhoc_bundle(bundle: Path) -> None:
    """Seal a complete bundle without requiring an Apple Developer identity."""

    commands = (
        [
            "/usr/bin/codesign",
            "--force",
            "--sign",
            "-",
            "--timestamp=none",
            str(bundle),
        ],
        [
            "/usr/bin/codesign",
            "--verify",
            "--deep",
            "--strict",
            "--verbose=2",
            str(bundle),
        ],
    )
    for command in commands:
        try:
            subprocess.run(
                command,
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
        except FileNotFoundError as error:
            raise PackageError("ad-hoc signing requires /usr/bin/codesign") from error
        except subprocess.CalledProcessError as error:
            detail = " ".join((error.stderr or error.stdout or "").split())
            if len(detail) > 512:
                detail = detail[-512:]
            suffix = f": {detail}" if detail else ""
            raise PackageError(
                f"ad-hoc code signing failed with exit status {error.returncode}{suffix}"
            ) from error


def _architecture_name(cpu_type: int) -> str:
    cpu_type &= 0xFFFFFFFF
    return {
        CPU_TYPE_ARM64: "arm64",
        CPU_TYPE_X86_64: "x86_64",
    }.get(cpu_type, f"unknown:{cpu_type:#x}")


def inspect_macho(path: Path) -> tuple[str, ...]:
    """Return the architectures in a thin or universal Mach-O executable."""

    file_size = path.stat().st_size
    with path.open("rb") as source:
        header = source.read(8)
        if len(header) < 8:
            raise PackageError(f"binary is too short to be Mach-O: {path}")

        thin_formats = {
            b"\xcf\xfa\xed\xfe": "little",
            b"\xfe\xed\xfa\xcf": "big",
        }
        if header[:4] in thin_formats:
            if len(source.read(24)) != 24:
                raise PackageError(f"binary has a truncated Mach-O 64-bit header: {path}")
            byteorder = thin_formats[header[:4]]
            cpu_type = int.from_bytes(header[4:8], byteorder=byteorder, signed=False)
            architectures = (_architecture_name(cpu_type),)
        else:
            fat_formats = {
                b"\xca\xfe\xba\xbe": ("big", 20),
                b"\xca\xfe\xba\xbf": ("big", 32),
                b"\xbe\xba\xfe\xca": ("little", 20),
                b"\xbf\xba\xfe\xca": ("little", 32),
            }
            fat_format = fat_formats.get(header[:4])
            if fat_format is None:
                raise PackageError(f"binary is not a recognized Mach-O file: {path}")
            byteorder, entry_size = fat_format
            count = int.from_bytes(header[4:8], byteorder=byteorder, signed=False)
            if count == 0 or count > 64:
                raise PackageError(f"invalid Mach-O architecture count {count}: {path}")
            table = source.read(count * entry_size)
            if len(table) != count * entry_size:
                raise PackageError(f"truncated Mach-O architecture table: {path}")
            architectures_list = []
            for entry_offset in range(0, len(table), entry_size):
                entry = table[entry_offset : entry_offset + entry_size]
                cpu_type = int.from_bytes(entry[0:4], byteorder=byteorder, signed=False)
                if entry_size == 20:
                    slice_offset = int.from_bytes(
                        entry[8:12], byteorder=byteorder, signed=False
                    )
                    slice_size = int.from_bytes(
                        entry[12:16], byteorder=byteorder, signed=False
                    )
                else:
                    slice_offset = int.from_bytes(
                        entry[8:16], byteorder=byteorder, signed=False
                    )
                    slice_size = int.from_bytes(
                        entry[16:24], byteorder=byteorder, signed=False
                    )
                if (
                    slice_size == 0
                    or slice_offset > file_size
                    or slice_size > file_size - slice_offset
                ):
                    raise PackageError(f"invalid Mach-O slice bounds: {path}")
                architectures_list.append(_architecture_name(cpu_type))
            architectures = tuple(architectures_list)

    if "arm64" not in architectures:
        rendered = ", ".join(architectures)
        raise PackageError(
            f"binary does not contain the required Apple Silicon arm64 slice: {rendered}"
        )
    return architectures


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def inspect_icns(path: Path) -> tuple[str, ...]:
    """Validate the repository app icon and return its representation types."""

    if path.is_symlink() or not path.is_file():
        raise PackageError(f"icon must be a regular file, not a symlink: {path}")
    data = path.read_bytes()
    if len(data) < 8 or data[:4] != b"icns":
        raise PackageError(f"icon is not a recognized ICNS file: {path}")
    declared_size = int.from_bytes(data[4:8], byteorder="big", signed=False)
    if declared_size != len(data):
        raise PackageError(f"icon ICNS size does not match its header: {path}")

    expected = dict(ICON_REPRESENTATIONS)
    representations: list[str] = []
    offset = 8
    while offset < len(data):
        if len(data) - offset < 8:
            raise PackageError(f"icon has a truncated ICNS chunk header: {path}")
        try:
            kind = data[offset : offset + 4].decode("ascii")
        except UnicodeDecodeError as error:
            raise PackageError(f"icon has a non-ASCII ICNS chunk type: {path}") from error
        chunk_size = int.from_bytes(
            data[offset + 4 : offset + 8], byteorder="big", signed=False
        )
        if chunk_size < 8 or chunk_size > len(data) - offset:
            raise PackageError(f"icon has invalid ICNS chunk bounds: {path}")
        payload = data[offset + 8 : offset + chunk_size]
        expected_size = expected.get(kind)
        if expected_size is None or kind in representations:
            raise PackageError(f"icon has unexpected ICNS representation {kind}: {path}")
        if (
            len(payload) < 24
            or payload[:8] != PNG_SIGNATURE
            or payload[12:16] != b"IHDR"
        ):
            raise PackageError(f"icon representation {kind} is not a PNG: {path}")
        width = int.from_bytes(payload[16:20], byteorder="big", signed=False)
        height = int.from_bytes(payload[20:24], byteorder="big", signed=False)
        if width != expected_size or height != expected_size:
            raise PackageError(
                f"icon representation {kind} must be {expected_size}x{expected_size}: {path}"
            )
        representations.append(kind)
        offset += chunk_size

    expected_kinds = tuple(kind for kind, _ in ICON_REPRESENTATIONS)
    if tuple(representations) != expected_kinds:
        raise PackageError(f"icon does not contain the required representations: {path}")
    return tuple(representations)


def is_replaceable_stillus_bundle(path: Path) -> bool:
    """Return whether path is a regular Stillus bundle produced by this packager."""

    if path.is_symlink() or not path.is_dir():
        return False
    info_path = path / "Contents" / "Info.plist"
    executable = path / "Contents" / "MacOS" / APP_NAME
    if (
        info_path.is_symlink()
        or not info_path.is_file()
        or executable.is_symlink()
        or not executable.is_file()
    ):
        return False
    try:
        with info_path.open("rb") as source:
            info = plistlib.load(source)
    except (OSError, plistlib.InvalidFileException):
        return False
    return (
        info.get("CFBundleIdentifier") == BUNDLE_IDENTIFIER
        and info.get("CFBundleExecutable") == APP_NAME
        and info.get("CFBundlePackageType") == "APPL"
    )


def build_bundle(
    binary: Path,
    output: Path,
    source_revision: str,
    *,
    adhoc_sign: bool = False,
    replace_existing: bool = False,
) -> Path:
    binary = binary.absolute()
    output = output.absolute()
    source_revision = source_revision.strip()

    if binary.is_symlink() or not binary.is_file():
        raise PackageError(f"binary must be a regular file, not a symlink: {binary}")
    if output.suffix != ".app":
        raise PackageError(f"output must end in .app: {output}")
    output_exists = output.exists() or output.is_symlink()
    if output_exists and not replace_existing:
        raise PackageError(f"refusing to overwrite existing output: {output}")
    if output_exists and not is_replaceable_stillus_bundle(output):
        raise PackageError(f"refusing to replace unrecognized output: {output}")
    if (
        not source_revision
        or len(source_revision) > 128
        or any(character.isspace() for character in source_revision)
    ):
        raise PackageError(
            "source revision must be 1-128 characters without whitespace"
        )

    architectures = inspect_macho(binary)
    source_hash = sha256(binary)
    icon_representations = inspect_icns(ICON_SOURCE)
    icon_hash = sha256(ICON_SOURCE)
    output.parent.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix=".stillus-package-", dir=output.parent) as temp:
        bundle = Path(temp) / output.name
        executable_dir = bundle / "Contents" / "MacOS"
        resources_dir = bundle / "Contents" / "Resources"
        executable_dir.mkdir(parents=True)
        resources_dir.mkdir(parents=True)

        executable = executable_dir / APP_NAME
        shutil.copyfile(binary, executable)
        executable.chmod(
            stat.S_IRUSR
            | stat.S_IWUSR
            | stat.S_IXUSR
            | stat.S_IRGRP
            | stat.S_IXGRP
            | stat.S_IROTH
            | stat.S_IXOTH
        )
        if sha256(executable) != source_hash:
            raise PackageError("copied executable checksum differs from the input binary")

        icon = resources_dir / ICON_FILENAME
        shutil.copyfile(ICON_SOURCE, icon)
        if sha256(icon) != icon_hash:
            raise PackageError("copied app icon checksum differs from the source resource")

        plist = {
            "CFBundleDevelopmentRegion": "en",
            "CFBundleDisplayName": APP_NAME,
            "CFBundleDocumentTypes": [
                {
                    "CFBundleTypeExtensions": ["md", "markdown", "txt"],
                    "CFBundleTypeName": "Markdown and Text",
                    "CFBundleTypeRole": "Editor",
                    "LSHandlerRank": "Owner",
                }
            ],
            "CFBundleExecutable": APP_NAME,
            "CFBundleIdentifier": BUNDLE_IDENTIFIER,
            "CFBundleInfoDictionaryVersion": "6.0",
            "CFBundleIconFile": ICON_FILENAME,
            "CFBundleName": APP_NAME,
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": APP_VERSION,
            "CFBundleVersion": APP_VERSION,
            "LSArchitecturePriority": ["arm64"],
            "NSHighResolutionCapable": True,
        }
        with (bundle / "Contents" / "Info.plist").open("wb") as destination:
            plistlib.dump(plist, destination, fmt=plistlib.FMT_XML, sort_keys=True)

        manifest = {
            "app_name": APP_NAME,
            "architectures": list(architectures),
            "binary_bytes": executable.stat().st_size,
            "binary_sha256": source_hash,
            "bundle_identifier": BUNDLE_IDENTIFIER,
            "format": 1,
            "icon_bytes": icon.stat().st_size,
            "icon_representations": list(icon_representations),
            "icon_sha256": icon_hash,
            "source_revision": source_revision,
            "target": "aarch64-apple-darwin",
            "version": APP_VERSION,
        }
        (resources_dir / "release.json").write_text(
            json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        if adhoc_sign:
            sign_adhoc_bundle(bundle)

        backup = Path(temp) / "previous-Stillus.app"
        if output_exists:
            if not is_replaceable_stillus_bundle(output):
                raise PackageError(
                    f"refusing to replace output changed during packaging: {output}"
                )
            output.rename(backup)
        try:
            bundle.rename(output)
        except OSError:
            if backup.exists() and not output.exists():
                backup.rename(output)
            raise

    return output


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument(
        "--adhoc-sign",
        action="store_true",
        help="seal the completed bundle with the local ad-hoc signing identity",
    )
    parser.add_argument(
        "--replace-existing",
        action="store_true",
        help="replace only an existing bundle recognized as a packaged Stillus app",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        output = build_bundle(
            args.binary,
            args.output,
            args.source_revision,
            adhoc_sign=args.adhoc_sign,
            replace_existing=args.replace_existing,
        )
    except (OSError, PackageError) as error:
        raise SystemExit(f"package-macos: {error}") from error
    print(f"PACKAGED_APP path={output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
