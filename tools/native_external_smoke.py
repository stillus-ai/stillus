#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Native macOS smoke for Launch Services external-file delivery."""

from __future__ import annotations

import json
import os
import plistlib
import shutil
import subprocess
import tempfile
import time
from pathlib import Path


def run_scenario(*, cold_start: bool) -> None:
    project = Path(__file__).resolve().parent.parent
    bundle = project / "dist" / "Stillus.app"
    source_workspace = project / "examples" / "demo-workspace"
    if not bundle.is_dir():
        raise SystemExit(f"native external smoke: bundle is missing: {bundle}")

    with tempfile.TemporaryDirectory(prefix="stillus-native-external-") as temp:
        # Match the canonical paths persisted by Stillus (/var is an alias for
        # /private/var on macOS).
        root = Path(temp).resolve()
        smoke_bundle = root / "Stillus External Smoke.app"
        shutil.copytree(bundle, smoke_bundle)
        info_path = smoke_bundle / "Contents" / "Info.plist"
        with info_path.open("rb") as source:
            info = plistlib.load(source)
        info["CFBundleIdentifier"] = (
            f"org.stillus.Stillus.externalSmoke{os.getpid()}{time.time_ns()}"
        )
        info["CFBundleDisplayName"] = "Stillus External Smoke"
        with info_path.open("wb") as destination:
            plistlib.dump(info, destination, sort_keys=True)

        home = root / "home"
        home.mkdir()
        workspace = root / "workspace"
        shutil.copytree(source_workspace, workspace)
        external = root / "External Smoke.MD"
        second = root / "Заметка #2.markdown"
        contents = "# External smoke\n\nBefore.\n"
        for path in (external, second):
            path.write_text(contents, encoding="utf-8")

        launched_at = time.monotonic()
        subprocess.run(
            [
                "open",
                "-na",
                str(smoke_bundle),
                "--env",
                f"HOME={home}",
                *([str(external), str(second)] if cold_start else []),
                "--args",
                str(workspace),
                "--smoke-exit-ms",
                "9000",
            ],
            check=True,
        )
        if not cold_start:
            time.sleep(1.0)
            subprocess.run(
                ["open", "-a", str(smoke_bundle), str(external), str(second)], check=True
            )

        settings_path = workspace / ".stillus" / "settings.json"
        deadline = time.monotonic() + 7.0
        expected_entries = [
            {"engine_id": "markdown", "absolute_path": str(path)}
            for path in (external, second)
        ]
        reopened = False
        while time.monotonic() < deadline:
            try:
                settings = json.loads(settings_path.read_text(encoding="utf-8"))
            except (FileNotFoundError, json.JSONDecodeError):
                time.sleep(0.1)
                continue
            entries = settings.get("external_files", [])
            selected = second if reopened else external
            if settings.get("selected_external") == str(selected) and entries == expected_entries:
                if not reopened:
                    # A later delivery must drain independently and select the
                    # requested file without attaching duplicate entries.
                    subprocess.run(
                        ["open", "-a", str(smoke_bundle), str(second)], check=True
                    )
                    reopened = True
                    continue
                for path in (external, second):
                    if path.read_text(encoding="utf-8") != contents:
                        raise SystemExit(f"native external smoke: file was modified: {path}")
                print(
                    f"NATIVE_EXTERNAL_SMOKE_OK cold_start={cold_start} "
                    f"ordered_files=2 reopened={second}"
                )
                time.sleep(max(0.0, launched_at + 9.5 - time.monotonic()))
                return
            time.sleep(0.1)
        observed = settings_path.read_text(encoding="utf-8") if settings_path.exists() else "missing"
        raise SystemExit(
            "native external smoke: external file was not persisted in "
            f"{settings_path}; observed settings: {observed}"
        )


if __name__ == "__main__":
    run_scenario(cold_start=True)
    run_scenario(cold_start=False)
