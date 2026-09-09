// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

use super::*;

const MENU_ROW_HEIGHT: f64 = 32.0;
const MENU_VISIBLE_ROWS: usize = 8;
const MENU_WIDTH: f64 = 248.0;

/// A screen supplies a label, state and action; navigation and presentation live here.
#[derive(Clone)]
pub(crate) struct MenuEntry {
    icon: &'static str,
    title: Rc<dyn Fn() -> String>,
    enabled: Rc<dyn Fn() -> bool>,
    selected: Rc<dyn Fn() -> bool>,
    action: Rc<dyn Fn()>,
    danger: bool,
    keep_open: bool,
}

impl MenuEntry {
    pub(crate) fn action(
        icon: &'static str,
        title: impl Fn() -> String + 'static,
        enabled: impl Fn() -> bool + 'static,
        action: impl Fn() + 'static,
    ) -> Self {
        Self {
            icon,
            title: Rc::new(title),
            enabled: Rc::new(enabled),
            selected: Rc::new(|| false),
            action: Rc::new(action),
            danger: false,
            keep_open: false,
        }
    }

    pub(crate) fn selected(mut self, selected: impl Fn() -> bool + 'static) -> Self {
        self.selected = Rc::new(selected);
        self
    }

    pub(crate) fn danger(mut self, danger: bool) -> Self {
        self.danger = danger;
        self
    }

    pub(crate) fn keep_open(mut self) -> Self {
        self.keep_open = true;
        self
    }

    fn invoke(&self) {
        if (self.enabled)() {
            if !self.keep_open {
                popover_close_top();
            }
            (self.action)();
        }
    }
}

fn menu_next(entries: &[MenuEntry], index: usize, backwards: bool) -> Option<usize> {
    let length = entries.len();
    (1..=length)
        .map(|step| {
            if backwards {
                (index + length - step % length) % length
            } else {
                (index + step) % length
            }
        })
        .find(|index| (entries[*index].enabled)())
}

fn menu_key(key: &Key, entries: &[MenuEntry], current: usize) -> Option<usize> {
    match key {
        Key::Named(NamedKey::ArrowDown) => menu_next(entries, current, false),
        Key::Named(NamedKey::ArrowUp) => menu_next(entries, current, true),
        Key::Named(NamedKey::Home) => entries.iter().position(|e| (e.enabled)()),
        Key::Named(NamedKey::End) => entries.iter().rposition(|e| (e.enabled)()),
        _ => None,
    }
}

fn is_menu_navigation(key: &Key) -> bool {
    matches!(
        key,
        Key::Named(NamedKey::ArrowDown | NamedKey::ArrowUp | NamedKey::Home | NamedKey::End)
    )
}

fn menu_row(
    entry: MenuEntry,
    palette: Palette,
    active: impl Fn() -> bool + 'static,
) -> impl IntoView {
    let title = entry.title.clone();
    let selected = entry.selected.clone();
    let enabled = entry.enabled.clone();
    let click = entry.clone();
    let key_entry = entry.clone();
    let icon = entry.icon;
    PrimaryPointerView::new(
        h_stack((
            svg(icon).style(|s| s.size(16.0, 16.0).flex_shrink(0.0)),
            label(move || title()).style(|s| {
                s.min_width(0.0)
                    .flex_grow(1.0)
                    .text_ellipsis()
                    .selectable(false)
            }),
            label(move || if selected() { "✓" } else { "" })
                .style(|s| s.width(16.0).selectable(false)),
        ))
        .style(|s| rtl_row(s).width_full().items_center().gap(8.0)),
        move |_| click.invoke(),
    )
    .keyboard_navigable()
    .on_event(EventListener::KeyDown, move |event| {
        if popover_handle_escape(event) {
            return EventPropagation::Stop;
        }
        if is_keyboard_activation(event) {
            key_entry.invoke();
            EventPropagation::Stop
        } else {
            EventPropagation::Continue
        }
    })
    .style(move |s| {
        s.width_full()
            .height(MENU_ROW_HEIGHT)
            .flex_shrink(0.0)
            .padding_horiz(8.0)
            .font_size(crate::ui::FONT_BODY as f32)
            .items_center()
            .border_radius(4.0)
            .color(if !enabled() {
                palette.muted
            } else if entry.danger {
                palette.danger
            } else {
                palette.ink
            })
            .background(if active() && enabled() {
                palette.accent_soft
            } else {
                palette.paper
            })
            .hover(|s| s.background(palette.accent_soft))
    })
}

pub(crate) fn menu_surface(content: impl IntoView + 'static, palette: Palette) -> impl IntoView {
    container(content).style(move |s| {
        s.width_full()
            .min_width(0.0)
            .padding(8.0)
            .background(palette.paper)
            .color(palette.ink)
            .border(1.0)
            .border_color(palette.divider)
            .border_radius(8.0)
    })
}

pub(crate) fn menu(entries: Vec<MenuEntry>, palette: Palette) -> impl IntoView {
    let entries = Rc::new(entries);
    let ids = Rc::new(RefCell::new(Vec::<ViewId>::new()));
    let active = create_rw_signal(
        entries
            .iter()
            .position(|entry| (entry.enabled)())
            .unwrap_or(0),
    );
    let rows = entries
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, entry)| {
            let key_entries = entries.clone();
            let key_ids = ids.clone();
            let row = menu_row(entry, palette, move || active.get() == index)
                .on_event_cont(EventListener::FocusGained, move |_| active.set(index))
                .on_event(EventListener::KeyDown, move |event| {
                    let Event::KeyDown(key) = event else {
                        return EventPropagation::Continue;
                    };
                    if let Some(next) =
                        menu_key(&key.key.logical_key, &key_entries, active.get_untracked())
                    {
                        active.set(next);
                        if let Some(id) = key_ids.borrow().get(next) {
                            id.request_focus();
                        }
                        EventPropagation::Stop
                    } else if is_menu_navigation(&key.key.logical_key) {
                        EventPropagation::Stop
                    } else {
                        EventPropagation::Continue
                    }
                });
            ids.borrow_mut().push(row.id());
            row
        })
        .collect::<Vec<_>>();
    let count = entries.len().min(MENU_VISIBLE_ROWS);
    let scroll_needed = entries.len() > MENU_VISIBLE_ROWS;
    let panel = menu_surface(
        scroll(v_stack_from_iter(rows).style(|s| s.width_full()))
            .scroll_to_view(move || ids.borrow().get(active.get()).copied())
            .scroll_style(move |s| s.hide_bars(!scroll_needed))
            .style(move |s| {
                s.width_full()
                    .height(count as f64 * MENU_ROW_HEIGHT)
                    .min_height(0.0)
                    .flex_shrink(1.0)
            }),
        palette,
    )
    .keyboard_navigable()
    .on_event(EventListener::KeyDown, move |event| {
        if popover_handle_escape(event) {
            return EventPropagation::Stop;
        }
        let Event::KeyDown(key) = event else {
            return EventPropagation::Continue;
        };
        if let Some(next) = menu_key(&key.key.logical_key, &entries, active.get_untracked()) {
            active.set(next);
            EventPropagation::Stop
        } else if is_menu_navigation(&key.key.logical_key) {
            EventPropagation::Stop
        } else if is_keyboard_activation(event) {
            if let Some(entry) = entries.get(active.get_untracked()) {
                entry.invoke();
            }
            EventPropagation::Stop
        } else {
            EventPropagation::Continue
        }
    });
    // Queue focus in the same overlay update; no timer can race fast navigation
    // or return focus to a menu that has already been replaced.
    panel.id().request_focus();
    panel
}

fn context_menu_shortcut(event: &Event) -> bool {
    matches!(event, Event::KeyDown(key) if
        key.key.logical_key == Key::Named(NamedKey::ContextMenu)
        || (key.key.logical_key == Key::Named(NamedKey::F10)
            && key.modifiers == floem::keyboard::Modifiers::SHIFT))
}

/// Attach a shared context menu without replacing the trigger's own input or
/// activation handlers. Entries are captured at opening, so actions keep their target.
pub(crate) fn context_menu_view(
    trigger: impl IntoView + 'static,
    palette: Palette,
    entries: impl Fn() -> Vec<MenuEntry> + 'static,
) -> impl IntoView {
    let open = create_rw_signal(false);
    let point = create_rw_signal(Point::ZERO);
    let height = create_rw_signal(0.0);
    let current = create_rw_signal(Vec::<MenuEntry>::new());
    let entries: Rc<dyn Fn() -> Vec<MenuEntry>> = Rc::new(entries);
    let show: Rc<dyn Fn(Point)> = Rc::new(move |position| {
        let entries = entries();
        if entries.is_empty() {
            return;
        }
        height.set(entries.len().min(MENU_VISIBLE_ROWS) as f64 * MENU_ROW_HEIGHT + 18.0);
        current.set(entries);
        point.set(position);
        open.set(true);
    });
    let pointer_show = show.clone();
    let keyboard_show = show;
    let trigger = trigger
        .into_view()
        .on_event(EventListener::PointerDown, move |event| {
            if let Event::PointerDown(pointer) = event
                && pointer.button.is_secondary()
            {
                pointer_show(pointer.pos);
                EventPropagation::Stop
            } else {
                EventPropagation::Continue
            }
        })
        .on_event(EventListener::KeyDown, move |event| {
            if context_menu_shortcut(event) {
                keyboard_show(Point::new(8.0, 24.0));
                EventPropagation::Stop
            } else {
                EventPropagation::Continue
            }
        });
    point_popover(trigger, open, point, MENU_WIDTH, height, move || {
        menu(current.get_untracked(), palette)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn menu_navigation_skips_unavailable_and_wraps() {
        let entry = |enabled| MenuEntry::action(ICON_NOTE, String::new, move || enabled, || {});
        let entries = vec![entry(true), entry(false), entry(true)];
        assert_eq!(menu_next(&entries, 0, false), Some(2));
        assert_eq!(menu_next(&entries, 0, true), Some(2));
        assert_eq!(menu_next(&entries, 2, false), Some(0));
        assert_eq!(menu_next(&[entry(false)], 0, false), None);
        assert_eq!(menu_next(&[], 0, false), None);
        assert_eq!(menu_key(&Key::Named(NamedKey::Home), &entries, 2), Some(0));
        assert_eq!(menu_key(&Key::Named(NamedKey::End), &entries, 0), Some(2));
    }
    #[test]
    fn menu_does_not_invoke_unavailable_action() {
        let calls = Rc::new(RefCell::new(0));
        let action_calls = calls.clone();
        MenuEntry::action(
            ICON_NOTE,
            String::new,
            || false,
            move || *action_calls.borrow_mut() += 1,
        )
        .invoke();
        assert_eq!(*calls.borrow(), 0);
    }

    #[test]
    fn menu_selection_callback_runs_once_without_dismissing_submenu() {
        let calls = Rc::new(RefCell::new(0));
        let action_calls = calls.clone();
        let entry = MenuEntry::action(
            ICON_NOTE,
            String::new,
            || true,
            move || *action_calls.borrow_mut() += 1,
        )
        .keep_open();
        assert!(entry.keep_open);
        entry.invoke();
        assert_eq!(*calls.borrow(), 1);
    }
}
