#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only

"""Drive the real Floem window through XTEST-backed pointer and keyboard events."""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
from typing import Callable

from ci_diagnostics import UI_DIAGNOSTIC_FILES, ui_diagnostic_context_valid
from generate_demo_data import generate_demo_workspace
from ui_ready import wait_for_first_paint
from x11_close_window import request_window_close


DISPLAY = ":99"
SCREEN_WIDTH = 1_240
SCREEN_HEIGHT = 800
APP_BINARY = Path("/var/cache/stillus/target/debug/stillus-app")
ARTIFACT_ROOT = Path("/var/cache/stillus/target/ui-acceptance")
# The create button in the sidebar header.
TOOLTIP_POINTER = (190, 27)
EVENT_SETTLE_SECONDS = 0.08
DEFAULT_TIMEOUT_SECONDS = 6.0
AGE_PREFIX = b"age-encryption.org/v1\n"
ARMORED_AGE_PREFIX = b"-----BEGIN AGE ENCRYPTED FILE-----\n"
ENCRYPTION_MARKER = b"stillus_encryption: age-body-v1"
SIDEBAR_WIDTH = 256
# Short notes keep a 28px line-number gutter plus a 12px gap next to the
# 256px sidebar. The application expands the gutter at larger digit counts;
# pointer scenarios below use short documents unless stated otherwise.
EDITOR_TEXT_LEFT = 296
EDITOR_LINE_NUMBER_CROP = (SIDEBAR_WIDTH + 4, 76, 34, 180)
EDITOR_CROP = (SIDEBAR_WIDTH, 56, 1_240 - SIDEBAR_WIDTH, 712)
# A rendered one-line marker document paints roughly 800 dark pixels in the
# monospace editor font; half of that still proves the text is on screen.
EXTERNAL_MARKER_MIN_DARK_PIXELS = 400
CONTEXT_MENU_X_OFFSET = 80
CONTEXT_MENU_FIRST_ROW_Y_OFFSET = 21
CONTEXT_MENU_ROW_HEIGHT = 32
# Sidebar tree geometry. The tree starts below the 32px header row and lists
# `Избранное`, a 6px separator, every category, `Все` and finally
# `Корзина` as
# 34px group rows with 2px gaps; an expanded group lists its notes inline as
# 30px rows. Category helpers recursively project visible rows so virtual
# parents and independently expanded descendants share the same geometry model
# as the Floem application layer.
SIDEBAR_TREE_TOP = 54
# Category rows reserve a 24px sort action plus an 8px gap immediately before
# the count. Keep the scrollbar oracle on the actual right gutter instead of
# sampling that scoped icon/button surface.
SIDEBAR_SCROLLBAR_CROP = (238, 200, 6, 500)
SIDEBAR_SCROLL_CONTENT_CROP = (12, 110, 220, 620)
SIDEBAR_SCROLLBAR_HIDE_SECONDS = 0.5
GROUP_ROW_HEIGHT = 34
GROUP_ROW_PITCH = 36
NOTE_ROW_HEIGHT = 30
NOTE_ROW_PITCH = 32
SECTION_GAP = 8
DEMO_CATEGORIES = ("Personal", "Planning", "Reading", "Work")
DEMO_NOTE_COUNTS = {
    "all": 3,
    "favorites": 1,
    "Personal": 1,
    "Planning": 1,
    "Reading": 1,
    "Work": 2,
    "trash": 0,
}
SIDEBAR_ROW_X = 120
# Title glyphs of note rows start right of the 13px note icon at x=42.
NOTE_TITLE_X = 62
# Group rows carry a 13px chevron at x=20; note rows leave that column dark.
CHEVRON_X = 20
CHEVRON_THRESHOLD = 40


def category_path_segments(category: str) -> tuple[str, ...]:
    segments = tuple(category.split("/"))
    return (category,) if any(not segment for segment in segments) else segments


def sidebar_category_forest(
    categories: tuple[str, ...],
    category_order: tuple[str, ...] | None = None,
) -> tuple[dict[str, object], ...]:
    roots: dict[str, dict[str, object]] = {}
    for category in categories:
        path = ""
        children = roots
        for segment in category_path_segments(category):
            path = segment if not path else f"{path}/{segment}"
            node = children.setdefault(
                segment,
                {"path": path, "label": segment, "children": {}},
            )
            children = node["children"]  # type: ignore[assignment]

    order = {
        path: index for index, path in enumerate(category_order or ())
    }

    def finish(children: dict[str, dict[str, object]]) -> tuple[dict[str, object], ...]:
        nodes = []
        ordered = sorted(
            children.values(),
            key=lambda node: (
                str(node["path"]) in order,
                order.get(str(node["path"]), 0),
                str(node["label"]),
            ),
        )
        for node in ordered:
            nodes.append(
                {
                    "path": node["path"],
                    "label": node["label"],
                    "children": finish(node["children"]),  # type: ignore[arg-type]
                }
            )
        return tuple(nodes)

    return finish(roots)


def sidebar_group_paths(
    categories: tuple[str, ...],
    category_order: tuple[str, ...] | None = None,
) -> tuple[str, ...]:
    paths: list[str] = []

    def visit(nodes: tuple[dict[str, object], ...]) -> None:
        for node in nodes:
            paths.append(str(node["path"]))
            visit(node["children"])  # type: ignore[arg-type]

    visit(sidebar_category_forest(categories, category_order))
    return ("favorites", *paths, "all", "trash")


def sidebar_visible_rows(
    *,
    expanded: str = "all",
    expanded_groups: tuple[str, ...] | None = None,
    counts: dict[str, int] = DEMO_NOTE_COUNTS,
    direct_counts: dict[str, int] | None = None,
    categories: tuple[str, ...] = DEMO_CATEGORIES,
    category_order: tuple[str, ...] | None = None,
) -> tuple[tuple[str, str, int, int, int], ...]:
    """Return ``kind, group, note-index, depth, top`` for visible rows."""
    open_groups = set((expanded,) if expanded_groups is None else expanded_groups)
    exact_counts = counts if direct_counts is None else direct_counts
    rows: list[tuple[str, str, int, int, int]] = []
    y = SIDEBAR_TREE_TOP

    def add_notes(group: str, depth: int) -> None:
        nonlocal y
        for index in range(exact_counts.get(group, counts.get(group, 0))):
            rows.append(("note", group, index, depth, y))
            y += NOTE_ROW_PITCH

    def add_special(group: str) -> None:
        nonlocal y
        rows.append(("group", group, -1, 0, y))
        y += GROUP_ROW_PITCH
        if group in open_groups:
            add_notes(group, 0)

    def add_categories(nodes: tuple[dict[str, object], ...], depth: int) -> None:
        nonlocal y
        for node in nodes:
            path = str(node["path"])
            rows.append(("group", path, -1, depth, y))
            y += GROUP_ROW_PITCH
            if path in open_groups:
                add_categories(node["children"], depth + 1)  # type: ignore[arg-type]
                add_notes(path, depth)

    add_special("favorites")
    forest = sidebar_category_forest(categories, category_order)
    if forest:
        y += SECTION_GAP
    add_categories(forest, 0)
    add_special("all")
    add_special("trash")
    return tuple(rows)


def group_row_top(
    group: str,
    *,
    expanded: str = "all",
    expanded_groups: tuple[str, ...] | None = None,
    counts: dict[str, int] = DEMO_NOTE_COUNTS,
    direct_counts: dict[str, int] | None = None,
    categories: tuple[str, ...] = DEMO_CATEGORIES,
    category_order: tuple[str, ...] | None = None,
) -> int:
    """Top y of a group row for the supplied independently expanded groups."""
    for kind, current, _, _, top in sidebar_visible_rows(
        expanded=expanded,
        expanded_groups=expanded_groups,
        counts=counts,
        direct_counts=direct_counts,
        categories=categories,
        category_order=category_order,
    ):
        if kind == "group" and current == group:
            return top
    raise AcceptanceFailure(f"unknown sidebar group {group!r}")


def group_row_center(group: str, **layout: object) -> tuple[int, int]:
    return SIDEBAR_ROW_X, group_row_top(group, **layout) + GROUP_ROW_HEIGHT // 2


def note_row_top(index: int, *, expanded: str = "all", **layout: object) -> int:
    """Top y of the ``index``-th note listed under the expanded group."""
    for kind, group, note_index, _, top in sidebar_visible_rows(
        expanded=expanded, **layout
    ):
        if kind == "note" and group == expanded and note_index == index:
            return top
    raise AcceptanceFailure(f"unknown sidebar note {expanded!r}[{index}]")


def note_row_center(index: int, *, x: int = 150, **layout: object) -> tuple[int, int]:
    return x, note_row_top(index, **layout) + NOTE_ROW_HEIGHT // 2


def note_title_crop(
    index: int, *, depth: int = 0, **layout: object
) -> tuple[int, int, int, int]:
    return NOTE_TITLE_X + min(depth, 6) * 16, note_row_top(index, **layout) + 5, 100, 20


def chevron_crop(top: int, *, depth: int = 0) -> tuple[int, int, int, int]:
    return CHEVRON_X + min(depth, 6) * 16, top + 10, 13, 13


NOTE_TITLE_CROPS = tuple(note_title_crop(index) for index in range(3))

# Editor actions occupy the left edge of the editor header in their production
# order: find, tags, protection, pin, favorite and trash.
EDITOR_HEADER_PADDING = 20
EDITOR_ACTION_SIZE = 32
EDITOR_ACTION_GAP = 6
EDITOR_ACTION_GROUP_LEFT = SIDEBAR_WIDTH + EDITOR_HEADER_PADDING
EDITOR_ACTION_GROUP_WIDTH = 6 * EDITOR_ACTION_SIZE + 5 * EDITOR_ACTION_GAP


def editor_action_center(index: int) -> tuple[int, int]:
    return (
        EDITOR_ACTION_GROUP_LEFT
        + EDITOR_ACTION_SIZE // 2
        + index * (EDITOR_ACTION_SIZE + EDITOR_ACTION_GAP),
        28,
    )


# Tag popover geometry. The 280px card hangs 6px below the 32px tag action and
# shares its left edge. Rows are 32px with 2px gaps inside a 10px card
# padding, every divider keeps 8px on both sides, and an overflowing list
# paints its scrollbar inside the right padding instead of over the rows.
TAG_POPOVER_WIDTH = 280
TAG_POPOVER_LEFT = editor_action_center(1)[0] - EDITOR_ACTION_SIZE // 2
TAG_POPOVER_RIGHT = TAG_POPOVER_LEFT + TAG_POPOVER_WIDTH
TAG_POPOVER_TOP = 50
TAG_POPOVER_PADDING = 10
TAG_POPOVER_CONTENT_LEFT = TAG_POPOVER_LEFT + 1 + TAG_POPOVER_PADDING
TAG_POPOVER_CONTENT_RIGHT = TAG_POPOVER_RIGHT - 1 - TAG_POPOVER_PADDING
TAG_POPOVER_CONTENT_TOP = TAG_POPOVER_TOP + 1 + TAG_POPOVER_PADDING
TAG_POPOVER_ROW_HEIGHT = 32
TAG_POPOVER_ROW_PITCH = 34
TAG_POPOVER_SECTION_GAP = 8
# Eight full rows plus half of the ninth.
TAG_POPOVER_LIST_MAX_HEIGHT = 8 * TAG_POPOVER_ROW_PITCH + TAG_POPOVER_ROW_HEIGHT // 2
TAG_POPOVER_REMOVE_SIZE = 24
TAG_POPOVER_REMOVE_INSET = 5
# A column inside the footer input that never carries placeholder glyphs or
# the caret, used to measure the vertical footer geometry.
TAG_POPOVER_FOOTER_X = TAG_POPOVER_LEFT + 212
# Grey level that still counts the anti-aliased card border and dividers as a
# line while leaving the input surface (luminance 247) out.
TAG_POPOVER_LINE_LUMINANCE = 244.0
TAG_POPOVER_CROP = (TAG_POPOVER_LEFT - 6, 44, 292, 390)

# AI settings geometry. Every section of the page is a settings card at the
# page content edge (settings sidebar 232 plus the 44px page padding), so the
# clicks below are anchored on a card border instead of on absolute rows: only
# the offsets inside one card are fixed here.
AI_SIDEBAR_ITEM = (90, 203)
AI_CONTENT_LEFT = 232 + 44
AI_CARD_PADDING = 22
AI_CARD_CONTENT_LEFT = AI_CONTENT_LEFT + 1 + AI_CARD_PADDING
# A column inside a filled primary button that its centred label never covers.
AI_PRIMARY_PROBE_X = AI_CARD_CONTENT_LEFT + 3
# Card borders are the divider grey; the expanded profile card is accented.
AI_CARD_BORDER_LUMINANCE = 240.0
AI_ACCENT_LUMINANCE = 150.0
# Shorter runs in the border column are glyphs of a heading, not a card.
AI_CARD_MIN_HEIGHT = 24
AI_EXPANDED_MIN_HEIGHT = 80
# Rounded corners leave the first and last rows of a card without a border.
AI_CARD_CORNER = 7
AI_CANCEL_X = AI_CARD_CONTENT_LEFT + 32 + 8 + 16


def tag_row_top(index: int) -> int:
    """Top y of the ``index``-th assigned tag row of an unscrolled popover."""
    return TAG_POPOVER_CONTENT_TOP + index * TAG_POPOVER_ROW_PITCH


def tag_row_center(
    index: int, *, x: int = TAG_POPOVER_LEFT + 62
) -> tuple[int, int]:
    return x, tag_row_top(index) + TAG_POPOVER_ROW_HEIGHT // 2


def tag_section_top(assigned: int) -> int:
    """Top y of the first row below the divider that follows ``assigned`` rows."""
    rows_bottom = tag_row_top(assigned) - (TAG_POPOVER_ROW_PITCH - TAG_POPOVER_ROW_HEIGHT)
    return rows_bottom + TAG_POPOVER_SECTION_GAP + 1 + TAG_POPOVER_SECTION_GAP


def tag_suggestion_center(assigned: int, index: int) -> tuple[int, int]:
    top = tag_section_top(assigned) + index * TAG_POPOVER_ROW_PITCH
    return TAG_POPOVER_LEFT + 112, top + TAG_POPOVER_ROW_HEIGHT // 2


def tag_input_center(assigned: int) -> tuple[int, int]:
    """Center of the footer input while no suggestions are listed."""
    return (
        TAG_POPOVER_LEFT + 112,
        tag_section_top(assigned) + TAG_POPOVER_ROW_HEIGHT // 2,
    )


def tag_input_crop(assigned: int) -> tuple[int, int, int, int]:
    """Bounds of the footer input while no suggestions are listed."""
    _, center_y = tag_input_center(assigned)
    return (
        TAG_POPOVER_CONTENT_LEFT,
        center_y - TAG_POPOVER_ROW_HEIGHT // 2,
        TAG_POPOVER_CONTENT_RIGHT - TAG_POPOVER_CONTENT_LEFT,
        TAG_POPOVER_ROW_HEIGHT,
    )


def tag_remove_crop(index: int) -> tuple[int, int, int, int]:
    """Bounds of the removal cross button at the right end of a tag row."""
    x = TAG_POPOVER_CONTENT_RIGHT - TAG_POPOVER_REMOVE_INSET - TAG_POPOVER_REMOVE_SIZE
    y = tag_row_top(index) + (TAG_POPOVER_ROW_HEIGHT - TAG_POPOVER_REMOVE_SIZE) // 2
    return x, y, TAG_POPOVER_REMOVE_SIZE, TAG_POPOVER_REMOVE_SIZE


def tag_remove_center(index: int) -> tuple[int, int]:
    x, y, size, _ = tag_remove_crop(index)
    return x + size // 2, y + size // 2


# Coordinates are relative to the asserted 1240x800 Stillus client window.
# Sidebar rows are listed for the default state (`Все` expanded with
# the three demo notes); other tree states use the geometry helpers above.
# Creation popover geometry mirrors CREATE_POPOVER_* and RSS_FORM_* in
# app/stillus/src/main.rs. The card is wider than the gap between the sidebar
# edge and its trigger, so it sits at the clamped x=8 under the header; the
# RSS form is its tallest state.
CREATE_POPOVER_CROP = (8, 40, 248, 146)
# Interior of the primary RSS button: accent fill once the field holds an
# address, divider fill while the disabled button is waiting for one.
RSS_SUBMIT_CROP = (170, 150, 60, 20)
RSS_SUBMIT_ACCENT = (54, 94, 130)


CONTROLS = {
    "all_notes": group_row_center("all"),
    "trash_group": group_row_center("trash"),
    "favorite_notes": group_row_center("favorites"),
    "category_personal": group_row_center("Personal"),
    "category_work": group_row_center("Work"),
    "reading_note": note_row_center(1),
    "search": (28, 27),
    "settings": (228, 27),
    "create_menu": TOOLTIP_POINTER,
    "create_note": (116, 63),
    "open_file": (116, 97),
    "create_rss": (116, 131),
    "rss_back": (54, 161),
    "settings_path": (610, 418),
    "settings_apply": (475, 464),
    "settings_back": (36, 42),
    "settings_encryption": (100, 163),
    "startup_primary": (785, 495),
    "encryption_current": (520, 201),
    "encryption_new": (520, 250),
    "encryption_confirmation": (520, 299),
    "encryption_submit": (375, 349),
    # The transparent hit area occupies the final 8px inside the sidebar so
    # the adjacent editor cannot win hit testing at the shared boundary.
    "sidebar_resize_default": (254, 400),
    "sidebar_resize_mid": (330, 400),
    "sidebar_resize_wide": (418, 400),
    "sidebar_resize_min": (178, 400),
    "sidebar_resize_far_left": (20, 400),
    "sidebar_resize_far_right": (700, 400),
    "note_find": editor_action_center(0),
    "go_to_line_submit": (862, 414),
    "tag_manager": editor_action_center(1),
    "tag_assigned_first": tag_row_center(0),
    "tag_remove_first": tag_remove_center(0),
    "tag_input": tag_input_center(2),
    "tag_suggestion_first": tag_suggestion_center(2, 0),
    "tag_suggestion_after_three": tag_suggestion_center(3, 0),
    "editor": (EDITOR_TEXT_LEFT + 2, 90),
    "editor_line_1_start": (EDITOR_TEXT_LEFT, 90),
    "editor_line_1_col_5": (EDITOR_TEXT_LEFT + 42, 90),
    "editor_line_1_col_9": (EDITOR_TEXT_LEFT + 76, 90),
    "editor_line_1_col_14": (EDITOR_TEXT_LEFT + 118, 90),
    "editor_word_bravo": (EDITOR_TEXT_LEFT + 71, 90),
    "editor_word_charlie": (EDITOR_TEXT_LEFT + 130, 90),
    "editor_line_2_col_5": (EDITOR_TEXT_LEFT + 42, 111),
    "editor_line_2_col_9": (EDITOR_TEXT_LEFT + 76, 111),
    "editor_line_2_end": (EDITOR_TEXT_LEFT + 101, 111),
    "editor_line_3_col_5": (EDITOR_TEXT_LEFT + 42, 133),
    "editor_line_3_col_9": (EDITOR_TEXT_LEFT + 76, 133),
    "sidebar_blank": (128, 600),
    "sidebar_scroll": (128, 400),
    "editor_line_4_far_right": (760, 156),
    "editor_line_5_col_9": (EDITOR_TEXT_LEFT + 76, 180),
    "editor_line_5_far_right": (760, 180),
    "editor_below_document": (760, 390),
    "pin": editor_action_center(3),
    "favorite": editor_action_center(4),
    "trash": editor_action_center(5),
    "protection": editor_action_center(2),
    "protection_lock": (440, 70),
    "protection_disable": (440, 104),
    # The fixed 390px password card is centered in the 1240x800 client. Setup
    # includes warning/confirmation content and therefore places its primary
    # field lower than the compact unlock card.
    "password_setup_primary": (760, 402),
    "password_unlock_primary": (760, 405),
    "password_confirmation": (760, 448),
    "password_unlock_cancel": (620, 474),
    "password_unlock_submit": (735, 474),
    "footer_retry": (1_176, 784),
    "footer_action": (1_210, 784),
    "integrity_restore": (675, 448),
    "integrity_retry": (785, 448),
}


def password_input_crop(control: str) -> tuple[int, int, int, int]:
    """Interior of a password input, excluding its horizontal borders."""
    _, center_y = CONTROLS[control]
    return (425, center_y - 17, 390, 34)


PASSWORD_UNLOCK_FEEDBACK_CROP = (425, 428, 250, 22)
PASSWORD_UNLOCK_STATIC_EDGE_CROP = (812, 280, 4, 240)
PASSWORD_UNLOCK_CANCEL_CROP = (568, 455, 84, 32)
PASSWORD_UNLOCK_SUBMIT_CROP = (660, 455, 134, 32)
PROTECTION_ACTION_ICON_CROP = (
    editor_action_center(2)[0] - 9,
    editor_action_center(2)[1] - 9,
    18,
    18,
)


class AcceptanceFailure(RuntimeError):
    """A click-driven acceptance condition was not satisfied."""


def export_screenshot(source: Path, destination: Path) -> None:
    """Export a preview even in a clean checkout or parallel acceptance run."""
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)


def failure_diagnostics(scenario: str, stage: str, error: Exception) -> list[str]:
    """Read code locations only: never format a traceback, source line or payload."""
    if not ui_diagnostic_context_valid(scenario, stage):
        raise ValueError("invalid UI diagnostic context")
    known_types = (
        AcceptanceFailure, AssertionError, ValueError, TypeError, RuntimeError,
        OSError, FileNotFoundError, PermissionError, UnicodeError, UnicodeDecodeError,
        subprocess.CalledProcessError, subprocess.TimeoutExpired,
    )
    exception = type(error).__name__ if type(error) in known_types else "Exception"
    prefix = f"UI_ACCEPTANCE_DIAGNOSTIC scenario={scenario} stage={stage} exception={exception}"
    lines = [prefix]
    root = Path(__file__).resolve().parent.parent
    known_files = {str(root / name): name for name in UI_DIAGNOSTIC_FILES}
    frame = error.__traceback__
    while frame is not None:
        filename = known_files.get(frame.tb_frame.f_code.co_filename)
        if filename is not None:
            lines.append(f"{prefix} location={filename}:{frame.tb_lineno}")
        frame = frame.tb_next
    return lines


def run_command(
    arguments: list[str],
    *,
    environment: dict[str, str] | None = None,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    completed = subprocess.run(
        arguments,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
    )
    if check and completed.returncode != 0:
        rendered = " ".join(arguments)
        raise AcceptanceFailure(
            f"command failed ({completed.returncode}): {rendered}\n"
            f"stdout: {completed.stdout.strip()}\n"
            f"stderr: {completed.stderr.strip()}"
        )
    return completed


def wait_until(
    description: str,
    predicate: Callable[[], bool],
    *,
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
    interval: float = 0.03,
) -> None:
    deadline = time.monotonic() + timeout
    last_error: OSError | UnicodeError | None = None
    while time.monotonic() < deadline:
        try:
            if predicate():
                return
        except (OSError, UnicodeError) as error:
            last_error = error
        time.sleep(interval)
    detail = f"; last read error: {last_error}" if last_error else ""
    raise AcceptanceFailure(f"timed out waiting for {description}{detail}")


def read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def contains(path: Path, marker: str) -> bool:
    return path.is_file() and marker in read_text(path)


def recovery_files(workspace: Path) -> list[Path]:
    recovery = workspace / ".stillus" / "recovery"
    return sorted(recovery.glob("*.nrrec")) if recovery.is_dir() else []


def note_body(path: Path) -> str:
    text = read_text(path)
    if not text.startswith("---\n"):
        return text
    boundary = text.find("\n---\n", 4)
    if boundary == -1:
        return text
    return text[boundary + len("\n---\n") :].lstrip("\n")


def category_order_value(path: Path, category: str) -> int | None:
    match = re.search(
        rf"(?m)^  ['\"]?{re.escape(category)}['\"]?: (\d+)$",
        read_text(path),
    )
    return int(match.group(1)) if match else None


def clipboard_text(environment: dict[str, str]) -> str | None:
    completed = run_command(
        ["xclip", "-selection", "clipboard", "-out"],
        environment=environment,
        check=False,
    )
    return completed.stdout if completed.returncode == 0 else None


def set_clipboard_text(environment: dict[str, str], text: str) -> None:
    owner = subprocess.Popen(
        ["xclip", "-selection", "clipboard", "-in", "-loops", "1"],
        env=environment,
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    if owner.stdin is None:
        raise AcceptanceFailure("could not open X clipboard owner stdin")
    owner.stdin.write(text)
    owner.stdin.close()
    # xclip becomes the asynchronous X selection owner. It exits after the
    # single diagnostic read below, or earlier when a real UI Copy replaces it.
    time.sleep(EVENT_SETTLE_SECONDS)
    if owner.poll() not in (None, 0):
        stderr = owner.stderr.read().strip() if owner.stderr is not None else ""
        raise AcceptanceFailure(f"could not seed X clipboard: {stderr}")


def image_difference(
    first: Path,
    second: Path,
    *,
    crop: tuple[int, int, int, int] | None = None,
) -> int:
    compared_first = first
    compared_second = second
    generated: list[Path] = []
    if crop is not None:
        x, y, width, height = crop
        geometry = f"{width}x{height}+{x}+{y}"
        compared_first = first.with_name(f"{first.stem}-crop.png")
        compared_second = second.with_name(f"{second.stem}-crop.png")
        for source, destination in (
            (first, compared_first),
            (second, compared_second),
        ):
            run_command(
                ["convert", str(source), "-crop", geometry, "+repage", str(destination)]
            )
            generated.append(destination)
    completed = run_command(
        ["compare", "-metric", "AE", str(compared_first), str(compared_second), "null:"],
        check=False,
    )
    for path in generated:
        path.unlink(missing_ok=True)
    if completed.returncode not in (0, 1):
        raise AcceptanceFailure(
            f"ImageMagick compare failed: {completed.stderr.strip()}"
        )
    metric = completed.stderr.strip().splitlines()[-1]
    try:
        return int(float(metric))
    except ValueError as error:
        raise AcceptanceFailure(f"unexpected ImageMagick metric: {metric}") from error


def dark_pixel_count(
    image: Path, *, crop: tuple[int, int, int, int] | None = None
) -> int:
    arguments = ["convert", str(image)]
    if crop is not None:
        x, y, width, height = crop
        arguments.extend(["-crop", f"{width}x{height}+{x}+{y}", "+repage"])
    arguments.extend(
        [
            "-colorspace",
            "Gray",
            "-threshold",
            "90%",
            "-format",
            "%[fx:(1-mean)*w*h]",
            "info:",
        ]
    )
    completed = run_command(arguments)
    try:
        return int(float(completed.stdout.strip()))
    except ValueError as error:
        raise AcceptanceFailure(
            f"unexpected ImageMagick dark-pixel metric: {completed.stdout.strip()}"
        ) from error


def editor_pixels_equal_except_caret(first: bytes, second: bytes, width: int) -> bool:
    """Accept only an unchanged image or one 2px by 18px accent caret blinking.

    Allow a one-pixel rasterization margin. Text, selection overlays and moving
    carets remain changes; do not use a general changed-pixel allowance.
    """
    if width <= 0 or len(first) != len(second) or len(first) % (3 * width):
        raise AcceptanceFailure("invalid editor pixel geometry")
    if first == second:
        return True
    left, top, right, bottom = width, len(first) // (3 * width), -1, -1
    first_accent = second_accent = False
    changed = []
    for offset in range(0, len(first), 3):
        before, after = first[offset:offset + 3], second[offset:offset + 3]
        if before == after:
            continue
        y, x = divmod(offset // 3, width)
        left, right = min(left, x), max(right, x)
        top, bottom = min(top, y), max(bottom, y)
        if right - left >= 3 or bottom - top >= 20:
            return False
        changed.append((before, after))
        first_accent |= all(abs(value - color) <= 8 for value, color in zip(before, (54, 94, 130)))
        second_accent |= all(abs(value - color) <= 8 for value, color in zip(after, (54, 94, 130)))
    # Exactly one frame must contain the caret's solid accent color.
    if bottom - top < 15 or first_accent == second_accent:
        return False
    for before, after in changed:
        painted, background = (before, after) if first_accent else (after, before)
        # Antialiased edge pixels must also lie between the underlying pixel
        # and the caret color. A nearby glyph disappearing is not a blink.
        if any(not min(base, accent) - 2 <= value <= max(base, accent) + 2
               for value, base, accent in zip(painted, background, (54, 94, 130))):
            return False
    return True


def editor_frames_equal_except_caret(
    first: Path, second: Path, crop: tuple[int, int, int, int]
) -> bool:
    x, y, width, height = crop

    def pixels(path: Path) -> bytes:
        result = subprocess.run(
            ["convert", str(path), "-crop", f"{width}x{height}+{x}+{y}",
             "+repage", "-depth", "8", "rgb:-"],
            check=True, capture_output=True,
        )
        if len(result.stdout) != width * height * 3:
            raise AcceptanceFailure("unexpected editor pixel count")
        return result.stdout

    return editor_pixels_equal_except_caret(pixels(first), pixels(second), width)


def bright_pixel_count(
    image: Path,
    *,
    crop: tuple[int, int, int, int] | None = None,
    threshold: int = 60,
) -> int:
    """Light pixels of a crop: sidebar glyphs and icons painted on the dark surface.

    ``threshold`` is the grey level (percent) a pixel must reach. Titles are near
    white, while thin anti-aliased chevrons only reach the 40% band.
    """
    arguments = ["convert", str(image)]
    if crop is not None:
        x, y, width, height = crop
        arguments.extend(["-crop", f"{width}x{height}+{x}+{y}", "+repage"])
    arguments.extend(
        [
            "-colorspace",
            "Gray",
            "-threshold",
            f"{threshold}%",
            "-format",
            "%[fx:mean*w*h]",
            "info:",
        ]
    )
    completed = run_command(arguments)
    try:
        return int(float(completed.stdout.strip()))
    except ValueError as error:
        raise AcceptanceFailure(
            f"unexpected ImageMagick bright-pixel metric: {completed.stdout.strip()}"
        ) from error


def near_color_pixel_count(
    image: Path,
    expected: tuple[int, int, int],
    *,
    crop: tuple[int, int, int, int],
    tolerance: int = 12,
) -> int:
    """Count antialiased pixels close to an expected scoped UI color."""
    x, y, width, height = crop
    completed = run_command(
        [
            "convert",
            str(image),
            "-crop",
            f"{width}x{height}+{x}+{y}",
            "+repage",
            "-depth",
            "8",
            "-format",
            "%c",
            "histogram:info:-",
        ]
    )
    return count_near_color_pixels(completed.stdout, expected, tolerance=tolerance)


def count_near_color_pixels(
    histogram: str, expected: tuple[int, int, int], *, tolerance: int
) -> int:
    count = 0
    for line in histogram.splitlines():
        match = re.match(
            r"\s*(\d+):\s+\(\s*(\d+),\s*(\d+),\s*(\d+)",
            line,
        )
        if match is None:
            continue
        pixels, red, green, blue = (int(value) for value in match.groups())
        channel_distance = max(
            abs(red - expected[0]),
            abs(green - expected[1]),
            abs(blue - expected[2]),
        )
        if channel_distance <= tolerance:
            count += pixels
    return count


def near_color_columns(
    image: Path,
    expected: tuple[int, int, int],
    *,
    crop: tuple[int, int, int, int],
    tolerance: int = 12,
) -> set[int]:
    """Absolute columns containing pixels close to an expected UI color."""
    x0, y0, width, height = crop
    completed = run_command(
        [
            "convert",
            str(image),
            "-crop",
            f"{width}x{height}+{x0}+{y0}",
            "+repage",
            "-depth",
            "8",
            "txt:-",
        ]
    )
    columns: set[int] = set()
    pixel = re.compile(r"^(\d+),(\d+): \((\d+),(\d+),(\d+)")
    for line in completed.stdout.splitlines():
        match = pixel.match(line)
        if match is None:
            continue
        x, _y, red, green, blue = (int(value) for value in match.groups())
        channel_distance = max(
            abs(red - expected[0]),
            abs(green - expected[1]),
            abs(blue - expected[2]),
        )
        if channel_distance <= tolerance:
            columns.add(x0 + x)
    return columns


def changed_pixel_columns(
    first: Path,
    second: Path,
    *,
    crop: tuple[int, int, int, int],
    threshold: int = 12,
) -> set[int]:
    """Absolute columns whose pixels changed between otherwise stable frames."""
    x0, y0, width, height = crop
    completed = run_command(
        [
            "convert",
            str(first),
            str(second),
            "-compose",
            "difference",
            "-composite",
            "-crop",
            f"{width}x{height}+{x0}+{y0}",
            "+repage",
            "-depth",
            "8",
            "txt:-",
        ]
    )
    columns: set[int] = set()
    pixel = re.compile(r"^(\d+),(\d+): \((\d+),(\d+),(\d+)")
    for line in completed.stdout.splitlines():
        match = pixel.match(line)
        if match is None:
            continue
        x, _y, red, green, blue = (int(value) for value in match.groups())
        if max(red, green, blue) >= threshold:
            columns.add(x0 + x)
    return columns


def mean_luminance(
    image: Path, *, crop: tuple[int, int, int, int] | None = None
) -> float:
    arguments = ["convert", str(image)]
    if crop is not None:
        x, y, width, height = crop
        arguments.extend(["-crop", f"{width}x{height}+{x}+{y}", "+repage"])
    arguments.extend(["-colorspace", "Gray", "-format", "%[fx:mean]", "info:"])
    completed = run_command(arguments)
    try:
        return float(completed.stdout.strip())
    except ValueError as error:
        raise AcceptanceFailure(
            f"unexpected ImageMagick luminance metric: {completed.stdout.strip()}"
        ) from error


def unique_color_count(
    image: Path, *, crop: tuple[int, int, int, int] | None = None
) -> int:
    arguments = ["convert", str(image)]
    if crop is not None:
        x, y, width, height = crop
        arguments.extend(["-crop", f"{width}x{height}+{x}+{y}", "+repage"])
    arguments.extend(["-format", "%k", "info:"])
    completed = run_command(arguments)
    try:
        return int(completed.stdout.strip())
    except ValueError as error:
        raise AcceptanceFailure(
            f"unexpected ImageMagick color-count metric: {completed.stdout.strip()}"
        ) from error


def column_profile(
    image: Path, crop: tuple[int, int, int, int]
) -> dict[str, set[int]]:
    """Classify every pixel column of a crop by what the editor painted there.

    ``ink`` holds glyph cores, ``overlay`` the editor's own selection color and
    ``caret`` the accent caret line. Columns are absolute screen x positions.
    """
    x0, y0, width, height = crop
    completed = run_command(
        [
            "convert",
            str(image),
            "-crop",
            f"{width}x{height}+{x0}+{y0}",
            "+repage",
            "-depth",
            "8",
            "txt:-",
        ]
    )
    profile: dict[str, set[int]] = {"ink": set(), "overlay": set(), "caret": set()}
    pixel = re.compile(r"^(\d+),(\d+): \((\d+),(\d+),(\d+)")
    for line in completed.stdout.splitlines():
        match = pixel.match(line)
        if match is None:
            continue
        x, _y, red, green, blue = (int(value) for value in match.groups())
        luminance = 0.299 * red + 0.587 * green + 0.114 * blue
        if luminance < 100 and blue - red < 40:
            profile["ink"].add(x0 + x)
        elif blue - red >= 40 and luminance < 150:
            profile["caret"].add(x0 + x)
        elif blue - red >= 10 and luminance > 200:
            profile["overlay"].add(x0 + x)
    return profile


def column_runs(columns: set[int], *, merge_gap: int) -> list[tuple[int, int]]:
    """Group sorted columns into inclusive runs, bridging gaps up to merge_gap."""
    runs: list[tuple[int, int]] = []
    for x in sorted(columns):
        if runs and x - runs[-1][1] <= merge_gap + 1:
            runs[-1] = (runs[-1][0], x)
        else:
            runs.append((x, x))
    return runs


def crop_luminances(
    image: Path, crop: tuple[int, int, int, int]
) -> dict[tuple[int, int], float]:
    """Luminance of every pixel of a crop keyed by its absolute (x, y)."""
    x0, y0, width, height = crop
    completed = run_command(
        [
            "convert",
            str(image),
            "-crop",
            f"{width}x{height}+{x0}+{y0}",
            "+repage",
            "-depth",
            "8",
            "txt:-",
        ]
    )
    luminances: dict[tuple[int, int], float] = {}
    pixel = re.compile(r"^(\d+),(\d+): \((\d+),(\d+),(\d+)")
    for line in completed.stdout.splitlines():
        match = pixel.match(line)
        if match is None:
            continue
        x, y, red, green, blue = (int(value) for value in match.groups())
        luminances[(x0 + x, y0 + y)] = 0.299 * red + 0.587 * green + 0.114 * blue
    return luminances


def sidebar_boundary_x(image: Path, *, y: int = 600) -> int:
    """Find the first light editor column after the dark sidebar surface."""
    left = 160
    right = 500
    luminances = crop_luminances(image, (left, y, right - left, 1))
    for x in range(left + 1, right - 3):
        dark_before = any(
            luminances[(candidate, y)] < 100.0
            for candidate in range(max(left, x - 8), x)
        )
        if dark_before and all(
            luminances[(x + offset, y)] > 220.0 for offset in range(4)
        ):
            return x
    raise AcceptanceFailure("could not locate the sidebar/editor boundary")


def shaded_column_coverage(
    image: Path, crop: tuple[int, int, int, int], *, max_luminance: float
) -> dict[int, int]:
    """Rows of every absolute column whose luminance is at most ``max_luminance``.

    Glyphs and one-pixel dividers cover a few rows per column; a painted
    scrollbar handle covers most of the band it scrolls.
    """
    coverage: dict[int, int] = {}
    for (x, _y), luminance in crop_luminances(image, crop).items():
        if luminance <= max_luminance:
            coverage[x] = coverage.get(x, 0) + 1
    return coverage


def shaded_row_runs(
    image: Path, *, x: int, y: int, height: int, max_luminance: float
) -> list[tuple[int, int]]:
    """Inclusive row runs of column ``x`` whose luminance is at most ``max_luminance``."""
    rows = {
        row
        for (_x, row), luminance in crop_luminances(image, (x, y, 1, height)).items()
        if luminance <= max_luminance
    }
    return column_runs(rows, merge_gap=0)


class WindowDriver:
    def __init__(self, scenario: str, temporary_root: Path) -> None:
        self.scenario = scenario
        self.stage = "startup"
        # Secure scenarios intentionally never create screenshots: the window can
        # contain a master password or decrypted note text at any failure point.
        self.sensitive = scenario.startswith("secure")
        self.temporary_root = temporary_root
        self.temporary_root.chmod(0o755)
        self.runtime = temporary_root / "xdg"
        self.runtime.mkdir(mode=0o700)
        self.home = temporary_root / "home"
        self.home.mkdir(mode=0o777)
        self.home.chmod(0o777)
        self.environment = os.environ.copy()
        self.environment.update(
            {
                "DISPLAY": DISPLAY,
                "HOME": str(self.home),
                "XDG_RUNTIME_DIR": str(self.runtime),
                "FLOEM_FORCE_TINY_SKIA": "1",
                "RUST_BACKTRACE": "1",
            }
        )
        ARTIFACT_ROOT.mkdir(parents=True, exist_ok=True)
        self.xvfb_log_path = ARTIFACT_ROOT / f"{scenario}-xvfb.log"
        self.app_log_paths: list[Path] = []
        self._sensitive_values: set[bytes] = set()
        self._log_handles: list[object] = []
        self._capture_sequence = 0
        self.xvfb: subprocess.Popen[bytes] | None = None
        self.app: subprocess.Popen[bytes] | None = None
        self.window_id: str | None = None

    def set_stage(self, stage: str) -> None:
        if not ui_diagnostic_context_valid(self.scenario, stage):
            raise ValueError("invalid UI diagnostic stage")
        self.stage = stage

    def start_xvfb(self) -> None:
        log = self.xvfb_log_path.open("wb")
        self._log_handles.append(log)
        self.xvfb = subprocess.Popen(
            [
                "Xvfb",
                DISPLAY,
                "-noreset",
                "-screen",
                "0",
                f"{SCREEN_WIDTH}x{SCREEN_HEIGHT}x24",
            ],
            stdout=log,
            stderr=subprocess.STDOUT,
            env=self.environment,
        )

        def ready() -> bool:
            if self.xvfb is not None and self.xvfb.poll() is not None:
                raise AcceptanceFailure(
                    f"Xvfb exited with {self.xvfb.returncode}; see {self.xvfb_log_path}"
                )
            return Path("/tmp/.X11-unix/X99").exists()

        wait_until("Xvfb display socket", ready)

    def start_app(
        self,
        workspace: Path | None,
        phase: str,
        *,
        run_as_uid: int | None = None,
        expected_size: tuple[int, int] = (SCREEN_WIDTH, SCREEN_HEIGHT),
        environment_overrides: dict[str, str] | None = None,
    ) -> None:
        if self.app is not None:
            raise AcceptanceFailure("attempted to start a second app before closing the first")
        log_path = ARTIFACT_ROOT / f"{self.scenario}-{phase}-app.log"
        self.app_log_paths.append(log_path)
        log = log_path.open("wb")
        self._log_handles.append(log)
        demote = None
        if run_as_uid is not None:
            os.chown(self.runtime, run_as_uid, run_as_uid)
            self.runtime.chmod(0o700)

            def demote() -> None:
                os.setgroups([])
                os.setgid(run_as_uid)
                os.setuid(run_as_uid)

        command = [str(APP_BINARY)]
        if workspace is not None:
            command.append(str(workspace))
        self.app = subprocess.Popen(
            command,
            stdout=log,
            stderr=subprocess.STDOUT,
            env=self.environment | (environment_overrides or {}),
            preexec_fn=demote,
        )
        self.window_id = self._wait_for_window()
        geometry = self.xdotool("getwindowgeometry", "--shell", self.window_id).stdout
        values = {}
        for line in geometry.splitlines():
            key, separator, value = line.partition("=")
            if separator:
                values[key] = value
        actual = (int(values.get("WIDTH", "0")), int(values.get("HEIGHT", "0")))
        if actual != expected_size:
            raise AcceptanceFailure(
                f"unexpected Stillus window size: {actual}, expected {expected_size}"
            )
        self.xdotool("windowfocus", "--sync", self.window_id)
        wait_for_first_paint(self.window_id, self.environment)

    def window_size(self) -> tuple[int, int]:
        if self.window_id is None:
            raise AcceptanceFailure("cannot read window size before the app is ready")
        geometry = self.xdotool(
            "getwindowgeometry", "--shell", self.window_id
        ).stdout
        values = {}
        for line in geometry.splitlines():
            key, separator, value = line.partition("=")
            if separator:
                values[key] = value
        return int(values.get("WIDTH", "0")), int(values.get("HEIGHT", "0"))

    def resize_window(self, width: int, height: int) -> None:
        if self.window_id is None:
            raise AcceptanceFailure("cannot resize a window before the app is ready")
        self.xdotool("windowsize", "--sync", self.window_id, str(width), str(height))
        wait_until(
            "window resize",
            lambda: self.window_size() == (width, height),
            interval=0.03,
        )

    def _wait_for_window(self) -> str:
        found: list[str] = []

        def search() -> bool:
            if self.app is not None and self.app.poll() is not None:
                raise AcceptanceFailure(
                    f"Stillus exited with {self.app.returncode} before opening a window"
                )
            completed = run_command(
                ["xdotool", "search", "--onlyvisible", "--name", "^Stillus$"],
                environment=self.environment,
                check=False,
            )
            if completed.returncode not in (0, 1):
                raise AcceptanceFailure(completed.stderr.strip() or "xdotool search failed")
            found[:] = [line for line in completed.stdout.splitlines() if line.strip()]
            return len(found) == 1

        wait_until("one visible Stillus window", search)
        return found[0]

    def xdotool(self, *arguments: str) -> subprocess.CompletedProcess[str]:
        return run_command(
            ["xdotool", *arguments], environment=self.environment, check=True
        )

    def move_to(self, control: str) -> None:
        if self.window_id is None:
            raise AcceptanceFailure("cannot move the pointer before the app window is ready")
        x, y = CONTROLS[control]
        self.xdotool("windowfocus", "--sync", self.window_id)
        self.xdotool(
            "mousemove",
            "--window",
            self.window_id,
            "620",
            "420",
        )
        self.xdotool(
            "mousemove",
            "--window",
            self.window_id,
            str(x),
            str(y),
        )

    def click(self, control: str) -> None:
        if control.startswith("password_"):
            self.wait_for_password_dialog()
        self.move_to(control)
        self.xdotool("click", "1")
        time.sleep(EVENT_SETTLE_SECONDS)

    def double_click(self, control: str) -> None:
        self.move_to(control)
        self.xdotool("click", "--repeat", "2", "--delay", "120", "1")
        time.sleep(EVENT_SETTLE_SECONDS)

    def triple_click(self, control: str) -> None:
        self.move_to(control)
        self.xdotool("click", "--repeat", "3", "--delay", "120", "1")
        time.sleep(EVENT_SETTLE_SECONDS)

    def right_click(self, control: str) -> None:
        self.move_to(control)
        self.xdotool("click", "3")
        time.sleep(EVENT_SETTLE_SECONDS)

    def press(self, control: str) -> None:
        """Press and hold the primary button over a control."""
        self.move_to(control)
        self.xdotool("mousedown", "1")
        time.sleep(EVENT_SETTLE_SECONDS)

    def drag_to(self, control: str) -> None:
        """Move the held pointer to a control without releasing the button."""
        if self.window_id is None:
            raise AcceptanceFailure("cannot drag before the app window is ready")
        x, y = CONTROLS[control]
        self.xdotool("mousemove", "--sync", "--window", self.window_id, str(x), str(y))
        time.sleep(EVENT_SETTLE_SECONDS)

    def release(self) -> None:
        self.xdotool("mouseup", "1")
        time.sleep(EVENT_SETTLE_SECONDS)

    def hover(self, control: str) -> None:
        """Move the released pointer to a control without any button."""
        self.drag_to(control)

    def click_context_menu_row(self, anchor: str, row: int) -> None:
        if self.window_id is None:
            raise AcceptanceFailure("cannot click a context menu before the app is ready")
        if row not in range(3):
            raise AcceptanceFailure(f"unsupported context menu row: {row}")
        anchor_x, anchor_y = CONTROLS[anchor]
        self.xdotool(
            "mousemove",
            "--sync",
            "--window",
            self.window_id,
            str(anchor_x + CONTEXT_MENU_X_OFFSET),
            str(
                anchor_y
                + CONTEXT_MENU_FIRST_ROW_Y_OFFSET
                + row * CONTEXT_MENU_ROW_HEIGHT
            ),
        )
        self.xdotool("click", "1")
        time.sleep(EVENT_SETTLE_SECONDS)

    def modified_click(self, control: str, modifier: str) -> None:
        self.move_to(control)
        self.xdotool("keydown", modifier)
        try:
            self.xdotool("click", "1")
        finally:
            self.xdotool("keyup", modifier)
        time.sleep(EVENT_SETTLE_SECONDS)

    def click_point(self, x: int, y: int, *, settle: bool = True) -> None:
        if self.window_id is None:
            raise AcceptanceFailure("cannot click before the app window is ready")
        self.xdotool("windowfocus", "--sync", self.window_id)
        self.xdotool(
            "mousemove",
            "--sync",
            "--window",
            self.window_id,
            "620",
            "420",
        )
        self.xdotool(
            "mousemove",
            "--sync",
            "--window",
            self.window_id,
            str(x),
            str(y),
            "click",
            "1",
        )
        if settle:
            time.sleep(EVENT_SETTLE_SECONDS)

    def press_point(self, x: int, y: int) -> None:
        if self.window_id is None:
            raise AcceptanceFailure("cannot press before the app window is ready")
        self.xdotool("windowfocus", "--sync", self.window_id)
        self.xdotool(
            "mousemove", "--sync", "--window", self.window_id, str(x), str(y)
        )
        self.xdotool("mousedown", "1")
        time.sleep(EVENT_SETTLE_SECONDS)

    def drag_point(self, x: int, y: int) -> None:
        if self.window_id is None:
            raise AcceptanceFailure("cannot drag before the app window is ready")
        self.xdotool(
            "mousemove", "--sync", "--window", self.window_id, str(x), str(y)
        )
        time.sleep(EVENT_SETTLE_SECONDS)

    def click_note(
        self,
        index: int,
        *,
        x: int = 150,
        settle: bool = True,
        expanded: str = "all",
        expanded_groups: tuple[str, ...] | None = None,
        counts: dict[str, int] = DEMO_NOTE_COUNTS,
        direct_counts: dict[str, int] | None = None,
        categories: tuple[str, ...] = DEMO_CATEGORIES,
    ) -> None:
        """Click the ``index``-th note listed under the expanded sidebar group."""
        _, y = note_row_center(
            index,
            x=x,
            expanded=expanded,
            expanded_groups=expanded_groups,
            counts=counts,
            direct_counts=direct_counts,
            categories=categories,
        )
        self.click_point(x, y, settle=settle)

    def wait_for_password_dialog(self, *, opened: bool = True) -> None:
        if self.window_id is None:
            raise AcceptanceFailure("cannot wait for a password dialog without a window")

        def painted() -> bool:
            # Sample only the modal's outer backdrop and blank left padding.
            # Return two luminances in memory, never capture passwords/note text.
            result = run_command(
                [
                    "import", "-display", DISPLAY, "-window", self.window_id,
                    "-crop", "21x1+410+400", "+repage", "-colorspace", "Gray",
                    "-format", "%[fx:p{0,0}.r] %[fx:p{20,0}.r]", "info:",
                ],
                environment=self.environment,
            )
            backdrop, padding = map(float, result.stdout.split())
            return (0.1 < backdrop < 0.85 and padding > 0.95) if opened else backdrop > 0.95

        wait_until("password dialog visibility", painted, interval=0.03)

    def window_color_pixel_count(
        self,
        expected: tuple[int, int, int],
        *,
        crop: tuple[int, int, int, int],
        tolerance: int = 12,
    ) -> int:
        """Read a scoped color histogram in memory, without a screenshot file."""
        if self.window_id is None:
            raise AcceptanceFailure("cannot sample a control without a window")
        x, y, width, height = crop
        completed = run_command(
            [
                "import", "-display", DISPLAY, "-window", self.window_id,
                "-crop", f"{width}x{height}+{x}+{y}", "+repage",
                "-depth", "8", "-format", "%c", "histogram:info:-",
            ],
            environment=self.environment,
        )
        return count_near_color_pixels(completed.stdout, expected, tolerance=tolerance)

    def window_image_signature(self, crop: tuple[int, int, int, int]) -> str:
        """Compare native pixels in memory without creating a screenshot file."""
        if self.window_id is None:
            raise AcceptanceFailure("cannot sample a control without a window")
        x, y, width, height = crop
        result = run_command(
            ["import", "-display", DISPLAY, "-window", self.window_id,
             "-crop", f"{width}x{height}+{x}+{y}", "+repage",
             "-format", "%#", "info:"],
            environment=self.environment,
        )
        signature = result.stdout.strip()
        if not re.fullmatch(r"[0-9a-f]{64}", signature):
            raise AcceptanceFailure("invalid in-memory image signature")
        return signature

    def wait_for_password_field_focus(self, control: str) -> None:
        # Key delivery and Floem's queued focus change are separate. In
        # particular, Backspace after Shift+Tab must reach the primary field.
        # Sample only its left border, never the masked value or note body.
        _, y = CONTROLS[control]
        wait_until(
            "password field focus",
            lambda: self.window_color_pixel_count(
                (54, 94, 130), crop=(444, y - 10, 4, 20)
            ) >= 15,
            interval=0.03,
        )

    def type_text(self, text: str) -> None:
        self.xdotool("type", "--clearmodifiers", "--delay", "12", text)
        time.sleep(EVENT_SETTLE_SECONDS)

    def type_sensitive_text(self, text: str) -> None:
        completed = subprocess.run(
            ["xdotool", "type", "--clearmodifiers", "--delay", "12", "--file", "-"],
            env=self.environment,
            input=text,
            check=False,
            capture_output=True,
            text=True,
        )
        if completed.returncode != 0:
            raise AcceptanceFailure(
                f"sensitive XTEST typing failed with exit code {completed.returncode}"
            )
        time.sleep(EVENT_SETTLE_SECONDS)

    def key(self, key: str) -> None:
        self.xdotool("key", "--clearmodifiers", key)
        time.sleep(EVENT_SETTLE_SECONDS)

    def wheel(
        self,
        direction: str,
        clicks: int = 4,
        *,
        control: str = "editor",
        delay_ms: int = 120,
        settle: bool = True,
    ) -> None:
        if direction not in {"up", "down"}:
            raise AcceptanceFailure(f"unsupported wheel direction: {direction}")
        self.move_to(control)
        button = "4" if direction == "up" else "5"
        self.xdotool(
            "click", "--repeat", str(clicks), "--delay", str(delay_ms), button
        )
        if settle:
            time.sleep(EVENT_SETTLE_SECONDS)

    def capture(
        self, name: str, *, crop: tuple[int, int, int, int] | None = None
    ) -> Path:
        self._capture_sequence += 1
        screenshot = self.temporary_root / (
            f"{self.scenario}-{self._capture_sequence:03d}-{name}.png"
        )
        arguments = ["import", "-display", DISPLAY, "-window", "root"]
        if crop is not None:
            x, y, width, height = crop
            arguments.extend(["-crop", f"{width}x{height}+{x}+{y}", "+repage"])
        run_command(
            [*arguments, str(screenshot)],
            environment=self.environment,
        )
        return screenshot

    def capture_safe_footer(self, name: str) -> Path:
        if self.window_id is None:
            raise AcceptanceFailure("cannot capture the footer before the app is ready")
        screenshot = self.temporary_root / (
            f"{self.scenario}-{self._capture_sequence:03d}-{name}-footer.png"
        )
        self._capture_sequence += 1
        run_command(
            [
                "import",
                "-display",
                DISPLAY,
                "-window",
                self.window_id,
                "-crop",
                "100x40+1140+760",
                "+repage",
                str(screenshot),
            ],
            environment=self.environment,
        )
        return screenshot

    def wait_for_footer_change(
        self,
        description: str,
        reference: Path,
        *,
        minimum_pixels: int = 30,
        timeout: float = DEFAULT_TIMEOUT_SECONDS,
    ) -> None:
        current = self.capture_safe_footer("footer-change")

        def changed() -> bool:
            nonlocal current
            current.unlink(missing_ok=True)
            current = self.capture_safe_footer("footer-change")
            return image_difference(reference, current) >= minimum_pixels

        try:
            wait_until(description, changed, timeout=timeout, interval=0.03)
        finally:
            current.unlink(missing_ok=True)

    def wait_for_stable_footer_action(
        self, description: str, *, timeout: float = DEFAULT_TIMEOUT_SECONDS
    ) -> Path:
        previous = self.capture_safe_footer("footer-stable-previous")
        stable = previous

        def ready() -> bool:
            nonlocal previous, stable
            current = self.capture_safe_footer("footer-stable-next")
            difference = image_difference(previous, current)
            previous.unlink(missing_ok=True)
            previous = current
            stable = current
            return difference == 0 and dark_pixel_count(current) >= 8

        wait_until(description, ready, timeout=timeout, interval=0.03)
        return stable

    def wait_for_stable_frame(
        self,
        description: str,
        *,
        crop: tuple[int, int, int, int] | None = None,
        minimum_dark_pixels: int = 0,
        minimum_luminance: float = 0.1,
        stable_for: float = 0.0,
        timeout: float = 3.0,
        ignore_editor_caret: bool = False,
    ) -> Path:
        if ignore_editor_caret and crop != EDITOR_CROP:
            raise AcceptanceFailure("caret-aware comparison requires the editor crop")
        previous = self.capture("stability-previous")
        stable = self.capture("stability-current")
        stable_since: float | None = None

        def unchanged() -> bool:
            nonlocal previous, stable, stable_since
            current = self.capture("stability-next")
            equal = (
                editor_frames_equal_except_caret(previous, current, EDITOR_CROP)
                if ignore_editor_caret else image_difference(previous, current, crop=crop) == 0
            )
            previous.unlink(missing_ok=True)
            previous = current
            stable = current
            frame_is_unchanged = (
                equal
                and dark_pixel_count(current, crop=crop) >= minimum_dark_pixels
                and mean_luminance(current, crop=crop) >= minimum_luminance
            )
            if not frame_is_unchanged:
                stable_since = None
                return False
            now = time.monotonic()
            if stable_since is None:
                stable_since = now
            return now - stable_since >= stable_for

        wait_until(description, unchanged, timeout=timeout, interval=0.05)
        return stable

    def wait_for_visual_change(
        self,
        description: str,
        reference: Path,
        *,
        crop: tuple[int, int, int, int],
        minimum_pixels: int = 50,
        timeout: float = DEFAULT_TIMEOUT_SECONDS,
        cropped_reference: bool = False,
    ) -> Path:
        # Input-caret checks sample only their small field, avoiding full-window
        # PNG encoding delays that can repeatedly sample the same blink phase.
        capture_crop = crop if cropped_reference else None
        compare_crop = None if cropped_reference else crop
        changed = self.capture("visual-change", crop=capture_crop)

        def differs() -> bool:
            nonlocal changed
            changed.unlink(missing_ok=True)
            changed = self.capture("visual-change", crop=capture_crop)
            return image_difference(reference, changed, crop=compare_crop) >= minimum_pixels

        wait_until(description, differs, timeout=timeout, interval=0.05)
        return changed

    def click_until(
        self,
        control: str,
        description: str,
        predicate: Callable[[], bool],
        *,
        timeout: float = DEFAULT_TIMEOUT_SECONDS,
    ) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if predicate():
                return
            self.click(control)
        raise AcceptanceFailure(f"timed out clicking {control} until {description}")

    def close_app(self) -> None:
        if self.app is None or self.window_id is None:
            raise AcceptanceFailure("cannot close an app that is not running")
        request_window_close(self.window_id, self.environment)
        try:
            return_code = self.app.wait(timeout=3.0)
        except subprocess.TimeoutExpired as error:
            self.app.terminate()
            self.app.wait(timeout=2.0)
            raise AcceptanceFailure("Stillus did not close after WM_DELETE_WINDOW") from error
        finally:
            self.app = None
            self.window_id = None
        if return_code != 0:
            raise AcceptanceFailure(f"Stillus closed with exit code {return_code}")

    def crash_app(self) -> None:
        if self.app is None:
            raise AcceptanceFailure("cannot crash an app that is not running")
        self.app.kill()
        self.app.wait(timeout=2.0)
        self.app = None
        self.window_id = None

    def capture_failure(self) -> Path | None:
        if self.sensitive:
            return None
        if self.xvfb is None or self.xvfb.poll() is not None:
            return None
        screenshot = ARTIFACT_ROOT / f"{self.scenario}-failure.png"
        completed = run_command(
            ["import", "-display", DISPLAY, "-window", "root", str(screenshot)],
            environment=self.environment,
            check=False,
        )
        return screenshot if completed.returncode == 0 else None

    def register_sensitive(self, *values: str) -> None:
        self._sensitive_values.update(
            value.encode("utf-8") for value in values if value
        )

    def sanitize_failure_logs(self) -> None:
        if not self.sensitive:
            return
        redacted = b"Stillus UI acceptance log redacted: sensitive test data detected.\n"
        for path in (self.xvfb_log_path, *self.app_log_paths):
            if not path.is_file():
                continue
            contents = path.read_bytes()
            if any(value in contents for value in self._sensitive_values):
                path.write_bytes(redacted)
        (ARTIFACT_ROOT / f"{self.scenario}-failure.png").unlink(missing_ok=True)

    def redact_message(self, message: str) -> str:
        if not self.sensitive:
            return message
        redacted = message
        for value in self._sensitive_values:
            redacted = redacted.replace(value.decode("utf-8"), "[REDACTED]")
        return redacted

    def cleanup(self) -> None:
        if self.app is not None:
            self.app.terminate()
            try:
                self.app.wait(timeout=2.0)
            except subprocess.TimeoutExpired:
                self.app.kill()
                self.app.wait(timeout=2.0)
            self.app = None
            self.window_id = None
        if self.xvfb is not None:
            self.xvfb.terminate()
            try:
                self.xvfb.wait(timeout=2.0)
            except subprocess.TimeoutExpired:
                self.xvfb.kill()
                self.xvfb.wait(timeout=2.0)
            self.xvfb = None
        for handle in self._log_handles:
            handle.close()
        self._log_handles.clear()

    def remove_success_artifacts(self) -> None:
        paths = [self.xvfb_log_path, *self.app_log_paths]
        paths.append(ARTIFACT_ROOT / f"{self.scenario}-failure.png")
        for path in paths:
            path.unlink(missing_ok=True)


def copy_demo(temporary_root: Path) -> Path:
    workspace = temporary_root / "workspace"
    return generate_demo_workspace(workspace)


def create_workspace(temporary_root: Path, name: str) -> Path:
    workspace = temporary_root / name
    (workspace / "notes").mkdir(parents=True)
    return workspace


def assert_transient_sidebar_scrollbar(
    driver: WindowDriver, description: str, initial_top_frame: Path
) -> None:
    """Prove reveal, timer reset, idle hide and retained scrolled viewport."""
    driver.wheel(
        "down",
        clicks=1,
        control="sidebar_scroll",
        delay_ms=20,
    )
    driver.move_to("editor")
    driver.wait_for_visual_change(
        f"{description} viewport movement",
        initial_top_frame,
        crop=SIDEBAR_SCROLL_CONTENT_CROP,
        minimum_pixels=200,
    )

    # The second movement happens shortly before the first 500ms deadline.
    # Capturing after that original deadline proves that a stale timer cannot
    # hide a scrollbar while a newer scroll gesture is still recent.
    time.sleep(0.35)
    before_second_scroll = driver.capture(f"{description}-before-second-scroll")
    driver.wheel(
        "down",
        clicks=1,
        control="sidebar_scroll",
        delay_ms=20,
    )
    driver.move_to("editor")
    # Use the frame that already proves movement. Taking several more frames
    # for stability can consume the entire 500ms visibility interval under load.
    visible = driver.wait_for_visual_change(
        f"{description} repeated viewport movement",
        before_second_scroll,
        crop=SIDEBAR_SCROLL_CONTENT_CROP,
        minimum_pixels=50,
    )

    time.sleep(SIDEBAR_SCROLLBAR_HIDE_SECONDS + 0.05)
    idle = driver.wait_for_stable_frame(
        f"{description} scrollbar hides after idle",
        crop=(12, 110, 232, 620),
    )
    bar_difference = image_difference(visible, idle, crop=SIDEBAR_SCROLLBAR_CROP)
    if bar_difference < 50:
        raise AcceptanceFailure(
            f"{description} scrollbar did not appear and hide ({bar_difference} pixels)"
        )
    if image_difference(initial_top_frame, idle, crop=SIDEBAR_SCROLL_CONTENT_CROP) < 200:
        raise AcceptanceFailure(
            f"{description} viewport returned while its scrollbar was hiding"
        )

    initial_colors = unique_color_count(initial_top_frame, crop=SIDEBAR_SCROLLBAR_CROP)
    if initial_colors != 1:
        raise AcceptanceFailure(
            f"{description} scrollbar was visible before scrolling "
            f"({initial_colors} gutter colors)"
        )


def assert_focused_input_caret(
    driver: WindowDriver,
    description: str,
    crop: tuple[int, int, int, int],
    placeholder_color: tuple[int, int, int],
) -> None:
    """An empty input must paint its muted placeholder and focused caret."""
    reference = driver.capture(f"{description}-caret-reference", crop=crop)

    def placeholder_is_painted() -> bool:
        nonlocal reference
        reference.unlink(missing_ok=True)
        reference = driver.capture(f"{description}-caret-reference", crop=crop)
        return near_color_pixel_count(
            reference, placeholder_color, crop=(0, 0, crop[2], crop[3])
        ) >= 8

    wait_until(
        f"{description} muted placeholder paint",
        placeholder_is_painted,
        timeout=2.0,
        interval=0.03,
    )
    changed = driver.wait_for_visual_change(
        f"{description} focused caret blink",
        reference,
        crop=crop,
        minimum_pixels=4,
        timeout=2.0,
        cropped_reference=True,
    )
    changed.unlink(missing_ok=True)


def assert_masked_password_caret(
    driver: WindowDriver,
    description: str,
    control: str,
    text_color: tuple[int, int, int],
    *,
    before_text: bool,
) -> None:
    """A masked field must blink its caret on the expected side of its text."""
    crop = password_input_crop(control)
    local_crop = (0, 0, crop[2], crop[3])
    time.sleep(EVENT_SETTLE_SECONDS * 2)
    reference = driver.capture(f"{description}-caret-reference", crop=crop)

    def text_is_painted() -> bool:
        nonlocal reference
        reference.unlink(missing_ok=True)
        reference = driver.capture(f"{description}-caret-reference", crop=crop)
        return len(near_color_columns(reference, text_color, crop=local_crop)) >= 2

    wait_until(
        f"{description} text paint",
        text_is_painted,
        timeout=2.0,
        interval=0.03,
    )
    changed = driver.wait_for_visual_change(
        f"{description} caret blink",
        reference,
        crop=crop,
        minimum_pixels=4,
        timeout=2.0,
        cropped_reference=True,
    )
    text_columns = near_color_columns(reference, text_color, crop=local_crop)
    caret_columns = changed_pixel_columns(reference, changed, crop=local_crop)
    reference.unlink(missing_ok=True)
    changed.unlink(missing_ok=True)
    if not caret_columns:
        raise AcceptanceFailure(f"{description} caret blink had no changed columns")
    text_edge = min(text_columns) if before_text else max(text_columns)
    adjacent_caret_columns = {
        column
        for column in caret_columns
        if (
            text_edge - 8 <= column < text_edge
            if before_text
            else text_edge < column <= text_edge + 8
        )
    }
    correctly_placed = bool(adjacent_caret_columns)
    if not correctly_placed:
        expected_side = "before" if before_text else "after"
        raise AcceptanceFailure(
            f"{description} caret was not {expected_side} text "
            f"(caret={min(caret_columns)}..{max(caret_columns)}, "
            f"text={min(text_columns)}..{max(text_columns)})"
        )


def assert_password_verification_feedback(
    driver: WindowDriver, before_verification: Path
) -> None:
    """Password verification is neutral and does not reflow the dialog."""
    verification = driver.capture("password-verification-feedback")

    def neutral_status_is_painted() -> bool:
        nonlocal verification
        verification.unlink(missing_ok=True)
        verification = driver.capture("password-verification-feedback")
        return (
            near_color_pixel_count(
                verification,
                (35, 39, 45),
                crop=PASSWORD_UNLOCK_FEEDBACK_CROP,
            )
            >= 8
            and near_color_pixel_count(
                verification,
                (246, 247, 248),
                crop=PASSWORD_UNLOCK_CANCEL_CROP,
            )
            >= 100
            and near_color_pixel_count(
                verification,
                (166, 184, 200),
                crop=PASSWORD_UNLOCK_SUBMIT_CROP,
            )
            >= 100
        )

    wait_until(
        "neutral password verification feedback",
        neutral_status_is_painted,
        timeout=0.6,
        interval=0.02,
    )
    static_difference = image_difference(
        before_verification,
        verification,
        crop=PASSWORD_UNLOCK_STATIC_EDGE_CROP,
    )
    protection_icon_colors = unique_color_count(
        verification,
        crop=PROTECTION_ACTION_ICON_CROP,
    )
    disabled_cancel_pixels = near_color_pixel_count(
        verification,
        (246, 247, 248),
        crop=PASSWORD_UNLOCK_CANCEL_CROP,
    )
    disabled_submit_pixels = near_color_pixel_count(
        verification,
        (166, 184, 200),
        crop=PASSWORD_UNLOCK_SUBMIT_CROP,
    )
    before_verification.unlink(missing_ok=True)
    verification.unlink(missing_ok=True)
    if static_difference > 8:
        raise AcceptanceFailure(
            "password verification changed the dialog button geometry "
            f"({static_difference} edge pixels)"
        )
    if protection_icon_colors < 3:
        raise AcceptanceFailure(
            "password verification hid the disabled protection icon "
            f"({protection_icon_colors} icon colors)"
        )
    if disabled_cancel_pixels < 100 or disabled_submit_pixels < 100:
        raise AcceptanceFailure(
            "password verification did not disable both dialog buttons "
            f"(cancel={disabled_cancel_pixels}, submit={disabled_submit_pixels})"
        )


def assert_password_button_hover_geometry(driver: WindowDriver) -> None:
    """Hover colors stay inside the fixed password-action rectangles."""
    cases = (
        (
            "password_unlock_cancel",
            PASSWORD_UNLOCK_CANCEL_CROP,
            (229, 238, 246),
        ),
        (
            "password_unlock_submit",
            PASSWORD_UNLOCK_SUBMIT_CROP,
            (44, 82, 117),
        ),
    )
    for control, crop, hover_color in cases:
        button_left = crop[0]
        button_right = crop[0] + crop[2] - 1
        scan_crop = (crop[0] - 4, crop[1] - 4, crop[2] + 8, crop[3] + 8)
        driver.hover(control)
        hovered = driver.capture(f"{control}-hover")
        columns: set[int] = set()

        def hover_is_painted() -> bool:
            nonlocal hovered, columns
            hovered.unlink(missing_ok=True)
            hovered = driver.capture(f"{control}-hover")
            columns = near_color_columns(
                hovered, hover_color, crop=scan_crop, tolerance=8
            )
            return len(columns) >= crop[2] - 4

        wait_until(
            f"{control} hover paint",
            hover_is_painted,
            timeout=0.6,
            interval=0.02,
        )
        hovered.unlink(missing_ok=True)
        left = min(columns)
        right = max(columns)
        if (
            left not in (button_left, button_left + 1)
            or right not in (button_right - 1, button_right)
            or right - left + 1 < crop[2] - 2
        ):
            bounds = "none" if not columns else f"{min(columns)}..{max(columns)}"
            raise AcceptanceFailure(
                f"{control} hover changed button geometry "
                f"(hover={bounds}, expected={button_left}..{button_right})"
            )


def write_tagged_note(path: Path, title: str, tags: list[str]) -> None:
    """Write a note whose first body line, YAML title and file stem all agree.

    Autosave relocates a note whose first line disagrees with its path, so
    scenarios that read the file back by its original path keep them equal.
    """
    rendered_tags = ", ".join(f"'{tag}'" for tag in tags)
    path.write_text(
        "---\n"
        "favorited: false\n"
        "pinned: false\n"
        f"tags: [{rendered_tags}]\n"
        f"title: '{title}'\n"
        "created: '2026-09-03T00:00:00.000Z'\n"
        "modified: '2026-09-03T00:00:00.000Z'\n"
        "---\n"
        f"{title}\n",
        encoding="utf-8",
    )


def create_secure_workspace(
    temporary_root: Path,
    name: str,
    *,
    title: str,
    body: str,
    tag: str,
) -> tuple[Path, Path]:
    workspace = create_workspace(temporary_root, name)
    note = workspace / "notes" / f"{title}.md"
    note.write_text(
        "---\n"
        "favorited: false\n"
        "pinned: false\n"
        f"tags: ['{tag}']\n"
        f"title: '{title}'\n"
        "created: '2026-09-02T00:00:00.000Z'\n"
        "modified: '2026-09-02T00:00:00.000Z'\n"
        "---\n"
        f"# {title}\n{body}\n",
        encoding="utf-8",
    )
    (workspace / "notes" / "ZZZZ Unrelated search decoy.md").write_text(
        "---\ntitle: 'ZZZZ Unrelated search decoy'\ntags: ['decoy']\n---\n"
        "# ZZZZ Unrelated search decoy\nunrelated search decoy body\n",
        encoding="utf-8",
    )
    return workspace, note


def make_workspace_accessible(workspace: Path) -> None:
    workspace.chmod(0o777)
    for path in workspace.rglob("*"):
        if path.is_dir():
            path.chmod(0o777)
        elif path.is_file():
            path.chmod(0o666)


def assert_no_temporary_files(workspace: Path) -> None:
    temporary = [
        path
        for path in workspace.rglob("*")
        if path.is_file()
        and (
            ".stillus-tmp-" in path.name
            or path.name.startswith(".ntrm-secure-")
            or (
                path.name.startswith(".ntrm-transition-")
                and path.name.endswith(".journal")
            )
            or path.name.startswith(".protected-")
        )
    ]
    if temporary:
        raise AcceptanceFailure(f"temporary note files remain: {temporary}")


def app_managed_files(workspace: Path) -> list[Path]:
    return sorted(
        path
        for path in workspace.rglob("*")
        if path.is_file() and not path.is_symlink()
    )


def plaintext_leaks(workspace: Path, *markers: str) -> list[str]:
    encoded = [(marker, marker.encode("utf-8")) for marker in markers if marker]
    leaked: set[str] = set()
    for path in app_managed_files(workspace):
        contents = path.read_bytes()
        for _, needle in encoded:
            if needle in contents:
                leaked.add(path.relative_to(workspace).as_posix())
    return sorted(leaked)


def assert_plaintext_absent(workspace: Path, *markers: str) -> None:
    leaked = plaintext_leaks(workspace, *markers)
    if leaked:
        raise AcceptanceFailure(
            f"protected plaintext leaked to {len(leaked)} app-managed file(s): {leaked}"
        )


def assert_logs_redacted(driver: WindowDriver, *markers: str) -> None:
    encoded = [(marker, marker.encode("utf-8")) for marker in markers if marker]
    leaked: set[str] = set()
    for path in (driver.xvfb_log_path, *driver.app_log_paths):
        if not path.is_file():
            continue
        contents = path.read_bytes()
        for _, needle in encoded:
            if needle in contents:
                leaked.add(path.name)
    if leaked:
        raise AcceptanceFailure(
            f"protected plaintext leaked to {len(leaked)} diagnostic log(s): {sorted(leaked)}"
        )


def protected_note_files(workspace: Path) -> list[Path]:
    notes = workspace / "notes"
    if not notes.is_dir():
        return []
    return sorted(
        path
        for path in notes.iterdir()
        if path.is_file()
        and ENCRYPTION_MARKER in path.read_bytes()
        and ARMORED_AGE_PREFIX in path.read_bytes()
    )


def encrypted_note_body(path: Path) -> bytes:
    contents = path.read_bytes()
    offset = contents.find(ARMORED_AGE_PREFIX)
    if offset < 0 or ENCRYPTION_MARKER not in contents[:offset]:
        raise AcceptanceFailure(f"protected body envelope is missing in {path.name}")
    return contents[offset:]


def protected_recovery_files(workspace: Path) -> list[Path]:
    return [
        path
        for path in recovery_files(workspace)
        if path.read_bytes().startswith(AGE_PREFIX)
    ]


def current_search_generation(workspace: Path) -> tuple[str, Path] | None:
    search = workspace / ".stillus" / "search"
    pointer = search / "CURRENT"
    if not pointer.is_file():
        return None
    generation = pointer.read_text(encoding="utf-8").strip()
    directory = search / generation
    if not generation.startswith("generation-") or not directory.is_dir():
        return None
    return generation, directory


def search_files_contain(root: Path, needles: tuple[bytes, ...]) -> bool:
    found = [False] * len(needles)
    for path in root.rglob("*"):
        if not path.is_file() or path.is_symlink():
            continue
        contents = path.read_bytes()
        for index, needle in enumerate(needles):
            found[index] = found[index] or needle in contents
    return all(found)


def wait_for_plaintext_search_generation(
    workspace: Path,
    *,
    original_relative_path: str,
    markers: tuple[str, ...],
) -> str:
    observed: list[str] = []
    path_hex = original_relative_path.encode("utf-8").hex().encode("ascii")
    source = workspace / original_relative_path
    marker_bytes = tuple(marker.encode("utf-8") for marker in markers)

    def indexed() -> bool:
        if not source.is_file():
            return False
        source_contents = source.read_bytes()
        if not all(marker in source_contents for marker in marker_bytes):
            return False
        current = current_search_generation(workspace)
        if current is None:
            return False
        generation, directory = current
        if not search_files_contain(directory, (path_hex,)):
            return False
        observed[:] = [generation]
        return True

    wait_until(
        "published search generation containing the plaintext fixture",
        indexed,
        timeout=10.0,
        interval=0.03,
    )
    return observed[0]


def search_leaks(
    workspace: Path,
    *,
    original_relative_path: str,
    protected_relative_path: str | None,
    markers: tuple[str, ...],
) -> list[Path]:
    search = workspace / ".stillus" / "search"
    if not search.exists():
        return []
    del original_relative_path, protected_relative_path
    forbidden = [marker.encode("utf-8") for marker in markers]
    leaked: list[Path] = []
    for path in search.rglob("*"):
        if not path.is_file() or path.is_symlink():
            continue
        contents = path.read_bytes()
        if any(needle in contents for needle in forbidden):
            leaked.append(path.relative_to(workspace))
    return leaked


def wait_for_search_purge(
    workspace: Path,
    *,
    previous_generation: str,
    original_relative_path: str,
    protected_relative_path: str,
    markers: tuple[str, ...],
) -> None:
    def purged() -> bool:
        current = current_search_generation(workspace)
        return (
            current is not None
            and current[0] != previous_generation
            and not search_leaks(
                workspace,
                original_relative_path=original_relative_path,
                protected_relative_path=protected_relative_path,
                markers=markers,
            )
        )

    wait_until(
        "new published metadata-only search generation without protected body",
        purged,
        timeout=10.0,
        interval=0.03,
    )


def assert_search_excludes_protected(
    workspace: Path,
    *,
    original_relative_path: str,
    protected_relative_path: str,
    markers: tuple[str, ...],
) -> None:
    if current_search_generation(workspace) is None:
        raise AcceptanceFailure("protected-note search generation is missing")
    leaked = search_leaks(
        workspace,
        original_relative_path=original_relative_path,
        protected_relative_path=protected_relative_path,
        markers=markers,
    )
    if leaked:
        raise AcceptanceFailure(f"protected note body remains searchable: {leaked}")


def prove_plaintext_search_result(
    driver: WindowDriver,
    workspace: Path,
    *,
    plaintext_note: Path,
    query: str,
    proof_marker: str,
) -> None:
    decoy = workspace / "notes" / "ZZZZ Unrelated search decoy.md"
    if not decoy.is_file():
        raise AcceptanceFailure("plaintext search proof decoy is missing")
    wait_until(
        "search catalog containing the unrelated decoy",
        lambda: current_search_generation(workspace) is not None
        and search_files_contain(
            current_search_generation(workspace)[1],
            (decoy.relative_to(workspace).as_posix().encode().hex().encode("ascii"),),
        ),
        timeout=10.0,
        interval=0.03,
    )
    decoy_selection_marker = "securesearchdecoyselectionproof0024"
    driver.register_sensitive(decoy_selection_marker)
    driver.click_note(
        1,
        counts={"all": 2, "favorites": 0},
        categories=("decoy", "secure"),
    )
    driver.click("editor_below_document")
    driver.key("Return")
    driver.type_sensitive_text(decoy_selection_marker)
    wait_until(
        "explicit selection of the unrelated search decoy",
        lambda: contains(decoy, decoy_selection_marker),
        timeout=6.0,
        interval=0.03,
    )
    if contains(plaintext_note, decoy_selection_marker):
        raise AcceptanceFailure("decoy selection marker changed the target note")
    decoy_before = decoy.read_bytes()
    driver.key("ctrl+k")
    wait_for_search_visibility(driver, opened=True)
    driver.type_sensitive_text(query)
    driver.move_to("editor")
    wait_for_private_search_results(driver)
    driver.key("Return")
    wait_for_search_visibility(driver, opened=False)
    driver.wait_for_stable_frame("selected plaintext search result", crop=EDITOR_CROP)
    driver.click("editor_below_document")
    driver.key("Return")
    driver.type_sensitive_text(proof_marker)
    wait_until(
        "plaintext search result selection and canonical autosave",
        lambda: contains(plaintext_note, proof_marker),
        timeout=6.0,
        interval=0.03,
    )
    if decoy.read_bytes() != decoy_before:
        raise AcceptanceFailure("body search did not leave the proven decoy selection")
    decoy_path_hex = (
        decoy.relative_to(workspace).as_posix().encode().hex().encode("ascii")
    )
    decoy.unlink()

    def decoy_removed_from_catalog() -> bool:
        current = current_search_generation(workspace)
        return current is not None and not search_files_contain(
            current[1], (decoy_path_hex,)
        )

    wait_until(
        "search reconcile after removing the proof decoy",
        decoy_removed_from_catalog,
        timeout=10.0,
        interval=0.03,
    )


def wait_for_plaintext_purge(workspace: Path, *markers: str) -> None:
    try:
        wait_until(
            "protected plaintext purge from canonical, search, recovery, cache, temp and trash",
            lambda: not plaintext_leaks(workspace, *markers),
            timeout=10.0,
            interval=0.03,
        )
    except AcceptanceFailure as error:
        leaks = plaintext_leaks(workspace, *markers)
        marker_indexes = {
            path: [
                index
                for index, marker in enumerate(markers)
                if marker and marker.encode("utf-8") in (workspace / path).read_bytes()
            ]
            for path in leaks
        }
        raise AcceptanceFailure(
            f"{error}; remaining files and marker indexes: {marker_indexes}"
        ) from error


def protect_selected_note(
    driver: WindowDriver,
    workspace: Path,
    plaintext_note: Path,
    password: str,
    *,
    verify_password_caret: bool = False,
) -> Path:
    driver.wait_for_stable_frame("workspace toolbar before protection",
                                 crop=(256, 0, 984, 56), stable_for=0.2, timeout=10)
    driver.click("protection")
    driver.wait_for_password_dialog()
    if verify_password_caret:
        assert_masked_password_caret(
            driver,
            "empty setup password",
            "password_setup_primary",
            (105, 112, 121),
            before_text=True,
        )
    # The dialog focuses Primary automatically. Clicking an already focused
    # field queues another delayed focus request, which can steal focus back
    # after Enter/Tab on a busy runner.
    driver.wait_for_password_field_focus("password_setup_primary")
    driver.type_sensitive_text(f"{password}x")
    if verify_password_caret:
        assert_masked_password_caret(
            driver,
            "masked setup password",
            "password_setup_primary",
            (35, 39, 45),
            before_text=False,
        )
    driver.key("Return")
    driver.wait_for_password_field_focus("password_confirmation")
    if verify_password_caret:
        assert_masked_password_caret(
            driver,
            "empty password confirmation",
            "password_confirmation",
            (105, 112, 121),
            before_text=True,
        )
    driver.type_sensitive_text(password)
    driver.key("shift+Tab")
    driver.wait_for_password_field_focus("password_setup_primary")
    driver.key("BackSpace")
    driver.key("Tab")
    driver.wait_for_password_field_focus("password_confirmation")
    driver.key("Return")

    protected: list[Path] = []

    def completed() -> bool:
        protected[:] = protected_note_files(workspace)
        return plaintext_note.exists() and protected == [plaintext_note]

    # Protection queues a search-index purge, vault setup and the encryption
    # worker. Allow that whole operation to finish on shared CI runners while
    # still requiring the encrypted envelope at the original path.
    wait_until("selected note body protection at title-derived path", completed, timeout=30.0)
    return protected[0]


def unlock_selected_note(
    driver: WindowDriver, password: str, *, categories: tuple[str, ...] = ()
) -> None:
    # A workspace containing only protected notes has no auto-selected body.
    # Its public tags still contribute category rows before All notes.
    counts = {"all": 1, "favorites": 0, **{category: 1 for category in categories}}
    driver.click_note(0, counts=counts, categories=categories)
    driver.click("password_unlock_primary")
    driver.type_sensitive_text(password)
    driver.key("Return")


def lock_selected_note(driver: WindowDriver) -> None:
    driver.click("protection")
    driver.click("protection_lock")


def disable_selected_note_protection(driver: WindowDriver) -> None:
    driver.click("protection")
    driver.click("protection_disable")


def focus_tag_input(driver: WindowDriver, assigned: int, description: str) -> None:
    closed = driver.wait_for_stable_frame(
        f"stable editor before {description}", crop=TAG_POPOVER_CROP
    )
    driver.click("tag_manager")
    input_center = tag_input_center(assigned)
    driver.wait_for_visual_change(
        f"{description} opens",
        closed,
        crop=TAG_POPOVER_CROP,
        minimum_pixels=200,
    )
    driver.wait_for_stable_frame(
        f"{description} settles", crop=TAG_POPOVER_CROP, minimum_dark_pixels=100
    )
    driver.click_point(*input_center)
    assert_focused_input_caret(
        driver,
        f"{description} input",
        tag_input_crop(assigned),
        (105, 112, 121),
    )


def wait_for_file_bytes_stable(
    path: Path, description: str, *, stable_for: float = 0.8, timeout: float = 8.0
) -> bytes:
    latest = path.read_bytes()
    unchanged_since = time.monotonic()

    def stable() -> bool:
        nonlocal latest, unchanged_since
        current = path.read_bytes()
        if current != latest:
            latest = current
            unchanged_since = time.monotonic()
        return time.monotonic() - unchanged_since >= stable_for

    wait_until(description, stable, timeout=timeout, interval=0.05)
    return latest


def editor_clipboard_contains(driver: WindowDriver, marker: str) -> bool:
    sentinel = "stillus-empty-clipboard-sentinel"
    set_clipboard_text(driver.environment, sentinel)
    driver.click("editor")
    driver.key("ctrl+a")
    driver.key("ctrl+c")
    copied = clipboard_text(driver.environment)
    return copied is not None and marker in copied


def wait_for_unlocked_editor(driver: WindowDriver, marker: str) -> None:
    wait_until(
        "decrypted note contents after a successful unlock",
        lambda: editor_clipboard_contains(driver, marker),
        timeout=6.0,
        interval=0.03,
    )


def assert_locked_editor_inaccessible(
    driver: WindowDriver, *, markers: tuple[str, ...]
) -> None:
    sentinel = "stillus-locked-editor-clipboard-sentinel"
    set_clipboard_text(driver.environment, sentinel)
    driver.click("editor")
    driver.key("ctrl+a")
    driver.key("ctrl+c")
    copied = clipboard_text(driver.environment) or ""
    exposed = [marker for marker in markers if marker in copied]
    if exposed:
        raise AcceptanceFailure(
            f"locked note exposed {len(exposed)} registered plaintext marker(s) through editor"
        )


def assert_focused_lock_inaccessible(
    driver: WindowDriver, *, markers: tuple[str, ...]
) -> None:
    sentinel = "stillus-focused-lock-clipboard-sentinel"
    set_clipboard_text(driver.environment, sentinel)
    driver.key("ctrl+a")
    driver.key("ctrl+c")
    copied = clipboard_text(driver.environment) or ""
    exposed = [marker for marker in markers if marker in copied]
    if exposed:
        raise AcceptanceFailure(
            f"rejected password exposed {len(exposed)} registered plaintext marker(s)"
        )


def workspace_scenario(driver: WindowDriver, workspace: Path) -> None:
    del workspace
    downloads = driver.home / "Downloads"
    downloads.mkdir()
    default_workspace = downloads / "Notes"
    global_config = driver.home / ".stillus.cfg"
    driver.start_app(None, "first-run")
    driver.wait_for_stable_frame(
        "first-run workspace picker",
        crop=(330, 240, 580, 320),
        minimum_dark_pixels=100,
    )
    driver.key("Escape")
    if default_workspace.exists() or global_config.exists():
        raise AcceptanceFailure("first-run picker changed files before confirmation")
    driver.click("startup_primary")
    wait_until(
        "default first-run workspace creation",
        lambda: (default_workspace / "notes").is_dir()
        and global_config.is_file(),
    )
    saved_default = json.loads(global_config.read_text(encoding="utf-8"))
    if saved_default != {
        "version": 1,
        "locale": "en",
        "last_workspace": str(default_workspace.resolve()),
    }:
        raise AcceptanceFailure(
            f"unexpected first-run workspace config: {saved_default}"
        )
    driver.close_app()

    driver.start_app(None, "first-run-restart")
    first_note = default_workspace / "notes" / "New note.md"
    driver.click("create_menu")
    driver.click("create_note")
    wait_until("remembered first-run workspace without picker", first_note.is_file)
    driver.close_app()
    shutil.rmtree(default_workspace)
    global_config.unlink()

    default_workspace.mkdir()
    preserved = default_workspace / "keep.bin"
    preserved.write_bytes(b"preserve-existing-workspace-entry\x00")
    driver.start_app(None, "first-run-existing-folder")
    driver.click("startup_primary")
    wait_until(
        "notes initialization inside an existing selected folder",
        lambda: (default_workspace / "notes").is_dir()
        and global_config.is_file(),
    )
    if preserved.read_bytes() != b"preserve-existing-workspace-entry\x00":
        raise AcceptanceFailure("workspace initialization changed an existing entry")
    driver.close_app()
    shutil.rmtree(default_workspace)
    global_config.unlink()

    stale = driver.temporary_root / "stale-remembered-workspace"
    global_config.write_text(
        json.dumps({"version": 1, "last_workspace": str(stale.resolve())}),
        encoding="utf-8",
    )
    driver.start_app(None, "stale-remembered")
    driver.wait_for_stable_frame(
        "picker after unavailable remembered workspace",
        crop=(330, 240, 580, 320),
        minimum_dark_pixels=100,
    )
    if stale.exists():
        raise AcceptanceFailure("unavailable remembered workspace was recreated")
    driver.close_app()
    global_config.unlink()

    missing = driver.temporary_root / "missing-workspace"
    driver.start_app(missing, "missing")
    driver.wait_for_stable_frame("missing-workspace error shell")
    driver.close_app()
    if missing.exists():
        raise AcceptanceFailure("opening a missing workspace created host data")
    if (driver.home / ".stillus.cfg").exists():
        raise AcceptanceFailure("missing workspace replaced global config")

    invalid = driver.temporary_root / "invalid-workspace"
    invalid.mkdir()
    sentinel = invalid / "keep.bin"
    sentinel.write_bytes(b"invalid-workspace-sentinel\x00")
    driver.start_app(invalid, "invalid")
    driver.wait_for_stable_frame("invalid-workspace error shell")
    driver.close_app()
    if list(invalid.iterdir()) != [sentinel] or sentinel.read_bytes() != b"invalid-workspace-sentinel\x00":
        raise AcceptanceFailure("invalid workspace launch changed existing data")
    if (driver.home / ".stillus.cfg").exists():
        raise AcceptanceFailure("invalid workspace replaced global config")

    empty = create_workspace(driver.temporary_root, "empty-workspace")
    driver.start_app(empty, "empty")
    created = empty / "notes" / "New note.md"
    driver.click("create_menu")
    driver.click("create_note")
    wait_until("first note in an empty workspace", created.is_file)
    driver.wait_for_stable_frame("new note selection in an empty workspace")
    marker = "created-in-empty-workspace"
    driver.type_text(marker)
    created = empty / "notes" / f"{marker}.md"
    wait_until(
        "empty-workspace note autosave",
        lambda: created.is_file() and note_body(created) == f"# {marker}",
    )
    driver.close_app()
    saved_global = json.loads(global_config.read_text(encoding="utf-8"))
    if saved_global != {
        "version": 1,
        "locale": "en",
        "last_workspace": str(empty.resolve()),
    }:
        raise AcceptanceFailure(f"unexpected global workspace config: {saved_global}")

    driver.start_app(None, "remembered-workspace")
    driver.click("editor_below_document")
    driver.key("ctrl+End")
    driver.key("Return")
    remembered_marker = "opened-from-global-config"
    driver.type_text(remembered_marker)
    wait_until(
        "launch without a path reopens the remembered workspace",
        lambda: contains(created, remembered_marker),
    )
    runtime_target = create_workspace(driver.temporary_root, "runtime-target")
    runtime_note = runtime_target / "notes" / "Runtime Target.md"
    runtime_note.write_text("# Runtime Target\n", encoding="utf-8")
    empty_before_switch = {
        path: path.read_bytes() for path in (empty / "notes").glob("*.md")
    }
    driver.click("settings")
    driver.wait_for_stable_frame(
        "settings page with General tab",
        crop=(0, 0, SCREEN_WIDTH, SCREEN_HEIGHT),
        minimum_dark_pixels=100,
    )
    driver.click("settings_path")
    driver.key("ctrl+a")
    driver.type_text(str(runtime_target.resolve()))
    driver.click("settings_apply")
    wait_until(
        "runtime workspace updates global config",
        lambda: json.loads(global_config.read_text(encoding="utf-8"))
        .get("last_workspace")
        == str(runtime_target.resolve()),
    )
    driver.key("Escape")
    driver.click("editor_below_document")
    driver.key("ctrl+End")
    driver.key("Return")
    runtime_marker = "runtime-workspace-switch"
    driver.type_text(runtime_marker)
    wait_until(
        "runtime workspace editor targets the new folder",
        lambda: contains(runtime_note, runtime_marker),
    )
    if any(path.read_bytes() != contents for path, contents in empty_before_switch.items()):
        raise AcceptanceFailure("runtime workspace switch changed old canonical notes")
    driver.close_app()
    created_text = read_text(created)
    required = [
        "favorited: false",
        "pinned: false",
        "tags: []",
        f"title: '{marker}'",
        "created:",
        "modified:",
        remembered_marker,
    ]
    missing_fields = [field for field in required if field not in created_text]
    if missing_fields:
        raise AcceptanceFailure(
            f"empty-workspace note is not Notable-compatible: {missing_fields}"
        )
    driver.start_app(None, "runtime-workspace-restart")
    driver.click("editor_below_document")
    driver.key("ctrl+End")
    restart_marker = "runtime-workspace-restart"
    driver.type_text(restart_marker)
    wait_until(
        "restart reopens the runtime-selected workspace",
        lambda: contains(runtime_note, restart_marker),
    )
    driver.close_app()
    assert_no_temporary_files(empty)
    assert_no_temporary_files(runtime_target)


def compatibility_scenario(driver: WindowDriver, workspace: Path) -> None:
    del workspace
    compatible = create_workspace(driver.temporary_root, "compatibility-workspace")
    notes = compatible / "notes"
    plain = notes / "A Plain.md"
    notable = notes / "B Notable.md"
    broken = notes / "C Broken.md"
    plain.write_text("# Plain\n\nplain-body\n", encoding="utf-8")
    notable.write_text(
        "---\n"
        "favorited: true\n"
        "pinned: false\n"
        "tags: [Личное, Work]\n"
        "title: B Notable\n"
        "created: '2022-02-03T18:57:43.598Z'\n"
        "modified: '2026-08-31T18:52:01.046Z'\n"
        "future_key: keep-me\n"
        "nested_unknown:\n"
        "  answer: 42\n"
        "---\n"
        "Привет compatibility\n",
        encoding="utf-8",
    )
    broken.write_text(
        "---\ntitle: Broken\ntags: [unterminated\n---\nDO NOT TOUCH BROKEN BODY\n",
        encoding="utf-8",
    )
    attachments = compatible / "attachments"
    attachments.mkdir()
    attachment = attachments / "opaque.bin"
    attachment.write_bytes(b"opaque-attachment\x00\xff")
    outside = driver.temporary_root / "outside.md"
    outside.write_bytes(b"outside-symlink-target\n")
    link = notes / "D Link.md"
    link.symlink_to(outside)

    before = {
        plain: plain.read_bytes(),
        notable: notable.read_bytes(),
        broken: broken.read_bytes(),
        attachment: attachment.read_bytes(),
        outside: outside.read_bytes(),
    }
    compatibility_categories = ("Work", "Личное")
    compatibility_counts = {
        "all": 3,
        "favorites": 1,
        "Work": 1,
        "Личное": 1,
    }
    driver.start_app(compatible, "scan-only")
    driver.wait_for_stable_frame("compatibility scan-only shell")
    driver.close_app()
    changed_on_scan = [path for path, contents in before.items() if path.read_bytes() != contents]
    if changed_on_scan:
        raise AcceptanceFailure(f"workspace scan rewrote files: {changed_on_scan}")

    driver.start_app(compatible, "interactive")
    driver.click("editor")
    plain_marker = "plain-click-edit"
    driver.key("ctrl+End")
    driver.key("Return")
    driver.type_text(plain_marker)
    relocated_plain = notes / "Plain.md"
    wait_until(
        "plain Markdown autosave and first-line relocation",
        lambda: contains(relocated_plain, plain_marker) and not plain.exists(),
    )
    time.sleep(0.2)

    # Relocating `A Plain.md` reorders the path-sorted rows to Broken, Plain,
    # Notable. Select the Notable body projection at its new index.
    driver.click_note(
        2, counts=compatibility_counts, categories=compatibility_categories
    )
    driver.click("editor")
    driver.key("ctrl+a")
    driver.key("ctrl+c")
    driver.key("Right")
    driver.key("Return")
    driver.key("ctrl+v")
    notable_marker = "unknown-metadata-edit"
    driver.type_text(notable_marker)
    relocated_notable = notes / "Привет compatibility.md"
    wait_until(
        "Notable-compatible clipboard/edit autosave and first-line relocation",
        lambda: contains(relocated_notable, notable_marker)
        and note_body(relocated_notable).count("Привет") == 2
        and not notable.exists(),
    )
    time.sleep(0.2)
    notable = relocated_notable

    driver.click_note(
        0, counts=compatibility_counts, categories=compatibility_categories
    )
    driver.click("editor")
    invalid_marker = "edit-remains-on-ready-note"
    driver.type_text(invalid_marker)
    final_notable = notes / f"{invalid_marker}Привет compatibility.md"
    wait_until(
        "invalid note click isolation and ready-note relocation",
        lambda: contains(final_notable, invalid_marker) and not notable.exists(),
    )
    notable = final_notable
    driver.close_app()

    if broken.read_bytes() != before[broken]:
        raise AcceptanceFailure("malformed front matter was modified through the UI")
    if attachment.read_bytes() != before[attachment] or outside.read_bytes() != before[outside]:
        raise AcceptanceFailure("unknown attachment or symlink target was modified")
    if not link.is_symlink():
        raise AcceptanceFailure("note symlink was replaced or followed")
    notable_text = read_text(notable)
    required_preserved = [
        "created: '2022-02-03T18:57:43.598Z'",
        "future_key: keep-me",
        "nested_unknown:\n  answer: 42",
        "favorited: true",
        "tags: [Личное, Work]",
    ]
    missing_preserved = [value for value in required_preserved if value not in notable_text]
    if missing_preserved:
        raise AcceptanceFailure(f"Notable metadata was not preserved: {missing_preserved}")
    assert_no_temporary_files(compatible)


def lifecycle_scenario(driver: WindowDriver, workspace: Path) -> None:
    notes = workspace / "notes"
    project = notes / "Project Alpha.md"
    reading = notes / "Reading List.md"
    weekly = notes / "Weekly Notes.md"
    project_before = project.read_bytes()
    weekly_before = weekly.read_bytes()

    driver.start_app(workspace, "lifecycle")
    driver.click("reading_note")
    driver.click("editor")
    driver.key("ctrl+End")
    driver.key("Return")
    selection_marker = "selected-by-click"
    driver.type_text(selection_marker)
    wait_until(
        "the clicked Reading List note to autosave",
        lambda: contains(reading, selection_marker),
    )
    time.sleep(0.2)
    if project.read_bytes() != project_before or weekly.read_bytes() != weekly_before:
        raise AcceptanceFailure("selecting Reading List modified an untouched note")

    default_note = notes / "New note.md"
    second_note = notes / "New note (2).md"
    driver.click("create_menu")
    driver.click("create_note")
    wait_until("default note creation", default_note.is_file)
    driver.click("create_menu")
    driver.click("create_note")
    wait_until("collision-free second note creation", second_note.is_file)

    # Create focuses the editor and selects only the text after `# `.
    driver.type_text("Acceptance Note")
    accepted_note = notes / "Acceptance Note.md"
    wait_until(
        "first-line title autosave",
        lambda: accepted_note.is_file() and not second_note.exists(),
    )

    driver.click("editor")
    driver.key("ctrl+End")
    driver.key("Return")
    body_marker = "click-body"
    driver.type_text(body_marker)
    driver.key("ctrl+z")
    driver.key("ctrl+shift+z")
    driver.key("ctrl+a")
    driver.key("ctrl+c")
    driver.key("Right")
    driver.key("Return")
    driver.key("ctrl+v")
    wait_until(
        "editor undo/redo/copy/paste autosave",
        lambda: accepted_note.is_file()
        and read_text(accepted_note).count(body_marker) == 2,
    )
    time.sleep(0.2)

    metadata_before = accepted_note.read_bytes()
    focus_tag_input(driver, 1, "lifecycle tag popover")
    driver.key("Return")
    if accepted_note.read_bytes() != metadata_before:
        raise AcceptanceFailure("empty tag action rewrote the note")

    driver.type_text("x" * 129)
    driver.key("Return")
    if accepted_note.read_bytes() != metadata_before:
        raise AcceptanceFailure("oversized tag action rewrote the note")

    # A rejected submit retains focus in the current input; replace its query
    # in place so no stale popover node can win the next keyboard event.
    driver.key("ctrl+a")
    driver.type_text("Acceptance")
    driver.key("Return")
    wait_until("tag addition", lambda: "  - 'Acceptance'" in read_text(accepted_note))

    # Only the row cross removes the tag. A successful mutation clears the
    # query and restores focus to the current popover input.
    driver.click("tag_remove_first")
    wait_until("tag removal", lambda: "  - 'Acceptance'" not in read_text(accepted_note))

    # A successful removal restores focus to the current input node, so the
    # retained freeform tag is submitted without relying on the rebuilt node
    # that made the earlier lifecycle sequence flaky under Xvfb.
    driver.type_text("Keep")
    driver.key("Return")
    wait_until("retained tag addition", lambda: "  - 'Keep'" in read_text(accepted_note))

    driver.click("pin")
    wait_until("pinned metadata", lambda: "pinned: true" in read_text(accepted_note))
    driver.click("pin")
    wait_until("unpinned metadata", lambda: "pinned: false" in read_text(accepted_note))
    driver.click("pin")
    wait_until("repinned metadata", lambda: "pinned: true" in read_text(accepted_note))
    driver.click("favorite")
    wait_until(
        "favorited metadata", lambda: "favorited: true" in read_text(accepted_note)
    )
    driver.click("favorite")
    wait_until(
        "unfavorited metadata", lambda: "favorited: false" in read_text(accepted_note)
    )
    driver.click("favorite")
    wait_until(
        "refavorited metadata", lambda: "favorited: true" in read_text(accepted_note)
    )

    driver.click("trash")
    wait_until(
        "canonical soft delete",
        lambda: accepted_note.is_file() and "deleted: true" in read_text(accepted_note),
    )
    driver.close_app()

    restore_categories = DEMO_CATEGORIES
    restore_counts = {
        "all": 4,
        "favorites": 1,
        "Personal": 1,
        "Planning": 1,
        "Reading": 1,
        "Work": 2,
        "trash": 1,
    }
    driver.start_app(workspace, "lifecycle-restore")
    driver.click_point(
        *group_row_center(
            "trash",
            expanded_groups=("all",),
            counts=restore_counts,
            categories=restore_categories,
        )
    )
    driver.click_note(
        0,
        expanded="trash",
        expanded_groups=("all", "trash"),
        counts=restore_counts,
        categories=restore_categories,
    )
    driver.click("trash")
    wait_until(
        "restore clears canonical deleted flag",
        lambda: accepted_note.is_file() and "deleted: false" in read_text(accepted_note),
    )
    driver.close_app()

    trashed_text = read_text(accepted_note)
    required = ["title: 'Acceptance Note'", "  - 'Keep'", "pinned: true", "favorited: true"]
    missing = [value for value in required if value not in trashed_text]
    if missing or trashed_text.count(body_marker) != 2:
        raise AcceptanceFailure(f"trashed note is missing acceptance state: {missing}")
    if "  - 'Acceptance'" in trashed_text:
        raise AcceptanceFailure("removed tag remained in the trashed note")
    if read_text(reading).count(selection_marker) != 1:
        raise AcceptanceFailure("clicked note did not receive exactly one selection marker")
    if project.read_bytes() != project_before or weekly.read_bytes() != weekly_before:
        raise AcceptanceFailure("an untouched note changed during lifecycle acceptance")
    if not default_note.exists() or second_note.exists():
        raise AcceptanceFailure("create/title-derived paths are inconsistent after lifecycle")
    assert_no_temporary_files(workspace)


def tags_scenario(driver: WindowDriver, workspace: Path) -> None:
    del workspace
    # Notes sort by their first-line title, so the target note is listed and
    # opened first under `Все` at startup.
    tags_workspace = create_workspace(driver.temporary_root, "tags-workspace")
    notes = tags_workspace / "notes"
    target = notes / "A Target.md"
    other = notes / "B Other.md"
    write_tagged_note(target, "A Target", ["Work", "Planning"])
    write_tagged_note(other, "B Other", ["Perennial", "Personal", "Project"])
    categories = ("Perennial", "Personal", "Planning", "Project", "Work")
    counts = {
        "all": 2,
        "favorites": 0,
        "Perennial": 1,
        "Personal": 1,
        "Planning": 1,
        "Project": 1,
        "Work": 1,
    }
    popover_crop = TAG_POPOVER_CROP

    driver.start_app(tags_workspace, "tags")
    driver.move_to("editor_below_document")
    closed = driver.wait_for_stable_frame("stable tag popover closed state")
    legacy_chrome_pixels = dark_pixel_count(closed, crop=(344, 58, 450, 18))
    if legacy_chrome_pixels > 5:
        raise AcceptanceFailure(
            "legacy persistent tag chips or tag bar remain below the 56px header: "
            f"{legacy_chrome_pixels} dark pixels"
        )

    driver.click("tag_manager")
    driver.move_to("tag_input")
    driver.wait_for_visual_change(
        "tag popover opening transition",
        closed,
        crop=popover_crop,
        minimum_pixels=200,
    )
    opened = driver.wait_for_stable_frame(
        "stable opened tag popover", crop=popover_crop, minimum_dark_pixels=100
    )
    assert_focused_input_caret(
        driver,
        "tag input",
        tag_input_crop(2),
        (105, 112, 121),
    )
    open_difference = image_difference(closed, opened, crop=popover_crop)
    if open_difference < 200:
        raise AcceptanceFailure(
            "tag icon did not open the anchored popover "
            f"({open_difference} changed pixels; closed/open dark pixels: "
            f"{dark_pixel_count(closed, crop=popover_crop)}/"
            f"{dark_pixel_count(opened, crop=popover_crop)})"
        )

    driver.click("tag_manager")
    driver.move_to("editor_below_document")
    driver.wait_for_visual_change(
        "tag popover icon-toggle closing transition",
        opened,
        crop=popover_crop,
        minimum_pixels=200,
    )
    toggled_closed = driver.wait_for_stable_frame(
        "stable tag popover after icon toggle", crop=popover_crop
    )
    if image_difference(closed, toggled_closed, crop=popover_crop) > 500:
        raise AcceptanceFailure("second tag-icon click did not close the popover")

    driver.click("tag_manager")
    driver.key("Escape")
    driver.key("Return")
    driver.wait_for_visual_change(
        "keyboard tag popover reopening transition",
        toggled_closed,
        crop=popover_crop,
        minimum_pixels=200,
    )
    escape_reopened = driver.wait_for_stable_frame(
        "Escape focus returned to the tag icon", crop=popover_crop
    )
    escape_difference = image_difference(closed, escape_reopened, crop=popover_crop)
    if escape_difference < 200:
        raise AcceptanceFailure(
            "Escape did not return keyboard focus to the tag icon "
            f"({escape_difference} changed pixels)"
        )
    driver.click("sidebar_blank")
    driver.wait_for_visual_change(
        "outside tag popover closing transition",
        escape_reopened,
        crop=popover_crop,
        minimum_pixels=200,
    )
    outside_closed = driver.wait_for_stable_frame(
        "outside click closed the tag popover", crop=popover_crop
    )
    if image_difference(closed, outside_closed, crop=popover_crop) > 500:
        raise AcceptanceFailure("outside click did not dismiss the tag popover")

    work_x, work_y = group_row_center(
        "Work", expanded="all", counts=counts, categories=categories
    )
    driver.click_point(work_x, work_y)
    driver.click("tag_manager")

    # ArrowUp begins at the last prefix match, while ArrowDown begins at the
    # first remaining match. Both submit the canonical workspace spelling.
    driver.type_text("per")
    driver.key("Up")
    driver.key("Return")
    wait_until(
        "ArrowUp autocomplete tag addition",
        lambda: "  - 'Personal'" in read_text(target),
    )

    driver.type_text("pro")
    driver.click("tag_suggestion_after_three")
    wait_until(
        "mouse autocomplete tag addition",
        lambda: "  - 'Project'" in read_text(target),
    )

    driver.type_text("per")
    driver.key("Down")
    driver.key("Return")
    wait_until(
        "ArrowDown autocomplete tag addition",
        lambda: "  - 'Perennial'" in read_text(target),
    )

    driver.type_text("Acceptance")
    driver.key("Return")
    wait_until(
        "freeform tag addition",
        lambda: "  - 'Acceptance'" in read_text(target),
    )

    duplicate_before = target.read_bytes()
    driver.type_text("Acceptance")
    driver.key("Return")
    time.sleep(0.25)
    if target.read_bytes() != duplicate_before:
        raise AcceptanceFailure("duplicate popover submit rewrote the canonical note")

    driver.key("ctrl+a")
    driver.type_text("x" * 129)
    driver.key("Return")
    time.sleep(0.25)
    if target.read_bytes() != duplicate_before:
        raise AcceptanceFailure("invalid popover submit rewrote the canonical note")

    driver.click("tag_assigned_first")
    if target.read_bytes() != duplicate_before or "  - 'Work'" not in read_text(target):
        raise AcceptanceFailure("clicking an assigned tag label removed or rewrote it")

    driver.move_to("editor_below_document")
    cross_crop = tag_remove_crop(0)
    cross_hidden = driver.wait_for_stable_frame(
        "stable hidden tag removal cross", crop=cross_crop
    )
    driver.move_to("tag_remove_first")
    cross_visible = driver.wait_for_visual_change(
        "tag removal cross appears on hover",
        cross_hidden,
        crop=cross_crop,
        minimum_pixels=3,
    )
    if image_difference(cross_hidden, cross_visible, crop=cross_crop) < 3:
        raise AcceptanceFailure("assigned tag hover did not reveal its removal cross")
    driver.click("tag_remove_first")
    wait_until("cross-only tag removal", lambda: "  - 'Work'" not in read_text(target))

    # The last Work category disappearing removes its expanded group, so only
    # `Все` still lists notes. The popover itself remains open and
    # restores input focus after removal.
    driver.type_text("Keep")
    driver.key("Return")
    wait_until(
        "popover remains open after tag removal",
        lambda: "  - 'Keep'" in read_text(target),
    )
    final_categories = ("Acceptance", "Keep", "Perennial", "Personal", "Planning", "Project")
    driver.click_note(1, expanded="all", counts=counts, categories=final_categories)
    driver.click("editor")
    # The marker goes below the title line: editing the first line would
    # rename the note and relocate its file.
    driver.key("ctrl+End")
    driver.key("Return")
    driver.type_text("active-filter-reset")
    wait_until(
        "removed active category resets to All notes",
        lambda: contains(other, "active-filter-reset"),
    )
    if contains(target, "active-filter-reset"):
        raise AcceptanceFailure("active tag reset kept editing the previous note")

    driver.close_app()
    assert_no_temporary_files(tags_workspace)

    # A note with more tags than the list shows keeps the popover geometry:
    # the scrollbar stays inside the card padding, rows and the footer input
    # share one right edge, and the footer keeps its full bottom padding.
    layout_workspace = create_workspace(driver.temporary_root, "tags-layout-workspace")
    layout_tags = [f"Layout tag {index:02d}" for index in range(1, 13)]
    layout_tags[2] = "Layout tag 03 with a name long enough to end in an ellipsis"
    write_tagged_note(layout_workspace / "notes" / "A Layout.md", "A Layout", layout_tags)
    driver.start_app(layout_workspace, "tags-layout")
    driver.move_to("editor_below_document")
    layout_closed = driver.wait_for_stable_frame("stable closed popover before layout check")
    driver.click("tag_manager")
    driver.move_to("editor_below_document")
    driver.wait_for_visual_change(
        "overflowing tag popover opening transition",
        layout_closed,
        crop=popover_crop,
        minimum_pixels=200,
    )
    overflowing = driver.wait_for_stable_frame(
        "stable overflowing tag popover", crop=popover_crop, minimum_dark_pixels=100
    )
    assert_tag_popover_overflow_layout(overflowing)
    driver.move_to("tag_assigned_first")
    driver.wait_for_visual_change(
        "hover highlight on the first overflowing row",
        overflowing,
        crop=popover_crop,
        minimum_pixels=200,
    )
    hovered = driver.wait_for_stable_frame(
        "stable hovered overflowing row", crop=popover_crop
    )
    assert_tag_row_layout(hovered, index=0)
    driver.key("Escape")
    driver.close_app()
    assert_no_temporary_files(layout_workspace)


def assert_tag_popover_overflow_layout(frame: Path) -> None:
    """Check the scrollbar gutter and the footer of a popover at its list limit."""
    band = (
        TAG_POPOVER_LEFT + 1,
        TAG_POPOVER_CONTENT_TOP,
        TAG_POPOVER_WIDTH - 2,
        TAG_POPOVER_LIST_MAX_HEIGHT,
    )
    coverage = shaded_column_coverage(frame, band, max_luminance=215.0)
    handle_columns = sorted(
        x for x, rows in coverage.items() if rows * 2 >= TAG_POPOVER_LIST_MAX_HEIGHT
    )
    over_rows = [x for x in handle_columns if x < TAG_POPOVER_CONTENT_RIGHT]
    if over_rows:
        raise AcceptanceFailure(
            "tag popover scrollbar is painted over the rows at columns "
            f"{over_rows[0]}..{over_rows[-1]} (rows end at {TAG_POPOVER_CONTENT_RIGHT})"
        )
    if not handle_columns:
        raise AcceptanceFailure(
            "overflowing tag list painted no scrollbar inside the right card padding"
        )

    # One-pixel lines land on half-pixel positions and blend into two rows,
    # so every line is located by the center of its run. The card border and
    # the divider blend to roughly 242 while the input surface stays at 247.
    runs = shaded_row_runs(
        frame,
        x=TAG_POPOVER_FOOTER_X,
        y=TAG_POPOVER_TOP - 2,
        height=TAG_POPOVER_CROP[1] + TAG_POPOVER_CROP[3] - TAG_POPOVER_TOP + 2,
        max_luminance=TAG_POPOVER_LINE_LUMINANCE,
    )
    if len(runs) < 4:
        raise AcceptanceFailure(f"tag popover footer lines are missing: {runs}")
    divider, input_top, input_bottom, card_bottom = (
        (start + end) / 2 for start, end in runs[-4:]
    )
    footer_gap = input_top - divider - 1
    input_height = input_bottom - input_top + 1
    bottom_padding = card_bottom - input_bottom - 1
    if abs(footer_gap - TAG_POPOVER_SECTION_GAP) > 1:
        raise AcceptanceFailure(
            f"footer divider to input gap is {footer_gap}px, expected "
            f"{TAG_POPOVER_SECTION_GAP}px: {runs}"
        )
    if abs(input_height - TAG_POPOVER_ROW_HEIGHT) > 1:
        raise AcceptanceFailure(
            f"footer input is {input_height}px tall, expected {TAG_POPOVER_ROW_HEIGHT}px: {runs}"
        )
    if abs(bottom_padding - TAG_POPOVER_PADDING) > 1:
        raise AcceptanceFailure(
            f"overflowing tag popover keeps {bottom_padding}px below the input, "
            f"expected {TAG_POPOVER_PADDING}px: {runs}"
        )


def assert_tag_row_layout(frame: Path, *, index: int) -> None:
    """Check a hovered row of a popover at its list limit against the footer."""
    row_band = (
        TAG_POPOVER_CONTENT_LEFT,
        tag_row_top(index),
        TAG_POPOVER_CONTENT_RIGHT - TAG_POPOVER_CONTENT_LEFT + TAG_POPOVER_PADDING,
        TAG_POPOVER_ROW_HEIGHT,
    )
    highlight = column_profile(frame, row_band)["overlay"]
    if not highlight:
        raise AcceptanceFailure(f"hovered tag row {index} painted no highlight")
    input_top = tag_section_top(0) + TAG_POPOVER_LIST_MAX_HEIGHT
    input_band = (
        TAG_POPOVER_CONTENT_LEFT,
        input_top,
        row_band[2],
        TAG_POPOVER_ROW_HEIGHT,
    )
    # The rounded input corners shorten its vertical borders, so a border
    # column only has to cover half of the input height.
    input_coverage = shaded_column_coverage(
        frame, input_band, max_luminance=TAG_POPOVER_LINE_LUMINANCE
    )
    input_edges = [
        x for x, rows in input_coverage.items() if rows >= TAG_POPOVER_ROW_HEIGHT // 2
    ]
    if not input_edges:
        raise AcceptanceFailure("footer input borders were not found below the list")
    expected_right = TAG_POPOVER_CONTENT_RIGHT - 1
    if abs(max(highlight) - expected_right) > 1 or abs(max(input_edges) - expected_right) > 1:
        raise AcceptanceFailure(
            f"row highlight ends at {max(highlight)} and the footer input at "
            f"{max(input_edges)}, expected both at {expected_right}"
        )

    cross_crop = tag_remove_crop(index)
    cross_coverage = shaded_column_coverage(frame, cross_crop, max_luminance=215.0)
    covered = [x for x, rows in cross_coverage.items() if rows >= cross_crop[3]]
    if covered:
        raise AcceptanceFailure(
            f"removal cross of row {index} is covered at columns {covered[0]}..{covered[-1]}"
        )
    if dark_pixel_count(frame, crop=cross_crop) < 6:
        raise AcceptanceFailure(f"removal cross of hovered row {index} is not visible")


def categories_scenario(driver: WindowDriver, workspace: Path) -> None:
    notes = workspace / "notes"
    project = notes / "Project Alpha.md"
    reading = notes / "Reading List.md"
    weekly = notes / "Weekly Notes.md"
    initial = {path: path.read_bytes() for path in (project, reading, weekly)}
    sidebar_crop = (0, 0, SIDEBAR_WIDTH, 420)

    driver.start_app(workspace, "categories")
    clean = driver.wait_for_stable_frame("stable initial category tree")
    reference_title_pixels = min(
        bright_pixel_count(clean, crop=note_title_crop(index)) for index in range(3)
    )
    if reference_title_pixels < 20:
        raise AcceptanceFailure(
            f"clean note titles are too faint for the tree oracle: {reference_title_pixels}"
        )
    # Hierarchy fixtures deliberately use short labels such as "A Leaf". Keep
    # the presence oracle relative to the pinned renderer without requiring a
    # short valid title to paint as many pixels as the longer demo titles.
    minimum_title_pixels = max(20, reference_title_pixels // 3)

    def ordered(
        groups: set[str],
        categories: tuple[str, ...],
        category_order: tuple[str, ...] | None = None,
    ) -> tuple[str, ...]:
        return tuple(
            group
            for group in sidebar_group_paths(categories, category_order)
            if group in groups
        )

    def assert_tree(
        frame: Path,
        expanded_groups: set[str],
        *,
        counts: dict[str, int] = DEMO_NOTE_COUNTS,
        direct_counts: dict[str, int] | None = None,
        categories: tuple[str, ...] = DEMO_CATEGORIES,
        category_order: tuple[str, ...] | None = None,
    ) -> None:
        open_groups = ordered(expanded_groups, categories, category_order)
        problems = []
        rows = sidebar_visible_rows(
            expanded_groups=open_groups,
            counts=counts,
            direct_counts=direct_counts,
            categories=categories,
            category_order=category_order,
        )
        for kind, group, index, depth, top in rows:
            if kind == "group":
                group_chevron = bright_pixel_count(
                    frame,
                    crop=chevron_crop(top, depth=depth),
                    threshold=CHEVRON_THRESHOLD,
                )
                if group_chevron <= 4:
                    problems.append(
                        f"group {group} has no chevron ({group_chevron})"
                    )
            else:
                title = bright_pixel_count(
                    frame,
                    crop=note_title_crop(
                        index,
                        depth=depth,
                        expanded=group,
                        expanded_groups=open_groups,
                        counts=counts,
                        direct_counts=direct_counts,
                        categories=categories,
                        category_order=category_order,
                    ),
                )
                chevron = bright_pixel_count(
                    frame,
                    crop=chevron_crop(top, depth=depth),
                    threshold=CHEVRON_THRESHOLD,
                )
                if title < minimum_title_pixels:
                    problems.append(
                        f"{group} row {index} title {title} < {minimum_title_pixels}"
                    )
                if chevron > 4:
                    problems.append(
                        f"{group} row {index} carries a group chevron ({chevron})"
                    )
        if problems:
            raise AcceptanceFailure(f"unexpected sidebar tree: {problems}")

    def wait_for_tree(
        description: str,
        expanded_groups: set[str],
        *,
        counts: dict[str, int] = DEMO_NOTE_COUNTS,
        direct_counts: dict[str, int] | None = None,
        categories: tuple[str, ...] = DEMO_CATEGORIES,
        category_order: tuple[str, ...] | None = None,
    ) -> Path:
        frame = driver.capture("tree-pending")

        def expected_tree_is_visible() -> bool:
            nonlocal frame
            frame = driver.capture("tree-poll")
            try:
                assert_tree(
                    frame,
                    expanded_groups,
                    counts=counts,
                    direct_counts=direct_counts,
                    categories=categories,
                    category_order=category_order,
                )
            except AcceptanceFailure:
                return False
            return True

        wait_until(description, expected_tree_is_visible, interval=0.05)
        stable = driver.wait_for_stable_frame(description, crop=sidebar_crop)
        assert_tree(
            stable,
            expanded_groups,
            counts=counts,
            direct_counts=direct_counts,
            categories=categories,
            category_order=category_order,
        )
        return stable

    def click_group(
        group: str,
        expanded_groups: set[str],
        *,
        counts: dict[str, int] = DEMO_NOTE_COUNTS,
        direct_counts: dict[str, int] | None = None,
        categories: tuple[str, ...] = DEMO_CATEGORIES,
        category_order: tuple[str, ...] | None = None,
    ) -> None:
        open_groups = ordered(expanded_groups, categories, category_order)
        x, y = group_row_center(
            group,
            expanded_groups=open_groups,
            counts=counts,
            direct_counts=direct_counts,
            categories=categories,
            category_order=category_order,
        )
        driver.click_point(x, y)
        if group in expanded_groups:
            expanded_groups.remove(group)
        else:
            expanded_groups.add(group)
        driver.click("sidebar_blank")

    expanded_groups = {"all"}
    assert_tree(clean, expanded_groups)

    click_group("favorites", expanded_groups)
    wait_for_tree("Favorites and All notes remain expanded", expanded_groups)
    if any(path.read_bytes() != contents for path, contents in initial.items()):
        raise AcceptanceFailure("expanding Favorites rewrote canonical notes")

    click_group("favorites", expanded_groups)
    wait_for_tree("Favorites closes without collapsing All notes", expanded_groups)

    click_group("Personal", expanded_groups)
    wait_for_tree("Personal and All notes remain expanded", expanded_groups)
    driver.click_note(
        0,
        expanded="Personal",
        expanded_groups=ordered(expanded_groups, DEMO_CATEGORIES),
    )
    driver.click("editor")
    driver.key("ctrl+End")
    driver.key("Return")
    personal_marker = "personal-filter-click"
    driver.type_text(personal_marker)
    wait_until("Personal note edit", lambda: contains(reading, personal_marker))
    if project.read_bytes() != initial[project] or weekly.read_bytes() != initial[weekly]:
        raise AcceptanceFailure("Personal group selected a note outside the tag")

    reading_after_edit = reading.read_bytes()
    click_group("Work", expanded_groups)
    wait_for_tree("Personal, Work and All notes remain expanded", expanded_groups)
    if (
        project.read_bytes() != initial[project]
        or reading.read_bytes() != reading_after_edit
        or weekly.read_bytes() != initial[weekly]
    ):
        raise AcceptanceFailure("expanding Work rewrote a note")

    driver.click_note(
        1,
        expanded="Work",
        expanded_groups=ordered(expanded_groups, DEMO_CATEGORIES),
    )
    driver.click("editor")
    driver.key("ctrl+End")
    driver.key("Return")
    work_marker = "work-filter-click"
    driver.type_text(work_marker)
    wait_until("second Work note edit", lambda: contains(weekly, work_marker))
    if project.read_bytes() != initial[project] or reading.read_bytes() != reading_after_edit:
        raise AcceptanceFailure("Work group selected the wrong note")

    click_group("Work", expanded_groups)
    wait_for_tree("Work closes while Personal and All remain expanded", expanded_groups)
    driver.click("editor")
    driver.key("ctrl+End")
    driver.key("Return")
    closed_group_marker = "selected-note-survives-group-close"
    driver.type_text(closed_group_marker)
    wait_until(
        "selected note remains open after its group closes",
        lambda: contains(weekly, closed_group_marker),
    )

    click_group("Personal", expanded_groups)
    wait_for_tree("only All notes remains expanded", expanded_groups)
    click_group("all", expanded_groups)
    wait_for_tree("all sidebar groups can be collapsed", expanded_groups)
    click_group("all", expanded_groups)
    wait_for_tree("All notes reopens independently", expanded_groups)

    driver.close_app()
    if (
        project.read_bytes() != initial[project]
        or reading.read_bytes() != reading_after_edit
        or not contains(weekly, work_marker)
        or not contains(weekly, closed_group_marker)
    ):
        raise AcceptanceFailure("category navigation changed notes beyond explicit input")
    assert_no_temporary_files(workspace)

    reorder_workspace = generate_demo_workspace(
        driver.temporary_root / "category-reorder-workspace"
    )
    reorder_notes = tuple(sorted((reorder_workspace / "notes").glob("*.md")))
    reorder_initial = {path: path.read_bytes() for path in reorder_notes}
    reorder_settings = reorder_workspace / ".stillus" / "settings.json"
    reordered_categories = ("Work", "Personal", "Planning", "Reading")
    reorder_expanded = {"all"}

    driver.start_app(reorder_workspace, "categories-reorder")
    wait_for_tree("stable category reorder source", reorder_expanded)
    source_x, source_y = group_row_center(
        "Work", expanded_groups=("all",), categories=DEMO_CATEGORIES
    )
    driver.press_point(source_x, source_y)
    driver.drag_point(source_x + 2, source_y + 1)
    driver.release()
    reorder_expanded.add("Work")
    wait_for_tree(
        "sub-threshold category movement remains a click",
        reorder_expanded,
    )
    click_group("Work", reorder_expanded)
    wait_for_tree(
        "category closes after the sub-threshold click",
        reorder_expanded,
    )

    target_top = group_row_top(
        "Personal", expanded_groups=("all",), categories=DEMO_CATEGORIES
    )
    driver.press_point(source_x, source_y)
    pressed_reorder = driver.capture("category-reorder-pressed")
    driver.drag_point(source_x, target_top + 8)
    driver.drag_point(source_x, target_top + 4)
    held_reorder = driver.capture("category-reorder-held")

    def root_marker_is_visible() -> bool:
        nonlocal held_reorder
        held_reorder = driver.capture("category-reorder-marker-poll")
        return near_color_pixel_count(
            held_reorder,
            (143, 184, 220),
            crop=(12, target_top, SIDEBAR_WIDTH - 24, 3),
        ) >= 40

    wait_until("root category drop marker", root_marker_is_visible, interval=0.05)
    editor_drag_difference = image_difference(
        pressed_reorder,
        held_reorder,
        crop=(SIDEBAR_WIDTH, 0, SCREEN_WIDTH - SIDEBAR_WIDTH, SCREEN_HEIGHT),
    )
    if editor_drag_difference > 20:
        raise AcceptanceFailure(
            "category drag painted outside the sidebar "
            f"({editor_drag_difference} changed editor pixels)"
        )
    driver.release()
    wait_for_tree(
        "root category moves before its sibling",
        reorder_expanded,
        category_order=reordered_categories,
    )

    def saved_root_order() -> bool:
        if not reorder_settings.is_file():
            return False
        saved = json.loads(reorder_settings.read_text(encoding="utf-8"))
        return saved.get("sidebar", {}).get("category_order") == list(
            reordered_categories
        )

    wait_until("root category order settings save", saved_root_order)
    driver.close_app()
    driver.start_app(reorder_workspace, "categories-reorder-restart")
    wait_for_tree(
        "root category order survives restart",
        reorder_expanded,
        category_order=reordered_categories,
    )
    click_group(
        "Work",
        reorder_expanded,
        category_order=reordered_categories,
    )
    wait_for_tree(
        "ordinary click still expands one reordered category",
        reorder_expanded,
        category_order=reordered_categories,
    )
    click_group(
        "Work",
        reorder_expanded,
        category_order=reordered_categories,
    )
    wait_for_tree(
        "ordinary click still collapses one reordered category",
        reorder_expanded,
        category_order=reordered_categories,
    )
    driver.hover("sidebar_blank")
    before_cancel = driver.wait_for_stable_frame(
        "stable category row before outside cancellation", crop=sidebar_crop
    )
    cancel_x, cancel_y = group_row_center(
        "Personal",
        expanded_groups=ordered(reorder_expanded, reordered_categories),
        categories=DEMO_CATEGORIES,
        category_order=reordered_categories,
    )
    driver.press_point(cancel_x, cancel_y)
    driver.drag_point(SIDEBAR_WIDTH + 120, cancel_y)
    driver.release()
    driver.hover("sidebar_blank")
    after_cancel = driver.wait_for_stable_frame(
        "outside category drop clears drag feedback", crop=sidebar_crop
    )
    cancel_row_top = group_row_top(
        "Personal",
        expanded_groups=ordered(reorder_expanded, reordered_categories),
        categories=DEMO_CATEGORIES,
        category_order=reordered_categories,
    )
    cancel_row_difference = 0

    def outside_feedback_cleared() -> bool:
        nonlocal after_cancel, cancel_row_difference
        after_cancel = driver.capture("category-outside-cancel-poll")
        cancel_row_difference = image_difference(
            before_cancel,
            after_cancel,
            crop=(0, cancel_row_top, SIDEBAR_WIDTH, GROUP_ROW_HEIGHT),
        )
        return cancel_row_difference <= 20

    try:
        wait_until(
            "outside category source feedback repaint",
            outside_feedback_cleared,
            timeout=2.0,
            interval=0.05,
        )
    except AcceptanceFailure as error:
        raise AcceptanceFailure(
            "outside category drop left stale source feedback "
            f"({cancel_row_difference} changed source-row pixels)"
        ) from error
    wait_for_tree(
        "outside category drop keeps persisted order",
        reorder_expanded,
        category_order=reordered_categories,
    )
    driver.close_app()
    if any(path.read_bytes() != contents for path, contents in reorder_initial.items()):
        raise AcceptanceFailure("category reorder changed canonical note bytes")
    assert_no_temporary_files(reorder_workspace)

    note_order_workspace = create_workspace(
        driver.temporary_root, "category-note-order-workspace"
    )
    note_order_notes = note_order_workspace / "notes"
    note_order_fixtures = (
        ("Pinned", True, "2020-01-01T00:00:00.000Z"),
        ("Alpha", False, "2022-01-01T00:00:00.000Z"),
        ("Beta", False, "2021-01-01T00:00:00.000Z"),
        ("Charlie", False, "2023-01-01T00:00:00.000Z"),
    )
    note_order_paths: dict[str, Path] = {}
    for title, pinned, created in note_order_fixtures:
        path = note_order_notes / f"{title}.md"
        note_order_paths[title] = path
        path.write_text(
            "---\n"
            "favorited: false\n"
            f"pinned: {str(pinned).lower()}\n"
            "tags: ['Work']\n"
            f"title: '{title}'\n"
            f"created: '{created}'\n"
            "modified: '2026-09-03T00:00:00.000Z'\n"
            "---\n"
            f"{title}\nprivate-{title.lower()}\n",
            encoding="utf-8",
        )
    note_order_bodies = {
        title: note_body(path) for title, path in note_order_paths.items()
    }
    note_order_categories = ("Work",)
    note_order_counts = {
        "all": 4,
        "favorites": 0,
        "Work": 4,
        "trash": 0,
    }
    note_order_expanded = {"all"}

    driver.start_app(note_order_workspace, "categories-note-order")
    wait_for_tree(
        "category note order starts alphabetically",
        note_order_expanded,
        counts=note_order_counts,
        categories=note_order_categories,
    )
    click_group(
        "Work",
        note_order_expanded,
        counts=note_order_counts,
        categories=note_order_categories,
    )
    wait_for_tree(
        "Work opens for note reorder",
        note_order_expanded,
        counts=note_order_counts,
        categories=note_order_categories,
    )
    note_order_layout = {
        "expanded": "Work",
        "expanded_groups": ordered(note_order_expanded, note_order_categories),
        "counts": note_order_counts,
        "categories": note_order_categories,
    }
    source_x, source_y = note_row_center(1, **note_order_layout)
    _, target_y = note_row_center(3, **note_order_layout)
    driver.press_point(source_x, source_y)
    driver.drag_point(source_x, target_y)
    driver.release()

    expected_manual_order = {
        "Pinned": 0,
        "Alpha": 2,
        "Beta": 0,
        "Charlie": 1,
    }

    def manual_note_order_saved() -> bool:
        return all(
            category_order_value(note_order_paths[title], "Work") == rank
            for title, rank in expected_manual_order.items()
        )

    wait_until("manual category note order save", manual_note_order_saved)
    if any(
        note_body(note_order_paths[title]) != body
        for title, body in note_order_bodies.items()
    ):
        raise AcceptanceFailure("manual note reorder changed a Markdown body")

    work_top = group_row_top(
        "Work",
        expanded_groups=ordered(note_order_expanded, note_order_categories),
        counts=note_order_counts,
        categories=note_order_categories,
    )
    sort_trigger = (SIDEBAR_WIDTH - 48, work_top + GROUP_ROW_HEIGHT // 2)
    before_sort_popover = driver.capture("category-sort-closed")
    driver.click_point(*sort_trigger)
    driver.wait_for_visual_change(
        "category sort popover opens",
        before_sort_popover,
        crop=(8, work_top + GROUP_ROW_HEIGHT, 248, 270),
        minimum_pixels=500,
    )
    popover_top = work_top + GROUP_ROW_HEIGHT + 4
    driver.click_point(100, popover_top + 80)
    driver.click_point(100, popover_top + 190)
    driver.click_point(100, popover_top + 242)

    wait_until(
        "automatic sort removes manual order",
        lambda: all(
            category_order_value(path, "Work") is None
            for path in note_order_paths.values()
        ),
    )

    note_sort_settings = note_order_workspace / ".stillus" / "settings.json"

    def automatic_sort_saved() -> bool:
        if not note_sort_settings.is_file():
            return False
        sidebar = json.loads(note_sort_settings.read_text(encoding="utf-8")).get(
            "sidebar", {}
        )
        return sidebar.get("note_sort") == [
            {
                "category": "Work",
                "field": "created",
                "direction": "descending",
            }
        ]

    wait_until("automatic category sort settings save", automatic_sort_saved)
    driver.click_note(1, **note_order_layout)
    driver.click("editor")
    driver.key("ctrl+End")
    driver.key("Return")
    driver.type_text("auto-sort-first")
    wait_until(
        "created-descending sort puts Charlie first after pinned notes",
        lambda: contains(note_order_paths["Charlie"], "auto-sort-first"),
    )
    driver.close_app()

    driver.start_app(note_order_workspace, "categories-note-order-restart")
    wait_for_tree(
        "automatic category note sort survives restart",
        note_order_expanded,
        counts=note_order_counts,
        categories=note_order_categories,
    )
    driver.click_note(2, **note_order_layout)
    driver.click("editor")
    driver.key("ctrl+End")
    driver.key("Return")
    driver.type_text("auto-sort-restored")
    wait_until(
        "restart keeps Alpha second in created-descending unpinned notes",
        lambda: contains(note_order_paths["Alpha"], "auto-sort-restored"),
    )
    driver.close_app()
    if any(
        note_body(path) != note_order_bodies[title]
        and not (
            title == "Charlie" and contains(path, "auto-sort-first")
        )
        and not (
            title == "Alpha" and contains(path, "auto-sort-restored")
        )
        for title, path in note_order_paths.items()
    ):
        raise AcceptanceFailure("category sort changed an unrelated Markdown body")
    assert_no_temporary_files(note_order_workspace)

    auto_workspace = generate_demo_workspace(
        driver.temporary_root / "category-auto-open-workspace"
    )
    auto_notes = auto_workspace / "notes"
    auto_project = auto_notes / "Project Alpha.md"
    auto_reading = auto_notes / "Reading List.md"
    auto_weekly = auto_notes / "Weekly Notes.md"
    auto_reading_initial = auto_reading.read_bytes()

    driver.start_app(auto_workspace, "categories-auto-open")
    auto_expanded = {"all"}
    wait_for_tree("stable auto-open initial state", auto_expanded)
    click_group("Personal", auto_expanded)
    wait_for_tree("Personal expands without replacing selection", auto_expanded)
    keep_open_marker = "existing-selection-stays-open"
    driver.click("editor")
    driver.key("ctrl+End")
    driver.key("Return")
    driver.type_text(keep_open_marker)
    wait_until(
        "existing document remains open across category activation",
        lambda: contains(auto_project, keep_open_marker),
    )
    if auto_reading.read_bytes() != auto_reading_initial:
        raise AcceptanceFailure("category activation replaced an existing selection")

    driver.wait_for_stable_frame("stable kept-open autosave")
    driver.click("trash")
    wait_until(
        "selected document soft delete leaves no selection",
        lambda: auto_project.exists() and "deleted: true" in read_text(auto_project),
    )
    remaining_before_activation = {
        auto_reading: auto_reading.read_bytes(),
        auto_weekly: auto_weekly.read_bytes(),
    }
    post_trash_categories = ("Personal", "Reading", "Work")
    post_trash_counts = {
        "all": 2,
        "favorites": 0,
        "Personal": 1,
        "Reading": 1,
        "Work": 1,
        "trash": 1,
    }
    wait_for_tree(
        "stable post-trash independent groups",
        auto_expanded,
        counts=post_trash_counts,
        categories=post_trash_categories,
    )
    click_group(
        "Work",
        auto_expanded,
        counts=post_trash_counts,
        categories=post_trash_categories,
    )
    wait_for_tree(
        "stable Work after empty selection",
        auto_expanded,
        counts=post_trash_counts,
        categories=post_trash_categories,
    )
    if any(
        path.read_bytes() != contents
        for path, contents in remaining_before_activation.items()
    ):
        raise AcceptanceFailure("auto-opening a category rewrote canonical notes")
    auto_open_marker = "first-work-note-auto-opened"
    driver.click("editor")
    driver.key("ctrl+End")
    driver.key("Return")
    driver.type_text(auto_open_marker)
    wait_until(
        "first Work note opens after selection becomes empty",
        lambda: contains(auto_weekly, auto_open_marker),
    )
    driver.close_app()

    if auto_reading.read_bytes() != auto_reading_initial:
        raise AcceptanceFailure("auto-open category selected a note outside Work")
    assert_no_temporary_files(auto_workspace)

    hierarchical_workspace = create_workspace(
        driver.temporary_root, "hierarchical-category-workspace"
    )
    hierarchical_notes = hierarchical_workspace / "notes"
    leaf_note = hierarchical_notes / "A Leaf.md"
    child_note = hierarchical_notes / "B Child.md"
    shared_note = hierarchical_notes / "C Shared.md"
    write_tagged_note(leaf_note, "A Leaf", ["Parent/Child/Leaf"])
    write_tagged_note(child_note, "B Child", ["Parent/Child"])
    write_tagged_note(
        shared_note,
        "C Shared",
        ["Parent/Child", "Parent/Other"],
    )
    hierarchical_initial = {
        path: path.read_bytes() for path in (leaf_note, child_note, shared_note)
    }
    hierarchical_categories = (
        "Parent/Child",
        "Parent/Child/Leaf",
        "Parent/Other",
    )
    hierarchical_counts = {
        "all": 3,
        "favorites": 0,
        "Parent": 3,
        "Parent/Child": 3,
        "Parent/Child/Leaf": 1,
        "Parent/Other": 1,
        "trash": 0,
    }
    hierarchical_direct_counts = {
        "all": 3,
        "favorites": 0,
        "Parent": 0,
        "Parent/Child": 2,
        "Parent/Child/Leaf": 1,
        "Parent/Other": 1,
        "trash": 0,
    }

    driver.start_app(hierarchical_workspace, "categories-hierarchical")
    hierarchical_expanded = {"all"}
    wait_for_tree(
        "virtual Parent starts collapsed",
        hierarchical_expanded,
        counts=hierarchical_counts,
        direct_counts=hierarchical_direct_counts,
        categories=hierarchical_categories,
    )
    for group, description in (
        ("Parent", "virtual Parent reveals direct children"),
        ("Parent/Child", "Child reveals Leaf before direct notes"),
        ("Parent/Child/Leaf", "Leaf reveals its exact note"),
    ):
        click_group(
            group,
            hierarchical_expanded,
            counts=hierarchical_counts,
            direct_counts=hierarchical_direct_counts,
            categories=hierarchical_categories,
        )
        wait_for_tree(
            description,
            hierarchical_expanded,
            counts=hierarchical_counts,
            direct_counts=hierarchical_direct_counts,
            categories=hierarchical_categories,
        )

    click_group(
        "Parent",
        hierarchical_expanded,
        counts=hierarchical_counts,
        direct_counts=hierarchical_direct_counts,
        categories=hierarchical_categories,
    )
    wait_for_tree(
        "collapsed Parent hides descendants",
        hierarchical_expanded,
        counts=hierarchical_counts,
        direct_counts=hierarchical_direct_counts,
        categories=hierarchical_categories,
    )
    driver.close_app()

    saved_sidebar = json.loads(
        (hierarchical_workspace / ".stillus" / "settings.json").read_text(
            encoding="utf-8"
        )
    )["sidebar"]
    saved_expanded_tags = {
        group["tag"]
        for group in saved_sidebar["expanded"]
        if group["kind"] == "tag"
    }
    if saved_expanded_tags != {"Parent/Child", "Parent/Child/Leaf"}:
        raise AcceptanceFailure(
            "nested expanded paths did not survive settings persistence: "
            f"{saved_expanded_tags}"
        )

    driver.start_app(hierarchical_workspace, "categories-hierarchical-restart")
    wait_for_tree(
        "restart keeps Parent collapsed",
        hierarchical_expanded,
        counts=hierarchical_counts,
        direct_counts=hierarchical_direct_counts,
        categories=hierarchical_categories,
    )
    click_group(
        "Parent",
        hierarchical_expanded,
        counts=hierarchical_counts,
        direct_counts=hierarchical_direct_counts,
        categories=hierarchical_categories,
    )
    wait_for_tree(
        "reopening Parent restores Child and Leaf expansion",
        hierarchical_expanded,
        counts=hierarchical_counts,
        direct_counts=hierarchical_direct_counts,
        categories=hierarchical_categories,
    )

    nested_order = (
        "Parent",
        "Parent/Other",
        "Parent/Child",
        "Parent/Child/Leaf",
    )
    nested_layout = {
        "expanded_groups": ordered(
            hierarchical_expanded, hierarchical_categories
        ),
        "counts": hierarchical_counts,
        "direct_counts": hierarchical_direct_counts,
        "categories": hierarchical_categories,
    }
    nested_source_x, nested_source_y = group_row_center(
        "Parent/Other", **nested_layout
    )
    nested_target_top = group_row_top("Parent/Child", **nested_layout)
    driver.press_point(nested_source_x, nested_source_y)
    driver.drag_point(nested_source_x, nested_target_top + 8)
    driver.drag_point(nested_source_x, nested_target_top + 4)
    nested_held = driver.capture("nested-category-reorder-held")

    def nested_marker_is_visible() -> bool:
        nonlocal nested_held
        nested_held = driver.capture("nested-category-reorder-marker-poll")
        return near_color_pixel_count(
            nested_held,
            (143, 184, 220),
            crop=(12, nested_target_top, SIDEBAR_WIDTH - 24, 3),
        ) >= 40

    wait_until("nested category drop marker", nested_marker_is_visible, interval=0.05)
    driver.release()
    wait_for_tree(
        "nested category moves before its sibling",
        hierarchical_expanded,
        counts=hierarchical_counts,
        direct_counts=hierarchical_direct_counts,
        categories=hierarchical_categories,
        category_order=nested_order,
    )

    def saved_nested_order() -> bool:
        saved = json.loads(
            (hierarchical_workspace / ".stillus" / "settings.json").read_text(
                encoding="utf-8"
            )
        )
        return saved.get("sidebar", {}).get("category_order") == list(nested_order)

    wait_until("nested category order settings save", saved_nested_order)
    driver.close_app()
    driver.start_app(
        hierarchical_workspace, "categories-hierarchical-reordered-restart"
    )
    wait_for_tree(
        "nested category order survives restart",
        hierarchical_expanded,
        counts=hierarchical_counts,
        direct_counts=hierarchical_direct_counts,
        categories=hierarchical_categories,
        category_order=nested_order,
    )

    created_parent_note = hierarchical_notes / "New note.md"
    driver.click("create_menu")
    driver.click("create_note")
    wait_until("note created in virtual Parent", created_parent_note.is_file)
    wait_until(
        "virtual Parent becomes the exact creation tag",
        lambda: created_parent_note.is_file()
        and "tags:\n  - 'Parent'\n" in read_text(created_parent_note),
    )
    final_hierarchical_categories = (
        "Parent",
        "Parent/Child",
        "Parent/Child/Leaf",
        "Parent/Other",
    )
    final_hierarchical_counts = dict(hierarchical_counts)
    final_hierarchical_counts.update({"all": 4, "Parent": 4})
    final_hierarchical_direct_counts = dict(hierarchical_direct_counts)
    final_hierarchical_direct_counts.update({"all": 4, "Parent": 1})
    wait_for_tree(
        "exact Parent merges with its virtual node and lists the new note last",
        hierarchical_expanded,
        counts=final_hierarchical_counts,
        direct_counts=final_hierarchical_direct_counts,
        categories=final_hierarchical_categories,
        category_order=nested_order,
    )
    driver.close_app()
    if any(
        path.read_bytes() != contents
        for path, contents in hierarchical_initial.items()
    ):
        raise AcceptanceFailure(
            "hierarchical navigation or creation rewrote existing canonical notes"
        )
    assert_no_temporary_files(hierarchical_workspace)

    overflow_workspace = create_workspace(
        driver.temporary_root, "category-scrollbar-workspace"
    )
    overflow_note = overflow_workspace / "notes" / "Category Scrollbar.md"
    write_tagged_note(
        overflow_note,
        "Category Scrollbar",
        [f"Category {index:02d}" for index in range(28)],
    )
    overflow_before = overflow_note.read_bytes()
    driver.start_app(overflow_workspace, "categories-scrollbar")
    overflow_initial = driver.wait_for_stable_frame(
        "category tree scrollbar is initially idle",
        crop=(12, 110, 232, 620),
    )
    assert_transient_sidebar_scrollbar(
        driver, "category-tree", overflow_initial
    )
    driver.wheel(
        "down",
        clicks=1,
        control="sidebar_scroll",
        delay_ms=20,
    )
    driver.close_app()
    if overflow_note.read_bytes() != overflow_before:
        raise AcceptanceFailure("scrolling the category tree changed canonical bytes")
    assert_no_temporary_files(overflow_workspace)


def interaction_scenario(driver: WindowDriver, workspace: Path) -> None:
    del workspace
    interaction_workspace = create_workspace(
        driver.temporary_root, "single-click-workspace"
    )
    notes = interaction_workspace / "notes"
    bodies = (
        "A Alpha\nalpha-001\nalpha-002\n",
        "B Bravo\nbravo-001\nbravo-002\n",
        "C Charlie\ncharlie-001\ncharlie-002\n",
    )
    paths = tuple(notes / name for name in ("A Alpha.md", "B Bravo.md", "C Charlie.md"))
    interaction_categories = ("Category 1", "Category 2", "Category 3")
    interaction_counts = {
        "all": 3,
        "favorites": 0,
        "Category 1": 1,
        "Category 2": 1,
        "Category 3": 1,
    }
    for index, (path, body) in enumerate(zip(paths, bodies, strict=True)):
        path.write_text(
            "---\n"
            "favorited: false\n"
            "pinned: false\n"
            f"tags: ['Category {index + 1}']\n"
            f"title: '{path.stem}'\n"
            "created: '2026-09-01T00:00:00.000Z'\n"
            "modified: '2026-09-01T00:00:00.000Z'\n"
            "---\n"
            f"{body}",
            encoding="utf-8",
        )

    driver.start_app(interaction_workspace, "single-click")

    closed_popover = driver.wait_for_stable_frame(
        "stable editor before tag popover", crop=TAG_POPOVER_CROP
    )
    driver.click("tag_manager")
    opened_popover = driver.wait_for_visual_change(
        "tag popover opens before the interaction check",
        closed_popover,
        crop=TAG_POPOVER_CROP,
        minimum_pixels=200,
    )
    driver.wait_for_stable_frame(
        "tag popover settles before the interaction check", crop=TAG_POPOVER_CROP
    )
    driver.click("tag_manager")
    driver.wait_for_visual_change(
        "tag popover toggle closes before editor click",
        opened_popover,
        crop=TAG_POPOVER_CROP,
        minimum_pixels=200,
    )
    driver.wait_for_stable_frame(
        "stable editor after tag popover closes", crop=TAG_POPOVER_CROP
    )
    driver.click("editor_line_2_col_5")
    driver.type_text("FIRST")
    first_click_expected = bodies[0].replace("alpha-001", "alphaFIRST-001", 1)
    wait_until(
        "first editor click to focus and position the caret",
        lambda: note_body(paths[0]) == first_click_expected,
    )

    markers = ("bravoclick", "charlieclick", "alphaclick", "charlieagain", "bravoagain")
    transitions = ((1, 60), (2, 235), (0, 150), (2, 60), (1, 235))
    for (note_index, click_x), marker in zip(transitions, markers, strict=True):
        driver.click_note(
            note_index,
            x=click_x,
            counts=interaction_counts,
            categories=interaction_categories,
        )
        driver.click("editor")
        driver.key("ctrl+End")
        driver.key("Return")
        driver.type_text(marker)
        wait_until(
            f"single note-row click {note_index} at x={click_x}",
            lambda path=paths[note_index], value=marker: contains(path, value),
        )
        wrong_paths = [
            path
            for index, path in enumerate(paths)
            if index != note_index and contains(path, marker)
        ]
        if wrong_paths:
            raise AcceptanceFailure(
                f"note-row click edited the wrong canonical note: {wrong_paths}"
            )

    driver.close_app()
    for path, marker in zip(
        (paths[1], paths[2], paths[0], paths[2], paths[1]), markers, strict=True
    ):
        if not contains(path, marker):
            raise AcceptanceFailure(f"missing single-click marker {marker} in {path}")
    assert_no_temporary_files(interaction_workspace)


def caret_scenario(driver: WindowDriver, workspace: Path) -> None:
    del workspace
    caret_workspace = create_workspace(driver.temporary_root, "caret-workspace")
    note = caret_workspace / "notes" / "A Caret.md"
    original_body = (
        "line-000 alpha\n"
        "line-001 bravo\n"
        "line-002 charlie\n"
        "line-003 delta\n"
        "line-004 echo\n"
    )
    note.write_text(
        "---\n"
        "favorited: false\n"
        "pinned: false\n"
        "tags: [Editor]\n"
        "title: A Caret\n"
        "created: '2026-09-01T00:00:00.000Z'\n"
        "modified: '2026-09-01T00:00:00.000Z'\n"
        "---\n"
        f"{original_body}",
        encoding="utf-8",
    )

    driver.start_app(caret_workspace, "pointer")
    clean_unfocused = driver.wait_for_stable_frame(
        "stable initially unfocused editor", crop=EDITOR_CROP
    )

    driver.click("editor_line_3_col_5")
    caret_on = driver.capture("caret-on")
    caret_crop = (EDITOR_TEXT_LEFT + 36, 120, 30, 30)
    caret_off = driver.wait_for_visual_change(
        "focused caret blink off",
        caret_on,
        crop=caret_crop,
        minimum_pixels=20,
        timeout=2.0,
    )
    driver.key("Right")
    activity_on = driver.wait_for_visual_change(
        "keyboard activity makes the caret visible immediately",
        caret_off,
        crop=caret_crop,
        minimum_pixels=20,
        timeout=1.0,
    )
    activity_off = driver.wait_for_visual_change(
        "activity-reset caret blinks off",
        activity_on,
        crop=caret_crop,
        minimum_pixels=20,
        timeout=2.0,
    )
    driver.wait_for_visual_change(
        "activity-reset caret blinks on again",
        activity_off,
        crop=caret_crop,
        minimum_pixels=20,
        timeout=2.0,
    )

    driver.click_note(
        0,
        counts={"all": 1, "favorites": 0, "Editor": 1},
        categories=("Editor",),
    )
    unfocused = driver.wait_for_stable_frame(
        "stable editor after focus leaves the caret", crop=EDITOR_CROP
    )
    if image_difference(clean_unfocused, unfocused, crop=EDITOR_CROP) != 0:
        raise AcceptanceFailure("caret remained visible after editor focus was lost")

    driver.click("editor_line_1_col_5")
    driver.modified_click("editor_line_1_col_9", "Shift_L")
    driver.type_text("SEL")
    selected_expected = original_body.replace("line-000 alpha", "line-SELalpha", 1)
    selected_note = caret_workspace / "notes" / "line-SELalpha.md"
    wait_until(
        "Shift-click selection replacement and first-line relocation",
        lambda: selected_note.is_file()
        and note_body(selected_note) == selected_expected
        and not note.exists(),
    )
    note = selected_note

    driver.click("editor_line_1_start")
    driver.type_text("^")
    start_expected = selected_expected.replace("line-SELalpha", "^line-SELalpha", 1)
    start_note = caret_workspace / "notes" / "^line-SELalpha.md"
    wait_until(
        "input at clicked line start and second relocation",
        lambda: start_note.is_file()
        and note_body(start_note) == start_expected
        and not note.exists(),
    )
    note = start_note

    driver.click("editor_line_2_col_5")
    driver.modified_click("editor_line_2_col_9", "Shift_L")
    driver.click("editor_line_3_col_5")
    driver.type_text("@")
    middle_expected = start_expected.replace("line-002", "line-@002", 1)
    wait_until(
        "input at clicked third-line column",
        lambda: note_body(note) == middle_expected,
    )

    driver.click("editor_line_4_far_right")
    driver.type_text("%")
    right_expected = middle_expected.replace("line-003 delta", "line-003 delta%", 1)
    wait_until(
        "click right of a line clamps to its end",
        lambda: note_body(note) == right_expected,
    )

    driver.click("editor_line_5_col_9")
    driver.type_text("#")
    fifth_expected = right_expected.replace("line-004 echo", "line-004 #echo", 1)
    wait_until(
        "input at clicked fifth-line column",
        lambda: note_body(note) == fifth_expected,
    )

    driver.click("editor_line_5_far_right")
    driver.type_text("!")
    end_expected = fifth_expected.replace("line-004 #echo", "line-004 #echo!", 1)
    wait_until(
        "far-right click clamps to the last byte of the line",
        lambda: note_body(note) == end_expected,
    )

    driver.click("editor_below_document")
    driver.type_text("$")
    final_expected = f"{end_expected}$"
    wait_until(
        "click below the document clamps to its final line",
        lambda: note_body(note) == final_expected,
    )

    driver.click("editor_line_1_col_5")
    shutdown_caret_on = driver.capture("shutdown-caret-on")
    driver.wait_for_visual_change(
        "caret timer is pending before normal close",
        shutdown_caret_on,
        crop=(EDITOR_TEXT_LEFT + 36, 78, 20, 30),
        minimum_pixels=20,
        timeout=2.0,
    )
    driver.close_app()
    if note_body(note) != final_expected:
        raise AcceptanceFailure("pointer caret edits did not persist exactly")
    app_log = read_text(driver.app_log_paths[-1])
    if "panicked at" in app_log or "already disposed" in app_log:
        raise AcceptanceFailure("caret blink callback panicked during normal shutdown")
    assert_no_temporary_files(caret_workspace)


def editor_scenario(driver: WindowDriver, workspace: Path) -> None:
    del workspace
    editor_workspace = create_workspace(driver.temporary_root, "editor-workspace")
    note = editor_workspace / "notes" / "A Editor.md"
    long_body = "".join(
        (
            f"line-{index:03d} " + "x" * 96 + "\n"
            if index == 1
            else f"line-{index:03d} viewport acceptance text\n"
        )
        for index in range(120)
    )
    note.write_text(
        "---\n"
        "favorited: false\n"
        "pinned: false\n"
        "tags: [Editor]\n"
        "title: A Editor\n"
        "created: '2026-09-01T00:00:00.000Z'\n"
        "modified: '2026-09-01T00:00:00.000Z'\n"
        "---\n"
        f"{long_body}",
        encoding="utf-8",
    )
    untouched = editor_workspace / "notes" / "B Untouched.md"
    untouched.write_text("untouched sibling\n", encoding="utf-8")
    untouched_before = untouched.read_bytes()

    driver.start_app(editor_workspace, "commands")
    driver.click("editor")
    initial_view = driver.wait_for_stable_frame(
        "initial editor viewport", crop=EDITOR_CROP
    )
    line_number_pixels = dark_pixel_count(initial_view, crop=EDITOR_LINE_NUMBER_CROP)
    if line_number_pixels < 80:
        raise AcceptanceFailure(
            "editor line-number gutter is not visibly painted "
            f"({line_number_pixels} dark pixels)"
        )
    full_width_pixels = dark_pixel_count(initial_view, crop=(1_110, 100, 90, 24))
    if full_width_pixels < 40:
        raise AcceptanceFailure(
            "editor text measure still stops at the former 96-column boundary "
            f"({full_width_pixels} far-right glyph pixels)"
        )
    driver.key("Page_Down")
    driver.wait_for_visual_change(
        "PageDown viewport movement",
        initial_view,
        crop=EDITOR_CROP,
        minimum_pixels=500,
    )
    driver.key("Page_Up")
    top_view = driver.wait_for_stable_frame("PageUp viewport reset", crop=EDITOR_CROP)
    canonical_before_wheel = note.read_bytes()
    driver.wheel("down")
    first_wheel = driver.wait_for_visual_change(
        "pointer-wheel viewport movement",
        top_view,
        crop=EDITOR_CROP,
        minimum_pixels=200,
    )
    driver.wheel("down")
    driver.wait_for_visual_change(
        "second wheel tick advances by another controlled step",
        first_wheel,
        crop=EDITOR_CROP,
        minimum_pixels=200,
    )
    if note.read_bytes() != canonical_before_wheel:
        raise AcceptanceFailure("controlled editor wheel changed canonical Markdown")
    driver.key("Page_Up")

    canonical_before_navigation = note.read_bytes()
    driver.key("ctrl+l")
    prompt = driver.wait_for_visual_change(
        "Ctrl+L opens the go-to-line prompt",
        top_view,
        crop=(540, 320, 420, 210),
        minimum_pixels=2_000,
    )
    driver.type_text("999")
    driver.key("Return")
    driver.wait_for_visual_change(
        "out-of-range line stays in the prompt with validation",
        prompt,
        crop=(560, 340, 380, 170),
        minimum_pixels=20,
    )
    if note.read_bytes() != canonical_before_navigation:
        raise AcceptanceFailure("invalid go-to-line changed canonical Markdown")
    driver.key("ctrl+a")
    driver.type_text("80")
    driver.click("go_to_line_submit")
    driver.wait_for_visual_change(
        "go-to-line button closes the prompt and moves the viewport",
        prompt,
        crop=EDITOR_CROP,
        minimum_pixels=2_000,
    )
    if note.read_bytes() != canonical_before_navigation:
        raise AcceptanceFailure("valid go-to-line navigation changed canonical Markdown")
    driver.type_text("@")
    jumped_expected = long_body.replace(
        "line-079 viewport acceptance text",
        "@line-079 viewport acceptance text",
        1,
    )
    jumped_note = editor_workspace / "notes" / "line-000 viewport acceptance text.md"
    wait_until(
        "valid go-to-line inserts at the requested line start",
        lambda: jumped_note.is_file()
        and note_body(jumped_note) == jumped_expected
        and not note.exists(),
    )
    note = jumped_note
    driver.key("ctrl+z")
    wait_until("undo restores the go-to-line probe", lambda: note_body(note) == long_body)
    canonical_after_probe = note.read_bytes()

    before_escape = driver.wait_for_stable_frame(
        "editor after go-to-line navigation", crop=EDITOR_CROP
    )
    driver.key("ctrl+l")
    opened_again = driver.wait_for_visual_change(
        "go-to-line prompt reopens",
        before_escape,
        crop=(540, 320, 420, 210),
        minimum_pixels=2_000,
    )
    driver.key("Escape")
    driver.wait_for_visual_change(
        "Escape closes go-to-line and returns editor focus",
        opened_again,
        crop=(540, 320, 420, 210),
        minimum_pixels=2_000,
    )
    if note.read_bytes() != canonical_after_probe:
        raise AcceptanceFailure("go-to-line navigation changed canonical Markdown")

    task_line = f"line-001 {'x' * 96}\n"
    checked_task_body = long_body.replace(task_line, f"- [x] {task_line}", 1)
    unchecked_task_body = long_body.replace(task_line, f"- [ ] {task_line}", 1)
    driver.key("ctrl+l")
    driver.type_text("2")
    driver.key("Return")
    driver.key("alt+d")
    wait_until(
        "Alt+D converts an ordinary line to a checked task",
        lambda: note_body(note) == checked_task_body,
    )
    driver.key("alt+d")
    wait_until(
        "Alt+D converts a checked task to unchecked",
        lambda: note_body(note) == unchecked_task_body,
    )
    driver.key("ctrl+z")
    wait_until(
        "task toggle undo is one operation",
        lambda: note_body(note) == checked_task_body,
    )
    driver.key("ctrl+shift+z")
    wait_until(
        "task toggle redo is one operation",
        lambda: note_body(note) == unchecked_task_body,
    )
    driver.key("Home")
    driver.key("shift+Down")
    driver.key("alt+d")
    wait_until(
        "Alt+D changes only the upper line of a multiline selection",
        lambda: note_body(note) == checked_task_body,
    )
    driver.key("ctrl+z")
    wait_until(
        "multiline task toggle undo restores exact canonical body",
        lambda: note_body(note) == unchecked_task_body,
    )
    if untouched.read_bytes() != untouched_before:
        raise AcceptanceFailure("Alt+D changed an untouched sibling note")

    driver.key("ctrl+a")
    driver.type_text("alpha")
    driver.key("Return")
    driver.type_text("beta")
    driver.key("Tab")
    driver.type_text("gamma")
    initial_expected = "alpha\nbeta    gamma"
    initial_note = editor_workspace / "notes" / "alpha.md"
    wait_until(
        "insert/Enter/Tab autosave and first-line relocation",
        lambda: initial_note.is_file()
        and note_body(initial_note) == initial_expected
        and not note.exists(),
    )
    note = initial_note

    driver.key("shift+Left")
    driver.key("shift+Left")
    driver.key("ctrl+x")
    wait_until("directed selection cut", lambda: note_body(note) == "alpha\nbeta    gam")
    driver.key("ctrl+v")
    driver.key("ctrl+z")
    driver.key("ctrl+shift+z")
    driver.key("Left")
    driver.key("Delete")
    driver.key("BackSpace")
    driver.type_text("MA")
    driver.key("Up")
    driver.type_text("X")
    driver.key("Down")
    driver.type_text("Y")
    driver.key("shift+Left")
    driver.key("ctrl+c")
    driver.key("Right")
    driver.key("Return")
    driver.key("ctrl+v")
    expected = "alphaX\nbeta  Y\nY  gamMA"
    edited_note = editor_workspace / "notes" / "alphaX.md"
    wait_until(
        "arrows/delete/copy/paste/undo/redo autosave and relocation",
        lambda: edited_note.is_file()
        and note_body(edited_note) == expected
        and not note.exists(),
    )
    note = edited_note
    driver.close_app()

    driver.start_app(editor_workspace, "reopen")
    driver.click("editor")
    driver.type_text("R")
    reopened_note = editor_workspace / "notes" / "RalphaX.md"
    wait_until(
        "reopened note edit and relocation",
        lambda: reopened_note.is_file()
        and note_body(reopened_note) == f"R{expected}"
        and not note.exists(),
    )
    driver.close_app()
    if untouched.read_bytes() != untouched_before:
        raise AcceptanceFailure("editor commands changed an untouched sibling note")
    assert_no_temporary_files(editor_workspace)


def context_menu_scenario(driver: WindowDriver, workspace: Path) -> None:
    del workspace
    context_workspace = create_workspace(driver.temporary_root, "context-menu-workspace")
    note = context_workspace / "notes" / "A Context Menu.md"
    original_body = "alpha bravo charlie\npaste target\n"
    note.write_text(
        "---\n"
        "favorited: false\n"
        "pinned: false\n"
        "tags: [Editor]\n"
        "title: A Context Menu\n"
        "created: '2026-09-01T00:00:00.000Z'\n"
        "modified: '2026-09-01T00:00:00.000Z'\n"
        "---\n"
        f"{original_body}",
        encoding="utf-8",
    )
    untouched = context_workspace / "notes" / "B Untouched.md"
    untouched.write_text("untouched sibling\n", encoding="utf-8")
    untouched_before = untouched.read_bytes()
    canonical_before = note.read_bytes()

    driver.start_app(context_workspace, "context-menu")
    baseline = driver.wait_for_stable_frame(
        "stable editor before double click", crop=EDITOR_CROP
    )

    driver.double_click("editor_word_bravo")
    selected = driver.wait_for_visual_change(
        "double click visibly selects the complete word",
        baseline,
        crop=(EDITOR_TEXT_LEFT + 46, 78, 52, 27),
        minimum_pixels=250,
    )
    if note.read_bytes() != canonical_before:
        raise AcceptanceFailure("double click changed canonical Markdown")

    driver.right_click("editor_word_bravo")
    copy_menu = driver.wait_for_visual_change(
        "secondary click opens the text context menu",
        selected,
        crop=(EDITOR_TEXT_LEFT + 64, 84, 220, 115),
        minimum_pixels=5_000,
    )
    driver.click_context_menu_row("editor_word_bravo", 1)
    wait_until(
        "context-menu Copy writes the selected word to the clipboard",
        lambda: clipboard_text(driver.environment) == "bravo",
    )
    driver.wait_for_visual_change(
        "Copy dismisses the context menu",
        copy_menu,
        crop=(EDITOR_TEXT_LEFT + 64, 84, 220, 115),
        minimum_pixels=5_000,
    )
    if note.read_bytes() != canonical_before:
        raise AcceptanceFailure("context-menu Copy changed canonical Markdown")

    driver.double_click("editor_word_charlie")
    before_cut_menu = driver.capture("before-cut-menu")
    driver.right_click("editor_word_charlie")
    driver.wait_for_visual_change(
        "secondary click keeps the selected word and opens Cut",
        before_cut_menu,
        crop=(EDITOR_TEXT_LEFT + 123, 84, 220, 115),
        minimum_pixels=5_000,
    )
    driver.click_context_menu_row("editor_word_charlie", 0)
    cut_expected = "alpha bravo \npaste target\n"
    cut_note = context_workspace / "notes" / "alpha bravo.md"
    wait_until(
        "context-menu Cut autosave and first-line relocation",
        lambda: cut_note.is_file()
        and note_body(cut_note) == cut_expected
        and not note.exists(),
    )
    note = cut_note
    wait_until(
        "context-menu Cut writes the selected word to the clipboard",
        lambda: clipboard_text(driver.environment) == "charlie",
    )

    driver.click("editor_line_2_end")
    before_paste_menu = driver.capture("before-paste-menu")
    driver.right_click("editor_line_2_end")
    driver.wait_for_visual_change(
        "secondary click opens Paste at the caret",
        before_paste_menu,
        crop=(EDITOR_TEXT_LEFT + 94, 105, 220, 115),
        minimum_pixels=5_000,
    )
    driver.click_context_menu_row("editor_line_2_end", 2)
    pasted_expected = "alpha bravo \npaste targetcharlie\n"
    wait_until(
        "context-menu Paste autosave",
        lambda: note_body(note) == pasted_expected,
    )

    driver.close_app()
    if note_body(note) != pasted_expected:
        raise AcceptanceFailure("context-menu edits did not persist exactly")
    if untouched.read_bytes() != untouched_before:
        raise AcceptanceFailure("context-menu actions changed an unrelated note")
    assert_no_temporary_files(context_workspace)


def selection_scenario(driver: WindowDriver, workspace: Path) -> None:
    """Pointer selection: double-click word, triple-click line, drag lifecycle.

    Every selection is proven twice: visually (only the editor's own overlay is
    painted, never a second grey one) and by typing over it, which must replace
    exactly the selected bytes in the canonical Markdown.
    """
    del workspace
    selection_workspace = create_workspace(driver.temporary_root, "selection-workspace")
    note = selection_workspace / "notes" / "A Selection.md"
    original_body = (
        "line-000 alpha\n"
        "line-001 bravo\n"
        "line-002 charlie\n"
        "line-003 delta\n"
        "line-004 echo\n"
    )
    note.write_text(
        "---\n"
        "favorited: false\n"
        "pinned: false\n"
        "tags: [Editor]\n"
        "title: A Selection\n"
        "created: '2026-09-01T00:00:00.000Z'\n"
        "modified: '2026-09-01T00:00:00.000Z'\n"
        "---\n"
        f"{original_body}",
        encoding="utf-8",
    )
    # The whole first rendered row, for glyph/overlay/caret column geometry.
    first_row_crop = (EDITOR_TEXT_LEFT, 81, 300, 20)
    # Bytes 5..9 of the third rendered row, where the drag below is painted.
    drag_crop = (EDITOR_TEXT_LEFT + 42, 124, 34, 20)
    # Glyph ink alone stays far below this; a grey overlay darkens every pixel.
    grey_overlay_dark_pixels = 400

    driver.start_app(selection_workspace, "selection")
    baseline = driver.wait_for_stable_frame(
        "stable editor before pointer selection", crop=EDITOR_CROP
    )

    # Double-click selects exactly the word under the glyph and keeps focus.
    driver.double_click("editor_line_1_col_9")
    driver.wait_for_visual_change(
        "double click paints the word selection",
        baseline,
        crop=(EDITOR_TEXT_LEFT + 72, 80, 50, 22),
        minimum_pixels=250,
    )
    # The painted overlay must start and end at the glyphs of ``alpha``: the
    # word begins after the gap that follows ``line-000`` and ends before the
    # line's trailing paper. Word gaps in the editor font are wider than the
    # gaps between glyph cores inside a word.
    selected = driver.wait_for_stable_frame(
        "stable editor with the double-clicked word", crop=EDITOR_CROP
    )
    profile = column_profile(selected, first_row_crop)
    words = column_runs(profile["ink"], merge_gap=6)
    overlay = column_runs(profile["overlay"], merge_gap=1)
    if len(words) != 2 or len(overlay) != 1:
        raise AcceptanceFailure(
            f"unexpected first-row geometry: ink runs {words}, overlay runs {overlay}"
        )
    (prefix_start, prefix_end), (word_start, word_end) = words
    (overlay_start, overlay_end) = overlay[0]
    if not prefix_end < overlay_start <= word_start:
        raise AcceptanceFailure(
            f"selection overlay {overlay[0]} does not start at the word {words[1]}"
            f" after the prefix {words[0]}"
        )
    if not word_end <= overlay_end <= word_end + 4:
        raise AcceptanceFailure(
            f"selection overlay {overlay[0]} does not end at the word {words[1]}"
        )
    # A caret placed at the end of a word sits right after its last glyph.
    # The caret blinks, so frames are polled until its line is painted.
    driver.click("editor_line_1_col_14")
    geometry: list[dict[str, set[int]]] = []

    def caret_painted() -> bool:
        frame = driver.capture("caret-geometry")
        profile = column_profile(frame, first_row_crop)
        frame.unlink(missing_ok=True)
        if not profile["caret"]:
            return False
        geometry[:] = [profile]
        return True

    wait_until("visible caret at the end of the word", caret_painted)
    words = column_runs(geometry[0]["ink"], merge_gap=6)
    caret = column_runs(geometry[0]["caret"], merge_gap=0)
    if len(words) != 2 or len(caret) != 1:
        raise AcceptanceFailure(
            f"unexpected caret geometry: ink runs {words}, caret runs {caret}"
        )
    word_end = words[1][1]
    caret_start = caret[0][0]
    if not word_end < caret_start <= word_end + 4:
        raise AcceptanceFailure(
            f"caret {caret[0]} is not right after the word {words[1]}"
        )

    # Typing over the double-clicked word replaces exactly that word.
    driver.double_click("editor_line_1_col_9")

    def overlay_painted() -> bool:
        frame = driver.capture("overlay-geometry")
        painted = bool(column_profile(frame, first_row_crop)["overlay"])
        frame.unlink(missing_ok=True)
        return painted

    wait_until("double click paints the word selection again", overlay_painted)
    driver.type_text("W")
    expected = original_body.replace("line-000 alpha", "line-000 W", 1)
    replaced_note = selection_workspace / "notes" / "line-000 W.md"
    wait_until(
        "double-click word replacement and first-line relocation",
        lambda: replaced_note.is_file()
        and note_body(replaced_note) == expected
        and not note.exists(),
    )
    note = replaced_note

    # Triple-click selects the whole line together with its line break.
    driver.triple_click("editor_line_2_col_5")
    driver.type_text("T")
    expected = expected.replace("line-001 bravo\n", "T", 1)
    wait_until(
        "triple-click line replacement",
        lambda: note_body(note) == expected,
    )

    # Pointer drag paints only the editor overlay, survives the release and
    # stops following the pointer once the button is up.
    before_drag = driver.wait_for_stable_frame(
        "stable editor before the pointer drag", crop=EDITOR_CROP
    )
    driver.press("editor_line_3_col_5")
    driver.drag_to("editor_line_3_col_9")
    driver.wait_for_visual_change(
        "pointer drag paints the selection while the button is held",
        before_drag,
        crop=drag_crop,
        minimum_pixels=200,
    )
    held = driver.wait_for_stable_frame(
        "stable editor while the drag is held", crop=EDITOR_CROP
    )
    if dark_pixel_count(held, crop=drag_crop) >= grey_overlay_dark_pixels:
        raise AcceptanceFailure("a second grey overlay is painted while dragging")
    driver.release()
    released = driver.wait_for_stable_frame(
        "stable editor after the drag is released", crop=EDITOR_CROP
    )
    if image_difference(before_drag, released, crop=drag_crop) < 200:
        raise AcceptanceFailure("released drag lost its selection")
    if dark_pixel_count(released, crop=drag_crop) >= grey_overlay_dark_pixels:
        raise AcceptanceFailure("a second grey overlay remains after the release")
    driver.hover("editor_line_5_far_right")
    hovered = driver.wait_for_stable_frame(
        "stable editor after hovering elsewhere", crop=EDITOR_CROP
    )
    if image_difference(released, hovered, crop=EDITOR_CROP) != 0:
        raise AcceptanceFailure("selection kept following the pointer after release")
    driver.type_text("D")
    expected = expected.replace("line-003 delta", "line-Ddelta", 1)
    wait_until(
        "typing replaces exactly the dragged range",
        lambda: note_body(note) == expected,
    )

    # Dragging left of and below the text extends the selection to the document
    # end, and releasing outside the editor still ends the drag. The software
    # renderer paints a frame a few hundred milliseconds after the input, so the
    # extension is awaited explicitly before the released state is pinned.
    before_outside_drag = driver.wait_for_stable_frame(
        "stable editor before the outside drag", crop=EDITOR_CROP
    )
    driver.press("editor_line_1_col_5")
    driver.drag_to("editor_line_1_col_9")
    driver.drag_to("sidebar_blank")
    driver.wait_for_visual_change(
        "drag left of and below the text paints the selection to the document end",
        before_outside_drag,
        crop=EDITOR_CROP,
        minimum_pixels=2_000,
    )
    driver.release()
    outside_released = driver.wait_for_stable_frame(
        "stable editor after releasing outside the editor", crop=EDITOR_CROP
    )
    driver.hover("editor_line_3_col_9")
    outside_hovered = driver.wait_for_stable_frame(
        "stable editor after hovering back into the editor", crop=EDITOR_CROP
    )
    if image_difference(outside_released, outside_hovered, crop=EDITOR_CROP) != 0:
        raise AcceptanceFailure("drag released outside the editor kept following the pointer")
    driver.type_text("O")
    expected = "line-O"
    final_note = selection_workspace / "notes" / "line-O.md"
    wait_until(
        "drag released outside the editor keeps its range and relocates",
        lambda: final_note.is_file()
        and note_body(final_note) == expected
        and not note.exists(),
    )
    note = final_note

    driver.close_app()
    assert_no_temporary_files(selection_workspace)


def persistence_scenario(driver: WindowDriver, workspace: Path) -> None:
    notes = workspace / "notes"
    project = notes / "Project Alpha.md"
    original = read_text(project)
    boundary = original.find("\n---\n", 4)
    if boundary == -1:
        raise AcceptanceFailure("demo Project Alpha front matter is missing")
    external_marker = "EXTERNAL-CLEAN-VERSION"
    external = original[: boundary + len("\n---\n")] + f"\n{external_marker}\n"

    reference_workspace = driver.temporary_root / "external-reference-workspace"
    generate_demo_workspace(reference_workspace)
    (reference_workspace / "notes" / "Project Alpha.md").write_text(
        external, encoding="utf-8"
    )
    driver.start_app(reference_workspace, "clean-reload-reference")
    external_reference = driver.wait_for_stable_frame(
        "external-version reference frame",
        crop=EDITOR_CROP,
        minimum_dark_pixels=EXTERNAL_MARKER_MIN_DARK_PIXELS,
    )
    driver.close_app()

    driver.start_app(workspace, "clean-reload")
    driver.wait_for_stable_frame(
        "clean document before external reload",
        crop=EDITOR_CROP,
        minimum_dark_pixels=EXTERNAL_MARKER_MIN_DARK_PIXELS,
    )
    project.write_text(external, encoding="utf-8")

    def external_version_rendered() -> bool:
        current = driver.capture("external-reload")
        return image_difference(external_reference, current, crop=EDITOR_CROP) == 0

    wait_until(
        "clean external version to render",
        external_version_rendered,
        timeout=7.0,
        interval=0.05,
    )
    driver.click("editor")
    ui_marker = "ui-after-clean-reload"
    driver.type_text(ui_marker)
    # The click intentionally inserts at the start of the externally reloaded
    # title. Finish the new first line before the old title so relocation is
    # derived from ui_marker while the external bytes remain in the body.
    driver.key("Return")
    relocated_project = notes / f"{ui_marker}.md"
    wait_until(
        "post-clean-reload autosave and first-line relocation",
        lambda: contains(relocated_project, external_marker)
        and contains(relocated_project, ui_marker)
        and not project.exists(),
    )
    driver.close_app()

    save_activity_workspace = create_workspace(
        driver.temporary_root, "save-activity-workspace"
    )
    save_activity_note = save_activity_workspace / "notes" / "A Loader.md"
    save_activity_note.write_text(
        "---\n"
        "favorited: false\n"
        "pinned: false\n"
        "tags: []\n"
        "title: A Loader\n"
        "created: '2026-09-01T00:00:00.000Z'\n"
        "modified: '2026-09-01T00:00:00.000Z'\n"
        "---\n"
        "loader-body\n",
        encoding="utf-8",
    )
    make_workspace_accessible(save_activity_workspace)
    save_activity_layout = {"counts": {"all": 1, "favorites": 0}, "categories": ()}
    sidebar_indicator_crop = (
        SIDEBAR_WIDTH - 36,
        note_row_top(0, expanded="all", **save_activity_layout) + 6,
        20,
        18,
    )
    driver.start_app(save_activity_workspace, "save-activity")
    clean_sidebar = driver.wait_for_stable_frame(
        "clean sidebar before save activity", crop=sidebar_indicator_crop
    )
    driver.click("editor")
    driver.type_text("x")
    saved_activity_note = save_activity_workspace / "notes" / "xloader-body.md"
    wait_until(
        "save-activity note autosave",
        lambda: saved_activity_note.is_file() and not save_activity_note.exists(),
    )
    settled_sidebar = driver.wait_for_stable_frame(
        "sidebar after save activity",
        crop=sidebar_indicator_crop,
        stable_for=0.25,
    )
    if image_difference(clean_sidebar, settled_sidebar, crop=sidebar_indicator_crop) != 0:
        raise AcceptanceFailure("save activity changed the sidebar indicator gutter")
    driver.close_app()

    retry_workspace = create_workspace(driver.temporary_root, "retry-workspace")
    retry_note = retry_workspace / "notes" / "A Retry.md"
    retry_note.write_text(
        "---\n"
        "favorited: false\n"
        "pinned: false\n"
        "tags: []\n"
        "title: A Retry\n"
        "created: '2026-09-01T00:00:00.000Z'\n"
        "modified: '2026-09-01T00:00:00.000Z'\n"
        "---\n"
        "retry-body\n",
        encoding="utf-8",
    )
    make_workspace_accessible(retry_workspace)
    driver.start_app(retry_workspace, "retry", run_as_uid=65_534)
    retry_reference = driver.wait_for_stable_frame("clean retry footer")
    (retry_workspace / "notes").chmod(0o555)
    driver.click("editor")
    retry_marker = "save-error-retry-marker"
    driver.type_text(retry_marker)
    wait_until(
        "recovery artifact during failed save",
        lambda: bool(recovery_files(retry_workspace)),
        timeout=3.0,
        interval=0.01,
    )
    driver.wait_for_visual_change(
        "retry action after write failure",
        retry_reference,
        crop=(1_158, 766, 36, 34),
        minimum_pixels=100,
        timeout=5.0,
    )
    (retry_workspace / "notes").chmod(0o777)
    saved_retry_note = (
        retry_workspace / "notes" / f"{retry_marker}retry-body.md"
    )
    driver.click_until(
        "footer_retry",
        "successful save retry",
        lambda: contains(saved_retry_note, retry_marker) and not retry_note.exists(),
        timeout=6.0,
    )
    wait_until(
        "recovery cleanup after retry",
        lambda: not recovery_files(retry_workspace),
    )
    driver.close_app()
    assert_no_temporary_files(workspace)
    assert_no_temporary_files(save_activity_workspace)
    assert_no_temporary_files(retry_workspace)


def recovery_scenario(driver: WindowDriver, workspace: Path) -> None:
    project = workspace / "notes" / "Project Alpha.md"
    canonical_before = project.read_bytes()
    marker = "recovery-click-marker"

    driver.start_app(workspace, "recovery-crash")
    driver.click("editor")
    driver.type_text(marker)
    wait_until(
        "a recovery artifact before canonical autosave",
        lambda: bool(recovery_files(workspace)),
        timeout=3.0,
        interval=0.01,
    )
    driver.crash_app()
    if project.read_bytes() != canonical_before:
        raise AcceptanceFailure("canonical note changed before the simulated crash")

    driver.start_app(workspace, "recovery-restore")
    time.sleep(0.25)
    driver.click("footer_action")
    wait_until(
        "recovered text to save and recovery artifact to be removed",
        lambda: contains(project, marker) and not recovery_files(workspace),
    )
    driver.close_app()
    if not contains(project, marker):
        raise AcceptanceFailure("recovery click did not restore the local marker")
    assert_no_temporary_files(workspace)


def conflict_scenario(driver: WindowDriver, workspace: Path) -> None:
    notes = workspace / "notes"
    project = notes / "Project Alpha.md"
    external_source = notes / "Reading List.md"
    external_bytes = external_source.read_bytes()
    local_marker = "local-conflict-marker"
    post_reload_marker = "post-reload-marker"

    driver.start_app(workspace, "conflict")
    driver.click("editor")
    driver.type_text(local_marker)
    wait_until(
        "a local recovery artifact before external replacement",
        lambda: bool(recovery_files(workspace)),
        timeout=3.0,
        interval=0.01,
    )
    project.write_bytes(external_bytes)
    driver.click_until(
        "footer_action",
        "disk reload to remove the conflict recovery artifact",
        lambda: project.read_bytes() == external_bytes and not recovery_files(workspace),
        timeout=7.0,
    )
    if local_marker in read_text(project):
        raise AcceptanceFailure("conflicting local text overwrote the external version")

    driver.click("editor")
    driver.type_text(post_reload_marker)
    reloaded_project = notes / f"{post_reload_marker}# Reading List.md"
    wait_until(
        "post-reload editor autosave and first-line relocation",
        lambda: contains(reloaded_project, post_reload_marker) and not project.exists(),
    )
    time.sleep(0.2)
    driver.close_app()
    project_text = read_text(reloaded_project)
    if local_marker in project_text or post_reload_marker not in project_text:
        raise AcceptanceFailure("reload action did not establish the external document")
    if external_source.read_bytes() != external_bytes:
        raise AcceptanceFailure("external source fixture changed during conflict acceptance")
    if recovery_files(workspace):
        raise AcceptanceFailure("recovery artifact remained after conflict reload and autosave")
    assert_no_temporary_files(workspace)


def secure_scenario(
    driver: WindowDriver, workspace: Path, *, password_dialog_only: bool = False
) -> None:
    del workspace
    title = "Vault Lifecycle 0024"
    body_marker = "securelifecyclebodymarker0024"
    tag = "securelifecycletag0024"
    tag_query = "securetagquery0028"
    edit_marker = "securelifecycleeditmarker0024"
    search_proof_marker = "securelifecyclesearchproof0024"
    password = "stillus acceptance password 0024"
    wrong_password = "definitely wrong password"
    driver.register_sensitive(
        title,
        body_marker,
        tag,
        tag_query,
        edit_marker,
        search_proof_marker,
        password,
        wrong_password,
    )
    secure_workspace, plaintext = create_secure_workspace(
        driver.temporary_root,
        "secure-lifecycle-workspace",
        title=title,
        body=body_marker,
        tag=tag,
    )

    driver.start_app(secure_workspace, "protect")
    search_generation = wait_for_plaintext_search_generation(
        secure_workspace,
        original_relative_path=f"notes/{title}.md",
        markers=(body_marker, tag),
    )
    prove_plaintext_search_result(
        driver,
        secure_workspace,
        plaintext_note=plaintext,
        query=body_marker,
        proof_marker=search_proof_marker,
    )
    protected = protect_selected_note(
        driver,
        secure_workspace,
        plaintext,
        password,
        verify_password_caret=password_dialog_only,
    )
    protected_relative_path = protected.relative_to(secure_workspace).as_posix()
    ciphertext_before_unlock = protected.read_bytes()
    encrypted_note_body(protected)
    if title.encode() not in ciphertext_before_unlock or tag.encode() not in ciphertext_before_unlock:
        raise AcceptanceFailure("protected canonical note hid public title or tags")
    if body_marker.encode() in ciphertext_before_unlock or search_proof_marker.encode() in ciphertext_before_unlock:
        raise AcceptanceFailure("protected canonical note exposed Markdown body")
    wait_for_search_purge(
        secure_workspace,
        previous_generation=search_generation,
        original_relative_path=f"notes/{title}.md",
        protected_relative_path=protected_relative_path,
        markers=(body_marker, search_proof_marker),
    )
    wait_for_plaintext_purge(secure_workspace, body_marker, search_proof_marker)
    assert_search_excludes_protected(
        secure_workspace,
        original_relative_path=f"notes/{title}.md",
        protected_relative_path=protected_relative_path,
        markers=(body_marker, search_proof_marker),
    )
    driver.close_app()
    driver.start_app(secure_workspace, "wrong-unlock")

    locked_counts = {"all": 1, "favorites": 0, tag: 1}
    driver.click_note(0, counts=locked_counts, categories=(tag,))
    driver.wait_for_password_dialog()
    driver.key("Escape")
    driver.wait_for_password_dialog(opened=False)
    assert_locked_editor_inaccessible(
        driver,
        markers=(title, body_marker, tag, edit_marker, search_proof_marker),
    )

    driver.click_note(0, counts=locked_counts, categories=(tag,))
    if password_dialog_only:
        assert_password_button_hover_geometry(driver)
        assert_masked_password_caret(
            driver,
            "empty unlock password",
            "password_unlock_primary",
            (105, 112, 121),
            before_text=True,
        )
    else:
        driver.click("password_unlock_primary")
    driver.type_sensitive_text(wrong_password)
    before_verification = (
        driver.capture("before-password-verification") if password_dialog_only else None
    )
    if password_dialog_only:
        driver.click("password_unlock_submit")
    else:
        driver.key("Return")
    if before_verification is not None:
        assert_password_verification_feedback(driver, before_verification)
    if protected.read_bytes() != ciphertext_before_unlock:
        raise AcceptanceFailure("wrong password changed protected ciphertext")
    if not plaintext.exists() or protected_note_files(secure_workspace) != [plaintext]:
        raise AcceptanceFailure("wrong password changed the title-derived protected-note identity")
    assert_focused_lock_inaccessible(
        driver,
        markers=(title, body_marker, tag, edit_marker, search_proof_marker),
    )
    driver.wait_for_stable_frame("wrong-password result before retry")
    time.sleep(0.5)

    # The rejected password is cleared while the neutral error stays open, so the
    # correct password can be entered after an explicit field click without
    # reopening the dialog. Copy/cut/select-all remain inert and never replace
    # the clipboard sentinel with the password.
    driver.click("password_unlock_primary")
    driver.type_sensitive_text(password)
    clipboard_sentinel = "stillus-password-clipboard-sentinel"
    set_clipboard_text(driver.environment, clipboard_sentinel)
    driver.click("password_unlock_primary")
    driver.key("ctrl+a")
    driver.key("ctrl+c")
    if clipboard_text(driver.environment) != clipboard_sentinel:
        raise AcceptanceFailure("password copy changed the system clipboard")
    set_clipboard_text(driver.environment, clipboard_sentinel)
    driver.key("ctrl+x")
    if clipboard_text(driver.environment) != clipboard_sentinel:
        raise AcceptanceFailure("password cut changed the system clipboard")
    driver.key("Return")
    wait_for_unlocked_editor(driver, body_marker)
    if protected.read_bytes() != ciphertext_before_unlock:
        raise AcceptanceFailure("unlock without edits rewrote protected ciphertext")
    if password_dialog_only:
        lock_selected_note(driver)
        assert_locked_editor_inaccessible(
            driver,
            markers=(title, body_marker, tag, edit_marker, search_proof_marker),
        )
        unlock_selected_note(driver, password, categories=(tag,))
        wait_for_unlocked_editor(driver, body_marker)
        disable_selected_note_protection(driver)
        wait_until(
            "protected note saved back as plaintext",
            lambda: protected.is_file()
            and ARMORED_AGE_PREFIX not in protected.read_bytes()
            and body_marker.encode() in protected.read_bytes(),
            timeout=10.0,
            interval=0.03,
        )
        if protected_note_files(secure_workspace):
            raise AcceptanceFailure("disable protection left an encrypted canonical note")
        driver.close_app()
        assert_no_temporary_files(secure_workspace)
        return

    driver.key("Right")
    driver.key("Return")
    driver.type_sensitive_text(edit_marker)
    wait_until(
        "encrypted autosave and recovery cleanup",
        lambda: protected.read_bytes() != ciphertext_before_unlock
        and not recovery_files(secure_workspace),
        timeout=8.0,
        interval=0.02,
    )
    wait_for_file_bytes_stable(protected, "final encrypted autosave before lock")
    wait_for_plaintext_purge(secure_workspace, body_marker, edit_marker)

    focus_tag_input(driver, 1, "secure tag popover before lock")
    driver.type_sensitive_text(tag_query)
    ciphertext_before_tag_lock = protected.read_bytes()
    lock_selected_note(driver)
    assert_locked_editor_inaccessible(
        driver,
        markers=(title, body_marker, tag, tag_query, edit_marker, search_proof_marker),
    )
    if protected.read_bytes() != ciphertext_before_tag_lock:
        raise AcceptanceFailure("locking an open tag popover rewrote protected ciphertext")
    wait_for_plaintext_purge(secure_workspace, body_marker, edit_marker)
    unlock_selected_note(driver, password, categories=(tag,))
    wait_for_unlocked_editor(driver, edit_marker)
    focus_tag_input(driver, 1, "secure empty tag popover after unlock")
    ciphertext_before_empty_tag_submit = protected.read_bytes()
    driver.key("Return")
    time.sleep(0.6)
    protected_after_empty_tag_submit = protected.read_bytes()
    if protected_after_empty_tag_submit != ciphertext_before_empty_tag_submit:
        before_prefix = ciphertext_before_empty_tag_submit.split(ARMORED_AGE_PREFIX, 1)[0]
        after_prefix = protected_after_empty_tag_submit.split(ARMORED_AGE_PREFIX, 1)[0]
        changed_prefix_fields = sorted(
            {
                line.split(b":", 1)[0].strip().decode("ascii", errors="replace")
                for before_line, after_line in zip(
                    before_prefix.splitlines(), after_prefix.splitlines()
                )
                if before_line != after_line
                for line in (before_line, after_line)
                if b":" in line
            }
        )
        detail = (
            "stale tag query was persisted"
            if tag_query.encode() in protected_after_empty_tag_submit
            else (
                "public metadata changed without the stale tag query "
                f"(fields={changed_prefix_fields})"
                if before_prefix != after_prefix
                else "only the encrypted body was regenerated"
            )
        )
        raise AcceptanceFailure(
            f"tag query survived lock and mutated encrypted metadata: {detail}"
        )
    encrypted_edit_body = encrypted_note_body(protected)
    driver.key("Escape")
    lock_selected_note(driver)
    assert_locked_editor_inaccessible(
        driver, markers=(title, body_marker, tag, tag_query, edit_marker)
    )
    driver.close_app()
    driver.start_app(secure_workspace, "locked-reopen")
    # Restoring the selected protected note opens an unlock dialog. Dismiss
    # that dialog before testing title search instead of typing into it.
    driver.wait_for_password_dialog()
    driver.key("Escape")
    driver.wait_for_password_dialog(opened=False)
    driver.key("ctrl+k")
    wait_for_search_visibility(driver, opened=True)
    driver.type_sensitive_text(title)
    driver.move_to("editor")
    wait_for_private_search_results(driver)
    driver.key("Return")
    driver.wait_for_password_dialog()
    driver.key("Escape")
    driver.wait_for_password_dialog(opened=False)
    driver.click_note(0, counts=locked_counts, categories=(tag,))
    driver.wait_for_password_dialog()
    driver.key("Escape")
    driver.wait_for_password_dialog(opened=False)
    assert_locked_editor_inaccessible(
        driver, markers=(title, body_marker, tag, edit_marker)
    )
    armored_before_locked_metadata = encrypted_note_body(protected)
    focus_tag_input(driver, 1, "secure locked metadata tag popover")
    driver.type_sensitive_text(tag_query)
    driver.key("Return")
    wait_until(
        "locked protected tag edit",
        lambda: tag_query.encode() in protected.read_bytes(),
        timeout=6.0,
        interval=0.03,
    )
    # Canonical replace happens in the worker before the UI thread consumes
    # its completion event and enables the next secure metadata action.
    time.sleep(0.25)
    driver.click("pin")
    wait_until(
        "locked protected pin edit",
        lambda: b"pinned: true" in protected.read_bytes(),
        timeout=6.0,
        interval=0.03,
    )
    time.sleep(0.25)
    driver.click("favorite")
    wait_until(
        "locked protected favorite edit",
        lambda: b"favorited: true" in protected.read_bytes(),
        timeout=6.0,
        interval=0.03,
    )
    time.sleep(0.25)
    if encrypted_note_body(protected) != armored_before_locked_metadata:
        raise AcceptanceFailure("locked metadata edit rewrote encrypted body bytes")
    driver.key("ctrl+k")
    wait_for_search_visibility(driver, opened=True)
    driver.type_sensitive_text(tag_query)
    driver.move_to("editor")
    wait_for_private_search_results(driver)
    driver.key("Return")
    driver.click("password_unlock_primary")
    driver.type_sensitive_text(password)
    driver.key("Return")
    wait_for_unlocked_editor(driver, edit_marker)
    reopened = clipboard_text(driver.environment) or ""
    if body_marker not in reopened:
        raise AcceptanceFailure("reopened protected edit lost the original note body")
    lock_selected_note(driver)
    assert_locked_editor_inaccessible(
        driver, markers=(title, body_marker, tag, edit_marker)
    )
    driver.close_app()

    if encrypted_note_body(protected) != encrypted_edit_body:
        raise AcceptanceFailure("relock/reopen or locked metadata edits rewrote encrypted body")
    assert_plaintext_absent(
        secure_workspace,
        body_marker,
        edit_marker,
        search_proof_marker,
    )
    assert_search_excludes_protected(
        secure_workspace,
        original_relative_path=f"notes/{title}.md",
        protected_relative_path=protected_relative_path,
        markers=(body_marker, edit_marker, search_proof_marker),
    )
    assert_logs_redacted(
        driver,
        title,
        body_marker,
        tag,
        tag_query,
        edit_marker,
        search_proof_marker,
        password,
        wrong_password,
    )
    assert_no_temporary_files(secure_workspace)


def password_dialog_scenario(driver: WindowDriver, workspace: Path) -> None:
    secure_scenario(driver, workspace, password_dialog_only=True)


def password_change_scenario(driver: WindowDriver, workspace: Path) -> None:
    del workspace
    driver.set_stage("prepare")
    title = "Password Change 0031"
    body_marker = "passwordchangebodymarker0031"
    tag = "passwordchangetag0031"
    old_password = "stillus old password 0031"
    new_password = "stillus new password 0031"
    driver.register_sensitive(body_marker, old_password, new_password)
    secure_workspace, plaintext = create_secure_workspace(
        driver.temporary_root,
        "password-change-workspace",
        title=title,
        body=body_marker,
        tag=tag,
    )

    driver.start_app(secure_workspace, "change")
    driver.set_stage("protect")
    protected = protect_selected_note(driver, secure_workspace, plaintext, old_password)
    old_ciphertext = protected.read_bytes()

    driver.set_stage("settings")
    driver.click("settings")
    driver.click("settings_encryption")
    driver.set_stage("empty/validation")
    driver.click("encryption_submit")
    if protected.read_bytes() != old_ciphertext:
        raise AcceptanceFailure("empty password validation changed protected ciphertext")
    driver.set_stage("clipboard")
    driver.click("encryption_current")
    driver.type_sensitive_text(old_password)
    clipboard_sentinel = "stillus-password-change-clipboard-sentinel"
    set_clipboard_text(driver.environment, clipboard_sentinel)
    driver.key("ctrl+a")
    driver.key("ctrl+c")
    if clipboard_text(driver.environment) != clipboard_sentinel:
        raise AcceptanceFailure("current master password copy changed the clipboard")
    driver.set_stage("confirmation/validation")
    driver.click("encryption_new")
    driver.type_sensitive_text(new_password)
    driver.click("encryption_confirmation")
    mismatched_confirmation = "stillus mismatched password 0031"
    driver.register_sensitive(mismatched_confirmation)
    driver.type_sensitive_text(mismatched_confirmation)
    driver.click("encryption_submit")
    if protected.read_bytes() != old_ciphertext:
        raise AcceptanceFailure("password mismatch validation changed protected ciphertext")
    driver.set_stage("rotate")
    driver.click("encryption_confirmation")
    for _ in mismatched_confirmation:
        driver.key("BackSpace")
    driver.type_sensitive_text(new_password)
    driver.click("encryption_submit")

    wait_until(
        "transactional master-password replacement",
        lambda: protected.read_bytes() != old_ciphertext,
        timeout=10.0,
        interval=0.03,
    )
    new_ciphertext = wait_for_file_bytes_stable(
        protected, "stable ciphertext after master-password replacement"
    )
    if body_marker.encode() in new_ciphertext:
        raise AcceptanceFailure("password change exposed protected plaintext")
    driver.set_stage("backup")
    transaction_root = (
        secure_workspace / ".stillus_backups" / "secure" / "transactions"
    )
    wait_until(
        "password-change transaction cleanup",
        lambda: not transaction_root.exists() or not any(transaction_root.iterdir()),
        timeout=6.0,
    )
    backup_files = [
        path
        for path in (secure_workspace / ".stillus_backups" / "secure").rglob("*")
        if path.is_file() and path.name != "manifest.json"
    ]
    if not any(path.read_bytes() == old_ciphertext for path in backup_files):
        raise AcceptanceFailure("password change did not retain the old ciphertext backup")
    driver.set_stage("restart")
    driver.close_app()

    driver.start_app(secure_workspace, "verify-new-password")
    locked_counts = {"all": 1, "favorites": 0, tag: 1}
    driver.click_note(0, counts=locked_counts, categories=(tag,))
    driver.wait_for_password_dialog()
    driver.set_stage("old/rejection")
    driver.click_point(760, 385)
    driver.type_sensitive_text(old_password)
    driver.key("Return")

    def old_password_rejection_is_painted() -> bool:
        frame = driver.capture("password-change-old-password-rejected")
        try:
            return near_color_pixel_count(
                frame, (164, 69, 69), crop=(425, 300, 390, 220)
            ) >= 8
        finally:
            frame.unlink(missing_ok=True)

    wait_until(
        "old password rejection before retrying with the new password",
        old_password_rejection_is_painted,
        timeout=6.0,
        interval=0.03,
    )
    assert_focused_lock_inaccessible(driver, markers=(body_marker,))
    driver.set_stage("new/unlock")
    # Focus the field interior after rejection, not the original card's border.
    driver.click_point(760, 385)
    driver.type_sensitive_text(new_password)
    driver.key("Return")
    wait_for_unlocked_editor(driver, body_marker)
    driver.set_stage("final/validation")
    driver.close_app()

    assert_no_temporary_files(secure_workspace)
    assert_logs_redacted(driver, body_marker, old_password, new_password)


def secure_recovery_scenario(driver: WindowDriver, workspace: Path) -> None:
    del workspace
    title = "Vault Recovery 0024"
    body_marker = "securerecoverybodymarker0024"
    tag = "securerecoverytag0024"
    recovery_marker = "securerecoverycrashmarker0024"
    search_proof_marker = "securerecoverysearchproof0024"
    password = "stillus recovery password 0024"
    driver.register_sensitive(
        title, body_marker, tag, recovery_marker, search_proof_marker, password
    )
    secure_workspace, plaintext = create_secure_workspace(
        driver.temporary_root,
        "secure-recovery-workspace",
        title=title,
        body=body_marker,
        tag=tag,
    )

    driver.start_app(secure_workspace, "setup")
    search_generation = wait_for_plaintext_search_generation(
        secure_workspace,
        original_relative_path=f"notes/{title}.md",
        markers=(body_marker, tag),
    )
    prove_plaintext_search_result(
        driver,
        secure_workspace,
        plaintext_note=plaintext,
        query=body_marker,
        proof_marker=search_proof_marker,
    )
    protected = protect_selected_note(driver, secure_workspace, plaintext, password)
    protected_relative_path = protected.relative_to(secure_workspace).as_posix()
    wait_for_search_purge(
        secure_workspace,
        previous_generation=search_generation,
        original_relative_path=f"notes/{title}.md",
        protected_relative_path=protected_relative_path,
        markers=(body_marker, search_proof_marker),
    )
    wait_for_plaintext_purge(secure_workspace, body_marker, search_proof_marker)
    driver.close_app()
    driver.start_app(secure_workspace, "edit-before-crash")
    unlock_selected_note(driver, password, categories=(tag,))
    wait_for_unlocked_editor(driver, body_marker)
    canonical_before = protected.read_bytes()

    driver.key("Right")
    driver.key("Return")
    driver.type_sensitive_text(recovery_marker)
    # Protected autosave intentionally precedes the slower recovery snapshot.
    # Touch the canonical file in place to force a version conflict without
    # changing its decryptable contents, so the delayed recovery path remains
    # observable before the simulated crash.
    protected.write_bytes(canonical_before)
    wait_until(
        "encrypted recovery artifact after protected autosave conflict",
        lambda: len(protected_recovery_files(secure_workspace)) == 1,
        timeout=3.0,
        interval=0.01,
    )
    if len(recovery_files(secure_workspace)) != 1:
        raise AcceptanceFailure("protected edit created a plaintext recovery artifact")
    if protected.read_bytes() != canonical_before:
        raise AcceptanceFailure("protected canonical note changed before simulated crash")
    assert_plaintext_absent(
        secure_workspace,
        body_marker,
        recovery_marker,
        search_proof_marker,
    )
    driver.crash_app()
    if protected.read_bytes() != canonical_before:
        raise AcceptanceFailure("protected canonical note changed during crash")

    driver.start_app(secure_workspace, "restore")
    unlock_selected_note(driver, password, categories=(tag,))
    wait_for_unlocked_editor(driver, body_marker)
    driver.click("footer_action")
    wait_until(
        "protected recovery restore and encrypted autosave",
        lambda: protected.read_bytes() != canonical_before
        and not recovery_files(secure_workspace),
        timeout=8.0,
        interval=0.02,
    )
    wait_for_unlocked_editor(driver, recovery_marker)
    restored = clipboard_text(driver.environment) or ""
    if body_marker not in restored:
        raise AcceptanceFailure("protected recovery lost the original note body")
    lock_selected_note(driver)
    assert_locked_editor_inaccessible(
        driver, markers=(title, body_marker, tag, recovery_marker)
    )
    driver.close_app()

    encrypted_note_body(protected)
    assert_plaintext_absent(
        secure_workspace,
        body_marker,
        recovery_marker,
        search_proof_marker,
    )
    assert_logs_redacted(
        driver,
        title,
        body_marker,
        tag,
        recovery_marker,
        search_proof_marker,
        password,
    )
    assert_no_temporary_files(secure_workspace)


def secure_conflict_scenario(driver: WindowDriver, workspace: Path) -> None:
    del workspace
    title = "Vault Conflict 0024"
    body_marker = "secureconflictbodymarker0024"
    tag = "secureconflicttag0024"
    external_marker = "secureexternalciphermarker0024"
    local_marker = "securelocalconflictmarker0024"
    post_reload_marker = "securepostreloadmarker0024"
    search_proof_marker = "secureconflictsearchproof0024"
    password = "stillus conflict password 0024"
    driver.register_sensitive(
        title,
        body_marker,
        tag,
        external_marker,
        local_marker,
        post_reload_marker,
        search_proof_marker,
        password,
    )
    secure_workspace, plaintext = create_secure_workspace(
        driver.temporary_root,
        "secure-conflict-workspace",
        title=title,
        body=body_marker,
        tag=tag,
    )

    driver.start_app(secure_workspace, "external-version")
    search_generation = wait_for_plaintext_search_generation(
        secure_workspace,
        original_relative_path=f"notes/{title}.md",
        markers=(body_marker, tag),
    )
    prove_plaintext_search_result(
        driver,
        secure_workspace,
        plaintext_note=plaintext,
        query=body_marker,
        proof_marker=search_proof_marker,
    )
    protected = protect_selected_note(driver, secure_workspace, plaintext, password)
    protected_relative_path = protected.relative_to(secure_workspace).as_posix()
    wait_for_search_purge(
        secure_workspace,
        previous_generation=search_generation,
        original_relative_path=f"notes/{title}.md",
        protected_relative_path=protected_relative_path,
        markers=(body_marker, search_proof_marker),
    )
    wait_for_plaintext_purge(secure_workspace, body_marker, search_proof_marker)
    base_ciphertext = protected.read_bytes()
    driver.close_app()
    driver.start_app(secure_workspace, "external-version-edit")
    unlock_selected_note(driver, password, categories=(tag,))
    wait_for_unlocked_editor(driver, body_marker)
    driver.key("Right")
    driver.key("Return")
    driver.type_sensitive_text(external_marker)
    wait_until(
        "external encrypted fixture autosave",
        lambda: protected.read_bytes() != base_ciphertext
        and not recovery_files(secure_workspace),
        timeout=8.0,
        interval=0.02,
    )
    external_ciphertext = protected.read_bytes()
    lock_selected_note(driver)
    assert_locked_editor_inaccessible(
        driver, markers=(title, body_marker, tag, external_marker)
    )
    driver.close_app()

    protected.write_bytes(base_ciphertext)
    driver.start_app(secure_workspace, "conflict")
    unlock_selected_note(driver, password, categories=(tag,))
    wait_for_unlocked_editor(driver, body_marker)
    if external_marker in (clipboard_text(driver.environment) or ""):
        raise AcceptanceFailure("base encrypted fixture unexpectedly contains external edit")
    clean_footer = driver.capture_safe_footer("before-local-recovery")
    driver.key("Right")
    driver.key("Return")
    driver.type_sensitive_text(local_marker)
    # Introduce the external version before the protected autosave deadline.
    # The save must reject the stale base and the later recovery snapshot must
    # retain the local edit for an explicit user decision.
    protected.write_bytes(external_ciphertext)
    wait_until(
        "encrypted local recovery after external ciphertext replacement",
        lambda: len(protected_recovery_files(secure_workspace)) == 1,
        timeout=3.0,
        interval=0.01,
    )
    if protected.read_bytes() != external_ciphertext:
        raise AcceptanceFailure("local protected edit overwrote the external conflict fixture")
    driver.wait_for_footer_change(
        "visible protected recovery action before external replacement",
        clean_footer,
        minimum_pixels=30,
    )
    clean_footer.unlink(missing_ok=True)

    recovery_footer = driver.wait_for_stable_footer_action(
        "stable protected recovery action before external replacement"
    )
    driver.wait_for_footer_change(
        "visible load-from-disk action after external ciphertext conflict",
        recovery_footer,
        minimum_pixels=30,
        timeout=7.0,
    )
    recovery_footer.unlink(missing_ok=True)
    if not recovery_files(secure_workspace):
        raise AcceptanceFailure("conflict recovery disappeared before load-from-disk click")
    driver.click("footer_action")
    driver.click_until(
        "footer_action",
        "external ciphertext reload and protected recovery cleanup",
        lambda: protected.read_bytes() == external_ciphertext
        and not recovery_files(secure_workspace),
        timeout=7.0,
    )
    wait_for_unlocked_editor(driver, external_marker)
    copied = clipboard_text(driver.environment) or ""
    if local_marker in copied:
        raise AcceptanceFailure("load-from-disk kept the conflicting protected local edit")

    driver.key("Right")
    driver.key("Return")
    driver.type_sensitive_text(post_reload_marker)
    wait_until(
        "post-conflict encrypted autosave",
        lambda: protected.read_bytes() != external_ciphertext
        and not recovery_files(secure_workspace),
        timeout=8.0,
        interval=0.02,
    )
    post_conflict_ciphertext = protected.read_bytes()
    lock_selected_note(driver)
    assert_locked_editor_inaccessible(
        driver,
        markers=(
            title,
            body_marker,
            tag,
            external_marker,
            local_marker,
            post_reload_marker,
            search_proof_marker,
        ),
    )
    driver.close_app()

    driver.start_app(secure_workspace, "post-conflict-reopen")
    unlock_selected_note(driver, password, categories=(tag,))
    wait_for_unlocked_editor(driver, post_reload_marker)
    reopened = clipboard_text(driver.environment) or ""
    has_external = external_marker in reopened
    has_local = local_marker in reopened
    has_body = body_marker in reopened
    if not has_body or not has_external or has_local:
        raise AcceptanceFailure(
            "reopened post-conflict document did not preserve the disk version exactly "
            f"(body={has_body}, external={has_external}, local={has_local})"
        )
    lock_selected_note(driver)
    assert_locked_editor_inaccessible(
        driver,
        markers=(
            title,
            body_marker,
            tag,
            external_marker,
            local_marker,
            post_reload_marker,
        ),
    )
    driver.close_app()

    encrypted_note_body(protected)
    if protected.read_bytes() != post_conflict_ciphertext:
        raise AcceptanceFailure("post-conflict reopen without edits rewrote ciphertext")
    assert_plaintext_absent(
        secure_workspace,
        body_marker,
        external_marker,
        local_marker,
        post_reload_marker,
        search_proof_marker,
    )
    assert_search_excludes_protected(
        secure_workspace,
        original_relative_path=f"notes/{title}.md",
        protected_relative_path=protected_relative_path,
        markers=(
            body_marker,
            external_marker,
            local_marker,
            post_reload_marker,
            search_proof_marker,
        ),
    )
    assert_logs_redacted(
        driver,
        title,
        body_marker,
        tag,
        external_marker,
        local_marker,
        post_reload_marker,
        search_proof_marker,
        password,
    )
    assert_no_temporary_files(secure_workspace)


def integrity_incident_pending(workspace: Path) -> bool:
    manifest = workspace / ".stillus_backups" / "secure" / "manifest.json"
    if not manifest.is_file():
        return False
    contents = json.loads(manifest.read_text(encoding="utf-8"))
    return any(note.get("pending") is not None for note in contents.get("notes", []))


def secure_integrity_scenario(driver: WindowDriver, workspace: Path) -> None:
    del workspace
    title = "Vault Integrity 0024"
    body_marker = "secureintegritybodymarker0024"
    retry_marker = "secureintegrityretrymarker0024"
    restore_marker = "secureintegrityrestoremarker0024"
    password = "stillus integrity password 0024"
    driver.register_sensitive(title, body_marker, retry_marker, restore_marker, password)
    secure_workspace, plaintext = create_secure_workspace(
        driver.temporary_root,
        "secure-integrity-workspace",
        title=title,
        body=body_marker,
        tag="secureintegritytag0024",
    )

    def wait_for_integrity_buttons() -> None:
        # The worker writes its journal before the UI receives the completion
        # and paints the modal. A journal alone is not a clickable button.
        def painted() -> bool:
            return driver.window_color_pixel_count(
                (54, 94, 130), crop=(755, 432, 60, 32), tolerance=16
            ) >= 100
        wait_until("painted integrity actions", painted, timeout=8.0, interval=0.05)

    driver.start_app(secure_workspace, "protect")
    protected = protect_selected_note(driver, secure_workspace, plaintext, password)
    driver.close_app()
    driver.start_app(secure_workspace, "retry")
    unlock_selected_note(driver, password, categories=("secureintegritytag0024",))
    wait_until(
        "initial decrypted note for integrity scenario",
        lambda: editor_clipboard_contains(driver, body_marker),
        timeout=6.0,
        interval=0.03,
    )
    before_retry = protected.read_bytes()
    trigger = secure_workspace / ".stillus" / "test-corrupt-protected-save"
    trigger.parent.mkdir(mode=0o700, exist_ok=True)
    # Insert the newline and marker as one edit: a delayed runner must not
    # autosave a standalone newline and open the blocking modal before typing.
    driver.key("Right")
    set_clipboard_text(driver.environment, "\n" + retry_marker)
    trigger.write_bytes(b"once")
    driver.key("ctrl+v")
    wait_until(
        "blocking integrity incident after protected autosave",
        lambda: integrity_incident_pending(secure_workspace),
        timeout=8.0,
        interval=0.02,
    )
    wait_for_integrity_buttons()
    driver.key("Escape")
    driver.click_point(30, 760)
    if not integrity_incident_pending(secure_workspace):
        raise AcceptanceFailure("Escape or outside click closed the integrity incident")
    driver.click("integrity_retry")
    wait_until(
        "verified retry resolving integrity journal",
        lambda: not integrity_incident_pending(secure_workspace)
        and protected.read_bytes() != before_retry,
        timeout=10.0,
        interval=0.02,
    )
    wait_until(
        "retry preserving the decrypted editor buffer",
        lambda: editor_clipboard_contains(driver, retry_marker),
        timeout=6.0,
        interval=0.03,
    )

    before_restore = protected.read_bytes()
    driver.key("Right")
    set_clipboard_text(driver.environment, "\n" + restore_marker)
    trigger.write_bytes(b"once")
    driver.key("ctrl+v")
    wait_until(
        "second blocking integrity incident",
        lambda: integrity_incident_pending(secure_workspace),
        timeout=8.0,
        interval=0.02,
    )
    wait_for_integrity_buttons()
    driver.click("integrity_restore")
    wait_until(
        "verified backup restore resolving integrity journal",
        lambda: not integrity_incident_pending(secure_workspace)
        and protected.read_bytes() == before_restore,
        timeout=10.0,
        interval=0.02,
    )
    driver.close_app()
    encrypted_note_body(protected)

    manifest = (
        secure_workspace / ".stillus_backups" / "secure" / "manifest.json"
    ).read_bytes()
    for marker in (body_marker, retry_marker, restore_marker, password):
        if marker.encode() in manifest:
            raise AcceptanceFailure("secure backup manifest leaked protected content")
    assert_plaintext_absent(
        secure_workspace,
        body_marker,
        retry_marker,
        restore_marker,
        password,
    )
    assert_logs_redacted(driver, body_marker, retry_marker, restore_marker, password)
    assert_no_temporary_files(secure_workspace)


def visual_scenario(driver: WindowDriver, workspace: Path) -> None:
    demo_before = {
        path: path.read_bytes() for path in (workspace / "notes").glob("*.md")
    }
    driver.start_app(workspace, "visual")
    clean = driver.wait_for_stable_frame("stable clean visual state")

    minimum_title_pixels = tuple(
        max(1, bright_pixel_count(clean, crop=crop) * 3 // 4)
        for crop in NOTE_TITLE_CROPS
    )

    def assert_note_titles_visible(frame: Path, transition: str) -> None:
        counts = tuple(bright_pixel_count(frame, crop=crop) for crop in NOTE_TITLE_CROPS)
        faint = [
            index
            for index, (actual, minimum) in enumerate(
                zip(counts, minimum_title_pixels, strict=True)
            )
            if actual < minimum
        ]
        if faint:
            raise AcceptanceFailure(
                f"note titles became faint after {transition}: "
                f"rows={faint}, dark_pixels={counts}, minimum={minimum_title_pixels}"
            )

    for transition, index in enumerate((1, 2, 0, 2, 1, 0), start=1):
        driver.click_note(index, settle=False)
        immediate = driver.capture(f"note-transition-{transition}-immediate")
        assert_note_titles_visible(immediate, f"immediate click {transition}")
        stable = driver.wait_for_stable_frame(
            f"stable note transition {transition}",
            crop=(0, SIDEBAR_TREE_TOP, SIDEBAR_WIDTH, 140),
        )
        assert_note_titles_visible(stable, f"stable click {transition}")

    def move_away_and_wait() -> Path:
        if driver.window_id is None:
            raise AcceptanceFailure("visual window disappeared")
        driver.xdotool(
            "mousemove",
            "--sync",
            "--window",
            driver.window_id,
            "620",
            "420",
        )
        return driver.wait_for_stable_frame("stable frame after tooltip dismissal")

    for control in (
        "create_menu",
        "search",
        "note_find",
        "tag_manager",
        "pin",
        "favorite",
        "trash",
    ):
        normal_state = move_away_and_wait()
        driver.move_to(control)
        driver.wait_for_visual_change(
            f"{control} tooltip surface",
            normal_state,
            crop=(0, 0, SCREEN_WIDTH, SCREEN_HEIGHT),
            minimum_pixels=1_500,
            timeout=3.0,
        )

    move_away_and_wait()
    driver.close_app()

    changed = [path for path, contents in demo_before.items() if path.read_bytes() != contents]
    if changed:
        raise AcceptanceFailure(f"visual acceptance modified demo notes: {changed}")


SEARCH_RESULTS_CROP = (12, 98, 232, 330)
SEARCH_FIRST_RESULT_CROP = (12, 98, 232, 56)
SEARCH_WAIT_SECONDS = 10.0


def wait_for_private_search_results(driver: WindowDriver) -> None:
    """The secure scenarios must never persist result snippets or note pixels."""
    previous: str | None = None
    stable_since: float | None = None

    def ready() -> bool:
        nonlocal previous, stable_since
        current = driver.window_image_signature(SEARCH_RESULTS_CROP)
        painted = (
            driver.window_color_pixel_count(
                (57, 66, 78), crop=SEARCH_FIRST_RESULT_CROP, tolerance=2
            ) >= 1_000
            and driver.window_color_pixel_count(
                (143, 184, 220), crop=(204, 98, 36, 78), tolerance=40
            ) >= 8
            and driver.window_color_pixel_count(
                (244, 246, 248), crop=(20, 98, 180, 78), tolerance=50
            ) >= 20
        )
        unchanged = current == previous
        previous = current
        if not painted or not unchanged:
            stable_since = None
            return False
        now = time.monotonic()
        if stable_since is None:
            stable_since = now
        return now - stable_since >= 0.25

    wait_until("painted current private search results", ready, timeout=SEARCH_WAIT_SECONDS)


def wait_for_search_visibility(driver: WindowDriver, *, opened: bool) -> None:
    # Sample the close button's background, outside text and the blinking caret.
    # The tree shown after search closes has no button at this location.
    def matches() -> bool:
        pixels = driver.window_color_pixel_count(
            (57, 66, 78), crop=(214, 56, 28, 28), tolerance=2
        )
        return pixels >= 400 if opened else pixels < 100

    wait_until("search controls visibility", matches, timeout=SEARCH_WAIT_SECONDS)


def wait_for_search_results(driver: WindowDriver) -> None:
    """Require a painted result row, then a stable result list (not the input)."""
    driver.set_stage("query/results")
    previous: Path | None = None
    stable_since: float | None = None

    def ready() -> bool:
        nonlocal previous, stable_since
        current = driver.capture("search-results-ready")
        try:
            painted = (
                near_color_pixel_count(current, (57, 66, 78),
                                       crop=SEARCH_FIRST_RESULT_CROP, tolerance=2) >= 1_000
                and near_color_pixel_count(current, (143, 184, 220),
                                           crop=(204, 98, 36, 78), tolerance=40) >= 8
                and bright_pixel_count(current, crop=SEARCH_FIRST_RESULT_CROP) >= 20
            )
            unchanged = previous is not None and image_difference(
                previous, current, crop=SEARCH_RESULTS_CROP
            ) == 0
        finally:
            if previous is not None:
                previous.unlink(missing_ok=True)
            previous = current
        if not painted or not unchanged:
            stable_since = None
            return False
        now = time.monotonic()
        if stable_since is None:
            stable_since = now
        return now - stable_since >= 0.25

    try:
        wait_until("painted current search results", ready, timeout=SEARCH_WAIT_SECONDS)
    finally:
        if previous is not None:
            previous.unlink(missing_ok=True)


def select_search_result_and_edit(
    driver: WindowDriver, workspace: Path, path: Path, marker: str, *, click: bool = False
) -> None:
    driver.set_stage("selection/open")
    if click:
        driver.click_point(128, 124)
    else:
        driver.key("Return")
    wait_for_search_visibility(driver, opened=False)
    expected = path.relative_to(workspace).as_posix()

    def selected() -> bool:
        settings = workspace / ".stillus" / "settings.json"
        if not settings.is_file():
            return False
        saved = json.loads(read_text(settings))
        return saved.get("selected_note") == expected

    wait_until("expected search note selection", selected, timeout=SEARCH_WAIT_SECONDS)
    driver.wait_for_stable_frame(
        "selected search result before editing", crop=EDITOR_CROP,
        minimum_dark_pixels=100, stable_for=0.15, timeout=SEARCH_WAIT_SECONDS,
        ignore_editor_caret=True,
    )
    driver.set_stage("selection/save")
    driver.click("editor_below_document")
    driver.key("Return")
    driver.type_text(marker)
    wait_until("search selection autosave", lambda: contains(path, marker),
               timeout=SEARCH_WAIT_SECONDS)


def search_scenario(driver: WindowDriver, workspace: Path) -> None:
    notes = workspace / "notes"
    title_note = notes / "Orbit Launch Plan.md"
    tag_note = notes / "Tag Reference.md"
    body_note = notes / "Body Reference.md"
    title_note.write_text(
        "---\ntitle: 'Orbit Launch Plan'\ntags: ['Acceptance']\n---\n# Orbit Launch Plan\ntitle source body\n",
        encoding="utf-8",
    )
    tag_note.write_text(
        "---\ntitle: 'Tag Reference'\ntags: ['Galactic', 'Acceptance']\n---\n# Tag Reference\ntag source body\n",
        encoding="utf-8",
    )
    body_note.write_text(
        "---\ntitle: 'Body Reference'\ntags: ['Acceptance']\n---\n# Body Reference\nuniquebodyneedle source body\n",
        encoding="utf-8",
    )
    untouched = notes / "Reading List.md"
    untouched_before = untouched.read_bytes()
    overflow_notes = []
    for index in range(18):
        title = f"sidebaroverflowprobe {index:02d}"
        path = notes / f"{title}.md"
        write_tagged_note(path, title, ["Scrollbar"])
        overflow_notes.append(path)
    overflow_before = {path: path.read_bytes() for path in overflow_notes}

    def wait_for_index() -> bool:
        pointer = workspace / ".stillus" / "search" / "CURRENT"
        return pointer.is_file() and pointer.read_text(encoding="utf-8").startswith(
            "generation-"
        )

    def indexed_stamp_matches(path: Path) -> bool:
        search_root = workspace / ".stillus" / "search"
        pointer = search_root / "CURRENT"
        if not pointer.is_file():
            return False
        generation = pointer.read_text(encoding="utf-8").strip()
        catalog = search_root / generation / "stillus.catalog"
        if not catalog.is_file():
            return False
        relative = path.relative_to(workspace).as_posix().encode().hex()
        stat = path.stat()
        expected = f"{relative}\t{stat.st_size}\t{stat.st_mtime_ns}"
        return expected in catalog.read_text(encoding="utf-8").splitlines()

    def open_query(query: str, *, click: bool = False) -> None:
        driver.set_stage("query/results")
        if click:
            driver.click("search")
        else:
            driver.key("ctrl+k")
        wait_for_search_visibility(driver, opened=True)
        driver.type_text(query)
        driver.move_to("editor")
        wait_for_search_results(driver)

    def select_and_edit(path: Path, marker: str, *, click: bool = False) -> None:
        select_search_result_and_edit(driver, workspace, path, marker, click=click)

    driver.start_app(workspace, "search")
    driver.set_stage("initial/index")
    wait_until("initial disposable search index", wait_for_index, timeout=8.0)

    driver.click("search")
    assert_focused_input_caret(
        driver,
        "sidebar search input",
        (12, SIDEBAR_TREE_TOP, 194, 32),
        (164, 173, 184),
    )
    driver.key("Escape")

    open_query("sidebaroverflowprobe", click=True)
    search_scroll_initial = driver.wait_for_stable_frame(
        "search results scrollbar is initially idle",
        crop=(12, 110, 232, 620),
    )
    assert_transient_sidebar_scrollbar(
        driver, "search-results", search_scroll_initial
    )
    driver.key("Escape")

    open_query("Orbit", click=True)
    select_and_edit(title_note, "title-search-click-marker", click=True)

    open_query("Galactic")
    select_and_edit(tag_note, "tag-search-keyboard-marker")

    open_query("uniquebodyneedle")
    select_and_edit(body_note, "body-search-marker")

    open_query("OritLn")
    select_and_edit(title_note, "fuzzy-title-marker")

    open_query("Acceptance")
    driver.key("Down")
    driver.key("Down")
    select_and_edit(tag_note, "arrow-navigation-marker")

    driver.key("ctrl+k")
    driver.type_text("escape query")
    driver.key("Escape")
    wait_for_search_visibility(driver, opened=False)
    driver.type_text("escape-focus-marker")
    wait_until(
        "Escape returns focus to the editor",
        lambda: contains(tag_note, "escape-focus-marker"),
    )
    time.sleep(0.2)

    body_note.write_text(
        read_text(body_note) + "\nexternalindexmarker\n", encoding="utf-8"
    )
    driver.set_stage("external/index")
    wait_until(
        "external note reconciliation in the disposable index",
        lambda: indexed_stamp_matches(body_note),
        timeout=5.0,
    )
    open_query("externalindexmarker")
    select_and_edit(body_note, "external-reconcile-marker")
    driver.close_app()

    shutil.rmtree(workspace / ".stillus" / "search")
    driver.start_app(workspace, "search-rebuild")
    driver.set_stage("rebuild/index")
    wait_until("search index rebuild after deletion", wait_for_index, timeout=8.0)
    wait_until(
        "completed search generation publish",
        lambda: not list((workspace / ".stillus" / "search").glob(".building-*")),
        timeout=3.0,
    )
    wait_until(
        "rebuilt index catalog contains the body note",
        lambda: indexed_stamp_matches(body_note),
        timeout=8.0,
    )
    driver.wait_for_stable_frame("rebuilt search worker ready in the UI")
    open_query("uniquebodyneedle")
    select_and_edit(body_note, "rebuilt-index-marker")
    driver.close_app()

    driver.set_stage("final/validation")
    if untouched.read_bytes() != untouched_before:
        raise AcceptanceFailure("search changed an unrelated canonical note")
    changed_overflow_notes = [
        path for path, contents in overflow_before.items() if path.read_bytes() != contents
    ]
    if changed_overflow_notes:
        raise AcceptanceFailure(
            f"scrolling search results changed canonical notes: {changed_overflow_notes}"
        )
    required = {
        title_note: ["title-search-click-marker", "fuzzy-title-marker"],
        tag_note: [
            "tag-search-keyboard-marker",
            "arrow-navigation-marker",
            "escape-focus-marker",
        ],
        body_note: [
            "body-search-marker",
            "externalindexmarker",
            "external-reconcile-marker",
            "rebuilt-index-marker",
        ],
    }
    for path, markers in required.items():
        text = read_text(path)
        if path == body_note and "uniquebodyneedle" not in text:
            raise AcceptanceFailure("body search source marker was lost")
        missing = [marker for marker in markers if marker not in text]
        if missing:
            raise AcceptanceFailure(f"search flow missed {path.name}: {missing}")
    assert_no_temporary_files(workspace)


def find_scenario(driver: WindowDriver, workspace: Path) -> None:
    notes = workspace / "notes"
    shutil.rmtree(notes)
    notes.mkdir()
    note = notes / "Find.md"
    note.write_text(
        "---\ntitle: Find\ntags: []\n---\n"
        "# Find\n"
        + "wrapped context " * 250
        + "\n"
        "first NeEdLe line\n"
        "second needle line\n"
        "ПрИвЕт один\n"
        "привет два\n",
        encoding="utf-8",
    )
    original = note.read_bytes()

    driver.start_app(workspace, "find")
    driver.click("note_find")
    # The find input opens immediately after the left-aligned action group.
    assert_focused_input_caret(
        driver,
        "find input",
        (
            EDITOR_ACTION_GROUP_LEFT + EDITOR_ACTION_GROUP_WIDTH + EDITOR_ACTION_GAP,
            4,
            156,
            32,
        ),
        (105, 112, 121),
    )
    driver.type_text("needle")

    def first_match_is_visible() -> bool:
        # Typed keys are still being processed while the first frames settle,
        # so poll for the revealed selection instead of trusting one frame.
        probe = driver.capture("find-first-match-probe")
        try:
            return (
                near_color_pixel_count(
                    probe,
                    (229, 238, 246),
                    crop=(300, 100, 680, 640),
                    tolerance=2,
                )
                >= 20
            )
        finally:
            probe.unlink(missing_ok=True)

    wait_until(
        "note find reveals the wrapped first match inside the editor viewport",
        first_match_is_visible,
        timeout=5.0,
        interval=0.05,
    )
    driver.wait_for_stable_frame(
        "manual note find shows the first match",
        crop=(SIDEBAR_WIDTH, 0, SCREEN_WIDTH - SIDEBAR_WIDTH, SCREEN_HEIGHT),
    )
    time.sleep(1.0)
    if note.read_bytes() != original:
        raise AcceptanceFailure("opening note find changed canonical bytes")

    before_next = driver.capture("find-first-match")
    driver.key("Return")
    driver.wait_for_visual_change(
        "Enter selects the next note match",
        before_next,
        crop=EDITOR_CROP,
        minimum_pixels=20,
    )
    driver.key("Escape")
    driver.type_text("SECOND_MATCH")
    wait_until(
        "Escape restores editor focus after manual note find",
        lambda: contains(note, "SECOND_MATCH"),
    )

    driver.key("ctrl+f")
    set_clipboard_text(driver.environment, "ПРИВЕТ")
    driver.key("ctrl+v")
    driver.wait_for_stable_frame(
        "shortcut note find matches Unicode case-insensitively", crop=EDITOR_CROP
    )
    driver.key("shift+Return")
    driver.key("Escape")
    driver.type_text("CYRILLIC_MATCH")
    wait_until(
        "Shift+Enter wraps to the previous Unicode match",
        lambda: contains(note, "CYRILLIC_MATCH"),
    )
    driver.close_app()

    text = read_text(note)
    if "first NeEdLe line" not in text or "second SECOND_MATCH line" not in text:
        raise AcceptanceFailure(
            f"Enter did not replace exactly the second ASCII match: {text!r}"
        )
    if "ПрИвЕт один" not in text or "CYRILLIC_MATCH два" not in text:
        raise AcceptanceFailure("Shift+Enter did not wrap to the last Unicode match")
    if text.count("SECOND_MATCH") != 1 or text.count("CYRILLIC_MATCH") != 1:
        raise AcceptanceFailure("note find replacement changed an unexpected range")

    note_after_workspace_find = note.read_bytes()
    external = driver.temporary_root / "External Find.txt"
    external.write_text(
        "first ExTeRnAlNeedle line\n"
        "second externalneedle line\n"
        "ПрИвЕт внешний один\n"
        "привет внешний два\n",
        encoding="utf-8",
    )
    settings_path = workspace / ".stillus" / "settings.json"
    settings = json.loads(settings_path.read_text(encoding="utf-8"))
    settings["external_files"] = [
        {"engine_id": "markdown", "absolute_path": str(external.resolve())}
    ]
    settings["selected_external"] = str(external.resolve())
    settings_path.write_text(
        json.dumps(settings, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )

    driver.start_app(workspace, "find-external")
    driver.click("note_find")
    assert_focused_input_caret(
        driver,
        "external file find input opened by icon",
        (
            EDITOR_ACTION_GROUP_LEFT + EDITOR_ACTION_SIZE + EDITOR_ACTION_GAP,
            4,
            156,
            32,
        ),
        (105, 112, 121),
    )
    driver.type_text("externalneedle")
    driver.key("Return")
    driver.key("Escape")
    driver.type_text("EXTERNAL_ICON_MATCH")
    wait_until(
        "icon-driven find replaces the second external-file match",
        lambda: "second EXTERNAL_ICON_MATCH line" in read_text(external),
    )

    driver.key("ctrl+f")
    set_clipboard_text(driver.environment, "ПРИВЕТ")
    driver.key("ctrl+v")
    driver.key("shift+Return")
    driver.key("Escape")
    driver.type_text("EXTERNAL_SHORTCUT_MATCH")
    wait_until(
        "shortcut-driven find replaces the wrapped external-file match",
        lambda: "EXTERNAL_SHORTCUT_MATCH внешний два" in read_text(external),
    )
    driver.close_app()

    if note.read_bytes() != note_after_workspace_find:
        raise AcceptanceFailure("external find changed the workspace note")
    external_text = read_text(external)
    if "first ExTeRnAlNeedle line" not in external_text:
        raise AcceptanceFailure("external icon find replaced the wrong match")
    if "ПрИвЕт внешний один" not in external_text:
        raise AcceptanceFailure("external shortcut find replaced the wrong Unicode match")
    assert_no_temporary_files(workspace)


def resize_scenario(driver: WindowDriver, workspace: Path) -> None:
    notes = workspace / "notes"
    before = {path: path.read_bytes() for path in notes.glob("*.md")}
    edited = notes / "Reading List.md"

    def wait_for_boundary(description: str, expected: int) -> int:
        driver.move_to("editor_below_document")
        measured = SIDEBAR_WIDTH

        def reached() -> bool:
            nonlocal measured
            try:
                measured = sidebar_boundary_x(driver.capture("sidebar-boundary"))
            except AcceptanceFailure:
                return False
            return abs(measured - expected) <= 1

        wait_until(description, reached, interval=0.05)
        return measured

    driver.start_app(workspace, "resize")
    if wait_for_boundary("default sidebar width", SIDEBAR_WIDTH) != SIDEBAR_WIDTH:
        raise AcceptanceFailure("sidebar did not start at its 256px default width")

    driver.press("sidebar_resize_default")
    driver.drag_to("sidebar_resize_mid")
    driver.drag_to("sidebar_resize_wide")
    driver.release()
    widened = wait_for_boundary("widened sidebar", 420)
    if abs(widened - 420) > 1:
        raise AcceptanceFailure(
            f"sidebar drag widened to {widened}px instead of 420px"
        )

    driver.press("sidebar_resize_wide")
    driver.drag_to("sidebar_resize_far_left")
    driver.release()
    minimum = wait_for_boundary("minimum sidebar clamp", 180)
    if abs(minimum - 180) > 1:
        raise AcceptanceFailure(
            f"sidebar width clamped at {minimum}px instead of 180px"
        )

    driver.press("sidebar_resize_min")
    driver.drag_to("sidebar_resize_far_right")
    driver.release()
    maximum = wait_for_boundary("maximum sidebar clamp", 480)
    if abs(maximum - 480) > 1:
        raise AcceptanceFailure(
            f"sidebar width clamped at {maximum}px instead of 480px"
        )

    driver.move_to("sidebar_blank")
    released = driver.wait_for_stable_frame(
        "released resize remains stationary", crop=(0, 0, 520, SCREEN_HEIGHT)
    )
    if abs(sidebar_boundary_x(released) - 480) > 1:
        raise AcceptanceFailure("sidebar continued resizing after primary release")
    changed_before_edit = [
        path for path, contents in before.items() if path.read_bytes() != contents
    ]
    if changed_before_edit:
        raise AcceptanceFailure(
            f"sidebar resize modified canonical notes: {changed_before_edit}"
        )

    # Persist navigation state before resizing the outer window. Reading List
    # is the second All-notes row; Work remains independently expanded and is
    # also the creation group after its header was activated.
    driver.click_note(1)
    driver.click_point(
        *group_row_center(
            "Work",
            expanded_groups=("all",),
            counts=DEMO_NOTE_COUNTS,
            categories=DEMO_CATEGORIES,
        )
    )
    settings_path = workspace / ".stillus" / "settings.json"

    def durable_state_written() -> bool:
        if not settings_path.is_file():
            return False
        try:
            state = json.loads(settings_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return False
        expanded = state.get("sidebar", {}).get("expanded", [])
        return (
            state.get("version") == 1
            and state.get("sidebar", {}).get("width") == 480.0
            and {entry.get("kind") for entry in expanded} == {"all", "tag"}
            and {entry.get("tag") for entry in expanded if entry.get("kind") == "tag"}
            == {"Work"}
            and state.get("sidebar", {}).get("creation_group")
            == {"kind": "tag", "tag": "Work"}
            and state.get("selected_note") == "notes/Reading List.md"
        )

    wait_until("durable sidebar and navigation settings", durable_state_written)
    driver.resize_window(1_100, 700)
    driver.close_app()

    saved = json.loads(settings_path.read_text(encoding="utf-8"))
    if saved.get("window") != {"width": 1100.0, "height": 700.0}:
        raise AcceptanceFailure(f"window size was not flushed on close: {saved.get('window')}")
    if any(path.read_bytes() != contents for path, contents in before.items()):
        raise AcceptanceFailure("workspace settings changed canonical notes")

    driver.start_app(workspace, "resize-restored", expected_size=(1_100, 700))
    if wait_for_boundary("restored sidebar width", 480) != 480:
        raise AcceptanceFailure("sidebar width was not restored after restart")
    restored_tree = driver.wait_for_stable_frame(
        "restored expanded sidebar groups", crop=(0, 0, 480, 700)
    )
    restored_groups = ("Work", "all")
    for group in restored_groups:
        for index in range(DEMO_NOTE_COUNTS[group]):
            title_pixels = bright_pixel_count(
                restored_tree,
                crop=note_title_crop(
                    index,
                    expanded=group,
                    expanded_groups=restored_groups,
                    counts=DEMO_NOTE_COUNTS,
                    categories=DEMO_CATEGORIES,
                ),
            )
            if title_pixels < 8:
                raise AcceptanceFailure(
                    f"restored {group} row {index} is not visibly expanded"
                )

    # Typing without selecting a row proves the restored document is the last
    # selected note, not the default first note.
    driver.click("editor_below_document")
    driver.type_text("sidebar-resize-marker")
    wait_until(
        "restored selected note receives editor input",
        lambda: contains(edited, "sidebar-resize-marker"),
    )
    driver.close_app()

    changed = [path for path, contents in before.items() if path.read_bytes() != contents]
    if changed != [edited]:
        raise AcceptanceFailure(f"resize flow changed unexpected notes: {changed}")
    assert_no_temporary_files(workspace)


def creation_scenario(driver: WindowDriver, workspace: Path) -> None:
    created = workspace / "notes" / "New note.md"
    driver.start_app(workspace, "creation")
    closed = driver.wait_for_stable_frame("creation menu closed")

    driver.click("create_menu")
    driver.wait_for_visual_change(
        "creation menu with note and file choices",
        closed,
        crop=(18, 34, 190, 80),
        minimum_pixels=250,
    )
    driver.click("create_note")
    wait_until("note choice creates a note", created.is_file)

    # The RSS form replaces the choice rows inside the same card. Its submit
    # button stays disabled until the field holds an address, so the button
    # fill is the oracle for the form state.
    driver.click("create_menu")
    menu = driver.wait_for_stable_frame(
        "creation menu before the RSS form",
        crop=CREATE_POPOVER_CROP,
    )
    driver.click("create_rss")
    form = driver.wait_for_visual_change(
        "RSS form replaces the creation choices",
        menu,
        crop=CREATE_POPOVER_CROP,
        minimum_pixels=250,
    )
    idle_submit = near_color_pixel_count(form, RSS_SUBMIT_ACCENT, crop=RSS_SUBMIT_CROP)
    if idle_submit > 20:
        raise AcceptanceFailure(
            f"empty RSS form offers an enabled submit button ({idle_submit} accent pixels)"
        )
    driver.type_text("https://example.com/feed.xml")
    typed = driver.wait_for_visual_change(
        "feed address enables the submit button",
        form,
        crop=RSS_SUBMIT_CROP,
        minimum_pixels=200,
    )
    filled_submit = near_color_pixel_count(typed, RSS_SUBMIT_ACCENT, crop=RSS_SUBMIT_CROP)
    if filled_submit < 400:
        raise AcceptanceFailure(
            f"submit button stayed disabled with a feed address ({filled_submit} accent pixels)"
        )
    driver.click("rss_back")
    driver.wait_for_visual_change(
        "Назад returns to the creation choices",
        typed,
        crop=CREATE_POPOVER_CROP,
        minimum_pixels=250,
    )

    # "Назад" leaves the popover open on the choice rows, so the external file
    # picker step continues without reopening it.
    file_menu = driver.wait_for_stable_frame(
        "creation menu before the external file picker",
        crop=(18, 34, 190, 80),
    )
    driver.click("open_file")
    driver.wait_for_visual_change(
        "file choice closes the menu and hands off to the native picker",
        file_menu,
        crop=(18, 34, 190, 80),
        minimum_pixels=250,
    )
    driver.close_app()


def cached_rss_workspace(
    driver: WindowDriver, name: str, entries: list[dict]
) -> tuple[Path, Path, Path]:
    workspace = create_workspace(driver.temporary_root, name)
    (workspace / "notes" / "Note.md").write_text("Note\n", encoding="utf-8")
    url = "https://example.test/feed"
    digest = hashlib.sha256(url.encode()).hexdigest()
    item_id = f"feeds/{digest}"
    config_path = workspace / ".stillus/engines/rss/subscriptions.json"
    cache = workspace / ".stillus/cache/rss" / digest
    config_path.parent.mkdir(parents=True)
    cache.mkdir(parents=True)
    # A cached trashed feed exercises the real view without any network task.
    config_path.write_text(json.dumps({
        "version": 1, "revision": 1, "subscriptions": [{
            "id": item_id, "url": url, "title_override": "Keyboard feed",
            "created": "2026-09-01T00:00:00Z", "modified": "2026-09-01T00:00:00Z",
            "categories": [], "pinned": False, "favorited": False,
            "deleted": True, "order": {}, "revision": 1,
        }],
    }), encoding="utf-8")
    (cache / "feed.json").write_text(json.dumps({
        "title": "Keyboard feed", "etag": None, "last_modified": None,
        "fetched_at": "2026-09-01T00:00:00Z",
        "entries": entries,
    }), encoding="utf-8")
    return workspace, config_path, cache


def wait_for_rss_card_at_top(driver: WindowDriver, *, stable_for: float = 0.0) -> None:
    # Only the selected card has this accent border, 20px below the fixed
    # 56px toolbar. An open edit bar shifts it down, even in a stable frame.
    stable_since: float | None = None

    def aligned() -> bool:
        nonlocal stable_since
        if driver.window_color_pixel_count((54, 94, 130), crop=(320, 75, 600, 3)) < 500:
            stable_since = None
            return False
        now = time.monotonic()
        if stable_since is None:
            stable_since = now
        return now - stable_since >= stable_for

    wait_until("selected RSS card is top-aligned below the closed toolbar", aligned)


def rss_keyboard_scenario(driver: WindowDriver, workspace: Path) -> None:
    del workspace
    workspace, config_path, cache = cached_rss_workspace(driver, "rss-keyboard", [
        {
            "id": f"entry/{index}", "title": f"Article {index}",
            "author": "Ada Lovelace", "published": "2026-09-04T12:40:53Z", "updated": None,
            "summary": f"Text of **article {index}**. [Читать далее][1] [1]: https://example.test/article/{index}",
            "link": None,
        } for index in range(12)
    ])
    state_path = cache / "state.json"

    def read_ids() -> set[str]:
        if not state_path.exists():
            return set()
        return set(json.loads(state_path.read_text(encoding="utf-8"))["read_entry_ids"])

    def expect_read(indices: range | list[int]) -> None:
        expected = {f"entry/{index}" for index in indices}
        wait_until(f"RSS read entries {sorted(expected)}", lambda: read_ids() == expected)

    def expect_card_at_top() -> None:
        wait_for_rss_card_at_top(driver)

    driver.start_app(workspace, "sidebar")
    counts = {"favorites": 0, "all": 1, "trash": 1}
    driver.click_point(*group_row_center("trash", categories=(), counts=counts))
    driver.click_note(0, expanded_groups=("all", "trash"), expanded="trash",
                      categories=(), counts=counts)
    driver.key("j")
    expect_read([0])
    expect_card_at_top()
    driver.key("j")
    expect_read([0, 1])
    expect_card_at_top()
    # Clicking a card rebuilds its projection. Focus must survive this update.
    driver.key("k")
    driver.click_point(450, 315)
    expect_read([0, 1])
    expect_card_at_top()
    driver.key("k")
    driver.click_point(450, 155)
    driver.key("j")
    expect_read([0, 1])
    driver.key("j")
    expect_read([0, 1, 2])
    driver.key("ctrl+j")
    driver.key("shift+j")
    expect_read([0, 1, 2])
    # Typing J/K in a toolbar field edits text instead of navigating the feed.
    driver.click_point(1052, 28)
    driver.click_point(520, 80)
    driver.key("ctrl+a")
    driver.type_text("jk")
    driver.key("Return")
    wait_until("RSS rename accepts J/K text", lambda: json.loads(
        config_path.read_text(encoding="utf-8")
    )["subscriptions"][0]["title_override"] == "jk")
    expect_read([0, 1, 2])
    # Config persistence precedes the toolbar closing and restoring feed focus.
    wait_for_rss_card_at_top(driver, stable_for=0.3)
    driver.key("j")
    expect_read([0, 1, 2, 3])
    expect_card_at_top()
    # Escape discards J/K text and returns focus without a click into the feed.
    driver.click_point(1052, 28)
    driver.click_point(520, 80)
    driver.key("ctrl+a")
    driver.type_text("kj")
    driver.key("Escape")
    wait_for_rss_card_at_top(driver, stable_for=0.3)
    expect_read([0, 1, 2, 3])
    if json.loads(config_path.read_text(encoding="utf-8"))["subscriptions"][0]["title_override"] != "jk":
        raise AcceptanceFailure("Escape saved the RSS rename draft")
    driver.key("j")
    expect_read([0, 1, 2, 3, 4])
    expect_card_at_top()
    for index in range(5, 12):
        driver.key("j")
        expect_read(range(index + 1))
        expect_card_at_top()
    driver.key("j")
    expect_read(range(12))
    bottom = driver.capture("rss-bottom")
    for _ in range(8):
        driver.key("k")
    driver.wait_for_visual_change("RSS K scrolls upward", bottom, crop=EDITOR_CROP)
    expect_card_at_top()
    driver.close_app()

    # Restored feeds receive focus without a preliminary click.
    state_path.write_text(json.dumps({"read_entry_ids": ["entry/0"], "last_read_at": None}),
                          encoding="utf-8")
    driver.start_app(workspace, "restored")
    driver.key("k")
    expect_read([0, 1])
    expect_card_at_top()
    driver.key("j")
    expect_read([0, 1, 2])
    driver.close_app()


def wait_for_rss_filter_text(driver: WindowDriver, expected: str, description: str) -> None:
    # Verify keyboard routing through the field's value, independently of font
    # rasterization and caret blink timing. Clear stale clipboard content first.
    set_clipboard_text(driver.environment, "RSS clipboard sentinel")

    def copied() -> bool:
        # Focus and Copy are queued UI events. Retrying these read-only actions
        # lets a slow runner settle without repeating clicks or text entry.
        driver.key("ctrl+a")
        driver.key("ctrl+c")
        return clipboard_text(driver.environment) == expected

    wait_until(description, copied)


def rss_filters_scenario(driver: WindowDriver, workspace: Path) -> None:
    del workspace
    workspace, config_path, cache = cached_rss_workspace(driver, "rss-filters", [
        {"id": f"entry/{i}", "title": title, "author": None, "published": None,
         "updated": None, "summary": "Native RSS content", "link": None}
        for i, title in enumerate(["Promotion", "Ambiguous news", "Rust promotion"])
    ])
    config = json.loads(config_path.read_text())
    config["subscriptions"][0]["deleted"] = False
    config["subscriptions"][0]["preferences"] = {
        "blacklist": "nothingmatches", "whitelist": "nothingmatches", "version": 1,
    }
    config_path.write_text(json.dumps(config))
    state_path = cache / "state.json"
    state_path.write_text(json.dumps({"read_entry_ids": ["entry/0"], "last_read_at": "before",
        "schedule": {"next_check": 9999999999999}}))
    # A real local regexp filter must work without credentials, aliases, or an AI stub.
    (driver.home / ".stillus.cfg").write_text(json.dumps({"version": 1, "locale": "en"}))
    driver.start_app(workspace, "filters")
    driver.click_note(1, expanded_groups=("all",), categories=(), counts={"all": 2, "favorites": 0, "trash": 0})
    blacklist = "promotion\nsponsored"
    whitelist = "rust\nuseful"
    black_y, white_y, footer_y = 225, 365, 510

    def preferences() -> dict:
        return json.loads(config_path.read_text())["subscriptions"][0]["preferences"]

    def state() -> dict:
        return json.loads(state_path.read_text())

    def open_filters() -> None:
        before = driver.wait_for_stable_frame(
            "feed ready for filter controls", crop=EDITOR_CROP, stable_for=0.3,
        )
        driver.click_point(1014, 28)
        driver.wait_for_visual_change("filter popup painted", before, crop=EDITOR_CROP)

    def field(y: int, value: str) -> None:
        driver.click_point(640, y)
        driver.key("ctrl+a")
        if value:
            for i, line in enumerate(value.split("\n")):
                if i:
                    driver.key("Return")
                driver.type_text(line)
            wait_for_rss_filter_text(driver, value, "regexp field retains exact multiline text")
        else:
            driver.key("BackSpace")

    def edit_preferences() -> None:
        open_filters()
        driver.capture("rss-filter-controls")
        field(black_y, blacklist)
        field(white_y, whitelist)
        driver.click_point(640, black_y)
        wait_for_rss_filter_text(driver, blacklist, "switching fields preserves independent drafts")

    edit_preferences()
    driver.key("Escape")
    if preferences()["blacklist"] != "nothingmatches":
        raise AcceptanceFailure("Escape saved the regexp draft")
    edit_preferences()
    driver.click_point(720, footer_y)  # Cancel
    if preferences()["blacklist"] != "nothingmatches":
        raise AcceptanceFailure("Cancel saved the regexp draft")
    edit_preferences()
    decisions_before = state().get("entries", {})
    driver.click_point(816, footer_y)  # Save
    wait_until("regexp rules saved", lambda: preferences()["blacklist"] == blacklist
               and preferences()["whitelist"] == whitelist)
    if any(entry.get("decision") == "hide" for entry in state().get("entries", {}).values()):
        raise AcceptanceFailure("Save applied the regexp rules")
    for entry_id, saved in decisions_before.items():
        if state()["entries"][entry_id].get("decision") != saved.get("decision"):
            raise AcceptanceFailure("Save changed an existing decision")
    open_filters()
    read_before = state()["read_entry_ids"]
    driver.click_point(938, footer_y)  # Save and Apply
    wait_until("blacklist hides the read promotion", lambda:
               state().get("entries", {}).get("entry/0", {}).get("decision") == "hide")
    if state()["read_entry_ids"] != read_before:
        raise AcceptanceFailure("filtering changed read marks")
    if any(state()["entries"][entry]["decision"] != "keep" for entry in ("entry/1", "entry/2")):
        raise AcceptanceFailure("blacklist/whitelist precedence is incorrect")
    driver.capture("rss-filter-applied")
    driver.key("j")
    wait_until("J skips hidden promotion", lambda: "entry/1" in state()["read_entry_ids"])
    driver.click_point(440, 116)
    driver.wait_for_stable_frame("hidden title expands", crop=EDITOR_CROP, stable_for=0.2)
    if state()["entries"]["entry/0"]["decision"] != "hide":
        raise AcceptanceFailure("expanding a hidden article changed its filter decision")
    driver.key("j")
    driver.key("j")
    wait_until("J reaches whitelist exception", lambda: "entry/2" in state()["read_entry_ids"])

    open_filters()
    field(black_y, "promotion\n[")
    before = (config_path.read_bytes(), state_path.read_bytes())
    driver.capture("rss-filter-invalid-line")
    driver.click_point(938, footer_y)
    if (config_path.read_bytes(), state_path.read_bytes()) != before:
        raise AcceptanceFailure("invalid regexp changed persisted rules or decisions")
    driver.key("Escape")
    open_filters()
    field(white_y, "")
    driver.click_point(938, footer_y)
    wait_until("removing whitelist hides its previous exception", lambda:
               state()["entries"]["entry/2"]["decision"] == "hide")
    open_filters()
    field(black_y, "")
    driver.click_point(938, footer_y)
    wait_until("empty blacklist reveals all articles", lambda:
               all(entry["decision"] == "keep" for entry in state()["entries"].values()))
    driver.close_app()
    driver.start_app(workspace, "filters-restored")
    if any(entry["decision"] != "keep" for entry in state()["entries"].values()):
        raise AcceptanceFailure("regexp decisions changed after restart")
    driver.close_app()


def rss_cards_scenario(driver: WindowDriver, workspace: Path) -> None:
    del workspace
    article_url = "https://example.test/article"
    workspace, config_path, cache = cached_rss_workspace(driver, "rss-cards", [{
        "id": "entry/0", "title": "RSS article", "author": None,
        "published": None, "updated": None,
        "summary": "A **native** RSS article.", "link": article_url,
    }])
    state_path = cache / "state.json"
    state_path.write_text(json.dumps({"read_entry_ids": [], "last_read_at": None}),
                          encoding="utf-8")
    # Restore one cached card directly into the first painted frame. No network,
    # sidebar transitions, scrolling, wrapped titles, or date/locale geometry.
    feed_id = json.loads(config_path.read_text(encoding="utf-8"))["subscriptions"][0]["id"]
    (workspace / ".stillus/settings.json").write_text(json.dumps({
        "version": 1, "window": {"width": SCREEN_WIDTH, "height": SCREEN_HEIGHT},
        "sidebar": {"width": SIDEBAR_WIDTH, "expanded": [], "creation_group": {"kind": "all"}},
        "selected_rss": feed_id,
    }), encoding="utf-8")

    # Exercise the production opener using a local text-browser stand-in.
    # Each invocation publishes its arguments atomically; polling cannot read
    # half a JSON record, and duplicate opens remain visible after app shutdown.
    browser_dir = driver.temporary_root / "browser"
    browser_dir.mkdir()
    browser = browser_dir / "lynx"
    browser.write_text(
        "#!/usr/bin/python3\nimport json, pathlib, sys, tempfile\n"
        f"with tempfile.NamedTemporaryFile(mode='w', dir={str(browser_dir)!r}, suffix='.tmp', delete=False) as output:\n"
        "    json.dump(sys.argv[1:], output)\n"
        "path = pathlib.Path(output.name)\n"
        "path.rename(path.with_suffix('.json'))\n",
        encoding="utf-8",
    )
    browser.chmod(0o755)

    def opened() -> list[list[str]]:
        return [json.loads(path.read_text(encoding="utf-8")) for path in browser_dir.glob("*.json")]

    def read_ids() -> list[str]:
        return json.loads(state_path.read_text(encoding="utf-8"))["read_entry_ids"]

    # start_app waits for first paint. The only click is inside the single-line
    # title's full-width hit area; it is never repeated to make a failed step pass.
    driver.start_app(workspace, "cards", environment_overrides={
        "BROWSER": str(browser), "PATH": str(browser_dir),
    })
    driver.click_point(450, 116)
    wait_until("RSS title opens its original URL once", lambda: opened() == [[article_url]])
    wait_until("RSS title persists the read state", lambda: read_ids() == ["entry/0"])
    # Measure both selected-card edges in its blank top padding, below the
    # rounded corners. A capped or overflowing list must fail even if centered.
    for width in (SCREEN_WIDTH, 960, SCREEN_WIDTH):
        driver.resize_window(width, SCREEN_HEIGHT)
        frame = driver.wait_for_stable_frame(
            f"RSS card layout at width {width}", stable_for=0.3,
            crop=(SIDEBAR_WIDTH, 80, width - SIDEBAR_WIDTH, 120),
        )
        edges = column_runs(near_color_columns(
            frame, (54, 94, 130),
            crop=(SIDEBAR_WIDTH, 90, width - SIDEBAR_WIDTH, 4),
        ), merge_gap=0)
        if len(edges) != 2 or any(end - start > 1 for start, end in edges):
            raise AcceptanceFailure(f"RSS card side borders missing at width {width}: {edges}")
        left = edges[0][0] - SIDEBAR_WIDTH
        right = width - 1 - edges[1][1]
        if abs(left - 20) > 1 or abs(right - 20) > 1 or abs(left - right) > 1:
            raise AcceptanceFailure(
                f"RSS card must fill content with equal 20px insets at width {width}: "
                f"left={left}, right={right}"
            )
    driver.close_app()
    if opened() != [[article_url]] or read_ids() != ["entry/0"]:
        raise AcceptanceFailure("RSS title open/read result changed before shutdown")


def localization_scenario(driver: WindowDriver, workspace: Path) -> None:
    def open_language_picker(rtl: bool) -> int:
        # Locate the dropdown arrow independently of each script's line metrics.
        frame = driver.capture("language-control")
        arrow_x = 271 if rtl else 577
        pixels = run_command(["convert", str(frame), "-crop", f"12x120+{arrow_x}+180",
                              "+repage", "-depth", "8", "txt:-"]).stdout
        rows = []
        for line in pixels.splitlines():
            match = re.match(r"\d+,(\d+): \(\s*(\d+),\s*(\d+),\s*(\d+)", line)
            if match and max(map(int, match.groups()[1:])) < 170:
                rows.append(int(match[1]) + 180)
        if not rows:
            raise AcceptanceFailure("language dropdown arrow is not visible")
        driver.click_point(411 if rtl else 448, max(rows))
        return max(rows)

    languages = ("en", "es", "ru", "zh/hans", "zh/hant", "pt/br", "pt/pt", "hi",
                 "ar", "fr", "bn", "id", "ur", "de", "ja", "tr", "ko")
    driver.start_app(workspace, "languages")
    driver.wait_for_stable_frame("initial English interface", crop=(0, 0, 256, 400), timeout=10)
    original_notes = {path.name: path.read_bytes() for path in (workspace / "notes").iterdir()}
    driver.click("settings")
    driver.wait_for_stable_frame("language settings", stable_for=0.3, timeout=10)
    driver.resize_window(860, 560)
    baseline = driver.wait_for_stable_frame("English language control", stable_for=0.3, timeout=10)
    config = driver.home / ".stillus.cfg"
    preserved_config = config.with_suffix(".preserved")
    original_config = config.read_bytes()
    config.rename(preserved_config)
    config.mkdir()
    try:
        control_y = open_language_picker(False)
        driver.key("Home")
        driver.key("Down")
        driver.key("Return")
        driver.wait_for_visual_change("language write error", baseline,
                                      crop=(298, 220, 495, 100), timeout=10)
        failed = driver.wait_for_stable_frame("rejected language retains English", stable_for=0.3)
        if near_color_pixel_count(failed, (164, 69, 69), crop=(298, 220, 495, 100)) < 20:
            raise AcceptanceFailure("language write failure did not show an error")
        if image_difference(baseline, failed, crop=(310, control_y - 12, 180, 23)) != 0:
            raise AcceptanceFailure("failed language save changed the displayed selection")
        if not config.is_dir() or preserved_config.read_bytes() != original_config:
            raise AcceptanceFailure("failed language save changed the previous configuration")
    finally:
        config.rmdir()
        preserved_config.rename(config)
    driver.click_point(448, control_y)
    driver.key("Home")
    driver.key("Return")
    wait_until("cleared language write error",
               lambda: near_color_pixel_count(driver.capture("clear-language-error"),
                   (164, 69, 69), crop=(298, 220, 495, 100)) == 0, timeout=10)
    driver.wait_for_stable_frame("cleared language error", stable_for=0.3, timeout=10)
    current = "en"
    for index, locale in enumerate(languages):
        rtl = current in {"ar", "ur"}
        previous = driver.capture("before-language-change")
        open_language_picker(rtl)
        driver.key("Home")
        for _ in range(index):
            driver.key("Down")
        driver.key("Return")
        wait_until("persisted language " + locale,
                   lambda: json.loads((driver.home / ".stillus.cfg").read_text()).get("locale") == locale)
        if locale != current:
            driver.wait_for_visual_change("rendered language " + locale, previous,
                                          crop=(0, 0, 860, 100), timeout=10)
        rtl = locale in {"ar", "ur"}
        sidebar_x = 628 if rtl else 0
        wait_until("sidebar direction " + locale,
                   lambda: near_color_pixel_count(driver.capture("language-direction"),
                       (35, 42, 51), crop=(sidebar_x, 0, 232, 540)) >= 50000, timeout=10)
        frame = driver.wait_for_stable_frame("translated settings " + locale,
                                             stable_for=0.3, timeout=10)
        if near_color_pixel_count(frame, (35, 42, 51), crop=(sidebar_x, 0, 232, 540)) < 50000:
            raise AcceptanceFailure("settings sidebar did not follow language direction: " + locale)
        if {path.name: path.read_bytes() for path in (workspace / "notes").iterdir()} != original_notes:
            raise AcceptanceFailure("changing language modified notes")
        if rtl:
            # Exercise the mirrored native resize handle and a menu near the right edge.
            def wait_rtl_boundary(edge: int) -> None:
                def positioned() -> bool:
                    pixels = crop_luminances(driver.capture("rtl-resize"), (edge - 2, 530, 12, 1))
                    return pixels[(edge - 2, 530)] > 220 and pixels[(edge + 8, 530)] < 100
                wait_until("mirrored sidebar boundary", positioned, timeout=10)

            driver.click_point(824, 42)
            wait_rtl_boundary(604)
            driver.press_point(606, 400)
            driver.drag_point(526, 400)
            driver.release()
            wait_rtl_boundary(524)
            before_menu = driver.capture("before-rtl-menu")
            driver.click_point(590, 27)
            driver.wait_for_visual_change("RTL creation menu", before_menu,
                                          crop=(524, 45, 336, 300), timeout=10)
            driver.key("Escape")
            driver.press_point(526, 400)
            driver.drag_point(606, 400)
            driver.release()
            wait_rtl_boundary(604)
            driver.click_point(632, 27)
            driver.wait_for_stable_frame("return to RTL settings", stable_for=0.3, timeout=10)
        current = locale
    driver.resize_window(SCREEN_WIDTH, SCREEN_HEIGHT)
    driver.close_app()
    driver.start_app(workspace, "Korean-language-restart")
    driver.wait_for_stable_frame("Korean after restart", crop=(0, 0, 256, 400), timeout=10)
    if json.loads(config.read_text())["locale"] != "ko":
        raise AcceptanceFailure("non-English language did not survive restart")
    driver.click("settings")
    driver.resize_window(860, 560)
    driver.wait_for_stable_frame("Korean settings after restart", stable_for=0.3, timeout=10)
    # Return to English using the same control, then check that the choice survives launch.
    open_language_picker(False)
    driver.key("Home")
    driver.key("Return")
    wait_until("English restored", lambda: json.loads((driver.home / ".stillus.cfg").read_text())["locale"] == "en")
    driver.resize_window(SCREEN_WIDTH, SCREEN_HEIGHT)
    driver.click("settings_back")
    driver.close_app()
    driver.start_app(workspace, "language-restart")
    driver.wait_for_stable_frame("English after restart", crop=(0, 0, 256, 400), timeout=10)
    if json.loads((driver.home / ".stillus.cfg").read_text())["locale"] != "en":
        raise AcceptanceFailure("language did not survive restart")
    if {path.name: path.read_bytes() for path in (workspace / "notes").iterdir()} != original_notes:
        raise AcceptanceFailure("language restart modified notes")
    driver.close_app()


def wait_for_ai_controls(driver: WindowDriver) -> None:
    # Focused text fields legitimately blink. Require identical frames, but not
    # a 600 ms quiet period spanning a caret blink. Preserve focus so typing,
    # Enter, blur validation and dropdown assertions exercise the real controls.
    driver.wait_for_stable_frame("AI controls", crop=(232, 0, 1008, 800), stable_for=0.15, timeout=10)


def ai_settings_scenario(driver: WindowDriver, workspace: Path) -> None:
    """Real native controls with test-only catalog and credential adapters."""
    original = {path: path.read_bytes() for path in (workspace / "notes").glob("*.md")}
    config = driver.home / ".stillus.cfg"
    fixture = {"STILLUS_TEST_AI": "1"}

    def state() -> dict:
        return json.loads(config.read_text()).get("ai", {})

    def settle() -> None:
        wait_for_ai_controls(driver)

    def open_ai() -> None:
        driver.click("settings")
        settle()
        before = driver.capture("before-ai")
        driver.click_point(*AI_SIDEBAR_ITEM)
        driver.wait_for_visual_change("AI section", before, crop=(260, 30, 740, 550), timeout=10)
        settle()

    def primary(within: tuple[int, int] | None = None) -> None:
        """Click the accent-filled primary action, proving it is enabled."""
        top, bottom = within or (0, SCREEN_HEIGHT)
        runs = shaded_row_runs(
            driver.capture("ai-primary"),
            x=AI_PRIMARY_PROBE_X,
            y=top,
            height=bottom - top,
            max_luminance=AI_ACCENT_LUMINANCE,
        )
        runs = [(start, end) for start, end in runs if end - start >= 20]
        if not runs:
            raise AcceptanceFailure("AI primary action is not enabled")
        start, end = max(runs, key=lambda run: run[1] - run[0])
        driver.click_point(AI_PRIMARY_PROBE_X, (start + end) // 2)

    def cards() -> list[tuple[int, int]]:
        """Every settings card of the page, read from its left border."""
        luminances = crop_luminances(
            driver.capture("ai-cards"), (AI_CONTENT_LEFT, 0, 1, SCREEN_HEIGHT)
        )
        rows = {
            row
            for (_x, row), luminance in luminances.items()
            if luminance <= AI_CARD_BORDER_LUMINANCE
        }
        return [
            (start - AI_CARD_CORNER, end + AI_CARD_CORNER)
            for start, end in column_runs(rows, merge_gap=0)
            if end - start >= AI_CARD_MIN_HEIGHT
        ]

    def bounds() -> tuple[int, int]:
        driver.click_point(*AI_SIDEBAR_ITEM)
        settle()
        runs = shaded_row_runs(driver.capture("ai-editor"), x=AI_CONTENT_LEFT,
                               y=0, height=SCREEN_HEIGHT, max_luminance=AI_ACCENT_LUMINANCE)
        runs = [(start, end) for start, end in runs if end - start >= AI_EXPANDED_MIN_HEIGHT]
        if len(runs) != 1:
            raise AcceptanceFailure("expected one alias editor")
        return runs[0][0] - AI_CARD_CORNER, runs[0][1] + AI_CARD_CORNER

    def controls() -> list[tuple[int, int]]:
        top, bottom = bounds()
        runs = shaded_row_runs(driver.capture("ai-fields"), x=AI_CARD_CONTENT_LEFT,
                               y=top, height=bottom - top, max_luminance=AI_CARD_BORDER_LUMINANCE)
        return [(start, end) for start, end in runs if end - start >= 22]

    def field(index: int) -> int:
        start, end = controls()[index]
        return (start + end) // 2

    def all_rows() -> list[tuple[int, int]]:
        driver.wheel("up", clicks=20, delay_ms=20)
        settle()
        return cards()

    def edit(index: int) -> None:
        rows = all_rows()
        driver.click_point(950, rows[index + 1][0] + 40)
        settle()
        bounds()

    def add(name: str) -> None:
        rows = all_rows()
        driver.click_point(980, rows[0][1] + 44)
        settle()
        driver.click_point(470, field(0))
        driver.type_text(name)
        settle()

    def choose(query: str, *, default: bool = False) -> None:
        model_index = 0 if default else 1
        driver.click_point(500, field(model_index))
        settle()
        review_frame("ai-model-dropdown")
        # Only Luna and Sol support effort in the fixture. End therefore selects
        # Sol even though GPT-4.1 is the last catalog entry.
        driver.key("Home" if query == "Luna" else "End")
        driver.key("Return")
        settle()
        driver.click_point(500, field(model_index + 1))
        settle()
        review_frame("ai-effort-dropdown")
        driver.key("End" if query == "Luna" else "Home")
        driver.key("Return")
        settle()

    def save_alias(name: str) -> None:
        primary(within=bounds())
        wait_until("saved alias", lambda: name in state()["aliases"])
        settle()

    def review_frame(name: str) -> None:
        # Only synthetic catalog keys are used in this scenario.
        shutil.copyfile(driver.capture(name), ARTIFACT_ROOT / f"{name}.png")

    def danger_pixels(crop: tuple[int, int, int, int] = (280, 150, 710, 450)) -> int:
        return near_color_pixel_count(driver.capture("ai-feedback"), (164, 69, 69), crop=crop)

    def key_field() -> tuple[int, int]:
        return (460, cards()[0][0] + 62)

    driver.start_app(workspace, "empty", environment_overrides=fixture)
    open_ai()
    review_frame("ai-empty")
    if len(cards()) != 1:
        raise AcceptanceFailure("unconnected models should not occupy a card")
    if state().get("connection") or state().get("aliases"):
        raise AcceptanceFailure("AI was configured without explicit input")
    driver.click_point(*key_field())
    driver.type_text("s")
    settle()
    if danger_pixels() > 5:
        raise AcceptanceFailure("AI key validation interrupted typing")
    driver.click_point(*AI_SIDEBAR_ITEM)
    settle()
    wait_until("invalid key feedback after blur", lambda: danger_pixels() >= 20)
    driver.click_point(*key_field())
    driver.type_text("k")
    settle()
    wait_until("editing clears outdated key feedback", lambda: danger_pixels() <= 5)
    set_clipboard_text(driver.environment, "invalid-key")
    driver.key("ctrl+v")
    settle()
    wait_until("invalid clipboard key feedback", lambda: danger_pixels() >= 20)
    set_clipboard_text(driver.environment, "sk-proj-abcdefghijklmnopqrstuvdenied")
    driver.click_point(957, key_field()[1])
    settle()
    wait_until("valid key paste clears format feedback", lambda: danger_pixels() <= 5)
    primary()
    settle()
    wait_until("rejected key feedback", lambda: danger_pixels() >= 20)
    if state().get("connection"):
        raise AcceptanceFailure("rejected key was saved")
    review_frame("ai-connection-error")
    driver.click_point(*key_field())
    set_clipboard_text(driver.environment, "  sk-proj-abcdefghijklmnopqrstuvslow  ")
    driver.key("ctrl+v")
    settle()
    field_y = key_field()[1]
    masked = driver.capture("ai-masked")
    driver.click_point(913, field_y)
    driver.click_point(*AI_SIDEBAR_ITEM)
    settle()
    revealed = driver.capture("ai-revealed")
    if image_difference(masked, revealed, crop=(310, field_y - 12, 400, 24)) < 50:
        raise AcceptanceFailure("AI reveal control did not reveal the fixture key")
    driver.click_point(913, field_y)
    driver.click_point(*AI_SIDEBAR_ITEM)
    settle()
    if image_difference(masked, driver.capture("ai-concealed"), crop=(310, field_y - 12, 400, 24)) > 5:
        raise AcceptanceFailure("AI reveal control did not conceal the key again")
    driver.click_point(*key_field())
    settle()
    driver.key("Return")
    review_frame("ai-connecting")
    # Enter during the in-flight request must not start a second connection.
    driver.key("Return")
    wait_until("verified AI connection", lambda: state().get("connection") is not None)
    settle()
    if state()["aliases"] != {"default": {"model": "gpt-5.6-luna", "effort": "high"}}:
        raise AcceptanceFailure("connecting did not create the expected default alias")

    review_frame("ai-default")
    edit(0)
    if danger_pixels() > 5:
        raise AcceptanceFailure("default has a delete action or an invalid initial configuration")
    choose("Sol", default=True)
    primary(within=bounds())
    wait_until("edited default", lambda: state()["aliases"]["default"]["model"] == "gpt-5.6-sol")
    settle()
    default = state()["aliases"]["default"]

    add("mini")
    choose("Luna")
    save_alias("mini")
    if state()["aliases"]["mini"] != {"model": "gpt-5.6-luna", "effort": "high"}:
        raise AcceptanceFailure("custom alias saved wrong model or effort")
    # Creating multiple names is not constrained by the old three task slots.
    for name in ["simple", "writing", "research"]:
        add(name)
        # A new alias already inherits the edited default (Sol/high).
        save_alias(name)
    if any(profile["model"] == "gpt-4.1" or profile["effort"] != "high"
           for profile in state()["aliases"].values()):
        raise AcceptanceFailure("dropdown selected a model without effort or wrong effort")
    if len(state()["aliases"]) != 5:
        raise AcceptanceFailure("custom aliases are still constrained to task slots")
    review_frame("ai-aliases")

    edit(1)  # default first, then alphabetical: mini
    driver.click_point(470, field(0))
    driver.key("ctrl+a")
    driver.type_text("default")
    settle()
    name_bottom = controls()[0][1]
    model_top = controls()[1][0]
    if danger_pixels((300, name_bottom, 650, model_top - name_bottom)) < 20:
        raise AcceptanceFailure("duplicate alias name was not rejected")
    driver.click_point(470, field(0))
    driver.key("ctrl+a")
    driver.type_text("quick")
    settle()
    primary(within=bounds())
    wait_until("renamed alias", lambda: "quick" in state()["aliases"] and "mini" not in state()["aliases"])
    settle()
    edit(1)  # quick sorts before research
    last = controls()[-1]
    driver.click_point(AI_CARD_CONTENT_LEFT + 16, (last[0] + last[1]) // 2)
    wait_until("deleted alias", lambda: "quick" not in state()["aliases"])
    settle()
    if state()["aliases"]["default"] != default:
        raise AcceptanceFailure("deleting another alias changed default")

    rows = all_rows()
    saved = state()
    driver.click_point(345, rows[0][1] - 39)
    settle()
    empty_key = driver.capture("ai-provider-empty")
    key_y = key_field()[1]
    set_clipboard_text(driver.environment, "sk-ant-api03-abcdefghijklmnopqrstuv")
    driver.click_point(957, key_y)
    driver.wait_for_visual_change("provider key pasted", empty_key,
                                  crop=(310, key_y - 15, 540, 30), timeout=10)
    settle()
    review_frame("ai-provider-warning")
    driver.click_point(435, cards()[0][1] - 39)
    wait_until("key edit cancelled", lambda: cards()[0][1] - cards()[0][0] < 160, timeout=10)
    settle()
    if state() != saved:
        raise AcceptanceFailure("cancelling provider change changed aliases")
    driver.click_point(345, cards()[0][1] - 39)
    settle()
    empty_key = driver.capture("ai-replacement-empty")
    set_clipboard_text(driver.environment, "sk-proj-abcdefghijklmnopqrstuv")
    driver.click_point(957, key_field()[1])
    driver.wait_for_visual_change("replacement key pasted", empty_key,
                                  crop=(310, key_field()[1] - 15, 540, 30), timeout=10)
    settle()
    primary(within=cards()[0])
    wait_until("replacement key", lambda: state()["connection"] != saved["connection"])
    settle()
    if state()["aliases"] != saved["aliases"]:
        raise AcceptanceFailure("same-provider replacement overwrote aliases")
    saved = state()
    driver.close_app()
    driver.start_app(workspace, "restart", environment_overrides=fixture)
    open_ai()
    if state() != saved:
        raise AcceptanceFailure("aliases changed on restart")
    for _ in range(2):
        edit(0)
        # The built-in alias has no removal footer; Cancel sits beside Save.
        last = controls()[-1]
        driver.click_point(AI_CANCEL_X, (last[0] + last[1]) // 2)
        settle()
    if state() != saved:
        raise AcceptanceFailure("cancelling alias edits changed configuration")

    edit(0)
    changed = json.loads(config.read_text())
    changed["ai"]["connection"]["checked_at"] += 1
    config.write_text(json.dumps(changed))
    primary(within=bounds())
    settle()
    top, bottom = bounds()
    if danger_pixels((300, top, 670, bottom - top)) < 20:
        raise AcceptanceFailure("save conflict was not shown inside the alias editor")
    if state() != changed["ai"]:
        raise AcceptanceFailure("conflicting save overwrote configuration")
    review_frame("ai-alias-error")
    driver.close_app()

    global_settings = json.loads(config.read_text())
    global_settings["locale"] = "ru"
    config.write_text(json.dumps(global_settings))
    driver.start_app(workspace, "ru", environment_overrides=fixture)
    open_ai()
    review_frame("ai-ready-ru")
    edit(0)
    driver.resize_window(860, 560)
    settle()
    driver.wheel("down", clicks=3, delay_ms=40)
    settle()
    review_frame("ai-narrow-ru")
    driver.resize_window(SCREEN_WIDTH, SCREEN_HEIGHT)
    settle()
    driver.close_app()
    global_settings = json.loads(config.read_text())
    global_settings["locale"] = "ar"
    config.write_text(json.dumps(global_settings))
    driver.start_app(workspace, "rtl", environment_overrides=fixture)
    driver.click_point(1010, 27)
    driver.wait_for_stable_frame("RTL settings", stable_for=0.6)
    driver.click_point(1110, 203)
    driver.wait_for_stable_frame("RTL AI", stable_for=0.6)
    driver.resize_window(860, 560)
    wait_until("AI RTL sidebar", lambda: near_color_pixel_count(
        driver.capture("ai-rtl-direction"), (35, 42, 51), crop=(628, 0, 232, 540)
    ) >= 50000, timeout=10)
    driver.wait_for_stable_frame("RTL AI narrow", crop=(0, 0, 860, 560), stable_for=0.6)
    review_frame("ai-narrow-rtl")
    driver.close_app()
    if any("sk-proj-abcdefghijklmnopqrstuv" in path.read_text(errors="replace") for path in driver.app_log_paths):
        raise AcceptanceFailure("AI credential reached diagnostics")
    if "sk-proj-abcdefghijklmnopqrstuv" in config.read_text():
        raise AcceptanceFailure("AI credential reached settings")
    if {path: path.read_bytes() for path in original} != original:
        raise AcceptanceFailure("AI configuration modified notes")


# Update prompt and settings geometry. The prompt is a card in the bottom
# right corner of the window; its accent-filled action is the only long dark
# run in that column, exactly like the AI page's primary action.
UPDATES_SIDEBAR_ITEM = (90, 241)
UPDATES_PROMPT_CROP = (SCREEN_WIDTH - 360, SCREEN_HEIGHT - 220, 342, 202)
UPDATES_PROMPT_LEFT = SCREEN_WIDTH - 348
UPDATES_ACCENT_LUMINANCE = 150.0
UPDATES_BUTTON_MIN_HEIGHT = 20
UPDATES_OFFERED_VERSION = "9.9.9"
UPDATES_SETTINGS_CROP = (280, 100, 700, 400)


def accent_button(image: Path, region: tuple[int, int, int, int]) -> tuple[int, int, int]:
    """Left edge, right edge and centre row of the filled button in a region."""
    button = find_accent_button(crop_luminances(image, region), region)
    if button is None:
        raise AcceptanceFailure("no filled update action is visible")
    return button


def find_accent_button(
    luminances: dict[tuple[int, int], float], region: tuple[int, int, int, int]
) -> tuple[int, int, int] | None:
    """Inspect one decoded crop; absence is distinct from a capture/decode error."""
    left, top, width, height = region
    columns: list[tuple[int, tuple[int, int]]] = []
    for x in range(left, left + width, 2):
        runs = [
            run
            for run in column_runs({
                row for row in range(top, top + height)
                if luminances[(x, row)] <= UPDATES_ACCENT_LUMINANCE
            }, merge_gap=0)
            if run[1] - run[0] >= UPDATES_BUTTON_MIN_HEIGHT
        ]
        if runs:
            columns.append((x, max(runs, key=lambda run: run[1] - run[0])))
    if not columns:
        return None
    start, end = columns[-1][1]
    return columns[0][0], columns[-1][0], (start + end) // 2


def restart_from_update_settings(driver: WindowDriver) -> None:
    """Retry only an unacknowledged click, within one process-exit deadline."""
    if driver.app is None or driver.window_id is None:
        raise AcceptanceFailure("restart requires the running application")
    driver.set_stage("restart/settings")
    deadline = time.monotonic() + 10.0
    clicks = 0
    last_click = 0.0
    button = None
    hovered = None
    stable_since = None
    previous = None
    baseline = None
    current = None
    try:
        while time.monotonic() < deadline:
            code = driver.app.poll()
            if code is not None:
                if clicks == 0 or code != 0:
                    raise AcceptanceFailure("old process failed during restart")
                driver.set_stage("restart/exit")
                return
            current = driver.capture("updates-restart-observe")
            if baseline is not None and any(
                image_difference(baseline, current, crop=region) >= 50
                for region in (UPDATES_PROMPT_CROP, UPDATES_SETTINGS_CROP)
            ):
                # The settings action opens the prompt and disables Restart.
                # Any reaction stops retries, but only process exit is success.
                driver.set_stage("restart/exit")
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    break
                if driver.app.wait(timeout=remaining) != 0:
                    raise AcceptanceFailure("old process failed during restart")
                return
            if clicks < 3 and (clicks == 0 or time.monotonic() - last_click >= 1.0):
                unchanged = previous is not None and image_difference(
                    previous, current, crop=UPDATES_SETTINGS_CROP
                ) == 0
                # Decoding the large crop is expensive on a loaded runner.
                # Identical pixels retain the already verified button geometry.
                if not unchanged:
                    button = find_accent_button(
                        crop_luminances(current, UPDATES_SETTINGS_CROP), UPDATES_SETTINGS_CROP
                    )
                point = None if button is None else ((button[0] + button[1]) // 2, button[2])
                if point is None:
                    stable_since = None
                elif point != hovered:
                    driver.xdotool("mousemove", "--sync", "--window", driver.window_id,
                                   str(point[0]), str(point[1]))
                    hovered = point
                    stable_since = None
                elif not unchanged:
                    stable_since = None
                elif stable_since is None:
                    stable_since = time.monotonic()
                elif time.monotonic() - stable_since >= 0.4:
                    now = time.monotonic()
                    if now >= deadline:
                        break
                    if driver.app.poll() is not None:
                        continue
                    driver.set_stage("restart/click")
                    if clicks == 0:
                        deadline = now + 20.0
                        baseline = current
                    clicks += 1
                    last_click = now
                    # The pointer is already over the freshly verified button.
                    driver.xdotool("click", "1")
                    stable_since = None
            if previous is not None and previous is not baseline:
                previous.unlink(missing_ok=True)
            previous = current
            time.sleep(0.05)
        raise AcceptanceFailure("timed out restarting from update settings")
    finally:
        for frame in (previous, baseline, current):
            if frame is not None:
                frame.unlink(missing_ok=True)


def release_fixture(directory: Path, *, published_at: str) -> None:
    """A complete release: metadata, checksum list and an installable package."""
    directory.mkdir(parents=True, exist_ok=True)
    architecture = platform.machine()
    name = f"stillus-{UPDATES_OFFERED_VERSION}-linux-{architecture}.tar.gz"
    revision = "0" * 40
    files = {"stillus": b"updated executable\n", "SOURCE_REVISION.txt": f"{revision}\n".encode()}
    manifest = {
        "source_revision": revision,
        "platform": "linux",
        "architecture": architecture,
        "files": [
            {"path": path, "sha256": hashlib.sha256(data).hexdigest()}
            for path, data in files.items()
        ],
    }
    files["build.json"] = json.dumps(manifest).encode()
    archive = directory / name
    with tarfile.open(archive, "w:gz") as package:
        for path, data in files.items():
            info = tarfile.TarInfo(f"{archive.name[: -len('.tar.gz')]}/{path}")
            info.size = len(data)
            info.mode = 0o755 if path == "stillus" else 0o644
            package.addfile(info, io.BytesIO(data))
    download = "https://github.com/stillus-ai/stillus/releases/download/v" + UPDATES_OFFERED_VERSION
    (directory / "SHA256SUMS").write_text(
        f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {name}\n", encoding="ascii"
    )
    (directory / "latest.json").write_text(
        json.dumps(
            {
                "tag_name": f"v{UPDATES_OFFERED_VERSION}",
                "draft": False,
                "prerelease": False,
                "published_at": published_at,
                "html_url": "https://github.com/stillus-ai/stillus/releases/tag/v"
                + UPDATES_OFFERED_VERSION,
                "body": "Improvements\n\nNone.",
                "assets": [
                    {
                        "name": name,
                        "size": archive.stat().st_size,
                        "state": "uploaded",
                        "browser_download_url": f"{download}/{name}",
                    },
                    {
                        "name": "SHA256SUMS",
                        "size": 64,
                        "state": "uploaded",
                        "browser_download_url": f"{download}/SHA256SUMS",
                    },
                ],
            }
        ),
        encoding="utf-8",
    )

def packaged_installation(installation: Path) -> None:
    """The installation the application replaces: a packaged Linux layout."""
    installation.mkdir(parents=True, exist_ok=True)
    (installation / "stillus").write_bytes(b"installed executable\n")
    (installation / "build.json").write_text('{"platform": "linux"}', encoding="utf-8")


def updates_scenario(driver: WindowDriver, workspace: Path) -> None:
    """The startup offer, an installed package and a declined release."""
    original = {path: path.read_bytes() for path in (workspace / "notes").glob("*.md")}
    releases = driver.temporary_root / "releases"
    installation = driver.temporary_root / "installation"
    config = driver.home / ".stillus.cfg"
    fixture = {
        "STILLUS_TEST_UPDATE": str(releases),
        "STILLUS_TEST_UPDATE_ROOT": str(installation),
    }
    executable = installation / "stillus"

    def settings() -> dict:
        return json.loads(config.read_text()).get("updates", {}) if config.is_file() else {}

    packaged_installation(installation)
    release_fixture(releases, published_at="2020-01-01T00:00:00Z")
    driver.start_app(workspace, "offer", environment_overrides=fixture)
    empty = driver.capture("updates-before-offer")
    driver.wait_for_visual_change(
        "the startup check offering a release", empty, crop=UPDATES_PROMPT_CROP, timeout=20
    )
    driver.wait_for_stable_frame("update prompt", crop=UPDATES_PROMPT_CROP, stable_for=0.4)
    offered = driver.capture("updates-offer")
    _, _, row = accent_button(offered, UPDATES_PROMPT_CROP)
    driver.click_point(SCREEN_WIDTH - 60, row)
    wait_until(
        "the package replacing the installed executable",
        lambda: executable.read_bytes() == b"updated executable\n",
        timeout=20,
    )
    if not (installation / "SOURCE_REVISION.txt").is_file():
        raise AcceptanceFailure("the installed package is missing its source revision")
    if any(path.name.startswith(".stillus-update-") for path in installation.iterdir()):
        raise AcceptanceFailure("the update left temporary files in the installation")
    driver.wait_for_visual_change(
        "the restart notice replacing the offer", offered, crop=UPDATES_PROMPT_CROP, timeout=20
    )
    driver.wait_for_stable_frame("restart notice", crop=UPDATES_PROMPT_CROP, stable_for=0.4)
    restart_notice = driver.capture("updates-restart")
    left, right, row = accent_button(restart_notice, UPDATES_PROMPT_CROP)
    # This fixture deliberately contains an invalid executable. A failed
    # launch must keep the old window alive and the Restart action available.
    driver.click_point((left + right) // 2, row)
    driver.wait_for_visual_change(
        "restart failure remaining in the open session", restart_notice,
        crop=UPDATES_PROMPT_CROP, timeout=10,
    )
    if driver.app is None or driver.app.poll() is not None:
        raise AcceptanceFailure("a failed restart closed the running session")
    driver.wait_for_stable_frame("restart failure", crop=UPDATES_PROMPT_CROP, stable_for=0.4)
    failure = driver.capture("updates-restart-failed")
    left, right, row = accent_button(failure, UPDATES_PROMPT_CROP)
    driver.click_point(left - 30, row)
    driver.wait_for_visual_change("Later dismissing restart", failure, crop=UPDATES_PROMPT_CROP)
    driver.click("settings")
    driver.wait_for_stable_frame("settings after Later", stable_for=0.4)
    driver.click_point(*UPDATES_SIDEBAR_ITEM)

    # Retry with the real app in the same installed path. This exercises a
    # real second process, its stdin handoff and workspace restoration without
    # packaging the debug binary or launching anything from the workspace.
    shutil.copy2(APP_BINARY, executable)
    global_config = json.loads(config.read_text())
    global_config.setdefault("updates", {})["automatic"] = False
    config.write_text(json.dumps(global_config))
    old_pid = driver.app.pid
    restart_from_update_settings(driver)
    driver.app = None
    driver.window_id = None
    new_pid = None
    try:
        driver.set_stage("restart/window")
        driver.window_id = driver._wait_for_window()
        new_pid = int(driver.xdotool("getwindowpid", driver.window_id).stdout.strip())
        if new_pid == old_pid:
            raise AcceptanceFailure("Restart did not launch a new process")
        if Path(f"/proc/{new_pid}/exe").resolve() != executable.resolve():
            raise AcceptanceFailure("Restart launched a different executable")
        wait_for_first_paint(driver.window_id, driver.environment)
        driver.wait_for_stable_frame("restarted workspace", stable_for=0.4)
        driver.capture("updates-restarted")
        driver.set_stage("restart/validation")
        arguments = Path(f"/proc/{new_pid}/cmdline").read_bytes().split(b"\0")
        if str(workspace).encode() not in arguments:
            raise AcceptanceFailure("Restart did not preserve the current workspace")
    finally:
        if driver.window_id is not None:
            request_window_close(driver.window_id, driver.environment)
            wait_until("restarted window closing", lambda: run_command(
                ["xdotool", "search", "--onlyvisible", "--name", "^Stillus$"],
                environment=driver.environment, check=False,
            ).returncode == 1)
            driver.window_id = None
        if new_pid is not None:
            wait_until("restarted process exiting", lambda: not Path(f"/proc/{new_pid}/exe").exists())
    driver.set_stage("scenario")
    global_config["updates"]["automatic"] = True
    config.write_text(json.dumps(global_config))
    executable.write_bytes(b"updated executable\n")

    # A release published hours ago is held back by the startup check and is
    # still described on the settings page.
    published = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() - 3 * 3600))
    release_fixture(releases, published_at=published)
    driver.start_app(workspace, "held", environment_overrides=fixture)
    quiet = driver.capture("updates-quiet")
    time.sleep(6.0)
    if image_difference(quiet, driver.capture("updates-still-quiet"), crop=UPDATES_PROMPT_CROP) >= 50:
        raise AcceptanceFailure("a release published hours ago was offered automatically")
    driver.click("settings")
    driver.wait_for_stable_frame("settings page", stable_for=0.4)
    before_section = driver.capture("updates-before-section")
    driver.click_point(*UPDATES_SIDEBAR_ITEM)
    driver.wait_for_visual_change(
        "the updates settings section", before_section, crop=(260, 30, 740, 550), timeout=10
    )
    if executable.read_bytes() != b"updated executable\n":
        raise AcceptanceFailure("opening the updates section changed the installation")
    driver.close_app()

    # Declining a release is remembered, so the same version is offered once.
    release_fixture(releases, published_at="2020-01-01T00:00:00Z")
    driver.start_app(workspace, "declined", environment_overrides=fixture)
    empty = driver.capture("updates-before-decline")
    driver.wait_for_visual_change(
        "the startup check offering a release again", empty, crop=UPDATES_PROMPT_CROP, timeout=20
    )
    driver.wait_for_stable_frame("update prompt", crop=UPDATES_PROMPT_CROP, stable_for=0.4)
    offer = driver.capture("updates-decline-offer")
    accent_left, _, row = accent_button(offer, UPDATES_PROMPT_CROP)
    driver.click_point(accent_left - 30, row)
    wait_until(
        "the declined version reaching the global settings",
        lambda: settings().get("dismissed") == UPDATES_OFFERED_VERSION,
        timeout=10,
    )
    driver.wait_for_visual_change(
        "the offer disappearing", offer, crop=UPDATES_PROMPT_CROP, timeout=10
    )
    driver.close_app()
    if {path: path.read_bytes() for path in original} != original:
        raise AcceptanceFailure("the update flow modified notes")



def ai_journal_scenario(driver: WindowDriver, workspace: Path) -> None:
    """Exercise the journal without credentials, including restart and real file cleanup."""
    original = {path: path.read_bytes() for path in (workspace / "notes").glob("*.md")}
    driver.start_app(workspace, "journal", environment_overrides={"STILLUS_TEST_AI": "1"})
    driver.click("settings")
    wait_for_ai_controls(driver)
    driver.click_point(*AI_SIDEBAR_ITEM)
    wait_for_ai_controls(driver)
    settings = driver.capture("journal-settings")
    driver.click_point(1140, 54)
    driver.wait_for_visual_change("journal without a connection", settings, crop=(260, 30, 720, 500), timeout=10)
    empty = driver.capture("journal-empty")
    directory = driver.home / ".stillus" / "ai" / "journal"
    directory.mkdir(parents=True, exist_ok=True)
    stamp = int(time.time() * 1000)
    ids = [f"{stamp:016x}{1:016x}{index:016x}" for index in range(3)]
    for index, identity in enumerate(ids):
        record = {"version": 1, "id": identity, "operation": "ai/catalog/fixture", "started_ms": stamp,
                  "provider": "openai", "purpose": "models/list", "endpoint": "https://api.openai.com/v1/models",
                  "parameters": {}, "request": None, "response": {"data": [{"id": "fixture/model"}]},
                  "http_status": 200, "duration_ms": 15, "status": "success" if index != 1 else "error",
                  "error": None if index != 1 else "Unauthorized", "model": None, "input_tokens": None,
                  "output_tokens": None, "tool_call": None, "content_policy": "public"}
        path = directory / f"{identity}.json"
        path.write_text(json.dumps(record))
        path.chmod(0o600)
    driver.wait_for_visual_change("new journal requests", empty, crop=(260, 180, 720, 230), timeout=10)
    rows = driver.capture("journal-rows")
    driver.click_point(420, 208)
    driver.wait_for_visual_change("request details", rows, crop=(260, 385, 720, 300), timeout=10)
    driver.wait_for_stable_frame("journal details rendered", crop=(260, 385, 720, 300), minimum_dark_pixels=800)
    preview = driver.capture("journal-details")
    export_screenshot(preview, Path("/workspace/dist/ai-journal-preview.png"))
    driver.click_point(365, 45)
    driver.wait_for_visual_change("history cleanup confirmation", preview, crop=(260, 180, 720, 160), timeout=10)
    driver.wait_for_stable_frame("history cleanup controls", crop=(260, 180, 720, 160), stable_for=0.5)
    driver.click_point(310, 223)
    wait_until("journal history cleared", lambda: not list(directory.glob("*.json")), timeout=10)
    driver.click_point(278, 45)
    wait_for_ai_controls(driver)
    driver.close_app()
    driver.start_app(workspace, "journal-restart", environment_overrides={"STILLUS_TEST_AI": "1"})
    driver.click("settings")
    wait_for_ai_controls(driver)
    driver.click_point(*AI_SIDEBAR_ITEM)
    wait_for_ai_controls(driver)
    driver.click_point(1140, 54)
    if list(directory.glob("*.json")):
        raise AcceptanceFailure("cleared journal returned after restart")
    driver.close_app()
    if {path: path.read_bytes() for path in original} != original:
        raise AcceptanceFailure("journal changed workspace notes")

CHAT_READING_CROP = (280, 80, 840, 180)


def wait_for_chat_reading_position(driver: WindowDriver, *, timeout: float = 10) -> Path:
    """Wait for painted scroll acknowledgement, not a stable pre-input frame."""
    previous: Path | None = None
    stable_since: float | None = None

    def ready() -> bool:
        nonlocal previous, stable_since
        frame = driver.capture("chat-reading-position")
        # The thumb must reach the top of the overflowing history, and Newest
        # must be enabled: both the wheel input and follow-mode change painted.
        thumb = shaded_row_runs(frame, x=1215, y=64, height=508, max_luminance=160)
        newest = sum(value < 160 for value in crop_luminances(frame, (1008, 28, 12, 16)).values())
        acknowledged = (
            len(thumb) == 1 and thumb[0][0] <= 66
            and 30 < thumb[0][1] - thumb[0][0] < 468 and newest >= 5
            and dark_pixel_count(frame, crop=CHAT_READING_CROP) >= 100
        )
        unchanged = previous is not None and image_difference(previous, frame, crop=CHAT_READING_CROP) == 0
        if previous is not None:
            previous.unlink(missing_ok=True)
        previous = frame
        if not acknowledged or not unchanged:
            stable_since = None
            return False
        now = time.monotonic()
        if stable_since is None:
            stable_since = now
        return now - stable_since >= 0.2

    wait_until("wheel input reaches the top of chat history with following disabled",
               ready, timeout=timeout, interval=0.05)
    assert previous is not None
    return previous


def chat_scenario(driver: WindowDriver, workspace: Path) -> None:
    """Native chat creation, composer persistence and provider streaming without network."""
    original = {path: path.read_bytes() for path in (workspace / "notes").glob("*.md")}
    root = workspace / ".stillus" / "engines" / "ai" / "chat"
    fixture = {"STILLUS_TEST_AI": "1"}
    driver.start_app(workspace, "chat", environment_overrides=fixture)
    driver.click("create_menu")
    driver.click_point(116, 165)
    wait_until("a chat created through the plus menu", lambda: root.exists() and len(list(root.glob("*/metadata.json"))) == 1)
    chat = next(root.glob("*/metadata.json")).parent
    driver.wait_for_stable_frame("empty chat", crop=(260, 40, 960, 500), stable_for=0.2)
    export_screenshot(driver.capture("chat-empty"), Path("/workspace/dist/chat-empty.png"))
    composer_crop = (280, 620, 900, 105)
    unfocused = driver.wait_for_stable_frame("unfocused placeholder has no blinking caret",
                                             crop=composer_crop, stable_for=1.2)
    driver.click_point(480, 670)
    focused = driver.wait_for_visual_change("empty composer caret starts before placeholder",
                                            unfocused, crop=(283, 625, 5, 40),
                                            minimum_pixels=10, timeout=2)
    driver.wait_for_visual_change("focused composer caret blinks", focused,
                                  crop=(283, 625, 5, 40), minimum_pixels=10, timeout=2)
    driver.xdotool("windowfocus", "0")
    inactive = driver.wait_for_stable_frame("inactive window hides composer caret",
                                           crop=composer_crop, stable_for=1.2)
    if image_difference(unfocused, inactive, crop=composer_crop) != 0:
        raise AcceptanceFailure("inactive window retained the composer caret")
    driver.xdotool("windowfocus", "--sync", driver.window_id)
    # Giving the toolbar keyboard focus must hide the caret without changing the draft.
    driver.click_point(1052, 36)
    driver.click_point(520, 80)
    driver.wait_for_stable_frame("composer loses focus to rename input",
                                 crop=composer_crop, stable_for=1.2)
    driver.click_point(1052, 36)
    driver.click_point(480, 670)
    driver.type_text("First line")
    driver.key("shift+Return")
    driver.type_text("second line")
    wait_until("multiline chat draft persisted", lambda: json.loads((chat / "draft.json").read_text())["data"]["text"] == "First line\nsecond line", timeout=10)
    set_clipboard_text(driver.environment, " pasted")
    driver.key("ctrl+v")
    wait_until("composer paste", lambda: json.loads((chat / "draft.json").read_text())["data"]["text"] == "First line\nsecond line pasted")
    driver.key("ctrl+z")
    wait_until("composer Undo", lambda: json.loads((chat / "draft.json").read_text())["data"]["text"] == "First line\nsecond line")
    driver.key("ctrl+shift+z")
    wait_until("composer Redo", lambda: json.loads((chat / "draft.json").read_text())["data"]["text"] == "First line\nsecond line pasted")
    driver.key("ctrl+z")
    wait_until("composer restores original draft", lambda: json.loads((chat / "draft.json").read_text())["data"]["text"] == "First line\nsecond line")
    driver.close_app()
    driver.start_app(workspace, "chat-restored", environment_overrides=fixture)
    driver.wait_for_stable_frame("restored chat composer", crop=(260, 540, 960, 60), stable_for=0.2)
    driver.wait_for_stable_frame("restored draft has no unfocused caret",
                                 crop=composer_crop, stable_for=1.2)
    state = json.loads((workspace / ".stillus" / "settings.json").read_text())
    if state.get("selected_chat") != chat.name:
        raise AcceptanceFailure("chat selection was not restored")
    driver.click("settings")
    wait_for_ai_controls(driver)
    driver.click_point(*AI_SIDEBAR_ITEM)
    wait_for_ai_controls(driver)
    luminances = crop_luminances(driver.capture("chat-ai-connect"), (AI_CONTENT_LEFT, 0, 1, SCREEN_HEIGHT))
    rows = {row for (_x, row), luminance in luminances.items() if luminance <= AI_CARD_BORDER_LUMINANCE}
    cards = [(start - AI_CARD_CORNER, end + AI_CARD_CORNER) for start, end in column_runs(rows, merge_gap=0) if end-start >= AI_CARD_MIN_HEIGHT]
    if not cards:
        raise AcceptanceFailure("AI connection card is missing")
    before_key = driver.capture("chat-before-key")
    set_clipboard_text(driver.environment, "sk-proj-abcdefghijklmnopqrstuv")
    driver.click_point(941, cards[0][0] + 62)
    driver.wait_for_visual_change("pasted fixture credential", before_key,
                                  crop=(299, 200, 600, 43), timeout=10)
    wait_for_ai_controls(driver)
    # Provider detection inserts a label and moves the Connect action down.
    actions = shaded_row_runs(driver.capture("chat-connect-action"),
                              x=AI_PRIMARY_PROBE_X, y=0, height=SCREEN_HEIGHT,
                              max_luminance=AI_ACCENT_LUMINANCE)
    actions = [(start, end) for start, end in actions if end - start >= 20]
    if len(actions) != 1:
        raise AcceptanceFailure("chat provider Connect action is not enabled")
    driver.click_point(AI_PRIMARY_PROBE_X, sum(actions[0]) // 2)
    config = driver.home / ".stillus.cfg"
    wait_until("fixture provider connected", lambda: json.loads(config.read_text()).get("ai", {}).get("connection") is not None)
    if json.loads((chat / "draft.json").read_text())["data"]["text"] != "First line\nsecond line":
        raise AcceptanceFailure("AI settings input reached the hidden chat composer")
    driver.click("settings_back")
    driver.wait_for_stable_frame("chat after connecting", crop=(260, 540, 960, 60), stable_for=0.2)
    driver.click_point(480, 670)
    driver.key("Return")
    wait_until("chat generation completed", lambda: (chat / "run.json").exists() and json.loads((chat / "run.json").read_text())["data"]["status"] == "completed", timeout=15)
    driver.wait_for_stable_frame("native streamed reply", crop=(260, 70, 960, 460), stable_for=0.2)
    driver.wait_for_stable_frame("empty composer after sending has no unfocused caret",
                                 crop=(280, 615, 900, 105), stable_for=1.2)
    response_frame = driver.capture("chat-response")
    export_screenshot(response_frame, Path("/workspace/dist/chat-preview.png"))
    # A 75%-width user bubble starts at x=512; assistant prose stays at x=276.
    if near_color_pixel_count(response_frame, (246, 247, 248), crop=(520, 70, 650, 70), tolerance=1) < 20000:
        raise AcceptanceFailure("user message does not have a right-aligned bubble")
    if dark_pixel_count(response_frame, crop=(276, 70, 200, 70)) > 0:
        raise AcceptanceFailure("user message escaped the right-hand bubble")
    history = [json.loads(path.read_text())["data"] for path in sorted((chat / "messages").glob("*.json"))]
    if [message["role"] for message in history] != ["user", "assistant"]:
        raise AcceptanceFailure("chat did not preserve the linear exchange")
    if "Ответ сохранён" not in history[-1]["text"]:
        raise AcceptanceFailure("fixture response is missing from chat history")
    if json.loads((chat / "metadata.json").read_text())["data"]["common"]["title"] != "First line":
        raise AcceptanceFailure("first message did not set the chat title")
    journal = driver.home / ".stillus" / "ai" / "journal"
    if not any(json.loads(path.read_text()).get("chat") == chat.name for path in journal.glob("*.json")):
        raise AcceptanceFailure("generation is not linked to the journal")
    # The chat opens its request directly in the existing journal page.
    driver.click_point(946, 36)
    driver.wait_for_stable_frame("chat request journal", crop=(280, 100, 850, 300), stable_for=0.2)
    export_screenshot(driver.capture("chat-journal"), Path("/workspace/dist/chat-journal.png"))
    driver.click_point(302, 45)

    def create_and_send(text: str) -> Path:
        before = set(root.glob("*/metadata.json"))
        driver.click("create_menu")
        driver.click_point(116, 165)
        wait_until("another chat created", lambda: len(set(root.glob("*/metadata.json")) - before) == 1)
        target = next(iter(set(root.glob("*/metadata.json")) - before)).parent
        driver.wait_for_stable_frame("new chat header", crop=(276, 24, 530, 28), stable_for=0.2)
        driver.click_point(480, 670)
        driver.type_text(text)
        driver.key("Return")
        wait_until("chat task started", lambda: (target / "run.json").exists() and json.loads((target / "run.json").read_text())["data"]["status"] == "running", timeout=10)
        return target

    first = create_and_send("slow first")
    second = create_and_send("slow second")
    if json.loads((first / "run.json").read_text())["data"]["status"] != "running":
        raise AcceptanceFailure("two native chat tasks did not overlap")
    driver.click("settings")
    for target in (first, second):
        wait_until("background chat completed", lambda target=target: json.loads((target / "run.json").read_text())["data"]["status"] == "completed", timeout=25)
        if not json.loads((target / "run.json").read_text())["data"]["unread"]:
            raise AcceptanceFailure("a hidden chat response was incorrectly marked read")
    driver.click("settings_back")
    wait_until("visible response marked read", lambda: not json.loads((second / "run.json").read_text())["data"]["unread"], timeout=10)
    stopped = create_and_send("slow stopped")
    before_trash = driver.capture("chat-before-trash")
    driver.click_point(1204, 36)
    wait_until("running chat stopped before trash", lambda: json.loads((stopped / "run.json").read_text())["data"]["status"] == "stopped" and json.loads((stopped / "metadata.json").read_text())["data"]["common"]["deleted"], timeout=15)
    conversation = {path.name: path.read_bytes() for path in (stopped / "messages").glob("*.json")}
    driver.wait_for_visual_change("chat Restore action", before_trash,
                                  crop=(1188, 20, 32, 32), timeout=10)
    driver.wait_for_stable_frame("chat Restore action", crop=(1188, 20, 32, 32), stable_for=0.2)
    driver.click_point(1204, 36)
    wait_until("chat restored from trash", lambda: not json.loads((stopped / "metadata.json").read_text())["data"]["common"]["deleted"])
    if {path.name: path.read_bytes() for path in (stopped / "messages").glob("*.json")} != conversation:
        raise AcceptanceFailure("restoring a chat rewrote its conversation")
    driver.wait_for_stable_frame("restored chat toolbar", crop=(1000, 20, 230, 34), stable_for=0.2)
    driver.click_point(1052, 36)
    driver.click_point(520, 80)
    driver.key("ctrl+a")
    driver.type_text("Organized chat")
    driver.key("Return")
    wait_until("chat renamed", lambda: json.loads((stopped / "metadata.json").read_text())["data"]["common"]["title"] == "Organized chat")
    driver.click_point(1090, 36)
    driver.click_point(520, 80)
    driver.key("ctrl+a")
    driver.type_text("Reading")
    driver.key("Return")
    wait_until("chat categorized", lambda: json.loads((stopped / "metadata.json").read_text())["data"]["common"]["categories"] == ["Reading"])
    before_pin = driver.capture("chat-before-pin")
    driver.click_point(1128, 36)
    wait_until("chat pinned", lambda: json.loads((stopped / "metadata.json").read_text())["data"]["common"]["pinned"])
    driver.wait_for_visual_change("pinned metadata applied to toolbar", before_pin,
                                  crop=(1112, 20, 32, 32), minimum_pixels=30)
    driver.wait_for_stable_frame("pinned toolbar", crop=(1112, 20, 32, 32), stable_for=0.2)
    driver.click_point(1166, 36)
    wait_until("chat favorited", lambda: json.loads((stopped / "metadata.json").read_text())["data"]["common"]["favorited"])
    if json.loads((stopped / "metadata.json").read_text())["data"]["automatic_title"]:
        raise AcceptanceFailure("manual chat title remained automatic")
    if {path.name: path.read_bytes() for path in (stopped / "messages").glob("*.json")} != conversation:
        raise AcceptanceFailure("organizing a chat rewrote its conversation")
    export_screenshot(driver.capture("chat-background"), Path("/workspace/dist/chat-background.png"))
    layout = create_and_send("layout fixture")
    driver.click_point(480, 670)
    driver.type_text("next draft")
    wait_until("draft input processed before testing streamed history",
               lambda: json.loads((layout / "draft.json").read_text())["data"]["text"] == "next draft")
    wait_until("streamed history grows beyond its viewport", lambda: sum(
        end - start for start, end in shaded_row_runs(driver.capture("chat-growing"),
                                                     x=1215, y=100, height=450,
                                                     max_luminance=160)) > 30, timeout=15)
    driver.xdotool("mousemove", "--window", driver.window_id, "700", "250", "click", "--repeat", "10", "--delay", "30", "4")
    reading = wait_for_chat_reading_position(driver)
    wait_until("long streamed response completed", lambda: json.loads((layout / "run.json").read_text())["data"]["status"] == "completed", timeout=20)
    # A streamed update must not replace the editor or take away its focus.
    driver.type_text(" still focused")
    wait_until("draft retains focus during streaming", lambda: json.loads((layout / "draft.json").read_text())["data"]["text"] == "next draft still focused")
    after_stream = driver.wait_for_stable_frame("chat reading position after streaming",
                                               crop=CHAT_READING_CROP, stable_for=0.2,
                                               minimum_dark_pixels=100, timeout=10)
    if image_difference(reading, after_stream, crop=CHAT_READING_CROP) != 0:
        raise AcceptanceFailure("streaming pulled the reader away from earlier text")
    driver.click_point(1014, 36)
    driver.click_point(1052, 36)
    driver.click_point(520, 80)
    driver.click_point(1052, 36)
    long_frame = driver.wait_for_stable_frame("bounded long streamed answer", crop=(276, 65, 940, 500), stable_for=0.2)
    if dark_pixel_count(long_frame, crop=(1192, 24, 24, 24)) < 10:
        raise AcceptanceFailure("long answer pushed the toolbar off screen")
    if dark_pixel_count(long_frame, crop=(284, 618, 250, 35)) < 20:
        raise AcceptanceFailure("long answer pushed the composer off screen")
    export_screenshot(long_frame, Path("/workspace/dist/chat-long.png"))
    driver.close_app()
    chat_paging_layout_scenario(driver, workspace, layout, fixture)
    if {path: path.read_bytes() for path in original} != original:
        raise AcceptanceFailure("ordinary chat modified workspace notes")


def chat_paging_layout_scenario(driver: WindowDriver, workspace: Path, chat: Path,
                                fixture: dict[str, str]) -> None:
    """Seed only the closed, disposable workspace with a multi-page conversation."""
    for path in (chat / "messages").glob("*.json"):
        path.unlink()
    for index in range(64):
        message_id = f"{index + 1:032x}"
        message = {"id": message_id, "run": "fixture", "role": "assistant" if index % 2 else "user",
                   "text": f"Message {index:02d}: a distinct history anchor.\nSecond line {index:02d}.",
                   "delivery": "complete", "created_ms": index, "tool": None}
        if index == 63:
            message["role"] = "tool"
            message["tool"] = {"id": "fixture/tool", "name": "search/query", "arguments": {"query": "topic"},
                               "state": "completed", "result": {"text": "bounded result " * 100}}
        (chat / "messages" / f"{message_id}.json").write_text(json.dumps({"version": 1, "data": message}))
    metadata = json.loads((chat / "metadata.json").read_text())
    metadata["data"]["common"]["title"] = "Long chat title " * 15
    (chat / "metadata.json").write_text(json.dumps(metadata))
    driver.start_app(workspace, "chat-pages", environment_overrides=fixture)
    # Compare the message content, excluding Copy button outlines whose pixel
    # rounding can change when preceding rows are inserted.
    history_crop = (280, 90, 840, 450)

    def navigation_ready(earlier: bool, newest: bool) -> None:
        driver.move_to("sidebar_blank")

        def matches() -> bool:
            frame = driver.capture("chat-navigation-state")
            up = sum(value < 160 for value in crop_luminances(frame, (970, 28, 12, 16)).values())
            down = sum(value < 160 for value in crop_luminances(frame, (1008, 28, 12, 16)).values())
            return (up >= 5) == earlier and (down >= 5) == newest

        wait_until("chat page load finishes and navigation availability updates", matches)

    navigation_ready(True, False)
    latest = driver.wait_for_stable_frame("latest conversation page", crop=history_crop, stable_for=0.3)
    driver.click_point(976, 36)
    # Loading a preceding page must leave the currently visible message stationary.
    driver.wait_for_visual_change("earlier page changes toolbar availability", latest,
                                  crop=(960, 20, 70, 32), minimum_pixels=10)
    navigation_ready(False, True)
    previous = driver.wait_for_stable_frame("history anchor after loading earlier page", crop=history_crop, stable_for=0.3)
    if image_difference(latest, previous, crop=history_crop) != 0:
        raise AcceptanceFailure("loading earlier messages moved the visible history anchor")
    driver.click_point(976, 36)
    driver.move_to("sidebar_blank")
    unchanged = driver.wait_for_stable_frame("earlier icon is disabled at oldest page", crop=history_crop, stable_for=0.3)
    if image_difference(previous, unchanged, crop=history_crop) != 0:
        raise AcceptanceFailure("disabled earlier icon changed the history")
    driver.xdotool("mousemove", "--window", driver.window_id, "700", "250", "click", "--repeat", "10", "--delay", "50", "4")
    scrolled = driver.wait_for_visual_change("history scrolls independently", unchanged,
                                            crop=history_crop, minimum_pixels=200)
    if image_difference(unchanged, scrolled, crop=(920, 20, 300, 32)) != 0:
        raise AcceptanceFailure("scrolling history moved the toolbar")
    driver.click_point(1014, 36)
    navigation_ready(True, False)
    newest = driver.wait_for_stable_frame("down arrow returns to newest messages", crop=history_crop, stable_for=0.3)
    if image_difference(latest, newest, crop=history_crop) != 0:
        raise AcceptanceFailure("down arrow did not return to the latest messages")
    driver.click_point(1180, 420)
    wait_until("copy a message after paging", lambda: clipboard_text(driver.environment) == "Message 62: a distinct history anchor.\nSecond line 62.")
    # The last card has just one compact header; clicking expands its bounded result.
    driver.click_point(350, 552)
    driver.xdotool("mousemove", "--window", driver.window_id, "700", "250", "click", "--repeat", "10", "--delay", "30", "5")
    expanded = driver.wait_for_visual_change("tool card expands without a journal row", newest,
                                            crop=(300, 430, 750, 100), minimum_pixels=200)
    expanded = driver.wait_for_stable_frame("expanded tool arguments and result",
                                           crop=(300, 430, 750, 100), stable_for=0.3)
    export_screenshot(expanded, Path("/workspace/dist/chat-tool.png"))
    driver.resize_window(860, 560)
    driver.xdotool("mousemove", "--window", driver.window_id, "255", "300", "mousedown", "1",
                    "mousemove", "--sync", "--window", driver.window_id, "480", "300", "mouseup", "1")
    driver.wait_for_stable_frame("minimum window with widest sidebar", stable_for=0.3)
    driver.click_point(592, 75)
    narrow = driver.wait_for_stable_frame("expanded result inside narrow history", stable_for=0.3)
    if abs(sidebar_boundary_x(narrow, y=530) - 480) > 1:
        raise AcceptanceFailure("chat sidebar did not resize to its maximum")
    if dark_pixel_count(narrow, crop=(782, 60, 45, 36)) < 15:
        raise AcceptanceFailure("wrapped toolbar buttons are outside the minimum window")
    if dark_pixel_count(narrow, crop=(505, 335, 250, 90)) < 20:
        raise AcceptanceFailure("composer is outside the minimum window")
    export_screenshot(narrow, Path("/workspace/dist/chat-narrow.png"))
    # Journal remains available in the wrapped toolbar and Back returns to this chat.
    driver.click_point(516, 78)
    driver.wait_for_visual_change("journal opens from wrapped toolbar", narrow,
                                  crop=(500, 100, 330, 200), minimum_pixels=100)
    driver.click_point(526, 45)
    returned = driver.wait_for_stable_frame("journal Back restores narrow chat",
                                           crop=(500, 333, 340, 115), stable_for=0.3)
    if image_difference(narrow, returned, crop=(500, 333, 340, 115)) != 0:
        raise AcceptanceFailure("journal Back did not restore the chat composer")
    driver.close_app()


def components_scenario(driver: WindowDriver, workspace: Path) -> None:
    driver.start_app(workspace, "gallery", environment_overrides={"STILLUS_TEST_COMPONENTS": "1"})
    fields = (24, 72, 900, 240)
    driver.wait_for_stable_frame("unfocused component fields", crop=fields, stable_for=1.2)
    # Every kind of icon button, including the unavailable Send, has a hover title.
    for x in (40, 80, 120, 155):
        driver.xdotool("mousemove", "--window", driver.window_id, "900", "500")
        baseline = driver.wait_for_stable_frame("no tooltip", crop=(20, 20, 400, 120), stable_for=0.2)
        driver.xdotool("mousemove", "--window", driver.window_id, str(x), "40")
        tip = driver.wait_for_visual_change("icon hover tooltip", baseline, crop=(20, 55, 400, 65), minimum_pixels=50, timeout=3)
        if x == 40:
            english_tip = tip
    driver.xdotool("mousemove", "--window", driver.window_id, "900", "500")
    counters = driver.wait_for_stable_frame("initial action counters", crop=(24, 384, 950, 24), stable_for=0.2)
    driver.click_point(80, 40)
    if image_difference(counters, driver.capture("disabled-action"), crop=(24, 384, 950, 24)):
        raise AcceptanceFailure("disabled button invoked its callback")
    driver.click_point(40, 40)
    driver.wait_for_visual_change("enabled action invoked once", counters, crop=(24, 384, 950, 24), minimum_pixels=3)
    # Tab reaches the unavailable action and exposes its title without a mouse hover.
    driver.xdotool("mousemove", "--window", driver.window_id, "900", "500")
    no_tip = driver.wait_for_stable_frame("before keyboard title", crop=(20, 55, 400, 65), stable_for=0.2)
    driver.key("Tab")
    driver.wait_for_visual_change("keyboard focus tooltip", no_tip, crop=(20, 55, 400, 65), minimum_pixels=50)
    driver.click_point(75, 348)  # Disable Copy and the first textarea.
    counters = driver.wait_for_stable_frame("disabled component state", crop=(24, 384, 950, 24), stable_for=0.2)
    driver.click_point(40, 40)
    if image_difference(counters, driver.capture("changed-availability"), crop=(24, 384, 950, 24)):
        raise AcceptanceFailure("button ignored its updated availability")
    driver.xdotool("mousemove", "--window", driver.window_id, "900", "500")
    no_tip = driver.wait_for_stable_frame("disabled Copy before hover", crop=(20, 55, 400, 65), stable_for=0.2)
    driver.xdotool("mousemove", "--window", driver.window_id, "40", "40")
    driver.wait_for_visual_change("disabled Copy retains its tooltip", no_tip, crop=(20, 55, 400, 65), minimum_pixels=50)
    driver.click_point(75, 348)
    driver.click_point(40, 40)
    driver.wait_for_visual_change("reenabled button works", counters, crop=(24, 384, 950, 24), minimum_pixels=3)
    driver.click_point(120, 40)
    driver.xdotool("mousemove", "--window", driver.window_id, "900", "500")
    no_tip = driver.wait_for_stable_frame("before toggled title", crop=(20, 55, 400, 65), stable_for=0.2)
    driver.xdotool("mousemove", "--window", driver.window_id, "120", "40")
    toggled = driver.wait_for_visual_change("toggled action title", no_tip, crop=(20, 55, 400, 65), minimum_pixels=50)
    driver.click_point(120, 40)
    driver.xdotool("mousemove", "--window", driver.window_id, "900", "500")
    no_tip = driver.wait_for_stable_frame("before untoggled title", crop=(20, 55, 400, 65), stable_for=0.2)
    driver.xdotool("mousemove", "--window", driver.window_id, "120", "40")
    untoggled = driver.wait_for_visual_change("untoggled tooltip appears", no_tip, crop=(20, 55, 400, 65), minimum_pixels=50, timeout=3)
    if image_difference(toggled, untoggled, crop=(125, 55, 180, 32)) < 20:
        raise AcceptanceFailure("tooltip did not change with toggle state")
    driver.xdotool("mousemove", "--window", driver.window_id, "900", "500")
    baseline = driver.wait_for_stable_frame("placeholder without caret", crop=fields, stable_for=1.2)
    driver.click_point(100, 100)
    caret = driver.wait_for_visual_change("caret precedes placeholder", baseline, crop=(32, 80, 4, 30), minimum_pixels=8, timeout=3)
    driver.wait_for_visual_change("focused caret blinks", caret, crop=(32, 80, 4, 30), minimum_pixels=8, timeout=3)
    driver.type_text("first")
    driver.key("shift+Return")
    driver.type_text("second")
    driver.key("ctrl+a")
    driver.key("ctrl+c")
    wait_until("multiline input", lambda: clipboard_text(driver.environment) == "first\nsecond")
    driver.key("End")
    set_clipboard_text(driver.environment, " pasted")
    driver.key("ctrl+v")
    driver.key("ctrl+z")
    driver.key("ctrl+a")
    driver.key("ctrl+c")
    wait_until("textarea Undo", lambda: clipboard_text(driver.environment) == "first\nsecond")
    driver.key("ctrl+shift+z")
    driver.key("ctrl+a")
    driver.key("ctrl+c")
    wait_until("textarea Redo", lambda: clipboard_text(driver.environment) == "first\nsecond pasted")
    driver.click_point(100, 225)
    driver.wait_for_stable_frame("first field loses caret", crop=(24, 72, 900, 112), stable_for=1.2)
    driver.type_text("other")
    driver.key("Return")
    driver.type_text("line")
    driver.key("ctrl+a")
    driver.key("ctrl+c")
    wait_until("independent field and newline mode", lambda: clipboard_text(driver.environment) == "other\nline")
    driver.xdotool("windowfocus", "0")
    driver.wait_for_stable_frame("inactive window hides every caret", crop=fields, stable_for=1.2)
    driver.xdotool("windowfocus", "--sync", driver.window_id)
    driver.click_point(215, 40)
    driver.click_point(100, 100)
    driver.type_text("after clear")
    driver.key("ctrl+a")
    driver.key("ctrl+c")
    wait_until("external reset updates existing editor", lambda: clipboard_text(driver.environment) == "after clear")
    driver.click_point(185, 348)
    driver.xdotool("mousemove", "--window", driver.window_id, "900", "500")
    no_tip = driver.wait_for_stable_frame("Russian controls", crop=(20, 55, 400, 65), stable_for=0.2)
    driver.xdotool("mousemove", "--window", driver.window_id, "40", "40")
    russian_tip = driver.wait_for_visual_change("translated tooltip appears", no_tip, crop=(20, 55, 400, 65), minimum_pixels=50, timeout=3)
    if image_difference(english_tip, russian_tip, crop=(45, 55, 180, 32)) < 20:
        raise AcceptanceFailure("tooltip did not follow the locale")
    driver.resize_window(860, 560)
    driver.xdotool("mousemove", "--window", driver.window_id, "700", "500")
    driver.wait_for_stable_frame("components in a narrow window", stable_for=0.3)
    driver.close_app()


SCENARIOS: dict[str, Callable[[WindowDriver, Path], None]] = {
    "components": components_scenario,
    "chat": chat_scenario,
    "ai": ai_settings_scenario,
    "ai_journal": ai_journal_scenario,
    "updates": updates_scenario,
    "localization": localization_scenario,
    "rss_cards": rss_cards_scenario,
    "rss_filters": rss_filters_scenario,
    "rss_keyboard": rss_keyboard_scenario,
    "creation": creation_scenario,
    "workspace": workspace_scenario,
    "compatibility": compatibility_scenario,
    "categories": categories_scenario,
    "interaction": interaction_scenario,
    "lifecycle": lifecycle_scenario,
    "tags": tags_scenario,
    "caret": caret_scenario,
    "editor": editor_scenario,
    "context_menu": context_menu_scenario,
    "selection": selection_scenario,
    "persistence": persistence_scenario,
    "recovery": recovery_scenario,
    "conflict": conflict_scenario,
    "search": search_scenario,
    "find": find_scenario,
    "resize": resize_scenario,
    "password_dialog": password_dialog_scenario,
    "password_change": password_change_scenario,
    "secure": secure_scenario,
    "secure_recovery": secure_recovery_scenario,
    "secure_conflict": secure_conflict_scenario,
    "secure_integrity": secure_integrity_scenario,
    "visual": visual_scenario,
}


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("scenario", choices=sorted(SCENARIOS))
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    if not APP_BINARY.is_file():
        raise SystemExit(f"Stillus binary is missing: {APP_BINARY}")

    with tempfile.TemporaryDirectory(prefix=f"stillus-ui-{arguments.scenario}-") as directory:
        temporary_root = Path(directory)
        driver = WindowDriver(arguments.scenario, temporary_root)
        failed = False
        screenshot: Path | None = None

        def record_failure(error: Exception, stage: str) -> None:
            nonlocal failed
            if not failed:
                print(f"UI_ACCEPTANCE_FAIL scenario={arguments.scenario}",
                      file=sys.stderr, flush=True)
            failed = True
            for line in failure_diagnostics(arguments.scenario, stage, error):
                print(line, file=sys.stderr, flush=True)

        try:
            workspace = copy_demo(temporary_root)
            driver.start_xvfb()
            driver.set_stage("scenario")
            SCENARIOS[arguments.scenario](driver, workspace)
        except Exception as error:
            # Flush the original failure before fallible artifact collection.
            record_failure(error, driver.stage)
            try:
                screenshot = driver.capture_failure()
            except Exception as capture_error:
                record_failure(capture_error, "capture")
        finally:
            try:
                driver.cleanup()
            except Exception as cleanup_error:
                record_failure(cleanup_error, "cleanup")
            try:
                if failed:
                    driver.sanitize_failure_logs()
                else:
                    driver.remove_success_artifacts()
            except Exception as artifact_error:
                record_failure(artifact_error, "artifacts")
        if failed:
            if screenshot is not None:
                print(f"failure screenshot: {screenshot}", file=sys.stderr)
            print(f"diagnostic directory: {ARTIFACT_ROOT}", file=sys.stderr)
            return 1
        print(f"UI_ACCEPTANCE_PASS scenario={arguments.scenario}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
