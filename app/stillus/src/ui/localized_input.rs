// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

//! Retains the native TextInput and its selection while translating its hint.
#![forbid(unsafe_code)]

use crate::i18n::{self, Key};
use floem::context::{ComputeLayoutCx, EventCx, LayoutCx, PaintCx, StyleCx, UpdateCx};
use floem::event::{Event, EventPropagation};
use floem::keyboard::{Key as LogicalKey, NamedKey};
use floem::kurbo::{Point, Rect};
use floem::reactive::{RwSignal, SignalGet, create_effect};
use floem::style::{FontFamily, FontSize};
use floem::text::{Attrs, AttrsList, FamilyOwned, TextLayout};
use floem::views::{TextInput, text_input};
use floem::{View, ViewId};
use floem_renderer::Renderer;
use std::any::Any;

type InputKeyHandler = dyn Fn(&Event) -> EventPropagation;

pub(crate) struct LocalizedInput {
    input: TextInput,
    buffer: RwSignal<String>,
    hint: InputHint,
    attrs: AttrsList,
    layout: TextLayout,
    on_escape: Option<Box<dyn Fn()>>,
    on_key: Option<Box<InputKeyHandler>>,
    composing: bool,
}

enum InputHint {
    Localized(Key),
    Literal(&'static str),
}
impl std::fmt::Display for InputHint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Localized(key) => write!(f, "{key}"),
            Self::Literal(value) => f.write_str(value),
        }
    }
}
impl LocalizedInput {
    pub(crate) fn new(buffer: RwSignal<String>, hint: Key) -> Self {
        let input = text_input(buffer);
        let id = input.id();
        create_effect(move |_| {
            i18n::current();
            id.request_paint();
        });
        Self {
            input,
            buffer,
            hint: InputHint::Localized(hint),
            attrs: AttrsList::new(Attrs::new()),
            layout: TextLayout::new(),
            on_escape: None,
            on_key: None,
            composing: false,
        }
    }

    /// Literal examples (URLs and paths) are decorative hints too.
    pub(crate) fn example(buffer: RwSignal<String>, hint: &'static str) -> Self {
        let mut input = Self::new(buffer, Key::SearchNotes);
        input.hint = InputHint::Literal(hint);
        input
    }

    /// Composite fields can route navigation before native input consumes it.
    pub(crate) fn on_key(mut self, action: impl Fn(&Event) -> EventPropagation + 'static) -> Self {
        self.on_key = Some(Box::new(action));
        self
    }

    pub(crate) fn on_escape(mut self, action: impl Fn() + 'static) -> Self {
        self.on_escape = Some(Box::new(action));
        self
    }
}

impl View for LocalizedInput {
    fn id(&self) -> ViewId {
        self.input.id()
    }
    fn debug_name(&self) -> std::borrow::Cow<'static, str> {
        "LocalizedInput".into()
    }
    fn update(&mut self, cx: &mut UpdateCx, state: Box<dyn Any>) {
        self.input.update(cx, state);
    }
    fn event_before_children(&mut self, cx: &mut EventCx, event: &Event) -> EventPropagation {
        match event {
            Event::ImePreedit { text, .. } => self.composing = !text.is_empty(),
            Event::ImeCommit(_) => self.composing = false,
            _ => {}
        }
        if super::popover_handle_escape(event) {
            return EventPropagation::Stop;
        }
        // Native TextInput consumes Escape to clear focus before decorators
        // run. Forms may instead close themselves and choose the next focus.
        if matches!(event, Event::KeyDown(key) if key.key.logical_key == LogicalKey::Named(NamedKey::Escape))
            && let Some(action) = &self.on_escape
        {
            action();
            return EventPropagation::Stop;
        }
        if !self.composing
            && let Some(action) = &self.on_key
            && matches!(event, Event::KeyDown(_))
            && action(event).is_processed()
        {
            return EventPropagation::Stop;
        }
        self.input.event_before_children(cx, event)
    }
    fn style_pass(&mut self, cx: &mut StyleCx<'_>) {
        self.input.style_pass(cx);
        let style = cx.style();
        let family = style
            .get(FontFamily)
            .unwrap_or_else(|| crate::ui::UI_FONT_FAMILY.to_owned());
        self.attrs = AttrsList::new(
            Attrs::new()
                .family(&[FamilyOwned::Name(family)])
                .font_size(style.get(FontSize).unwrap_or(crate::ui::FONT_BODY as f32))
                .color(super::Palette::new().ink3),
        );
    }
    fn layout(&mut self, cx: &mut LayoutCx) -> floem::taffy::tree::NodeId {
        self.input.layout(cx)
    }
    fn compute_layout(&mut self, cx: &mut ComputeLayoutCx) -> Option<Rect> {
        self.input.compute_layout(cx)
    }
    fn paint(&mut self, cx: &mut PaintCx) {
        self.input.paint(cx);
        if !self.buffer.get_untracked().is_empty() {
            return;
        }
        let Some(bounds) = self.id().get_layout() else {
            return;
        };
        self.layout
            .set_text(&self.hint.to_string(), self.attrs.clone());
        let size = self.layout.size();
        let left = f64::from(bounds.padding.left + bounds.border.left);
        let right = f64::from(bounds.size.width - bounds.padding.right - bounds.border.right);
        let x = if i18n::current().is_rtl() {
            (right - size.width).max(left)
        } else {
            left
        };
        let y = (f64::from(bounds.size.height) - size.height) / 2.0;
        cx.save();
        cx.clip(&Rect::new(left, 0.0, right, f64::from(bounds.size.height)));
        cx.draw_text(&self.layout, Point::new(x, y));
        cx.restore();
    }
}
