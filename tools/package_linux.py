#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Add desktop integration to the existing portable Linux binary."""

import platform
from pathlib import Path
import shutil

from register_linux import TEXT_MIME_TYPES

ROOT = Path(__file__).resolve().parent.parent
destination = ROOT / "dist/linux" / platform.machine()
shutil.copyfile(ROOT / "tools/register_linux.py", destination / "Register.py")
shutil.copyfile(ROOT / "app/stillus/assets/stillus-app-icon.svg", destination / "stillus.svg")
shutil.copyfile(ROOT / "LICENSE", destination / "LICENSE.txt")
(destination / "org.stillus.Stillus.desktop").write_text(
    "[Desktop Entry]\nType=Application\nName=Stillus\nExec=stillus -- %F\n"
    "Icon=org.stillus.Stillus\nTerminal=false\nCategories=Office;TextEditor;\n"
    f"MimeType={';'.join(TEXT_MIME_TYPES)};\n",
    encoding="utf-8",
)
