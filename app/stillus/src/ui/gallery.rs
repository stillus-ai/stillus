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
    ))
    .style(move |s| {
        s.size_full()
            .padding(24.0)
            .gap(16.0)
            .background(palette.paper)
            .color(palette.ink)
            .font_family(UI_FONT_FAMILY.to_owned())
    })
}
