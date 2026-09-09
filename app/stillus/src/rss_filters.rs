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
    let trigger = toolbar_control(
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

fn field_error(value: &str, blacklist: bool) -> Option<stillus_core::RssFilterError> {
    RssPreferences {
        blacklist: if blacklist {
            value.to_owned()
        } else {
            String::new()
        },
        whitelist: if blacklist {
            String::new()
        } else {
            value.to_owned()
        },
        ..Default::default()
    }
    .compile()
    .err()
}

fn multiline(
    value: RwSignal<String>,
    open: RwSignal<bool>,
    blacklist: bool,
    palette: Palette,
) -> AnyView {
    let error = floem::reactive::create_memo(move |_| field_error(&value.get(), blacklist));
    let line = create_rw_signal(None);
    v_stack((
        TextArea::new(value, palette)
            .visible(move || open.get())
            .invalid(move || error.get().is_some())
            .focus_line(line)
            .on_escape(move || open.set(false))
            .build(move |_| {}),
        selectable_row(
            label(move || error.get().map(validation_message).unwrap_or_default()).style(
                move |s| {
                    s.width_full()
                        .font_size(crate::ui::FONT_CAPTION)
                        .color(palette.danger)
                },
            ),
            move || {
                if let Some(stillus_core::RssFilterError::Invalid { line: number, .. }) =
                    error.get_untracked()
                {
                    line.set(None);
                    line.set(Some(number));
                }
            },
        )
        .style(move |s| {
            s.width_full()
                .min_width(0.0)
                .cursor(CursorStyle::Pointer)
                .focus_visible(|s| s.border(1.0).border_color(palette.danger))
                .apply_if(error.get().is_none(), |s| s.hide())
        }),
    ))
    .style(|s| s.width_full().min_width(0.0).gap(4.0))
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

pub(crate) fn form(
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
        if error.get() {
            return tr!(RssFilterConflict);
        }
        if pending.get().is_some() {
            return tr!(RssFilterBusy);
        }
        String::new()
    })
    .style(move |s| {
        s.font_size(crate::ui::FONT_CAPTION as f32)
            .color(if error.get() {
                palette.danger
            } else {
                palette.muted
            })
            .width_full()
            .apply_if(!error.get() && pending.get().is_none(), |s| s.hide())
    });
    let save_button = |apply: bool| {
        let model = model.clone();
        let id = id.clone();
        form_action_button(
            if apply {
                ButtonAction::Custom(ButtonAction::Save.icon())
            } else {
                ButtonAction::Save
            },
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
        label(move || tr!(RssFilters)).style(move |s| {
            s.font_size(crate::ui::FONT_SECTION as f32)
                .font_family(crate::ui::HEADING_FONT_FAMILY.to_owned())
                .font_weight(floem::text::Weight::SEMIBOLD)
                .color(palette.ink)
        }),
        label(move || tr!(RssFilterHint)).style(move |s| {
            s.width_full()
                .font_size(crate::ui::FONT_CAPTION as f32)
                .color(palette.muted)
        }),
        label(move || tr!(RssFilterBlacklist)),
        multiline(blacklist, open, true, palette),
        label(move || tr!(RssFilterWhitelist)),
        multiline(whitelist, open, false, palette),
        status,
        actions((
            dialog_button(
                ButtonAction::Cancel,
                msg!(Cancel),
                IconButtonTone::Secondary,
                palette,
                move || open.set(false),
            ),
            save_button(false),
            save_button(true),
        ))
        .style(|s| s.width_full().justify_end().margin_top(8.0)),
    ))
    .style(move |s| {
        s.width(480.0)
            .padding(16.0)
            .gap(8.0)
            .background(palette.paper)
            .color(palette.ink)
            .border(1.0)
            .border_color(palette.divider)
            .border_radius(8.0)
    })
    .on_event(EventListener::KeyDown, move |event| {
        if matches!(event, Event::KeyDown(e) if e.key.logical_key == Key::Named(NamedKey::Escape)) {
            open.set(false);
        }
        EventPropagation::Stop
    })
    .into_any()
}

#[cfg(test)]
mod tests {
    #[test]
    fn validation_identifies_the_field_and_original_line() {
        for blacklist in [true, false] {
            assert_eq!(
                super::field_error("valid\n\n[", blacklist),
                Some(stillus_core::RssFilterError::Invalid { blacklist, line: 3 })
            );
            assert_eq!(super::field_error("valid\n.*", blacklist), None);
        }
    }
}
