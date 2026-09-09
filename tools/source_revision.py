#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Validate CI provenance against the actual checkout, including PR merge commits."""

import argparse
import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def validate_revision(value: str, actual: str) -> str:
    if not re.fullmatch(r"[0-9a-f]{40}", value):
        raise ValueError("SOURCE_REVISION must be a complete 40-character commit SHA")
    if value != actual:
        raise ValueError("SOURCE_REVISION does not match the checked-out HEAD")
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("revision")
    args = parser.parse_args()
    actual = subprocess.check_output(
        ["git", "-c", f"safe.directory={ROOT}", "rev-parse", "HEAD"], cwd=ROOT, text=True
    ).strip()
    print(validate_revision(args.revision, actual))


if __name__ == "__main__":
    main()
