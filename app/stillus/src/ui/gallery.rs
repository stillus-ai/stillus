// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]
//! Disposable component fixture, compiled only with test-utils.
use super::*;

pub(crate) fn view() -> impl IntoView {
    let palette = Palette::new();
    let first = create_rw_signal(String::new());
    let second = create_rw_signal(String::new());
    let enabled = create_rw_signal(true);
    let active = create_rw_signal(false);
    let calls = create_rw_signal(0);
    let submissions = create_rw_signal(0);
    let choice = create_rw_signal(Some("one"));
    let menu_open = create_rw_signal(false);
    let edit_open = create_rw_signal(false);
    let edit_value = create_rw_signal(String::new());
    scroll(
        v_stack((
            h_stack((
                action_button(
                    ButtonAction::Copy,
                    || tr!(Copy),
                    IconButtonTone::Secondary,
                    palette,
                    move || enabled.get(),
                    move || calls.update(|n| *n += 1),
                ),
                action_button(
                    ButtonAction::Send,
                    || tr!(ChatSend),
                    IconButtonTone::Primary,
                    palette,
                    || false,
                    move || calls.update(|n| *n += 100),
                ),
                icon_toggle_button(
                    ButtonAction::Pin.icon(),
                    move || {
                        if active.get() {
                            tr!(UnpinNote)
                        } else {
                            tr!(PinNote)
                        }
                    },
                    palette,
                    move || active.get(),
                    move || active.update(|v| *v = !*v),
                ),
                compact_icon_button(
                    || ICON_CANCEL,
                    || tr!(Close),
                    IconButtonTone::Secondary,
                    palette,
                    22.0,
                    || true,
                    || {},
                ),
                dialog_button(
                    ButtonAction::Save,
                    crate::i18n::msg!(Save),
                    IconButtonTone::Primary,
                    palette,
                    move || {
                        first.set(String::new());
                        second.set(String::new());
                    },
                ),
            ))
            .style(|s| s.height(32.0).items_center().gap(8.0)),
            TextArea::new(first, palette)
                .placeholder(i18n::Key::ChatPlaceholder)
                .on_submit(move || submissions.update(|n| *n += 1))
                .enabled(move || enabled.get())
                .build(|_| {}),
            TextArea::new(second, palette)
                .placeholder(i18n::Key::SearchNotes)
                .build(|_| {}),
            h_stack((
                dialog_button(
                    ButtonAction::Settings,
                    crate::i18n::msg!(Settings),
                    IconButtonTone::Secondary,
                    palette,
                    move || enabled.update(|v| *v = !*v),
                ),
                dialog_button(
                    ButtonAction::Custom(ButtonAction::Settings.icon()),
                    crate::i18n::msg!(Language),
                    IconButtonTone::Secondary,
                    palette,
                    || {
                        i18n::set_current(if i18n::current() == i18n::Locale::English {
                            i18n::Locale::Russian
                        } else {
                            i18n::Locale::English
                        })
                    },
                ),
                select(
                    choice,
                    vec!["one", "two"],
                    |v| v.unwrap_or_default().to_owned(),
                    move |v| choice.set(Some(v)),
                    || true,
                    palette,
                )
                .style(|s| s.width(180.0)),
            ))
            .style(|s| s.gap(8.0).height(40.0)),
            label(move || {
                format!(
                    "calls={} submissions={} first={:?} second={:?}",
                    calls.get(),
                    submissions.get(),
                    first.get(),
                    second.get()
                )
            })
            .style(|s| s.font_size(crate::ui::FONT_BODY as f32)),
            h_stack((
                anchored_popover(
                    dialog_button(
                        ButtonAction::Add,
                        crate::i18n::msg!(CreateOrOpen),
                        IconButtonTone::Secondary,
                        palette,
                        move || menu_open.update(|v| *v = !*v),
                    ),
                    menu_open,
                    248.0,
                    8.0,
                    true,
                    move || {
                        menu(
                            (0..9)
                                .map(|index| {
                                    MenuEntry::action(
                                        ButtonAction::Copy.icon(),
                                        move || format!("{} {}", tr!(Copy), index + 1),
                                        move || index != 1,
                                        move || calls.update(|n| *n += 1),
                                    )
                                })
                                .collect(),
                            palette,
                        )
                    },
                ),
                context_menu_view(
                    text(crate::i18n::msg!(Note))
                        .keyboard_navigable()
                        .style(|s| s.width(160.0).height(40.0).items_center()),
                    palette,
                    move || {
                        vec![
                            MenuEntry::action(
                                ButtonAction::Copy.icon(),
                                || tr!(Copy),
                                || true,
                                move || calls.update(|n| *n += 1),
                            ),
                            MenuEntry::action(
                                ButtonAction::Pin.icon(),
                                || tr!(PinNote),
                                || true,
                                move || active.update(|v| *v = !*v),
                            )
                            .selected(move || active.get()),
                            MenuEntry::action(
                                ButtonAction::Delete.icon(),
                                || tr!(Trash),
                                || false,
                                || {},
                            )
                            .danger(true),
                        ]
                    },
                ),
                dialog_button(
                    ButtonAction::Save,
                    crate::i18n::msg!(NewTitle),
                    IconButtonTone::Secondary,
                    palette,
                    move || edit_open.update(|v| *v = !*v),
                ),
            ))
            .style(|s| s.width_full().height(40.0).items_center().gap(16.0)),
            toolbar_edit_bar(
                ToolbarEditBar {
                    open: edit_open,
                    value: edit_value,
                    label: i18n::Key::NewTitle,
                    placeholder: i18n::Key::NewTitle,
                },
                palette,
                move || {
                    submissions.update(|n| *n += 1);
                    edit_open.set(false);
                },
            ),
            settings_card(
                i18n::Key::Workspace,
                Some(ICON_FOLDER),
                Some(i18n::Key::WorkspaceDescription),
                actions((dialog_button(
                    ButtonAction::Save,
                    crate::i18n::msg!(Save),
                    IconButtonTone::Secondary,
                    palette,
                    move || calls.update(|n| *n += 1),
                ),)),
                palette,
            ),
        ))
        .style(move |s| {
            s.width_full()
                .padding(24.0)
                .gap(16.0)
                .background(palette.paper)
                .color(palette.ink)
                .font_family(UI_FONT_FAMILY.to_owned())
        }),
    )
    .style(|s| s.size_full())
}
