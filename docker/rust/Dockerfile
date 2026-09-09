# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only

FROM rust:1.88.0-bookworm@sha256:af306cfa71d987911a781c37b59d7d67d934f49684058f96cf72079c3626bfe0

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        clang \
        gcc-mingw-w64-x86-64 \
        g++-mingw-w64-x86-64 \
        binutils-mingw-w64-x86-64 \
        fonts-noto-core \
        fonts-noto-cjk \
        imagemagick \
        libegl1-mesa-dev \
        libgtk-3-dev \
        libgtk-4-dev \
        libwayland-dev \
        libx11-xcb-dev \
        libxcb-shape0-dev \
        libxcb-xfixes0-dev \
        libxkbcommon-dev \
        libxkbcommon-x11-dev \
        pkg-config \
        xclip \
        xdotool \
        xvfb \
    && rm -rf /var/lib/apt/lists/*

RUN rustup component add clippy rustfmt \
    && rustup target add aarch64-apple-darwin x86_64-pc-windows-gnu

RUN cargo install --locked --version 0.22.2 cargo-audit \
    && cargo install --locked --version 0.20.2 cargo-deny

WORKDIR /workspace

ENV CARGO_TARGET_DIR=/var/cache/stillus/target
ENV CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc \
    CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++ \
    AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar \
    CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
