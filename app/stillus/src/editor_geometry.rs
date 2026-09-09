// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::fmt;
use std::ops::Range;

use floem::text::{
    Attrs, AttrsList, Cursor, FamilyOwned, LayoutGlyph, LineHeightValue, TextLayout, Wrap,
};

/// Matches the core viewport byte ceiling. Geometry never shapes an
/// accidentally unbounded document snapshot.
pub const MAX_GEOMETRY_BYTES: usize = 256 * 1024;
pub const MAX_GEOMETRY_ROWS: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GeometryLine<'a> {
    pub line_index: usize,
    pub document_start: usize,
    pub document_end: usize,
    pub text: &'a str,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GeometryConfig {
    pub font_family: String,
    pub font_size: f32,
    pub line_height: f32,
    pub content_width: f32,
    pub tab_width: usize,
    pub origin_x: f64,
    pub origin_y: f64,
    pub top_reserved_rows: usize,
    pub first_line_skip_rows: usize,
    pub max_rows: usize,
    pub caret_height: f64,
    pub selection_height: f64,
    pub selection_marker_width: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeometryRow {
    pub line_slot: usize,
    pub line_index: usize,
    pub document_start: usize,
    pub start: usize,
    pub end: usize,
    pub layout_row: usize,
    pub width: f64,
    pub last_in_line: bool,
    pub truncated_line: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextHit {
    pub document_offset: usize,
    pub row: usize,
    pub is_inside: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CaretGeometry {
    pub document_offset: usize,
    pub row: usize,
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SelectionRect {
    pub row: usize,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GeometryError {
    InvalidMetrics,
    TooManyRows(usize),
    ViewportTooLarge(usize),
    InvalidLine {
        line_index: usize,
        message: &'static str,
    },
}

impl fmt::Display for GeometryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMetrics => {
                formatter.write_str("editor text metrics must be finite and positive")
            }
            Self::TooManyRows(rows) => write!(
                formatter,
                "editor geometry requested {rows} rows, maximum is {MAX_GEOMETRY_ROWS}"
            ),
            Self::ViewportTooLarge(bytes) => write!(
                formatter,
                "editor geometry received {bytes} bytes, maximum is {MAX_GEOMETRY_BYTES}"
            ),
            Self::InvalidLine {
                line_index,
                message,
            } => write!(formatter, "invalid viewport line {line_index}: {message}"),
        }
    }
}

impl std::error::Error for GeometryError {}

struct ShapedLine {
    line_index: usize,
    document_start: usize,
    document_end: usize,
    text_len: usize,
    first_row: usize,
    first_layout_row: usize,
    layout: TextLayout,
}

/// Pixel geometry for only the supplied viewport lines. Each line is shaped
/// with the same Floem engine used for painting, so wrapping, caret placement,
/// hit-testing and selections share glyph-cluster boundaries.
pub struct EditorTextGeometry {
    config: GeometryConfig,
    lines: Vec<ShapedLine>,
    rows: Vec<GeometryRow>,
    truncated_after: bool,
}

impl EditorTextGeometry {
    pub fn build(
        lines: &[GeometryLine<'_>],
        config: GeometryConfig,
    ) -> Result<Self, GeometryError> {
        validate_config(&config)?;
        let total_bytes = lines
            .iter()
            .try_fold(0_usize, |total, line| total.checked_add(line.text.len()));
        let Some(total_bytes) = total_bytes else {
            return Err(GeometryError::ViewportTooLarge(usize::MAX));
        };
        if total_bytes > MAX_GEOMETRY_BYTES {
            return Err(GeometryError::ViewportTooLarge(total_bytes));
        }

        let mut shaped_lines = Vec::with_capacity(lines.len().min(config.max_rows));
        let mut rows = Vec::with_capacity(config.max_rows);
        let mut truncated_after = false;

        for (input_slot, line) in lines.iter().enumerate() {
            validate_line(line)?;
            if rows.len() == config.max_rows {
                truncated_after = true;
                break;
            }

            let remaining_rows = config.max_rows - rows.len();
            let requested_skip = if input_slot == 0 {
                config.first_line_skip_rows
            } else {
                0
            };
            let shaped_row_limit = remaining_rows
                .saturating_add(requested_skip)
                .min(MAX_GEOMETRY_ROWS);
            let layout = shape_line(line.text, &config, shaped_row_limit);
            let layout_rows = layout.layout_runs().count();
            let first_layout_row = requested_skip.min(layout_rows.saturating_sub(1));
            let line_slot = shaped_lines.len();
            let first_row = rows.len();
            let mut previous_end = first_layout_row
                .checked_sub(1)
                .and_then(|previous_row| layout.layout_runs().nth(previous_row))
                .and_then(|run| run.glyphs.iter().map(|glyph| glyph.end).max())
                .unwrap_or(0);
            for (layout_row, run) in layout
                .layout_runs()
                .enumerate()
                .skip(first_layout_row)
                .take(remaining_rows)
            {
                let start = previous_end.min(line.text.len());
                let end = run
                    .glyphs
                    .iter()
                    .map(|glyph| glyph.end)
                    .max()
                    .unwrap_or(start)
                    .max(start)
                    .min(line.text.len());
                previous_end = end;
                rows.push(GeometryRow {
                    line_slot,
                    line_index: line.line_index,
                    document_start: line.document_start,
                    start,
                    end,
                    layout_row,
                    width: f64::from(run.line_w),
                    last_in_line: end == line.text.len(),
                    truncated_line: line.truncated,
                });
            }
            if rows.len() == first_row {
                rows.push(GeometryRow {
                    line_slot,
                    line_index: line.line_index,
                    document_start: line.document_start,
                    start: 0,
                    end: 0,
                    layout_row: 0,
                    width: 0.0,
                    last_in_line: true,
                    truncated_line: line.truncated,
                });
            }
            if rows
                .last()
                .is_some_and(|row| row.line_slot == line_slot && row.end < line.text.len())
            {
                truncated_after = true;
            }
            shaped_lines.push(ShapedLine {
                line_index: line.line_index,
                document_start: line.document_start,
                document_end: line.document_end,
                text_len: line.text.len(),
                first_row,
                first_layout_row,
                layout,
            });
            if truncated_after || input_slot + 1 < lines.len() && rows.len() == config.max_rows {
                truncated_after = true;
                break;
            }
        }

        Ok(Self {
            config,
            lines: shaped_lines,
            rows,
            truncated_after,
        })
    }

    pub fn rows(&self) -> &[GeometryRow] {
        &self.rows
    }

    pub fn row_text(&self, row: usize) -> Option<&str> {
        let row = self.rows.get(row)?;
        let line = self.lines.get(row.line_slot)?;
        let text = line.layout.lines().first()?.text();
        text.get(row.start..row.end)
    }

    pub fn truncated_after(&self) -> bool {
        self.truncated_after
    }

    /// Nearest legal caret boundary. Floem's shaping cursor is authoritative,
    /// then the result is clamped to the visible wrapped row.
    pub fn hit_test_caret(&self, x: f64, y: f64) -> Option<TextHit> {
        let (row_index, inside_y) = self.row_for_y(y)?;
        let row = self.rows.get(row_index)?;
        let line = self.lines.get(row.line_slot)?;
        let local_y = (row.layout_row as f64 + 0.5) * f64::from(self.config.line_height);
        let cursor = line
            .layout
            .hit((x - self.config.origin_x) as f32, local_y as f32)?;
        let local = cursor.index.clamp(row.start, row.end);
        let is_inside_x = x >= self.config.origin_x
            && x <= self.config.origin_x + f64::from(self.config.content_width);
        Some(TextHit {
            document_offset: line.document_start.saturating_add(local),
            row: row_index,
            is_inside: inside_y && is_inside_x,
        })
    }

    /// Byte range start of the glyph cluster visually under the pointer. This
    /// differs from nearest-caret hit-testing in the right half of a glyph and
    /// is suitable for double-click word selection.
    pub fn hit_test_glyph(&self, x: f64, y: f64) -> Option<TextHit> {
        let (row_index, inside_y) = self.row_for_y(y)?;
        let row = self.rows.get(row_index)?;
        let line = self.lines.get(row.line_slot)?;
        let local_x = (x - self.config.origin_x) as f32;
        let run = line.layout.layout_runs().nth(row.layout_row)?;
        let local = run
            .glyphs
            .iter()
            .find(|glyph| local_x >= glyph.x && local_x < glyph.x + glyph.w)
            .map(|glyph| glyph.start)
            .unwrap_or_else(|| {
                line.layout
                    .hit(
                        local_x,
                        (row.layout_row as f32 + 0.5) * self.config.line_height,
                    )
                    .map(|cursor| cursor.index)
                    .unwrap_or(row.start)
            })
            .clamp(row.start, row.end);
        let is_inside_x = x >= self.config.origin_x
            && x <= self.config.origin_x + f64::from(self.config.content_width);
        Some(TextHit {
            document_offset: line.document_start.saturating_add(local),
            row: row_index,
            is_inside: inside_y && is_inside_x,
        })
    }

    /// Returns the actual shaped caret point and snaps an offset inside a
    /// combining/emoji cluster back to that cluster's leading boundary.
    pub fn caret(&self, document_line: usize, document_offset: usize) -> Option<CaretGeometry> {
        let (line_slot, line) = self
            .lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.line_index == document_line)?;
        let local = document_offset
            .saturating_sub(line.document_start)
            .min(line.text_len);
        let hit = line.layout.hit_position(local);
        let row_index = line
            .first_row
            .checked_add(hit.line.checked_sub(line.first_layout_row)?)?;
        let row = self.rows.get(row_index)?;
        if row.line_slot != line_slot {
            return None;
        }
        let run = line.layout.layout_runs().nth(row.layout_row)?;
        let snapped = snap_to_cluster_start(run.glyphs, local).clamp(row.start, row.end);
        let hit = line.layout.hit_position(snapped);
        Some(CaretGeometry {
            document_offset: line.document_start.saturating_add(snapped),
            row: row_index,
            x: self.config.origin_x + hit.point.x,
            y: self.row_y(row_index)
                + (f64::from(self.config.line_height) - self.config.caret_height) / 2.0,
        })
    }

    pub fn selection_rects(&self, selection: Range<usize>) -> Vec<SelectionRect> {
        if selection.start >= selection.end {
            return Vec::new();
        }
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(row_index, row)| self.selection_rect(row_index, row, &selection))
            .collect()
    }

    fn selection_rect(
        &self,
        row_index: usize,
        row: &GeometryRow,
        selection: &Range<usize>,
    ) -> Option<SelectionRect> {
        let line = self.lines.get(row.line_slot)?;
        let row_start = line.document_start.saturating_add(row.start);
        let row_end = line.document_start.saturating_add(row.end);
        let start = selection.start.max(row_start);
        let end = selection.end.min(row_end);
        let covers_line_break = row.last_in_line
            && !row.truncated_line
            && selection.start <= row_end
            && selection.end > row_end
            && row_end < line.document_end;
        if start >= end && !covers_line_break {
            return None;
        }

        let run = line.layout.layout_runs().nth(row.layout_row)?;
        let local_start =
            snap_to_cluster_start(run.glyphs, start.saturating_sub(line.document_start))
                .clamp(row.start, row.end);
        let local_end = snap_to_cluster_end(run.glyphs, end.saturating_sub(line.document_start))
            .clamp(local_start, row.end);
        let highlighted = (local_start < local_end)
            .then(|| run.highlight(Cursor::new(0, local_start), Cursor::new(0, local_end)))
            .flatten();
        let (x, width) = highlighted
            .map(|(x, width)| (f64::from(x), f64::from(width)))
            .unwrap_or_else(|| {
                let caret = line.layout.hit_position(local_start);
                (caret.point.x, self.config.selection_marker_width)
            });
        Some(SelectionRect {
            row: row_index,
            x: self.config.origin_x + x,
            y: self.row_y(row_index)
                + (f64::from(self.config.line_height) - self.config.selection_height) / 2.0,
            width: width.max(self.config.selection_marker_width),
            height: self.config.selection_height,
        })
    }

    fn row_for_y(&self, y: f64) -> Option<(usize, bool)> {
        if self.rows.is_empty() {
            return None;
        }
        let rows_top = self.config.origin_y
            + self.config.top_reserved_rows as f64 * f64::from(self.config.line_height);
        let relative = (y - rows_top) / f64::from(self.config.line_height);
        let inside = relative >= 0.0 && relative < self.rows.len() as f64;
        let row = if relative < 0.0 {
            0
        } else {
            (relative.floor() as usize).min(self.rows.len() - 1)
        };
        Some((row, inside))
    }

    fn row_y(&self, row: usize) -> f64 {
        self.config.origin_y
            + (row + self.config.top_reserved_rows) as f64 * f64::from(self.config.line_height)
    }
}

fn validate_config(config: &GeometryConfig) -> Result<(), GeometryError> {
    let positive_f32 = |value: f32| value.is_finite() && value > 0.0;
    let positive_f64 = |value: f64| value.is_finite() && value > 0.0;
    if !positive_f32(config.font_size)
        || !positive_f32(config.line_height)
        || !positive_f32(config.content_width)
        || config.tab_width == 0
        || !config.origin_x.is_finite()
        || !config.origin_y.is_finite()
        || !positive_f64(config.caret_height)
        || !positive_f64(config.selection_height)
        || !positive_f64(config.selection_marker_width)
    {
        return Err(GeometryError::InvalidMetrics);
    }
    if config.max_rows == 0 || config.max_rows > MAX_GEOMETRY_ROWS {
        return Err(GeometryError::TooManyRows(config.max_rows));
    }
    Ok(())
}

fn validate_line(line: &GeometryLine<'_>) -> Result<(), GeometryError> {
    let Some(text_end) = line.document_start.checked_add(line.text.len()) else {
        return Err(GeometryError::InvalidLine {
            line_index: line.line_index,
            message: "document offsets overflow",
        });
    };
    if line.document_end < text_end {
        return Err(GeometryError::InvalidLine {
            line_index: line.line_index,
            message: "document end precedes visible text end",
        });
    }
    if line.text.contains(['\n', '\r']) {
        return Err(GeometryError::InvalidLine {
            line_index: line.line_index,
            message: "line text contains a line ending",
        });
    }
    Ok(())
}

fn shape_line(text: &str, config: &GeometryConfig, max_rows: usize) -> TextLayout {
    let mut families = FamilyOwned::parse_list(&config.font_family).collect::<Vec<_>>();
    if families.is_empty() {
        families.push(FamilyOwned::Monospace);
    }
    let attrs = Attrs::new()
        .family(&families)
        .font_size(config.font_size)
        .line_height(LineHeightValue::Px(config.line_height));
    let mut layout = TextLayout::new();
    layout.set_tab_width(config.tab_width);
    layout.set_wrap(Wrap::WordOrGlyph);
    layout.set_size(config.content_width, max_rows as f32 * config.line_height);
    layout.set_text(text, AttrsList::new(attrs));
    layout
}

fn snap_to_cluster_start(glyphs: &[LayoutGlyph], offset: usize) -> usize {
    glyphs
        .iter()
        .find(|glyph| glyph.start < offset && offset < glyph.end)
        .map_or(offset, |glyph| glyph.start)
}

fn snap_to_cluster_end(glyphs: &[LayoutGlyph], offset: usize) -> usize {
    glyphs
        .iter()
        .find(|glyph| glyph.start < offset && offset < glyph.end)
        .map_or(offset, |glyph| glyph.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN_X: f64 = 17.0;
    const ORIGIN_Y: f64 = 11.0;

    fn config(content_width: f32, max_rows: usize) -> GeometryConfig {
        GeometryConfig {
            font_family: "monospace".to_owned(),
            font_size: 14.0,
            line_height: 22.0,
            content_width,
            tab_width: 4,
            origin_x: ORIGIN_X,
            origin_y: ORIGIN_Y,
            top_reserved_rows: 0,
            first_line_skip_rows: 0,
            max_rows,
            caret_height: 18.0,
            selection_height: 20.0,
            selection_marker_width: 4.0,
        }
    }

    fn line<'a>(line_index: usize, document_start: usize, text: &'a str) -> GeometryLine<'a> {
        GeometryLine {
            line_index,
            document_start,
            document_end: document_start + text.len() + 1,
            text,
            truncated: false,
        }
    }

    #[test]
    fn caret_and_hits_follow_real_cjk_emoji_and_combining_glyphs() {
        let text = "A界👩‍💻e\u{301}Z";
        let geometry = EditorTextGeometry::build(&[line(7, 100, text)], config(1_000.0, 8))
            .expect("shape Unicode line");
        assert_eq!(geometry.rows().len(), 1);
        let shaped = &geometry.lines[0];

        for local in [
            0,
            "A".len(),
            "A界".len(),
            "A界👩‍💻".len(),
            "A界👩‍💻e\u{301}".len(),
            text.len(),
        ] {
            let caret = geometry.caret(7, 100 + local).expect("visible caret");
            let expected = shaped.layout.hit_position(local);
            assert!((caret.x - (ORIGIN_X + expected.point.x)).abs() < 0.01);
            assert_eq!(caret.row, 0);
        }

        let combining_start = "A界👩‍💻".len();
        let inside_combining_cluster = combining_start + "e".len();
        let combining_glyph = shaped
            .layout
            .layout_runs()
            .flat_map(|run| run.glyphs)
            .find(|glyph| {
                glyph.start < inside_combining_cluster && inside_combining_cluster < glyph.end
            })
            .expect("shaper keeps the combining sequence in one cluster");
        let caret = geometry
            .caret(7, 100 + inside_combining_cluster)
            .expect("cluster caret");
        assert_eq!(caret.document_offset, 100 + combining_glyph.start);

        let cjk_start = "A".len();
        let cjk_glyph = shaped
            .layout
            .layout_runs()
            .flat_map(|run| run.glyphs)
            .find(|glyph| glyph.start == cjk_start)
            .expect("CJK glyph");
        let hit = geometry
            .hit_test_glyph(
                ORIGIN_X + f64::from(cjk_glyph.x + cjk_glyph.w / 2.0),
                ORIGIN_Y + 11.0,
            )
            .expect("glyph hit");
        assert_eq!(hit.document_offset, 100 + cjk_glyph.start);
        assert!(hit.is_inside);
    }

    #[test]
    fn wrap_rows_use_shaped_widths_and_keep_cluster_boundaries() {
        let text = "界界界界 👩‍💻👩‍💻 e\u{301}e\u{301}";
        let unwrapped = shape_line(text, &config(10_000.0, 32), 32);
        let width = (unwrapped.size().width / 3.0) as f32;
        let geometry = EditorTextGeometry::build(&[line(0, 0, text)], config(width, 32))
            .expect("shape wrapped Unicode line");

        assert!(geometry.rows().len() >= 3);
        assert!(
            geometry
                .rows()
                .iter()
                .enumerate()
                .all(|(index, row)| geometry.row_text(index) == text.get(row.start..row.end))
        );
        assert!(
            geometry
                .rows()
                .iter()
                .all(|row| row.width <= f64::from(width) + 0.1)
        );
        for row in geometry.rows() {
            assert!(text.is_char_boundary(row.start));
            assert!(text.is_char_boundary(row.end));
            let run = geometry.lines[row.line_slot]
                .layout
                .layout_runs()
                .nth(row.layout_row)
                .expect("layout run for row");
            assert!(!run.glyphs.iter().any(|glyph| {
                glyph.start < row.start && row.start < glyph.end
                    || glyph.start < row.end && row.end < glyph.end
            }));
        }
    }

    #[test]
    fn first_line_visual_skip_keeps_offsets_and_caret_geometry_aligned() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        let full = EditorTextGeometry::build(&[line(4, 100, text)], config(70.0, 16))
            .expect("shape full wrapped line");
        assert!(full.rows().len() > 3);
        let expected = full.rows()[2];

        let mut skipped_config = config(70.0, 16);
        skipped_config.first_line_skip_rows = 2;
        let skipped = EditorTextGeometry::build(&[line(4, 100, text)], skipped_config)
            .expect("shape skipped wrapped line");
        assert_eq!(skipped.rows()[0].layout_row, 2);
        assert_eq!(skipped.rows()[0].start, expected.start);
        assert_eq!(skipped.row_text(0), full.row_text(2));
        assert!(skipped.caret(4, 100 + full.rows()[0].start).is_none());
        let inside_first_cluster = expected.start
            + skipped
                .row_text(0)
                .and_then(|text| text.chars().next())
                .map(char::len_utf8)
                .expect("first retained glyph");
        let caret = skipped
            .caret(4, 100 + inside_first_cluster)
            .expect("caret on first retained row");
        assert_eq!(caret.row, 0);
        assert!((caret.y - (ORIGIN_Y + 2.0)).abs() < 0.01);
    }

    #[test]
    fn selection_uses_glyph_highlights_and_expands_partial_clusters() {
        let text = "a界e\u{301}z";
        let geometry = EditorTextGeometry::build(&[line(3, 50, text)], config(1_000.0, 8))
            .expect("shape selection line");
        let combining_start = "a界".len();
        let inside_combining_cluster = combining_start + "e".len();
        let rects =
            geometry.selection_rects(50 + inside_combining_cluster..50 + "a界e\u{301}".len());
        assert_eq!(rects.len(), 1);
        assert!(rects[0].width > 0.0);

        let run = geometry.lines[0]
            .layout
            .layout_runs()
            .next()
            .expect("selection run");
        let glyph = run
            .glyphs
            .iter()
            .find(|glyph| {
                glyph.start < inside_combining_cluster && inside_combining_cluster < glyph.end
            })
            .expect("combining cluster");
        let expected = run
            .highlight(Cursor::new(0, glyph.start), Cursor::new(0, glyph.end))
            .expect("cluster highlight");
        assert!((rects[0].x - (ORIGIN_X + f64::from(expected.0))).abs() < 0.01);
        assert!((rects[0].width - f64::from(expected.1)).abs() < 0.01);

        let empty = EditorTextGeometry::build(&[line(4, 90, "")], config(1_000.0, 8))
            .expect("shape empty line");
        let marker = empty.selection_rects(90..91);
        assert_eq!(marker.len(), 1);
        assert_eq!(marker[0].width, 4.0);
    }

    #[test]
    fn viewport_geometry_is_row_and_byte_bounded() {
        let text = "界 ".repeat(2_000);
        let geometry = EditorTextGeometry::build(&[line(0, 0, &text)], config(30.0, 2))
            .expect("shape bounded rows");
        assert_eq!(geometry.rows().len(), 2);
        assert!(geometry.truncated_after());

        let oversized = "x".repeat(MAX_GEOMETRY_BYTES + 1);
        assert_eq!(
            EditorTextGeometry::build(&[line(0, 0, &oversized)], config(100.0, 2))
                .err()
                .expect("oversized viewport rejected"),
            GeometryError::ViewportTooLarge(MAX_GEOMETRY_BYTES + 1)
        );
        assert!(matches!(
            EditorTextGeometry::build(&[line(0, 0, "x")], config(100.0, 0)),
            Err(GeometryError::TooManyRows(0))
        ));
    }

    #[test]
    fn hit_testing_clamps_outside_points_without_splitting_glyphs() {
        let text = "界abc";
        let geometry = EditorTextGeometry::build(&[line(9, 200, text)], config(1_000.0, 4))
            .expect("shape hit-test line");
        let left = geometry
            .hit_test_caret(-1_000.0, -1_000.0)
            .expect("left clamp");
        assert_eq!(left.document_offset, 200);
        assert!(!left.is_inside);
        let right = geometry
            .hit_test_caret(10_000.0, 10_000.0)
            .expect("right clamp");
        assert_eq!(right.document_offset, 200 + text.len());
        assert!(!right.is_inside);
    }
}
