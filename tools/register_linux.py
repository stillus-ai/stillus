#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Opt-in desktop registration for a portable Stillus Linux package."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import shutil
import subprocess

APP_ID = "org.stillus.Stillus"


def exec_argument(value: str) -> str:
    # Exec quoting and Desktop Entry string escaping are two separate layers.
    if any(character in value for character in "\n\r\0"):
        raise ValueError("desktop paths must not contain line breaks or NUL")
    escaped = value.replace("%", "%%")
    for character in ('\\', '"', '`', '$'):
        escaped = escaped.replace(character, '\\' + character)
    return ('"' + escaped + '"').replace('\\', '\\\\')


def desktop_entry(executable: Path) -> str:
    return (
        "[Desktop Entry]\nType=Application\nName=Stillus\n"
        "Comment=Local Markdown editor and RSS reader\n"
        f"Exec=/usr/bin/env -- {exec_argument(str(executable))} -- %F\n"
        f"Icon={APP_ID}\nTerminal=false\nCategories=Office;TextEditor;\n"
        "MimeType=text/markdown;text/plain;\n"
        f"X-Stillus-Executable={exec_argument(str(executable))}\n"
    )


def register(package: Path, data: Path, remove: bool = False) -> None:
    executable = package.resolve() / "stillus"
    entry = desktop_entry(executable)
    desktop = data / "applications" / f"{APP_ID}.desktop"
    icon = data / "icons/hicolor/scalable/apps" / f"{APP_ID}.svg"
    if desktop.is_symlink() or icon.is_symlink():
        raise ValueError("refusing to replace or remove a linked desktop entry/icon")
    owner = f"X-Stillus-Executable={exec_argument(str(executable))}"
    if remove:
        if desktop.exists() and owner in desktop.read_text().splitlines():
            desktop.unlink()
            if icon.exists() and icon.read_bytes() == (package / "stillus.svg").read_bytes():
                icon.unlink()
    else:
        if not executable.is_file() or not os.access(executable, os.X_OK):
            raise ValueError(f"executable is missing: {executable}")
        if desktop.exists() and not any(
            line.startswith("X-Stillus-Executable=") for line in desktop.read_text().splitlines()
        ):
            raise ValueError(f"refusing to replace unrelated desktop entry: {desktop}")
        desktop.parent.mkdir(parents=True, exist_ok=True)
        icon.parent.mkdir(parents=True, exist_ok=True)
        desktop.write_text(entry, encoding="utf-8")
        shutil.copyfile(package / "stillus.svg", icon)
    update = shutil.which("update-desktop-database")
    if update and desktop.parent.is_dir():
        subprocess.run([update, str(desktop.parent)], check=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--remove", action="store_true")
    args = parser.parse_args()
    data = Path(os.environ.get("XDG_DATA_HOME", str(Path.home() / ".local/share")))
    if not data.is_absolute():
        raise SystemExit("XDG_DATA_HOME must be absolute")
    register(Path(__file__).resolve().parent, data, args.remove)
    print("Stillus desktop registration removed." if args.remove else "Stillus is available in Open With; defaults are unchanged.")


if __name__ == "__main__":
    main()
