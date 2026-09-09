#!/bin/sh
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only

set -eu

RUST_VERSION="1.88.0"
RUSTUP_VERSION="1.29.0"
RUSTUP_SHA256="aeb4105778ca1bd3c6b0e75768f581c656633cd51368fa61289b6a71696ac7e1"

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
host_build_dir=${STILLUS_HOST_BUILD_DIR:-"$project_root/.host-build"}
rustup_home="$host_build_dir/rustup"
cargo_home="$host_build_dir/cargo"
target_dir="$host_build_dir/target"
rustup_init="$host_build_dir/rustup-init"
output="$project_root/dist/Stillus.app"

if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
    echo "build-macos: make build-macos requires an Apple Silicon Mac" >&2
    exit 1
fi
if ! command -v xcrun >/dev/null 2>&1; then
    echo "build-macos: Xcode Command Line Tools are required" >&2
    exit 1
fi
if ! command -v curl >/dev/null 2>&1 || ! command -v shasum >/dev/null 2>&1; then
    echo "build-macos: curl and shasum are required" >&2
    exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
    echo "build-macos: system python3 is required for bundle assembly" >&2
    exit 1
fi
if [ -z "${SOURCE_REVISION:-}" ]; then
    echo "build-macos: SOURCE_REVISION is required" >&2
    exit 1
fi

mkdir -p "$host_build_dir" "$rustup_home" "$cargo_home" "$target_dir"

if [ ! -x "$cargo_home/bin/rustup" ]; then
    rustup_url="https://static.rust-lang.org/rustup/archive/$RUSTUP_VERSION/aarch64-apple-darwin/rustup-init"
    echo "build-macos: downloading pinned rustup $RUSTUP_VERSION"
    curl --fail --location --proto '=https' --tlsv1.2 --output "$rustup_init" "$rustup_url"
    actual_sha256=$(shasum -a 256 "$rustup_init" | awk '{print $1}')
    if [ "$actual_sha256" != "$RUSTUP_SHA256" ]; then
        echo "build-macos: rustup-init checksum mismatch" >&2
        exit 1
    fi
    chmod 700 "$rustup_init"
    RUSTUP_HOME="$rustup_home" CARGO_HOME="$cargo_home" "$rustup_init" \
        -y \
        --no-modify-path \
        --profile minimal \
        --default-host aarch64-apple-darwin \
        --default-toolchain "$RUST_VERSION"
fi

export RUSTUP_HOME="$rustup_home"
export CARGO_HOME="$cargo_home"
export CARGO_TARGET_DIR="$target_dir"
# Use the system Git HTTPS transport: the pinned Cargo/libgit2 transport can
# fail its TLS handshake with GitHub on macOS when fetching Git dependencies.
export CARGO_NET_GIT_FETCH_WITH_CLI=true
export SDKROOT
SDKROOT=$(xcrun --sdk macosx --show-sdk-path)
export MACOSX_DEPLOYMENT_TARGET="11.0"

if ! "$cargo_home/bin/rustup" run "$RUST_VERSION" rustc --version >/dev/null 2>&1; then
    "$cargo_home/bin/rustup" toolchain install "$RUST_VERSION" --profile minimal
fi

cd "$project_root"
echo "build-macos: compiling Stillus with Rust $RUST_VERSION"
"$cargo_home/bin/cargo" "+$RUST_VERSION" build --locked --release -p stillus-app

python3 tools/package_macos.py \
    --binary "$target_dir/release/stillus-app" \
    --output "$output" \
    --source-revision "$SOURCE_REVISION" \
    --adhoc-sign \
    --replace-existing

echo "BUILT_APP path=$output"

python3 tools/package_macos_dmg.py \
    --bundle "$output" \
    --output "$project_root/dist/Stillus.dmg" \
    --replace-existing
