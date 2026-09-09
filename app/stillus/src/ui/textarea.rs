// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]
//! A focused native editor with a decorative placeholder outside the document.
use super::*;
use floem::views::editor::{
    core::{editor::EditType, selection::Selection},
    text::Document,
};
use floem::{
    reactive::{Scope, create_updater},
    views::editor::{
        CurrentLineColor, Editor, ScrollBeyondLastLine, SelectionColor, WrapProp,
        command::CommandExecuted,
        gutter::GutterClass,
        keypress::default_key_handler,
        text::{SimpleStyling, WrapMethod},
        text_document::TextDocument,
        view::{EditorViewClass, editor_container_view},
    },
};

/// A form editor. Persistent storage and submission remain with the caller.
pub(crate) struct TextArea {
    value: RwSignal<String>,
    palette: Palette,
    placeholder: Option<i18n::Key>,
    enabled: Rc<dyn Fn() -> bool>,
    visible: Rc<dyn Fn() -> bool>,
    invalid: Rc<dyn Fn() -> bool>,
    height: f64,
    submit: Option<Rc<dyn Fn()>>,
    escape: Option<Rc<dyn Fn()>>,
    focus_line: Option<RwSignal<Option<usize>>>,
}
impl TextArea {
    pub(crate) fn new(value: RwSignal<String>, palette: Palette) -> Self {
        Self {
            value,
            palette,
            placeholder: None,
            enabled: Rc::new(|| true),
            visible: Rc::new(|| true),
            invalid: Rc::new(|| false),
            height: 112.0,
            submit: None,
            escape: None,
            focus_line: None,
        }
    }
    pub(crate) fn placeholder(mut self, key: i18n::Key) -> Self {
        self.placeholder = Some(key);
        self
    }
    pub(crate) fn enabled(mut self, enabled: impl Fn() -> bool + 'static) -> Self {
        self.enabled = Rc::new(enabled);
        self
    }
    pub(crate) fn visible(mut self, visible: impl Fn() -> bool + 'static) -> Self {
        self.visible = Rc::new(visible);
        self
    }
    pub(crate) fn invalid(mut self, invalid: impl Fn() -> bool + 'static) -> Self {
        self.invalid = Rc::new(invalid);
        self
    }
    pub(crate) fn height(mut self, height: f64) -> Self {
        self.height = height;
        self
    }
    /// A one-based line requested by a form validation message.
    pub(crate) fn focus_line(mut self, line: RwSignal<Option<usize>>) -> Self {
        self.focus_line = Some(line);
        self
    }
    /// Enter submits; Shift+Enter always inserts a newline. Without this callback Enter inserts.
    pub(crate) fn on_submit(mut self, submit: impl Fn() + 'static) -> Self {
        self.submit = Some(Rc::new(submit));
        self
    }
    pub(crate) fn on_escape(mut self, escape: impl Fn() + 'static) -> Self {
        self.escape = Some(Rc::new(escape));
        self
    }
    pub(crate) fn build(self, update: impl Fn(String) + 'static) -> AnyView {
        let Self {
            value: draft,
            palette,
            placeholder,
            enabled,
            visible,
            invalid,
            height,
            submit,
            escape,
            focus_line,
        } = self;
        let enabled = floem::reactive::create_memo(move |_| enabled());
        let visible = floem::reactive::create_memo(move |_| visible());
        let scope = Scope::current().create_child();
        let focused = create_rw_signal(false);
        let window_active = create_rw_signal(true);
        let active = floem::reactive::create_memo(move |_| {
            caret_active(
                focused.get(),
                window_active.get(),
                enabled.get(),
                visible.get(),
            )
        });
        let doc = Rc::new(TextDocument::new(scope, draft.get_untracked()));
        doc.keep_indent.set(false);
        let mut styling = SimpleStyling::new();
        styling.set_font_size(crate::ui::FONT_BODY as usize);
        styling.set_font_family(vec![floem::text::FamilyOwned::Name(
            crate::ui::UI_FONT_FAMILY.to_owned(),
        )]);
        let mut editor = Editor::new(scope, doc.clone(), Rc::new(styling), false);
        editor.cursor_info.should_blink = Rc::new(move || active.get_untracked());
        let gain = editor.editor_view_focused;
        let lose = editor.editor_view_focus_lost;
        create_updater(move || gain.track(), move |_| focused.set(true));
        create_updater(move || lose.track(), move |_| focused.set(false));
        let cursor = editor.cursor_info.clone();
        let ime_editor = editor.clone();
        create_effect(move |_| {
            let is_active = active.get();
            cursor.reset();
            if !is_active {
                cursor.hidden.set(true);
                ime_editor.clear_preedit();
                if ime_editor.ime_allowed.get_untracked() {
                    ime_editor.ime_allowed.set(false);
                    floem::action::set_ime_allowed(false);
                }
            }
        });
        doc.add_pre_command(editor.id(), move |event| {
            if !active.get_untracked() {
                return CommandExecuted::Yes;
            }
            if event.cmd.str() == "insert_new_line"
                && !event.mods.shift()
                && let Some(submit) = &submit
            {
                submit();
                CommandExecuted::Yes
            } else {
                CommandExecuted::No
            }
        });
        doc.add_on_update(move |event| {
            if let Some(editor) = event.editor {
                let value = editor.text().to_string();
                draft.set(value.clone());
                update(value);
            }
        });
        let external_editor = editor.clone();
        create_effect(move |_| {
            let value = draft.get();
            let current = doc.text();
            if current.to_string() != value {
                doc.edit_single(Selection::region(0, current.len()), &value, EditType::Other);
                external_editor.cursor.update(|cursor| {
                    let mut offset = cursor.offset().min(value.len());
                    while !value.is_char_boundary(offset) {
                        offset -= 1;
                    }
                    cursor.set_insert(Selection::caret(offset));
                });
                external_editor.clear_preedit();
            }
        });
        let editor = create_rw_signal(editor);
        if let Some(line) = focus_line {
            create_effect(move |_| {
                if let Some(line_number) = line.get() {
                    let editor = editor.get_untracked();
                    let text = editor.text();
                    let offset = text.offset_of_line(
                        line_number
                            .saturating_sub(1)
                            .min(text.line_of_offset(text.len())),
                    );
                    editor
                        .cursor
                        .update(|cursor| cursor.set_insert(Selection::caret(offset)));
                    if let Some(id) = editor.editor_view_id.get_untracked() {
                        id.request_focus();
                    }
                }
            });
        }
        let content = editor_container_view(
            editor,
            move |_| active.get(),
            move |key, mods| {
                use floem::views::editor::keypress::key::KeyInput;
                if matches!(
                    &key.key,
                    KeyInput::Keyboard(Key::Named(NamedKey::Escape), _)
                ) && super::popover_close_top_on_escape()
                {
                    return CommandExecuted::Yes;
                }
                if matches!(
                    &key.key,
                    KeyInput::Keyboard(Key::Named(NamedKey::Escape), _)
                ) && let Some(escape) = &escape
                {
                    escape();
                    CommandExecuted::Yes
                } else {
                    default_key_handler(editor)(key, mods)
                }
            },
        );
        let placeholder = label(move || placeholder.map(|key| key.to_string()).unwrap_or_default())
            .style(move |s| {
                s.absolute()
                    .inset_left(2.0)
                    .inset_right(2.0)
                    .inset_top(0.0)
                    .min_width(0.0)
                    .text_ellipsis()
                    .apply_if(i18n::current().is_rtl(), |s| s.justify_end())
                    .font_size(crate::ui::FONT_BODY as f32)
                    .color(palette.ink3)
                    .apply_if(!draft.get().is_empty(), |s| s.hide())
            })
            .pointer_events(|| false);
        let inner =
            stack((content, placeholder)).style(|s| s.size_full().min_width(0.0).min_height(0.0));
        stack((inner,))
            .disabled(move || !enabled.get())
            .on_event_cont(EventListener::WindowGotFocus, move |_| {
                window_active.set(true)
            })
            .on_event_cont(EventListener::WindowLostFocus, move |_| {
                window_active.set(false)
            })
            .style(move |s| {
                s.apply_if(!visible.get(), |s| s.hide())
                    .width_full()
                    .min_width(0.0)
                    .height(height)
                    .flex_shrink(0.0)
                    .padding(8.0)
                    .border(1.0)
                    .border_color(if invalid() {
                        palette.danger
                    } else if active.get() {
                        palette.accent
                    } else {
                        palette.divider
                    })
                    .border_radius(8.0)
                    .background(if enabled.get() {
                        palette.paper
                    } else {
                        palette.canvas
                    })
                    .color(if enabled.get() {
                        palette.ink
                    } else {
                        palette.muted
                    })
                    .cursor(if enabled.get() {
                        CursorStyle::Text
                    } else {
                        CursorStyle::Default
                    })
                    .class(GutterClass, |s| s.hide())
                    .class(EditorViewClass, |s| {
                        s.set(ScrollBeyondLastLine, false)
                            .set(CurrentLineColor, Color::TRANSPARENT)
                            .set(floem::style::CursorColor, palette.accent)
                            .set(SelectionColor, palette.accent_soft)
                            .set(WrapProp, WrapMethod::EditorWidth)
                    })
            })
            .into_any()
    }
}

fn caret_active(focused: bool, window_active: bool, enabled: bool, visible: bool) -> bool {
    focused && window_active && enabled && visible
}
#[cfg(test)]
mod tests {
    #[test]
    fn caret_requires_focus_active_window_enabled_and_visible_field() {
        for mask in 0..16 {
            assert_eq!(
                super::caret_active(mask & 1 != 0, mask & 2 != 0, mask & 4 != 0, mask & 8 != 0),
                mask == 15
            );
        }
    }
}
