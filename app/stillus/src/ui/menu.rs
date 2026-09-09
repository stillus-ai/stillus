// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

use super::*;

pub(crate) fn choice_row(
    title: i18n::Message,
    selected: impl Fn() -> bool + 'static,
    on_press: impl Fn() + 'static,
    palette: Palette,
) -> impl IntoView {
    let selected = Rc::new(selected);
    let indicator_selected = selected.clone();
    reliable_button(
        h_stack((
            svg(ICON_SORT).style(|s| s.size(16.0, 16.0).margin_right(8.0)),
            text(title).style(move |style| {
                style
                    .font_size(crate::ui::FONT_BODY as f32)
                    .color(palette.ink)
                    .selectable(false)
            }),
            empty().style(|style| style.flex_grow(1.0)),
            label(move || {
                if indicator_selected() {
                    "✓".to_owned()
                } else {
                    String::new()
                }
            })
            .style(move |style| {
                style
                    .width(16.0)
                    .font_size(crate::ui::FONT_BODY as f32)
                    .color(palette.accent)
                    .selectable(false)
            }),
        ))
        .style(|style| style.width_full().items_center()),
        on_press,
    )
    .style(move |style| {
        style
            .width_full()
            .height(30.0)
            .padding_horiz(8.0)
            .border_radius(5.0)
            .background(if selected() {
                palette.accent_soft
            } else {
                Color::TRANSPARENT
            })
            .hover(move |style| style.background(palette.canvas))
    })
}

pub(crate) fn menu_action_row(
    title: i18n::Message,
    danger: bool,
    palette: Palette,
    on_press: impl Fn() + 'static,
) -> impl IntoView {
    content_button(
        ICON_LOCK,
        text(title).style(move |style| {
            style
                .font_size(crate::ui::FONT_BODY as f32)
                .color(if danger { palette.danger } else { palette.ink })
                .selectable(false)
        }),
        on_press,
    )
    .style(move |style| {
        style
            .width_full()
            .height(32.0)
            .padding_horiz(8.0)
            .items_center()
            .cursor(CursorStyle::Pointer)
            .border_radius(5.0)
            .hover(move |style| style.background(palette.canvas))
    })
}

pub(crate) fn menu_item(
    icon: &'static str,
    title: i18n::Message,
    enabled: bool,
    palette: Palette,
    on_press: impl Fn() + 'static,
) -> impl IntoView {
    menu_button(
        icon,
        move || title.to_string(),
        palette,
        move || enabled,
        on_press,
    )
}

pub(crate) fn native_edit_menu(
    can_copy: bool,
    can_paste: bool,
    cut: impl Fn() + 'static,
    copy: impl Fn() + 'static,
    paste: impl Fn() + 'static,
) -> floem::menu::Menu {
    use floem::menu::{Menu, MenuItem};
    type Entry = (ButtonAction, String, Box<dyn Fn()>);
    let entries: [Entry; 3] = [
        (ButtonAction::Cut, tr!(Cut), Box::new(cut)),
        (ButtonAction::Copy, tr!(Copy), Box::new(copy)),
        (ButtonAction::Paste, tr!(Paste), Box::new(paste)),
    ];
    let mut menu = Menu::new("");
    for (kind, title, action) in entries {
        let enabled = if kind == ButtonAction::Paste {
            menu = menu.separator();
            can_paste
        } else {
            can_copy
        };
        menu = menu.entry(MenuItem::new(title).enabled(enabled).action(action));
    }
    menu
}
