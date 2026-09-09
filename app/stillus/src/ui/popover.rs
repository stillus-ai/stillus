// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

use super::*;

pub(crate) enum AnchoredPopoverMessage {
    Open(bool),
}

pub(crate) struct AnchoredPopover {
    id: ViewId,
    overlay_ids: Rc<RefCell<Vec<ViewId>>>,
    open: RwSignal<bool>,
    content: Rc<dyn Fn() -> AnyView>,
    width: f64,
    gap: f64,
    align_start: bool,
    window_origin: Option<Point>,
}

pub(crate) fn anchored_popover<V, C, CV>(
    trigger: V,
    open: RwSignal<bool>,
    width: f64,
    gap: f64,
    align_start: bool,
    content: C,
) -> impl IntoView
where
    V: IntoView + 'static,
    C: Fn() -> CV + 'static,
    CV: IntoView + 'static,
{
    let id = ViewId::new();
    id.add_child(Box::new(trigger.into_view()));
    create_effect(move |_| {
        id.update_state(AnchoredPopoverMessage::Open(open.get()));
    });
    let overlay_ids = Rc::new(RefCell::new(Vec::new()));
    let cleanup_overlay_ids = overlay_ids.clone();
    AnchoredPopover {
        id,
        overlay_ids,
        open,
        content: Rc::new(move || content().into_any()),
        width,
        gap,
        align_start,
        window_origin: None,
    }
    .on_cleanup(move || {
        let overlay_ids = std::mem::take(&mut *cleanup_overlay_ids.borrow_mut());
        for overlay_id in overlay_ids.into_iter().rev() {
            remove_overlay(overlay_id);
        }
    })
}

impl AnchoredPopover {
    fn close_overlay(&mut self) {
        let overlay_ids = std::mem::take(&mut *self.overlay_ids.borrow_mut());
        for overlay_id in overlay_ids.into_iter().rev() {
            remove_overlay(overlay_id);
        }
    }
}

pub(crate) fn popover_left(
    origin: f64,
    trigger: f64,
    width: f64,
    window: f64,
    align_start: bool,
    rtl: bool,
) -> f64 {
    let left = if align_start != rtl {
        origin
    } else {
        origin + trigger - width
    };
    left.clamp(8.0, (window - width - 8.0).max(8.0))
}

impl View for AnchoredPopover {
    fn id(&self) -> ViewId {
        self.id
    }

    fn update(&mut self, _cx: &mut floem::context::UpdateCx, state: Box<dyn std::any::Any>) {
        let Ok(message) = state.downcast::<AnchoredPopoverMessage>() else {
            return;
        };
        match *message {
            AnchoredPopoverMessage::Open(false) => self.close_overlay(),
            AnchoredPopoverMessage::Open(true) => {
                if !self.overlay_ids.borrow().is_empty() {
                    return;
                }
                let Some(origin) = self.window_origin else {
                    self.open.set(false);
                    return;
                };
                let layout = self.id.get_layout().unwrap_or_default();
                let mut root = self.id;
                while let Some(parent) = root.parent() {
                    root = parent;
                }
                let window_width = root
                    .get_layout()
                    .map_or(860.0, |layout| f64::from(layout.size.width));
                let left = popover_left(
                    origin.x,
                    f64::from(layout.size.width),
                    self.width,
                    window_width,
                    self.align_start,
                    i18n::current().is_rtl(),
                );
                let top = origin.y + f64::from(layout.size.height) + self.gap;
                let content = self.content.clone();
                let dismiss_layer = add_overlay(Point::new(0.0, 0.0), move |_| {
                    empty()
                        .pointer_events(|| false)
                        .style(|style| style.absolute().size_full())
                });
                let card = add_overlay(Point::new(left, top), move |_| content());
                self.overlay_ids.borrow_mut().extend([dismiss_layer, card]);
            }
        }
    }

    fn compute_layout(
        &mut self,
        cx: &mut floem::context::ComputeLayoutCx,
    ) -> Option<floem::kurbo::Rect> {
        self.window_origin = Some(cx.window_origin());
        let mut layout_rect: Option<floem::kurbo::Rect> = None;
        for child in self.id.children() {
            if let Some(child_layout) = cx.compute_view_layout(child) {
                layout_rect =
                    Some(layout_rect.map_or(child_layout, |rect| rect.union(child_layout)));
            }
        }
        layout_rect
    }
}
