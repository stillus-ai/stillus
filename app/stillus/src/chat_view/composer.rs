// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]
//! A focused native editor with a decorative placeholder outside the document.
use super::*;
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

pub(super) fn view(
    draft: RwSignal<String>,
    disabled: RwSignal<bool>,
    palette: Palette,
    submit: Rc<dyn Fn()>,
    update: impl Fn(String) + 'static,
) -> AnyView {
    let scope = Scope::current().create_child();
    let focused = create_rw_signal(false);
    let window_active = create_rw_signal(true);
    let active = floem::reactive::create_memo(move |_| {
        focused.get() && window_active.get() && !disabled.get()
    });
    let doc = Rc::new(TextDocument::new(scope, draft.get_untracked()));
    doc.keep_indent.set(false);
    let mut styling = SimpleStyling::new();
    styling.set_font_size(14);
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
        if event.cmd.str() == "insert_new_line" && !event.mods.shift() {
            submit();
            CommandExecuted::Yes
        } else {
            CommandExecuted::No
        }
    });
    doc.add_on_update(move |event| {
        if let Some(editor) = event.editor {
            update(editor.text().to_string());
        }
    });
    let editor = create_rw_signal(editor);
    let content = editor_container_view(editor, move |_| active.get(), default_key_handler(editor));
    let placeholder = label(|| tr!(ChatPlaceholder))
        .style(move |s| {
            s.absolute()
                .inset_left(2.0)
                .inset_top(0.0)
                .font_size(14.0)
                .color(palette.muted)
                .apply_if(!draft.get().is_empty(), |s| s.hide())
        })
        .pointer_events(|| false);
    let inner =
        stack((content, placeholder)).style(|s| s.size_full().min_width(0.0).min_height(0.0));
    stack((inner,))
        .disabled(move || disabled.get())
        .on_event_cont(EventListener::WindowGotFocus, move |_| {
            window_active.set(true)
        })
        .on_event_cont(EventListener::WindowLostFocus, move |_| {
            window_active.set(false)
        })
        .style(move |s| {
            s.width_full()
                .min_width(0.0)
                .height(116.0)
                .flex_shrink(0.0)
                .padding(8.0)
                .border(1.0)
                .border_color(palette.divider)
                .border_radius(8.0)
                .background(palette.paper)
                .color(palette.ink)
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
