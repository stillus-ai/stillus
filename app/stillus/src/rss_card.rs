// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

//! Bounded, native RSS presentation. Markdown is data, never a browser surface.
#![forbid(unsafe_code)]

use std::ops::Range;

use crate::i18n::{Key, tr};
use crate::ui::{FONT_BODY, MONO_FONT_FAMILY, UI_FONT_FAMILY};
use chrono::{DateTime, Datelike, Local, Timelike};
use floem::peniko::Color;
use floem::text::{Attrs, AttrsList, FamilyOwned, LineHeightValue, Style, TextLayout, Weight};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use url::Url;

const MAX_EXCERPT_CHARS: usize = 700;

pub fn collapsed(unread: bool, hidden: bool, expanded: bool) -> bool {
    (!unread || hidden) && !expanded
}

/// Ephemeral positioning for one mounted feed; never changes read marks.
#[derive(Default)]
pub struct FeedScroll {
    initialized: bool,
    initial_target: Option<String>,
    selected: Option<String>,
    follow_selection: bool,
}

impl FeedScroll {
    pub fn update(
        &mut self,
        feed: &stillus_core::RssFeedCache,
        state: &stillus_core::RssReadState,
        selected: Option<&str>,
        loading: bool,
    ) {
        if self.selected.as_deref() != selected {
            self.selected = selected.map(str::to_owned);
            self.follow_selection = selected.is_some();
            if selected.is_some() {
                self.initialized = true;
                self.initial_target = None;
            }
        }
        if !self.initialized
            && (!feed.entries.is_empty() || (!loading && feed.fetched_at.is_some()))
        {
            self.initialized = true;
            self.initial_target = feed
                .entries
                .iter()
                .find(|entry| !state.hidden(&entry.id) && !state.read_entry_ids.contains(&entry.id))
                .map(|entry| entry.id.clone());
        }
    }

    pub fn manual_scroll(&mut self) {
        self.initialized = true;
        self.initial_target = None;
        self.follow_selection = false;
    }

    pub fn select(&mut self, id: &str) {
        self.initialized = true;
        self.initial_target = None;
        self.selected = Some(id.to_owned());
        self.follow_selection = true;
    }

    pub fn reveal(&mut self, id: &str) -> bool {
        let reveal = self.should_reveal(id);
        if self.initial_target.as_deref() == Some(id) {
            self.initial_target = None;
        }
        reveal
    }

    pub fn should_reveal(&self, id: &str) -> bool {
        self.initial_target.as_deref() == Some(id)
            || (self.follow_selection && self.selected.as_deref() == Some(id))
    }
}

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

/// A 60% title tint with sufficient contrast even on the slightly darker canvas.
pub fn read_title_ink(paper: Color) -> Color {
    faded_ink(Color::rgb8(10, 14, 18), paper, 0.6)
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
    render(markdown, Some(MAX_EXCERPT_CHARS), true)
}
pub fn markdown(markdown: &str) -> Excerpt {
    render(markdown, None, false)
}
fn render(markdown: &str, limit: Option<usize>, continuation: bool) -> Excerpt {
    let source = restore_reference_lines(markdown);
    let mut result = Excerpt::default();
    let (mut strong, mut emphasis, mut code, mut image) = (0_u32, 0_u32, 0_u32, 0_u32);
    let mut link: Option<(usize, String)> = None;
    let mut lists: Vec<Option<u64>> = Vec::new();
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
                    if continuation && is_continuation(&label) {
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
            Event::Start(Tag::List(start)) => lists.push(start),
            Event::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                if !continuation {
                    result
                        .text
                        .push_str(&"  ".repeat(lists.len().saturating_sub(1).min(8)));
                }
                if !continuation && lists.last().is_some_and(Option::is_some) {
                    if let Some(Some(number)) = lists.last_mut() {
                        result.text.push_str(&format!("{number}. "));
                        *number += 1;
                    }
                } else {
                    result.text.push_str("• ");
                }
            }
            _ => {}
        }
    }
    if let Some((end, _)) = limit.and_then(|limit| result.text.char_indices().nth(limit)) {
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
        let family = [FamilyOwned::Name(UI_FONT_FAMILY.to_owned())];
        let base = Attrs::new()
            .font_size(FONT_BODY as f32)
            .line_height(LineHeightValue::Normal(1.55))
            .family(&family)
            .color(color);
        let mut attrs = AttrsList::new(base);
        let monospace = [FamilyOwned::Name(MONO_FONT_FAMILY.to_owned())];
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
    fn rss_read_and_filtered_cards_collapse_until_expanded() {
        for hidden in [false, true] {
            for unread in [false, true] {
                assert!(!collapsed(unread, hidden, true));
            }
            assert!(collapsed(false, hidden, false));
        }
        assert!(collapsed(true, true, false));
        assert!(!collapsed(true, false, false));
    }

    fn feed_fixture() -> (stillus_core::RssFeedCache, stillus_core::RssReadState) {
        (
            stillus_core::RssFeedCache {
                entries: (0..14)
                    .map(|index| stillus_core::RssEntry {
                        id: index.to_string(),
                        title: format!("Article {index}"),
                        author: None,
                        published: None,
                        updated: None,
                        summary: "Body".into(),
                        link: None,
                    })
                    .collect(),
                fetched_at: Some("2026-09-20T12:00:00Z".into()),
                ..Default::default()
            },
            stillus_core::RssReadState::default(),
        )
    }

    #[test]
    fn rss_initial_scroll_skips_read_and_filtered_entries_without_marking_read() {
        let (feed, mut state) = feed_fixture();
        state.read_entry_ids = (0..10).chain([12]).map(|index| index.to_string()).collect();
        state.entries.entry("10".into()).or_default().decision =
            Some(stillus_core::RssDecision::Hide);
        let before = state.clone();
        let mut scroll = FeedScroll::default();
        scroll.update(&feed, &state, None, false);
        for entry in &feed.entries {
            assert_eq!(scroll.reveal(&entry.id), entry.id == "11");
        }
        assert_eq!(state, before);
        // Refreshing or remounting the target must not repeat the initial jump.
        scroll.update(&feed, &state, None, false);
        assert!(!scroll.reveal("11"));
    }

    #[test]
    fn rss_initial_scroll_handles_all_read_empty_and_delayed_feeds() {
        let (feed, mut state) = feed_fixture();
        let mut scroll = FeedScroll::default();
        scroll.update(&Default::default(), &state, None, true);
        scroll.update(&feed, &state, None, false);
        assert!(scroll.reveal("0"));

        state.read_entry_ids = feed.entries.iter().map(|entry| entry.id.clone()).collect();
        let mut scroll = FeedScroll::default();
        scroll.update(&feed, &state, None, false);
        assert!(feed.entries.iter().all(|entry| !scroll.reveal(&entry.id)));
        state.read_entry_ids.clear();
        scroll.update(&feed, &state, None, false);
        assert!(!scroll.reveal("0"));

        let mut empty = feed.clone();
        empty.entries.clear();
        let mut scroll = FeedScroll::default();
        scroll.update(&empty, &state, None, false);
        scroll.update(&feed, &state, None, false);
        assert!(!scroll.reveal("0"));
    }

    #[test]
    fn rss_manual_navigation_cancels_initial_scroll_and_selection_anchoring() {
        let (feed, state) = feed_fixture();
        for loaded in [false, true] {
            let mut scroll = FeedScroll::default();
            if loaded {
                scroll.update(&feed, &state, None, false);
            }
            scroll.manual_scroll();
            scroll.update(&feed, &state, None, false);
            assert!(!scroll.reveal("0"));
            scroll.select("4");
            scroll.update(&feed, &state, Some("4"), false);
            assert!(scroll.reveal("4"));
            // Keep the selected card aligned as preceding cards collapse.
            assert!(scroll.reveal("4"));
            scroll.manual_scroll();
            scroll.update(&feed, &state, Some("4"), false);
            assert!(!scroll.reveal("4"));
            // Keyboard navigation establishes a new anchor.
            scroll.update(&feed, &state, Some("5"), false);
            assert!(!scroll.reveal("4"));
            assert!(scroll.reveal("5"));
        }
    }

    #[test]
    fn emphasis_keeps_the_bundled_sans_family_with_cyrillic() {
        crate::ui::register_fonts();
        let layout = markdown("Текст **жирный** *курсив*").layout(Color::BLACK);
        let ids = layout
            .layout_runs()
            .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.font_id))
            .collect::<Vec<_>>();
        assert!(!ids.is_empty());
        let fonts = floem::text::FONT_SYSTEM.lock();
        for id in ids {
            assert!(
                fonts
                    .db()
                    .face(id)
                    .unwrap()
                    .families
                    .iter()
                    .any(|(name, _)| name == UI_FONT_FAMILY)
            );
        }
    }

    #[test]
    fn read_title_and_secondary_text_keep_accessible_contrast() {
        let luminance = |color: Color| {
            let channel = |value: u8| {
                let value = f64::from(value) / 255.0;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
        };
        let palette = crate::ui::Palette::new();
        for background in [palette.paper, palette.canvas] {
            for foreground in [read_title_ink(background), palette.ink2] {
                let contrast = (luminance(background) + 0.05) / (luminance(foreground) + 0.05);
                assert!(contrast >= 4.5, "contrast {contrast}");
            }
        }
    }

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
