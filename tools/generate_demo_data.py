#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only

"""Generate the disposable Stillus demo workspace."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import shutil
import tempfile


DEFAULT_DEMO_WORKSPACE = Path(__file__).resolve().parent.parent / "examples/demo-workspace"
TEMPORARY_ROOT = Path("/tmp")
GENERATED_MARKER = ".stillus-demo-generated-v1"
DEMO_NOTES = {
    "Project Alpha.md": """---
favorited: true
pinned: true
tags: [Work, Planning]
title: Project Alpha
created: '2026-08-29T09:15:00.000Z'
modified: '2026-09-01T08:40:00.000Z'
---
# Project Alpha

Короткий план на сентябрь.

## В фокусе

- закончить редакторский shell
- проверить большой файл
- сохранить совместимость с Notable

> Данные важнее визуального блеска.
""",
    "Reading List.md": """---
favorited: false
pinned: false
tags: [Personal, Reading]
title: Reading List
created: '2026-08-30T11:00:00.000Z'
modified: '2026-08-31T15:10:00.000Z'
---
# Reading List

- Designing Data-Intensive Applications
- The Humane Interface
- A Philosophy of Software Design
""",
    "Weekly Notes.md": """---
favorited: false
pinned: false
tags: [Work]
title: Weekly Notes
created: '2026-08-31T07:30:00.000Z'
modified: '2026-09-01T10:05:00.000Z'
---
# Weekly Notes

Понедельник начался спокойно: сначала проверяем инварианты, потом пишем код.
""",
}


def _validate_workspace(workspace: Path) -> None:
    absolute = Path(os.path.abspath(workspace))
    if absolute == Path(absolute.anchor):
        raise ValueError("refusing to generate demo data at a filesystem root")
    component = Path(absolute.anchor)
    for part in absolute.parts[1:]:
        component /= part
        if component.is_symlink():
            raise ValueError(f"demo workspace path must not contain a symlink: {component}")
    if workspace.exists() and not workspace.is_dir():
        raise ValueError(f"demo workspace is not a directory: {workspace}")
    if absolute == DEFAULT_DEMO_WORKSPACE:
        return
    if not absolute.is_relative_to(TEMPORARY_ROOT):
        raise ValueError(
            "demo data may only be generated at the default workspace or under /tmp"
        )
    if workspace.exists() and any(workspace.iterdir()):
        marker = workspace / GENERATED_MARKER
        if not marker.is_file() or marker.is_symlink():
            raise ValueError(
                f"refusing to reset an unmarked temporary workspace: {workspace}"
            )


def _write_atomic(path: Path, content: str) -> None:
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{path.name}.stillus-demo-tmp-", dir=path.parent
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as handle:
            descriptor = -1
            handle.write(content)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        temporary.unlink(missing_ok=True)


def generate_demo_workspace(workspace: Path) -> Path:
    """Reset a specific disposable workspace to the deterministic demo dataset."""

    workspace = Path(workspace)
    _validate_workspace(workspace)
    notes = workspace / "notes"
    if notes.is_symlink():
        raise ValueError(f"demo notes directory must not be a symlink: {notes}")
    if notes.exists() and not notes.is_dir():
        raise ValueError(f"demo notes path is not a directory: {notes}")

    managed_state = workspace / ".stillus"
    if managed_state.is_symlink():
        raise ValueError(f"demo state directory must not be a symlink: {managed_state}")
    if managed_state.exists():
        if not managed_state.is_dir():
            raise ValueError(f"demo state path is not a directory: {managed_state}")

    marker = workspace / GENERATED_MARKER
    if marker.is_symlink() or (marker.exists() and not marker.is_file()):
        raise ValueError(f"demo ownership marker is not a regular file: {marker}")

    notes.mkdir(parents=True, exist_ok=True)
    existing_notes: list[Path] = []
    for path in notes.glob("*.md"):
        if path.is_symlink():
            raise ValueError(f"demo note must not be a symlink: {path}")
        if not path.is_file():
            raise ValueError(f"demo note path is not a regular file: {path}")
        existing_notes.append(path)

    if managed_state.exists():
        shutil.rmtree(managed_state)
    for path in existing_notes:
        path.unlink()
    for name, content in DEMO_NOTES.items():
        _write_atomic(notes / name, content)
    _write_atomic(marker, "generated; safe to reset\n")
    return workspace


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("workspace", type=Path)
    return parser.parse_args()


def main() -> int:
    workspace = generate_demo_workspace(parse_arguments().workspace)
    print(f"DEMO_DATA_READY workspace={workspace} notes={len(DEMO_NOTES)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
