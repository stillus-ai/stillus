// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

use super::*;

const SELECT_ROW_HEIGHT: f64 = 34.0;
const SELECT_VISIBLE_ROWS: usize = 8;

fn next_index(key: &Key, index: usize, length: usize) -> Option<usize> {
    if length == 0 {
        return None;
    }
    match key {
        Key::Named(NamedKey::ArrowDown) => Some((index + 1).min(length - 1)),
        Key::Named(NamedKey::ArrowUp) => Some(index.saturating_sub(1)),
        Key::Named(NamedKey::Home) => Some(0),
        Key::Named(NamedKey::End) => Some(length - 1),
        Key::Named(NamedKey::PageDown) => Some((index + SELECT_VISIBLE_ROWS).min(length - 1)),
        Key::Named(NamedKey::PageUp) => Some(index.saturating_sub(SELECT_VISIBLE_ROWS)),
        _ => None,
    }
}

pub(crate) fn select<T: Clone + 'static>(
    value: RwSignal<Option<T>>,
    items: Vec<T>,
    display: impl Fn(Option<T>) -> String + 'static,
    accept: impl Fn(T) + 'static,
    enabled: impl Fn() -> bool + 'static,
    palette: Palette,
) -> impl IntoView {
    let open = create_rw_signal(false);
    let current = create_rw_signal(0_usize);
    let display = Rc::new(display);
    let items = Rc::new(items);
    let accept = Rc::new(accept);
    let enabled = Rc::new(enabled);
    let selected_display = display.clone();
    let open_items = items.clone();
    let open_display = display.clone();
    let open_enabled = enabled.clone();
    let toggle: Rc<dyn Fn()> = Rc::new(move || {
        if !open_enabled() || open_items.is_empty() {
            return;
        }
        if !open.get_untracked() {
            let selected = open_display(value.get_untracked());
            current.set(
                open_items
                    .iter()
                    .position(|item| open_display(Some(item.clone())) == selected)
                    .unwrap_or(0),
            );
        }
        open.update(|v| *v = !*v);
    });
    let click_toggle = toggle.clone();
    let keyboard_toggle = toggle;
    let key_items = items.clone();
    let key_accept = accept.clone();
    let key_enabled = enabled.clone();
    let trigger = PrimaryPointerView::new(
        h_stack((
            label(move || selected_display(value.get())).style(|s| {
                s.min_width(0.0)
                    .flex_shrink(1.0)
                    .text_ellipsis()
                    .selectable(false)
            }),
            svg(ICON_CHEVRON_DOWN).style(|s| s.size(12.0, 12.0).flex_shrink(0.0)),
        ))
        .style(|s| {
            rtl_row(s)
                .width_full()
                .items_center()
                .justify_between()
                .gap(8.0)
        }),
        move |_| click_toggle(),
    )
    .keyboard_navigable()
    .on_event(EventListener::KeyDown, move |event| {
        if popover_handle_escape(event) {
            return EventPropagation::Stop;
        }
        let Event::KeyDown(key) = event else {
            return EventPropagation::Continue;
        };
        if !key_enabled() || key_items.is_empty() {
            return EventPropagation::Continue;
        }
        if open.get_untracked() {
            if let Some(next) = next_index(
                &key.key.logical_key,
                current.get_untracked(),
                key_items.len(),
            ) {
                current.set(next);
                return EventPropagation::Stop;
            }
            if is_keyboard_activation(event) {
                if let Some(item) = key_items.get(current.get_untracked()) {
                    open.set(false);
                    key_accept(item.clone());
                }
                return EventPropagation::Stop;
            }
        }
        if is_keyboard_activation(event)
            || matches!(
                key.key.logical_key,
                Key::Named(NamedKey::ArrowDown | NamedKey::ArrowUp)
            )
        {
            keyboard_toggle();
            EventPropagation::Stop
        } else {
            EventPropagation::Continue
        }
    })
    .style(move |s| {
        let s = settings_control_style(s, palette)
            .min_width(0.0)
            .width_full()
            .cursor(CursorStyle::Pointer)
            .focus(|s| s.border_color(palette.accent));
        if enabled() {
            s
        } else {
            s.color(palette.muted).cursor(CursorStyle::Default)
        }
    });
    anchored_popover(trigger, open, 0.0, 8.0, true, move || {
        let row_ids = Rc::new(RefCell::new(Vec::<ViewId>::new()));
        let rows = items
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, item)| {
                let title = display(Some(item.clone()));
                let accept = accept.clone();
                let keyboard_accept = accept.clone();
                let keyboard_items = items.clone();
                let keyboard_ids = row_ids.clone();
                let view = PrimaryPointerView::new(
                    text(title).style(|s| s.width_full().text_ellipsis().selectable(false)),
                    move |_| {
                        open.set(false);
                        accept(item.clone());
                    },
                )
                .keyboard_navigable()
                .on_event(EventListener::KeyDown, move |event| {
                    if popover_handle_escape(event) {
                        return EventPropagation::Stop;
                    }
                    let Event::KeyDown(key) = event else {
                        return EventPropagation::Continue;
                    };
                    if let Some(next) = next_index(
                        &key.key.logical_key,
                        current.get_untracked(),
                        keyboard_items.len(),
                    ) {
                        current.set(next);
                        if let Some(id) = keyboard_ids.borrow().get(next) {
                            ViewId::request_focus(id);
                        }
                        EventPropagation::Stop
                    } else if is_keyboard_activation(event) {
                        if let Some(item) = keyboard_items.get(current.get_untracked()) {
                            open.set(false);
                            keyboard_accept(item.clone());
                        }
                        EventPropagation::Stop
                    } else {
                        EventPropagation::Continue
                    }
                })
                .on_event_cont(EventListener::FocusGained, move |_| current.set(index))
                .style(move |s| {
                    s.width_full()
                        .height(SELECT_ROW_HEIGHT)
                        .flex_shrink(0.0)
                        .padding_horiz(12.0)
                        .items_center()
                        .font_size(13.0)
                        .color(palette.ink)
                        .background(if current.get() == index {
                            palette.accent_soft
                        } else {
                            palette.paper
                        })
                        .hover(|s| s.background(palette.accent_soft))
                });
                row_ids.borrow_mut().push(view.id());
                view
            })
            .collect::<Vec<_>>();
        let rows_count = items.len().min(SELECT_VISIBLE_ROWS);
        let hide_bars = items.len() <= SELECT_VISIBLE_ROWS;
        scroll(v_stack_from_iter(rows).style(|s| s.width_full()))
            .scroll_to_view(move || row_ids.borrow().get(current.get()).copied())
            .scroll_style(move |s| s.hide_bars(hide_bars))
            .style(move |s| {
                s.width_full()
                    .height(SELECT_ROW_HEIGHT * rows_count as f64 + 2.0)
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
    let value = create_rw_signal(Some(i18n::current()));
    create_effect(move |_| value.set(Some(i18n::current())));
    select(
        value,
        i18n::Locale::ALL.to_vec(),
        |locale| locale.map_or_else(String::new, |locale| locale.native_name().to_owned()),
        accept,
        || true,
        palette,
    )
    .style(|s| s.width(300.0).min_width(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_keyboard_bounds_and_page_navigation() {
        assert_eq!(next_index(&Key::Named(NamedKey::ArrowUp), 0, 17), Some(0));
        assert_eq!(
            next_index(&Key::Named(NamedKey::ArrowDown), 16, 17),
            Some(16)
        );
        assert_eq!(next_index(&Key::Named(NamedKey::Home), 10, 17), Some(0));
        assert_eq!(next_index(&Key::Named(NamedKey::End), 0, 17), Some(16));
        assert_eq!(next_index(&Key::Named(NamedKey::PageDown), 3, 17), Some(11));
        assert_eq!(next_index(&Key::Named(NamedKey::ArrowDown), 0, 0), None);
    }
}
