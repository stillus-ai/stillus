// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

use super::*;

pub(crate) const CONTENT_HEADER_HEIGHT_PX: f64 = 56.0;

/// A shared, fixed header for notes, feeds and chats. Its title can shrink while
/// action controls remain reachable at the right edge of the content column.
pub(crate) fn content_header(
    icon: &'static str,
    title: impl Fn() -> String + 'static,
    actions: impl IntoView + 'static,
    on_title_click: Option<Rc<dyn Fn()>>,
    palette: Palette,
) -> AnyView {
    let title: Rc<dyn Fn() -> String> = Rc::new(title);
    let label_title = title.clone();
    let title_label = label(move || label_title()).style(move |style| {
        style
            .min_width(0.0)
            .flex_shrink(1.0)
            .font_family(HEADING_FONT_FAMILY.to_owned())
            .font_size(super::FONT_SCREEN)
            .font_weight(floem::text::Weight::SEMIBOLD)
            .text_ellipsis()
            .selectable(false)
            .color(palette.ink)
    });
    let title_row = h_stack((
        svg(icon).style(move |style| style.size(18.0, 18.0).flex_shrink(0.0).color(palette.ink)),
        title_label,
    ))
    .style(|style| {
        style
            .min_width(0.0)
            .flex_shrink(1.0)
            .flex_grow(1.0)
            .items_center()
            .gap(10.0)
    });
    let title_row = if let Some(action) = on_title_click {
        reliable_button(title_row, move || action())
            .style(move |style| {
                style
                    .min_width(0.0)
                    .flex_grow(1.0)
                    .flex_shrink(1.0)
                    .cursor(CursorStyle::Pointer)
                    .border_radius(4.0)
                    .focus_visible(move |style| style.background(palette.accent_soft))
            })
            .into_any()
    } else {
        title_row.into_any()
    };
    let title_row = super::anchored_tooltip(title_row, title, palette);
    h_stack((
        title_row,
        actions.into_view().style(|style| style.flex_shrink(0.0)),
    ))
    .style(move |style| {
        style
            .width_full()
            .min_width(0.0)
            .height(CONTENT_HEADER_HEIGHT_PX)
            .min_height(CONTENT_HEADER_HEIGHT_PX)
            .max_height(CONTENT_HEADER_HEIGHT_PX)
            .flex_shrink(0.0)
            .items_center()
            .gap(12.0)
            .padding_horiz(20.0)
            .background(palette.paper)
            .border_bottom(1.0)
            .border_color(palette.divider)
    })
    .into_any()
}
