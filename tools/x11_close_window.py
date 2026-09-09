# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only

"""Request a normal application close on Xvfb without a window manager."""

import ctypes


class ClientMessageData(ctypes.Union):
    _fields_ = [
        ("b", ctypes.c_char * 20),
        ("s", ctypes.c_short * 10),
        ("l", ctypes.c_long * 5),
    ]


class ClientMessage(ctypes.Structure):
    _fields_ = [
        ("type", ctypes.c_int),
        ("serial", ctypes.c_ulong),
        ("send_event", ctypes.c_int),
        ("display", ctypes.c_void_p),
        ("window", ctypes.c_ulong),
        ("message_type", ctypes.c_ulong),
        ("format", ctypes.c_int),
        ("data", ClientMessageData),
    ]


class XEvent(ctypes.Union):
    _fields_ = [("client", ClientMessage), ("pad", ctypes.c_long * 24)]


def request_window_close(window: str, environment: dict[str, str]) -> None:
    # xdotool windowclose uses XDestroyWindow. That bypasses CloseRequested
    # and races winit's geometry queries when ConfigureNotify is still queued.
    # Send the same protocol message as a window manager's Close action instead.
    window_id = int(window, 0)
    if window_id <= 0:
        raise ValueError("invalid X11 window")
    display_name = environment.get("DISPLAY")
    if not display_name:
        raise ValueError("DISPLAY is not set")
    x11 = ctypes.CDLL("libX11.so.6")
    x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
    x11.XOpenDisplay.restype = ctypes.c_void_p
    x11.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
    x11.XInternAtom.restype = ctypes.c_ulong
    x11.XSendEvent.argtypes = [
        ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_long,
        ctypes.POINTER(XEvent),
    ]
    x11.XSendEvent.restype = ctypes.c_int
    x11.XFlush.argtypes = [ctypes.c_void_p]
    x11.XFlush.restype = ctypes.c_int
    x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
    x11.XCloseDisplay.restype = ctypes.c_int

    display = x11.XOpenDisplay(display_name.encode())
    if not display:
        raise RuntimeError("cannot open X11 display for close request")
    try:
        protocols = x11.XInternAtom(display, b"WM_PROTOCOLS", True)
        delete_window = x11.XInternAtom(display, b"WM_DELETE_WINDOW", True)
        if not protocols or not delete_window:
            raise RuntimeError("X11 close protocol is unavailable")
        event = XEvent()
        event.client.type = 33  # ClientMessage
        event.client.display = display
        event.client.window = window_id
        event.client.message_type = protocols
        event.client.format = 32
        event.client.data.l[0] = delete_window
        event.client.data.l[1] = 0  # CurrentTime
        if not x11.XSendEvent(display, window_id, False, 0, ctypes.byref(event)):
            raise RuntimeError("could not send X11 close request")
        x11.XFlush(display)
    finally:
        x11.XCloseDisplay(display)
