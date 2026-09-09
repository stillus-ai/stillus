// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read};

pub const MAX_FRONT_MATTER_BYTES: usize = 65_536;
const READ_CHUNK_BYTES: usize = 1_024;
const BODY_SEPARATOR_LOOKAHEAD_BYTES: usize = 2;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NoteMetadata {
    pub favorited: Option<bool>,
    pub pinned: Option<bool>,
    pub deleted: Option<bool>,
    pub tags: Vec<String>,
    pub title: Option<String>,
    pub created: Option<String>,
    pub modified: Option<String>,
    pub order: BTreeMap<String, u32>,
    pub encryption: Option<EncryptionFormat>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncryptionFormat {
    AgeBodyV1,
}

impl EncryptionFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgeBodyV1 => "age-body-v1",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EncryptionPatch {
    #[default]
    Keep,
    Set(EncryptionFormat),
    Remove,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedFrontMatter {
    pub metadata: NoteMetadata,
    pub source: String,
    /// Offset of editor-visible Markdown after one optional blank separator.
    pub body_offset: u64,
    pub line_ending: LineEnding,
    pub closing_has_line_ending: bool,
    pub body_separator: Option<LineEnding>,
    pub present_fields: Vec<MetadataField>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataField {
    Favorited,
    Pinned,
    Deleted,
    Tags,
    Title,
    Created,
    Modified,
    Order,
    Encryption,
}

impl MetadataField {
    fn key(self) -> &'static str {
        match self {
            Self::Favorited => "favorited",
            Self::Pinned => "pinned",
            Self::Deleted => "deleted",
            Self::Tags => "tags",
            Self::Title => "title",
            Self::Created => "created",
            Self::Modified => "modified",
            Self::Order => "order",
            Self::Encryption => "stillus_encryption",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrontMatterIssue {
    InvalidUtf8,
    MalformedYaml(String),
    Unclosed,
    TooLarge,
}

impl fmt::Display for FrontMatterIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8 => formatter.write_str("front matter is not valid UTF-8"),
            Self::MalformedYaml(error) => write!(formatter, "malformed YAML: {error}"),
            Self::Unclosed => formatter.write_str("front matter has no closing delimiter"),
            Self::TooLarge => write!(
                formatter,
                "front matter exceeds {MAX_FRONT_MATTER_BYTES} bytes"
            ),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrontMatterStatus {
    Plain,
    Parsed(ParsedFrontMatter),
    Invalid {
        issue: FrontMatterIssue,
        body_offset: Option<u64>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrontMatterScan {
    pub status: FrontMatterStatus,
    pub bytes_read: usize,
    pub line_ending: LineEnding,
}

pub fn scan_reader(reader: impl Read) -> io::Result<FrontMatterScan> {
    let mut reader =
        reader.take((MAX_FRONT_MATTER_BYTES + 1 + BODY_SEPARATOR_LOOKAHEAD_BYTES) as u64);
    let mut bytes = Vec::with_capacity(READ_CHUNK_BYTES);
    let mut chunk = [0_u8; READ_CHUNK_BYTES];

    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return finish_at_eof(&bytes);
        }
        bytes.extend_from_slice(&chunk[..read]);

        match opening_end(&bytes) {
            Opening::NeedMore => continue,
            Opening::Plain => {
                if let Some(line_ending) = first_line_ending(&bytes) {
                    return Ok(FrontMatterScan {
                        status: FrontMatterStatus::Plain,
                        bytes_read: bytes.len(),
                        line_ending,
                    });
                }
                if bytes.len() <= MAX_FRONT_MATTER_BYTES {
                    continue;
                }
                return Ok(FrontMatterScan {
                    status: FrontMatterStatus::Plain,
                    bytes_read: bytes.len(),
                    line_ending: LineEnding::Lf,
                });
            }
            Opening::FrontMatter(opening_end) => {
                if let Some((yaml_end, front_matter_end)) =
                    closing_delimiter(&bytes, opening_end, false)
                {
                    if front_matter_end > MAX_FRONT_MATTER_BYTES {
                        return too_large_scan(bytes.len(), opening_end);
                    }
                    if let Some((body_offset, body_separator)) =
                        body_boundary(&bytes, front_matter_end, false)
                    {
                        return parse_front_matter(
                            &bytes,
                            opening_end,
                            yaml_end,
                            front_matter_end,
                            body_offset,
                            body_separator,
                        );
                    }
                    continue;
                }
                if bytes.len() > MAX_FRONT_MATTER_BYTES {
                    return too_large_scan(bytes.len(), opening_end);
                }
            }
        }
    }
}

fn finish_at_eof(bytes: &[u8]) -> io::Result<FrontMatterScan> {
    let status = match opening_end(bytes) {
        Opening::Plain | Opening::NeedMore => FrontMatterStatus::Plain,
        Opening::FrontMatter(opening_end) => {
            if let Some((yaml_end, front_matter_end)) = closing_delimiter(bytes, opening_end, true)
            {
                if front_matter_end > MAX_FRONT_MATTER_BYTES {
                    return too_large_scan(bytes.len(), opening_end);
                }
                let (body_offset, body_separator) = body_boundary(bytes, front_matter_end, true)
                    .expect("EOF always resolves the body boundary");
                return parse_front_matter(
                    bytes,
                    opening_end,
                    yaml_end,
                    front_matter_end,
                    body_offset,
                    body_separator,
                );
            }
            FrontMatterStatus::Invalid {
                issue: FrontMatterIssue::Unclosed,
                body_offset: None,
            }
        }
    };
    Ok(FrontMatterScan {
        status,
        bytes_read: bytes.len(),
        line_ending: first_line_ending(bytes).unwrap_or(LineEnding::Lf),
    })
}

fn too_large_scan(bytes_read: usize, yaml_start: usize) -> io::Result<FrontMatterScan> {
    Ok(FrontMatterScan {
        status: FrontMatterStatus::Invalid {
            issue: FrontMatterIssue::TooLarge,
            body_offset: None,
        },
        bytes_read,
        line_ending: opening_line_ending(yaml_start),
    })
}

fn parse_front_matter(
    bytes: &[u8],
    yaml_start: usize,
    yaml_end: usize,
    front_matter_end: usize,
    body_offset: usize,
    body_separator: Option<LineEnding>,
) -> io::Result<FrontMatterScan> {
    let status = match std::str::from_utf8(&bytes[yaml_start..yaml_end]) {
        Err(_) => FrontMatterStatus::Invalid {
            issue: FrontMatterIssue::InvalidUtf8,
            body_offset: Some(body_offset as u64),
        },
        Ok(source) => match serde_yaml_ng::from_str::<serde_yaml_ng::Value>(source) {
            Err(error) => FrontMatterStatus::Invalid {
                issue: FrontMatterIssue::MalformedYaml(error.to_string()),
                body_offset: Some(body_offset as u64),
            },
            Ok(value) => match extract_metadata(value) {
                Err(error) => FrontMatterStatus::Invalid {
                    issue: FrontMatterIssue::MalformedYaml(error),
                    body_offset: Some(body_offset as u64),
                },
                Ok((metadata, present_fields)) => FrontMatterStatus::Parsed(ParsedFrontMatter {
                    metadata,
                    source: source.to_owned(),
                    body_offset: body_offset as u64,
                    line_ending: if yaml_start == 5 {
                        LineEnding::Crlf
                    } else {
                        LineEnding::Lf
                    },
                    closing_has_line_ending: front_matter_end > yaml_end + 3,
                    body_separator,
                    present_fields,
                }),
            },
        },
    };
    Ok(FrontMatterScan {
        status,
        bytes_read: bytes.len(),
        line_ending: opening_line_ending(yaml_start),
    })
}

fn opening_line_ending(yaml_start: usize) -> LineEnding {
    if yaml_start == 5 {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    }
}

fn first_line_ending(bytes: &[u8]) -> Option<LineEnding> {
    let newline = bytes.iter().position(|byte| *byte == b'\n')?;
    Some(if newline > 0 && bytes[newline - 1] == b'\r' {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    })
}

fn extract_metadata(
    value: serde_yaml_ng::Value,
) -> Result<(NoteMetadata, Vec<MetadataField>), String> {
    let serde_yaml_ng::Value::Mapping(mapping) = value else {
        return Err("front matter root must be a mapping".to_owned());
    };
    let known_fields = [
        MetadataField::Favorited,
        MetadataField::Pinned,
        MetadataField::Deleted,
        MetadataField::Tags,
        MetadataField::Title,
        MetadataField::Created,
        MetadataField::Modified,
        MetadataField::Order,
        MetadataField::Encryption,
    ];
    let present_fields = known_fields
        .iter()
        .copied()
        .filter(|field| mapping.contains_key(serde_yaml_ng::Value::String(field.key().to_owned())))
        .collect::<Vec<_>>();
    Ok((
        NoteMetadata {
            favorited: optional_boolean(&mapping, MetadataField::Favorited)?,
            pinned: optional_boolean(&mapping, MetadataField::Pinned)?,
            deleted: optional_boolean(&mapping, MetadataField::Deleted)?,
            tags: tags(&mapping)?,
            title: optional_string(&mapping, MetadataField::Title)?,
            created: optional_string(&mapping, MetadataField::Created)?,
            modified: optional_string(&mapping, MetadataField::Modified)?,
            order: note_order(&mapping)?,
            encryption: encryption_format(&mapping)?,
        },
        present_fields,
    ))
}

fn encryption_format(mapping: &serde_yaml_ng::Mapping) -> Result<Option<EncryptionFormat>, String> {
    let Some(value) = field_value(mapping, MetadataField::Encryption) else {
        return Ok(None);
    };
    match value {
        serde_yaml_ng::Value::String(value) if value == EncryptionFormat::AgeBodyV1.as_str() => {
            Ok(Some(EncryptionFormat::AgeBodyV1))
        }
        serde_yaml_ng::Value::Null => Ok(None),
        _ => Err(format!(
            "metadata field `{}` must be `{}`",
            MetadataField::Encryption.key(),
            EncryptionFormat::AgeBodyV1.as_str()
        )),
    }
}

fn field_value(
    mapping: &serde_yaml_ng::Mapping,
    field: MetadataField,
) -> Option<&serde_yaml_ng::Value> {
    mapping.get(serde_yaml_ng::Value::String(field.key().to_owned()))
}

fn optional_string(
    mapping: &serde_yaml_ng::Mapping,
    field: MetadataField,
) -> Result<Option<String>, String> {
    let Some(value) = field_value(mapping, field) else {
        return Ok(None);
    };
    match value {
        serde_yaml_ng::Value::Null => Ok(None),
        serde_yaml_ng::Value::String(value) => Ok(Some(value.clone())),
        serde_yaml_ng::Value::Number(value) => Ok(Some(value.to_string())),
        serde_yaml_ng::Value::Bool(value) => Ok(Some(value.to_string())),
        _ => Err(format!(
            "metadata field `{}` must be a scalar string, number, boolean, or null",
            field.key()
        )),
    }
}

fn optional_boolean(
    mapping: &serde_yaml_ng::Mapping,
    field: MetadataField,
) -> Result<Option<bool>, String> {
    let Some(value) = field_value(mapping, field) else {
        return Ok(None);
    };
    match value {
        serde_yaml_ng::Value::Null => Ok(None),
        serde_yaml_ng::Value::Bool(value) => Ok(Some(*value)),
        serde_yaml_ng::Value::String(value) => match value.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" => Ok(Some(true)),
            "false" | "no" | "off" => Ok(Some(false)),
            _ => Err(format!(
                "metadata field `{}` must be a boolean or yes/no/on/off string",
                field.key()
            )),
        },
        _ => Err(format!(
            "metadata field `{}` must be a boolean, boolean string, or null",
            field.key()
        )),
    }
}

fn tags(mapping: &serde_yaml_ng::Mapping) -> Result<Vec<String>, String> {
    let Some(value) = field_value(mapping, MetadataField::Tags) else {
        return Ok(Vec::new());
    };
    let serde_yaml_ng::Value::Sequence(values) = value else {
        return if matches!(value, serde_yaml_ng::Value::Null) {
            Ok(Vec::new())
        } else {
            Err("metadata field `tags` must be a sequence or null".to_owned())
        };
    };
    values
        .iter()
        .map(|value| match value {
            serde_yaml_ng::Value::String(value) => Ok(value.clone()),
            serde_yaml_ng::Value::Number(value) => Ok(value.to_string()),
            serde_yaml_ng::Value::Bool(value) => Ok(value.to_string()),
            _ => Err("metadata field `tags` contains a non-scalar value".to_owned()),
        })
        .collect()
}

fn note_order(mapping: &serde_yaml_ng::Mapping) -> Result<BTreeMap<String, u32>, String> {
    let Some(value) = field_value(mapping, MetadataField::Order) else {
        return Ok(BTreeMap::new());
    };
    if matches!(value, serde_yaml_ng::Value::Null) {
        return Ok(BTreeMap::new());
    }
    let serde_yaml_ng::Value::Mapping(values) = value else {
        return Err("metadata field `order` must be a mapping or null".to_owned());
    };
    let mut order = BTreeMap::new();
    for (key, value) in values {
        let serde_yaml_ng::Value::String(category) = key else {
            return Err("metadata field `order` keys must be strings".to_owned());
        };
        let serde_yaml_ng::Value::Number(value) = value else {
            return Err("metadata field `order` values must be non-negative integers".to_owned());
        };
        let value = value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| {
                "metadata field `order` values must fit unsigned 32-bit integers".to_owned()
            })?;
        if order.insert(category.clone(), value).is_some() {
            return Err("metadata field `order` contains a duplicate category".to_owned());
        }
    }
    Ok(order)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MetadataPatch {
    pub favorited: Option<bool>,
    pub pinned: Option<bool>,
    pub deleted: Option<bool>,
    pub tags: Option<Vec<String>>,
    pub title: Option<String>,
    pub modified: Option<String>,
    pub order: Option<BTreeMap<String, u32>>,
    pub encryption: EncryptionPatch,
}

impl MetadataPatch {
    pub fn is_empty(&self) -> bool {
        self.favorited.is_none()
            && self.pinned.is_none()
            && self.deleted.is_none()
            && self.tags.is_none()
            && self.title.is_none()
            && self.modified.is_none()
            && self.order.is_none()
            && self.encryption == EncryptionPatch::Keep
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataRewrite {
    pub prefix: Vec<u8>,
    pub body_offset: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatchError {
    InvalidFrontMatter(FrontMatterIssue),
    UnsupportedStructure(String),
    UnsupportedValue(String),
}

impl fmt::Display for PatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFrontMatter(issue) => write!(formatter, "invalid front matter: {issue}"),
            Self::UnsupportedStructure(message) => {
                write!(formatter, "unsupported YAML structure: {message}")
            }
            Self::UnsupportedValue(message) => {
                write!(formatter, "unsupported metadata value: {message}")
            }
        }
    }
}

pub fn patch_front_matter(
    scan: &FrontMatterScan,
    patch: &MetadataPatch,
) -> Result<Option<MetadataRewrite>, PatchError> {
    if patch.is_empty() {
        return Ok(None);
    }
    match &scan.status {
        FrontMatterStatus::Plain => {
            let line_ending = scan.line_ending;
            let source = patch_source("", line_ending, &[], patch)?;
            Ok(Some(MetadataRewrite {
                prefix: format!(
                    "---{line_ending}{source}---{line_ending}",
                    line_ending = line_ending.as_str()
                )
                .into_bytes(),
                body_offset: 0,
            }))
        }
        FrontMatterStatus::Parsed(parsed) => {
            let source = patch_source(
                &parsed.source,
                parsed.line_ending,
                &parsed.present_fields,
                patch,
            )?;
            let line_ending = parsed.line_ending.as_str();
            let closing_ending = if parsed.closing_has_line_ending {
                line_ending
            } else {
                ""
            };
            let body_separator = parsed.body_separator.map(LineEnding::as_str).unwrap_or("");
            Ok(Some(MetadataRewrite {
                prefix: format!("---{line_ending}{source}---{closing_ending}{body_separator}")
                    .into_bytes(),
                body_offset: parsed.body_offset,
            }))
        }
        FrontMatterStatus::Invalid { issue, .. } => {
            Err(PatchError::InvalidFrontMatter(issue.clone()))
        }
    }
}

fn patch_source(
    source: &str,
    line_ending: LineEnding,
    present_fields: &[MetadataField],
    patch: &MetadataPatch,
) -> Result<String, PatchError> {
    let requested = requested_entries(patch, line_ending)?;
    let mut replacements = Vec::new();
    let mut additions = Vec::new();

    for (field, replacement) in requested {
        let spans = simple_field_spans(source, field.key());
        if spans.len() > 1 {
            return Err(PatchError::UnsupportedStructure(format!(
                "duplicate top-level key `{}`",
                field.key()
            )));
        }
        if let Some(span) = spans.first() {
            replacements.push((span.0, span.1, replacement));
        } else if present_fields.contains(&field) {
            return Err(PatchError::UnsupportedStructure(format!(
                "key `{}` is not a simple unquoted top-level entry",
                field.key()
            )));
        } else {
            additions.push(replacement);
        }
    }

    replacements.sort_by_key(|replacement| replacement.0);
    let mut result = String::new();
    let mut copied_until = 0;
    for (start, end, replacement) in replacements {
        result.push_str(&source[copied_until..start]);
        result.push_str(&replacement);
        copied_until = end;
    }
    result.push_str(&source[copied_until..]);
    if !additions.is_empty() {
        if !result.is_empty() && !result.ends_with('\n') && !result.ends_with('\r') {
            result.push_str(line_ending.as_str());
        }
        for addition in additions {
            result.push_str(&addition);
        }
    }
    validate_patched_source(source, &result, patch)?;
    Ok(result)
}

fn validate_patched_source(
    source: &str,
    patched: &str,
    patch: &MetadataPatch,
) -> Result<(), PatchError> {
    let patched_value =
        serde_yaml_ng::from_str::<serde_yaml_ng::Value>(patched).map_err(|error| {
            PatchError::UnsupportedStructure(format!(
                "patched front matter would not be valid YAML: {error}"
            ))
        })?;
    let (patched_metadata, _) = extract_metadata(patched_value.clone()).map_err(|error| {
        PatchError::UnsupportedStructure(format!(
            "patched front matter would not have supported metadata: {error}"
        ))
    })?;
    if patch
        .title
        .as_ref()
        .is_some_and(|expected| patched_metadata.title.as_ref() != Some(expected))
        || patch
            .tags
            .as_ref()
            .is_some_and(|expected| &patched_metadata.tags != expected)
        || patch
            .pinned
            .is_some_and(|expected| patched_metadata.pinned != Some(expected))
        || patch
            .favorited
            .is_some_and(|expected| patched_metadata.favorited != Some(expected))
        || patch
            .deleted
            .is_some_and(|expected| patched_metadata.deleted != Some(expected))
        || patch
            .modified
            .as_ref()
            .is_some_and(|expected| patched_metadata.modified.as_ref() != Some(expected))
        || patch
            .order
            .as_ref()
            .is_some_and(|expected| &patched_metadata.order != expected)
        || match patch.encryption {
            EncryptionPatch::Keep => false,
            EncryptionPatch::Set(expected) => patched_metadata.encryption != Some(expected),
            EncryptionPatch::Remove => patched_metadata.encryption.is_some(),
        }
    {
        return Err(PatchError::UnsupportedStructure(
            "patched metadata does not exactly match the requested values".to_owned(),
        ));
    }

    let original_value = if source.is_empty() {
        serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new())
    } else {
        serde_yaml_ng::from_str::<serde_yaml_ng::Value>(source).map_err(|error| {
            PatchError::UnsupportedStructure(format!(
                "original front matter could not be revalidated: {error}"
            ))
        })?
    };
    let serde_yaml_ng::Value::Mapping(mut original) = original_value else {
        return Err(PatchError::UnsupportedStructure(
            "original front matter root is not a mapping".to_owned(),
        ));
    };
    let serde_yaml_ng::Value::Mapping(mut rewritten) = patched_value else {
        return Err(PatchError::UnsupportedStructure(
            "patched front matter root is not a mapping".to_owned(),
        ));
    };
    for field in requested_fields(patch) {
        let key = serde_yaml_ng::Value::String(field.key().to_owned());
        original.remove(&key);
        rewritten.remove(&key);
    }
    if original.len() != rewritten.len()
        || original
            .iter()
            .any(|(key, value)| rewritten.get(key) != Some(value))
    {
        return Err(PatchError::UnsupportedStructure(
            "patch would change unrequested metadata".to_owned(),
        ));
    }
    Ok(())
}

fn requested_fields(patch: &MetadataPatch) -> Vec<MetadataField> {
    let mut fields = Vec::new();
    if patch.title.is_some() {
        fields.push(MetadataField::Title);
    }
    if patch.tags.is_some() {
        fields.push(MetadataField::Tags);
    }
    if patch.pinned.is_some() {
        fields.push(MetadataField::Pinned);
    }
    if patch.favorited.is_some() {
        fields.push(MetadataField::Favorited);
    }
    if patch.deleted.is_some() {
        fields.push(MetadataField::Deleted);
    }
    if patch.modified.is_some() {
        fields.push(MetadataField::Modified);
    }
    if patch.order.is_some() {
        fields.push(MetadataField::Order);
    }
    if patch.encryption != EncryptionPatch::Keep {
        fields.push(MetadataField::Encryption);
    }
    fields
}

fn requested_entries(
    patch: &MetadataPatch,
    line_ending: LineEnding,
) -> Result<Vec<(MetadataField, String)>, PatchError> {
    let newline = line_ending.as_str();
    let mut entries = Vec::new();
    if let Some(title) = &patch.title {
        entries.push((
            MetadataField::Title,
            format!("title: {}{newline}", quoted(title, "title")?),
        ));
    }
    if let Some(tags) = &patch.tags {
        let mut entry = String::from("tags:");
        if tags.is_empty() {
            entry.push_str(" []");
            entry.push_str(newline);
        } else {
            entry.push_str(newline);
            for tag in tags {
                entry.push_str("  - ");
                entry.push_str(&quoted(tag, "tag")?);
                entry.push_str(newline);
            }
        }
        entries.push((MetadataField::Tags, entry));
    }
    if let Some(pinned) = patch.pinned {
        entries.push((MetadataField::Pinned, format!("pinned: {pinned}{newline}")));
    }
    if let Some(favorited) = patch.favorited {
        entries.push((
            MetadataField::Favorited,
            format!("favorited: {favorited}{newline}"),
        ));
    }
    if let Some(deleted) = patch.deleted {
        entries.push((
            MetadataField::Deleted,
            format!("deleted: {deleted}{newline}"),
        ));
    }
    if let Some(modified) = &patch.modified {
        entries.push((
            MetadataField::Modified,
            format!("modified: {}{newline}", quoted(modified, "modified")?),
        ));
    }
    if let Some(order) = &patch.order {
        let replacement = if order.is_empty() {
            String::new()
        } else {
            let mut entry = format!("order:{newline}");
            for (category, value) in order {
                entry.push_str("  ");
                entry.push_str(&quoted(category, "order category")?);
                entry.push_str(": ");
                entry.push_str(&value.to_string());
                entry.push_str(newline);
            }
            entry
        };
        entries.push((MetadataField::Order, replacement));
    }
    match patch.encryption {
        EncryptionPatch::Keep => {}
        EncryptionPatch::Set(format) => entries.push((
            MetadataField::Encryption,
            format!("stillus_encryption: {}{newline}", format.as_str()),
        )),
        EncryptionPatch::Remove => entries.push((MetadataField::Encryption, String::new())),
    }
    Ok(entries)
}

fn quoted(value: &str, field: &str) -> Result<String, PatchError> {
    if value.chars().any(|character| character.is_control()) {
        return Err(PatchError::UnsupportedValue(format!(
            "`{field}` contains a control character"
        )));
    }
    Ok(format!("'{}'", value.replace('\'', "''")))
}

fn simple_field_spans(source: &str, key: &str) -> Vec<(usize, usize)> {
    let lines = source_lines(source);
    let mut spans = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if simple_top_level_key(line.content) != Some(key) {
            continue;
        }
        let mut end = line.end;
        let mut following_index = index + 1;
        while following_index < lines.len() {
            let following = &lines[following_index];
            if following.content.trim().is_empty() {
                let next_nonempty = lines[following_index + 1..]
                    .iter()
                    .position(|line| !line.content.trim().is_empty())
                    .map(|relative| following_index + 1 + relative);
                if next_nonempty.is_some_and(|next| is_indented(lines[next].content)) {
                    following_index += 1;
                    continue;
                }
                break;
            }
            if !is_indented(following.content) {
                break;
            }
            end = following.end;
            following_index += 1;
        }
        spans.push((line.start, end));
    }
    spans
}

fn is_indented(line: &str) -> bool {
    line.as_bytes()
        .first()
        .is_some_and(|byte| *byte == b' ' || *byte == b'\t')
}

struct SourceLine<'a> {
    start: usize,
    end: usize,
    content: &'a str,
}

fn source_lines(source: &str) -> Vec<SourceLine<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;
    while start < source.len() {
        let relative_end = source[start..].find('\n');
        let end = relative_end.map_or(source.len(), |relative| start + relative + 1);
        let content_end = relative_end.map_or(source.len(), |relative| start + relative);
        let content = source[start..content_end]
            .strip_suffix('\r')
            .unwrap_or(&source[start..content_end]);
        lines.push(SourceLine {
            start,
            end,
            content,
        });
        start = end;
    }
    lines
}

fn simple_top_level_key(line: &str) -> Option<&str> {
    if line
        .as_bytes()
        .first()
        .is_none_or(|byte| matches!(*byte, b' ' | b'\t' | b'#' | b'?' | b'-'))
    {
        return None;
    }
    let colon = line.find(':')?;
    let key = &line[..colon];
    if key.is_empty()
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }
    let remainder = &line[colon + 1..];
    (remainder.is_empty()
        || remainder
            .as_bytes()
            .first()
            .is_some_and(|byte| *byte == b' ' || *byte == b'\t'))
    .then_some(key)
}

enum Opening {
    NeedMore,
    Plain,
    FrontMatter(usize),
}

fn opening_end(bytes: &[u8]) -> Opening {
    if bytes.len() < 3 {
        return if b"---".starts_with(bytes) {
            Opening::NeedMore
        } else {
            Opening::Plain
        };
    }
    if &bytes[..3] != b"---" {
        return Opening::Plain;
    }
    match bytes.get(3) {
        None => Opening::NeedMore,
        Some(b'\n') => Opening::FrontMatter(4),
        Some(b'\r') => match bytes.get(4) {
            None => Opening::NeedMore,
            Some(b'\n') => Opening::FrontMatter(5),
            Some(_) => Opening::Plain,
        },
        Some(_) => Opening::Plain,
    }
}

fn closing_delimiter(bytes: &[u8], mut line_start: usize, eof: bool) -> Option<(usize, usize)> {
    while line_start < bytes.len() {
        let Some(relative_end) = bytes[line_start..].iter().position(|byte| *byte == b'\n') else {
            if eof {
                let line = bytes[line_start..]
                    .strip_suffix(b"\r")
                    .unwrap_or(&bytes[line_start..]);
                return (line == b"---").then_some((line_start, bytes.len()));
            }
            return None;
        };
        let line_end = line_start + relative_end;
        let line = bytes[line_start..line_end]
            .strip_suffix(b"\r")
            .unwrap_or(&bytes[line_start..line_end]);
        if line == b"---" {
            return Some((line_start, line_end + 1));
        }
        line_start = line_end + 1;
    }
    None
}

/// Treat exactly one empty line after a closed front matter block as a
/// presentation separator. `None` means that one more byte may be required to
/// distinguish EOF/body content from an LF or CRLF separator.
fn body_boundary(
    bytes: &[u8],
    front_matter_end: usize,
    eof: bool,
) -> Option<(usize, Option<LineEnding>)> {
    let remainder = &bytes[front_matter_end..];
    match remainder {
        [b'\n', ..] => Some((front_matter_end + 1, Some(LineEnding::Lf))),
        [b'\r', b'\n', ..] => Some((front_matter_end + 2, Some(LineEnding::Crlf))),
        [] | [b'\r'] if !eof => None,
        _ => Some((front_matter_end, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn scan(input: impl AsRef<[u8]>) -> FrontMatterScan {
        scan_reader(Cursor::new(input.as_ref())).unwrap()
    }

    fn rewrite(input: &[u8], patch: &MetadataPatch) -> Result<Vec<u8>, PatchError> {
        let scan = scan(input);
        let Some(rewrite) = patch_front_matter(&scan, patch)? else {
            return Ok(input.to_vec());
        };
        let mut output = rewrite.prefix;
        output.extend_from_slice(&input[rewrite.body_offset as usize..]);
        Ok(output)
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
            .expect("needle must be present")
    }

    #[test]
    fn parses_known_fields_and_ignores_unknown_fields() {
        let input = "---\r\ntitle: Текущие задачи\r\ntags: [Задачи, Work]\r\npinned: true\r\nfavorited: false\r\ncreated: '2022-02-03T18:57:43.598Z'\r\nmodified: '2026-08-31T18:52:01.046Z'\r\nsome_future_field: 123\r\n---\r\nbody";
        let result = scan(input);
        let FrontMatterStatus::Parsed(parsed) = result.status else {
            panic!("expected parsed front matter");
        };
        assert_eq!(parsed.metadata.title.as_deref(), Some("Текущие задачи"));
        assert_eq!(parsed.metadata.tags, ["Задачи", "Work"]);
        assert_eq!(parsed.metadata.pinned, Some(true));
        assert_eq!(parsed.metadata.favorited, Some(false));
        assert_eq!(parsed.body_offset, input.find("body").unwrap() as u64);
    }

    #[test]
    fn encryption_marker_set_remove_and_plain_creation_are_lossless() {
        let input = b"---\r\ntitle: Vault\r\n# keep this comment\r\nfuture_field: 42\r\n---\r\n\r\nVault\r\nsecret\r\n";
        let protected = rewrite(
            input,
            &MetadataPatch {
                encryption: EncryptionPatch::Set(EncryptionFormat::AgeBodyV1),
                ..MetadataPatch::default()
            },
        )
        .unwrap();
        assert!(
            protected
                .windows(b"stillus_encryption: age-body-v1\r\n".len())
                .any(|window| window == b"stillus_encryption: age-body-v1\r\n")
        );
        assert!(
            protected
                .windows(b"# keep this comment\r\nfuture_field: 42\r\n".len())
                .any(|window| window == b"# keep this comment\r\nfuture_field: 42\r\n")
        );
        let FrontMatterStatus::Parsed(parsed) = scan(&protected).status else {
            panic!("expected protected front matter");
        };
        assert_eq!(
            parsed.metadata.encryption,
            Some(EncryptionFormat::AgeBodyV1)
        );

        let plain = rewrite(
            &protected,
            &MetadataPatch {
                encryption: EncryptionPatch::Remove,
                ..MetadataPatch::default()
            },
        )
        .unwrap();
        assert!(
            !plain
                .windows(b"stillus_encryption".len())
                .any(|window| { window == b"stillus_encryption" })
        );
        assert!(
            plain
                .windows(b"# keep this comment\r\nfuture_field: 42\r\n".len())
                .any(|window| window == b"# keep this comment\r\nfuture_field: 42\r\n")
        );
        assert!(plain.ends_with(b"Vault\r\nsecret\r\n"));

        let created = rewrite(
            b"New note\nprivate body\n",
            &MetadataPatch {
                title: Some("New note".to_owned()),
                encryption: EncryptionPatch::Set(EncryptionFormat::AgeBodyV1),
                ..MetadataPatch::default()
            },
        )
        .unwrap();
        assert!(
            created.starts_with(b"---\ntitle: 'New note'\nstillus_encryption: age-body-v1\n---\n")
        );
        assert!(created.ends_with(b"New note\nprivate body\n"));
    }

    #[test]
    fn note_order_map_is_typed_lossless_and_removed_when_empty() {
        let input = b"---\r\ntitle: Note\r\norder:\r\n  'Work': 2\r\n  'Parent/Child': 0\r\nfuture: keep # comment\r\n---\r\nbody\r\n";
        let scan = scan_reader(input.as_slice()).unwrap();
        let FrontMatterStatus::Parsed(parsed) = &scan.status else {
            panic!("expected parsed front matter");
        };
        assert_eq!(
            parsed.metadata.order,
            BTreeMap::from([("Parent/Child".to_owned(), 0), ("Work".to_owned(), 2)])
        );

        let rewritten = rewrite(
            input,
            &MetadataPatch {
                order: Some(BTreeMap::from([
                    ("Personal".to_owned(), 1),
                    ("Work".to_owned(), 3),
                ])),
                ..MetadataPatch::default()
            },
        )
        .unwrap();
        let rewritten = String::from_utf8(rewritten).unwrap();
        assert!(rewritten.contains("order:\r\n  'Personal': 1\r\n  'Work': 3\r\n"));
        assert!(rewritten.contains("future: keep # comment\r\n"));
        assert!(rewritten.ends_with("---\r\nbody\r\n"));

        let removed = rewrite(
            rewritten.as_bytes(),
            &MetadataPatch {
                order: Some(BTreeMap::new()),
                ..MetadataPatch::default()
            },
        )
        .unwrap();
        let removed = String::from_utf8(removed).unwrap();
        assert!(!removed.contains("order:"));
        assert!(removed.contains("future: keep # comment\r\n"));
    }

    #[test]
    fn note_order_rejects_non_mapping_keys_and_invalid_numbers() {
        for source in [
            "---\norder: [1, 2]\n---\nbody",
            "---\norder: {Work: -1}\n---\nbody",
            "---\norder: {Work: 1.5}\n---\nbody",
            "---\norder: {7: 1}\n---\nbody",
        ] {
            assert!(matches!(
                scan_reader(source.as_bytes()).unwrap().status,
                FrontMatterStatus::Invalid { .. }
            ));
        }
    }

    #[test]
    fn hides_one_empty_front_matter_separator_and_preserves_it_when_patching() {
        let cases: [(&[u8], LineEnding); 2] = [
            (b"---\ntitle: Old\n---\n\n# Body\n", LineEnding::Lf),
            (
                b"---\r\ntitle: Old\r\n---\r\n\r\n# Body\r\n",
                LineEnding::Crlf,
            ),
        ];

        for (input, expected_separator) in cases {
            let result = scan(input);
            let FrontMatterStatus::Parsed(parsed) = result.status else {
                panic!("expected parsed front matter");
            };
            assert_eq!(parsed.body_offset, find_bytes(input, b"# Body") as u64);
            assert_eq!(parsed.body_separator, Some(expected_separator));

            let output = rewrite(
                input,
                &MetadataPatch {
                    title: Some("New".to_owned()),
                    ..MetadataPatch::default()
                },
            )
            .unwrap();
            let expected_separator = match expected_separator {
                LineEnding::Lf => b"---\n\n# Body".as_slice(),
                LineEnding::Crlf => b"---\r\n\r\n# Body".as_slice(),
            };
            assert!(
                output
                    .windows(expected_separator.len())
                    .any(|window| { window == expected_separator })
            );
        }
    }

    #[test]
    fn leaves_non_separator_and_additional_leading_lines_in_the_body() {
        let no_separator = b"---\ntitle: Note\n---\n# Body\n";
        let FrontMatterStatus::Parsed(parsed) = scan(no_separator).status else {
            panic!("expected parsed front matter");
        };
        assert_eq!(
            parsed.body_offset,
            find_bytes(no_separator, b"# Body") as u64
        );
        assert_eq!(parsed.body_separator, None);

        let two_empty_lines = b"---\ntitle: Note\n---\n\n\n# Body\n";
        let FrontMatterStatus::Parsed(parsed) = scan(two_empty_lines).status else {
            panic!("expected parsed front matter");
        };
        assert_eq!(
            &two_empty_lines[parsed.body_offset as usize..],
            b"\n# Body\n"
        );
        assert_eq!(parsed.body_separator, Some(LineEnding::Lf));

        let whitespace_line = b"---\ntitle: Note\n---\n  \n# Body\n";
        let FrontMatterStatus::Parsed(parsed) = scan(whitespace_line).status else {
            panic!("expected parsed front matter");
        };
        assert_eq!(
            &whitespace_line[parsed.body_offset as usize..],
            b"  \n# Body\n"
        );
        assert_eq!(parsed.body_separator, None);
    }

    #[test]
    fn detects_separator_across_reader_chunk_boundary() {
        let opening = b"---\ntitle: ";
        let closing = b"\n---\n";
        let padding_len = READ_CHUNK_BYTES - opening.len() - closing.len();
        let mut input = opening.to_vec();
        input.extend(std::iter::repeat_n(b'x', padding_len));
        input.extend_from_slice(closing);
        assert_eq!(input.len(), READ_CHUNK_BYTES);
        input.extend_from_slice(b"\n# Body\n");

        let result = scan(&input);
        let FrontMatterStatus::Parsed(parsed) = result.status else {
            panic!("expected parsed front matter");
        };
        assert_eq!(parsed.body_offset, find_bytes(&input, b"# Body") as u64);
        assert_eq!(parsed.body_separator, Some(LineEnding::Lf));
        assert!(result.bytes_read > READ_CHUNK_BYTES);
    }

    #[test]
    fn coerces_supported_scalar_metadata_without_invalidating_the_note() {
        let cases = [
            ("title: 2024", "2024"),
            ("title: 1.5", "1.5"),
            ("title: true", "true"),
        ];
        for (source, expected) in cases {
            let result = scan(format!("---\n{source}\n---\nbody"));
            let FrontMatterStatus::Parsed(parsed) = result.status else {
                panic!("expected scalar title `{source}` to stay accessible");
            };
            assert_eq!(parsed.metadata.title.as_deref(), Some(expected));
        }

        let result = scan(
            "---\ntitle: null\ntags: [2024, Work, true]\npinned: YES\nfavorited: off\ncreated: 2022-02-03T18:57:43.598Z\nmodified: false\n---\nbody",
        );
        let FrontMatterStatus::Parsed(parsed) = result.status else {
            panic!("expected supported scalar coercions to parse");
        };
        assert_eq!(parsed.metadata.title, None);
        assert_eq!(parsed.metadata.tags, ["2024", "Work", "true"]);
        assert_eq!(parsed.metadata.pinned, Some(true));
        assert_eq!(parsed.metadata.favorited, Some(false));
        assert_eq!(
            parsed.metadata.created.as_deref(),
            Some("2022-02-03T18:57:43.598Z")
        );
        assert_eq!(parsed.metadata.modified.as_deref(), Some("false"));
    }

    #[test]
    fn rejects_non_scalar_known_values_but_accepts_null_tags() {
        for source in ["title: [nested]", "pinned: 1", "tags: [ok, [nested]]"] {
            assert!(matches!(
                scan(format!("---\n{source}\n---\nbody")).status,
                FrontMatterStatus::Invalid {
                    issue: FrontMatterIssue::MalformedYaml(_),
                    body_offset: Some(_)
                }
            ));
        }

        let FrontMatterStatus::Parsed(parsed) = scan("---\ntags: null\n---\nbody").status else {
            panic!("null tags must remain accessible");
        };
        assert!(parsed.metadata.tags.is_empty());
    }

    #[test]
    fn plain_markdown_is_not_an_error() {
        let result = scan("# Plain Markdown\nbody");
        assert_eq!(result.status, FrontMatterStatus::Plain);
        assert!(result.bytes_read <= READ_CHUNK_BYTES);
    }

    #[test]
    fn classifies_malformed_unclosed_oversized_and_invalid_utf8() {
        let malformed = scan("---\ntags: scalar\n---\nbody");
        assert!(matches!(
            malformed.status,
            FrontMatterStatus::Invalid {
                issue: FrontMatterIssue::MalformedYaml(_),
                body_offset: Some(_)
            }
        ));

        let unclosed = scan("---\ntitle: no closing marker\nbody");
        assert_eq!(
            unclosed.status,
            FrontMatterStatus::Invalid {
                issue: FrontMatterIssue::Unclosed,
                body_offset: None
            }
        );

        let mut oversized = b"---\nvalue: ".to_vec();
        oversized.resize(MAX_FRONT_MATTER_BYTES + 100, b'x');
        let oversized = scan(oversized);
        assert_eq!(
            oversized.bytes_read,
            MAX_FRONT_MATTER_BYTES + 1 + BODY_SEPARATOR_LOOKAHEAD_BYTES
        );
        assert_eq!(
            oversized.status,
            FrontMatterStatus::Invalid {
                issue: FrontMatterIssue::TooLarge,
                body_offset: None
            }
        );

        let invalid_utf8 = scan([
            b'-', b'-', b'-', b'\n', 0xff, b'\n', b'-', b'-', b'-', b'\n',
        ]);
        assert_eq!(
            invalid_utf8.status,
            FrontMatterStatus::Invalid {
                issue: FrontMatterIssue::InvalidUtf8,
                body_offset: Some(10)
            }
        );
    }

    #[test]
    fn closing_delimiter_without_trailing_newline_is_supported() {
        let result = scan("---\ntitle: Empty body\n---");
        let FrontMatterStatus::Parsed(parsed) = result.status else {
            panic!("expected parsed front matter");
        };
        assert_eq!(parsed.body_offset, 25);
    }

    #[test]
    fn closing_delimiter_must_finish_within_limit() {
        let mut input = b"---\nkey: ".to_vec();
        input.resize(MAX_FRONT_MATTER_BYTES - 3, b'x');
        input.extend_from_slice(b"\n---");
        let result = scan(input);
        assert_eq!(
            result.status,
            FrontMatterStatus::Invalid {
                issue: FrontMatterIssue::TooLarge,
                body_offset: None
            }
        );
    }

    #[test]
    fn patch_preserves_unknown_entries_comments_created_and_body() {
        let input = b"---\n# keep this comment\nsome_future_field: {nested: 123}\ntitle: Old # replaced with the target entry\ncreated: '2022-02-03T18:57:43.598Z'\ntags:\n  - Old\nunknown_after: yes\n---\n# Body\nraw body bytes\n";
        let patch = MetadataPatch {
            title: Some("O'Brien".to_owned()),
            tags: Some(vec!["Задачи".to_owned(), "Work".to_owned()]),
            pinned: Some(true),
            modified: Some("2026-09-01T00:00:00.000Z".to_owned()),
            ..MetadataPatch::default()
        };
        let output = rewrite(input, &patch).unwrap();
        let output_text = std::str::from_utf8(&output).unwrap();
        assert!(output_text.contains("# keep this comment\n"));
        assert!(output_text.contains("some_future_field: {nested: 123}\n"));
        assert!(output_text.contains("unknown_after: yes\n"));
        assert!(output_text.contains("created: '2022-02-03T18:57:43.598Z'\n"));
        assert!(output_text.contains("title: 'O''Brien'\n"));
        assert!(output_text.contains("tags:\n  - 'Задачи'\n  - 'Work'\n"));
        assert!(output_text.contains("pinned: true\n"));
        let body = &input[input
            .windows(6)
            .position(|bytes| bytes == b"# Body")
            .unwrap()..];
        assert!(output.ends_with(body));
    }

    #[test]
    fn patch_preserves_crlf_and_creates_front_matter_for_plain_markdown() {
        let crlf = b"---\r\ntitle: Old\r\nfuture: keep\r\n---\r\nBody\r\n";
        let output = rewrite(
            crlf,
            &MetadataPatch {
                title: Some("New".to_owned()),
                ..MetadataPatch::default()
            },
        )
        .unwrap();
        assert_eq!(
            output,
            b"---\r\ntitle: 'New'\r\nfuture: keep\r\n---\r\nBody\r\n"
        );

        let plain = b"# Plain\nbody\n";
        let output = rewrite(
            plain,
            &MetadataPatch {
                favorited: Some(false),
                tags: Some(Vec::new()),
                ..MetadataPatch::default()
            },
        )
        .unwrap();
        assert_eq!(
            output,
            b"---\ntags: []\nfavorited: false\n---\n# Plain\nbody\n"
        );

        let plain_crlf = b"# Plain\r\nbody\r\n";
        let output = rewrite(
            plain_crlf,
            &MetadataPatch {
                title: Some("2024".to_owned()),
                ..MetadataPatch::default()
            },
        )
        .unwrap();
        assert_eq!(
            output,
            b"---\r\ntitle: '2024'\r\n---\r\n# Plain\r\nbody\r\n"
        );
    }

    #[test]
    fn deleted_boolean_is_parsed_and_patched_without_touching_unknown_metadata_or_body() {
        let input = b"---\ntitle: Keep\ndeleted: false\nfuture: {nested: 7}\n---\n# Body\nbytes\n";
        let output = rewrite(
            input,
            &MetadataPatch {
                deleted: Some(true),
                modified: Some("2026-09-03T12:00:00.000Z".to_owned()),
                ..MetadataPatch::default()
            },
        )
        .unwrap();
        let FrontMatterStatus::Parsed(parsed) = scan(&output).status else {
            panic!("soft-deleted note must remain valid front matter");
        };
        assert_eq!(parsed.metadata.deleted, Some(true));
        assert!(String::from_utf8_lossy(&output).contains("future: {nested: 7}\n"));
        assert!(output.ends_with(b"# Body\nbytes\n"));

        let plain = rewrite(
            b"plain body\n",
            &MetadataPatch {
                deleted: Some(true),
                ..MetadataPatch::default()
            },
        )
        .unwrap();
        assert_eq!(plain, b"---\ndeleted: true\n---\nplain body\n");
    }

    #[test]
    fn patch_quotes_coerced_title_and_preserves_created_and_unknown_bytes() {
        let input = b"---\ntitle: 2024\ncreated: 2022-02-03T18:57:43.598Z\nfuture: {number: 7}\n---\nbody\n";
        let output = rewrite(
            input,
            &MetadataPatch {
                title: Some("2024".to_owned()),
                ..MetadataPatch::default()
            },
        )
        .unwrap();
        assert_eq!(
            output,
            b"---\ntitle: '2024'\ncreated: 2022-02-03T18:57:43.598Z\nfuture: {number: 7}\n---\nbody\n"
        );
    }

    #[test]
    fn patch_spans_include_internal_blank_lines_in_sequences_and_block_scalars() {
        let sequence = b"---\ntags:\n  - a\n\n  - b\nunknown: |\n  first\n\n  second\n---\nbody\n";
        let output = rewrite(
            sequence,
            &MetadataPatch {
                tags: Some(vec!["new".to_owned()]),
                ..MetadataPatch::default()
            },
        )
        .unwrap();
        assert_eq!(
            output,
            b"---\ntags:\n  - 'new'\nunknown: |\n  first\n\n  second\n---\nbody\n"
        );
        let FrontMatterStatus::Parsed(parsed) = scan(&output).status else {
            panic!("patched sequence must remain valid");
        };
        assert_eq!(parsed.metadata.tags, ["new"]);

        let block = b"---\ntitle: |\n  first\n\n  second\ncreated: |\n  2022-02-03\n\n  18:57:43.598Z\nother: x\n---\nbody\n";
        let output = rewrite(
            block,
            &MetadataPatch {
                title: Some("new".to_owned()),
                ..MetadataPatch::default()
            },
        )
        .unwrap();
        assert_eq!(
            output,
            b"---\ntitle: 'new'\ncreated: |\n  2022-02-03\n\n  18:57:43.598Z\nother: x\n---\nbody\n"
        );
        let FrontMatterStatus::Parsed(parsed) = scan(&output).status else {
            panic!("patched block scalar must remain valid");
        };
        assert_eq!(parsed.metadata.title.as_deref(), Some("new"));
        assert_eq!(
            parsed.metadata.created.as_deref(),
            Some("2022-02-03\n\n18:57:43.598Z\n")
        );
    }

    #[test]
    fn patch_preserves_trailing_blank_lines_and_fails_closed_on_ambiguous_span() {
        let trailing_blank = b"---\ntags:\n  - old\n\nother: keep\n---\nbody\n";
        assert_eq!(
            rewrite(
                trailing_blank,
                &MetadataPatch {
                    tags: Some(vec!["new".to_owned()]),
                    ..MetadataPatch::default()
                }
            )
            .unwrap(),
            b"---\ntags:\n  - 'new'\n\nother: keep\n---\nbody\n"
        );

        let ambiguous = b"---\ntags: [\n  old,\n\n  stale\n]\nother: keep\n---\nbody\n";
        assert!(matches!(
            scan(ambiguous).status,
            FrontMatterStatus::Parsed(_)
        ));
        assert!(matches!(
            rewrite(
                ambiguous,
                &MetadataPatch {
                    tags: Some(vec!["new".to_owned()]),
                    ..MetadataPatch::default()
                }
            ),
            Err(PatchError::UnsupportedStructure(_))
        ));
    }

    #[test]
    fn patch_rejects_invalid_ambiguous_and_control_character_inputs() {
        let malformed = scan("---\ntags: scalar\n---\nbody");
        assert!(matches!(
            patch_front_matter(
                &malformed,
                &MetadataPatch {
                    title: Some("New".to_owned()),
                    ..MetadataPatch::default()
                }
            ),
            Err(PatchError::InvalidFrontMatter(_))
        ));

        let quoted_key = scan("---\n\"title\": Old\n---\nbody");
        assert!(matches!(
            patch_front_matter(
                &quoted_key,
                &MetadataPatch {
                    title: Some("New".to_owned()),
                    ..MetadataPatch::default()
                }
            ),
            Err(PatchError::UnsupportedStructure(_))
        ));

        let plain = scan("body");
        assert!(matches!(
            patch_front_matter(
                &plain,
                &MetadataPatch {
                    title: Some("line\nbreak".to_owned()),
                    ..MetadataPatch::default()
                }
            ),
            Err(PatchError::UnsupportedValue(_))
        ));
    }

    #[test]
    fn empty_patch_is_a_noop() {
        let scan = scan("---\ntitle: Untouched\n---\nbody");
        assert_eq!(
            patch_front_matter(&scan, &MetadataPatch::default()).unwrap(),
            None
        );
    }

    #[test]
    fn deterministic_malformed_corpus_stays_bounded_and_never_loses_body() {
        let mut random = Lcg::new(0x4e4f_5452_554d_0008);
        for case in 0..512 {
            let payload_len = random.next_usize(4_096);
            let mut input = b"---\n".to_vec();
            for _ in 0..payload_len {
                input.push(random.next_usize(256) as u8);
            }
            if case % 3 != 0 {
                input.extend_from_slice(b"\n---\nBODY_SENTINEL\0\xff");
            }

            let scanned = scan(&input);
            assert!(
                scanned.bytes_read <= MAX_FRONT_MATTER_BYTES + 1 + BODY_SEPARATOR_LOOKAHEAD_BYTES
            );
            let result = patch_front_matter(
                &scanned,
                &MetadataPatch {
                    title: Some(format!("Corpus {case} 🦀")),
                    ..MetadataPatch::default()
                },
            );
            if let Ok(Some(rewrite)) = result {
                let original_body = &input[rewrite.body_offset as usize..];
                let mut output = rewrite.prefix;
                output.extend_from_slice(original_body);
                assert!(output.ends_with(original_body));
            }
        }
    }

    #[test]
    fn generated_updates_preserve_unknown_metadata_unicode_and_body() {
        for case in 0..128 {
            let input = format!(
                "---\ntitle: Before {case}\ntags: [Existing]\nfuture_{case}: {{nested: {case}}}\n---\n# Тело {case}\n🦀 sentinel\n"
            );
            let output = rewrite(
                input.as_bytes(),
                &MetadataPatch {
                    title: Some(format!("После {case} 🦀")),
                    tags: Some(vec!["Задачи".to_owned(), format!("Группа/{case}")]),
                    modified: Some("2026-09-01T00:00:00.000Z".to_owned()),
                    ..MetadataPatch::default()
                },
            )
            .unwrap();
            let output = std::str::from_utf8(&output).unwrap();
            assert!(output.contains(&format!("future_{case}: {{nested: {case}}}\n")));
            assert!(output.contains(&format!("title: 'После {case} 🦀'\n")));
            assert!(output.contains("tags:\n  - 'Задачи'\n"));
            assert!(output.ends_with(&format!("# Тело {case}\n🦀 sentinel\n")));
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
            ((self.0 >> 32) as usize) % upper
        }
    }
}
