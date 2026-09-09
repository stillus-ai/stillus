#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Native Linux checks for launch paths and the isolated fatal-error dialog."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

from generate_demo_data import generate_demo_workspace
from register_linux import register
from ui_ready import wait_for_first_paint

BINARY = Path("/var/cache/stillus/target/debug/stillus-app")


def wait_for(check, seconds=8, interval=0.05):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(interval)
    raise AssertionError("desktop condition timed out")


def windows(env, title):
    result = subprocess.run(["xdotool", "search", "--onlyvisible", "--name", title], env=env, text=True, capture_output=True)
    return result.stdout.split()


def persistence_smoke(root, env, scenario):
    workspace = root / "workspace"
    generate_demo_workspace(workspace)
    project = workspace / "notes/Project Alpha.md"
    before = project.read_bytes()
    marker = b"[stillus autosave smoke]"
    saved = workspace / "notes/[stillus autosave smoke].md"
    recovery = workspace / ".stillus/recovery"
    command = [str(BINARY), str(workspace)]
    if scenario in {"autosave", "recovery", "conflict"}:
        command.append("--smoke-autosave")
    elif scenario == "operations":
        command.append("--smoke-operations")
    if scenario == "conflict":
        command.extend(["--smoke-exit-ms", "2500"])
    process = subprocess.Popen(command, env=env)

    def close_ready_window():
        window = wait_for(lambda: windows(env, "^Stillus$")[0:1])[0]
        wait_for_first_paint(window, env)
        subprocess.run(["xdotool", "windowclose", window], env=env, check=True)
        assert process.wait(timeout=8) == 0

    try:
        if scenario in {"recovery", "conflict"}:
            # Observe durable recovery, not a fixed offset from process launch.
            # The renderer and worker can start at different speeds under load.
            wait_for(lambda: any(recovery.glob("*.nrrec")), interval=0.01)
            if scenario == "recovery":
                process.kill()
                process.wait(timeout=8)
                assert project.read_bytes() == before
                assert not saved.exists()
                process = subprocess.Popen(
                    [str(BINARY), str(workspace), "--smoke-restore"], env=env
                )
                wait_for(lambda: marker in project.read_bytes()
                         and not any(recovery.glob("*.nrrec")))
                close_ready_window()
            else:
                external = (workspace / "notes/Reading List.md").read_bytes()
                project.write_bytes(external)
                assert process.wait(timeout=15) == 0
                assert project.read_bytes() == external
                assert any(recovery.glob("*.nrrec"))
        elif scenario == "autosave":
            wait_for(lambda: saved.is_file() and marker in saved.read_bytes()
                     and not project.exists())
            close_ready_window()
        elif scenario == "operations":
            deleted = workspace / "notes/Smoke Renamed.md"
            wait_for(lambda: deleted.is_file() and b"deleted: true" in deleted.read_bytes())
            close_ready_window()
            contents = deleted.read_bytes()
            for expected in (b"title: 'Smoke Renamed'", b"  - 'Smoke'",
                             b"pinned: true", b"favorited: true", b"deleted: true"):
                assert expected in contents
            assert project.read_bytes() == before
            assert not (workspace / "notes/Smoke Note.md").exists()
            assert not (workspace / ".stillus/trash").exists()
            assert not any((workspace / "notes").rglob("*.stillus-tmp-*"))
        else:
            close_ready_window()
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=8)


def external(root, env):
    workspace = root / "workspace"
    generate_demo_workspace(workspace)
    first = root / "Заметка #1.MD"
    second = root / "two space.txt"
    for path in (first, second):
        path.write_text("External unchanged\n", encoding="utf-8")
    subprocess.run([str(BINARY), "--workspace", str(workspace), "--open", first.name, second.name,
                    "--smoke-exit-ms", "1400"], env=env, cwd=root, check=True, timeout=20)
    settings = workspace / ".stillus/settings.json"
    initial = json.loads(settings.read_text())
    assert [record["absolute_path"] for record in initial["external_files"]] == [str(first), str(second)]
    assert initial["selected_external"] == str(first)
    package = root / 'package $ % " 日本語'
    package.mkdir()
    shutil.copy2(BINARY, package / "stillus")
    shutil.copy2(Path(__file__).resolve().parent.parent / "app/stillus/assets/stillus-app-icon.svg", package / "stillus.svg")
    data = root / "data"
    register(package, data)
    desktop = data / "applications/org.stillus.Stillus.desktop"
    with (root / "desktop.log").open("w") as log:
        result = subprocess.run(["gio", "launch", str(desktop), str(second), str(first)],
                                env=env, stdout=log, stderr=log, timeout=15)
        if result.returncode:
            raise AssertionError((root / "desktop.log").read_text())
        window = wait_for(lambda: windows(env, "^Stillus$")[0:1])[0]
        try:
            wait_for_first_paint(window, env)
            wait_for(lambda: json.loads(settings.read_text())["selected_external"] == str(second))
        finally:
            subprocess.run(["xdotool", "windowclose", window], env=env, check=True)
            wait_for(lambda: not windows(env, "^Stillus$"))
    for path in (first, second):
        assert path.read_text() == "External unchanged\n"
    fresh_home = root / "first home"
    (fresh_home / "Downloads").mkdir(parents=True)
    fresh = dict(env, HOME=str(fresh_home))
    process = subprocess.Popen([str(BINARY), "--open", str(first), "--smoke-exit-ms", "4000"], env=fresh, cwd=root)
    try:
        startup_window = wait_for(lambda: windows(fresh, "^Stillus$"))[0]
        wait_for_first_paint(startup_window, fresh)
        default = fresh_home / "Downloads/Notes"
        assert not default.exists()
        subprocess.run(["xdotool", "mousemove", "785", "495", "click", "1"], env=fresh, check=True)
        def pending_opened():
            try:
                return json.loads((default / ".stillus/settings.json").read_text()).get("selected_external") == str(first)
            except FileNotFoundError:
                return False
        wait_for(pending_opened)
        assert process.wait(timeout=8) == 0
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
    concurrent(root, env)


def concurrent(root, env):
    workspace = root / "concurrent workspace"
    generate_demo_workspace(workspace)
    path = root / "concurrent.md"
    path.write_text("Original external body\n")
    processes = []

    def xd(*args):
        return subprocess.run(["xdotool", *args], env=env, text=True, capture_output=True)

    def window(process):
        found = xd("search", "--all", "--onlyvisible", "--pid", str(process.pid), "--name", "^Stillus$").stdout.split()
        return found[0] if found else None

    def edit(window_id, text):
        xd("windowraise", window_id, "windowfocus", "--sync", window_id).check_returncode()
        time.sleep(0.08)
        xd("mousemove", "--window", window_id, "500", "160", "click", "1").check_returncode()
        time.sleep(0.08)
        xd("key", "--clearmodifiers", "ctrl+End").check_returncode()
        time.sleep(0.08)
        xd("type", "--clearmodifiers", "--delay", "1", text).check_returncode()

    try:
        for _ in range(2):
            process = subprocess.Popen([str(BINARY), "--workspace", str(workspace), "--open", str(path)],
                                       env=env, cwd=root, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            processes.append(process)
            window_id = wait_for(lambda: window(process))
            wait_for_first_paint(window_id, env)
        first, second = map(window, processes)
        assert first != second and all(process.poll() is None for process in processes)
        edit(first, "FIRST_PROCESS_SAVED")
        edit(second, "SECOND_PROCESS_UNSAVED")
        wait_for(lambda: "FIRST_PROCESS_SAVED" in path.read_text())
        committed = path.read_bytes()
        time.sleep(2)
        assert b"SECOND_PROCESS_UNSAVED" not in committed
        # A competing recovery write is rejected while the first window owns
        # the artifact. A subsequent edit can persist after its checked save.
        edit(second, "_RECOVERY_RETRY")
        recovery = workspace / ".stillus/recovery"
        def retry_persisted():
            try:
                return any(b"SECOND_PROCESS_UNSAVED_RECOVERY_RETRY" in record.read_bytes()
                           for record in recovery.glob("*.nrrec"))
            except FileNotFoundError:
                # Another process can finish its checked cleanup between listing
                # and reading. Wait for the completed replacement, not any record.
                return False
        wait_for(retry_persisted)
        assert path.read_bytes() == committed, "stale window overwrote the saved file"
    finally:
        # Simulate independent crashes; never let a close prompt discard work.
        for process in processes:
            if process.poll() is None:
                process.kill()
            process.wait()



def crash(root, env):
    sentinel = "synthetic protected body must never reach diagnostics"
    with (root / "crash.stderr").open("w+") as log:
        process = subprocess.Popen([str(BINARY), "--smoke-panic"], env=env, cwd=root, stdout=log, stderr=log)
        try:
            window = wait_for(lambda: windows(env, "Stillus")[0:1])[0]
            wait_for_first_paint(window, env)
            subprocess.run(["xdotool", "windowfocus", "--sync", window, "key", "Tab", "Return"], env=env, check=True)
            def clipboard():
                result = subprocess.run(["xclip", "-selection", "clipboard", "-o"], env=env, capture_output=True, text=True, timeout=2)
                return result.stdout if "Backtrace:" in result.stdout else None
            copied = wait_for(clipboard)
            assert sentinel not in copied
            subprocess.run(["xdotool", "windowclose", window], env=env, check=True)
            assert process.wait(timeout=8) == 1
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
        log.seek(0)
        assert sentinel not in log.read()
    report = (root / "error.log").read_text()
    assert "Backtrace:" in report and sentinel not in report
    headless = dict(env, DISPLAY=":9876", WAYLAND_DISPLAY="stillus-missing")
    result = subprocess.run([str(BINARY), "--smoke-panic"], cwd=root, env=headless, capture_output=True, text=True, timeout=8)
    assert result.returncode == 1 and sentinel not in result.stderr


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("scenario", choices=[
        "external", "crash", "launch", "autosave", "recovery", "conflict", "operations",
    ])
    scenario = parser.parse_args().scenario
    with tempfile.TemporaryDirectory(prefix="stillus-desktop-") as temporary:
        root = Path(temporary)
        for name in ("home", "runtime"):
            (root / name).mkdir(mode=0o700)
        env = dict(os.environ, HOME=str(root / "home"), DISPLAY=":99", XDG_RUNTIME_DIR=str(root / "runtime"),
                   XDG_DATA_HOME=str(root / "data"), FLOEM_FORCE_TINY_SKIA="1", GDK_BACKEND="x11", GSK_RENDERER="cairo")
        server = subprocess.Popen(["Xvfb", ":99", "-screen", "0", "1240x800x24"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            wait_for(lambda: Path("/tmp/.X11-unix/X99").exists())
            if scenario in {"external", "crash"}:
                (external if scenario == "external" else crash)(root, env)
            else:
                persistence_smoke(root, env, scenario)
            print(f"DESKTOP_SMOKE {scenario}=passed")
        finally:
            server.terminate()
            server.wait(timeout=5)


if __name__ == "__main__":
    main()
