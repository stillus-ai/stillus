// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use crate::*;
use stillus_core::RssPreferences;

pub(crate) fn control(
    model: Rc<RefCell<AppModel>>,
    id: ItemId,
    revision: RwSignal<u64>,
    open: RwSignal<bool>,
    palette: Palette,
) -> AnyView {
    model.borrow_mut().rss_filters_open = Some(open);
    let trigger = toolbar_action_button(
        ToolbarAction::Filters,
        ToolbarSubject::Feed,
        palette,
        move || open.get(),
        move || open.update(|value| *value = !*value),
    );
    anchored_popover(trigger, open, 480.0, 8.0, false, move || {
        form(model.clone(), id.clone(), revision, open, palette)
    })
    .into_any()
}

fn multiline(value: RwSignal<String>, open: RwSignal<bool>, palette: Palette) -> AnyView {
    use floem::style::CursorColor;
    use floem::views::editor::command::CommandExecuted;
    use floem::views::editor::keypress::{default_key_handler, key::KeyInput};
    use floem::views::editor::text::WrapMethod;
    use floem::views::editor::view::EditorViewClass;
    floem::views::text_editor::text_editor_keys(
        value.get_untracked(),
        move |editor, key, modifiers| {
            if matches!(
                &key.key,
                KeyInput::Keyboard(Key::Named(NamedKey::Escape), _)
            ) {
                open.set(false);
                CommandExecuted::Yes
            } else {
                default_key_handler(editor)(key, modifiers)
            }
        },
    )
    .update(move |event| {
        if let Some(editor) = event.editor {
            value.set(editor.rope_text().text.to_string());
        }
    })
    .editor_style(|style| style.hide_gutter(true).wrap_method(WrapMethod::EditorWidth))
    .style(move |style| {
        form_field_style(style, palette, value.get().len() > 16 * 1024)
            .width_full()
            .height(112.0)
            .font_size(14.0)
            .color(palette.ink)
            .background(palette.paper)
            // Floem's TextEditor treats every instance as active. Its inner
            // EditorView owns keyboard focus, so scope the caret to that view.
            .class(EditorViewClass, move |style| {
                style
                    .set(CursorColor, Color::TRANSPARENT)
                    .focus(move |style| style.set(CursorColor, palette.ink))
            })
    })
    .into_any()
}

fn validation_message(error: stillus_core::RssFilterError) -> String {
    use stillus_core::RssFilterError;
    match error {
        RssFilterError::TooLong => tr!(RssFilterTooLong),
        RssFilterError::Invalid { blacklist, line } => tr!(RssFilterInvalid,
            "list" => if blacklist { tr!(RssFilterBlacklist) } else { tr!(RssFilterWhitelist) },
            "line" => line),
        RssFilterError::TooComplex { blacklist } => tr!(RssFilterTooComplex,
            "list" => if blacklist { tr!(RssFilterBlacklist) } else { tr!(RssFilterWhitelist) }),
    }
}

fn form(
    model: Rc<RefCell<AppModel>>,
    id: ItemId,
    revision: RwSignal<u64>,
    open: RwSignal<bool>,
    palette: Palette,
) -> AnyView {
    let preferences = model
        .borrow()
        .workspace
        .as_ref()
        .and_then(|w| w.rss_preferences(&id).ok())
        .unwrap_or_default();
    let expected = preferences.version;
    let blacklist = create_rw_signal(preferences.blacklist);
    let whitelist = create_rw_signal(preferences.whitelist);
    let validation = floem::reactive::create_memo(move |_| {
        RssPreferences {
            blacklist: blacklist.get(),
            whitelist: whitelist.get(),
            ..Default::default()
        }
        .compile()
        .err()
        .map(validation_message)
    });
    let error = create_rw_signal(false);
    let pending = create_rw_signal(None::<u64>);
    let save_model = model.clone();
    let save_id = id.clone();
    create_effect(move |_| {
        revision.get();
        let completed = save_model.borrow().rss_saves.get(save_id.as_str()).copied();
        if let Some(token) = pending.get()
            && let Some((completed, success)) = completed
            && completed == token
        {
            pending.set(None);
            if success {
                open.set(false);
            } else {
                error.set(true);
            }
        }
    });
    let status = label(move || {
        if let Some(message) = validation.get() {
            return message;
        }
        if error.get() {
            return tr!(RssFilterConflict);
        }
        if pending.get().is_some() {
            return tr!(RssFilterBusy);
        }
        String::new()
    })
    .style(move |s| {
        s.font_size(12.0)
            .color(palette.muted)
            .width_full()
            .height(36.0)
    });
    let save_button = |apply: bool| {
        let model = model.clone();
        let id = id.clone();
        action_button(
            move || {
                if apply {
                    tr!(RssFilterSaveApply)
                } else {
                    tr!(Save)
                }
            },
            if apply {
                IconButtonTone::Primary
            } else {
                IconButtonTone::Secondary
            },
            palette,
            move || pending.get().is_none() && validation.get().is_none(),
            move || {
                let value = RssPreferences {
                    blacklist: blacklist.get_untracked(),
                    whitelist: whitelist.get_untracked(),
                    ..Default::default()
                };
                if value.validate().is_err() {
                    error.set(true);
                    return;
                }
                error.set(false);
                let mut model = model.borrow_mut();
                model.rss_save_sequence += 1;
                let token = model.rss_save_sequence;
                let accepted = model.rss_command(rss_service::Command::Preferences(
                    id.clone(),
                    token,
                    expected,
                    value,
                    apply,
                ));
                drop(model);
                if accepted {
                    pending.set(Some(token));
                } else {
                    error.set(true);
                }
            },
        )
    };
    v_stack((
        label(move || tr!(RssFilters)).style(move |s| s.font_size(16.0).color(palette.ink)),
        label(move || tr!(RssFilterHint))
            .style(move |s| s.width_full().font_size(12.0).color(palette.muted)),
        label(move || tr!(RssFilterBlacklist)),
        multiline(blacklist, open, palette),
        label(move || tr!(RssFilterWhitelist)),
        multiline(whitelist, open, palette),
        status,
        h_stack((
            empty().style(|s| s.flex_grow(1.0)),
            text_button(
                msg!(Cancel),
                IconButtonTone::Secondary,
                palette,
                move || open.set(false),
            ),
            save_button(false),
            save_button(true),
        ))
        .style(|s| s.width_full().items_center().gap(8.0)),
    ))
    .style(move |s| {
        s.width(480.0)
            .padding(18.0)
            .gap(8.0)
            .background(palette.paper)
            .color(palette.ink)
            .border(1.0)
            .border_color(palette.divider)
            .border_radius(7.0)
    })
    .on_event(EventListener::KeyDown, move |event| {
        if matches!(event, Event::KeyDown(e) if e.key.logical_key == Key::Named(NamedKey::Escape)) {
            open.set(false);
        }
        EventPropagation::Stop
    })
    .into_any()
}
