// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Built-in Markdown file engine. The established document/session pipeline
//! remains byte-compatible while the coordinator can address it generically.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use stillus_engine::{
    CommonMetadata, EngineCapabilities, EngineError, EngineId, ExternalFileSummary, FileEngine,
    FileEngineFactory, ItemAvailability, ItemId, ItemSummary, LocalSearchDocument,
    LocalSearchMatch, LocalSearchProvider, LocalSearchRequest, ReferencedSecret, SearchHit,
    SearchProvider, SearchRequest, SettingsCandidate, SettingsSchema,
};
use stillus_frontmatter::FrontMatterStatus;
use stillus_search::SearchIndex;
use stillus_storage::{EMPTY_NOTE_TITLE, NoteScanResult, create_note, scan_workspace};

pub const MARKDOWN_ENGINE_ID: &str = "markdown";
const UTF8_READ_BYTES: usize = 64 * 1024;

#[derive(Default)]
pub struct MarkdownEngineFactory;

impl FileEngineFactory for MarkdownEngineFactory {
    fn id(&self) -> EngineId {
        markdown_engine_id()
    }

    fn display_name(&self) -> &str {
        "Markdown"
    }

    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            create: true,
            settings: false,
            global_search: true,
            local_search: true,
            scheduled_tasks: false,
            manual_tasks: false,
            external_files: true,
        }
    }

    fn external_file_extensions(&self) -> Vec<String> {
        ["md", "markdown", "txt"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    fn external_file_fallback(&self) -> bool {
        true
    }

    fn settings_schema(&self) -> SettingsSchema {
        SettingsSchema::default()
    }

    fn open(&self, workspace: &Path) -> Result<Box<dyn FileEngine>, EngineError> {
        Ok(Box::new(MarkdownEngine::open(workspace)?))
    }
}

pub struct MarkdownEngine {
    workspace: PathBuf,
    search: MarkdownSearchProvider,
    local_search: MarkdownLocalSearchProvider,
    external_files: BTreeMap<ItemId, ExternalFileSummary>,
}

impl MarkdownEngine {
    pub fn open(workspace: impl AsRef<Path>) -> Result<Self, EngineError> {
        let workspace = workspace.as_ref().to_path_buf();
        if !workspace.join("notes").is_dir() {
            return Err(EngineError::Io("notes directory is unavailable".to_owned()));
        }
        Ok(Self {
            search: MarkdownSearchProvider::new(workspace.clone()),
            local_search: MarkdownLocalSearchProvider,
            workspace,
            external_files: BTreeMap::new(),
        })
    }
}

impl FileEngine for MarkdownEngine {
    fn id(&self) -> EngineId {
        markdown_engine_id()
    }

    fn items(&self) -> Result<Vec<ItemSummary>, EngineError> {
        let scan = scan_workspace(&self.workspace).map_err(engine_io)?;
        scan.notes
            .into_iter()
            .map(|note| {
                let relative = note
                    .path
                    .strip_prefix(&self.workspace)
                    .map_err(|error| EngineError::Io(error.to_string()))?
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/");
                let fallback = note
                    .path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or(EMPTY_NOTE_TITLE)
                    .to_owned();
                let (title, availability) = match note.result {
                    NoteScanResult::Scanned(scanned) => {
                        let title = match scanned.frontmatter.status {
                            FrontMatterStatus::Parsed(parsed) => scanned
                                .body_title
                                .or(parsed.metadata.title)
                                .unwrap_or(fallback),
                            FrontMatterStatus::Plain => scanned.body_title.unwrap_or(fallback),
                            FrontMatterStatus::Invalid { issue, .. } => {
                                return Ok(ItemSummary {
                                    engine_id: markdown_engine_id(),
                                    item_id: ItemId::new(relative)?,
                                    metadata_version: "filesystem".to_owned(),
                                    metadata: CommonMetadata {
                                        title: fallback,
                                        ..CommonMetadata::default()
                                    },
                                    availability: ItemAvailability::Invalid(issue.to_string()),
                                    badge: None,
                                });
                            }
                        };
                        (title, ItemAvailability::Ready)
                    }
                    NoteScanResult::Protected(scanned) => {
                        let title = match scanned.frontmatter.status {
                            FrontMatterStatus::Parsed(parsed) => {
                                parsed.metadata.title.unwrap_or(fallback)
                            }
                            _ => fallback,
                        };
                        (title, ItemAvailability::NeedsUnlock)
                    }
                    NoteScanResult::LegacyProtected => (
                        fallback,
                        ItemAvailability::Invalid("legacy protected note".to_owned()),
                    ),
                    NoteScanResult::InvalidProtected(message) => {
                        (fallback, ItemAvailability::Invalid(message))
                    }
                    NoteScanResult::IoError(message) => {
                        (fallback, ItemAvailability::Unavailable(message))
                    }
                };
                Ok(ItemSummary {
                    engine_id: markdown_engine_id(),
                    item_id: ItemId::new(relative)?,
                    metadata_version: "filesystem".to_owned(),
                    metadata: CommonMetadata {
                        title,
                        ..CommonMetadata::default()
                    },
                    availability,
                    badge: None,
                })
            })
            .collect()
    }

    fn external_files(&self) -> Vec<ExternalFileSummary> {
        self.external_files.values().cloned().collect()
    }

    fn open_external_file(&mut self, path: &Path) -> Result<ExternalFileSummary, EngineError> {
        if !path.is_absolute() {
            return Err(EngineError::Io(
                "external file path must be absolute".to_owned(),
            ));
        }
        let requested = normalize_absolute_path(path)?;
        let metadata = fs::symlink_metadata(&requested);
        let (normalized, mut availability) = match metadata {
            Ok(metadata) if metadata.file_type().is_file() => (
                requested.canonicalize().map_err(engine_io)?,
                ItemAvailability::Ready,
            ),
            Ok(_) => {
                return Err(EngineError::Io(
                    "external target must be a regular file and not a symlink".to_owned(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (requested, ItemAvailability::Unavailable(error.to_string()))
            }
            Err(error) => return Err(engine_io(error)),
        };
        let title = normalized
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| EngineError::Io("external file name is not valid UTF-8".to_owned()))?
            .to_owned();
        if matches!(availability, ItemAvailability::Ready) {
            let validation = fs::File::open(&normalized)
                .map_err(ExternalUtf8Error::Io)
                .and_then(validate_external_utf8);
            match validation {
                Ok(()) => {}
                Err(ExternalUtf8Error::Invalid { offset }) => {
                    return Err(EngineError::Io(format!(
                        "external file is not valid UTF-8 at byte {offset}"
                    )));
                }
                Err(ExternalUtf8Error::Binary { offset }) => {
                    return Err(EngineError::Io(format!(
                        "external file contains binary data (NUL) at byte {offset}"
                    )));
                }
                Err(ExternalUtf8Error::Io(error)) => {
                    availability = ItemAvailability::Unavailable(error.to_string());
                }
            }
        }
        let item_id = external_item_id(&normalized)?;
        let summary = ExternalFileSummary {
            engine_id: markdown_engine_id(),
            item_id: item_id.clone(),
            path: normalized,
            title,
            availability,
            recovery_available: false,
        };
        self.external_files.insert(item_id, summary.clone());
        Ok(summary)
    }

    fn close_external_file(&mut self, item: &ItemId) -> Result<bool, EngineError> {
        Ok(self.external_files.remove(item).is_some())
    }

    fn create(&mut self, settings: SettingsCandidate) -> Result<ItemId, EngineError> {
        if !settings.public.is_empty() || !settings.secrets.is_empty() {
            return Err(EngineError::InvalidSetting(
                "markdown has no creation settings".to_owned(),
            ));
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| EngineError::Io(error.to_string()))?
            .as_millis()
            .to_string();
        let commit = create_note(&self.workspace, EMPTY_NOTE_TITLE, &timestamp)
            .map_err(|error| EngineError::Io(error.to_string()))?;
        let relative = commit
            .path
            .strip_prefix(&self.workspace)
            .map_err(|error| EngineError::Io(error.to_string()))?
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        ItemId::new(relative)
    }

    fn update_settings(
        &mut self,
        _item: &ItemId,
        _expected_version: &str,
        _settings: SettingsCandidate,
    ) -> Result<String, EngineError> {
        Err(EngineError::Unsupported(
            "markdown items use their canonical front matter".to_owned(),
        ))
    }

    fn referenced_secrets(&self) -> Result<Vec<ReferencedSecret>, EngineError> {
        Ok(Vec::new())
    }

    fn search_provider(&self) -> Option<&dyn SearchProvider> {
        Some(&self.search)
    }

    fn local_search_provider(&self) -> Option<&dyn LocalSearchProvider> {
        Some(&self.local_search)
    }

    fn background_tasks(&self) -> Vec<stillus_engine::BackgroundTaskDescriptor> {
        Vec::new()
    }

    fn quiesce(&mut self) -> Result<(), EngineError> {
        Ok(())
    }

    fn resume(&mut self) {}

    fn security_rotated(&mut self) {}
}

#[derive(Debug)]
enum ExternalUtf8Error {
    Io(io::Error),
    Invalid { offset: u64 },
    Binary { offset: u64 },
}

/// Validate the entire file without retaining its contents. A UTF-8 character
/// split across reads leaves at most three bytes at the start of the next block.
fn validate_external_utf8(mut reader: impl Read) -> Result<(), ExternalUtf8Error> {
    let mut buffer = [0_u8; UTF8_READ_BYTES];
    let mut carry = 0;
    let mut offset = 0_u64;
    loop {
        let read = match reader.read(&mut buffer[carry..]) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(ExternalUtf8Error::Io(error)),
        };
        if read == 0 {
            return if carry == 0 {
                Ok(())
            } else {
                Err(ExternalUtf8Error::Invalid { offset })
            };
        }
        let length = carry + read;
        if let Some(index) = buffer[..length].iter().position(|byte| *byte == 0) {
            return Err(ExternalUtf8Error::Binary {
                offset: offset + index as u64,
            });
        }
        match std::str::from_utf8(&buffer[..length]) {
            Ok(_) => {
                offset += length as u64;
                carry = 0;
            }
            Err(error) if error.error_len().is_none() => {
                let valid = error.valid_up_to();
                offset += valid as u64;
                carry = length - valid;
                buffer.copy_within(valid..length, 0);
            }
            Err(error) => {
                return Err(ExternalUtf8Error::Invalid {
                    offset: offset + error.valid_up_to() as u64,
                });
            }
        }
    }
}

fn normalize_absolute_path(path: &Path) -> Result<PathBuf, EngineError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => normalized.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return Err(EngineError::Io(
                        "external path escapes its filesystem root".to_owned(),
                    ));
                }
            }
            std::path::Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn external_item_id(path: &Path) -> Result<ItemId, EngineError> {
    let path = path
        .to_str()
        .ok_or_else(|| EngineError::Io("external path is not valid UTF-8".to_owned()))?;
    let digest = Sha256::digest(path.as_bytes());
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    ItemId::new(format!("external/{encoded}"))
}

struct MarkdownSearchProvider {
    workspace: PathBuf,
    index: Mutex<Option<SearchIndex>>,
}

impl MarkdownSearchProvider {
    fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            index: Mutex::new(None),
        }
    }
}

impl SearchProvider for MarkdownSearchProvider {
    fn search(&self, request: &SearchRequest) -> Result<Vec<SearchHit>, EngineError> {
        let mut guard = self
            .index
            .lock()
            .map_err(|_| EngineError::Io("markdown search lock failed".to_owned()))?;
        if guard.is_none() {
            *guard = Some(SearchIndex::open_or_rebuild(&self.workspace).map_err(engine_io)?);
        }
        let results = guard
            .as_ref()
            .expect("search index initialized")
            .query(&request.query, request.limit)
            .map_err(engine_io)?;
        results
            .into_iter()
            .map(|result| {
                Ok(SearchHit {
                    engine_id: markdown_engine_id(),
                    item_id: ItemId::new(result.relative_path)?,
                    title: result.title,
                    snippet: result.snippet,
                    score_micros: (result.score.max(0.0) * 1_000_000.0) as u64,
                })
            })
            .collect()
    }
}

struct MarkdownLocalSearchProvider;

impl LocalSearchProvider for MarkdownLocalSearchProvider {
    fn search_document(
        &self,
        request: &LocalSearchRequest,
        document: &dyn LocalSearchDocument,
    ) -> Result<Vec<LocalSearchMatch>, EngineError> {
        Ok(document.find_case_insensitive(&request.query, request.limit))
    }
}

pub fn markdown_engine_id() -> EngineId {
    EngineId::new(MARKDOWN_ENGINE_ID).expect("built-in markdown engine id is valid")
}

fn engine_io(error: impl std::fmt::Display) -> EngineError {
    EngineError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use tempfile::TempDir;

    struct SearchDocument;

    impl LocalSearchDocument for SearchDocument {
        fn find_case_insensitive(&self, query: &str, limit: usize) -> Vec<LocalSearchMatch> {
            assert_eq!(query, "needle");
            assert_eq!(limit, 3);
            vec![LocalSearchMatch {
                start_byte: 2,
                end_byte: 8,
            }]
        }
    }

    #[test]
    fn markdown_factory_has_stable_empty_contract() {
        let factory = MarkdownEngineFactory;
        assert_eq!(factory.id().as_str(), "markdown");
        assert_eq!(factory.display_name(), "Markdown");
        assert!(factory.settings_schema().fields.is_empty());
        assert!(factory.capabilities().create);
        assert!(factory.capabilities().global_search);
        assert!(factory.capabilities().local_search);
        assert!(factory.capabilities().external_files);
        assert!(factory.external_file_fallback());
        assert_eq!(
            factory.external_file_extensions(),
            ["md", "markdown", "txt"]
        );
        assert!(!factory.capabilities().settings);
    }

    #[test]
    fn markdown_local_search_delegates_to_the_open_document() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("notes")).unwrap();
        let engine = MarkdownEngine::open(workspace.path()).unwrap();
        let matches = engine
            .local_search_provider()
            .unwrap()
            .search_document(
                &LocalSearchRequest {
                    item_id: ItemId::new("notes/Example.md").unwrap(),
                    query: "needle".to_owned(),
                    limit: 3,
                },
                &SearchDocument,
            )
            .unwrap();
        assert_eq!(matches[0].start_byte, 2);
        assert_eq!(matches[0].end_byte, 8);
    }

    #[test]
    fn opening_and_cataloging_preserve_markdown_bytes() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("notes")).unwrap();
        let path = workspace.path().join("notes/Alpha.md");
        let original = b"---\ntitle: Alpha\nunknown: retained\n---\n# Alpha\nneedle\n";
        fs::write(&path, original).unwrap();

        let engine = MarkdownEngine::open(workspace.path()).unwrap();
        let items = engine.items().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].engine_id.as_str(), "markdown");
        assert_eq!(items[0].item_id.as_str(), "notes/Alpha.md");
        assert_eq!(items[0].metadata.title, "Alpha");
        assert_eq!(items[0].availability, ItemAvailability::Ready);
        assert_eq!(items[0].badge, None);
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn external_identity_is_stable_deduplicated_and_case_insensitive() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("notes")).unwrap();
        let path = workspace.path().join("External.MD");
        fs::write(&path, "---\ntitle: literal\n---\n# Body\n").unwrap();
        let mut engine = MarkdownEngine::open(workspace.path()).unwrap();

        let first = engine.open_external_file(&path).unwrap();
        let second = engine.open_external_file(&path).unwrap();
        assert_eq!(first.item_id, second.item_id);
        assert_eq!(first.title, "External.MD");
        assert_eq!(engine.external_files().len(), 1);

        let reopened = MarkdownEngine::open(workspace.path())
            .unwrap()
            .open_external_file(&path)
            .unwrap();
        assert_eq!(first.item_id, reopened.item_id);
    }

    #[test]
    fn invalid_utf8_and_non_files_are_rejected_without_registration() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("notes")).unwrap();
        let invalid = workspace.path().join("invalid.txt");
        fs::write(&invalid, [0xff, 0xfe]).unwrap();
        let directory = workspace.path().join("directory.md");
        fs::create_dir(&directory).unwrap();
        let mut engine = MarkdownEngine::open(workspace.path()).unwrap();

        assert!(engine.open_external_file(&invalid).is_err());
        assert!(engine.open_external_file(&directory).is_err());
        assert!(engine.external_files().is_empty());
    }

    #[test]
    fn external_text_accepts_any_name_without_changing_bytes() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("notes")).unwrap();
        let mut engine = MarkdownEngine::open(workspace.path()).unwrap();
        let names = [
            "app.LOG",
            "data.json",
            "data.csv",
            "data.tsv",
            "index.php",
            "app.js",
            "index.html",
            "README",
            "Dockerfile",
            ".env",
            "custom.unknown",
            "file.",
        ];
        for (index, name) in names.iter().enumerate() {
            let path = workspace.path().join(name);
            let original = "\u{feff}Привет 日本語\tvalue\r\n\x1b[31mred\x1b[0m\n";
            fs::write(&path, original).unwrap();
            let summary = engine.open_external_file(&path).unwrap();
            assert_eq!(summary.availability, ItemAvailability::Ready);
            assert_eq!(
                engine.open_external_file(&path).unwrap().item_id,
                summary.item_id
            );
            assert_eq!(engine.external_files().len(), index + 1);
            assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        }
        let empty = workspace.path().join("empty");
        fs::write(&empty, []).unwrap();
        assert_eq!(
            engine.open_external_file(&empty).unwrap().availability,
            ItemAvailability::Ready
        );
    }

    #[test]
    fn external_binary_data_is_rejected_at_any_offset_without_registration() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("notes")).unwrap();
        let mut engine = MarkdownEngine::open(workspace.path()).unwrap();
        for name in ["binary", "binary.log", "binary.unknown"] {
            let path = workspace.path().join(name);
            for offset in [
                0,
                UTF8_READ_BYTES - 1,
                UTF8_READ_BYTES,
                UTF8_READ_BYTES * 3 + 5,
            ] {
                for byte in [0, 0xff] {
                    let mut data = vec![b'a'; offset];
                    data.push(byte);
                    fs::write(&path, &data).unwrap();
                    assert!(engine.open_external_file(&path).is_err());
                    assert!(engine.external_files().is_empty());
                    assert_eq!(fs::read(&path).unwrap(), data);
                }
            }
        }
        let mut data = vec![b'a'; UTF8_READ_BYTES - 1];
        data.extend_from_slice("я\0".as_bytes());
        assert!(matches!(validate_external_utf8(data.as_slice()),
            Err(ExternalUtf8Error::Binary { offset }) if offset == UTF8_READ_BYTES as u64 + 1));
    }

    #[test]
    fn external_utf8_accepts_characters_split_at_every_block_boundary() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("notes")).unwrap();
        let path = workspace.path().join("unicode.txt");
        let mut engine = MarkdownEngine::open(workspace.path()).unwrap();
        for character in ["я", "界", "😀"] {
            for split in 1..character.len() {
                let content = format!("{}{character}tail", "a".repeat(UTF8_READ_BYTES - split));
                fs::write(&path, &content).unwrap();
                let summary = engine.open_external_file(&path).unwrap();
                assert_eq!(summary.availability, ItemAvailability::Ready);
                assert_eq!(fs::read_to_string(&path).unwrap(), content);
            }
        }
        assert_eq!(engine.external_files().len(), 1);
        validate_external_utf8(io::empty()).unwrap();
        validate_external_utf8(io::repeat(b'a').take(UTF8_READ_BYTES as u64)).unwrap();
    }

    #[test]
    fn external_utf8_rejects_invalid_and_truncated_sequences_after_the_first_block() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("notes")).unwrap();
        let path = workspace.path().join("invalid.txt");
        let mut engine = MarkdownEngine::open(workspace.path()).unwrap();
        for (offset, suffix) in [
            (UTF8_READ_BYTES + 7, &[0xff][..]),
            (UTF8_READ_BYTES - 1, &[0xf0, 0x9f][..]),
            (UTF8_READ_BYTES - 1, &[0xf0, b'a'][..]),
            (UTF8_READ_BYTES + 7, &[0xe2, 0x82][..]),
        ] {
            let mut content = vec![b'a'; offset];
            content.extend_from_slice(suffix);
            assert!(matches!(
                validate_external_utf8(content.as_slice()),
                Err(ExternalUtf8Error::Invalid { offset: actual }) if actual == offset as u64
            ));
            fs::write(&path, &content).unwrap();
            assert!(engine.open_external_file(&path).is_err());
            assert!(engine.external_files().is_empty());
        }
    }

    #[test]
    fn external_utf8_retries_interruptions_and_accepts_short_reads() {
        struct ShortReads<'a> {
            bytes: &'a [u8],
            interrupt: bool,
        }
        impl Read for ShortReads<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.interrupt = !self.interrupt;
                if self.interrupt {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.bytes.read(&mut buffer[..1])
            }
        }
        validate_external_utf8(ShortReads {
            bytes: "ASCII я界😀".as_bytes(),
            interrupt: false,
        })
        .unwrap();
    }

    #[test]
    fn external_utf8_preserves_read_errors_and_missing_files_remain_unavailable() {
        struct FailingRead;
        impl Read for FailingRead {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "read failed",
                ))
            }
        }
        // A read error while a character is incomplete must stay an I/O error.
        let input = io::Cursor::new([0xf0, 0x9f]).chain(FailingRead);
        assert!(matches!(
            validate_external_utf8(input),
            Err(ExternalUtf8Error::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied
                && error.to_string() == "read failed"
        ));
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("notes")).unwrap();
        let mut engine = MarkdownEngine::open(workspace.path()).unwrap();
        let summary = engine
            .open_external_file(&workspace.path().join("missing.txt"))
            .unwrap();
        assert!(matches!(
            summary.availability,
            ItemAvailability::Unavailable(_)
        ));
        assert_eq!(engine.external_files().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn external_large_file_attachment_is_streamed() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("notes")).unwrap();
        let path = workspace.path().join("large.log");
        // Write real text in bounded chunks: sparse NUL-filled files are binary.
        // This also runs under a 192 MiB container limit to catch whole-file allocations.
        let size = 1024 * 1024 * 1024;
        io::copy(
            &mut io::repeat(b'a').take(size),
            &mut fs::File::create(&path).unwrap(),
        )
        .unwrap();
        let mut engine = MarkdownEngine::open(workspace.path()).unwrap();
        assert_eq!(
            engine.open_external_file(&path).unwrap().availability,
            ItemAvailability::Ready
        );
        assert_eq!(engine.external_files().len(), 1);
        assert_eq!(fs::metadata(path).unwrap().len(), size);
    }

    #[cfg(unix)]
    #[test]
    fn external_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("notes")).unwrap();
        let target = workspace.path().join("target.md");
        let link = workspace.path().join("link.md");
        fs::write(&target, "target").unwrap();
        symlink(&target, &link).unwrap();
        let mut engine = MarkdownEngine::open(workspace.path()).unwrap();

        assert!(engine.open_external_file(&link).is_err());
        assert!(engine.external_files().is_empty());
    }
}
