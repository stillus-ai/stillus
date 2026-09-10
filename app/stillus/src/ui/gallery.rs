// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]
//! Disposable component fixture, compiled only with test-utils.
use super::*;

pub(crate) fn view() -> AnyView {
    if std::env::var_os("STILLUS_TEST_REVIEW").is_some() {
        return review_fixture();
    }
    if std::env::var_os("STILLUS_TEST_SECRET").as_deref() == Some(std::ffi::OsStr::new("1")) {
        return secret_fixture();
    }
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
                toolbar_action_button(
                    ButtonAction::Copy,
                    || tr!(Copy),
                    IconButtonTone::Secondary,
                    palette,
                    move || enabled.get(),
                    move || calls.update(|n| *n += 1),
                ),
                toolbar_action_button(
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
                form_action_button(
                    ButtonAction::Save,
                    || tr!(Save),
                    IconButtonTone::Primary,
                    palette,
                    || true,
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
                actions((form_action_button(
                    ButtonAction::Save,
                    || tr!(Save),
                    IconButtonTone::Secondary,
                    palette,
                    || true,
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
    .into_any()
}

fn review_fixture() -> AnyView {
    let palette = Palette::new();
    let selected = create_rw_signal(Some(0_usize));
    let draft = create_rw_signal(String::new());
    let edit_open = create_rw_signal(false);
    let edit_value = create_rw_signal("Original title".to_owned());
    let title = create_rw_signal("Original title".to_owned());
    let summary = selectable_rich_text(
        move || {
            let text = format!(
                "Selected {} · {}",
                selected.get().unwrap_or_default(),
                title.get()
            );
            let mut layout = floem::text::TextLayout::new();
            layout.set_text(
                &text,
                floem::text::AttrsList::new(floem::text::Attrs::new().font_size(14.0)),
            );
            (text, layout)
        },
        palette,
        None,
    );
    v_stack((
        searchable_select(
            selected,
            (0..100).collect(),
            |value| {
                format!(
                    "Model {:03} — a long readable model name",
                    value.unwrap_or_default()
                )
            },
            move |value| selected.set(Some(value)),
            || true,
            palette,
        )
        .style(|s| s.width(560.0)),
        summary.style(|s| s.height(24.0)),
        // Reserve fixture geometry; the actual field inside must still grow.
        container(
            TextArea::new(draft, palette)
                .auto_height(|| 180.0)
                .build(|_| {}),
        )
        .style(|s| s.width(560.0).height(180.0).items_start()),
        actions((
            form_action_button(
                ButtonAction::Edit,
                || tr!(NewTitle),
                IconButtonTone::Secondary,
                palette,
                || true,
                move || {
                    edit_value.set(title.get_untracked());
                    edit_open.set(true);
                },
            )
            .style(|s| s.width(180.0)),
            form_action_button(
                ButtonAction::Settings,
                || tr!(Language),
                IconButtonTone::Secondary,
                palette,
                || true,
                || {
                    i18n::set_current(match i18n::current() {
                        i18n::Locale::English => i18n::Locale::Russian,
                        i18n::Locale::Russian => i18n::Locale::Arabic,
                        i18n::Locale::Arabic => i18n::Locale::Urdu,
                        _ => i18n::Locale::English,
                    })
                },
            )
            .style(|s| s.width(160.0)),
        ))
        .style(|s| s.width(560.0)),
        toolbar_edit_bar(
            ToolbarEditBar {
                open: edit_open,
                value: edit_value,
                label: i18n::Key::NewTitle,
                placeholder: i18n::Key::NewTitle,
            },
            palette,
            move || {
                title.set(edit_value.get_untracked());
                edit_open.set(false);
            },
        )
        .style(|s| s.width(560.0)),
    ))
    .style(|s| {
        s.size_full()
            .padding(24.0)
            .gap(16.0)
            .background(Color::WHITE)
    })
    .into_any()
}

fn secret_fixture() -> AnyView {
    use zeroize::Zeroizing;
    let palette = Palette::new();
    let value = Rc::new(RefCell::new(Zeroizing::new(String::with_capacity(1024))));
    let read = value.clone();
    let write = value.clone();
    let revision = create_rw_signal(0_u64);
    let selected = create_rw_signal(Some(0_usize));
    stack((
        v_stack((
            SecretInput::new(
                move || read.borrow().clone(),
                move |range, insert| {
                    let accepted = replace_secret(&mut write.borrow_mut(), range, insert, 1024);
                    revision.update(|v| *v += 1);
                    accepted
                },
                revision,
                i18n::Key::EnterPassword,
                palette,
            )
            .keyboard_navigable()
            .style(move |s| settings_control_style(s, palette).width(300.0)),
            label(move || {
                revision.get();
                format!("verified={}", value.borrow().as_str() == "aZcdXY")
            })
            .style(|s| s.height(24.0)),
            label(move || format!("selected={}", selected.get().unwrap_or_default())),
        ))
        .style(|s| s.absolute().inset_left(24.0).inset_top(24.0).gap(16.0)),
        select(
            selected,
            (0..12).collect(),
            |value| {
                format!(
                    "An intentionally long model alias with the same prefix — {}",
                    value.unwrap_or_default()
                )
            },
            move |value| selected.set(Some(value)),
            || true,
            palette,
        )
        .style(|s| {
            s.absolute()
                .inset_left(24.0)
                .inset_bottom(24.0)
                .width(240.0)
        }),
    ))
    .style(move |s| s.size_full().background(palette.paper).color(palette.ink))
    .into_any()
}
