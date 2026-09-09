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
                       "Menu::new(title)", "MenuItem::new(title)", "row.context_menu(menu)",
                       "label(title).tooltip(content)",
                       "fn chat_icon_button() {}"):
            with self.subTest(source=source):
                self.assertTrue(violations(Path("chat_view.rs"), source))

    def test_shared_controls_accept_native_primitives_but_not_app_models(self):
        self.assertFalse(violations(Path("ui/input.rs"), "text_input(value)"))
        for source in ("AppModel", "Controller", "application::settings"):
            self.assertTrue(violations(Path("ui/input.rs"), source))

    def test_screen_can_compose_components_and_specialized_editor(self):
        self.assertFalse(violations(Path("main.rs"),
                                    "TextArea::new(value, palette); form_action_button(kind, title); "
                                    "toolbar_action_button(kind, title); render_editor(model);"))

    def test_actions_must_declare_their_context(self):
        for path in (Path("main.rs"), Path("ui/gallery.rs")):
            for name in ("action_button", "dialog_action_button"):
                with self.subTest(path=path, name=name):
                    self.assertTrue(violations(path, f"{name}(kind, title)"))

    def test_inline_form_cannot_use_icon_only_actions(self):
        for name in ("toolbar_action_button", "icon_button", "compact_icon_button",
                     "enabled_icon_button"):
            self.assertTrue(violations(Path("ui/form.rs"), f"{name}(kind, title)"))
        self.assertFalse(violations(Path("ui/form.rs"), "form_action_button(kind, title)"))

    def test_screen_can_compose_shared_menus_and_forms(self):
        self.assertFalse(violations(Path("main.rs"),
            "menu(entries, palette); context_menu_view(row, palette, entries); "
            "anchored_popover(trigger, open, width, gap, true, content); "
            "toolbar_edit_bar(bar, palette, save); settings_card(title, icon, description, content, palette);"))


if __name__ == "__main__":
    unittest.main()
