#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Deterministic audit for project-owned Rust and direct manifests."""

from __future__ import annotations

import re
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
OWNED_ROOTS = (ROOT / "app", ROOT / "crates", ROOT / "tools")
FORBIDDEN_RUST = {
    "runtime network API": re.compile(
        r"\b(?:std::net|TcpStream|TcpListener|UdpSocket|reqwest|surf|hyper)::?"
    ),
    "process spawning": re.compile(r"\b(?:std::process::Command|Command::new)\b"),
    "database API": re.compile(r"\b(?:rusqlite|sqlx|sqlite|diesel)::?"),
    "browser or JavaScript runtime": re.compile(
        r"\b(?:webview|webkit|javascript|quick_js|deno_core|boa_engine|v8)::?",
        re.IGNORECASE,
    ),
}
FORBIDDEN_DEPENDENCIES = re.compile(
    r"^\s*(?:boa_engine|curl|deno_core|diesel|hyper|quick-js|reqwest|rusqlite|sqlx|sqlite|surf|v8|webkit2gtk|wry)\s*=",
    re.MULTILINE,
)


def fail(message: str) -> None:
    print(f"SOURCE_AUDIT_ERROR {message}", file=sys.stderr)


def main() -> int:
    rust_files = sorted(
        path
        for root in OWNED_ROOTS
        for path in root.rglob("*.rs")
        if path.is_file()
    )
    manifests = sorted(
        path
        for root in OWNED_ROOTS
        for path in root.rglob("Cargo.toml")
        if path.is_file()
    )
    errors = 0
    for path in rust_files:
        text = path.read_text(encoding="utf-8")
        relative = path.relative_to(ROOT)
        if "#![forbid(unsafe_code)]" not in "\n".join(text.splitlines()[:5]):
            fail(f"{relative}: missing crate-level #![forbid(unsafe_code)]")
            errors += 1
        for line_number, line in enumerate(text.splitlines(), start=1):
            if re.search(r"\bunsafe\b", line):
                fail(f"{relative}:{line_number}: project-owned unsafe token")
                errors += 1
        for label, pattern in FORBIDDEN_RUST.items():
            # Explicit update restart is the sole process-launch boundary.
            # The module launches the installed Stillus executable directly.
            if label == "process spawning" and relative == Path("app/stillus/src/restart.rs"):
                continue
            # Only these test-only integration targets may run a loopback
            # fixture server: the RSS one exercises its client, the update one
            # proves that its client refuses every host but GitHub. Production
            # networking still goes exclusively through ureq.
            if (
                label == "runtime network API"
                and relative
                in (
                    Path("crates/stillus-rss/tests/http.rs"),
                    Path("crates/stillus-update/tests/http.rs"),
                )
                and "#![cfg(test)]" in text.splitlines()[:5]
            ):
                continue
            match = pattern.search(text)
            if match:
                line_number = text.count("\n", 0, match.start()) + 1
                fail(f"{relative}:{line_number}: forbidden {label}")
                errors += 1
        if re.search(r"\bureq::", text) and relative.parts[:2] not in (
            ("crates", "stillus-rss"), ("crates", "stillus-ai"), ("crates", "stillus-update")
        ):
            fail(f"{relative}: ureq is restricted to RSS, AI and update transport crates")
            errors += 1
        if relative.parts[:2] == ("crates", "stillus-ai") and re.search(r"\bureq::", text):
            allowed_urls = {
                "https://api.openai.com/v1/models",
                "https://api.anthropic.com/v1/models",
                "https://api.openai.com/v1/responses",
            }
            urls = set(re.findall(r'"(https?://[^"\s]+)"', text))
            if relative == Path("crates/stillus-ai/src/openai/cancellation_transport.rs"):
                # This wrapper only polls an already connected TCP transport;
                # endpoint selection and the standard TLS chain stay in openai.rs.
                valid = not urls and not re.search(r"\b(?:Agent|TcpConnector|RustlsConnector|DefaultResolver)\b", text)
            else:
                valid = relative in (Path("crates/stillus-ai/src/openai.rs"), Path("crates/stillus-ai/src/transport.rs")) and not (urls - allowed_urls) and all(fragment in text for fragment in (".https_only(true)", ".max_redirects(0)", ".proxy(None)"))
            if not valid:
                fail(f"{relative}: AI transport violates fixed-endpoint HTTPS boundary")
                errors += 1
        if re.search(r"\bwebbrowser::", text) and relative.parts[:2] != ("crates", "stillus-rss"):
            fail(f"{relative}: browser handoff is restricted to crates/stillus-rss")
            errors += 1

    # Native views may project state and schedule the owner pump, but must not
    # own worker channels, call executors, or borrow mutable core sessions.
    ui_files = ("main.rs", "ai_settings.rs", "ai_journal_view.rs", "chat_view.rs", "update.rs")
    ui_forbidden = {
        "mutable workspace access": r"\.workspace\s*\.as_mut\s*\(",
        "mutable document access": r"\.document_mut\s*\(",
        "worker channel access": r"\.(?:save|secure|search)_(?:sender|receiver)\b",
        "worker launch": r"application::(?:ai|chat|persistence|security|updates)::start\w*\s*\(",
        "journal storage access": r"\bFileJournal\b",
        "chat storage or provider executor": r"\b(?:ChatStore|ResponsesTransport|AiProviderEngine|SystemCredentials)\b|\.(?:io|tools)_(?:send|receive)\b",
        "settings storage access": r"\b(?:GlobalSettingsStore|UiSettingsStore)\b",
    }
    for name in ui_files:
        path = ROOT / "app/stillus/src" / name
        production = path.read_text(encoding="utf-8").split("#[cfg(test)]\nmod tests", 1)[0]
        for label, pattern in ui_forbidden.items():
            if re.search(pattern, production):
                fail(f"{path.relative_to(ROOT)}: UI crosses application boundary: {label}")
                errors += 1
    for path in sorted((ROOT / "app/stillus/src/application").glob("*.rs")):
        production = path.read_text(encoding="utf-8").split("#[cfg(test)]\nmod tests", 1)[0]
        if path.name in ("api_tests.rs",):
            continue
        if re.search(r"\b(?:floem|floem_reactive)::|\b(?:RwSignal|AppModel|exec_after|create_rw_signal)\b|\btr!", production):
            fail(f"{path.relative_to(ROOT)}: application actions depend on native UI context")
            errors += 1

    for path in manifests:
        text = path.read_text(encoding="utf-8")
        relative = path.relative_to(ROOT)
        match = FORBIDDEN_DEPENDENCIES.search(text)
        if match:
            line_number = text.count("\n", 0, match.start()) + 1
            fail(f"{relative}:{line_number}: forbidden direct dependency")
            errors += 1
        if re.search(r"^\s*ureq\s*=", text, re.MULTILINE) and relative not in (
            Path("crates/stillus-rss/Cargo.toml"),
            Path("crates/stillus-ai/Cargo.toml"),
            Path("crates/stillus-update/Cargo.toml"),
        ):
            fail(f"{relative}: HTTP dependency crosses the RSS/AI/update transport boundary")
            errors += 1
        if re.search(r"^\s*webbrowser\s*=", text, re.MULTILINE) and relative != Path("crates/stillus-rss/Cargo.toml"):
            fail(f"{relative}: browser handoff crosses the RSS boundary")
            errors += 1
        if re.search(r"^\s*keyring\s*=", text, re.MULTILINE) and relative != Path("crates/stillus-platform/Cargo.toml"):
            fail(f"{relative}: credential dependency crosses the platform boundary")
            errors += 1
        if relative.parts[0] == "crates" and re.search(r"^\s*floem\s*=", text, re.MULTILINE):
            fail(f"{relative}: UI dependency crosses a core crate boundary")
            errors += 1

    if errors:
        return 1
    print(
        "SOURCE_AUDIT "
        f"rust_files={len(rust_files)} manifests={len(manifests)} "
        "project_unsafe=0 rss_http_boundary=1 ai_https_boundary=1 update_https_boundary=1 "
        "update_restart_boundary=1 ui_application_boundary=1 database=0 web_js=0"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
