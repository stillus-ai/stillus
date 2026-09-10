#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Shared safe CI diagnostics, also shipped with the native Windows test kit."""

import json
from pathlib import Path
import re
import sys


UI_SCENARIOS = frozenset({
    "review",
    "components", "ai", "ai_journal", "chat", "localization", "rss_cards", "rss_keyboard", "rss_filters", "creation", "workspace",
    "compatibility", "categories", "interaction", "lifecycle", "tags", "caret",
    "editor", "context_menu", "sidebar_context", "selection", "persistence", "recovery", "conflict",
    "search", "find", "external_sidebar", "resize", "password_dialog", "password_change", "secure",
    "secure_recovery", "secure_conflict", "secure_integrity", "visual",
    "updates",
})
UI_COMMON_STAGES = frozenset({"startup", "scenario", "capture", "cleanup", "artifacts"})
UI_UPDATES_STAGES = frozenset({
    "restart/settings", "restart/click", "restart/exit", "restart/window",
    "restart/validation",
})
UI_SEARCH_STAGES = frozenset({
    "initial/index", "query/results", "selection/open", "selection/save",
    "external/index", "rebuild/index", "final/validation",
})
UI_PASSWORD_CHANGE_STAGES = frozenset({
    "prepare", "protect", "settings", "empty/validation", "clipboard",
    "confirmation/validation", "rotate", "backup", "restart", "old/rejection",
    "new/unlock", "final/validation",
})
UI_EXCEPTION_NAMES = frozenset({
    "Exception", "AcceptanceFailure", "AssertionError", "ValueError", "TypeError",
    "RuntimeError", "OSError", "FileNotFoundError", "PermissionError", "UnicodeError",
    "UnicodeDecodeError", "CalledProcessError", "TimeoutExpired",
})
UI_DIAGNOSTIC_FILES = frozenset({
    "tools/ui_acceptance.py", "tools/ui_ready.py", "tools/generate_demo_data.py",
    "tools/x11_close_window.py",
})

NATIVE_IO_STAGES = {
    "Lock": {"Create", "Validate", "Open", "Acquire"},
    "Metadata": {"Open", "Inspect", "Permissions", "Hash", "Restore"},
    "Replace": {"Publish"},
    "Cleanup": {"Remove"},
}
NATIVE_IO_KINDS = frozenset({
    "NotFound", "PermissionDenied", "AlreadyExists", "InvalidInput", "InvalidData",
    "Unsupported", "WouldBlock", "Interrupted", "UnexpectedEof", "Other",
})

DELETE_STAGES = frozenset({
    "SelectedTarget", "RecoveryKey", "RecoveryRemove", "RewriteMetadata",
    "RefreshScan", "RefreshFind", "RefreshOpen", "SecureBegin", "SecureFinish",
})
SAVE_STAGES = frozenset({
    "OpenTarget", "Scan", "CreateTemp", "Write", "FileSync", "ConflictCheck",
    "Replace", "SourceRemove", "ParentSync",
})
OPERATION_STAGES = frozenset({
    "Validate", "CreateDirectory", "Write", "FileSync", "Publish", "SourceRemove",
    "DirectorySync",
})
DELETE_ERRORS = frozenset({
    "Workspace", "NoteUnavailable", "UnsavedChanges", "Secure", "Security",
    "PasswordChange", "MasterPasswordRequired", "Clock", "Editor",
    *("Recovery/" + kind for kind in (
        "UnsupportedPlatform", "InvalidPath", "InvalidStore", "InvalidArtifact", "Io")),
    *(prefix + kind for prefix in ("Save/", "Operation/Save/") for kind in (
        "UnsupportedPlatform", "InvalidTarget", "Patch", "Conflict", "PreCommit",
        "PostReplaceSync", "PartialCommit")),
    *("Operation/" + kind for kind in (
        "InvalidName", "InvalidTag", "InvalidWorkspace", "Collision", "Conflict",
        "Failed", "PartialCommit")),
})


def native_line(line):
    if re.fullmatch(
        r"NATIVE_RUNNER stage=(rust|start|window|state|close|close/request|close/wait|verify|complete) "
        r"reason=(none|test/timeout|test/failed|window/timeout|state/timeout|process/early/exit|"
        r"process/exit/code|process/close|process/close/rejected|process/exit/timeout|process/cleanup|state/mismatch|content/changed|runner/error) "
        r"duration_ms=[0-9]{1,10}", line,
    ):
        return line
    retry = re.fullmatch(
        r"NATIVE_REPLACE_RETRY thread=ThreadId\(([1-9][0-9]{0,19})\) "
        r"attempt=([1-4]) delay_ms=(10|20|40|80) os_error=(5|32)", line,
    )
    if retry:
        return line if (int(retry[1]) <= 2**64 - 1
                        and int(retry[3]) == 10 * 2**(int(retry[2]) - 1)) else None
    marker_retry = re.fullmatch(
        r"NATIVE_DIRECTORY_SYNC_RETRY thread=ThreadId\(([1-9][0-9]{0,19})\) "
        r"stage=(Publish|Remove) attempt=([1-7]) delay_ms=(0|10|20|40|80|160|320) "
        r"os_error=32 ownership=(Owned|Missing|Changed|Unavailable)", line,
    )
    if marker_retry:
        thread, _, attempt, delay, _ = marker_retry.groups()
        valid_delay = int(delay) == 0 or (int(attempt) <= 6
                                        and int(delay) == 10 * 2**(int(attempt) - 1))
        return line if int(thread) <= 2**64 - 1 and valid_delay else None
    post_replace = re.fullmatch(
        r"NATIVE_(POST_REPLACE|DIRECTORY_SYNC) thread=ThreadId\(([1-9][0-9]{0,19})\) "
        r"stage=([A-Za-z]+) kind=([A-Za-z]+) os_error=(-?[0-9]{1,10})", line,
    )
    if post_replace:
        operation, thread, stage, kind, os_error = post_replace.groups()
        if int(thread) > 2**64 - 1 or not -(2**31) <= int(os_error) < 2**31:
            return None
        if operation == "POST_REPLACE" and stage == "CommittedIdentity":
            return line if kind == "IdentityMismatch" and os_error == "0" else None
        stages = ({"CommittedInspect", "ParentCheckpoint", "ParentSync", "ReplaceReportedFailure"}
                  if operation == "POST_REPLACE" else
                  {"Validate", "Create", "FileSync", "Publish", "Remove", "Cleanup", "Exhausted"})
        return line if stage in stages and kind in NATIVE_IO_KINDS else None
    deletion = re.fullmatch(
        r"NATIVE_DELETE stage=([A-Za-z]+) outcome=(Success|Failed) "
        r"error=([A-Za-z/]+) error_stage=([A-Za-z]+)", line,
    )
    if deletion:
        stage, outcome, error, error_stage = deletion.groups()
        if stage not in DELETE_STAGES:
            return None
        if outcome == "Success":
            return line if error == error_stage == "None" else None
        if error not in DELETE_ERRORS:
            return None
        allowed = (SAVE_STAGES if error in {"Save/PreCommit", "Operation/Save/PreCommit"}
                   else OPERATION_STAGES if error == "Operation/Failed" else {"None"})
        return line if error_stage in allowed else None
    match = re.fullmatch(
        r"NATIVE_IO operation=([A-Za-z]+) stage=([A-Za-z]+) kind=([A-Za-z]+) os_error=(-?[0-9]{1,10})",
        line,
    )
    if match:
        return line if (match[2] in NATIVE_IO_STAGES.get(match[1], set())
                        and match[3] in NATIVE_IO_KINDS
                        and -(2**31) <= int(match[4]) < 2**31) else None
    patterns = (
        r"NATIVE_LIFECYCLE stage=(WindowClosed|WindowSettingsFlushed|WindowSettingsFailed|EventLoopExited|FinalSettingsFlushed|FinalSettingsFailed|ShutdownComplete)",
        r"NATIVE_WINDOW scenario=(startup|external) process=(unknown|running|exited) window=(unknown|present|absent) responding=(unknown|true|false) close_accepted=(true|false) close_attempts=[0-9]{1,3}",
        r"NATIVE_DELETE_TEST round=([1-9]|[12][0-9]|3[0-2]) deletion=[12] phase=(Begin|End)",
        r"NATIVE_VERSION site=(MetadataOpened|OpenVersioned|RewriteTarget|RewriteOpened|RewriteBeforeReplace|RewriteRetryTarget) identity_equal=(true|false|Unavailable) size_equal=(true|false) modified_equal=(true|false) changed_equal=(true|false|Unavailable) digest_equal=(true|false|Unavailable)",
        r"NATIVE_SAVE stage=(OpenTarget|Scan|CreateTemp|Write|FileSync|ConflictCheck|Replace|SourceRemove|ParentSync) outcome=PreCommit",
        r"NATIVE_OPERATION stage=(Validate|CreateDirectory|Write|FileSync|Publish|SourceRemove|DirectorySync) outcome=Failed",
        r"NATIVE_CLEANUP outcome=(Removed|Absent|Failed)",
        r"NATIVE_TEMP kind=(Regular|Secure) count=[0-9]{1,10}",
        r"NATIVE_RESULT operation=ExternalSave outcome=(Success|Conflict|PreCommit|PostReplaceSync|PartialCommit|InvalidTarget|Patch|UnsupportedPlatform)",
        r"NATIVE_ASSERT operation=(NoteOrder|DeleteNote|ConcurrentLock) success=(true|false)",
        r"NATIVE_PATH operation=(WorkspaceNote|ExternalSelection) requested_verbatim=(true|false) stored_verbatim=(true|false) lexical_equal=(true|false) canonical_equal=(true|false)",
    )
    return line if any(re.fullmatch(pattern, line) for pattern in patterns) else None


def ui_diagnostic_context_valid(scenario, stage):
    return scenario in UI_SCENARIOS and (
        stage in UI_COMMON_STAGES
        or (scenario == "password_change" and stage in UI_PASSWORD_CHANGE_STAGES)
        or (scenario == "search" and stage in UI_SEARCH_STAGES)
        or (scenario == "updates" and stage in UI_UPDATES_STAGES)
    )


def safe_line(line):
    """Never copy arbitrary test output, panic payloads, app logs or document paths."""
    line = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", line).strip()
    # Reject malformed native records before any legacy substring extractors.
    if line.startswith("NATIVE_") and not line.startswith("NATIVE_EXTERNAL_SMOKE_OK"):
        return native_line(line)
    # Reject malformed diagnostics before the permissive legacy extractors.
    if line.startswith("UI_ACCEPTANCE_DIAGNOSTIC"):
        match = re.fullmatch(
            r"UI_ACCEPTANCE_DIAGNOSTIC scenario=([a-z_]+) stage=([a-z/]+) "
            r"exception=([A-Za-z]+)(?: location=(tools/[a-z_]+\.py):([1-9][0-9]*))?",
            line,
        )
        if (match and ui_diagnostic_context_valid(match[1], match[2])
                and match[3] in UI_EXCEPTION_NAMES
                and (match[4] is None or match[4] in UI_DIAGNOSTIC_FILES)):
            return line
        return None
    if line in {"Panic payload omitted for privacy.", "Rust panic: payload omitted"}:
        return "Rust panic: payload omitted"
    dependency = re.fullmatch(
        r"(?:Location: (?:[A-Za-z]:)?[/\\][^\r\n]*[/\\]registry[/\\]src[/\\][a-zA-Z0-9_-]+[/\\]|Rust dependency location: )"
        r"([a-zA-Z0-9_-]+-[0-9][a-zA-Z0-9.+-]*)[/\\](src(?:[/\\][a-zA-Z0-9_-]+)+\.rs):([1-9][0-9]{0,8}):([1-9][0-9]{0,8})",
        line,
    )
    if dependency:
        crate, source, row, column = dependency.groups()
        source = source.replace("\\", "/")
        return f"Rust dependency location: {crate}/{source}:{row}:{column}"
    if re.fullmatch(r"test [a-zA-Z0-9_:]+ \.\.\. (ok|FAILED|ignored)", line):
        return line
    if re.fullmatch(r"test result: (ok|FAILED)\. [0-9a-z ;.]+", line):
        return line
    if re.fullmatch(r"WINDOWS_ACL_MISMATCH expected_count=[0-9]{1,5} actual_count=[0-9]{1,5} index=[0-9]{1,5} expected_flags=[0-9]{1,3} actual_flags=[0-9]{1,3}", line):
        return line
    if re.fullmatch(r"Rust failure: (?:stage=[A-Za-z]+(?: os_error=[0-9]+)?|os_error=[0-9]+)", line):
        return line
    stage = re.search(r"PreCommit \{ stage: (OpenTarget|Scan|CreateTemp|Write|FileSync|ConflictCheck|Replace|SourceRemove|ParentSync)\b", line)
    os_error = re.search(r"(?:\(os error |Os \{ code: )([0-9]{1,5})(?:\)|,)", line)
    if stage or os_error:
        parts = ([f"stage={stage[1]}"] if stage else []) + ([f"os_error={os_error[1]}"] if os_error else [])
        return "Rust failure: " + " ".join(parts)
    match = re.fullmatch(r"(FAIL|ERROR): (test_[a-zA-Z0-9_]+) \(([a-zA-Z0-9_.]+)\)", line)
    if match:
        return f"Python test {match[1]}: {match[3]}"
    match = re.fullmatch(r'File "(?:[^"\n]*/)?(tools/[a-zA-Z0-9_]+\.py)", line ([0-9]+), in [a-zA-Z0-9_<>]+', line)
    if match:
        return f"Python diagnostic location: {match[1]}:{match[2]}"
    match = re.match(r"(?:subprocess\.)?(AssertionError|ValueError|OSError|FileNotFoundError|PermissionError|CalledProcessError|TimeoutExpired)(?::|$)", line)
    if match:
        return f"Python exception: {match[1]} (details omitted)"
    match = re.match(r"UI_ACCEPTANCE_(PASS|FAIL) scenario=([a-z_]+)(?:\s|:|$)", line)
    if match:
        return f"UI_ACCEPTANCE_{match[1]} scenario={match[2]}"
    match = re.match(r"(DESKTOP_SMOKE) (external|crash)=passed$", line)
    if match:
        return match[0]
    match = re.match(r"NATIVE_EXTERNAL_SMOKE_OK cold_start=(True|False) ordered_files=([0-9]+)", line)
    if match:
        return match[0]
    if re.fullmatch(r"SOURCE_AUDIT [a-z0-9_= ]+", line):
        return line
    if line in ("bans ok, licenses ok, sources ok", "bans ok", "licenses ok", "sources ok"):
        return line
    if line in (
        "build-macos: make build-macos requires an Apple Silicon Mac",
        "build-macos: Xcode Command Line Tools are required",
        "build-macos: curl and shasum are required",
        "build-macos: system python3 is required for bundle assembly",
        "build-macos: SOURCE_REVISION is required",
        "build-macos: rustup-init checksum mismatch",
    ):
        return line
    match = re.match(r"warning: ([0-9]+) allowed warnings found", line)
    if match:
        return match[0]
    match = re.match(r"error(?:\[(E[0-9]+)\])?:", line)
    if match:
        return f"Compiler/tool error: {match[1] or 'unspecified'} (details omitted)"
    match = re.search(r"((?:app|crates|tools)/[a-zA-Z0-9_./-]+\.rs):([0-9]+):([0-9]+)", line)
    if match:
        return f"Rust diagnostic location: {match[0]}"
    match = re.match(r"(?:Compiling|Checking) ([a-zA-Z0-9_-]+) (v[0-9][a-zA-Z0-9.+-]*)", line)
    if match:
        return match[0]
    match = re.match(r"make(?:\[[0-9]+\])?: \*\*\* \[([a-zA-Z0-9_./: -]+)\] Error ([0-9]+)", line)
    if match:
        return match[0]
    return None


def rust_test_report(lines):
    failed = []
    diagnostics = []
    for line in lines:
        cleaned = safe_line(line)
        if cleaned is None:
            continue
        match = re.fullmatch(r"test ([a-zA-Z0-9_:]+) \.\.\. FAILED", cleaned)
        if match and match[1] not in failed:
            failed.append(match[1])
        diagnostics.append(cleaned)
    return {"failedTests": failed, "diagnostics": diagnostics}


if __name__ == "__main__":
    def lines():
        for name in sys.argv[1:]:
            with Path(name).open(encoding="utf-8-sig", errors="replace") as source:
                yield from source
    print(json.dumps(rust_test_report(lines())))
