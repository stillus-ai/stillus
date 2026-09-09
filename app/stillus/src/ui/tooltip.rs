// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

use super::*;
use floem::kurbo::{Rect, Size};
use floem::text::{Attrs, AttrsList, FamilyOwned, TextLayout};
use std::cell::Cell;

thread_local! {
    static TOOLTIPS: RefCell<Vec<(std::rc::Weak<RefCell<Option<ViewId>>>, std::rc::Weak<Cell<u64>>)>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn close_button_tooltips() {
    TOOLTIPS.with(|tooltips| {
        tooltips.borrow_mut().retain(|(weak, generation)| {
            let Some(overlay) = weak.upgrade() else {
                return false;
            };
            if let Some(generation) = generation.upgrade() {
                generation.set(generation.get().wrapping_add(1));
            }
            close(&overlay);
            true
        });
    });
}

fn close(overlay: &RefCell<Option<ViewId>>) {
    if let Some(id) = overlay.borrow_mut().take() {
        remove_overlay(id);
    }
}

fn position(anchor: Rect, size: Size, window: Size) -> Point {
    let x = anchor
        .x0
        .clamp(8.0, (window.width - size.width - 8.0).max(8.0));
    let below = anchor.y1 + 6.0;
    let y = if below + size.height <= window.height - 8.0 {
        below
    } else {
        (anchor.y0 - 6.0 - size.height).max(8.0)
    };
    Point::new(x, y)
}

/// Hover and keyboard hints share placement based on the whole anchor bounds.
pub(crate) fn anchored_tooltip(
    child: impl IntoView + 'static,
    title: Rc<dyn Fn() -> String>,
    palette: Palette,
) -> AnyView {
    let child = child.into_view();
    let id = child.id();
    let origin = Rc::new(Cell::new(Point::ZERO));
    let size = Rc::new(Cell::new(Size::ZERO));
    let overlay = Rc::new(RefCell::new(None));
    let generation = Rc::new(Cell::new(0_u64));
    let pointer_down = Rc::new(Cell::new(false));
    TOOLTIPS.with(|tooltips| {
        tooltips
            .borrow_mut()
            .push((Rc::downgrade(&overlay), Rc::downgrade(&generation)))
    });
    let effect_title = title.clone();
    let show: Rc<dyn Fn()> = {
        let overlay = overlay.clone();
        let origin = origin.clone();
        let size = size.clone();
        Rc::new(move || {
            if overlay.borrow().is_some() || id.is_hidden_recursive() {
                return;
            }
            let text = title();
            // A value hint may disappear as its owner is being updated.
            if text.trim().is_empty() {
                return;
            }
            let mut root = id;
            while let Some(parent) = root.parent() {
                root = parent;
            }
            let window = root.get_size().unwrap_or(Size::new(960.0, 600.0));
            let width_limit = (window.width - 16.0).clamp(1.0, 320.0);
            let mut layout = TextLayout::new();
            layout.set_text(
                &text,
                AttrsList::new(
                    Attrs::new()
                        .family(&[FamilyOwned::Name(UI_FONT_FAMILY.to_owned())])
                        .font_size(FONT_CAPTION as f32),
                ),
            );
            layout.set_size((width_limit - 20.0).max(1.0) as f32, f32::INFINITY);
            let measured = layout.size();
            let width = (measured.width + 20.0).min(width_limit);
            let anchor = Rect::from_origin_size(origin.get(), size.get());
            let available = (window.height - anchor.y1 - 14.0)
                .max(anchor.y0 - 14.0)
                .max(1.0);
            let height = (measured.height + 14.0).min(available);
            let point = position(anchor, Size::new(width, height), window);
            close_button_tooltips();
            let title = title.clone();
            *overlay.borrow_mut() = Some(add_overlay(point, move |_| {
                scroll(label(move || title()).style(move |s| {
                    s.width((width - 20.0).max(1.0))
                        .font_family(UI_FONT_FAMILY.to_owned())
                        .font_size(FONT_CAPTION)
                }))
                .scroll_style(|s| s.hide_bars(true))
                .pointer_events(|| false)
                .style(move |s| {
                    s.width(width)
                        .height(height)
                        .padding_horiz(9.0)
                        .padding_vert(6.0)
                        .background(Color::rgb8(28, 33, 40))
                        .color(palette.sidebar_ink)
                        .border(1.0)
                        .border_color(palette.sidebar_border)
                        .border_radius(5.0)
                        .selectable(false)
                })
            }));
        })
    };
    let refresh_show = show.clone();
    let refresh_overlay = overlay.clone();
    create_effect(move |previous: Option<String>| {
        let text = effect_title();
        if previous.as_ref().is_some_and(|previous| previous != &text)
            && refresh_overlay.borrow().is_some()
        {
            close(&refresh_overlay);
            refresh_show();
        }
        text
    });
    let move_origin = origin;
    let resize_size = size;
    let move_overlay = overlay.clone();
    let resize_overlay = overlay.clone();
    let enter_show = show.clone();
    let enter_generation = generation.clone();
    let leave_generation = generation.clone();
    let leave_overlay = overlay.clone();
    let down_overlay = overlay.clone();
    let down_generation = generation.clone();
    let down_flag = pointer_down.clone();
    let focus_overlay = overlay.clone();
    let focus_generation = generation.clone();
    let window_generation = generation.clone();
    let window_overlay = overlay.clone();
    child
        .on_move(move |point| {
            move_origin.set(point);
            close(&move_overlay);
        })
        .on_resize(move |rect| {
            resize_size.set(rect.size());
            close(&resize_overlay);
        })
        .on_event_cont(EventListener::PointerEnter, move |_| {
            let token = enter_generation.get().wrapping_add(1);
            enter_generation.set(token);
            let generation = enter_generation.clone();
            let show = enter_show.clone();
            exec_after(Duration::from_millis(400), move |_| {
                if generation.get() == token {
                    show();
                }
            });
        })
        .on_event_cont(EventListener::PointerLeave, move |_| {
            leave_generation.set(leave_generation.get().wrapping_add(1));
            close(&leave_overlay);
        })
        .on_event_cont(EventListener::PointerDown, move |_| {
            down_generation.set(down_generation.get().wrapping_add(1));
            close(&down_overlay);
            down_flag.set(true);
            let flag = down_flag.clone();
            exec_after(Duration::from_millis(50), move |_| flag.set(false));
        })
        .on_event_cont(EventListener::FocusGained, move |_| {
            if !pointer_down.get() && !button_focus_is_pointer() {
                show();
            }
        })
        .on_event_cont(EventListener::FocusLost, move |_| {
            focus_generation.set(focus_generation.get().wrapping_add(1));
            close(&focus_overlay);
        })
        .on_event_cont(EventListener::WindowLostFocus, move |_| {
            window_generation.set(window_generation.get().wrapping_add(1));
            close(&window_overlay);
        })
        .on_cleanup(move || {
            generation.set(generation.get().wrapping_add(1));
            close(&overlay);
        })
        .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_close_invalidates_pending_hover_before_an_overlay_exists() {
        let overlay = Rc::new(RefCell::new(None));
        let generation = Rc::new(Cell::new(7_u64));
        TOOLTIPS.with(|tooltips| {
            tooltips
                .borrow_mut()
                .push((Rc::downgrade(&overlay), Rc::downgrade(&generation)));
        });
        close_button_tooltips();
        assert_ne!(
            generation.get(),
            7,
            "the pending hover must no longer be eligible to show"
        );
        assert!(overlay.borrow().is_none());
    }

    #[test]
    fn tooltip_uses_anchor_edges_and_flips_without_overlap() {
        let window = Size::new(960.0, 600.0);
        let size = Size::new(180.0, 32.0);
        let middle = Rect::new(100.0, 100.0, 220.0, 140.0);
        assert_eq!(position(middle, size, window), Point::new(100.0, 146.0));
        let edge = Rect::new(920.0, 560.0, 952.0, 592.0);
        let point = position(edge, size, window);
        assert!(point.x + size.width <= window.width - 8.0);
        assert_eq!(point.y + size.height, edge.y0 - 6.0);
    }
}
