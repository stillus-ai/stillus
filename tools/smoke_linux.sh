#!/bin/sh
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only

set -eu

binary="/workspace/dist/linux/$(uname -m)/stillus"
test -x "$binary"
workspace=$(mktemp -d)
server=
cleanup() {
    if [ -n "$server" ]; then
        kill "$server" 2>/dev/null || true
        wait "$server" 2>/dev/null || true
    fi
    rm -rf -- "$workspace"
}
trap cleanup EXIT
python3 -B tools/generate_demo_data.py "$workspace/workspace"
mkdir -m 700 "$workspace/home" "$workspace/runtime"
Xvfb :99 -screen 0 1240x800x24 >"$workspace/xvfb.log" 2>&1 &
server=$!
attempt=0
until [ -S /tmp/.X11-unix/X99 ]; do
    attempt=$((attempt + 1))
    if ! kill -0 "$server" 2>/dev/null || [ "$attempt" -ge 50 ]; then
        cat "$workspace/xvfb.log" >&2
        exit 1
    fi
    sleep 0.1
done

HOME="$workspace/home" DISPLAY=:99 XDG_RUNTIME_DIR="$workspace/runtime" \
    FLOEM_FORCE_TINY_SKIA=1 timeout 30s "$binary" \
    "$workspace/workspace" --smoke-exit-ms 1800
