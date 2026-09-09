#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only

import contextlib
import ctypes
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
import io
import json
import os
from pathlib import Path
import re
import sys
import subprocess
import tarfile
import tempfile
from threading import Barrier
import unittest
from unittest.mock import Mock, patch
import zipfile

import ci
from ci_diagnostics import UI_SCENARIOS, rust_test_report
from source_revision import validate_revision
import ui_acceptance
import x11_close_window

SHA = "1234567890abcdef1234567890abcdef12345678"


class CITests(unittest.TestCase):
    def test_screenshot_export_creates_missing_parents(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "capture.png"
            source.write_bytes(b"screenshot\x00\xff")
            destination = root / "checkout" / "dist" / "chat-empty.png"
            ui_acceptance.export_screenshot(source, destination)
            self.assertEqual(destination.read_bytes(), source.read_bytes())

    def test_screenshot_export_reuses_directory_and_replaces_preview(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "capture.png"
            destination = root / "dist" / "chat-empty.png"
            destination.parent.mkdir()
            unrelated = destination.parent / "keep.txt"
            unrelated.write_bytes(b"keep")
            for content in (b"first screenshot", b"updated screenshot"):
                source.write_bytes(content)
                ui_acceptance.export_screenshot(source, destination)
                self.assertEqual(destination.read_bytes(), content)
                self.assertEqual(unrelated.read_bytes(), b"keep")

    def test_screenshot_exports_share_missing_directory_concurrently(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            sources = [root / name for name in ("chat.png", "journal.png")]
            for source in sources:
                source.write_bytes(source.name.encode())
            ready = Barrier(len(sources))

            def export(source):
                ready.wait(timeout=5)
                ui_acceptance.export_screenshot(source, root / "dist" / source.name)

            with ThreadPoolExecutor(max_workers=len(sources)) as workers:
                list(workers.map(export, sources))
            for source in sources:
                self.assertEqual((root / "dist" / source.name).read_bytes(), source.read_bytes())

    def test_screenshot_export_propagates_missing_source(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            destination = root / "dist" / "chat-empty.png"
            with self.assertRaises(FileNotFoundError):
                ui_acceptance.export_screenshot(root / "missing.png", destination)
            self.assertFalse(destination.exists())

    def test_screenshot_export_propagates_write_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "capture.png"
            source.write_bytes(b"screenshot")
            destination = root / "dist" / "chat-empty.png"
            # A directory at the target path makes the real write fail even as root.
            destination.mkdir(parents=True)
            with self.assertRaises(IsADirectoryError):
                ui_acceptance.export_screenshot(source, destination)

    def test_x11_close_delivers_protocol_without_destroying_window(self):
        # Own a real X11 window but do not run its event loop until after the
        # close request. The driver must leave it alive for the client to close.
        with tempfile.TemporaryFile() as display_output:
            server = subprocess.Popen(
                ["Xvfb", "-displayfd", "1", "-screen", "0", "320x240x24", "-nolisten", "tcp"],
                stdout=display_output, stderr=subprocess.PIPE,
            )
            try:
                def display_ready():
                    if server.poll() is not None:
                        self.fail("test Xvfb exited before becoming ready")
                    return os.pread(display_output.fileno(), 32, 0).strip()

                ui_acceptance.wait_until("test Xvfb ready", display_ready)
                environment = dict(os.environ, DISPLAY=":" + display_ready().decode())
                x11 = ctypes.CDLL("libX11.so.6")
                signatures = {
                    "XOpenDisplay": ([ctypes.c_char_p], ctypes.c_void_p),
                    "XDefaultRootWindow": ([ctypes.c_void_p], ctypes.c_ulong),
                    "XCreateSimpleWindow": ([ctypes.c_void_p, ctypes.c_ulong,
                        ctypes.c_int, ctypes.c_int, ctypes.c_uint, ctypes.c_uint,
                        ctypes.c_uint, ctypes.c_ulong, ctypes.c_ulong], ctypes.c_ulong),
                    "XInternAtom": ([ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int], ctypes.c_ulong),
                    "XSetWMProtocols": ([ctypes.c_void_p, ctypes.c_ulong,
                        ctypes.POINTER(ctypes.c_ulong), ctypes.c_int], ctypes.c_int),
                    "XSync": ([ctypes.c_void_p, ctypes.c_int], ctypes.c_int),
                    "XPending": ([ctypes.c_void_p], ctypes.c_int),
                    "XNextEvent": ([ctypes.c_void_p,
                        ctypes.POINTER(x11_close_window.XEvent)], ctypes.c_int),
                    "XCloseDisplay": ([ctypes.c_void_p], ctypes.c_int),
                }
                for name, (arguments, result) in signatures.items():
                    getattr(x11, name).argtypes = arguments
                    getattr(x11, name).restype = result
                display = x11.XOpenDisplay(environment["DISPLAY"].encode())
                self.assertTrue(display)
                try:
                    root = x11.XDefaultRootWindow(display)
                    window = x11.XCreateSimpleWindow(display, root, 0, 0, 100, 80, 0, 0, 0)
                    delete = ctypes.c_ulong(x11.XInternAtom(display, b"WM_DELETE_WINDOW", False))
                    self.assertTrue(x11.XSetWMProtocols(display, window, ctypes.byref(delete), 1))
                    protocols = x11.XInternAtom(display, b"WM_PROTOCOLS", True)
                    x11.XSync(display, False)

                    x11_close_window.request_window_close(str(window), environment)

                    ui_acceptance.wait_until("close protocol delivered", lambda: x11.XPending(display) > 0)
                    event = x11_close_window.XEvent()
                    x11.XNextEvent(display, ctypes.byref(event))
                    self.assertEqual(event.client.type, 33)
                    self.assertTrue(event.client.send_event)
                    self.assertEqual(event.client.window, window)
                    self.assertEqual(event.client.message_type, protocols)
                    self.assertEqual(event.client.format, 32)
                    self.assertEqual(list(event.client.data.l), [delete.value, 0, 0, 0, 0])
                    geometry = subprocess.run(
                        ["xdotool", "getwindowgeometry", "--shell", str(window)],
                        env=environment, check=True, capture_output=True, text=True,
                    ).stdout
                    self.assertIn("WIDTH=100\n", geometry)
                    self.assertIn("HEIGHT=80\n", geometry)
                finally:
                    x11.XCloseDisplay(display)
            finally:
                server.terminate()
                server.communicate(timeout=3)

    def test_x11_close_reports_connection_protocol_and_send_failures(self):
        for failure in ("connection", "protocol", "send"):
            with self.subTest(failure=failure):
                x11 = Mock()
                x11.XOpenDisplay.return_value = 0 if failure == "connection" else 1
                x11.XInternAtom.return_value = 0 if failure == "protocol" else 2
                x11.XSendEvent.return_value = 0
                with patch.object(x11_close_window.ctypes, "CDLL", return_value=x11):
                    with self.assertRaises(RuntimeError):
                        x11_close_window.request_window_close("123", {"DISPLAY": ":99"})
                if failure == "connection":
                    x11.XCloseDisplay.assert_not_called()
                else:
                    x11.XCloseDisplay.assert_called_once_with(1)
                if failure != "send":
                    x11.XSendEvent.assert_not_called()

    def test_acceptance_close_requires_clean_process_exit(self):
        for result in (0, 1, subprocess.TimeoutExpired("stillus", 3)):
            with self.subTest(result=result):
                driver = object.__new__(ui_acceptance.WindowDriver)
                process = Mock()
                process.wait.side_effect = [result, -15]
                driver.app = process
                driver.window_id = "123"
                driver.environment = {"DISPLAY": ":99"}
                with patch.object(ui_acceptance, "request_window_close") as request:
                    if result == 0:
                        driver.close_app()
                    else:
                        with self.assertRaises(ui_acceptance.AcceptanceFailure):
                            driver.close_app()
                    request.assert_called_once_with("123", driver.environment)
                self.assertIsNone(driver.app)
                self.assertIsNone(driver.window_id)
                if isinstance(result, subprocess.TimeoutExpired):
                    process.terminate.assert_called_once()
                else:
                    process.terminate.assert_not_called()

    def test_split_ci_gates_preserve_all_local_checks_without_duplicates(self):
        def commands(target):
            result = subprocess.run(
                ["make", "--dry-run", "--no-print-directory", target, "MAKE=:", "RUN=:", "GIT=:"],
                cwd=ci.ROOT, check=True, text=True, capture_output=True,
            )
            return Counter(result.stdout.splitlines())

        linux = commands("check-linux")
        ui = commands("ui-check")
        windows = commands("check-windows-build")
        self.assertEqual(commands("check"), linux + ui + windows)
        self.assertTrue(any("tools/ui_acceptance.py" in line for line in ui))
        self.assertTrue(any("tools/desktop_smoke.py" in line for line in ui))
        self.assertFalse(any("tools/ui_acceptance.py" in line or "tools/desktop_smoke.py" in line
                             for line in linux))
        self.assertFalse(any("cargo test" in line or "cargo clippy" in line or "cargo audit" in line
                             for line in ui))

    def test_ui_workflow_job_starts_independently_and_keeps_failure_reports(self):
        workflow = (ci.ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        jobs = dict(re.findall(r"^  ([\w-]+):\n(.*?)(?=^  [\w-]+:|\Z)",
                               workflow.split("\njobs:\n", 1)[1], re.MULTILINE | re.DOTALL))
        ui = jobs["ui"]
        self.assertNotRegex(ui, r"(?m)^    (needs|if|continue-on-error):")
        self.assertNotIn("actions/download-artifact@", ui)
        self.assertIn("run: make ci-ui\n", ui)
        self.assertIn('if: always()\n        run: python3 tools/ci.py finish linux "${{ job.status }}"', ui)
        self.assertRegex(ui, r"if: always\(\)\n        uses: actions/upload-artifact@[^\n]+\n"
                            r"        with:\n          name: reports-ui\n          path: .ci/reports/linux/")
        self.assertNotIn("make ci-ui", jobs["linux"])

    @staticmethod
    def editor_frame(*, caret_x=None, text_shift=0, overlay=False):
        width, height = 64, 32
        pixels = bytearray(b"\xff\xff\xff" * width * height)

        def fill(x, y, w, h, color):
            for row in range(y, y + h):
                for column in range(x, x + w):
                    offset = (row * width + column) * 3
                    pixels[offset:offset + 3] = bytes(color)

        fill(12 + text_shift, 8, 20, 12, (35, 39, 45))
        if caret_x is not None:
            fill(caret_x, 5, 2, 18, (54, 94, 130))
        if overlay:
            fill(4, 5, 28, 18, (54, 94, 130))
        return bytes(pixels)

    def test_editor_comparison_ignores_only_caret_blink(self):
        hidden = self.editor_frame()
        visible = self.editor_frame(caret_x=4)
        for first, second in ((hidden, hidden), (hidden, visible), (visible, hidden)):
            self.assertTrue(ui_acceptance.editor_pixels_equal_except_caret(first, second, 64))
        for changed in (self.editor_frame(text_shift=1), self.editor_frame(caret_x=5),
                        self.editor_frame(caret_x=4, text_shift=1),
                        self.editor_frame(overlay=True)):
            self.assertFalse(ui_acceptance.editor_pixels_equal_except_caret(visible, changed, 64))
        # A small accent-colored change is not a whole caret blinking.
        tiny = bytearray(hidden)
        tiny[0:3] = bytes((54, 94, 130))
        self.assertFalse(ui_acceptance.editor_pixels_equal_except_caret(hidden, bytes(tiny), 64))
        adjacent_glyph = bytearray(hidden)
        offset = (12 * 64 + 6) * 3
        adjacent_glyph[offset:offset + 3] = bytes((35, 39, 45))
        self.assertFalse(ui_acceptance.editor_pixels_equal_except_caret(bytes(adjacent_glyph), visible, 64))

    def test_search_editor_wait_handles_slow_alternating_blink_samples(self):
        for state in ("blink", "text_changes", "blank", "strict"):
            with self.subTest(state=state):
                clock = [0.0]
                frames = [0]
                driver = object.__new__(ui_acceptance.WindowDriver)

                def capture(_name):
                    # On a slow runner PNG/compare work can sample a different
                    # 530ms caret phase on every iteration, indefinitely.
                    clock[0] += 0.53
                    frames[0] += 1
                    return Mock(pixels=self.editor_frame(
                        caret_x=4 if frames[0] % 2 else None,
                        text_shift=frames[0] % 2 if state == "text_changes" else 0,
                    ))

                driver.capture = Mock(side_effect=capture)
                with patch.object(ui_acceptance.time, "monotonic", side_effect=lambda: clock[0]), \
                        patch.object(ui_acceptance.time, "sleep"), \
                        patch.object(ui_acceptance, "dark_pixel_count", return_value=0 if state == "blank" else 240), \
                        patch.object(ui_acceptance, "mean_luminance", return_value=0.9), \
                        patch.object(ui_acceptance, "image_difference", side_effect=
                                     lambda a, b, **_kwargs: int(a.pixels != b.pixels)), \
                        patch.object(ui_acceptance, "editor_frames_equal_except_caret", side_effect=
                                     lambda a, b, _crop: ui_acceptance.editor_pixels_equal_except_caret(a.pixels, b.pixels, 64)):
                    kwargs = dict(crop=ui_acceptance.EDITOR_CROP, minimum_dark_pixels=100,
                                  stable_for=0.15, timeout=10, ignore_editor_caret=state != "strict")
                    if state == "blink":
                        driver.wait_for_stable_frame("editor", **kwargs)
                        self.assertLess(clock[0], 3)
                    else:
                        with self.assertRaises(ui_acceptance.AcceptanceFailure):
                            driver.wait_for_stable_frame("editor", **kwargs)
                        self.assertGreaterEqual(clock[0], 10)

    def test_rss_toolbar_wait_requires_continuous_closed_card_alignment(self):
        for state in ("closed", "delayed", "reopened", "open", "unstable"):
            with self.subTest(state=state):
                clock = [0.0]
                driver = Mock(spec=ui_acceptance.WindowDriver)

                def advance(seconds):
                    clock[0] += seconds

                def count(*_args, **_kwargs):
                    now = clock[0]
                    closed = (
                        state == "closed"
                        or state == "delayed" and now >= 0.6
                        or state == "reopened" and (now < 0.15 or now >= 0.6)
                        or state == "unstable" and int(now / 0.15) % 2 == 0
                    )
                    return 600 if closed else 0

                driver.window_color_pixel_count.side_effect = count
                with patch.object(ui_acceptance.time, "monotonic", side_effect=lambda: clock[0]), \
                        patch.object(ui_acceptance.time, "sleep", side_effect=advance):
                    if state in {"open", "unstable"}:
                        with self.assertRaises(ui_acceptance.AcceptanceFailure):
                            ui_acceptance.wait_for_rss_card_at_top(driver, stable_for=0.3)
                        self.assertGreaterEqual(clock[0], ui_acceptance.DEFAULT_TIMEOUT_SECONDS)
                        self.assertLess(clock[0], ui_acceptance.DEFAULT_TIMEOUT_SECONDS + 0.1)
                    else:
                        ui_acceptance.wait_for_rss_card_at_top(driver, stable_for=0.3)
                        earliest = 0.3 if state == "closed" else 0.9
                        self.assertGreaterEqual(clock[0], earliest)
                        self.assertLess(clock[0], earliest + 0.1)
                for call in driver.window_color_pixel_count.call_args_list:
                    self.assertEqual(call.args, ((54, 94, 130),))
                    self.assertEqual(call.kwargs, {"crop": (320, 75, 600, 3)})
                driver.key.assert_not_called()
                driver.click_point.assert_not_called()
                driver.wait_for_stable_frame.assert_not_called()
                driver.capture.assert_not_called()

    def test_rss_filter_text_wait_handles_delayed_focus_and_rejects_stale_or_wrong_text(self):
        expected = "Rust\nsecond line"
        for behavior in ("ready", "delayed", "unfocused", "wrong_field", "single_line"):
            with self.subTest(behavior=behavior):
                clock = [0.0]
                clipboard = [expected]
                driver = Mock(spec=ui_acceptance.WindowDriver)
                driver.environment = {}

                def advance(seconds):
                    clock[0] += seconds

                def seed(_environment, text):
                    clipboard[0] = text

                def key(command):
                    advance(0.08)
                    if command != "ctrl+c":
                        return
                    if behavior == "ready" or behavior == "delayed" and clock[0] >= 0.6:
                        clipboard[0] = expected
                    elif behavior == "wrong_field":
                        clipboard[0] = "Promotions"
                    elif behavior == "single_line":
                        clipboard[0] = expected.replace("\n", "")

                driver.key.side_effect = key
                with patch.object(ui_acceptance.time, "monotonic", side_effect=lambda: clock[0]), \
                        patch.object(ui_acceptance.time, "sleep", side_effect=advance), \
                        patch.object(ui_acceptance, "set_clipboard_text", side_effect=seed), \
                        patch.object(ui_acceptance, "clipboard_text", side_effect=lambda _env: clipboard[0]):
                    if behavior in {"ready", "delayed"}:
                        ui_acceptance.wait_for_rss_filter_text(driver, expected, "field focus")
                        self.assertLess(clock[0], 1.0)
                        if behavior == "delayed":
                            self.assertGreaterEqual(clock[0], 0.6)
                    else:
                        with self.assertRaises(ui_acceptance.AcceptanceFailure):
                            ui_acceptance.wait_for_rss_filter_text(driver, expected, "field focus")
                        self.assertGreaterEqual(clock[0], ui_acceptance.DEFAULT_TIMEOUT_SECONDS)
                        self.assertLess(clock[0], ui_acceptance.DEFAULT_TIMEOUT_SECONDS + 0.3)
                driver.click_point.assert_not_called()
                driver.type_text.assert_not_called()
                driver.capture.assert_not_called()
                driver.window_color_pixel_count.assert_not_called()

    def test_private_search_wait_never_captures_note_pixels_to_disk(self):
        for state in ("delayed", "empty", "changing"):
            with self.subTest(state=state):
                clock = [0.0]
                driver = Mock(spec=ui_acceptance.WindowDriver)

                def advance(seconds):
                    clock[0] += seconds

                def signature(_crop):
                    advance(0.05)
                    return str(clock[0]) if state == "changing" else "private fingerprint"

                driver.window_image_signature.side_effect = signature
                driver.window_color_pixel_count.side_effect = lambda *_args, **_kwargs: (
                    2_000 if state != "empty" and clock[0] >= 0.4 else 0
                )
                with patch.object(ui_acceptance.time, "monotonic", side_effect=lambda: clock[0]), \
                        patch.object(ui_acceptance.time, "sleep", side_effect=advance):
                    if state == "delayed":
                        ui_acceptance.wait_for_private_search_results(driver)
                        self.assertGreaterEqual(clock[0], 0.65)
                    else:
                        with self.assertRaises(ui_acceptance.AcceptanceFailure):
                            ui_acceptance.wait_for_private_search_results(driver)
                        self.assertGreaterEqual(clock[0], ui_acceptance.SEARCH_WAIT_SECONDS)
                driver.capture.assert_not_called()
                driver.type_sensitive_text.assert_not_called()
                for call in driver.window_image_signature.call_args_list:
                    self.assertEqual(call.args, (ui_acceptance.SEARCH_RESULTS_CROP,))

    def test_window_signature_uses_only_memory_and_rejects_unexpected_output(self):
        driver = object.__new__(ui_acceptance.WindowDriver)
        driver.window_id = "123"
        driver.environment = {"DISPLAY": ui_acceptance.DISPLAY}
        with patch.object(ui_acceptance, "run_command", return_value=Mock(stdout="a" * 64)) as command:
            self.assertEqual(driver.window_image_signature((12, 98, 232, 330)), "a" * 64)
            self.assertEqual(command.call_args.args[0], [
                "import", "-display", ui_acceptance.DISPLAY, "-window", "123",
                "-crop", "232x330+12+98", "+repage", "-format", "%#", "info:",
            ])
        with patch.object(ui_acceptance, "run_command", return_value=Mock(stdout="SYNTHETIC_SECRET")):
            with self.assertRaises(ui_acceptance.AcceptanceFailure) as caught:
                driver.window_image_signature((12, 98, 232, 330))
            self.assertNotIn("SYNTHETIC_SECRET", str(caught.exception))

    def test_search_wait_requires_painted_and_stable_results(self):
        for state in ("delayed", "empty", "changing"):
            with self.subTest(state=state):
                clock = [0.0]
                frames = []

                def advance(seconds):
                    clock[0] += seconds

                def capture(_name):
                    advance(0.05)
                    frame = Mock(painted=state != "empty" and clock[0] >= 0.4,
                                 content=len(frames) if state == "changing" else 1)
                    frames.append(frame)
                    return frame

                def colors(frame, color, **_kwargs):
                    if not frame.painted:
                        return 0
                    return 2_000 if color == (57, 66, 78) else 20

                driver = Mock(spec=ui_acceptance.WindowDriver)
                driver.capture.side_effect = capture
                with patch.object(ui_acceptance.time, "monotonic", side_effect=lambda: clock[0]), \
                        patch.object(ui_acceptance.time, "sleep", side_effect=advance), \
                        patch.object(ui_acceptance, "near_color_pixel_count", side_effect=colors), \
                        patch.object(ui_acceptance, "bright_pixel_count", return_value=100), \
                        patch.object(ui_acceptance, "image_difference", side_effect=
                                     lambda a, b, **_kwargs: int(a.content != b.content)) as difference:
                    if state == "delayed":
                        ui_acceptance.wait_for_search_results(driver)
                        self.assertGreaterEqual(clock[0], 0.65)
                    else:
                        with self.assertRaises(ui_acceptance.AcceptanceFailure):
                            ui_acceptance.wait_for_search_results(driver)
                        self.assertGreaterEqual(clock[0], ui_acceptance.SEARCH_WAIT_SECONDS)
                self.assertLess(clock[0], ui_acceptance.SEARCH_WAIT_SECONDS + 0.2)
                for call in difference.call_args_list:
                    self.assertEqual(call.kwargs["crop"], ui_acceptance.SEARCH_RESULTS_CROP)
                for frame in frames:
                    frame.unlink.assert_called_once_with(missing_ok=True)
                driver.type_text.assert_not_called()

    def test_search_edit_waits_for_close_and_expected_selection_without_retyping(self):
        for state in ("keyboard", "pointer", "open", "wrong_note", "unsaved"):
            with self.subTest(state=state), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                note = root / "notes" / "Expected.md"
                note.parent.mkdir()
                note.write_text("original body", encoding="utf-8")
                settings = root / ".stillus" / "settings.json"
                settings.parent.mkdir()
                settings.write_text(json.dumps({
                    "selected_note": "notes/Wrong.md" if state == "wrong_note"
                    else "notes/Expected.md",
                }), encoding="utf-8")
                clock = [0.0]

                def advance(seconds):
                    clock[0] += seconds

                driver = Mock(spec=ui_acceptance.WindowDriver)
                driver.window_color_pixel_count.side_effect = lambda *_args, **_kwargs: (
                    700 if state == "open" or clock[0] < 0.4 else 0
                )

                def type_marker(marker):
                    self.assertGreaterEqual(clock[0], 0.4)
                    driver.wait_for_stable_frame.assert_called_once()
                    self.assertTrue(driver.wait_for_stable_frame.call_args.kwargs["ignore_editor_caret"])
                    if state != "unsaved":
                        note.write_text("original body\n" + marker, encoding="utf-8")

                driver.type_text.side_effect = type_marker
                with patch.object(ui_acceptance.time, "monotonic", side_effect=lambda: clock[0]), \
                        patch.object(ui_acceptance.time, "sleep", side_effect=advance):
                    if state in {"keyboard", "pointer"}:
                        ui_acceptance.select_search_result_and_edit(
                            driver, root, note, "marker", click=state == "pointer"
                        )
                    else:
                        with self.assertRaises(ui_acceptance.AcceptanceFailure):
                            ui_acceptance.select_search_result_and_edit(driver, root, note, "marker")
                if state in {"open", "wrong_note"}:
                    driver.type_text.assert_not_called()
                    driver.click.assert_not_called()
                    driver.wait_for_stable_frame.assert_not_called()
                    self.assertEqual(note.read_text(encoding="utf-8"), "original body")
                else:
                    driver.type_text.assert_called_once_with("marker")
                if state == "pointer":
                    driver.click_point.assert_called_once_with(128, 124)
                    self.assertEqual(driver.key.call_count, 1)  # Newline in the editor only.
                self.assertLess(clock[0], 2 * ui_acceptance.SEARCH_WAIT_SECONDS + 0.2)

    def test_search_diagnostic_stages_preserve_only_known_context(self):
        for stage in ("initial/index", "query/results", "selection/open", "selection/save",
                      "external/index", "rebuild/index", "final/validation"):
            try:
                ui_acceptance.wait_until("SYNTHETIC_SECRET", lambda: False, timeout=0)
            except ui_acceptance.AcceptanceFailure as error:
                lines = ui_acceptance.failure_diagnostics("search", stage, error)
            self.assertIn(f"stage={stage}", lines[0])
            for line in lines:
                self.assertEqual(ci.safe_line(line), line)
                self.assertEqual(ci.safe_line(ci.safe_line(line)), line)
                self.assertNotIn("SYNTHETIC_SECRET", line)
                self.assertIsNone(ci.safe_line(line + " query=SYNTHETIC_SECRET"))
            self.assertIsNone(ci.safe_line(lines[0].replace("scenario=search", "scenario=visual")))
        with self.assertRaises(ValueError):
            ui_acceptance.failure_diagnostics("search", "SYNTHETIC_SECRET", RuntimeError())

    def test_updates_accent_button_uses_one_crop_and_distinguishes_absence(self):
        region = (10, 10, 100, 60)
        pixels = {(x, y): 255 for x in range(10, 110) for y in range(10, 70)}
        self.assertIsNone(ui_acceptance.find_accent_button(pixels, region))
        for x in range(24, 85):
            for y in range(30, 61):
                pixels[x, y] = 90
        # White text inside the filled surface must not move the click outside it.
        for x in range(40, 70):
            for y in range(40, 50):
                pixels[x, y] = 255
        with patch.object(ui_acceptance, "crop_luminances", return_value=pixels) as decode:
            self.assertEqual(ui_acceptance.accent_button(Path("frame"), region), (24, 84, 45))
        decode.assert_called_once_with(Path("frame"), region)

    def run_updates_restart_probe(self, mode):
        clock = [0.0]
        clicks = []
        hovered = [None]
        frames = []
        driver = Mock(spec=ui_acceptance.WindowDriver)
        driver.app = Mock()
        driver.window_id = "123"
        driver.scenario = "updates"
        driver.set_stage.side_effect = lambda stage: ui_acceptance.WindowDriver.set_stage(driver, stage)

        def capture(_name):
            clock[0] += 0.02
            if mode == "capture/error":
                raise ui_acceptance.AcceptanceFailure("capture failed")
            point = None if clock[0] < 0.3 or mode == "missing" else (
                (400, 500, 200) if clock[0] < 0.6 else (400, 500, 240)
            )
            reaction = bool(clicks) and mode in ("prompt", "settings", "hung", "nonzero")
            frame = Mock(button=point, prompt=reaction and mode != "settings",
                         settings=(point, reaction and mode == "settings",
                                   clock[0] if mode == "changing" else 0))
            frames.append(frame)
            return frame

        def command(*args):
            if args[0] == "mousemove":
                hovered[0] = (int(args[-2]), int(args[-1]))
            elif args == ("click", "1"):
                clicks.append((clock[0], hovered[0]))
            else:
                self.fail(f"unexpected command: {args}")

        def poll():
            if mode == "early/exit":
                return 0
            if len(clicks) >= (2 if mode == "lost/first" else 1) and mode in ("fast", "slow/decode", "lost/first"):
                return 0
            return None

        def wait(*, timeout):
            self.assertIn(mode, ("prompt", "settings", "hung", "nonzero"))
            if mode == "hung":
                clock[0] += timeout
                raise subprocess.TimeoutExpired(["SYNTHETIC_SECRET"], timeout)
            clock[0] += 0.2
            return 7 if mode == "nonzero" else 0

        def advance(seconds):
            clock[0] += seconds

        def decode(frame, _region):
            if mode == "slow/decode":
                clock[0] += 3.0
            return frame

        def difference(first, second, *, crop):
            attribute = "prompt" if crop == ui_acceptance.UPDATES_PROMPT_CROP else "settings"
            return 100 if getattr(first, attribute) != getattr(second, attribute) else 0

        driver.capture.side_effect = capture
        driver.xdotool.side_effect = command
        driver.app.poll.side_effect = poll
        driver.app.wait.side_effect = wait
        failure = None
        with patch.object(ui_acceptance.time, "monotonic", side_effect=lambda: clock[0]), \
                patch.object(ui_acceptance.time, "sleep", side_effect=advance), \
                patch.object(ui_acceptance, "crop_luminances", side_effect=decode), \
                patch.object(ui_acceptance, "find_accent_button", side_effect=lambda frame, _region: frame.button), \
                patch.object(ui_acceptance, "image_difference", side_effect=difference):
            try:
                ui_acceptance.restart_from_update_settings(driver)
            except (ui_acceptance.AcceptanceFailure, subprocess.TimeoutExpired) as error:
                failure = error
        for frame in frames:
            frame.unlink.assert_called_with(missing_ok=True)
        return driver, clicks, clock[0], failure

    def test_updates_restart_waits_for_stable_hovered_button(self):
        for mode in ("fast", "slow/decode"):
            with self.subTest(mode=mode):
                driver, clicks, _, failure = self.run_updates_restart_probe(mode)
                self.assertIsNone(failure)
                self.assertEqual(len(clicks), 1)
                self.assertEqual(clicks[0][1], (450, 240))
                self.assertGreaterEqual(clicks[0][0], 1.0)
                self.assertEqual(driver.stage, "restart/exit")
                driver.app.wait.assert_not_called()
        for mode in ("missing", "changing"):
            with self.subTest(mode=mode):
                driver, clicks, elapsed, failure = self.run_updates_restart_probe(mode)
                self.assertIsInstance(failure, ui_acceptance.AcceptanceFailure)
                self.assertEqual(clicks, [])
                self.assertEqual(driver.stage, "restart/settings")
                self.assertGreaterEqual(elapsed, 10)
                self.assertLess(elapsed, 10.1)

    def test_updates_restart_retries_only_before_reaction(self):
        for mode, count in (("lost/first", 2), ("prompt", 1), ("settings", 1)):
            with self.subTest(mode=mode):
                driver, clicks, _, failure = self.run_updates_restart_probe(mode)
                self.assertIsNone(failure)
                self.assertEqual(len(clicks), count)
                self.assertEqual(driver.stage, "restart/exit")
                if count == 2:
                    self.assertGreaterEqual(clicks[1][0] - clicks[0][0], 1.0)
                else:
                    self.assertLess(driver.app.wait.call_args.kwargs["timeout"], 20)

    def test_updates_restart_failures_keep_click_limit_and_deadline(self):
        for mode, count, stage in (("lost/all", 3, "restart/click"),
                                   ("hung", 1, "restart/exit"),
                                   ("nonzero", 1, "restart/exit"),
                                   ("early/exit", 0, "restart/settings"),
                                   ("capture/error", 0, "restart/settings")):
            with self.subTest(mode=mode):
                driver, clicks, elapsed, failure = self.run_updates_restart_probe(mode)
                self.assertIsNotNone(failure)
                self.assertEqual(len(clicks), count)
                self.assertEqual(driver.stage, stage)
                if mode in ("lost/all", "hung"):
                    self.assertGreaterEqual(elapsed - clicks[0][0], 20)
                    self.assertLess(elapsed - clicks[0][0], 20.1)

    def test_updates_diagnostic_stages_preserve_only_known_context(self):
        for stage in ("restart/settings", "restart/click", "restart/exit",
                      "restart/window", "restart/validation"):
            error = subprocess.TimeoutExpired(["SYNTHETIC_SECRET"], 20)
            lines = ui_acceptance.failure_diagnostics("updates", stage, error)
            self.assertIn(f"stage={stage}", lines[0])
            for line in lines:
                self.assertEqual(ci.safe_line(line), line)
                self.assertEqual(ci.safe_line(ci.safe_line(line)), line)
                self.assertNotIn("SYNTHETIC_SECRET", line)
                self.assertIsNone(ci.safe_line(line + " detail=SYNTHETIC_SECRET"))
                self.assertIsNone(ci.safe_line(line.replace("scenario=updates", "scenario=ai")))
        with self.assertRaises(ValueError):
            ui_acceptance.failure_diagnostics("updates", "SYNTHETIC_SECRET", RuntimeError())

    def test_protection_waits_for_delayed_worker_but_requires_original_path(self):
        for result in ("delayed", "missing", "wrong_path"):
            with self.subTest(result=result), tempfile.TemporaryDirectory() as directory:
                workspace = Path(directory)
                note = workspace / "note.md"
                note.write_text("synthetic body", encoding="utf-8")
                clock = [0.0]

                def advance(seconds):
                    clock[0] += seconds

                def protected(_workspace):
                    if result == "missing" or clock[0] < 12.0:
                        return []
                    return [note if result == "delayed" else workspace / "other.md"]

                driver = Mock(spec=ui_acceptance.WindowDriver)
                with patch.object(ui_acceptance.time, "monotonic", side_effect=lambda: clock[0]), \
                        patch.object(ui_acceptance.time, "sleep", side_effect=advance), \
                        patch.object(ui_acceptance, "protected_note_files", side_effect=protected):
                    if result == "delayed":
                        self.assertEqual(
                            ui_acceptance.protect_selected_note(driver, workspace, note, "fixture"),
                            note,
                        )
                    else:
                        with self.assertRaises(ui_acceptance.AcceptanceFailure):
                            ui_acceptance.protect_selected_note(driver, workspace, note, "fixture")
                self.assertGreaterEqual(clock[0], 12.0)
                self.assertLess(clock[0], 60.0)

    def test_password_focus_waits_for_accent_border_without_screenshot(self):
        driver = object.__new__(ui_acceptance.WindowDriver)
        driver.window_id = "123"
        driver.environment = {"DISPLAY": ui_acceptance.DISPLAY}
        divider = "20: (214,219,225) #D6DBE1 srgb(214,219,225)\n"
        accent = "20: (54,94,130) #365E82 srgb(54,94,130)\n"
        with patch.object(ui_acceptance, "run_command", side_effect=[
            Mock(stdout=divider), Mock(stdout=divider), Mock(stdout=accent),
        ]) as command, patch.object(ui_acceptance.time, "sleep"), \
                patch.object(driver, "capture") as capture:
            driver.wait_for_password_field_focus("password_confirmation")
        self.assertEqual(command.call_count, 3)
        for call in command.call_args_list:
            arguments = call.args[0]
            self.assertEqual(arguments[0], "import")
            self.assertEqual(arguments[arguments.index("-crop") + 1], "4x20+444+438")
            self.assertEqual(arguments[-1], "histogram:info:-")
        capture.assert_not_called()

    def test_ai_control_wait_accepts_blinking_caret_but_rejects_changing_content(self):
        for changing_content in (False, True):
            with self.subTest(changing_content=changing_content):
                clock = [0.0]
                frames = [0]

                def capture(_name):
                    clock[0] += 0.05
                    frames[0] += 1
                    return Mock(caret=int(clock[0] / 0.5) % 2,
                                content=frames[0] if changing_content else 0)

                def advance(seconds):
                    clock[0] += seconds

                def difference(previous, current, **_kwargs):
                    return int((previous.caret, previous.content) !=
                               (current.caret, current.content))

                driver = object.__new__(ui_acceptance.WindowDriver)
                driver.capture = capture
                with patch.object(ui_acceptance.time, "monotonic", side_effect=lambda: clock[0]), \
                        patch.object(ui_acceptance.time, "sleep", side_effect=advance), \
                        patch.object(ui_acceptance, "image_difference", side_effect=difference), \
                        patch.object(ui_acceptance, "dark_pixel_count", return_value=100), \
                        patch.object(ui_acceptance, "mean_luminance", return_value=0.8):
                    if changing_content:
                        with self.assertRaises(ui_acceptance.AcceptanceFailure):
                            ui_acceptance.wait_for_ai_controls(driver)
                    else:
                        ui_acceptance.wait_for_ai_controls(driver)
                        self.assertGreaterEqual(frames[0], 3)
                        self.assertLess(clock[0], 0.5)

    def test_replace_retry_diagnostics_reject_invalid_attempts_and_payloads(self):
        accepted = [
            f"NATIVE_REPLACE_RETRY thread=ThreadId(7) attempt={attempt} delay_ms={delay} os_error={code}"
            for attempt, delay in enumerate((10, 20, 40, 80), 1)
            for code in (5, 32)
        ] + [
            "NATIVE_POST_REPLACE thread=ThreadId(7) stage=ReplaceReportedFailure kind=PermissionDenied os_error=5",
            "NATIVE_VERSION site=RewriteRetryTarget identity_equal=true size_equal=false modified_equal=false changed_equal=false digest_equal=Unavailable",
        ]
        rejected = [
            accepted[0].replace("attempt=1", "attempt=0"),
            accepted[0].replace("attempt=1", "attempt=5"),
            accepted[0].replace("delay_ms=10", "delay_ms=80"),
            accepted[0].replace("os_error=5", "os_error=87"),
            accepted[0].replace("ThreadId(7)", "ThreadId(18446744073709551616)"),
            accepted[0].replace("ThreadId(7)", "ThreadId(0)"),
        ]
        for line in accepted:
            self.assertEqual(ci.safe_line(ci.safe_line(line)), line)
            for secret in ("SYNTHETIC_SECRET", r"C:\private\note.md", "body (os error 5)"):
                rejected.extend([line + " " + secret, line.replace("=", "=" + secret, 1)])
        for line in rejected:
            with self.subTest(line=line):
                self.assertIsNone(ci.safe_line(line))
        self.assertEqual(rust_test_report(accepted + rejected)["diagnostics"], accepted)

    def test_directory_sync_retries_expose_only_bounded_fixed_fields(self):
        prefix = "NATIVE_DIRECTORY_SYNC_RETRY thread=ThreadId(7)"
        accepted = [
            f"{prefix} stage={stage} attempt={attempt} delay_ms={delay} os_error=32 ownership={owner}"
            for stage in ("Publish", "Remove")
            for attempt, delay in [(i, 10 * 2**(i - 1)) for i in range(1, 7)] + [(7, 0)]
            for owner in ("Owned", "Missing", "Changed", "Unavailable")
        ] + [f"{prefix} stage=Publish attempt=2 delay_ms=0 os_error=32 ownership=Changed"]
        first = accepted[0]
        rejected = [first.replace(old, new) for old, new in (
            ("attempt=1", "attempt=0"), ("attempt=1", "attempt=8"),
            ("delay_ms=10", "delay_ms=320"), ("os_error=32", "os_error=5"),
            ("stage=Publish", "stage=Save"), ("ownership=Owned", "ownership=Unknown"),
            ("ThreadId(7)", "ThreadId(0)"),
            ("ThreadId(7)", "ThreadId(18446744073709551616)"),
        )]
        secrets = ["SYNTHETIC_SECRET", r"C:\private\note.md", "body (os error 32)"]
        for line in accepted:
            self.assertEqual(ci.safe_line(ci.safe_line(line)), line)
            for secret in secrets:
                rejected.extend([line + " " + secret, line.replace("=", "=" + secret, 1)])
        for line in rejected:
            self.assertIsNone(ci.safe_line(line))
        report = rust_test_report(accepted + rejected)
        self.assertEqual(report["diagnostics"], accepted)
        for secret in secrets:
            self.assertNotIn(secret, json.dumps(report))

    def test_post_replace_diagnostics_keep_stage_thread_and_os_code_only(self):
        accepted = [
            f"NATIVE_POST_REPLACE thread=ThreadId(7) stage={stage} kind=PermissionDenied os_error=32"
            for stage in ("CommittedInspect", "ParentCheckpoint", "ParentSync")
        ] + [
            "NATIVE_POST_REPLACE thread=ThreadId(7) stage=CommittedIdentity kind=IdentityMismatch os_error=0",
        ] + [
            f"NATIVE_DIRECTORY_SYNC thread=ThreadId(7) stage={stage} kind=PermissionDenied os_error=5"
            for stage in ("Validate", "Create", "FileSync", "Publish", "Remove", "Cleanup", "Exhausted")
        ]
        rejected = [
            accepted[0].replace("ThreadId(7)", "ThreadId(0)"),
            accepted[0].replace("ThreadId(7)", "ThreadId(18446744073709551616)"),
            accepted[0].replace("ThreadId(7)", "worker"),
            accepted[0].replace("CommittedInspect", "Unknown"),
            accepted[0].replace("CommittedInspect", "Publish"),
            accepted[0].replace("PermissionDenied", "IdentityMismatch"),
            accepted[0].replace("os_error=32", "os_error=2147483648"),
            accepted[0].replace("os_error=32", "os_error=-2147483649"),
            accepted[3].replace("os_error=0", "os_error=32"),
            accepted[3].replace("IdentityMismatch", "Other"),
            accepted[4].replace("Validate", "ParentSync"),
            accepted[4].replace("PermissionDenied", "IdentityMismatch"),
        ]
        secrets = ["SYNTHETIC_SECRET", r"C:\Users\secret\note.md", "body (os error 5)"]
        for line in accepted:
            self.assertEqual(ci.safe_line(ci.safe_line(line)), line)
            for secret in secrets:
                rejected.extend([line + " " + secret, line.replace("=", "=" + secret, 1)])
        for line in rejected:
            with self.subTest(line=line):
                self.assertIsNone(ci.safe_line(line))
        report = rust_test_report(accepted + rejected)
        self.assertEqual(report["diagnostics"], accepted)
        self.assertEqual(rust_test_report(report["diagnostics"]), report)
        for secret in secrets:
            self.assertNotIn(secret, json.dumps(report))

    def test_native_diagnostics_are_strict_private_and_idempotent(self):
        accepted = [
            "NATIVE_IO operation=Lock stage=Create kind=PermissionDenied os_error=5",
            "NATIVE_IO operation=Lock stage=Acquire kind=Other os_error=0",
            "NATIVE_IO operation=Metadata stage=Hash kind=UnexpectedEof os_error=0",
            "NATIVE_IO operation=Replace stage=Publish kind=PermissionDenied os_error=32",
            "NATIVE_IO operation=Cleanup stage=Remove kind=PermissionDenied os_error=5",
            "NATIVE_SAVE stage=ConflictCheck outcome=PreCommit",
            "NATIVE_OPERATION stage=SourceRemove outcome=Failed",
            "NATIVE_CLEANUP outcome=Removed",
            "NATIVE_CLEANUP outcome=Failed",
            "NATIVE_TEMP kind=Secure count=2",
            "NATIVE_RESULT operation=ExternalSave outcome=Conflict",
            "NATIVE_ASSERT operation=ConcurrentLock success=false",
            "NATIVE_DELETE stage=RecoveryRemove outcome=Failed error=Recovery/Io error_stage=None",
            "NATIVE_DELETE stage=RewriteMetadata outcome=Failed error=Save/Conflict error_stage=None",
            "NATIVE_DELETE stage=RewriteMetadata outcome=Failed error=Save/PreCommit error_stage=OpenTarget",
            "NATIVE_DELETE stage=SecureFinish outcome=Failed error=Operation/Save/PreCommit error_stage=Replace",
            "NATIVE_DELETE stage=RefreshOpen outcome=Success error=None error_stage=None",
            "NATIVE_DELETE_TEST round=1 deletion=1 phase=Begin",
            "NATIVE_DELETE_TEST round=32 deletion=2 phase=End",
            "NATIVE_VERSION site=MetadataOpened identity_equal=true size_equal=true modified_equal=false changed_equal=false digest_equal=true",
            "NATIVE_VERSION site=RewriteBeforeReplace identity_equal=false size_equal=true modified_equal=true changed_equal=false digest_equal=Unavailable",
            "NATIVE_PATH operation=ExternalSelection requested_verbatim=false stored_verbatim=true lexical_equal=false canonical_equal=true",
        ]
        secrets = ["SYNTHETIC_SECRET", r"C:\Users\secret\note.md", "body (os error 5)"]
        rejected = [
            "NATIVE_DELETE stage=Unknown outcome=Failed error=Recovery/Io error_stage=None",
            "NATIVE_DELETE stage=RecoveryRemove outcome=Failed error=Unknown error_stage=None",
            "NATIVE_DELETE stage=RefreshOpen outcome=Success error=Save/Conflict error_stage=None",
            "NATIVE_DELETE stage=RefreshOpen outcome=Failed error=None error_stage=None",
            "NATIVE_DELETE stage=RewriteMetadata outcome=Failed error=Save/Conflict error_stage=Replace",
            "NATIVE_DELETE stage=RewriteMetadata outcome=Failed error=Save/PreCommit error_stage=None",
            "NATIVE_DELETE_TEST round=33 deletion=2 phase=Begin",
            "NATIVE_DELETE_TEST round=0 deletion=2 phase=Begin",
            "NATIVE_DELETE_TEST round=1 deletion=3 phase=Begin",
            "NATIVE_VERSION site=Unknown identity_equal=true size_equal=true modified_equal=true changed_equal=true digest_equal=true",
            "NATIVE_VERSION site=OpenVersioned identity_equal=1 size_equal=true modified_equal=true changed_equal=true digest_equal=true",
            "NATIVE_IO operation=Lock stage=Hash kind=Other os_error=0",
            "NATIVE_IO operation=Replace stage=Publish kind=PermissionDenied os_error=2147483648",
            "NATIVE_IO operation=Replace stage=Publish kind=PermissionDenied os_error=-2147483649",
            "NATIVE_TEMP kind=Regular count=-1",
            "NATIVE_PATH operation=WorkspaceNote requested_verbatim=1 stored_verbatim=true lexical_equal=false canonical_equal=true",
        ]
        for line in accepted:
            self.assertEqual(ci.safe_line(ci.safe_line(line)), line)
            for secret in secrets:
                rejected.extend([line + " " + secret, line.replace("=", "=" + secret, 1)])
        for line in rejected:
            with self.subTest(line=line):
                self.assertIsNone(ci.safe_line(line))
        report = rust_test_report(accepted + rejected)
        self.assertEqual(report["diagnostics"], accepted)
        self.assertEqual(rust_test_report(report["diagnostics"]), report)
        for secret in secrets:
            self.assertNotIn(secret, json.dumps(report))

    def test_ui_diagnostics_keep_only_context_and_known_traceback_locations(self):
        self.assertEqual(UI_SCENARIOS, set(ui_acceptance.SCENARIOS))
        try:
            ui_acceptance.wait_until("SYNTHETIC_SECRET", lambda: False, timeout=0)
        except ui_acceptance.AcceptanceFailure as error:
            error.add_note("SYNTHETIC_SECRET")
            lines = ui_acceptance.failure_diagnostics("password_change", "rotate", error)
        self.assertEqual(lines[0], "UI_ACCEPTANCE_DIAGNOSTIC scenario=password_change "
                         "stage=rotate exception=AcceptanceFailure")
        self.assertEqual(len(lines), 2)
        self.assertRegex(lines[1], r" location=tools/ui_acceptance\.py:[1-9][0-9]*$")
        for line in lines:
            self.assertEqual(ci.safe_line(line), line)
            self.assertNotIn("SYNTHETIC_SECRET", line)
            self.assertNotIn(str(Path(__file__).parent), line)
            self.assertNotIn("test_ci.py", line)

    def test_ui_diagnostics_do_not_render_unknown_types_or_subprocess_payloads(self):
        class PrivateError(Exception):
            def __str__(self):
                raise AssertionError("exception payload must not be rendered")

        PrivateError.__name__ = "SYNTHETIC_SECRET"
        # A custom class cannot impersonate an allowlisted exception by name.
        disguised = type("ValueError", (Exception,), {})
        for error, kind in (
            (PrivateError(), "Exception"),
            (disguised("SYNTHETIC_SECRET"), "Exception"),
            (subprocess.CalledProcessError(1, ["SYNTHETIC_SECRET"],
                                          output="SYNTHETIC_SECRET"), "CalledProcessError"),
            (subprocess.TimeoutExpired(["SYNTHETIC_SECRET"], 1,
                                       stderr="SYNTHETIC_SECRET"), "TimeoutExpired"),
        ):
            with self.subTest(kind=kind):
                lines = ui_acceptance.failure_diagnostics("password_change", "rotate", error)
                self.assertEqual(lines, ["UI_ACCEPTANCE_DIAGNOSTIC scenario=password_change "
                                         f"stage=rotate exception={kind}"])
                self.assertEqual(ci.safe_line(lines[0]), lines[0])

    def test_ui_diagnostic_filter_rejects_unknown_fields_and_context(self):
        valid = ("UI_ACCEPTANCE_DIAGNOSTIC scenario=password_change stage=rotate "
                 "exception=AcceptanceFailure location=tools/ui_acceptance.py:123")
        for line in (
            valid.replace("password_change", "private_scenario"),
            valid.replace("stage=rotate", "stage=private_stage"),
            valid.replace("password_change", "ai"),
            valid.replace("AcceptanceFailure", "PrivateError"),
            valid.replace("tools/ui_acceptance.py", "tools/private.py"),
            valid.replace("tools/ui_acceptance.py", "/tmp/private/tools/ui_acceptance.py"),
            valid.replace(":123", ":0"),
            valid.replace(":123", ":-1"),
            valid + " secret=SYNTHETIC_SECRET",
            valid + " crates/private/src/lib.rs:12:3",
            valid + " (os error 5)",
            valid + "\nSYNTHETIC_SECRET",
        ):
            with self.subTest(line=line):
                self.assertIsNone(ci.safe_line(line))
        self.assertEqual(ci.safe_line(valid), valid)

    def run_ui_with_fake_driver(self, *, scenario_fails=True, secondary_failures=False,
                                cleanup_fails=False):
        driver = Mock(spec=ui_acceptance.WindowDriver)
        driver.scenario = "password_change"
        driver.stage = "startup"
        # Preserve the real stage validator while replacing construction below.
        set_stage = ui_acceptance.WindowDriver.set_stage
        driver.set_stage.side_effect = lambda stage: set_stage(driver, stage)
        output = io.StringIO()

        def scenario(current, workspace):
            current.set_stage("rotate")
            if scenario_fails:
                ui_acceptance.wait_until("SYNTHETIC_SECRET", lambda: False, timeout=0)

        def capture():
            self.assertIn("stage=rotate exception=AcceptanceFailure", output.getvalue())
            self.assertIn("location=tools/ui_acceptance.py:", output.getvalue())
            if secondary_failures:
                raise ValueError("SYNTHETIC_SECRET")
            return None

        driver.capture_failure.side_effect = capture
        if secondary_failures or cleanup_fails:
            driver.cleanup.side_effect = OSError("SYNTHETIC_SECRET")
        if secondary_failures:
            driver.sanitize_failure_logs.side_effect = PermissionError("SYNTHETIC_SECRET")
        with patch.object(ui_acceptance, "APP_BINARY", Path(__file__)), \
                patch.object(sys, "argv", ["ui_acceptance.py", "password_change"]), \
                patch.object(ui_acceptance, "WindowDriver", return_value=driver), \
                patch.object(ui_acceptance, "copy_demo", return_value=Path("unused")), \
                patch.dict(ui_acceptance.SCENARIOS, password_change=scenario), \
                contextlib.redirect_stderr(output), contextlib.redirect_stdout(output):
            code = ui_acceptance.main()
        self.assertNotIn("SYNTHETIC_SECRET", output.getvalue())
        return code, output.getvalue(), driver

    def test_ui_failure_survives_capture_cleanup_and_artifact_failures(self):
        code, output, driver = self.run_ui_with_fake_driver(secondary_failures=True)
        self.assertEqual(code, 1)
        self.assertEqual(output.count("UI_ACCEPTANCE_FAIL scenario=password_change"), 1)
        self.assertNotIn("UI_ACCEPTANCE_PASS", output)
        stages = ["stage=rotate", "stage=capture", "stage=cleanup", "stage=artifacts"]
        positions = [output.index(stage) for stage in stages]
        self.assertEqual(positions, sorted(positions))
        driver.cleanup.assert_called_once()
        driver.sanitize_failure_logs.assert_called_once()
        driver.remove_success_artifacts.assert_not_called()

    def test_ui_success_and_cleanup_failure_keep_exit_status(self):
        code, output, driver = self.run_ui_with_fake_driver(scenario_fails=False)
        self.assertEqual(code, 0)
        self.assertEqual(output, "UI_ACCEPTANCE_PASS scenario=password_change\n")
        driver.remove_success_artifacts.assert_called_once()
        driver.capture_failure.assert_not_called()
        code, output, driver = self.run_ui_with_fake_driver(scenario_fails=False,
                                                         cleanup_fails=True)
        self.assertEqual(code, 1)
        self.assertIn("stage=cleanup exception=OSError", output)
        self.assertNotIn("UI_ACCEPTANCE_PASS", output)
        driver.sanitize_failure_logs.assert_called_once()

    def test_ui_failure_reaches_ci_console_and_report_without_secrets(self):
        code, diagnostics, _ = self.run_ui_with_fake_driver()
        with tempfile.TemporaryDirectory() as temporary, patch.dict(os.environ, SOURCE_REVISION=SHA):
            root = Path(temporary)
            console = io.StringIO()
            with patch.object(ci, "ROOT", root), patch.object(ci, "REPORTS", root / "reports"), \
                    contextlib.redirect_stdout(console):
                result = ci.run("linux", [sys.executable, "-c",
                                         f"import sys; sys.stderr.write({diagnostics!r}); "
                                         f"print('SYNTHETIC_SECRET'); sys.exit({code})"])
            self.assertEqual(result, 1)
            report = json.loads((root / "reports/linux/status.json").read_text())
            self.assertEqual(report["status"], "failed")
            self.assertEqual(report["exit_code"], 1)
            log = (root / "reports/linux/checks.log").read_text()
            self.assertEqual(console.getvalue(), log)
            self.assertIn("stage=rotate exception=AcceptanceFailure", log)
            self.assertIn("location=tools/ui_acceptance.py:", log)
            for path in (root / "reports").rglob("*"):
                if path.is_file():
                    self.assertNotIn("SYNTHETIC_SECRET", path.read_text())
                    self.assertNotIn(str(root), path.read_text())
            self.assertNotIn("diagnostic directory:", log)

    def test_revision_requires_exact_checkout_sha(self):
        self.assertEqual(validate_revision(SHA, SHA), SHA)
        for value in ("", "master", SHA[:7], SHA + "-dirty", "f" * 40):
            with self.subTest(value=value), self.assertRaises(ValueError):
                validate_revision(value, SHA)

    def test_diagnostics_exclude_payloads_thread_names_and_document_data(self):
        secret = "synthetic password and protected editor body"
        for line in (secret, f"thread '{secret}' panicked", f"left: {secret}", f"error: {secret}"):
            self.assertNotIn(secret, ci.safe_line(line) or "")
        self.assertEqual(ci.safe_line(f"thread '{secret}' panicked at crates/core/src/lib.rs:12:3:"),
                         "Rust diagnostic location: crates/core/src/lib.rs:12:3")
        self.assertEqual(ci.safe_line(f"UI_ACCEPTANCE_FAIL scenario=secure: {secret}"),
                         "UI_ACCEPTANCE_FAIL scenario=secure")
        self.assertEqual(ci.safe_line("FAIL: test_icon (__main__.PackageMacosTests.test_icon)"),
                         "Python test FAIL: __main__.PackageMacosTests.test_icon")
        self.assertEqual(ci.safe_line('File "/workspace/tools/test_package_macos.py", line 111, in test_icon'),
                         "Python diagnostic location: tools/test_package_macos.py:111")
        self.assertEqual(ci.safe_line(f"AssertionError: {secret}"),
                         "Python exception: AssertionError (details omitted)")
        self.assertEqual(ci.safe_line("AssertionError"),
                         "Python exception: AssertionError (details omitted)")
        self.assertEqual(ci.safe_line(f"subprocess.CalledProcessError: {secret}"),
                         "Python exception: CalledProcessError (details omitted)")
        self.assertIsNone(ci.safe_line(f"FAIL: test_icon (__main__.PackageMacosTests.test_icon) {secret}"))
        self.assertEqual(ci.safe_line(f'PreCommit {{ stage: Replace, message: "{secret} (os error 5)" }}'),
                         "Rust failure: stage=Replace os_error=5")
        self.assertEqual(ci.safe_line(f'Os {{ code: 32, kind: PermissionDenied, message: "{secret}" }}'),
                         "Rust failure: os_error=32")
        self.assertEqual(ci.safe_line("Rust failure: stage=Replace os_error=5"),
                         "Rust failure: stage=Replace os_error=5")
        acl = "WINDOWS_ACL_MISMATCH expected_count=3 actual_count=3 index=0 expected_flags=16 actual_flags=0"
        self.assertEqual(ci.safe_line(acl), acl)
        self.assertIsNone(ci.safe_line(acl + " " + secret))
        self.assertIsNone(ci.safe_line(acl.replace("expected_flags=16", "expected_flags=" + secret)))

    def test_failed_command_retains_only_safe_report(self):
        with tempfile.TemporaryDirectory() as temporary, patch.dict(os.environ, SOURCE_REVISION=SHA):
            root = Path(temporary)
            console = io.StringIO()
            with patch.object(ci, "ROOT", root), patch.object(ci, "REPORTS", root / "reports"), \
                    contextlib.redirect_stdout(console):
                code = ci.run("linux", [sys.executable, "-c",
                                       "print('SYNTHETIC_SECRET'); print('test tests::example ... FAILED'); exit(7)"])
            self.assertEqual(code, 7)
            self.assertNotIn("SYNTHETIC_SECRET", console.getvalue())
            report = json.loads((root / "reports/linux/status.json").read_text())
            self.assertEqual(report["status"], "failed")
            self.assertEqual(report["source_revision"], SHA)
            for path in (root / "reports").rglob("*"):
                if path.is_file():
                    self.assertNotIn("SYNTHETIC_SECRET", path.read_text())

    def test_windows_rust_report_keeps_failures_and_locations_without_payloads(self):
        raw = "\n".join([
            "test tests::first ... ok",
            "test tests::second ... FAILED",
            "thread 'SYNTHETIC_SECRET' panicked at app/stillus/src/main.rs:123:4:",
            "assertion failed: SYNTHETIC_SECRET",
            "left: C:\\Users\\SYNTHETIC_SECRET\\note.md",
            "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s",
        ])
        report = rust_test_report(raw.splitlines())
        self.assertEqual(report["failedTests"], ["tests::second"])
        self.assertIn("Rust diagnostic location: app/stillus/src/main.rs:123:4", report["diagnostics"])
        self.assertIn("test tests::first ... ok", report["diagnostics"])
        self.assertNotIn("SYNTHETIC_SECRET", json.dumps(report))
        self.assertEqual(rust_test_report(["SYNTHETIC_SECRET"]),
                         {"failedTests": [], "diagnostics": []})
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "Windows 日本語 test.log"
            log.write_text(raw, encoding="utf-8-sig")
            result = subprocess.run([sys.executable, str(Path(ci.__file__).with_name("ci_diagnostics.py")),
                                     str(log)], check=True, text=True, capture_output=True)
            self.assertEqual(json.loads(result.stdout), report)
            self.assertNotIn("SYNTHETIC_SECRET", result.stdout + result.stderr)

    def test_windows_runner_diagnostics_preserve_only_bounded_known_fields(self):
        valid = [
            "NATIVE_RUNNER stage=rust reason=test/timeout duration_ms=600000",
            "NATIVE_RUNNER stage=state reason=state/timeout duration_ms=60000",
            "NATIVE_RUNNER stage=close reason=process/close duration_ms=30000",
            "NATIVE_RUNNER stage=close/request reason=process/close/rejected duration_ms=5100",
            "NATIVE_RUNNER stage=close/wait reason=process/exit/timeout duration_ms=31000",
            "NATIVE_RUNNER stage=close/wait reason=process/exit/code duration_ms=1100",
            "NATIVE_RUNNER stage=complete reason=none duration_ms=2100",
        ]
        for line in valid:
            self.assertEqual(ci.safe_line(line), line)
            self.assertEqual(ci.safe_line(ci.safe_line(line)), line)
        for line in [
            valid[0] + " path=SYNTHETIC_SECRET",
            valid[0].replace("rust", "SYNTHETIC_SECRET"),
            valid[0].replace("test/timeout", "SYNTHETIC_SECRET"),
            valid[0].replace("600000", "-1"),
            valid[0].replace("600000", "999999999999999"),
        ]:
            self.assertIsNone(ci.safe_line(line))

    def test_native_shutdown_diagnostics_keep_stages_and_process_state_without_payloads(self):
        stages = ("WindowClosed", "WindowSettingsFlushed", "WindowSettingsFailed",
                  "EventLoopExited", "FinalSettingsFlushed", "FinalSettingsFailed", "ShutdownComplete")
        records = [f"NATIVE_LIFECYCLE stage={stage}" for stage in stages]
        window = ("NATIVE_WINDOW scenario=startup process=running window=present responding=false "
                  "close_accepted=true close_attempts=1")
        records += [window, window.replace("present", "absent").replace("responding=false", "responding=unknown")]
        for line in records:
            self.assertEqual(ci.safe_line(line), line)
            self.assertEqual(ci.safe_line(ci.safe_line(line)), line)
            self.assertIsNone(ci.safe_line(line + " path=SYNTHETIC_SECRET"))
        for line in (
            "NATIVE_LIFECYCLE stage=SYNTHETIC_SECRET",
            "NATIVE_LIFECYCLE stage=WindowClosed os_error=32",
            window.replace("startup", "SYNTHETIC_SECRET"),
            window.replace("running", "SYNTHETIC_SECRET"),
            window.replace("present", "SYNTHETIC_SECRET"),
            window.replace("responding=false", "responding=SYNTHETIC_SECRET"),
            window.replace("close_accepted=true", "close_accepted=SYNTHETIC_SECRET"),
            window.replace("attempts=1", "attempts=-1"),
            window.replace("attempts=1", "attempts=9999999999999"),
        ):
            self.assertIsNone(ci.safe_line(line))
        report = rust_test_report(records + ["SYNTHETIC_SECRET"])
        self.assertEqual(report, {"failedTests": [], "diagnostics": records})

    def test_windows_separate_output_streams_keep_failure_locations(self):
        with tempfile.TemporaryDirectory() as temporary:
            stdout = Path(temporary) / "stdout.log"
            stderr = Path(temporary) / "stderr.log"
            stdout.write_text("test fixture::failed ... FAILED\n", encoding="utf-8")
            stderr.write_text("thread 'SYNTHETIC_SECRET' panicked at crates/stillus-update/src/archive.rs:1:2:\n"
                              "SYNTHETIC_SECRET\n", encoding="utf-8")
            result = subprocess.run(
                [sys.executable, str(Path(ci.__file__).with_name("ci_diagnostics.py")), str(stdout), str(stderr)],
                check=True, text=True, capture_output=True,
            )
            report = json.loads(result.stdout)
            self.assertEqual(report["failedTests"], ["fixture::failed"])
            self.assertIn("Rust diagnostic location: crates/stillus-update/src/archive.rs:1:2", report["diagnostics"])
            self.assertNotIn("SYNTHETIC_SECRET", result.stdout + result.stderr)

    def test_linux_archive_preserves_mode_and_excludes_unrelated_files(self):
        with tempfile.TemporaryDirectory() as temporary, patch.dict(os.environ, SOURCE_REVISION=SHA):
            root = Path(temporary)
            directory = root / "dist/linux/x86_64"
            directory.mkdir(parents=True)
            for name in ("stillus", "stillus.svg", "Register.py", "org.stillus.Stillus.desktop", "LICENSE.txt"):
                (directory / name).write_text("fixture")
            (directory / "stillus").chmod(0o755)
            (directory / "personal.md").write_text("must not be archived")
            with patch.object(ci, "ROOT", root), patch.object(ci, "ARTIFACTS", root / "artifacts"), \
                    patch("ci.platform.machine", return_value="x86_64"):
                ci.package("linux")
            with tarfile.open(root / "artifacts/linux/stillus-linux-x86_64.tar.gz") as archive:
                prefix = "stillus-linux-x86_64/"
                self.assertEqual(archive.getmember(prefix + "stillus").mode & 0o777, 0o755)
                self.assertEqual(archive.extractfile(prefix + "SOURCE_REVISION.txt").read().strip().decode(), SHA)
                self.assertFalse(any(name.endswith("personal.md") for name in archive.getnames()))

    def test_windows_transfer_verifies_revision_checksum_and_paths(self):
        with tempfile.TemporaryDirectory() as temporary, patch.dict(os.environ, SOURCE_REVISION=SHA):
            root = Path(temporary)
            directory = root / "dist/windows/x86_64"
            (directory / "tests").mkdir(parents=True)
            for name in ("Stillus.exe", "LICENSE.txt", "Register.ps1", "tests/test.exe", "tests/Run-Tests.ps1", "tests/ci_diagnostics.py", "tests/windows_test_support.ps1", "tests/test_windows_support.ps1"):
                (directory / name).write_text("fixture")
            for folder in (directory, directory / "tests"):
                (folder / "dependencies.json").write_text("{}")
            (directory / "tests/tests.json").write_text('["test.exe"]')
            with patch.object(ci, "ROOT", root), patch.object(ci, "ARTIFACTS", root / "artifacts"):
                ci.package("windows-tests")
            archive = root / "artifacts/windows-tests/stillus-windows-tests-x86_64.zip"
            with self.assertRaises(ValueError):
                ci.extract_windows(archive, root / "wrong", "f" * 40)
            ci.extract_windows(archive, root / "valid", SHA)
            self.assertTrue((root / "valid/tests/test.exe").is_file())
            tampered = root / "tampered.zip"
            with zipfile.ZipFile(archive) as original, zipfile.ZipFile(tampered, "w") as output:
                for name in original.namelist():
                    output.writestr(name, b"modified" if name == "Stillus.exe" else original.read(name))
            with self.assertRaises(ValueError):
                ci.extract_windows(tampered, root / "tampered", SHA)
            with zipfile.ZipFile(root / "unsafe.zip", "w") as output:
                output.writestr("../outside", "must not escape")
            with self.assertRaises(ValueError):
                ci.extract_windows(root / "unsafe.zip", root / "unsafe", SHA)
            self.assertFalse((root / "outside").exists())


if __name__ == "__main__":
    unittest.main()
