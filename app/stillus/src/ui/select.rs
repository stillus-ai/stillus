// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

use super::*;

pub(crate) fn select<T: Clone + 'static>(
    value: RwSignal<Option<T>>,
    items: Vec<T>,
    display: impl Fn(Option<T>) -> String + 'static,
    accept: impl Fn(T) + 'static,
    enabled: impl Fn() -> bool + 'static,
    palette: Palette,
) -> impl IntoView {
    let display = Rc::new(display);
    let item_display = display.clone();
    floem::views::dropdown::Dropdown::custom(
        move || value.get(),
        move |item| {
            let display = display.clone();
            h_stack((
                label(move || display(item.clone()))
                    .style(|style| style.min_width(0.0).text_ellipsis().selectable(false)),
                svg(ICON_CHEVRON_DOWN).style(|style| style.size(12.0, 12.0)),
            ))
            .style(|style| {
                rtl_row(style)
                    .width_full()
                    .items_center()
                    .justify_between()
                    .gap(8.0)
            })
            .into_any()
        },
        items.into_iter().map(Some).collect::<Vec<_>>(),
        move |item| {
            let display = item_display.clone();
            label(move || display(item.clone()))
                .style(move |style| {
                    style
                        .width_full()
                        .height(34.0)
                        .padding_horiz(12.0)
                        .items_center()
                        .font_size(13.0)
                        .color(palette.ink)
                        .background(palette.paper)
                        .selectable(false)
                        .hover(|style| style.background(palette.accent_soft))
                        .focus(|style| style.background(palette.accent_soft))
                })
                .into_any()
        },
    )
    .on_accept(move |item| {
        if let Some(item) = item {
            accept(item);
        }
    })
    .disabled(move || !enabled())
    .keyboard_navigable()
    .style(move |style| {
        settings_control_style(style, palette)
            .cursor(CursorStyle::Pointer)
            .focus(|style| style.border_color(palette.accent))
            .disabled(|style| style.color(palette.muted).cursor(CursorStyle::Default))
            .class(floem::views::scroll::ScrollClass, |style| {
                style
                    .width_full()
                    .max_height(204.0)
                    .background(palette.paper)
                    .border(1.0)
                    .border_color(palette.divider)
                    .border_radius(6.0)
            })
    })
}

pub(crate) fn language_select(
    accept: impl Fn(i18n::Locale) + 'static,
    palette: Palette,
) -> impl IntoView {
    floem::views::dropdown::Dropdown::new(i18n::current, i18n::Locale::ALL.iter().copied())
        .main_view(|_| {
            h_stack((
                label(|| i18n::current().native_name()).style(|style| style.font_size(13.0)),
                svg(ICON_CHEVRON_DOWN).style(|style| style.size(12.0, 12.0)),
            ))
            .style(|style| {
                rtl_row(style)
                    .width_full()
                    .items_center()
                    .justify_between()
                    .gap(8.0)
                    .font_family("sans-serif".to_owned())
            })
            .into_any()
        })
        .list_item_view(move |locale| {
            text(locale.native_name())
                .style(move |style| {
                    style
                        .width_full()
                        .min_height(30.0)
                        .padding_horiz(10.0)
                        .padding_vert(6.0)
                        .font_family("sans-serif".to_owned())
                        .font_size(13.0)
                        .color(palette.ink)
                        .background(palette.paper)
                        .hover(move |style| style.background(palette.accent_soft))
                        .focus(move |style| style.background(palette.accent_soft))
                })
                .into_any()
        })
        .style(move |style| {
            style
                .width(300.0)
                .min_height(36.0)
                .padding_horiz(10.0)
                .padding_vert(6.0)
                .background(palette.paper)
                .color(palette.ink)
                .border(1.0)
                .border_color(palette.divider)
                .border_radius(6.0)
                .class(floem::views::scroll::ScrollClass, |style| {
                    style
                        .width(300.0)
                        .max_height(280.0)
                        .background(palette.paper)
                        .border(1.0)
                        .border_color(palette.divider)
                        .border_radius(6.0)
                })
        })
        .on_accept(accept)
}
