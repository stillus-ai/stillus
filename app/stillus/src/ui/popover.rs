// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

use super::*;
use floem::kurbo::{Rect, Size};

#[derive(Clone)]
struct OpenLayer {
    owner: ViewId,
    anchor: ViewId,
    open: RwSignal<bool>,
    overlays: Rc<RefCell<Vec<ViewId>>>,
}

#[derive(Default)]
struct LayerStack {
    layers: Vec<OpenLayer>,
    escape_release: bool,
}

thread_local! {
    static LAYERS: RefCell<LayerStack> = RefCell::new(LayerStack::default());
}

fn remove_layers(overlays: &RefCell<Vec<ViewId>>) -> bool {
    let ids = std::mem::take(&mut *overlays.borrow_mut());
    let removed = !ids.is_empty();
    for id in ids.into_iter().rev() {
        remove_overlay(id);
    }
    removed
}

fn unregister(owner: ViewId) {
    LAYERS.with(|stack| {
        stack
            .borrow_mut()
            .layers
            .retain(|layer| layer.owner != owner)
    });
}

fn dismiss(layer: OpenLayer, restore_focus: bool) {
    unregister(layer.owner);
    remove_layers(&layer.overlays);
    if layer.open.try_get_untracked().is_some() {
        layer.open.set(false);
        if restore_focus && !layer.anchor.is_hidden_recursive() {
            layer.anchor.request_focus();
        }
    }
}

/// Close the uppermost shared layer and return focus to its anchor.
pub(crate) fn popover_close_top() -> bool {
    let layer = LAYERS.with(|stack| stack.borrow_mut().layers.pop());
    if let Some(layer) = layer {
        dismiss(layer, true);
        true
    } else {
        false
    }
}

/// Screen changes dispose their overlays without stealing the new screen's focus.
pub(crate) fn popover_close_all() {
    close_button_tooltips();
    let layers = LAYERS.with(|stack| std::mem::take(&mut stack.borrow_mut().layers));
    for layer in layers.into_iter().rev() {
        dismiss(layer, false);
    }
}

/// Form controls that consume Escape call this before their own Escape action.
pub(crate) fn popover_close_top_on_escape() -> bool {
    let awaiting_release = LAYERS.with(|stack| stack.borrow().escape_release);
    if awaiting_release {
        return true;
    }
    if popover_close_top() {
        LAYERS.with(|stack| stack.borrow_mut().escape_release = true);
        true
    } else {
        false
    }
}

/// Call before screen-level key handlers, for both KeyDown and KeyUp. The release
/// belonging to a dismissed layer must not close its settings/search owner.
pub(crate) fn popover_handle_escape(event: &Event) -> bool {
    match event {
        Event::KeyDown(key) if key.key.logical_key == Key::Named(NamedKey::Escape) => {
            popover_close_top_on_escape()
        }
        Event::KeyUp(key) if key.key.logical_key == Key::Named(NamedKey::Escape) => {
            let consumed =
                LAYERS.with(|stack| std::mem::take(&mut stack.borrow_mut().escape_release));
            consumed || popover_close_top()
        }
        _ => false,
    }
}

pub(crate) enum AnchoredPopoverMessage {
    Open(bool),
}

pub(crate) struct AnchoredPopover {
    id: ViewId,
    anchor: ViewId,
    overlay_ids: Rc<RefCell<Vec<ViewId>>>,
    open: RwSignal<bool>,
    content: Rc<dyn Fn() -> AnyView>,
    width: f64,
    gap: f64,
    align_start: bool,
    point: Option<(RwSignal<Point>, RwSignal<f64>)>,
    window_origin: Option<Point>,
    geometry: RwSignal<(Rect, Size)>,
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
    popover(trigger, open, width, gap, align_start, None, content)
}

/// A cursor-positioned menu uses the same dismissal and owner contract as an
/// anchored popover. The point is relative to the trigger's window origin.
pub(crate) fn point_popover<V, C, CV>(
    trigger: V,
    open: RwSignal<bool>,
    point: RwSignal<Point>,
    width: f64,
    height: RwSignal<f64>,
    content: C,
) -> impl IntoView
where
    V: IntoView + 'static,
    C: Fn() -> CV + 'static,
    CV: IntoView + 'static,
{
    popover(
        trigger,
        open,
        width,
        8.0,
        true,
        Some((point, height)),
        content,
    )
}

fn popover<V, C, CV>(
    trigger: V,
    open: RwSignal<bool>,
    width: f64,
    gap: f64,
    align_start: bool,
    point: Option<(RwSignal<Point>, RwSignal<f64>)>,
    content: C,
) -> impl IntoView
where
    V: IntoView + 'static,
    C: Fn() -> CV + 'static,
    CV: IntoView + 'static,
{
    let id = ViewId::new();
    let trigger = trigger.into_view();
    let anchor = button_focus_target(trigger.id());
    id.add_child(Box::new(trigger));
    create_effect(move |_| {
        id.update_state(AnchoredPopoverMessage::Open(open.get()));
    });
    let overlay_ids = Rc::new(RefCell::new(Vec::new()));
    let cleanup_overlay_ids = overlay_ids.clone();
    AnchoredPopover {
        id,
        anchor,
        overlay_ids,
        open,
        content: Rc::new(move || content().into_any()),
        width,
        gap,
        align_start,
        point,
        window_origin: None,
        geometry: create_rw_signal((Rect::ZERO, Size::ZERO)),
    }
    .on_cleanup(move || {
        unregister(id);
        remove_layers(&cleanup_overlay_ids);
    })
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

pub(crate) fn menu_position(
    point: Point,
    width: f64,
    height: f64,
    window_width: f64,
    window_height: f64,
) -> Point {
    Point::new(
        point.x.clamp(8.0, (window_width - width - 8.0).max(8.0)),
        point.y.clamp(8.0, (window_height - height - 8.0).max(8.0)),
    )
}

fn vertical_placement(anchor: Rect, desired: f64, window: f64, gap: f64) -> (f64, f64) {
    let below = (window - anchor.y1 - gap - 8.0).max(1.0);
    let above = (anchor.y0 - gap - 8.0).max(1.0);
    if desired <= below || below >= above {
        (anchor.y1 + gap, desired.min(below).max(1.0))
    } else {
        let height = desired.min(above).max(1.0);
        ((anchor.y0 - gap - height).max(8.0), height)
    }
}

/// The backdrop keeps pointer capture through the release even though the card
/// disappears on press. Neither half of an outside click reaches the editor.
struct DismissLayer {
    id: ViewId,
    overlay: ViewId,
    layer: OpenLayer,
    pressed: bool,
}

/// Limit both drawing and pointer routing to the card's allocated rectangle.
/// Floem containers otherwise allow a tall child to paint beyond max_height.
struct PopoverClip {
    id: ViewId,
}

impl View for PopoverClip {
    fn id(&self) -> ViewId {
        self.id
    }

    fn paint(&mut self, cx: &mut floem::context::PaintCx) {
        let layout = self.id.get_layout().unwrap_or_default();
        let rect = floem::kurbo::Rect::new(
            0.0,
            0.0,
            f64::from(layout.size.width),
            f64::from(layout.size.height),
        );
        cx.save();
        cx.clip(&rect);
        cx.paint_children(self.id);
        cx.restore();
    }
}

impl View for DismissLayer {
    fn id(&self) -> ViewId {
        self.id
    }

    fn event_before_children(
        &mut self,
        _cx: &mut floem::context::EventCx,
        event: &Event,
    ) -> EventPropagation {
        match event {
            Event::PointerDown(_) => {
                self.pressed = true;
                self.id.request_active();
                self.layer
                    .overlays
                    .borrow_mut()
                    .retain(|id| *id != self.overlay);
                dismiss(self.layer.clone(), false);
                button_request_pointer_focus(self.layer.anchor);
                EventPropagation::Stop
            }
            Event::PointerUp(_) => {
                if self.pressed {
                    remove_overlay(self.overlay);
                }
                EventPropagation::Stop
            }
            Event::PointerMove(_) | Event::PointerWheel(_) => EventPropagation::Stop,
            _ => EventPropagation::Continue,
        }
    }
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
            AnchoredPopoverMessage::Open(false) => {
                unregister(self.id);
                if remove_layers(&self.overlay_ids) && !self.anchor.is_hidden_recursive() {
                    self.anchor.request_focus();
                }
            }
            AnchoredPopoverMessage::Open(true) => {
                // A queued open can outlive the screen that requested it.
                if self.open.try_get_untracked() != Some(true)
                    || !self.overlay_ids.borrow().is_empty()
                {
                    return;
                }
                let Some(_) = self.window_origin else {
                    self.open.set(false);
                    return;
                };
                let geometry = self.geometry;
                let preferred_width = self.width;
                let gap = self.gap.max(8.0);
                let align_start = self.align_start;
                let point = self.point;
                let content = self.content.clone();
                let layer = OpenLayer {
                    owner: self.id,
                    anchor: self.anchor,
                    open: self.open,
                    overlays: self.overlay_ids.clone(),
                };
                let backdrop_layer = layer.clone();
                let dismiss_layer = add_overlay(Point::ZERO, move |overlay| {
                    DismissLayer {
                        id: ViewId::new(),
                        overlay,
                        layer: backdrop_layer,
                        pressed: false,
                    }
                    .style(move |s| {
                        let (_, window) = geometry.get();
                        s.width(window.width).height(window.height)
                    })
                });
                let card = add_overlay(Point::ZERO, move |overlay| {
                    let measured_height = create_rw_signal(1.0_f64);
                    let width = floem::reactive::create_memo(move |_| {
                        let (anchor, window) = geometry.get();
                        let desired = if preferred_width > 0.0 {
                            preferred_width
                        } else {
                            anchor.width()
                        };
                        desired.min((window.width - 16.0).max(1.0))
                    });
                    let bounds = floem::reactive::create_memo(move |_| {
                        let (anchor, window) = geometry.get();
                        let width = width.get();
                        let desired = measured_height.get();
                        let (top, height) = vertical_placement(anchor, desired, window.height, gap);
                        let left = popover_left(
                            anchor.x0,
                            anchor.width(),
                            width,
                            window.width,
                            align_start,
                            i18n::current().is_rtl(),
                        );
                        if let Some((point, _)) = point {
                            let height = desired.min((window.height - 16.0).max(1.0));
                            let pos = menu_position(
                                anchor.origin() + point.get().to_vec2(),
                                width,
                                height,
                                window.width,
                                window.height,
                            );
                            Rect::from_origin_size(pos, Size::new(width, height))
                        } else {
                            Rect::from_origin_size(Point::new(left, top), Size::new(width, height))
                        }
                    });
                    // Move the overlay itself: margins would make the transparent
                    // area above a bottom-anchored menu intercept outside clicks.
                    create_effect(move |_| overlay.update_state(bounds.get().origin()));
                    let id = ViewId::new();
                    id.set_children(vec![
                        scroll(
                            content()
                                .style(move |s| s.width(width.get()).flex_shrink(0.0))
                                .on_resize(move |rect| {
                                    if (measured_height.get_untracked() - rect.height()).abs() > 0.5
                                    {
                                        measured_height.set(rect.height());
                                    }
                                }),
                        )
                        .style(move |s| s.width(width.get()).height(bounds.get().height()))
                        .into_any(),
                    ]);
                    PopoverClip { id }
                        .style(move |s| {
                            let bounds = bounds.get();
                            s.width(bounds.width()).height(bounds.height())
                        })
                        .on_event(EventListener::KeyDown, |event| {
                            if popover_handle_escape(event) {
                                EventPropagation::Stop
                            } else {
                                EventPropagation::Continue
                            }
                        })
                });
                self.overlay_ids.borrow_mut().extend([dismiss_layer, card]);
                LAYERS.with(|stack| stack.borrow_mut().layers.push(layer));
            }
        }
    }

    fn compute_layout(
        &mut self,
        cx: &mut floem::context::ComputeLayoutCx,
    ) -> Option<floem::kurbo::Rect> {
        self.window_origin = Some(cx.window_origin());
        let layout = self.id.get_layout().unwrap_or_default();
        let mut root = self.id;
        while let Some(parent) = root.parent() {
            root = parent;
        }
        let window = root.get_size().unwrap_or(Size::ZERO);
        let geometry = (
            Rect::from_origin_size(
                cx.window_origin(),
                Size::new(f64::from(layout.size.width), f64::from(layout.size.height)),
            ),
            window,
        );
        if self.geometry.get_untracked() != geometry {
            self.geometry.set(geometry);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchored_panel_flips_and_constrains_height() {
        assert_eq!(
            vertical_placement(Rect::new(20.0, 540.0, 260.0, 580.0), 274.0, 600.0, 8.0),
            (258.0, 274.0)
        );
        assert_eq!(
            vertical_placement(Rect::new(20.0, 40.0, 260.0, 80.0), 274.0, 600.0, 8.0),
            (88.0, 274.0)
        );
        let (top, height) =
            vertical_placement(Rect::new(20.0, 300.0, 260.0, 340.0), 900.0, 600.0, 8.0);
        assert_eq!((top, height), (8.0, 284.0));
    }

    #[test]
    fn context_menu_position_keeps_layout_and_hit_test_inside_window() {
        assert_eq!(
            menu_position(Point::new(950.0, 590.0), 248.0, 242.0, 960.0, 600.0),
            Point::new(704.0, 350.0)
        );
        assert_eq!(
            menu_position(Point::new(40.0, 70.0), 248.0, 114.0, 960.0, 600.0),
            Point::new(40.0, 70.0)
        );
    }

    fn layer(scope: floem::reactive::Scope) -> OpenLayer {
        OpenLayer {
            owner: ViewId::new(),
            anchor: ViewId::new(),
            open: scope.create_rw_signal(true),
            overlays: Rc::new(RefCell::new(Vec::new())),
        }
    }

    #[test]
    fn popover_dismisses_only_top_layer_and_latches_escape_repeat() {
        let scope = floem::reactive::Scope::new();
        let first = layer(scope);
        let second = layer(scope);
        LAYERS.with(|s| {
            *s.borrow_mut() = LayerStack {
                layers: vec![first.clone(), second.clone()],
                escape_release: false,
            }
        });
        assert!(popover_close_top_on_escape());
        assert!(first.open.get_untracked());
        assert!(!second.open.get_untracked());
        assert!(popover_close_top_on_escape());
        assert!(first.open.get_untracked());
        LAYERS.with(|s| s.borrow_mut().escape_release = false);
        popover_close_all();
        assert!(!first.open.get_untracked());
        scope.dispose();
    }

    #[test]
    fn popover_owner_cleanup_unregisters_stale_signals() {
        let scope = floem::reactive::Scope::new();
        let disposed = layer(scope);
        LAYERS.with(|s| {
            *s.borrow_mut() = LayerStack {
                layers: vec![disposed.clone()],
                escape_release: false,
            }
        });
        unregister(disposed.owner);
        scope.dispose();
        assert!(!popover_close_top());
    }
}
