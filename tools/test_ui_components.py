#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Regression checks for the component architecture gate."""
from pathlib import Path
import unittest
from audit_ui_components import violations


class BoundaryTests(unittest.TestCase):
    def test_screen_cannot_construct_native_form_controls(self):
        for source in ("text_input(value)", "Dropdown::custom(value)",
                       "text_editor_keys(value)", "TextDocument::new(scope, text)",
                       "button(label)", "floem::views::button(label)",
                       "reliable_button(label, action)", "selectable_row(svg(icon), action)",
                       "fn chat_icon_button() {}"):
            with self.subTest(source=source):
                self.assertTrue(violations(Path("chat_view.rs"), source))

    def test_shared_controls_accept_native_primitives_but_not_app_models(self):
        self.assertFalse(violations(Path("ui/input.rs"), "text_input(value)"))
        for source in ("AppModel", "Controller", "application::settings"):
            self.assertTrue(violations(Path("ui/input.rs"), source))

    def test_screen_can_compose_components_and_specialized_editor(self):
        self.assertFalse(violations(Path("main.rs"),
                                    "TextArea::new(value, palette); action_button(kind, title); render_editor(model);"))


if __name__ == "__main__":
    unittest.main()
