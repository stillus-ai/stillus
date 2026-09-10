// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

use super::*;
use floem::context::ComputeLayoutCx;
use floem::kurbo::Rect;
use std::cell::Cell;

fn cycle_index(current: Option<usize>, length: usize, backwards: bool) -> Option<usize> {
    if length == 0 {
        return None;
    }
    Some(match current {
        None => {
            if backwards {
                length - 1
            } else {
                0
            }
        }
        Some(index) if backwards => (index + length - 1) % length,
        Some(index) => (index + 1) % length,
    })
}

fn focused_descendant(id: ViewId, state: &floem::AppState) -> Option<ViewId> {
    if state.is_focused(&id) {
        return Some(id);
    }
    id.children()
        .into_iter()
        .find_map(|child| focused_descendant(child, state))
}

/// Capture the opener before a form requests focus. Modal forms also own Tab;
/// inline forms leave normal traversal intact. No application state is retained.
pub(crate) fn form_focus_scope(
    child: impl IntoView + 'static,
    members: Vec<ViewId>,
    active: impl Fn() -> bool + 'static,
    trap: bool,
) -> impl IntoView {
    let id = ViewId::new();
    id.add_child(Box::new(child.into_view()));
    let active = floem::reactive::create_memo(move |_| active());
    create_effect(move |_| {
        active.get();
        id.request_layout();
    });
    if trap {
        let members = Rc::new(members);
        for target in members.iter().copied() {
            let members = members.clone();
            // Floem dispatches keys directly to the focused view, not through
            // its ancestors. Install traversal on each actual focus target.
            target.add_event_listener(
                EventListener::KeyDown,
                Box::new(move |event| {
                    if active.get_untracked()
                        && let Event::KeyDown(key) = event
                        && key.key.logical_key == Key::Named(NamedKey::Tab)
                    {
                        let visible = members
                            .iter()
                            .copied()
                            .filter(|id| !id.is_hidden_recursive())
                            .collect::<Vec<_>>();
                        let current = visible.iter().position(|id| *id == target);
                        if let Some(next) =
                            cycle_index(current, visible.len(), key.modifiers.shift())
                        {
                            visible[next].request_focus();
                        }
                        EventPropagation::Stop
                    } else {
                        EventPropagation::Continue
                    }
                }),
            );
        }
    }
    let restore = Rc::new(Cell::new(None::<ViewId>));
    let cleanup = restore.clone();
    let was_active = Rc::new(Cell::new(false));
    let reset_active = was_active.clone();
    let close_restore = restore.clone();
    create_effect(move |_| {
        if !active.get() {
            reset_active.set(false);
            if let Some(id) = close_restore.take()
                && !id.is_hidden_recursive()
            {
                id.request_focus();
            }
        }
    });
    FocusScope {
        id,
        active,
        was_active,
        restore,
    }
    .on_cleanup(move || {
        if let Some(id) = cleanup.take()
            && !id.is_hidden_recursive()
        {
            id.request_focus();
        }
    })
}

struct FocusScope {
    id: ViewId,
    active: floem::reactive::Memo<bool>,
    was_active: Rc<Cell<bool>>,
    restore: Rc<Cell<Option<ViewId>>>,
}

impl View for FocusScope {
    fn id(&self) -> ViewId {
        self.id
    }

    fn compute_layout(&mut self, cx: &mut ComputeLayoutCx) -> Option<Rect> {
        let active = self.active.get_untracked();
        if active && !self.was_active.get() {
            let mut root = self.id;
            while let Some(parent) = root.parent() {
                root = parent;
            }
            let focused = focused_descendant(root, cx.app_state());
            // A field may already have focus after a queued open. Never restore
            // into this form when it closes.
            let inside = focused_descendant(self.id, cx.app_state());
            self.restore.set(focused.filter(|id| Some(*id) != inside));
        }
        self.was_active.set(active);
        let mut bounds: Option<Rect> = None;
        for child in self.id.children() {
            if let Some(rect) = cx.compute_view_layout(child) {
                bounds = Some(bounds.map_or(rect, |bounds| bounds.union(rect)));
            }
        }
        bounds
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tab_visits_fields_and_actions_in_both_directions() {
        let mut current = Some(0);
        for expected in [1, 2, 3, 0] {
            current = cycle_index(current, 4, false);
            assert_eq!(current, Some(expected));
        }
        for expected in [3, 2, 1, 0] {
            current = cycle_index(current, 4, true);
            assert_eq!(current, Some(expected));
        }
        assert_eq!(cycle_index(None, 4, false), Some(0));
        assert_eq!(cycle_index(None, 4, true), Some(3));
        assert_eq!(cycle_index(None, 0, false), None);
    }
}
