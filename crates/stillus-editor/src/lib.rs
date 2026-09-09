// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read, Write};

use lapce_xi_rope::rope::RopeInfo;
use lapce_xi_rope::tree::TreeBuilder;
use lapce_xi_rope::{LinesMetric, Rope};
use unicode_segmentation::UnicodeSegmentation;

pub const DEFAULT_HISTORY_BUDGET_BYTES: usize = 64 * 1024 * 1024;
const HISTORY_ENTRY_OVERHEAD_BYTES: usize = 4 * 1024;
const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct ByteOffset(usize);

impl ByteOffset {
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    pub const fn get(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteRange {
    start: ByteOffset,
    end: ByteOffset,
}

impl ByteRange {
    pub fn new(start: ByteOffset, end: ByteOffset) -> Result<Self, EditorError> {
        if start > end {
            return Err(EditorError::InvalidRange {
                start: start.get(),
                end: end.get(),
            });
        }
        Ok(Self { start, end })
    }

    pub const fn empty(at: ByteOffset) -> Self {
        Self { start: at, end: at }
    }

    pub const fn start(self) -> ByteOffset {
        self.start
    }

    pub const fn end(self) -> ByteOffset {
        self.end
    }

    pub const fn len(self) -> usize {
        self.end.0 - self.start.0
    }

    pub const fn is_empty(self) -> bool {
        self.start.0 == self.end.0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Selection {
    anchor: ByteOffset,
    focus: ByteOffset,
}

impl Selection {
    pub const fn new(anchor: ByteOffset, focus: ByteOffset) -> Self {
        Self { anchor, focus }
    }

    pub const fn caret(at: ByteOffset) -> Self {
        Self::new(at, at)
    }

    pub const fn anchor(self) -> ByteOffset {
        self.anchor
    }

    pub const fn focus(self) -> ByteOffset {
        self.focus
    }

    pub const fn is_caret(self) -> bool {
        self.anchor.0 == self.focus.0
    }

    pub fn normalized(self) -> ByteRange {
        if self.anchor <= self.focus {
            ByteRange {
                start: self.anchor,
                end: self.focus,
            }
        } else {
            ByteRange {
                start: self.focus,
                end: self.anchor,
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditorError {
    InvalidRange { start: usize, end: usize },
    OutOfBounds { offset: usize, len: usize },
    NotCodepointBoundary { offset: usize },
    InvalidUtf8 { valid_up_to: usize },
    Io(String),
}

impl fmt::Display for EditorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRange { start, end } => {
                write!(formatter, "invalid byte range: {start}..{end}")
            }
            Self::OutOfBounds { offset, len } => {
                write!(
                    formatter,
                    "byte offset {offset} exceeds document length {len}"
                )
            }
            Self::NotCodepointBoundary { offset } => {
                write!(
                    formatter,
                    "byte offset {offset} is not a UTF-8 codepoint boundary"
                )
            }
            Self::InvalidUtf8 { valid_up_to } => {
                write!(formatter, "input is not valid UTF-8 at byte {valid_up_to}")
            }
            Self::Io(message) => write!(formatter, "I/O error: {message}"),
        }
    }
}

impl std::error::Error for EditorError {}

/// Returns the Unicode word-boundary segment containing `offset`.
///
/// The input is intentionally a borrowed, caller-bounded text window so this
/// helper never materializes a full editor buffer. UAX #29 word boundaries
/// keep letters, numbers, combining sequences, contractions and decimal
/// numbers together while leaving Markdown punctuation as separate segments.
/// Whitespace is returned as its own segment. An offset at the end of a
/// non-empty string selects the final segment.
pub fn word_range_in_text(text: &str, offset: usize) -> Result<ByteRange, EditorError> {
    if offset > text.len() {
        return Err(EditorError::OutOfBounds {
            offset,
            len: text.len(),
        });
    }
    if !text.is_char_boundary(offset) {
        return Err(EditorError::NotCodepointBoundary { offset });
    }
    if text.is_empty() {
        return Ok(ByteRange::empty(ByteOffset::new(0)));
    }

    let target = if offset == text.len() {
        text.char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0)
    } else {
        offset
    };
    let Some((start, segment)) = text
        .split_word_bound_indices()
        .find(|(start, segment)| *start <= target && target < *start + segment.len())
    else {
        return Ok(ByteRange::empty(ByteOffset::new(offset)));
    };

    ByteRange::new(
        ByteOffset::new(start),
        ByteOffset::new(start + segment.len()),
    )
}

/// Returns the previous UAX #29 segment boundary, skipping whitespace between
/// words. The caller is expected to pass a bounded line-sized text slice.
pub fn previous_word_boundary_in_text(
    text: &str,
    offset: usize,
) -> Result<ByteOffset, EditorError> {
    validate_text_offset(text, offset)?;
    let mut cursor = offset;
    while cursor > 0 {
        let previous = text[..cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        let range = word_range_in_text(text, previous)?;
        let segment = &text[range.start().get()..range.end().get()];
        if !segment.chars().all(char::is_whitespace) {
            return Ok(range.start());
        }
        cursor = range.start().get();
    }
    Ok(ByteOffset::new(0))
}

/// Returns the next UAX #29 segment boundary, skipping leading whitespace.
/// The caller is expected to pass a bounded line-sized text slice.
pub fn next_word_boundary_in_text(text: &str, offset: usize) -> Result<ByteOffset, EditorError> {
    validate_text_offset(text, offset)?;
    let mut cursor = offset;
    while cursor < text.len() {
        let range = word_range_in_text(text, cursor)?;
        let segment = &text[range.start().get()..range.end().get()];
        cursor = range.end().get();
        if !segment.chars().all(char::is_whitespace) {
            return Ok(range.end());
        }
    }
    Ok(ByteOffset::new(text.len()))
}

fn validate_text_offset(text: &str, offset: usize) -> Result<(), EditorError> {
    if offset > text.len() {
        return Err(EditorError::OutOfBounds {
            offset,
            len: text.len(),
        });
    }
    if !text.is_char_boundary(offset) {
        return Err(EditorError::NotCodepointBoundary { offset });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EditOutcome {
    pub removed_bytes: usize,
    pub inserted_bytes: usize,
    pub selection: Selection,
}

/// Explicit edit classes that may be collapsed into one undo entry.
///
/// The caller owns the interaction boundary and must call
/// [`Editor::break_history_group`] after a pause or a non-edit command that
/// should end coalescing. Keeping the clock outside the editor makes grouping
/// deterministic in core and property tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditGroup {
    Typing,
    Backspace,
    DeleteForward,
}

struct HistoryEntry {
    before: Rope,
    after: Rope,
    selection_before: Selection,
    selection_after: Selection,
    cost_bytes: usize,
    group: Option<EditGroup>,
    group_epoch: u64,
}

pub struct Editor {
    text: Rope,
    selection: Selection,
    undo: VecDeque<HistoryEntry>,
    redo: VecDeque<HistoryEntry>,
    history_budget_bytes: usize,
    history_bytes: usize,
    history_group_epoch: u64,
}

#[derive(Clone)]
pub struct EditorSnapshot {
    text: Rope,
}

impl EditorSnapshot {
    pub fn len_bytes(&self) -> usize {
        self.text.len()
    }

    pub fn write_to(&self, mut writer: impl Write) -> io::Result<()> {
        for chunk in self.text.iter_chunks(..) {
            writer.write_all(chunk.as_bytes())?;
        }
        Ok(())
    }

    pub fn checksum_fnv1a(&self) -> u64 {
        self.text
            .iter_chunks(..)
            .fold(FNV_OFFSET_BASIS, |mut hash, chunk| {
                for byte in chunk.as_bytes() {
                    hash ^= u64::from(*byte);
                    hash = hash.wrapping_mul(FNV_PRIME);
                }
                hash
            })
    }
}

impl Default for Editor {
    fn default() -> Self {
        Self::new("")
    }
}

impl Editor {
    pub fn new(text: &str) -> Self {
        Self::with_history_budget(text, DEFAULT_HISTORY_BUDGET_BYTES)
    }

    pub fn with_history_budget(text: &str, history_budget_bytes: usize) -> Self {
        Self::from_rope(Rope::from(text), history_budget_bytes)
    }

    pub fn from_reader(reader: impl Read) -> Result<Self, EditorError> {
        Self::from_reader_with_history_budget(reader, DEFAULT_HISTORY_BUDGET_BYTES)
    }

    pub fn from_reader_with_history_budget(
        mut reader: impl Read,
        history_budget_bytes: usize,
    ) -> Result<Self, EditorError> {
        let mut builder: TreeBuilder<RopeInfo> = TreeBuilder::new();
        let mut chunk = [0_u8; STREAM_CHUNK_BYTES];
        let mut pending = Vec::with_capacity(STREAM_CHUNK_BYTES + 4);
        let mut accepted_bytes = 0_usize;

        loop {
            let read = reader
                .read(&mut chunk)
                .map_err(|error| EditorError::Io(error.to_string()))?;
            if read == 0 {
                break;
            }
            pending.extend_from_slice(&chunk[..read]);
            match std::str::from_utf8(&pending) {
                Ok(valid) => {
                    builder.push_str(valid);
                    accepted_bytes += pending.len();
                    pending.clear();
                }
                Err(error) if error.error_len().is_none() => {
                    let valid_len = error.valid_up_to();
                    let valid = std::str::from_utf8(&pending[..valid_len]).map_err(|_| {
                        EditorError::InvalidUtf8 {
                            valid_up_to: accepted_bytes,
                        }
                    })?;
                    builder.push_str(valid);
                    accepted_bytes += valid_len;
                    pending.drain(..valid_len);
                }
                Err(error) => {
                    return Err(EditorError::InvalidUtf8 {
                        valid_up_to: accepted_bytes + error.valid_up_to(),
                    });
                }
            }
        }

        if !pending.is_empty() {
            let tail = std::str::from_utf8(&pending).map_err(|error| EditorError::InvalidUtf8 {
                valid_up_to: accepted_bytes + error.valid_up_to(),
            })?;
            builder.push_str(tail);
        }
        Ok(Self::from_rope(builder.build(), history_budget_bytes))
    }

    fn from_rope(text: Rope, history_budget_bytes: usize) -> Self {
        Self {
            text,
            selection: Selection::default(),
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            history_budget_bytes,
            history_bytes: 0,
            history_group_epoch: 0,
        }
    }

    pub fn len_bytes(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.len() == 0
    }

    pub fn line_count(&self) -> usize {
        self.text.measure::<LinesMetric>() + 1
    }

    pub fn selection(&self) -> Selection {
        self.selection
    }

    pub fn set_selection(&mut self, selection: Selection) -> Result<(), EditorError> {
        self.validate_offset(selection.anchor())?;
        self.validate_offset(selection.focus())?;
        if self.selection != selection {
            self.break_history_group();
        }
        self.selection = selection;
        Ok(())
    }

    pub fn slice(&self, range: ByteRange) -> Result<String, EditorError> {
        self.validate_range(range)?;
        Ok(self.text.slice(range.start.0..range.end.0).to_string())
    }

    pub fn line_of_offset(&self, offset: ByteOffset) -> Result<usize, EditorError> {
        self.validate_offset(offset)?;
        Ok(self.text.line_of_offset(offset.0))
    }

    pub fn is_codepoint_boundary(&self, offset: ByteOffset) -> Result<bool, EditorError> {
        if offset.0 > self.text.len() {
            return Err(EditorError::OutOfBounds {
                offset: offset.0,
                len: self.text.len(),
            });
        }
        Ok(self.text.is_codepoint_boundary(offset.0))
    }

    pub fn offset_of_line(&self, line: usize) -> Option<ByteOffset> {
        (line < self.line_count()).then(|| ByteOffset(self.text.offset_of_line(line)))
    }

    pub fn previous_codepoint(
        &self,
        offset: ByteOffset,
    ) -> Result<Option<ByteOffset>, EditorError> {
        self.validate_offset(offset)?;
        Ok(self.text.prev_codepoint_offset(offset.0).map(ByteOffset))
    }

    pub fn next_codepoint(&self, offset: ByteOffset) -> Result<Option<ByteOffset>, EditorError> {
        self.validate_offset(offset)?;
        Ok(self.text.next_codepoint_offset(offset.0).map(ByteOffset))
    }

    pub fn previous_grapheme(&self, offset: ByteOffset) -> Result<Option<ByteOffset>, EditorError> {
        self.validate_offset(offset)?;
        Ok(self.text.prev_grapheme_offset(offset.0).map(ByteOffset))
    }

    pub fn next_grapheme(&self, offset: ByteOffset) -> Result<Option<ByteOffset>, EditorError> {
        self.validate_offset(offset)?;
        Ok(self.text.next_grapheme_offset(offset.0).map(ByteOffset))
    }

    pub fn insert(&mut self, offset: ByteOffset, text: &str) -> Result<EditOutcome, EditorError> {
        self.replace(ByteRange::empty(offset), text)
    }

    pub fn delete(&mut self, range: ByteRange) -> Result<EditOutcome, EditorError> {
        self.replace(range, "")
    }

    pub fn replace_selection(&mut self, text: &str) -> Result<EditOutcome, EditorError> {
        self.replace(self.selection.normalized(), text)
    }

    pub fn replace(
        &mut self,
        range: ByteRange,
        replacement: &str,
    ) -> Result<EditOutcome, EditorError> {
        self.replace_with_group(range, replacement, None, None)
    }

    pub fn replace_with_selection(
        &mut self,
        range: ByteRange,
        replacement: &str,
        selection_after: Selection,
    ) -> Result<EditOutcome, EditorError> {
        self.replace_with_group(range, replacement, None, Some(selection_after))
    }

    pub fn replace_grouped(
        &mut self,
        range: ByteRange,
        replacement: &str,
        group: EditGroup,
    ) -> Result<EditOutcome, EditorError> {
        let group = self
            .group_matches_edit(range, replacement, group)
            .then_some(group);
        self.replace_with_group(range, replacement, group, None)
    }

    fn replace_with_group(
        &mut self,
        range: ByteRange,
        replacement: &str,
        group: Option<EditGroup>,
        selection_after: Option<Selection>,
    ) -> Result<EditOutcome, EditorError> {
        self.validate_range(range)?;
        let selection_before = self.selection;
        let selection_after = selection_after
            .unwrap_or_else(|| Selection::caret(ByteOffset(range.start.0 + replacement.len())));
        let before = self.text.clone();
        let mut after = before.clone();
        after.edit(range.start.0..range.end.0, replacement);
        Self::validate_offset_in(&after, selection_after.anchor())?;
        Self::validate_offset_in(&after, selection_after.focus())?;
        let outcome = EditOutcome {
            removed_bytes: range.len(),
            inserted_bytes: replacement.len(),
            selection: selection_after,
        };
        if range.is_empty() && replacement.is_empty() {
            self.selection = selection_after;
            return Ok(outcome);
        }

        self.text = after;
        self.selection = selection_after;
        let entry = HistoryEntry {
            before,
            after: self.text.clone(),
            selection_before,
            selection_after,
            cost_bytes: HISTORY_ENTRY_OVERHEAD_BYTES
                .saturating_add(range.len())
                .saturating_add(replacement.len()),
            group,
            group_epoch: self.history_group_epoch,
        };
        self.push_history(entry);
        Ok(outcome)
    }

    pub fn break_history_group(&mut self) {
        self.history_group_epoch = self.history_group_epoch.wrapping_add(1);
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        self.break_history_group();
        let Some(entry) = self.undo.pop_back() else {
            return false;
        };
        self.text = entry.before.clone();
        self.selection = entry.selection_before;
        self.redo.push_back(entry);
        true
    }

    pub fn redo(&mut self) -> bool {
        self.break_history_group();
        let Some(entry) = self.redo.pop_back() else {
            return false;
        };
        self.text = entry.after.clone();
        self.selection = entry.selection_after;
        self.undo.push_back(entry);
        true
    }

    pub fn clear_history(&mut self) {
        self.break_history_group();
        self.undo.clear();
        self.redo.clear();
        self.history_bytes = 0;
    }

    pub fn history_budget_bytes(&self) -> usize {
        self.history_budget_bytes
    }

    pub fn history_bytes(&self) -> usize {
        self.history_bytes
    }

    pub fn set_history_budget_bytes(&mut self, history_budget_bytes: usize) {
        self.history_budget_bytes = history_budget_bytes;
        self.enforce_history_budget();
    }

    pub fn chunks(&self) -> impl Iterator<Item = &str> {
        self.text.iter_chunks(..)
    }

    /// Finds literal case-insensitive matches without materializing the rope
    /// as one contiguous string. Returned ranges always follow UTF-8 source
    /// boundaries, including when lowercase expansion emits several chars.
    pub fn find_case_insensitive(&self, query: &str, limit: usize) -> Vec<ByteRange> {
        let needle = query
            .chars()
            .flat_map(char::to_lowercase)
            .collect::<Vec<_>>();
        if needle.is_empty() || limit == 0 {
            return Vec::new();
        }

        let mut prefix = vec![0_usize; needle.len()];
        for index in 1..needle.len() {
            let mut matched = prefix[index - 1];
            while matched > 0 && needle[index] != needle[matched] {
                matched = prefix[matched - 1];
            }
            if needle[index] == needle[matched] {
                matched += 1;
            }
            prefix[index] = matched;
        }

        let mut matches = Vec::new();
        let mut source_window = VecDeque::with_capacity(needle.len());
        let mut matched = 0_usize;
        let mut chunk_start = 0_usize;
        'chunks: for chunk in self.chunks() {
            for (offset, source) in chunk.char_indices() {
                let source_start = chunk_start + offset;
                let source_end = source_start + source.len_utf8();
                for normalized in source.to_lowercase() {
                    while matched > 0 && normalized != needle[matched] {
                        matched = prefix[matched - 1];
                    }
                    if normalized == needle[matched] {
                        matched += 1;
                    }

                    source_window.push_back((source_start, source_end));
                    if source_window.len() > needle.len() {
                        source_window.pop_front();
                    }
                    if matched == needle.len() {
                        let start = source_window
                            .front()
                            .map(|(start, _)| *start)
                            .unwrap_or(source_start);
                        let range = ByteRange {
                            start: ByteOffset::new(start),
                            end: ByteOffset::new(source_end),
                        };
                        if matches.last().copied() != Some(range) {
                            matches.push(range);
                            if matches.len() == limit {
                                break 'chunks;
                            }
                        }
                        matched = prefix[matched - 1];
                    }
                }
            }
            chunk_start += chunk.len();
        }
        matches
    }

    pub fn snapshot(&self) -> EditorSnapshot {
        EditorSnapshot {
            text: self.text.clone(),
        }
    }

    pub fn write_to(&self, mut writer: impl Write) -> io::Result<()> {
        for chunk in self.chunks() {
            writer.write_all(chunk.as_bytes())?;
        }
        Ok(())
    }

    pub fn checksum_fnv1a(&self) -> u64 {
        self.chunks().fold(FNV_OFFSET_BASIS, |mut hash, chunk| {
            for byte in chunk.as_bytes() {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(FNV_PRIME);
            }
            hash
        })
    }

    fn validate_range(&self, range: ByteRange) -> Result<(), EditorError> {
        self.validate_offset(range.start)?;
        self.validate_offset(range.end)
    }

    fn validate_offset(&self, offset: ByteOffset) -> Result<(), EditorError> {
        Self::validate_offset_in(&self.text, offset)
    }

    fn validate_offset_in(text: &Rope, offset: ByteOffset) -> Result<(), EditorError> {
        if offset.0 > text.len() {
            return Err(EditorError::OutOfBounds {
                offset: offset.0,
                len: text.len(),
            });
        }
        if !text.is_codepoint_boundary(offset.0) {
            return Err(EditorError::NotCodepointBoundary { offset: offset.0 });
        }
        Ok(())
    }

    fn push_history(&mut self, entry: HistoryEntry) {
        while let Some(redo) = self.redo.pop_front() {
            self.history_bytes = self.history_bytes.saturating_sub(redo.cost_bytes);
        }
        let coalesces = entry.group.is_some()
            && self.undo.back().is_some_and(|previous| {
                previous.group == entry.group
                    && previous.group_epoch == entry.group_epoch
                    && previous.selection_after == entry.selection_before
                    && previous.selection_after.is_caret()
                    && entry.selection_before.is_caret()
            });
        if coalesces && let Some(previous) = self.undo.back_mut() {
            let added_cost = entry
                .cost_bytes
                .saturating_sub(HISTORY_ENTRY_OVERHEAD_BYTES);
            previous.after = entry.after;
            previous.selection_after = entry.selection_after;
            previous.cost_bytes = previous.cost_bytes.saturating_add(added_cost);
            self.history_bytes = self.history_bytes.saturating_add(added_cost);
            self.enforce_history_budget();
            return;
        }
        self.history_bytes = self.history_bytes.saturating_add(entry.cost_bytes);
        self.undo.push_back(entry);
        self.enforce_history_budget();
    }

    fn group_matches_edit(&self, range: ByteRange, replacement: &str, group: EditGroup) -> bool {
        if !self.selection.is_caret() {
            return false;
        }
        match group {
            EditGroup::Typing => {
                range.is_empty()
                    && range.start() == self.selection.focus()
                    && !replacement.is_empty()
                    && !replacement.chars().any(char::is_whitespace)
            }
            EditGroup::Backspace => {
                replacement.is_empty() && !range.is_empty() && range.end() == self.selection.focus()
            }
            EditGroup::DeleteForward => {
                replacement.is_empty()
                    && !range.is_empty()
                    && range.start() == self.selection.focus()
            }
        }
    }

    fn enforce_history_budget(&mut self) {
        while self.history_bytes > self.history_budget_bytes {
            let removed = self.undo.pop_front().or_else(|| self.redo.pop_front());
            let Some(removed) = removed else {
                self.history_bytes = 0;
                break;
            };
            self.history_bytes = self.history_bytes.saturating_sub(removed.cost_bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn text(editor: &Editor) -> String {
        let mut output = Vec::new();
        editor.write_to(&mut output).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn finds_case_insensitive_matches_across_rope_chunks_with_a_limit() {
        let mut body = "a".repeat(STREAM_CHUNK_BYTES - 2);
        body.push_str("NeEdLe needle NEEDLE");
        let editor = Editor::new(&body);

        let matches = editor.find_case_insensitive("needle", 2);

        assert_eq!(matches.len(), 2);
        assert_eq!(editor.slice(matches[0]).unwrap(), "NeEdLe");
        assert_eq!(editor.slice(matches[1]).unwrap(), "needle");
    }

    #[test]
    fn finds_unicode_matches_and_returns_source_codepoint_boundaries() {
        let editor = Editor::new("ПрИвЕт, привет; İ İ.");

        let cyrillic = editor.find_case_insensitive("ПРИВЕТ", 10);
        assert_eq!(cyrillic.len(), 2);
        assert_eq!(editor.slice(cyrillic[0]).unwrap(), "ПрИвЕт");
        assert_eq!(editor.slice(cyrillic[1]).unwrap(), "привет");

        let expanded = editor.find_case_insensitive("i\u{307}", 10);
        assert_eq!(expanded.len(), 2);
        assert!(
            expanded
                .iter()
                .all(|range| editor.slice(*range).unwrap() == "İ")
        );
    }

    #[test]
    fn find_supports_overlaps_and_empty_inputs() {
        let editor = Editor::new("aaaa");
        let matches = editor.find_case_insensitive("aa", 10);

        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].start().get(), 0);
        assert_eq!(matches[1].start().get(), 1);
        assert_eq!(matches[2].start().get(), 2);
        assert!(editor.find_case_insensitive("", 10).is_empty());
        assert!(editor.find_case_insensitive("a", 0).is_empty());
    }

    #[test]
    fn validates_boundaries_and_keeps_codepoint_and_grapheme_moves_explicit() {
        let mut editor = Editor::new("a🦀e\u{301}\nnext");
        assert_eq!(
            editor.previous_codepoint(ByteOffset::new(5)).unwrap(),
            Some(ByteOffset::new(1))
        );
        assert_eq!(
            editor.next_codepoint(ByteOffset::new(1)).unwrap(),
            Some(ByteOffset::new(5))
        );
        assert_eq!(
            editor.previous_grapheme(ByteOffset::new(8)).unwrap(),
            Some(ByteOffset::new(5))
        );
        assert_eq!(
            editor.next_grapheme(ByteOffset::new(5)).unwrap(),
            Some(ByteOffset::new(8))
        );
        assert_eq!(editor.line_count(), 2);
        assert_eq!(editor.line_of_offset(ByteOffset::new(9)).unwrap(), 1);
        assert_eq!(editor.offset_of_line(1), Some(ByteOffset::new(9)));
        assert_eq!(editor.offset_of_line(2), None);

        let before = text(&editor);
        let error = editor.insert(ByteOffset::new(2), "x").unwrap_err();
        assert_eq!(error, EditorError::NotCodepointBoundary { offset: 2 });
        assert_eq!(text(&editor), before);
        assert!(!editor.can_undo());

        let error = editor
            .set_selection(Selection::caret(ByteOffset::new(100)))
            .unwrap_err();
        assert_eq!(
            error,
            EditorError::OutOfBounds {
                offset: 100,
                len: before.len()
            }
        );
        assert_eq!(editor.selection(), Selection::default());
    }

    fn selected_word(text: &str, offset: usize) -> &str {
        let range = word_range_in_text(text, offset).unwrap();
        &text[range.start().get()..range.end().get()]
    }

    #[test]
    fn unicode_word_ranges_preserve_graphemes_and_uax_segments() {
        let text = "Привет, can't 29.3 e\u{301}lan 👩‍💻";

        assert_eq!(selected_word(text, text.find('и').unwrap()), "Привет");
        assert_eq!(
            selected_word(text, text.find("can't").unwrap() + 3),
            "can't"
        );
        assert_eq!(selected_word(text, text.find("29.3").unwrap() + 2), "29.3");
        assert_eq!(
            selected_word(text, text.find("lan").unwrap()),
            "e\u{301}lan"
        );
        assert_eq!(selected_word(text, text.find('💻').unwrap()), "👩‍💻");
    }

    #[test]
    fn markdown_punctuation_and_whitespace_remain_separate_segments() {
        let text = "**bold** [label](url)  \t";

        assert_eq!(selected_word(text, 0), "*");
        assert_eq!(selected_word(text, text.find("bold").unwrap() + 2), "bold");
        assert_eq!(selected_word(text, text.find("label").unwrap()), "label");
        assert_eq!(selected_word(text, text.find("url").unwrap() + 1), "url");
        assert_eq!(selected_word(text, text.find(")  ").unwrap() + 1), "  ");
        assert_eq!(selected_word(text, text.find('\t').unwrap()), "\t");
        assert_eq!(selected_word("final", "final".len()), "final");
    }

    #[test]
    fn word_range_rejects_invalid_offsets_without_mutating_input() {
        let text = String::from("éclair");
        let original = text.clone();

        assert_eq!(
            word_range_in_text(&text, 1),
            Err(EditorError::NotCodepointBoundary { offset: 1 })
        );
        assert_eq!(
            word_range_in_text(&text, text.len() + 1),
            Err(EditorError::OutOfBounds {
                offset: text.len() + 1,
                len: text.len(),
            })
        );
        assert_eq!(
            word_range_in_text("", 0).unwrap(),
            ByteRange::empty(ByteOffset::new(0))
        );
        assert_eq!(text, original);
    }

    #[test]
    fn word_navigation_skips_whitespace_and_preserves_unicode_segments() {
        let text = "Привет  e\u{301}lan 👩‍💻!";
        let hello_end = "Привет".len();
        let elan_start = text.find('e').unwrap();
        let elan_end = elan_start + "e\u{301}lan".len();
        let emoji_start = text.find('👩').unwrap();
        let emoji_end = emoji_start + "👩‍💻".len();

        assert_eq!(
            next_word_boundary_in_text(text, 0).unwrap(),
            ByteOffset::new(hello_end)
        );
        assert_eq!(
            next_word_boundary_in_text(text, hello_end).unwrap(),
            ByteOffset::new(elan_end)
        );
        assert_eq!(
            next_word_boundary_in_text(text, elan_start + 1).unwrap(),
            ByteOffset::new(elan_end)
        );
        assert_eq!(
            previous_word_boundary_in_text(text, emoji_end).unwrap(),
            ByteOffset::new(emoji_start)
        );
        assert_eq!(
            previous_word_boundary_in_text(text, elan_start).unwrap(),
            ByteOffset::new(0)
        );
        assert_eq!(
            next_word_boundary_in_text(text, text.len()).unwrap(),
            ByteOffset::new(text.len())
        );
    }

    #[test]
    fn reversed_selection_replaces_to_caret_and_undo_restores_direction() {
        let mut editor = Editor::new("zero Привет end");
        let anchor = ByteOffset::new("zero Привет".len());
        let focus = ByteOffset::new("zero ".len());
        let reversed = Selection::new(anchor, focus);
        editor.set_selection(reversed).unwrap();
        let outcome = editor.replace_selection("мир").unwrap();
        assert_eq!(text(&editor), "zero мир end");
        assert_eq!(
            outcome.selection,
            Selection::caret(ByteOffset::new("zero мир".len()))
        );
        assert_eq!(editor.selection(), outcome.selection);

        assert!(editor.undo());
        assert_eq!(text(&editor), "zero Привет end");
        assert_eq!(editor.selection(), reversed);
        assert!(editor.redo());
        assert_eq!(text(&editor), "zero мир end");
        assert_eq!(editor.selection(), outcome.selection);
    }

    #[test]
    fn atomic_replace_keeps_explicit_selection_boundaries_and_one_history_entry() {
        let original = "α task\nβ";
        let range = ByteRange::new(ByteOffset::new(0), ByteOffset::new("α".len())).unwrap();
        let replacement = "- [x]";
        let expected = "- [x] task\nβ";

        for reversed in [false, true] {
            let mut editor = Editor::new(original);
            let selection_before = if reversed {
                Selection::new(ByteOffset::new(original.len()), ByteOffset::new("α".len()))
            } else {
                Selection::new(ByteOffset::new("α".len()), ByteOffset::new(original.len()))
            };
            let selection_after = if reversed {
                Selection::new(
                    ByteOffset::new(expected.len()),
                    ByteOffset::new(replacement.len()),
                )
            } else {
                Selection::new(
                    ByteOffset::new(replacement.len()),
                    ByteOffset::new(expected.len()),
                )
            };
            editor.set_selection(selection_before).unwrap();

            let outcome = editor
                .replace_with_selection(range, replacement, selection_after)
                .unwrap();
            assert_eq!(text(&editor), expected);
            assert_eq!(outcome.selection, selection_after);
            assert_eq!(editor.selection(), selection_after);

            assert!(editor.undo());
            assert_eq!(text(&editor), original);
            assert_eq!(editor.selection(), selection_before);
            assert!(!editor.undo());
            assert!(editor.redo());
            assert_eq!(text(&editor), expected);
            assert_eq!(editor.selection(), selection_after);
            assert!(!editor.redo());
        }

        let mut editor = Editor::new("task");
        let error = editor
            .replace_with_selection(
                ByteRange::empty(ByteOffset::new(0)),
                "🦀",
                Selection::caret(ByteOffset::new(1)),
            )
            .unwrap_err();
        assert_eq!(error, EditorError::NotCodepointBoundary { offset: 1 });
        assert_eq!(text(&editor), "task");
        assert!(!editor.can_undo());
    }

    #[test]
    fn empty_document_noop_and_invalid_range_are_safe() {
        let mut editor = Editor::default();
        assert!(editor.is_empty());
        assert_eq!(editor.line_count(), 1);
        let outcome = editor.replace_selection("").unwrap();
        assert_eq!(outcome.removed_bytes, 0);
        assert_eq!(outcome.inserted_bytes, 0);
        assert!(!editor.can_undo());
        assert_eq!(editor.previous_codepoint(ByteOffset::new(0)).unwrap(), None);
        assert_eq!(editor.next_codepoint(ByteOffset::new(0)).unwrap(), None);
        assert!(matches!(
            ByteRange::new(ByteOffset::new(1), ByteOffset::new(0)),
            Err(EditorError::InvalidRange { start: 1, end: 0 })
        ));
    }

    #[test]
    fn history_budget_evicts_oldest_entries_and_new_edit_clears_redo() {
        let two_entries = HISTORY_ENTRY_OVERHEAD_BYTES * 2 + 2;
        let mut editor = Editor::with_history_budget("", two_entries);
        editor.insert(ByteOffset::new(0), "a").unwrap();
        editor.insert(ByteOffset::new(1), "b").unwrap();
        editor.insert(ByteOffset::new(2), "c").unwrap();
        assert!(editor.history_bytes() <= two_entries);
        assert!(editor.undo());
        assert!(editor.undo());
        assert!(!editor.undo());
        assert_eq!(text(&editor), "a");
        assert!(editor.can_redo());

        editor.insert(ByteOffset::new(1), "!").unwrap();
        assert_eq!(text(&editor), "a!");
        assert!(!editor.can_redo());

        editor.set_history_budget_bytes(0);
        assert_eq!(editor.history_bytes(), 0);
        assert!(!editor.can_undo());
        editor.insert(ByteOffset::new(2), "x").unwrap();
        assert_eq!(text(&editor), "a!x");
        assert!(!editor.can_undo());
    }

    #[test]
    fn grouped_typing_and_deletion_undo_as_interaction_units() {
        let mut editor = Editor::new("");
        for character in ["п", "р", "и", "в", "е", "т"] {
            let caret = editor.selection().focus();
            editor
                .replace_grouped(ByteRange::empty(caret), character, EditGroup::Typing)
                .unwrap();
        }
        assert_eq!(text(&editor), "привет");
        assert!(editor.undo());
        assert_eq!(text(&editor), "");
        assert!(editor.redo());
        assert_eq!(text(&editor), "привет");

        editor.break_history_group();
        for _ in 0..2 {
            let caret = editor.selection().focus();
            let previous = editor.previous_grapheme(caret).unwrap().unwrap();
            editor
                .replace_grouped(
                    ByteRange::new(previous, caret).unwrap(),
                    "",
                    EditGroup::Backspace,
                )
                .unwrap();
        }
        assert_eq!(text(&editor), "прив");
        assert!(editor.undo());
        assert_eq!(text(&editor), "привет");
    }

    #[test]
    fn whitespace_selection_and_explicit_break_end_typing_group() {
        let mut editor = Editor::new("");
        for character in ["a", "b"] {
            let caret = editor.selection().focus();
            editor
                .replace_grouped(ByteRange::empty(caret), character, EditGroup::Typing)
                .unwrap();
        }
        let caret = editor.selection().focus();
        editor
            .replace_grouped(ByteRange::empty(caret), " ", EditGroup::Typing)
            .unwrap();
        for character in ["c", "d"] {
            let caret = editor.selection().focus();
            editor
                .replace_grouped(ByteRange::empty(caret), character, EditGroup::Typing)
                .unwrap();
        }
        assert_eq!(text(&editor), "ab cd");
        assert!(editor.undo());
        assert_eq!(text(&editor), "ab ");
        assert!(editor.undo());
        assert_eq!(text(&editor), "ab");

        editor.break_history_group();
        let caret = editor.selection().focus();
        editor
            .replace_grouped(ByteRange::empty(caret), "x", EditGroup::Typing)
            .unwrap();
        editor.break_history_group();
        let caret = editor.selection().focus();
        editor
            .replace_grouped(ByteRange::empty(caret), "y", EditGroup::Typing)
            .unwrap();
        assert!(editor.undo());
        assert_eq!(text(&editor), "abx");

        editor
            .set_selection(Selection::new(ByteOffset::new(0), ByteOffset::new(2)))
            .unwrap();
        editor
            .replace_grouped(editor.selection().normalized(), "z", EditGroup::Typing)
            .unwrap();
        assert_eq!(text(&editor), "zx");
        assert!(editor.undo());
        assert_eq!(text(&editor), "abx");
    }

    #[test]
    fn streaming_load_handles_split_unicode_and_reports_invalid_byte() {
        let input = "start 🦀 Привет e\u{301} end";
        let editor = Editor::from_reader(ChunkedReader::new(input.as_bytes(), 1)).unwrap();
        assert_eq!(text(&editor), input);
        assert_eq!(editor.checksum_fnv1a(), fnv(input.as_bytes()));

        let invalid = match Editor::from_reader(Cursor::new(b"abc\xfftail")) {
            Ok(_) => panic!("invalid UTF-8 unexpectedly loaded"),
            Err(error) => error,
        };
        assert_eq!(invalid, EditorError::InvalidUtf8 { valid_up_to: 3 });
        let truncated = match Editor::from_reader(Cursor::new(b"abc\xf0\x9f")) {
            Ok(_) => panic!("truncated UTF-8 unexpectedly loaded"),
            Err(error) => error,
        };
        assert_eq!(truncated, EditorError::InvalidUtf8 { valid_up_to: 3 });
    }

    #[test]
    fn snapshot_is_immutable_and_streams_without_a_full_string_copy() {
        let mut editor = Editor::new("before 🦀\n");
        let snapshot = editor.snapshot();
        editor
            .insert(ByteOffset::new(editor.len_bytes()), "after")
            .unwrap();

        let mut output = Vec::new();
        snapshot.write_to(&mut output).unwrap();
        assert_eq!(output, "before 🦀\n".as_bytes());
        assert_eq!(snapshot.len_bytes(), output.len());
        assert_eq!(snapshot.checksum_fnv1a(), fnv(&output));
        assert_ne!(snapshot.checksum_fnv1a(), editor.checksum_fnv1a());
    }

    #[test]
    fn deterministic_operations_match_reference_string_model() {
        let initial = "α beta 🦀 e\u{301}\n";
        let mut editor = Editor::with_history_budget(initial, 32 * 1024 * 1024);
        let mut reference = ReferenceEditor::new(initial);
        let mut random = Lcg::new(0x5eed_cafe_f00d_beef);
        let payloads = ["x", "Ж", "🦀", "e\u{301}", "\nline\n"];

        for _ in 0..2_000 {
            let boundaries = char_boundaries(&reference.text);
            let operation = if reference.text.len() > 512 {
                1
            } else {
                random.next_usize(7)
            };
            match operation {
                0 => {
                    let at = boundaries[random.next_usize(boundaries.len())];
                    let payload = payloads[random.next_usize(payloads.len())];
                    editor.insert(ByteOffset::new(at), payload).unwrap();
                    reference.replace(at, at, payload);
                }
                1 => {
                    let (start, end) = random_range(&boundaries, &mut random);
                    editor
                        .delete(
                            ByteRange::new(ByteOffset::new(start), ByteOffset::new(end)).unwrap(),
                        )
                        .unwrap();
                    reference.replace(start, end, "");
                }
                2 => {
                    let (start, end) = random_range(&boundaries, &mut random);
                    let payload = payloads[random.next_usize(payloads.len())];
                    editor
                        .replace(
                            ByteRange::new(ByteOffset::new(start), ByteOffset::new(end)).unwrap(),
                            payload,
                        )
                        .unwrap();
                    reference.replace(start, end, payload);
                }
                3 => {
                    let anchor = boundaries[random.next_usize(boundaries.len())];
                    let focus = boundaries[random.next_usize(boundaries.len())];
                    editor
                        .set_selection(Selection::new(
                            ByteOffset::new(anchor),
                            ByteOffset::new(focus),
                        ))
                        .unwrap();
                    reference.selection = (anchor, focus);
                }
                4 => {
                    let payload = payloads[random.next_usize(payloads.len())];
                    editor.replace_selection(payload).unwrap();
                    reference.replace_selection(payload);
                }
                5 => assert_eq!(editor.undo(), reference.undo()),
                _ => assert_eq!(editor.redo(), reference.redo()),
            }
            assert_eq!(text(&editor), reference.text);
            assert_eq!(
                editor.selection(),
                Selection::new(
                    ByteOffset::new(reference.selection.0),
                    ByteOffset::new(reference.selection.1)
                )
            );
            assert_eq!(editor.can_undo(), !reference.undo.is_empty());
            assert_eq!(editor.can_redo(), !reference.redo.is_empty());
        }
    }

    fn char_boundaries(text: &str) -> Vec<usize> {
        text.char_indices()
            .map(|(offset, _)| offset)
            .chain(std::iter::once(text.len()))
            .collect()
    }

    fn random_range(boundaries: &[usize], random: &mut Lcg) -> (usize, usize) {
        let first = boundaries[random.next_usize(boundaries.len())];
        let second = boundaries[random.next_usize(boundaries.len())];
        (first.min(second), first.max(second))
    }

    fn fnv(bytes: &[u8]) -> u64 {
        bytes.iter().fold(FNV_OFFSET_BASIS, |mut hash, byte| {
            hash ^= u64::from(*byte);
            hash.wrapping_mul(FNV_PRIME)
        })
    }

    struct ChunkedReader<'a> {
        bytes: &'a [u8],
        position: usize,
        chunk_size: usize,
    }

    impl<'a> ChunkedReader<'a> {
        fn new(bytes: &'a [u8], chunk_size: usize) -> Self {
            Self {
                bytes,
                position: 0,
                chunk_size,
            }
        }
    }

    impl Read for ChunkedReader<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.position == self.bytes.len() {
                return Ok(0);
            }
            let read = self
                .chunk_size
                .min(buffer.len())
                .min(self.bytes.len() - self.position);
            buffer[..read].copy_from_slice(&self.bytes[self.position..self.position + read]);
            self.position += read;
            Ok(read)
        }
    }

    struct Lcg(u64);

    impl Lcg {
        fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next_usize(&mut self, upper: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 as usize) % upper
        }
    }

    #[derive(Clone)]
    struct ReferenceEntry {
        before: String,
        after: String,
        selection_before: (usize, usize),
        selection_after: (usize, usize),
    }

    struct ReferenceEditor {
        text: String,
        selection: (usize, usize),
        undo: Vec<ReferenceEntry>,
        redo: Vec<ReferenceEntry>,
    }

    impl ReferenceEditor {
        fn new(text: &str) -> Self {
            Self {
                text: text.to_owned(),
                selection: (0, 0),
                undo: Vec::new(),
                redo: Vec::new(),
            }
        }

        fn replace(&mut self, start: usize, end: usize, replacement: &str) {
            let selection_after = (start + replacement.len(), start + replacement.len());
            if start == end && replacement.is_empty() {
                self.selection = selection_after;
                return;
            }
            let before = self.text.clone();
            let selection_before = self.selection;
            self.text.replace_range(start..end, replacement);
            self.selection = selection_after;
            self.undo.push(ReferenceEntry {
                before,
                after: self.text.clone(),
                selection_before,
                selection_after,
            });
            self.redo.clear();
        }

        fn replace_selection(&mut self, replacement: &str) {
            let start = self.selection.0.min(self.selection.1);
            let end = self.selection.0.max(self.selection.1);
            self.replace(start, end, replacement);
        }

        fn undo(&mut self) -> bool {
            let Some(entry) = self.undo.pop() else {
                return false;
            };
            self.text = entry.before.clone();
            self.selection = entry.selection_before;
            self.redo.push(entry);
            true
        }

        fn redo(&mut self) -> bool {
            let Some(entry) = self.redo.pop() else {
                return false;
            };
            self.text = entry.after.clone();
            self.selection = entry.selection_after;
            self.undo.push(entry);
            true
        }
    }
}
