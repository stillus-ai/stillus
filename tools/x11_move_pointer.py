#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only

"""Move the X11 pointer for deterministic hover screenshots."""

from __future__ import annotations

import ctypes
import os
import sys


def main() -> int:
    if len(sys.argv) != 3:
        raise SystemExit("usage: x11_move_pointer.py X Y")
    display_name = os.environ.get("DISPLAY")
    if not display_name:
        raise SystemExit("DISPLAY is not set")

    x = int(sys.argv[1])
    y = int(sys.argv[2])
    x11 = ctypes.CDLL("libX11.so.6")
    x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
    x11.XOpenDisplay.restype = ctypes.c_void_p
    x11.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
    x11.XDefaultRootWindow.restype = ctypes.c_ulong
    x11.XWarpPointer.argtypes = [
        ctypes.c_void_p,
        ctypes.c_ulong,
        ctypes.c_ulong,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_uint,
        ctypes.c_uint,
        ctypes.c_int,
        ctypes.c_int,
    ]
    x11.XWarpPointer.restype = ctypes.c_int
    x11.XFlush.argtypes = [ctypes.c_void_p]
    x11.XFlush.restype = ctypes.c_int
    x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
    x11.XCloseDisplay.restype = ctypes.c_int

    display = x11.XOpenDisplay(display_name.encode())
    if not display:
        raise SystemExit(f"cannot open X11 display {display_name}")
    root = x11.XDefaultRootWindow(display)
    try:
        x11.XWarpPointer(display, 0, root, 0, 0, 0, 0, x, y)
        x11.XFlush(display)
    finally:
        x11.XCloseDisplay(display)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
