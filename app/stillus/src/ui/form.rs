// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

use super::*;

const TOOLBAR_EDIT_BAR_MIN_HEIGHT_PX: f64 = 48.0;
/// An inline editing row under a toolbar: the shared shape behind renaming an
/// item and editing its categories.
#[derive(Clone, Copy)]
pub(crate) struct ToolbarEditBar {
    pub(crate) open: RwSignal<bool>,
    pub(crate) value: RwSignal<String>,
    pub(crate) label: i18n::Key,
    pub(crate) placeholder: i18n::Key,
}

pub(crate) fn toolbar_edit_bar(
    bar: ToolbarEditBar,
    palette: Palette,
    on_submit: impl Fn() + 'static,
) -> impl IntoView {
    let submit: Rc<dyn Fn()> = Rc::new(on_submit);
    let key_submit = submit.clone();
    let original = create_rw_signal(String::new());
    let width = create_rw_signal(720.0);
    create_effect(move |previous| {
        let open = bar.open.get();
        if open && previous != Some(true) {
            original.set(bar.value.get_untracked());
        }
        open
    });
    let cancel = move || {
        bar.value.set(original.get_untracked());
        bar.open.set(false);
    };
    let input = localized_input::LocalizedInput::new(bar.value, bar.placeholder)
        .on_escape(cancel)
        .on_event(EventListener::KeyDown, move |event| {
            let Event::KeyDown(key) = event else {
                return EventPropagation::Continue;
            };
            if key.key.logical_key == Key::Named(NamedKey::Enter) {
                key_submit();
                EventPropagation::Stop
            } else {
                EventPropagation::Continue
            }
        })
        .style(move |style| {
            form_field_style(style, palette, false)
                .min_width(160.0)
                .flex_grow(1.0)
                .flex_shrink(1.0)
                .apply_if(width.get() < 640.0, |s| s.width_full())
        });
    // Opening the bar hands the field the caret, so the control that opened it
    // does not have to be followed by a click into the field.
    let input_id = input.id();
    let focus_generation = create_rw_signal(0_u64);
    create_effect(move |_| {
        let open = bar.open.get();
        let generation = focus_generation.get_untracked().wrapping_add(1);
        focus_generation.set(generation);
        if open {
            exec_after(Duration::from_millis(10), move |_| {
                // Closing, reopening or disposing the bar invalidates this
                // request, even if its timer was already queued by Floem.
                if bar.open.try_get_untracked() == Some(true)
                    && focus_generation.try_get_untracked() == Some(generation)
                {
                    input_id.request_focus();
                }
            });
        }
    });
    let content = h_stack((
        text(bar.label).style(move |style| {
            style
                .font_size(crate::ui::FONT_CAPTION as f32)
                .color(palette.muted)
                .selectable(false)
                .apply_if(i18n::current().is_rtl(), |s| s.justify_end())
                .width(if width.get() < 640.0 {
                    width.get() - 48.0
                } else {
                    120.0
                })
                .flex_shrink(0.0)
        }),
        input,
        actions((
            form_action_button(
                ButtonAction::Cancel,
                || tr!(Cancel),
                IconButtonTone::Secondary,
                palette,
                || true,
                cancel,
            ),
            form_action_button(
                ButtonAction::Save,
                || tr!(Save),
                IconButtonTone::Primary,
                palette,
                || true,
                move || {
                    submit();
                },
            ),
        ))
        .style(|s| s.min_width(0.0).flex_shrink(0.0)),
    ))
    .style(move |style| {
        let style = rtl_row(style)
            .width_full()
            .min_width(0.0)
            .min_height(TOOLBAR_EDIT_BAR_MIN_HEIGHT_PX)
            .flex_shrink(0.0)
            .flex_wrap(floem::taffy::FlexWrap::Wrap)
            .padding_vert(8.0)
            .padding_horiz(24.0)
            .items_center()
            .gap(8.0)
            .background(palette.canvas)
            .border_bottom(1.0)
            .border_color(palette.divider);
        if bar.open.get() { style } else { style.hide() }
    })
    .on_resize(move |rect| width.set(rect.width()));
    form_focus_scope(content, vec![input_id], move || bar.open.get(), false).style(move |s| {
        s.width_full()
            .min_width(0.0)
            .flex_shrink(0.0)
            .apply_if(!bar.open.get(), |s| s.hide())
    })
}

/// The page heading of a settings section, matching the general and
/// encryption pages.
pub(crate) fn page_title(key: i18n::Key, palette: Palette) -> impl IntoView {
    label(move || key.to_string()).style(move |style| {
        style
            .font_size(crate::ui::FONT_SCREEN as f32)
            .font_family(crate::ui::HEADING_FONT_FAMILY.to_owned())
            .font_weight(floem::text::Weight::SEMIBOLD)
            .color(palette.ink)
            .selectable(false)
    })
}

pub(crate) fn page_description(key: i18n::Key, palette: Palette) -> impl IntoView {
    label(move || key.to_string()).style(move |style| {
        style
            .font_size(crate::ui::FONT_BODY as f32)
            .color(palette.muted)
            .selectable(false)
    })
}

/// A step of the page: the connection and the model aliases are two sections
/// of one page, titled like the cards of the general settings page.
pub(crate) fn section_title(key: i18n::Key, palette: Palette) -> impl IntoView {
    label(move || key.to_string()).style(move |style| {
        style
            .font_size(crate::ui::FONT_SECTION as f32)
            .font_family(crate::ui::HEADING_FONT_FAMILY.to_owned())
            .font_weight(floem::text::Weight::SEMIBOLD)
            .color(palette.ink)
            .selectable(false)
    })
}

pub(crate) fn spacer(height: f64) -> impl IntoView {
    empty().style(move |style| style.height(height))
}

/// One row of form actions. Buttons keep their own width instead of
/// stretching across the card, and wrap before they shrink.
pub(crate) fn actions(children: impl ViewTuple + 'static) -> impl IntoView {
    h_stack(children).style(|style| {
        rtl_row(style)
            .gap(8.0)
            .items_center()
            .flex_wrap(floem::taffy::FlexWrap::Wrap)
    })
}

/// A settings section with the same heading, surface and inset on every page.
pub(crate) fn settings_card(
    title: i18n::Key,
    icon: Option<&'static str>,
    description: Option<i18n::Key>,
    content: impl IntoView + 'static,
    palette: Palette,
) -> impl IntoView {
    let icon = icon.map_or_else(
        || empty().style(|s| s.hide()).into_any(),
        |icon| {
            svg(icon)
                .style(|s| s.size(20.0, 20.0).flex_shrink(0.0))
                .into_any()
        },
    );
    let description = description.map_or_else(
        || empty().style(|s| s.hide()).into_any(),
        |key| {
            page_description(key, palette)
                .style(|s| s.width_full().min_width(0.0))
                .into_any()
        },
    );
    v_stack((
        h_stack((icon, section_title(title, palette)))
            .style(|s| rtl_row(s).width_full().items_center().gap(8.0)),
        description,
        container(content).style(|s| s.width_full().min_width(0.0).items_start()),
    ))
    .style(move |s| settings_card_style(s, palette).gap(16.0).items_start())
}
