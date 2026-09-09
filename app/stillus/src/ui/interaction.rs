// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

use super::*;

pub(crate) fn is_primary_pointer_down(event: &Event) -> bool {
    matches!(event, Event::PointerDown(pointer) if pointer.button.is_primary())
}

pub(crate) struct PrimaryPointerView {
    id: ViewId,
    on_press: Box<dyn Fn(&PointerInputEvent)>,
    capture_pointer: bool,
}

impl PrimaryPointerView {
    pub(crate) fn new(
        child: impl IntoView,
        on_press: impl Fn(&PointerInputEvent) + 'static,
    ) -> Self {
        let id = ViewId::new();
        id.add_child(Box::new(child.into_view()));
        Self {
            id,
            on_press: Box::new(on_press),
            capture_pointer: false,
        }
    }

    /// Route every pointer event to this view until the primary button is
    /// released, even when the pointer leaves its bounds. Pointer drags need
    /// this so the release always ends the drag instead of leaving the
    /// selection following later hover movement.
    pub(crate) fn capture_pointer(mut self) -> Self {
        self.capture_pointer = true;
        self
    }
}

impl View for PrimaryPointerView {
    fn id(&self) -> ViewId {
        self.id
    }

    fn event_before_children(
        &mut self,
        _cx: &mut floem::context::EventCx,
        event: &Event,
    ) -> EventPropagation {
        if is_primary_pointer_down(event) {
            let Event::PointerDown(pointer) = event else {
                unreachable!("primary pointer-down predicate only accepts PointerDown")
            };
            self.id.request_focus();
            if self.capture_pointer {
                self.id.request_active();
            }
            (self.on_press)(pointer);
            EventPropagation::Continue
        } else {
            EventPropagation::Continue
        }
    }
}

pub(crate) fn is_keyboard_activation(event: &Event) -> bool {
    let Event::KeyDown(key_event) = event else {
        return false;
    };
    match &key_event.key.logical_key {
        Key::Named(NamedKey::Enter | NamedKey::Space) => true,
        Key::Character(character) => character == " ",
        _ => false,
    }
}

pub(crate) fn reliable_button<V>(child: V, on_press: impl Fn() + 'static) -> impl IntoView
where
    V: IntoView + 'static,
{
    let on_press: Rc<dyn Fn()> = Rc::new(on_press);
    let pointer_press = on_press.clone();
    PrimaryPointerView::new(child, move |_| pointer_press())
        .keyboard_navigable()
        .on_event(EventListener::KeyDown, move |event| {
            if is_keyboard_activation(event) {
                on_press();
                EventPropagation::Stop
            } else {
                EventPropagation::Continue
            }
        })
}

/// Selectable document/result/link content, not an action button. Action controls use button.rs.
pub(crate) fn selectable_row(
    child: impl IntoView + 'static,
    action: impl Fn() + 'static,
) -> impl IntoView {
    reliable_button(child, action)
}
