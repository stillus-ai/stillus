#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Package an existing Stillus bundle for drag-and-drop installation on macOS."""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

from package_macos import PackageError, is_replaceable_stillus_bundle


def build_dmg(bundle: Path, output: Path, *, replace_existing: bool = False) -> Path:
    bundle = bundle.absolute()
    output = output.absolute()
    if not is_replaceable_stillus_bundle(bundle):
        raise PackageError(f"input must be a packaged Stillus app: {bundle}")
    bundle = bundle.resolve()
    output = output.parent.resolve() / output.name
    if output.suffix != ".dmg":
        raise PackageError(f"output must end in .dmg: {output}")
    if output.is_relative_to(bundle):
        raise PackageError("DMG output must be outside the input bundle")

    def check_output() -> None:
        if output.is_symlink() or (output.exists() and not output.is_file()):
            raise PackageError(f"refusing to replace non-regular output: {output}")
        if output.exists() and not replace_existing:
            raise PackageError(f"refusing to overwrite existing output: {output}")

    check_output()
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".stillus-dmg-", dir=output.parent) as temp:
        staging = Path(temp) / "contents"
        staging.mkdir()
        shutil.copytree(bundle, staging / "Stillus.app", symlinks=True)
        (staging / "Applications").symlink_to("/Applications", target_is_directory=True)
        image = Path(temp) / "Stillus.dmg"
        try:
            subprocess.run(
                [
                    "/usr/bin/hdiutil", "create", "-volname", "Stillus",
                    "-srcfolder", str(staging), "-fs", "HFS+", "-format", "UDZO",
                    str(image),
                ],
                check=True,
            )
            subprocess.run(["/usr/bin/hdiutil", "verify", str(image)], check=True)
        except FileNotFoundError as error:
            raise PackageError("DMG packaging requires macOS /usr/bin/hdiutil") from error
        except subprocess.CalledProcessError as error:
            raise PackageError(
                f"DMG creation or verification failed with exit status {error.returncode}"
            ) from error
        check_output()
        if replace_existing:
            os.replace(image, output)
        else:
            # Publish without overwriting a file that appeared during packaging.
            os.link(image, output)
    return output


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--replace-existing", action="store_true")
    args = parser.parse_args()
    try:
        output = build_dmg(args.bundle, args.output, replace_existing=args.replace_existing)
    except (OSError, PackageError) as error:
        raise SystemExit(f"package-macos-dmg: {error}") from error
    print(f"PACKAGED_DMG path={output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
