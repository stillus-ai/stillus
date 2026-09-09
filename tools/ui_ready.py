#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Wait for a native test window to paint before sending input or closing it."""

import subprocess
import time


def wait_for_first_paint(window_id: str, environment: dict[str, str]) -> None:
    # Xvfb maps Floem windows with black contents before its asynchronous renderer
    # is ready. The app paints a nonblack background at the top-left corner.
    # Read only that background pixel as a scalar, never a screenshot or note text;
    # this is also safe for protected-note scenarios and restored window sizes.
    deadline = time.monotonic() + 10.0
    while time.monotonic() < deadline:
        result = subprocess.run(
            [
                "import", "-display", environment["DISPLAY"], "-window", window_id,
                "-crop", "1x1+0+0", "+repage", "-colorspace", "Gray",
                "-format", "%[fx:mean]", "info:",
            ],
            env=environment,
            capture_output=True,
            text=True,
            check=True,
            timeout=2.0,
        )
        if float(result.stdout.strip()) > 0.02:
            return
        time.sleep(0.05)
    raise AssertionError("native window did not paint its background within 10 seconds")
