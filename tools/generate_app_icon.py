#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Generate Stillus.icns from its vector source using the toolchain renderer."""

from __future__ import annotations

import argparse
import struct
import subprocess
import tempfile
from pathlib import Path


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


def render_png(source: Path, output: Path, size: int) -> None:
    subprocess.run(
        [
            "convert",
            "-background",
            "none",
            str(source),
            "-resize",
            f"{size}x{size}",
            "-colorspace",
            "sRGB",
            "-strip",
            "-define",
            "png:exclude-chunk=date,time",
            f"PNG32:{output}",
        ],
        check=True,
    )


def build_icns(source: Path, output: Path) -> None:
    source = source.absolute()
    output = output.absolute()
    if source.is_symlink() or not source.is_file():
        raise ValueError(f"icon source must be a regular SVG file: {source}")
    if source.suffix.lower() != ".svg":
        raise ValueError(f"icon source must end in .svg: {source}")
    if output.suffix.lower() != ".icns":
        raise ValueError(f"icon output must end in .icns: {output}")

    chunks: list[bytes] = []
    with tempfile.TemporaryDirectory(prefix="stillus-icon-") as temp:
        temp_path = Path(temp)
        for kind, size in ICON_REPRESENTATIONS:
            png_path = temp_path / f"{kind}-{size}.png"
            render_png(source, png_path, size)
            png = png_path.read_bytes()
            if not png.startswith(PNG_SIGNATURE):
                raise ValueError(f"renderer did not produce PNG for {kind}")
            chunk_size = 8 + len(png)
            chunks.append(kind.encode("ascii") + struct.pack(">I", chunk_size) + png)

    payload = b"".join(chunks)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(b"icns" + struct.pack(">I", 8 + len(payload)) + payload)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        build_icns(args.source, args.output)
    except (OSError, subprocess.CalledProcessError, ValueError) as error:
        raise SystemExit(f"generate-app-icon: {error}") from error
    print(f"GENERATED_APP_ICON path={args.output.absolute()}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
