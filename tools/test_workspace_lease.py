#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Exercise OS lease ownership across normal exit and forced process death."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time


def main():
    build = subprocess.run(
        ["cargo", "test", "-p", "stillus-platform", "--lib", "--no-run", "--message-format=json"],
        check=True, capture_output=True, text=True,
    )
    executable = next(
        item["executable"] for line in build.stdout.splitlines()
        if (item := json.loads(line)).get("reason") == "compiler-artifact"
        and item.get("executable") and item["profile"]["test"]
    )
    command = [executable, "--exact", "workspace_lease::tests::process_probe", "--nocapture"]
    with tempfile.TemporaryDirectory(prefix="stillus-lease-process-") as directory:
        root = Path(directory)
        def env(mode):
            return dict(os.environ, STILLUS_LEASE_PROBE=str(root), STILLUS_LEASE_MODE=mode)
        for crash in (False, True):
            owner = subprocess.Popen(command, env=env("hold"))
            try:
                deadline = time.monotonic() + 10
                while not (root / "ready").exists():
                    if owner.poll() is not None or time.monotonic() > deadline:
                        raise AssertionError("lease owner did not become ready")
                    time.sleep(0.01)
                subprocess.run(command, env=env("busy"), check=True, timeout=10)
                if crash:
                    owner.kill()  # no Rust destructors run
                else:
                    (root / "release").touch()
                result = owner.wait(timeout=10)
                if not crash:
                    assert result == 0
                assert (root / ".stillus-session.lock").is_file()
                subprocess.run(command, env=env("free"), check=True, timeout=10)
            finally:
                if owner.poll() is None:
                    owner.kill()
                    owner.wait(timeout=10)
                (root / "ready").unlink(missing_ok=True)
                (root / "release").unlink(missing_ok=True)
    print("WORKSPACE_LEASE_PROCESSES_OK normal_exit=true forced_exit=true")


if __name__ == "__main__":
    main()
