// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

//! Uppercase section eyebrows with optical tracking, without changing their data.
use floem::context::{LayoutCx, PaintCx, UpdateCx};
use floem::kurbo::{Point, Size};
use floem::peniko::Color;
use floem::reactive::create_updater;
use floem::style::Style;
use floem::taffy::tree::NodeId;
use floem::text::{Attrs, AttrsList, FamilyOwned, TextLayout};
use floem::{View, ViewId};
use floem_renderer::Renderer;
use std::any::Any;

const TRACKING_EM: f64 = 0.04;

/// This intentionally serves section eyebrows, not dates, hints or body text.
pub(crate) fn caption(value: impl Fn() -> String + 'static, color: Color) -> Caption {
    let id = ViewId::new();
    let initial = create_updater(value, move |value| id.update_state(value));
    Caption {
        id,
        text: initial,
        color,
        layouts: Vec::new(),
        text_node: None,
        measured: Size::ZERO,
    }
}

pub(crate) struct Caption {
    id: ViewId,
    text: String,
    color: Color,
    layouts: Vec<(TextLayout, f64)>,
    text_node: Option<NodeId>,
    measured: Size,
}

/// Split only simple Latin/Cyrillic runs. Combining marks, joining scripts and
/// mixed-script strings retain whole-run shaping instead of isolated glyphs.
fn display_parts(text: &str) -> (Vec<String>, bool) {
    let trackable = !text.is_empty()
        && text.chars().all(|c| {
            c.is_ascii()
                || matches!(c as u32, 0x00c0..=0x024f | 0x0400..=0x052f)
                    && c.to_uppercase().to_string() != c.to_lowercase().to_string()
        });
    if trackable {
        (
            text.to_uppercase().chars().map(|c| c.to_string()).collect(),
            true,
        )
    } else {
        // Uppercasing still applies to cased letters, while uncased scripts are
        // unchanged. Keep combining sequences and bidirectional runs together.
        (vec![text.to_uppercase()], false)
    }
}

fn shape(text: &str, color: Color) -> (Vec<(TextLayout, f64)>, Size) {
    let (parts, tracked) = display_parts(text);
    let families = [FamilyOwned::Name(super::UI_FONT_FAMILY.to_owned())];
    let attrs = AttrsList::new(
        Attrs::new()
            .font_size(super::FONT_CAPTION as f32)
            .family(&families)
            .color(color),
    );
    let mut layouts = Vec::with_capacity(parts.len());
    let mut size = Size::ZERO;
    for part in parts {
        if !layouts.is_empty() && tracked {
            size.width += super::FONT_CAPTION * TRACKING_EM;
        }
        let mut layout = TextLayout::new();
        layout.set_text(&part, attrs.clone());
        let measured = layout.size();
        layouts.push((layout, size.width));
        size.width += measured.width;
        size.height = size.height.max(measured.height);
    }
    (layouts, size)
}

impl View for Caption {
    fn id(&self) -> ViewId {
        self.id
    }
    fn debug_name(&self) -> std::borrow::Cow<'static, str> {
        "Caption".into()
    }
    fn update(&mut self, _cx: &mut UpdateCx, state: Box<dyn Any>) {
        if let Ok(text) = state.downcast::<String>() {
            self.text = *text;
            self.layouts.clear();
            self.id.request_layout();
        }
    }
    fn layout(&mut self, cx: &mut LayoutCx) -> NodeId {
        cx.layout_node(self.id, true, |_cx| {
            if self.layouts.is_empty() {
                (self.layouts, self.measured) = shape(&self.text, self.color);
            }
            let node = *self
                .text_node
                .get_or_insert_with(|| self.id.new_taffy_node());
            let style = Style::new()
                .width(self.measured.width.ceil())
                .height(self.measured.height.ceil())
                .to_taffy_style();
            self.id.set_taffy_style(node, style);
            vec![node]
        })
    }
    fn paint(&mut self, cx: &mut PaintCx) {
        let Some(node) = self.text_node else {
            return;
        };
        let location = self
            .id
            .taffy_layout(node)
            .map(|layout| layout.location)
            .unwrap_or_default();
        for (layout, x) in &self.layouts {
            cx.draw_text(
                layout,
                Point::new(f64::from(location.x) + x, f64::from(location.y)),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caption_tracking_preserves_data_and_shapes_joining_scripts_together() {
        let original = "Раздел abc";
        let (parts, tracked) = display_parts(original);
        assert!(tracked);
        assert_eq!(parts.concat(), "РАЗДЕЛ ABC");
        assert_eq!(original, "Раздел abc");
        for sample in ["العربية", "हिन्दी", "বাংলা", "日本語", "a\u{301}"]
        {
            let (parts, tracked) = display_parts(sample);
            assert!(!tracked);
            assert_eq!(parts, [sample.to_uppercase()]);
        }
    }

    #[test]
    fn caption_adds_exactly_four_percent_between_glyphs() {
        super::super::register_fonts();
        let (layouts, size) = shape("Тест", Color::BLACK);
        assert_eq!(layouts.len(), 4);
        let advances: f64 = layouts.iter().map(|(layout, _)| layout.size().width).sum();
        assert!(
            (size.width - advances - 3.0 * super::super::FONT_CAPTION * TRACKING_EM).abs() < 0.001
        );
        for pair in layouts.windows(2) {
            let gap = pair[1].1 - pair[0].1 - pair[0].0.size().width;
            assert!((gap - 0.48).abs() < 0.001);
        }
    }
}
