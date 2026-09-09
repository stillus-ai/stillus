// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

//! A bounded secret editor. Only its owner retains the secret; the view keeps
//! cursor offsets and shapes a mask (or an explicitly revealed value) per paint.
use super::*;
use floem::context::{EventCx, PaintCx, UpdateCx};
use floem::kurbo::{Rect, Size};
use floem::text::{Attrs, AttrsList, FamilyOwned, TextLayout};
use floem_renderer::Renderer;
use std::{any::Any, ops::Range};
use zeroize::{Zeroize, Zeroizing};

pub(crate) fn replace_secret(
    value: &mut String,
    range: Range<usize>,
    insert: &str,
    maximum: usize,
) -> bool {
    if range.start > range.end
        || range.end > value.len()
        || !value.is_char_boundary(range.start)
        || !value.is_char_boundary(range.end)
        || insert.chars().any(char::is_control)
    {
        return false;
    }
    let length = value.len() - range.len() + insert.len();
    if length > maximum || length > value.capacity() {
        return false;
    }
    let mut next = Zeroizing::new(String::with_capacity(maximum));
    next.push_str(&value[..range.start]);
    next.push_str(insert);
    next.push_str(&value[range.end..]);
    value.zeroize();
    value.push_str(&next);
    true
}

#[derive(Default, Clone, Copy)]
struct Selection {
    anchor: usize,
    caret: usize,
}
impl Selection {
    fn range(self) -> Range<usize> {
        self.anchor.min(self.caret)..self.anchor.max(self.caret)
    }
    fn move_to(&mut self, offset: usize, extend: bool) {
        self.caret = offset;
        if !extend {
            self.anchor = offset;
        }
    }
    fn clamp(&mut self, value: &str) {
        for offset in [&mut self.anchor, &mut self.caret] {
            *offset = (*offset).min(value.len());
            while !value.is_char_boundary(*offset) {
                *offset -= 1;
            }
        }
    }
}

type ReadSecret = dyn Fn() -> Zeroizing<String>;
type ReplaceSecret = dyn Fn(Range<usize>, &str) -> bool;
type SecretCommand = dyn Fn(&Event) -> EventPropagation;

pub(crate) struct SecretInput {
    id: ViewId,
    read: Box<ReadSecret>,
    replace: Box<ReplaceSecret>,
    command: Box<SecretCommand>,
    pasted: Box<dyn Fn()>,
    trim_paste: bool,
    revision: RwSignal<u64>,
    last_revision: u64,
    own_revision: u64,
    revealed: Box<dyn Fn() -> bool>,
    enabled: Box<dyn Fn() -> bool>,
    hint: i18n::Key,
    palette: Palette,
    selection: Selection,
    scroll_x: f64,
    dragging: bool,
    composing: bool,
    focused: RwSignal<bool>,
    active_window: bool,
    blink: RwSignal<bool>,
    generation: RwSignal<u64>,
}

impl SecretInput {
    pub(crate) fn new(
        read: impl Fn() -> Zeroizing<String> + 'static,
        replace: impl Fn(Range<usize>, &str) -> bool + 'static,
        revision: RwSignal<u64>,
        hint: i18n::Key,
        palette: Palette,
    ) -> Self {
        let id = ViewId::new();
        create_effect(move |_| {
            let revision = revision.get();
            i18n::current();
            id.update_state(revision);
        });
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
        Self {
            id,
            read: Box::new(read),
            replace: Box::new(replace),
            command: Box::new(|_| EventPropagation::Continue),
            pasted: Box::new(|| {}),
            trim_paste: false,
            revision,
            last_revision: revision.get_untracked(),
            own_revision: revision.get_untracked(),
            revealed: Box::new(|| false),
            enabled: Box::new(|| true),
            hint,
            palette,
            selection: Selection::default(),
            scroll_x: 0.0,
            dragging: false,
            composing: false,
            focused: create_rw_signal(false),
            active_window: true,
            blink: create_rw_signal(false),
            generation: create_rw_signal(0),
        }
    }
    pub(crate) fn on_command(
        mut self,
        command: impl Fn(&Event) -> EventPropagation + 'static,
    ) -> Self {
        self.command = Box::new(command);
        self
    }
    pub(crate) fn on_paste(mut self, pasted: impl Fn() + 'static) -> Self {
        self.pasted = Box::new(pasted);
        self
    }
    /// Credential tokens may trim clipboard whitespace; passwords preserve it.
    pub(crate) fn trim_paste(mut self) -> Self {
        self.trim_paste = true;
        self
    }
    pub(crate) fn revealed(mut self, revealed: impl Fn() -> bool + 'static) -> Self {
        let revealed = floem::reactive::create_memo(move |_| revealed());
        let id = self.id;
        create_effect(move |_| {
            revealed.get();
            id.request_paint();
        });
        self.revealed = Box::new(move || revealed.get());
        self
    }
    pub(crate) fn enabled(mut self, enabled: impl Fn() -> bool + 'static) -> Self {
        let enabled = floem::reactive::create_memo(move |_| enabled());
        let id = self.id;
        create_effect(move |_| {
            enabled.get();
            id.request_paint();
        });
        self.enabled = Box::new(move || enabled.get());
        self
    }
    fn reset_blink(&self) {
        let generation = self.generation.get_untracked().wrapping_add(1);
        self.generation.set(generation);
        self.blink
            .set(self.focused.get_untracked() && self.active_window && (self.enabled)());
        if self.blink.get_untracked() {
            blink(
                self.id,
                self.focused,
                self.blink,
                self.generation,
                generation,
            );
        }
        self.id.request_paint();
    }
    fn layout_text(&self, value: &str) -> (TextLayout, bool) {
        let placeholder = value.is_empty();
        let display = Zeroizing::new(if placeholder {
            self.hint.to_string()
        } else if (self.revealed)() {
            value.to_owned()
        } else {
            "•".repeat(value.chars().count())
        });
        let mut layout = TextLayout::new();
        layout.set_text(
            &display,
            AttrsList::new(
                Attrs::new()
                    .family(&[FamilyOwned::Name(UI_FONT_FAMILY.to_owned())])
                    .font_size(FONT_BODY as f32)
                    .line_height(floem::text::LineHeightValue::Normal(1.4))
                    .color(if placeholder || !(self.enabled)() {
                        self.palette.muted
                    } else {
                        self.palette.ink
                    }),
            ),
        );
        (layout, placeholder)
    }
    fn display_offset(&self, value: &str, offset: usize) -> usize {
        if (self.revealed)() {
            offset
        } else {
            value[..offset].chars().count() * "•".len()
        }
    }
    fn pointer_offset(&self, value: &str, point: Point) -> usize {
        let (layout, _) = self.layout_text(value);
        let x = point.x - self.id.get_content_rect().x0 + self.scroll_x;
        value
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(value.len()))
            .min_by(|a, b| {
                let distance =
                    |i| (layout.hit_position(self.display_offset(value, i)).point.x - x).abs();
                distance(*a).total_cmp(&distance(*b))
            })
            .unwrap_or(0)
    }
    fn insert(&mut self, value: &str) {
        let range = self.selection.range();
        if (self.replace)(range.clone(), value) {
            self.selection.move_to(range.start + value.len(), false);
        }
        self.own_revision = self.revision.get_untracked();
    }
}

fn blink(
    id: ViewId,
    focused: RwSignal<bool>,
    visible: RwSignal<bool>,
    generation: RwSignal<u64>,
    expected: u64,
) {
    exec_after(Duration::from_millis(530), move |_| {
        if generation.try_get_untracked() != Some(expected)
            || focused.try_get_untracked() != Some(true)
        {
            return;
        }
        visible.update(|v| *v = !*v);
        id.request_paint();
        blink(id, focused, visible, generation, expected);
    });
}

impl View for SecretInput {
    fn id(&self) -> ViewId {
        self.id
    }
    fn update(&mut self, _cx: &mut UpdateCx, state: Box<dyn Any>) {
        if let Some(revision) = state.downcast_ref::<u64>() {
            if *revision != self.last_revision {
                if *revision != self.own_revision {
                    self.selection.move_to((self.read)().len(), false);
                }
                self.last_revision = *revision;
            }
        }
        if let Ok(focused) = state.downcast::<bool>() {
            self.focused.set(*focused);
            self.dragging = false;
            self.composing = false;
            if *focused {
                floem::action::set_ime_allowed(true);
            }
            self.reset_blink();
        }
        self.selection.clamp(&(self.read)());
        self.id.request_paint();
    }
    fn event_before_children(&mut self, _cx: &mut EventCx, event: &Event) -> EventPropagation {
        match event {
            Event::FocusGained => {
                self.focused.set(true);
                floem::action::set_ime_allowed(true);
                self.reset_blink();
            }
            Event::FocusLost => {
                self.focused.set(false);
                self.dragging = false;
                self.reset_blink();
            }
            Event::WindowLostFocus => {
                self.active_window = false;
                self.reset_blink();
            }
            Event::WindowGotFocus => {
                self.active_window = true;
                self.reset_blink();
            }
            _ => {}
        }
        if !(self.enabled)() {
            return EventPropagation::Continue;
        }
        let value = (self.read)();
        self.selection.clamp(&value);
        match event {
            Event::PointerDown(pointer) if pointer.button.is_primary() => {
                self.id.request_focus();
                self.id.request_active();
                self.dragging = true;
                self.selection.move_to(
                    self.pointer_offset(&value, pointer.pos),
                    pointer.modifiers.shift(),
                );
            }
            Event::PointerMove(pointer) if self.dragging => {
                self.selection
                    .move_to(self.pointer_offset(&value, pointer.pos), true);
            }
            Event::PointerUp(_) if self.dragging => {
                self.dragging = false;
            }
            Event::ImeCommit(text) => {
                self.composing = false;
                self.insert(text);
            }
            Event::ImePreedit { text, .. } => {
                self.composing = !text.is_empty();
                return EventPropagation::Stop;
            }
            Event::KeyDown(key) => {
                if self.composing {
                    return EventPropagation::Stop;
                }
                let altgr = key.modifiers.control() && key.modifiers.alt() && !key.modifiers.meta();
                let shortcut = (key.modifiers.control() || key.modifiers.meta()) && !altgr;
                let extend = key.modifiers.shift();
                let previous = || {
                    value[..self.selection.caret]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(i, _)| i)
                };
                let next = || {
                    self.selection.caret
                        + value[self.selection.caret..]
                            .chars()
                            .next()
                            .map_or(0, char::len_utf8)
                };
                match &key.key.logical_key {
                    Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowRight) => {
                        let left = key.key.logical_key == Key::Named(NamedKey::ArrowLeft);
                        let offset = if shortcut {
                            if left { 0 } else { value.len() }
                        } else if !extend && !self.selection.range().is_empty() {
                            if left {
                                self.selection.range().start
                            } else {
                                self.selection.range().end
                            }
                        } else if left {
                            previous()
                        } else {
                            next()
                        };
                        self.selection.move_to(offset, extend);
                    }
                    Key::Named(NamedKey::Home) => self.selection.move_to(0, extend),
                    Key::Named(NamedKey::End) => self.selection.move_to(value.len(), extend),
                    Key::Character(c) if shortcut && c.eq_ignore_ascii_case("a") => {
                        self.selection.anchor = 0;
                        self.selection.caret = value.len();
                    }
                    Key::Character(c) if shortcut && c.eq_ignore_ascii_case("v") => {
                        match floem::Clipboard::get_contents() {
                            Ok(text) => {
                                let text = Zeroizing::new(text);
                                self.insert(if self.trim_paste { text.trim() } else { &text });
                                (self.pasted)();
                            }
                            Err(_) => return (self.command)(event),
                        }
                    }
                    Key::Named(NamedKey::Backspace | NamedKey::Delete) => {
                        if self.selection.range().is_empty() {
                            self.selection.anchor =
                                if key.key.logical_key == Key::Named(NamedKey::Backspace) {
                                    previous()
                                } else {
                                    next()
                                };
                        }
                        self.insert("");
                    }
                    Key::Character(c) if !shortcut => self.insert(c),
                    Key::Named(NamedKey::Space) if !shortcut => self.insert(" "),
                    Key::Named(NamedKey::Tab | NamedKey::Enter | NamedKey::Escape) => {
                        return (self.command)(event);
                    }
                    _ => return EventPropagation::Stop,
                }
            }
            _ => return EventPropagation::Continue,
        }
        self.reset_blink();
        EventPropagation::Stop
    }
    fn paint(&mut self, cx: &mut PaintCx) {
        let value = (self.read)();
        self.selection.clamp(&value);
        let (layout, placeholder) = self.layout_text(&value);
        let bounds = self.id.get_content_rect();
        let caret = if placeholder {
            0.0
        } else {
            layout
                .hit_position(self.display_offset(&value, self.selection.caret))
                .point
                .x
        };
        self.scroll_x = self
            .scroll_x
            .min(caret)
            .max(caret - (bounds.width() - 2.0).max(1.0))
            .max(0.0);
        let origin = Point::new(
            bounds.x0 - self.scroll_x,
            bounds.y0 + (bounds.height() - layout.size().height) / 2.0,
        );
        cx.save();
        cx.clip(&bounds);
        if self.focused.get_untracked() && !self.selection.range().is_empty() {
            let range = self.selection.range();
            let x0 = layout
                .hit_position(self.display_offset(&value, range.start))
                .point
                .x;
            let x1 = layout
                .hit_position(self.display_offset(&value, range.end))
                .point
                .x;
            cx.fill(
                &Rect::new(
                    origin.x + x0,
                    origin.y,
                    origin.x + x1,
                    origin.y + layout.size().height,
                ),
                self.palette.accent_soft,
                0.0,
            );
        }
        cx.draw_text(&layout, origin);
        if self.blink.get_untracked() && (self.enabled)() {
            let window_origin = self.id.layout_rect().origin();
            let caret_x = (window_origin.x + origin.x + caret).round() - window_origin.x;
            let caret_y = (window_origin.y + origin.y).round() - window_origin.y;
            cx.fill(
                &Rect::new(
                    caret_x,
                    caret_y,
                    caret_x + 1.0,
                    caret_y + layout.size().height.round(),
                ),
                self.palette.accent,
                0.0,
            );
        }
        cx.restore();
        if self.focused.get_untracked() && self.active_window {
            floem::action::set_ime_cursor_area(
                self.id.layout_rect().origin() + Point::new(origin.x + caret, bounds.y1).to_vec2(),
                Size::new(1.0, bounds.height()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edits_unicode_in_the_middle_and_replaces_selection_without_growing_buffer() {
        let mut value = String::with_capacity(32);
        value.push_str("пароль");
        assert!(replace_secret(&mut value, 2..4, "A", 32));
        assert_eq!(value, "пAроль");
        assert!(replace_secret(&mut value, 2..3, "", 32));
        assert_eq!(value, "проль");
        let before = value.clone();
        assert!(!replace_secret(&mut value, 1..2, "x", 32));
        assert!(!replace_secret(&mut value, 0..0, &"x".repeat(33), 32));
        assert!(!replace_secret(&mut value, 0..0, "\n", 32));
        assert_eq!(value, before);
        let len = value.len();
        assert!(replace_secret(&mut value, 0..len, "новый", 32));
        assert_eq!(value, "новый");
        assert!(value.capacity() >= 32);
    }
    #[test]
    fn selection_collapses_and_extends_at_utf8_boundaries() {
        let mut selection = Selection::default();
        selection.move_to(4, false);
        selection.move_to(2, true);
        assert_eq!(selection.range(), 2..4);
        selection.move_to(2, false);
        assert!(selection.range().is_empty());
        selection.caret = 99;
        selection.anchor = 1;
        selection.clamp("аб");
        assert_eq!(selection.range(), 0..4);
    }
}
