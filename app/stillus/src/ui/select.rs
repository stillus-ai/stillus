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
    select_impl(value, items, display, accept, enabled, palette, false)
}

pub(crate) fn searchable_select<T: Clone + 'static>(
    value: RwSignal<Option<T>>,
    items: Vec<T>,
    display: impl Fn(Option<T>) -> String + 'static,
    accept: impl Fn(T) + 'static,
    enabled: impl Fn() -> bool + 'static,
    palette: Palette,
) -> impl IntoView {
    select_impl(value, items, display, accept, enabled, palette, true)
}

fn select_impl<T: Clone + 'static>(
    value: RwSignal<Option<T>>,
    items: Vec<T>,
    display: impl Fn(Option<T>) -> String + 'static,
    accept: impl Fn(T) + 'static,
    enabled: impl Fn() -> bool + 'static,
    palette: Palette,
    searchable: bool,
) -> impl IntoView {
    let open = create_rw_signal(false);
    let current = create_rw_signal(0_usize);
    let query = create_rw_signal(String::new());
    let searchable = searchable && items.len() > SELECT_VISIBLE_ROWS;
    let display = Rc::new(display);
    let items = Rc::new(items);
    let accept = Rc::new(accept);
    let enabled = Rc::new(enabled);
    let filter_items = items.clone();
    let filter_display = display.clone();
    let filtered = floem::reactive::create_memo(move |_| {
        let titles = filter_items
            .iter()
            .map(|item| filter_display(Some(item.clone())))
            .collect::<Vec<_>>();
        matching_indices(&titles, &query.get())
    });
    let reset_items = items.clone();
    let reset_display = display.clone();
    create_effect(move |_| {
        let query = query.get();
        let selected = reset_display(value.get_untracked());
        current.set(if query.is_empty() {
            reset_items
                .iter()
                .position(|item| reset_display(Some(item.clone())) == selected)
                .unwrap_or(0)
        } else {
            0
        });
    });
    let selected_display = display.clone();
    let tooltip_display = display.clone();
    let open_items = items.clone();
    let open_display = display.clone();
    let open_enabled = enabled.clone();
    let toggle: Rc<dyn Fn()> = Rc::new(move || {
        if !open_enabled() || open_items.is_empty() {
            return;
        }
        if !open.get_untracked() {
            query.set(String::new());
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
                filtered.get_untracked().len(),
            ) {
                current.set(next);
                return EventPropagation::Stop;
            }
            if is_keyboard_activation(event) {
                if let Some(item) = filtered
                    .get_untracked()
                    .get(current.get_untracked())
                    .and_then(|index| key_items.get(*index))
                {
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
    let trigger = anchored_tooltip(
        trigger,
        Rc::new(move || tooltip_display(value.get())),
        palette,
    );
    anchored_popover(trigger, open, 0.0, 8.0, true, move || {
        let row_ids = Rc::new(RefCell::new(Vec::<ViewId>::new()));
        let search_items = items.clone();
        let search_accept = accept.clone();
        let search = localized_input::LocalizedInput::new(query, i18n::Key::SelectSearch)
            .on_key(move |event| {
                let Event::KeyDown(key) = event else {
                    return EventPropagation::Continue;
                };
                let indices = filtered.get_untracked();
                if let Some(next) =
                    next_index(&key.key.logical_key, current.get_untracked(), indices.len())
                {
                    current.set(next);
                    EventPropagation::Stop
                } else if key.key.logical_key == Key::Named(NamedKey::Enter) {
                    if let Some(item) = indices
                        .get(current.get_untracked())
                        .and_then(|i| search_items.get(*i))
                    {
                        open.set(false);
                        search_accept(item.clone());
                    }
                    EventPropagation::Stop
                } else {
                    EventPropagation::Continue
                }
            })
            .style(move |s| {
                form_field_style(s, palette, false)
                    .width_full()
                    .min_width(0.0)
                    .apply_if(!searchable, |s| s.hide())
            });
        let search_id = search.id();
        if searchable {
            exec_after(Duration::from_millis(10), move |_| {
                if open.try_get_untracked() == Some(true) {
                    search_id.request_focus();
                }
            });
        }
        let rows = items
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, item)| {
                let title = display(Some(item.clone()));
                let hint = title.clone();
                let selected_title = title.clone();
                let selected_display = display.clone();
                let accept = accept.clone();
                let keyboard_accept = accept.clone();
                let keyboard_items = items.clone();
                let keyboard_ids = row_ids.clone();
                let view = PrimaryPointerView::new(
                    h_stack((
                        label(move || {
                            if selected_display(value.get()) == selected_title {
                                "✓"
                            } else {
                                ""
                            }
                        })
                        .style(|s| s.width(16.0).flex_shrink(0.0)),
                        text(title).style(move |s| {
                            s.min_width(0.0)
                                .flex_grow(1.0)
                                .apply_if(!searchable, |s| s.text_ellipsis())
                                .selectable(false)
                        }),
                    ))
                    .style(|s| {
                        rtl_row(s)
                            .width_full()
                            .min_width(0.0)
                            .items_center()
                            .gap(6.0)
                    }),
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
                        filtered.get_untracked().len(),
                    ) {
                        current.set(next);
                        if let Some(id) = filtered
                            .get_untracked()
                            .get(next)
                            .and_then(|i| keyboard_ids.borrow().get(*i).copied())
                        {
                            ViewId::request_focus(&id);
                        }
                        EventPropagation::Stop
                    } else if is_keyboard_activation(event) {
                        if let Some(item) = filtered
                            .get_untracked()
                            .get(current.get_untracked())
                            .and_then(|i| keyboard_items.get(*i))
                        {
                            open.set(false);
                            keyboard_accept(item.clone());
                        }
                        EventPropagation::Stop
                    } else {
                        EventPropagation::Continue
                    }
                })
                .on_event_cont(EventListener::FocusGained, move |_| {
                    if let Some(position) =
                        filtered.get_untracked().iter().position(|i| *i == index)
                    {
                        current.set(position);
                    }
                })
                .style(move |s| {
                    s.width_full()
                        .min_height(SELECT_ROW_HEIGHT)
                        .apply_if(!searchable, |s| s.height(SELECT_ROW_HEIGHT))
                        .apply_if(searchable, |s| s.padding_vert(8.0))
                        .apply_if(!filtered.get().contains(&index), |s| s.hide())
                        .flex_shrink(0.0)
                        .padding_horiz(12.0)
                        .items_center()
                        .font_size(crate::ui::FONT_BODY as f32)
                        .color(palette.ink)
                        .background(if filtered.get().get(current.get()) == Some(&index) {
                            palette.accent_soft
                        } else {
                            palette.paper
                        })
                        .hover(|s| s.background(palette.accent_soft))
                });
                row_ids.borrow_mut().push(view.id());
                anchored_tooltip(view, Rc::new(move || hint.clone()), palette)
            })
            .collect::<Vec<_>>();
        let selected_row_ids = row_ids.clone();
        let list = scroll(v_stack_from_iter(rows).style(|s| s.width_full()))
            .scroll_to_view(move || {
                filtered
                    .get()
                    .get(current.get())
                    .and_then(|i| selected_row_ids.borrow().get(*i).copied())
            })
            .scroll_style(move |s| {
                s.hide_bars(!searchable && filtered.get().len() <= SELECT_VISIBLE_ROWS)
            })
            .style(move |s| {
                s.width_full()
                    .min_height(0.0)
                    .apply_if(!searchable, |s| {
                        s.height(
                            SELECT_ROW_HEIGHT
                                * filtered.get().len().min(SELECT_VISIBLE_ROWS) as f64
                                + 2.0,
                        )
                    })
                    .apply_if(searchable, |s| {
                        s.max_height(SELECT_ROW_HEIGHT * SELECT_VISIBLE_ROWS as f64)
                    })
            });
        v_stack((
            search,
            label(|| tr!(NoMatches)).style(move |s| {
                s.padding(12.0)
                    .color(palette.muted)
                    .apply_if(!filtered.get().is_empty(), |s| s.hide())
            }),
            list,
        ))
        .style(move |s| {
            s.width_full()
                .min_width(0.0)
                .background(palette.paper)
                .border(1.0)
                .border_color(palette.divider)
                .border_radius(6.0)
        })
    })
    .style(|style| style.width_full().min_width(0.0))
}

fn matching_indices(titles: &[String], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    titles
        .iter()
        .enumerate()
        .filter_map(|(index, title)| title.to_lowercase().contains(&query).then_some(index))
        .collect()
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
    fn search_matches_unicode_and_retains_original_item_identity() {
        let titles = vec!["GPT Alpha".into(), "Модель Beta".into(), "GPT Beta".into()];
        assert_eq!(matching_indices(&titles, " beta "), vec![1, 2]);
        assert_eq!(matching_indices(&titles, "МОДЕЛЬ"), vec![1]);
        assert!(matching_indices(&titles, "missing").is_empty());
        assert_eq!(matching_indices(&titles, ""), vec![0, 1, 2]);
    }

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
