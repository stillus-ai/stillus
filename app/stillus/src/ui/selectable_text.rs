// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

//! Read-only rich text with native selection; it never edits the source.
use super::*;
use floem::context::{ComputeLayoutCx, EventCx, PaintCx, UpdateCx};
use floem::kurbo::Rect;
use floem::text::{Cursor, TextLayout, Wrap};
use floem_renderer::Renderer;
use std::{any::Any, ops::Range};

pub(crate) fn selectable_rich_text(
    content: impl Fn() -> (String, TextLayout) + 'static,
    palette: Palette,
    on_click: Option<Rc<dyn Fn()>>,
) -> AnyView {
    let id = ViewId::new();
    let (text, mut layout) = content();
    layout.set_wrap(Wrap::WordOrGlyph);
    let rendered = create_rw_signal(layout.clone());
    let child = floem::views::rich_text(move || rendered.get())
        .pointer_events(|| false)
        .style(|s| s.width_full().min_width(0.0));
    id.add_child(Box::new(child));
    for (listener, focused) in [
        (EventListener::FocusGained, true),
        (EventListener::FocusLost, false),
    ] {
        id.add_event_listener(
            listener,
            Box::new(move |_| {
                id.update_state(focused);
                EventPropagation::Continue
            }),
        );
    }
    create_effect(move |_| id.update_state(content()));
    SelectableText {
        id,
        text,
        layout,
        rendered,
        anchor: 0,
        extent: 0,
        pressed: None,
        focused: false,
        palette,
        on_click,
    }
    .keyboard_navigable()
    .style(|s| s.width_full().min_width(0.0).cursor(CursorStyle::Text))
    .into_any()
}

struct SelectableText {
    id: ViewId,
    text: String,
    layout: TextLayout,
    rendered: RwSignal<TextLayout>,
    anchor: usize,
    extent: usize,
    pressed: Option<Point>,
    focused: bool,
    palette: Palette,
    on_click: Option<Rc<dyn Fn()>>,
}

fn selection_range(text: &str, anchor: usize, extent: usize) -> Option<Range<usize>> {
    let range = anchor.min(extent)..anchor.max(extent);
    text.get(range.clone()).map(|_| range)
}

fn text_cursor(layout: &TextLayout, index: usize) -> Cursor {
    let ranges = layout.lines_range();
    let line = ranges
        .partition_point(|range| range.start <= index)
        .saturating_sub(1);
    Cursor::new(
        line,
        index.saturating_sub(ranges.get(line).map_or(0, |r| r.start)),
    )
}

impl SelectableText {
    fn hit(&self, point: Point) -> usize {
        if point.y < 0.0 {
            return 0;
        }
        if point.y >= self.layout.size().height {
            return self.text.len();
        }
        self.layout
            .hit(point.x as f32, point.y as f32)
            .and_then(|cursor| {
                self.layout
                    .lines_range()
                    .get(cursor.line)
                    .map(|line| line.start + cursor.index)
            })
            .filter(|index| self.text.is_char_boundary(*index))
            .unwrap_or(self.text.len())
    }
}

impl View for SelectableText {
    fn id(&self) -> ViewId {
        self.id
    }

    fn update(&mut self, _cx: &mut UpdateCx, state: Box<dyn Any>) {
        if let Some(focused) = state.downcast_ref::<bool>() {
            self.focused = *focused;
            if !focused {
                self.pressed = None;
                self.id.clear_active();
            }
            self.id.request_paint();
            return;
        }
        if let Ok(content) = state.downcast::<(String, TextLayout)>() {
            let (text, mut layout) = *content;
            if text != self.text {
                self.anchor = 0;
                self.extent = 0;
                self.pressed = None;
            }
            layout.set_wrap(Wrap::WordOrGlyph);
            self.text = text;
            self.rendered.set(layout.clone());
            self.layout = layout;
            self.id.request_layout();
        }
    }

    fn compute_layout(&mut self, cx: &mut ComputeLayoutCx) -> Option<Rect> {
        if let Some(size) = self.id.get_size() {
            self.layout.set_size(size.width.max(1.0) as f32, f32::MAX);
        }
        let mut rect = None;
        for child in self.id.children() {
            if let Some(child_rect) = cx.compute_view_layout(child) {
                rect = Some(rect.map_or(child_rect, |rect: Rect| rect.union(child_rect)));
            }
        }
        rect
    }

    fn event_before_children(&mut self, _cx: &mut EventCx, event: &Event) -> EventPropagation {
        match event {
            Event::PointerDown(pointer) if pointer.button.is_primary() => {
                self.extent = self.hit(pointer.pos);
                if !pointer.modifiers.shift() {
                    self.anchor = self.extent;
                }
                self.pressed = Some(pointer.pos);
                self.id.request_focus();
                self.id.request_active();
            }
            Event::PointerMove(pointer) if self.pressed.is_some() => {
                self.extent = self.hit(pointer.pos);
            }
            Event::PointerUp(pointer) if pointer.button.is_primary() && self.pressed.is_some() => {
                let start = self.pressed.take().expect("pressed pointer");
                self.extent = self.hit(pointer.pos);
                self.id.clear_active();
                if self.anchor == self.extent && start.distance(pointer.pos) < 3.0 {
                    if let Some(action) = &self.on_click {
                        action();
                    }
                }
            }
            Event::KeyDown(key) => {
                let shortcut =
                    (key.modifiers.control() || key.modifiers.meta()) && !key.modifiers.alt();
                match &key.key.logical_key {
                    Key::Character(ch) if shortcut && ch.eq_ignore_ascii_case("c") => {
                        if let Some(range) = selection_range(&self.text, self.anchor, self.extent)
                            && !range.is_empty()
                        {
                            let _ = floem::Clipboard::set_contents(self.text[range].to_owned());
                        }
                    }
                    Key::Character(ch) if shortcut && ch.eq_ignore_ascii_case("a") => {
                        self.anchor = 0;
                        self.extent = self.text.len();
                    }
                    Key::Named(NamedKey::Escape) if self.anchor != self.extent => {
                        self.extent = self.anchor;
                    }
                    Key::Named(NamedKey::Enter) if self.on_click.is_some() => {
                        if let Some(action) = &self.on_click {
                            action();
                        }
                    }
                    _ => return EventPropagation::Continue,
                }
            }
            Event::FocusGained => self.focused = true,
            Event::FocusLost => {
                self.focused = false;
                self.pressed = None;
                self.id.clear_active();
            }
            _ => return EventPropagation::Continue,
        }
        self.id.request_paint();
        EventPropagation::Stop
    }

    fn paint(&mut self, cx: &mut PaintCx) {
        if self.focused
            && let Some(range) = selection_range(&self.text, self.anchor, self.extent)
        {
            let start = text_cursor(&self.layout, range.start);
            let end = text_cursor(&self.layout, range.end);
            for run in self.layout.layout_runs() {
                if let Some((x, width)) = run.highlight(start, end) {
                    let rect = Rect::new(
                        x as f64,
                        run.line_top as f64,
                        (x + width) as f64,
                        (run.line_top + run.line_height) as f64,
                    );
                    cx.fill(&rect, self.palette.accent_soft, 0.0);
                }
            }
        }
        cx.paint_children(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_handles_reversed_unicode_ranges_without_splitting_characters() {
        let text = "RSS: Привет 🌍\nnext";
        let start = "RSS: ".len();
        let end = "RSS: Привет 🌍".len();
        assert_eq!(
            &text[selection_range(text, end, start).unwrap()],
            "Привет 🌍"
        );
        assert!(selection_range(text, start + 1, end).is_none());
        assert!(selection_range(text, 0, text.len() + 1).is_none());
    }
}
