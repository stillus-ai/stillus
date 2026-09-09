// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

//! Bounded, native RSS presentation. Markdown is data, never a browser surface.
#![forbid(unsafe_code)]

use std::ops::Range;

use crate::i18n::{Key, tr};
use chrono::{DateTime, Datelike, Local, Timelike};
use floem::peniko::Color;
use floem::text::{Attrs, AttrsList, FamilyOwned, LineHeightValue, Style, TextLayout, Weight};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use url::Url;

const MAX_EXCERPT_CHARS: usize = 700;

// Vger's glyph rendering does not reliably preserve color alpha. Preblend
// text against the card surface so read-state contrast is renderer-independent.
pub fn faded_ink(ink: Color, paper: Color, opacity: f32) -> Color {
    let channel = |ink: u8, paper: u8| {
        (f32::from(ink) * opacity + f32::from(paper) * (1.0 - opacity)).round() as u8
    };
    Color::rgb8(
        channel(ink.r, paper.r),
        channel(ink.g, paper.g),
        channel(ink.b, paper.b),
    )
}

#[derive(Default)]
pub struct Excerpt {
    pub text: String,
    spans: Vec<(Range<usize>, bool, bool, bool)>,
    pub continuation: Option<String>,
}

pub fn article_url(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.to_string())
}

pub fn date_label(value: Option<&str>) -> String {
    let Some(date) = value.and_then(|value| DateTime::parse_from_rfc3339(value).ok()) else {
        return String::new();
    };
    let date = date.with_timezone(&Local);
    let months = [
        Key::Month1,
        Key::Month2,
        Key::Month3,
        Key::Month4,
        Key::Month5,
        Key::Month6,
        Key::Month7,
        Key::Month8,
        Key::Month9,
        Key::Month10,
        Key::Month11,
        Key::Month12,
    ];
    tr!(Date, "day" => date.day() as usize, "month" => months[date.month0() as usize].message(),
        "year" => date.year().to_string(), "time" => format!("{:02}:{:02}", date.hour(), date.minute()))
}

// Old caches flattened html2text's reference definitions onto the same line.
// Restore only numeric URL definitions, before parsing (and before truncating).
fn restore_reference_lines(value: &str) -> String {
    let mut restored = String::with_capacity(value.len());
    for (offset, character) in value.char_indices() {
        if character == '[' && offset > 0 && value[..offset].ends_with(char::is_whitespace) {
            let tail = &value[offset + 1..];
            if let Some((number, destination)) = tail.split_once("]:")
                && !number.is_empty()
                && number.bytes().all(|byte| byte.is_ascii_digit())
                && (destination.trim_start().starts_with("https://")
                    || destination.trim_start().starts_with("http://"))
            {
                restored.push_str("\n\n");
            }
        }
        restored.push(character);
    }
    restored
}

fn is_continuation(label: &str) -> bool {
    matches!(
        label
            .trim()
            .trim_end_matches(['.', '…', '→'])
            .trim()
            .to_lowercase()
            .as_str(),
        "читать далее" | "читать полностью" | "read more" | "continue reading"
    )
}

pub fn excerpt(markdown: &str) -> Excerpt {
    let source = restore_reference_lines(markdown);
    let mut result = Excerpt::default();
    let (mut strong, mut emphasis, mut code, mut image) = (0_u32, 0_u32, 0_u32, 0_u32);
    let mut link: Option<(usize, String)> = None;
    for event in Parser::new(&source) {
        match event {
            Event::Start(Tag::Strong | Tag::Heading { .. }) => strong += 1,
            Event::End(TagEnd::Strong) => strong = strong.saturating_sub(1),
            Event::End(TagEnd::Heading(_)) => {
                strong = strong.saturating_sub(1);
                result.text.push('\n');
            }
            Event::Start(Tag::Emphasis) => emphasis += 1,
            Event::End(TagEnd::Emphasis) => emphasis = emphasis.saturating_sub(1),
            Event::Start(Tag::CodeBlock(_)) => code += 1,
            Event::End(TagEnd::CodeBlock) => {
                code = code.saturating_sub(1);
                result.text.push('\n');
            }
            Event::Start(Tag::Image { .. }) => image += 1,
            Event::End(TagEnd::Image) => image = image.saturating_sub(1),
            Event::Start(Tag::Link { dest_url, .. }) if image == 0 => {
                link = Some((result.text.len(), dest_url.to_string()));
            }
            Event::End(TagEnd::Link) => {
                if let Some((start, destination)) = link.take() {
                    let label = result.text[start..].to_owned();
                    if is_continuation(&label) {
                        result.continuation =
                            result.continuation.or_else(|| article_url(&destination));
                        result.text.truncate(start);
                        result.spans.retain(|(range, ..)| range.end <= start);
                    }
                }
            }
            Event::Code(value) if image == 0 => {
                let start = result.text.len();
                result.text.push_str(&value);
                result
                    .spans
                    .push((start..result.text.len(), strong > 0, emphasis > 0, true));
            }
            Event::Text(value) if image == 0 => {
                let start = result.text.len();
                result.text.push_str(&value);
                result
                    .spans
                    .push((start..result.text.len(), strong > 0, emphasis > 0, code > 0));
            }
            Event::SoftBreak => result.text.push(' '),
            Event::HardBreak | Event::End(TagEnd::Paragraph | TagEnd::Item) => {
                if !result.text.ends_with('\n') {
                    result.text.push('\n');
                }
            }
            Event::Start(Tag::Item) => result.text.push_str("• "),
            _ => {}
        }
    }
    if let Some((end, _)) = result.text.char_indices().nth(MAX_EXCERPT_CHARS) {
        result.text.truncate(end);
        result.spans.retain(|(range, ..)| range.start < end);
        for (range, ..) in &mut result.spans {
            range.end = range.end.min(end);
        }
        result.text.push('…');
    }
    result.text = result.text.trim_end().to_owned();
    result
}

impl Excerpt {
    pub fn layout(&self, color: Color) -> TextLayout {
        let base = Attrs::new()
            .font_size(18.0)
            .line_height(LineHeightValue::Normal(1.55))
            .family(&[FamilyOwned::Serif])
            .color(color);
        let mut attrs = AttrsList::new(base);
        let monospace = [FamilyOwned::Monospace];
        for (range, strong, emphasis, code) in &self.spans {
            let range = range.start..range.end.min(self.text.len());
            if range.is_empty() {
                continue;
            }
            let mut span = base;
            if *strong {
                span = span.weight(Weight::SEMIBOLD);
            }
            if *emphasis {
                span = span.style(Style::Italic);
            }
            if *code {
                span = span.family(&monospace);
            }
            attrs.add_span(range, span);
        }
        let mut layout = TextLayout::new();
        layout.set_text(&self.text, attrs);
        layout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_markdown_and_extracts_legacy_read_more_without_url_noise() {
        let value = excerpt(
            "**Заголовок** и *текст*. [Читать далее][1] [1]: https://example.test/article?utm_source=rss",
        );
        assert_eq!(value.text, "Заголовок и текст.");
        assert_eq!(
            value.continuation.as_deref(),
            Some("https://example.test/article?utm_source=rss")
        );
        assert!(value.spans.iter().any(|(_, strong, ..)| *strong));
        assert!(value.spans.iter().any(|(_, _, emphasis, _)| *emphasis));
    }

    #[test]
    fn parses_links_before_unicode_excerpt_truncation_and_ignores_images() {
        let value = excerpt(&format!(
            "{} [Читать далее](https://example.test/post) ![image](https://example.test/image)",
            "я".repeat(900)
        ));
        assert_eq!(value.text.chars().count(), MAX_EXCERPT_CHARS + 1);
        assert!(value.text.ends_with('…'));
        assert!(value.continuation.is_some());
        // Keep the inert rejected-URL fixture distinct from runtime API tokens
        // scanned by the source audit.
        let value = excerpt(concat!(
            "[Источник](https://example.test/source) [Script](javascript",
            ":alert) [login](https://user:pass@example.test/)"
        ));
        assert_eq!(value.text, "Источник Script login");
        assert!(value.continuation.is_none());
        assert!(article_url("https://user:pass@example.test/").is_none());
        assert!(article_url("http://user:pass@example.test/").is_none());
        assert!(article_url("file:///tmp/article.html").is_none());
    }

    #[test]
    fn accepts_http_article_and_read_more_links() {
        let url = "http://localhost:8080/article";
        assert_eq!(article_url(url).as_deref(), Some(url));
        for markdown in [
            format!("Text [Read more]({url})"),
            format!("Text [Read more][1] [1]: {url}"),
        ] {
            let value = excerpt(&markdown);
            assert_eq!(value.text, "Text");
            assert_eq!(value.continuation.as_deref(), Some(url));
        }
    }

    #[test]
    fn code_and_truncated_unicode_keep_valid_style_ranges() {
        let value = excerpt(&format!("`code` **{}** tail", "я".repeat(900)));
        assert!(value.spans.iter().any(|(_, _, _, code)| *code));
        for (range, ..) in &value.spans {
            assert!(value.text.get(range.clone()).is_some());
        }
        let dimmed = faded_ink(Color::rgb8(35, 39, 45), Color::WHITE, 0.42);
        assert!(dimmed.r > 150 && dimmed.g > 150 && dimmed.b > 150);
        assert_eq!(dimmed.a, 255);
    }

    #[test]
    fn dates_are_human_readable_and_missing_metadata_stays_empty() {
        assert!(date_label(Some("2026-09-04T12:40:53Z")).contains("Sep 2026 ·"));
        assert_eq!(date_label(None), "");
        assert_eq!(date_label(Some("invalid")), "");
    }
}
