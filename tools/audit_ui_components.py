#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Keep native form controls behind Stillus's shared UI boundary."""
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "app/stillus/src"


def violations(path: Path, source: str) -> list[str]:
    if path.parts[0] == "ui":
        rules = {
            "component depends on application state": r"\b(?:AppModel|Controller|GlobalApplication)\b|\bapplication::",
        }
    else:
        rules = {
            "native text field outside ui": r"\bTextInput::new\b|\btext_input\s*\(",
            "native multiline editor outside ui": r"\btext_editor(?:_keys)?\s*\(|\bTextDocument::new\b|\beditor_container_view\s*\(",
            "native select outside ui": r"\bDropdown::(?:new|custom)\b",
            "native button outside ui": r"\b(?:button|reliable_button)\s*\(",
            "local button implementation": r"\bfn\s+\w*(?:icon_button|text_button|action_button|dialog_button)\s*\(",
            "untitled icon surface": r"\bselectable_row\s*\(\s*svg\s*\(",
        }
    return [f"{path}: {label}" for label, pattern in rules.items() if re.search(pattern, source)]


def main() -> int:
    errors = [error for path in sorted(SOURCE.rglob("*.rs"))
              if "application" not in path.relative_to(SOURCE).parts
              for error in violations(path.relative_to(SOURCE), path.read_text())]
    for error in errors:
        print(f"UI_COMPONENT_AUDIT_ERROR {error}", file=sys.stderr)
    if not errors:
        print("UI_COMPONENT_AUDIT_OK")
    return int(bool(errors))


if __name__ == "__main__":
    raise SystemExit(main())
