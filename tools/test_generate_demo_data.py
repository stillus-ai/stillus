#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only

"""Regression tests for the generated, untracked demo workspace."""

from __future__ import annotations

from pathlib import Path
import tempfile
import unittest

from generate_demo_data import DEMO_NOTES, GENERATED_MARKER, generate_demo_workspace


class GenerateDemoDataTests(unittest.TestCase):
    def test_generation_is_deterministic_and_cleans_generated_state(self) -> None:
        with tempfile.TemporaryDirectory(prefix="stillus-demo-data-") as directory:
            workspace = Path(directory) / "demo-workspace"
            generate_demo_workspace(workspace)
            first = {
                path.name: path.read_bytes()
                for path in sorted((workspace / "notes").glob("*.md"))
            }

            (workspace / "notes" / "Stale.md").write_text("stale", encoding="utf-8")
            recovery = workspace / ".stillus" / "recovery"
            recovery.mkdir(parents=True)
            (recovery / "stale.nrrec").write_text("stale", encoding="utf-8")
            generate_demo_workspace(workspace)

            second = {
                path.name: path.read_bytes()
                for path in sorted((workspace / "notes").glob("*.md"))
            }
            self.assertEqual(first, second)
            self.assertEqual(set(second), set(DEMO_NOTES))
            self.assertFalse((workspace / ".stillus").exists())
            self.assertTrue((workspace / GENERATED_MARKER).is_file())

    def test_unmarked_nonempty_temporary_workspace_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory(prefix="stillus-demo-data-") as directory:
            workspace = Path(directory) / "demo-workspace"
            notes = workspace / "notes"
            notes.mkdir(parents=True)
            note = notes / "Personal.md"
            note.write_text("do not replace", encoding="utf-8")

            with self.assertRaisesRegex(ValueError, "unmarked temporary workspace"):
                generate_demo_workspace(workspace)
            self.assertEqual(note.read_text(encoding="utf-8"), "do not replace")

    def test_destination_outside_demo_and_temporary_roots_is_rejected(self) -> None:
        workspace = Path("/workspace/.stillus-generator-must-not-write")
        with self.assertRaisesRegex(ValueError, "default workspace or under /tmp"):
            generate_demo_workspace(workspace)
        self.assertFalse(workspace.exists())

    def test_workspace_symlink_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory(prefix="stillus-demo-data-") as directory:
            root = Path(directory)
            real_workspace = root / "real"
            real_workspace.mkdir()
            linked_workspace = root / "demo-workspace"
            linked_workspace.symlink_to(real_workspace, target_is_directory=True)

            with self.assertRaisesRegex(ValueError, "must not contain a symlink"):
                generate_demo_workspace(linked_workspace)

    def test_workspace_with_symlinked_parent_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory(prefix="stillus-demo-data-") as directory:
            root = Path(directory)
            real_parent = root / "real"
            real_parent.mkdir()
            linked_parent = root / "linked"
            linked_parent.symlink_to(real_parent, target_is_directory=True)

            with self.assertRaisesRegex(ValueError, "must not contain a symlink"):
                generate_demo_workspace(linked_parent / "demo-workspace")

    def test_note_symlink_is_rejected_before_generated_state_is_removed(self) -> None:
        with tempfile.TemporaryDirectory(prefix="stillus-demo-data-") as directory:
            workspace = Path(directory) / "demo-workspace"
            generate_demo_workspace(workspace)
            recovery = workspace / ".stillus" / "recovery"
            recovery.mkdir(parents=True)
            artifact = recovery / "keep.nrrec"
            artifact.write_text("keep", encoding="utf-8")
            external = Path(directory) / "external.md"
            external.write_text("external", encoding="utf-8")
            (workspace / "notes" / "Linked.md").symlink_to(external)

            with self.assertRaisesRegex(ValueError, "note must not be a symlink"):
                generate_demo_workspace(workspace)
            self.assertEqual(artifact.read_text(encoding="utf-8"), "keep")
            self.assertEqual(external.read_text(encoding="utf-8"), "external")

    def test_non_regular_note_is_rejected_before_generated_state_is_removed(self) -> None:
        with tempfile.TemporaryDirectory(prefix="stillus-demo-data-") as directory:
            workspace = Path(directory) / "demo-workspace"
            generate_demo_workspace(workspace)
            recovery = workspace / ".stillus" / "recovery"
            recovery.mkdir(parents=True)
            artifact = recovery / "keep.nrrec"
            artifact.write_text("keep", encoding="utf-8")
            (workspace / "notes" / "Directory.md").mkdir()

            with self.assertRaisesRegex(ValueError, "not a regular file"):
                generate_demo_workspace(workspace)
            self.assertEqual(artifact.read_text(encoding="utf-8"), "keep")


if __name__ == "__main__":
    unittest.main()
