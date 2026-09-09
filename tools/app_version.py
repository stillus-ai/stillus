# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Read and update the application's version without requiring host tomllib."""

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = "app/stillus/Cargo.toml"
VERSION_FILES = (MANIFEST, "Cargo.lock")


def version_match(text, *, lock=False):
    header = r'\[\[package\]\]' if lock else r'\[package\]'
    for section in re.finditer(r'^' + header + r'[^\n]*\n.*?(?=^\[|\Z)', text, re.MULTILINE | re.DOTALL):
        body = section.group()
        if lock and not re.search(r'^name = "stillus-app"$', body, re.MULTILINE):
            continue
        match = re.search(r'^version\s*=\s*"([^"]+)"', body, re.MULTILINE)
        if match:
            start = section.start() + match.start(1)
            return match[1], start, start + len(match[1])
    raise ValueError("application package version is missing")


def read_version(text=None):
    if text is None:
        text = (ROOT / MANIFEST).read_text(encoding="utf-8")
    return version_match(text)[0]


def next_version(version):
    if not re.fullmatch(r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)', version):
        raise ValueError("publish requires a numeric major.minor.patch version")
    major, minor, patch = map(int, version.split("."))
    # Windows VERSIONINFO stores each component in an unsigned 16-bit word.
    if max(major, minor, patch + 1) > 65535:
        raise ValueError("next version exceeds Windows VERSIONINFO limits")
    return f"{major}.{minor}.{patch + 1}"


def replace_version(text, old, new, *, lock=False):
    current, start, end = version_match(text, lock=lock)
    if current != old:
        raise ValueError("application manifest and lockfile versions disagree")
    return text[:start] + new + text[end:]
