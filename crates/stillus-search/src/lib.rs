// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Disposable, rebuildable local full-text search for Stillus workspaces.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use stillus_platform::fs::{self, File, OpenOptions};

use stillus_frontmatter::FrontMatterStatus;
use stillus_storage::{NoteScanResult, scan_note};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{
    Field, IndexRecordOption, STORED, STRING, Schema, TantivyDocument, TextFieldIndexing,
    TextOptions, Value,
};
use tantivy::tokenizer::NgramTokenizer;
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, Term};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

pub const INDEX_SCHEMA_VERSION: u32 = 1;
pub const MAX_QUERY_CHARS: usize = 256;
pub const MAX_RESULTS: usize = 50;
pub const MAX_SNIPPET_CHARS: usize = 180;
const BODY_CHUNK_BYTES: usize = 64 * 1024;
const BODY_OVERLAP_CHARS: usize = MAX_QUERY_CHARS - 1;
const BODY_READ_BYTES: usize = BODY_CHUNK_BYTES - (MAX_QUERY_CHARS * 4);
const WRITER_MEMORY_BYTES: usize = 20_000_000;
const MANIFEST: &str = "STILLUS_SEARCH\nschema=1\n";
const BODY_CANDIDATE_MULTIPLIER: usize = 12;
const BODY_TOKENIZER: &str = "stillus_body_ngram3";
static GENERATION_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MatchKind {
    Body,
    Tag,
    Title,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchResult {
    pub relative_path: String,
    pub title: String,
    pub tags: Vec<String>,
    pub snippet: String,
    pub match_kind: MatchKind,
    pub score: f32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReconcileReport {
    pub added_or_updated: usize,
    pub removed: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SearchError {
    Workspace(String),
    InvalidPath(String),
    Io(String),
    Index(String),
    Corrupt(String),
}

impl fmt::Display for SearchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workspace(message) => write!(formatter, "workspace search error: {message}"),
            Self::InvalidPath(message) => write!(formatter, "invalid search path: {message}"),
            Self::Io(message) => write!(formatter, "search I/O error: {message}"),
            Self::Index(message) => write!(formatter, "search index error: {message}"),
            Self::Corrupt(message) => write!(formatter, "search index is corrupt: {message}"),
        }
    }
}

impl std::error::Error for SearchError {}

impl From<io::Error> for SearchError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<tantivy::TantivyError> for SearchError {
    fn from(error: tantivy::TantivyError) -> Self {
        Self::Index(error.to_string())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileStamp {
    len: u64,
    modified_nanos: u128,
}

#[derive(Clone, Debug)]
struct CatalogEntry {
    absolute_path: PathBuf,
    relative_path: String,
    title: String,
    tags: Vec<String>,
    body_offset: u64,
    index_body: bool,
    stamp: FileStamp,
}

#[derive(Clone, Copy)]
struct SearchFields {
    path: Field,
    chunk: Field,
    body: Field,
    display: Field,
}

pub struct SearchIndex {
    workspace: PathBuf,
    search_root: PathBuf,
    generation_name: String,
    index: Index,
    reader: IndexReader,
    fields: SearchFields,
    catalog: BTreeMap<String, CatalogEntry>,
    indexed_stamps: BTreeMap<String, FileStamp>,
    excluded: BTreeSet<String>,
    #[cfg(test)]
    last_reconcile_scanned_files: usize,
}

impl SearchIndex {
    pub fn open_or_rebuild(workspace: impl AsRef<Path>) -> Result<Self, SearchError> {
        let workspace = workspace.as_ref().to_path_buf();
        validate_workspace(&workspace)?;
        match Self::open_current(&workspace) {
            Ok(index) => Ok(index),
            Err(_) => Self::build_and_publish(&workspace, &BTreeSet::new(), PublishFault::None),
        }
    }

    pub fn rebuild(&mut self) -> Result<(), SearchError> {
        let replacement =
            Self::build_and_publish(&self.workspace, &self.excluded, PublishFault::None)?;
        *self = replacement;
        Ok(())
    }

    pub fn reconcile(&mut self) -> Result<ReconcileReport, SearchError> {
        let (current, _scanned_files) =
            scan_catalog_incremental(&self.workspace, &self.catalog, &self.excluded)?;
        #[cfg(test)]
        {
            self.last_reconcile_scanned_files = _scanned_files;
        }
        let current_stamps = current
            .iter()
            .map(|(path, entry)| (path.clone(), entry.stamp.clone()))
            .collect::<BTreeMap<_, _>>();
        let removed = self
            .indexed_stamps
            .keys()
            .filter(|path| !current_stamps.contains_key(*path))
            .cloned()
            .collect::<Vec<_>>();
        let changed = current_stamps
            .iter()
            .filter(|(path, stamp)| self.indexed_stamps.get(*path) != Some(*stamp))
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();

        if removed.is_empty() && changed.is_empty() {
            self.catalog = current;
            return Ok(ReconcileReport::default());
        }

        let mut writer = self.index.writer(WRITER_MEMORY_BYTES)?;
        for path in removed.iter().chain(changed.iter()) {
            writer.delete_term(Term::from_field_text(self.fields.path, path));
        }
        for path in &changed {
            if let Some(entry) = current.get(path) {
                let _ = index_entry(&mut writer, self.fields, entry)?;
            }
        }
        writer.commit()?;
        self.reader.reload()?;

        let generation_path = self.search_root.join(&self.generation_name);
        write_catalog_atomic(&generation_path, &current_stamps)?;
        self.catalog = current;
        self.indexed_stamps = current_stamps;
        Ok(ReconcileReport {
            added_or_updated: changed.len(),
            removed: removed.len(),
        })
    }

    pub fn purge(&mut self, note_path: impl AsRef<Path>) -> Result<(), SearchError> {
        self.purge_with_fault(note_path.as_ref(), PurgeFault::None)
    }

    fn purge_with_fault(&mut self, note_path: &Path, fault: PurgeFault) -> Result<(), SearchError> {
        let relative_path = relative_note_path(&self.workspace, note_path)?;
        self.excluded.insert(relative_path.clone());
        let result = Self::build_and_publish(&self.workspace, &self.excluded, PublishFault::None);
        match result {
            Ok(replacement) => {
                *self = replacement;
                self.excluded.insert(relative_path);
                if fault == PurgeFault::AfterPublish {
                    return Err(SearchError::Io(
                        "injected cleanup failure after purge publication".to_owned(),
                    ));
                }
                cleanup_generations_with_fault(
                    &self.search_root,
                    &self.generation_name,
                    true,
                    fault,
                )?;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Restores a note to the disposable index after a later operation made a purge unnecessary.
    ///
    /// A successful restore publishes a physically rebuilt generation before removing the note
    /// from the in-memory exclusion set. Repeating the call after success is a no-op. If building
    /// or publishing the replacement fails, the currently published generation and exclusion set
    /// remain unchanged, and any unpublished generation is removed.
    pub fn restore_after_failed_purge(
        &mut self,
        note_path: impl AsRef<Path>,
    ) -> Result<(), SearchError> {
        self.restore_after_failed_purge_with_fault(note_path.as_ref(), PublishFault::None)
    }

    fn restore_after_failed_purge_with_fault(
        &mut self,
        note_path: &Path,
        fault: PublishFault,
    ) -> Result<(), SearchError> {
        let relative_path = relative_note_path(&self.workspace, note_path)?;
        if !self.excluded.contains(&relative_path) {
            return Ok(());
        }

        let mut restored_exclusions = self.excluded.clone();
        restored_exclusions.remove(&relative_path);
        match Self::build_and_publish(&self.workspace, &restored_exclusions, fault) {
            Ok(replacement) => {
                *self = replacement;
                Ok(())
            }
            Err(error) => {
                cleanup_generations(&self.search_root, &self.generation_name, true).map_err(
                    |cleanup_error| {
                        SearchError::Io(format!(
                            "{error}; failed to remove unpublished restore generation: {cleanup_error}"
                        ))
                    },
                )?;
                Err(error)
            }
        }
    }

    pub fn query(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, SearchError> {
        let normalized = normalize_query(query);
        if normalized.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let limit = limit.min(MAX_RESULTS);
        let mut candidates = BTreeMap::<String, Candidate>::new();

        for entry in self.catalog.values() {
            if let Some((kind, score)) = metadata_score(&normalized, entry) {
                candidates.insert(
                    entry.relative_path.clone(),
                    Candidate {
                        kind,
                        score,
                        snippet: String::new(),
                    },
                );
            }
        }

        let searcher = self.reader.searcher();
        let mut parser = QueryParser::for_index(&self.index, vec![self.fields.body]);
        parser.set_conjunction_by_default();
        let escaped = escape_query_parser(&normalized);
        let (body_query, _) = parser.parse_query_lenient(&escaped);
        let body_limit = limit
            .saturating_mul(BODY_CANDIDATE_MULTIPLIER)
            .clamp(64, 600);
        let docs = searcher.search(
            &body_query,
            &TopDocs::with_limit(body_limit).order_by_score(),
        )?;
        for (score, address) in docs {
            let document = searcher.doc::<TantivyDocument>(address)?;
            let Some(path_value) = document.get_first(self.fields.path) else {
                continue;
            };
            let Some(path) = path_value.as_str() else {
                continue;
            };
            if !self.catalog.contains_key(path) {
                continue;
            }
            let snippet =
                document
                    .get_first(self.fields.display)
                    .map_or_else(String::new, |value| {
                        value
                            .as_str()
                            .map(|body| bounded_snippet(body, &normalized))
                            .unwrap_or_default()
                    });
            let body_score = 1_000.0 + score.min(999.0);
            candidates
                .entry(path.to_owned())
                .and_modify(|candidate| {
                    if candidate.kind == MatchKind::Body && body_score > candidate.score {
                        candidate.score = body_score;
                        candidate.snippet.clone_from(&snippet);
                    }
                })
                .or_insert(Candidate {
                    kind: MatchKind::Body,
                    score: body_score,
                    snippet,
                });
        }

        let mut results = candidates
            .into_iter()
            .filter_map(|(relative_path, candidate)| {
                self.catalog.get(&relative_path).map(|entry| SearchResult {
                    relative_path,
                    title: entry.title.clone(),
                    tags: entry.tags.clone(),
                    snippet: candidate.snippet,
                    match_kind: candidate.kind,
                    score: candidate.score,
                })
            })
            .collect::<Vec<_>>();
        results.sort_by(|left, right| {
            right
                .match_kind
                .cmp(&left.match_kind)
                .then_with(|| right.score.total_cmp(&left.score))
                .then_with(|| normalize_query(&left.title).cmp(&normalize_query(&right.title)))
                .then_with(|| left.relative_path.cmp(&right.relative_path))
        });
        results.truncate(limit);
        Ok(results)
    }

    pub fn search_root(&self) -> &Path {
        &self.search_root
    }

    pub fn generation_name(&self) -> &str {
        &self.generation_name
    }

    fn open_current(workspace: &Path) -> Result<Self, SearchError> {
        let search_root = workspace.join(".stillus/search");
        validate_search_root(&search_root)?;
        let generation_name = read_current_pointer(&search_root)?;
        let generation_path = search_root.join(&generation_name);
        validate_generation_directory(&generation_path)?;
        validate_manifest(&generation_path)?;
        let index = Index::open_in_dir(&generation_path)?;
        configure_index(&index)?;
        let expected_schema = search_schema();
        if index.schema() != expected_schema {
            return Err(SearchError::Corrupt("schema mismatch".to_owned()));
        }
        let fields = fields(&expected_schema)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let catalog = scan_catalog(workspace)?;
        let indexed_stamps = read_catalog(&generation_path)?;
        Ok(Self {
            workspace: workspace.to_path_buf(),
            search_root,
            generation_name,
            index,
            reader,
            fields,
            catalog,
            indexed_stamps,
            excluded: BTreeSet::new(),
            #[cfg(test)]
            last_reconcile_scanned_files: 0,
        })
    }

    fn build_and_publish(
        workspace: &Path,
        excluded: &BTreeSet<String>,
        fault: PublishFault,
    ) -> Result<Self, SearchError> {
        validate_workspace(workspace)?;
        let search_root = workspace.join(".stillus/search");
        validate_search_root(&search_root)?;
        fs::create_dir_all(&search_root)?;
        let unique = generation_suffix();
        let building_name = format!(".building-{unique}");
        let generation_name = format!("generation-{unique}");
        let building_path = search_root.join(&building_name);
        fs::create_dir(&building_path)?;

        let schema = search_schema();
        let fields = fields(&schema)?;
        let index = Index::create_in_dir(&building_path, schema.clone())?;
        configure_index(&index)?;
        let mut writer: IndexWriter = index.writer(WRITER_MEMORY_BYTES)?;
        let (catalog, _) = scan_catalog_incremental(workspace, &BTreeMap::new(), excluded)?;
        let indexed_stamps = catalog
            .iter()
            .map(|(path, entry)| (path.clone(), entry.stamp.clone()))
            .collect::<BTreeMap<_, _>>();
        for entry in catalog.values() {
            let _ = index_entry(&mut writer, fields, entry)?;
        }
        writer.commit()?;
        // Windows cannot move a directory while Tantivy still owns open writer
        // and mmap handles within it. Complete merges and close the old index
        // before publishing its generation under the final directory name.
        writer.wait_merging_threads()?;
        drop(index);
        write_synced(&building_path.join("stillus.manifest"), MANIFEST.as_bytes())?;
        write_catalog(&building_path, &indexed_stamps)?;
        sync_directory(&building_path)?;

        let generation_path = search_root.join(&generation_name);
        fs::rename(&building_path, &generation_path)?;
        sync_directory(&search_root)?;
        if fault == PublishFault::BeforePointer {
            return Err(SearchError::Io(
                io::Error::from(io::ErrorKind::PermissionDenied).to_string(),
            ));
        }

        validate_manifest(&generation_path)?;
        let index = Index::open_in_dir(&generation_path)?;
        configure_index(&index)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        write_current_pointer(&search_root, &generation_name)?;
        let result = Self {
            workspace: workspace.to_path_buf(),
            search_root: search_root.clone(),
            generation_name: generation_name.clone(),
            index,
            reader,
            fields,
            catalog,
            indexed_stamps,
            excluded: excluded.clone(),
            #[cfg(test)]
            last_reconcile_scanned_files: 0,
        };
        cleanup_generations(&search_root, &generation_name, false)?;
        Ok(result)
    }
}

#[derive(Clone, Debug)]
struct Candidate {
    kind: MatchKind,
    score: f32,
    snippet: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublishFault {
    None,
    BeforePointer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PurgeFault {
    None,
    AfterPublish,
    BeforeCleanupSync,
}

fn validate_workspace(workspace: &Path) -> Result<(), SearchError> {
    let notes = workspace.join("notes");
    if !notes.is_dir() {
        return Err(SearchError::Workspace(
            "workspace notes directory is missing".to_owned(),
        ));
    }
    Ok(())
}

fn validate_search_root(search_root: &Path) -> Result<(), SearchError> {
    if let Some(state_root) = search_root.parent() {
        reject_symlink_or_non_directory(state_root)?;
    }
    reject_symlink_or_non_directory(search_root)
}

fn reject_symlink_or_non_directory(path: &Path) -> Result<(), SearchError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(SearchError::InvalidPath(
            format!("managed search path is a symlink: {}", path.display()),
        )),
        Ok(metadata) if !metadata.is_dir() => Err(SearchError::InvalidPath(format!(
            "managed search path is not a directory: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn validate_generation_directory(generation_path: &Path) -> Result<(), SearchError> {
    let metadata = fs::symlink_metadata(generation_path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SearchError::Corrupt(
            "generation is not a regular directory".to_owned(),
        ));
    }
    for entry in fs::read_dir(generation_path)? {
        if entry?.file_type()?.is_symlink() {
            return Err(SearchError::Corrupt(
                "generation contains a symlink".to_owned(),
            ));
        }
    }
    Ok(())
}

fn search_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("path", STRING | STORED);
    builder.add_u64_field("chunk", STORED);
    let body_indexing = TextFieldIndexing::default()
        .set_tokenizer(BODY_TOKENIZER)
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    builder.add_text_field(
        "body",
        TextOptions::default().set_indexing_options(body_indexing),
    );
    builder.add_text_field("display", STORED);
    builder.build()
}

fn configure_index(index: &Index) -> Result<(), SearchError> {
    let tokenizer =
        NgramTokenizer::new(3, 3, false).map_err(|error| SearchError::Index(error.to_string()))?;
    index.tokenizers().register(BODY_TOKENIZER, tokenizer);
    Ok(())
}

fn fields(schema: &Schema) -> Result<SearchFields, SearchError> {
    Ok(SearchFields {
        path: schema
            .get_field("path")
            .map_err(|_| SearchError::Corrupt("path field missing".to_owned()))?,
        chunk: schema
            .get_field("chunk")
            .map_err(|_| SearchError::Corrupt("chunk field missing".to_owned()))?,
        body: schema
            .get_field("body")
            .map_err(|_| SearchError::Corrupt("body field missing".to_owned()))?,
        display: schema
            .get_field("display")
            .map_err(|_| SearchError::Corrupt("display field missing".to_owned()))?,
    })
}

fn scan_catalog(workspace: &Path) -> Result<BTreeMap<String, CatalogEntry>, SearchError> {
    scan_catalog_incremental(workspace, &BTreeMap::new(), &BTreeSet::new())
        .map(|(catalog, _)| catalog)
}

fn scan_catalog_incremental(
    workspace: &Path,
    previous: &BTreeMap<String, CatalogEntry>,
    excluded: &BTreeSet<String>,
) -> Result<(BTreeMap<String, CatalogEntry>, usize), SearchError> {
    let notes_directory = workspace.join("notes");
    let directory_metadata = fs::symlink_metadata(&notes_directory)?;
    if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
        return Err(SearchError::InvalidPath(
            "workspace notes path must be a real directory".to_owned(),
        ));
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(&notes_directory)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file()
            && path.extension().is_some_and(|extension| extension == "md")
        {
            paths.push(path);
        }
    }
    paths.sort();

    let mut catalog = BTreeMap::new();
    let mut scanned_files = 0_usize;
    for path in paths {
        let relative_path = relative_note_path(workspace, &path)?;
        if excluded.contains(&relative_path) {
            continue;
        }
        let metadata = match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => continue,
        };
        let stamp = file_stamp(&metadata);
        if let Some(existing) = previous
            .get(&relative_path)
            .filter(|entry| entry.stamp == stamp)
        {
            catalog.insert(relative_path, existing.clone());
            continue;
        }

        scanned_files = scanned_files.saturating_add(1);
        let result = match scan_note(&path) {
            Ok(result) => result,
            Err(_) => continue,
        };
        let (title, tags, body_offset, index_body) = match result {
            NoteScanResult::Scanned(scan) => match scan.frontmatter.status {
                FrontMatterStatus::Plain => (
                    scan.body_title.unwrap_or_else(|| fallback_title(&path)),
                    Vec::new(),
                    0,
                    true,
                ),
                FrontMatterStatus::Parsed(parsed) => {
                    if parsed.metadata.deleted.unwrap_or(false) {
                        continue;
                    }
                    (
                        scan.body_title.unwrap_or_else(|| {
                            parsed
                                .metadata
                                .title
                                .unwrap_or_else(|| fallback_title(&path))
                        }),
                        parsed.metadata.tags,
                        parsed.body_offset,
                        true,
                    )
                }
                FrontMatterStatus::Invalid { body_offset, .. } => (
                    fallback_title(&path),
                    Vec::new(),
                    body_offset.unwrap_or(0),
                    true,
                ),
            },
            NoteScanResult::Protected(scan) => match scan.frontmatter.status {
                FrontMatterStatus::Parsed(parsed) => {
                    if parsed.metadata.deleted.unwrap_or(false) {
                        continue;
                    }
                    (
                        parsed
                            .metadata
                            .title
                            .unwrap_or_else(|| fallback_title(&path)),
                        parsed.metadata.tags,
                        scan.body_offset,
                        false,
                    )
                }
                _ => continue,
            },
            NoteScanResult::LegacyProtected
            | NoteScanResult::InvalidProtected(_)
            | NoteScanResult::IoError(_) => continue,
        };
        catalog.insert(
            relative_path.clone(),
            CatalogEntry {
                absolute_path: path,
                relative_path,
                title,
                tags,
                body_offset,
                index_body,
                stamp,
            },
        );
    }
    Ok((catalog, scanned_files))
}

fn file_stamp(metadata: &fs::Metadata) -> FileStamp {
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    FileStamp {
        len: metadata.len(),
        modified_nanos,
    }
}

fn fallback_title(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Untitled".to_owned())
}

fn relative_note_path(workspace: &Path, path: &Path) -> Result<String, SearchError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };
    let relative = absolute
        .strip_prefix(workspace)
        .map_err(|_| SearchError::InvalidPath("note is outside workspace".to_owned()))?;
    let components = relative.components().collect::<Vec<_>>();
    if components.len() != 2
        || components[0] != Component::Normal("notes".as_ref())
        || !matches!(components[1], Component::Normal(_))
        || relative
            .extension()
            .is_none_or(|extension| extension != "md")
    {
        return Err(SearchError::InvalidPath(
            "note must be a direct UTF-8 .md child of notes".to_owned(),
        ));
    }
    relative
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| format!("notes/{name}"))
        .ok_or_else(|| SearchError::InvalidPath("note path is not UTF-8".to_owned()))
}

fn index_entry(
    writer: &mut IndexWriter,
    fields: SearchFields,
    entry: &CatalogEntry,
) -> Result<bool, SearchError> {
    if !entry.index_body {
        return Ok(true);
    }
    let path_term = Term::from_field_text(fields.path, &entry.relative_path);
    let mut ordinal = 0_u64;
    let mut overlap = String::new();
    let result = stream_utf8_chunks(&entry.absolute_path, entry.body_offset, |chunk| {
        let indexed_chunk = format!("{overlap}{chunk}");
        let mut document = TantivyDocument::default();
        document.add_text(fields.path, &entry.relative_path);
        document.add_u64(fields.chunk, ordinal);
        document.add_text(fields.body, normalize_text(&indexed_chunk));
        document.add_text(fields.display, &indexed_chunk);
        writer.add_document(document)?;
        overlap = trailing_chars(&indexed_chunk, BODY_OVERLAP_CHARS);
        ordinal = ordinal.saturating_add(1);
        Ok(())
    });
    if result.is_err() {
        writer.delete_term(path_term);
        return Ok(false);
    }
    Ok(true)
}

fn stream_utf8_chunks(
    path: &Path,
    offset: u64,
    mut consume: impl FnMut(&str) -> Result<(), SearchError>,
) -> Result<(), SearchError> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = vec![0_u8; BODY_READ_BYTES];
    let mut carry = Vec::<u8>::new();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let mut bytes = Vec::with_capacity(carry.len() + read);
        bytes.extend_from_slice(&carry);
        bytes.extend_from_slice(&buffer[..read]);
        carry.clear();
        match std::str::from_utf8(&bytes) {
            Ok(text) => consume(text)?,
            Err(error) if error.error_len().is_none() => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    let text = std::str::from_utf8(&bytes[..valid]).map_err(|_| {
                        SearchError::Corrupt("UTF-8 validation changed unexpectedly".to_owned())
                    })?;
                    consume(text)?;
                }
                carry.extend_from_slice(&bytes[valid..]);
                if carry.len() > 3 {
                    return Err(SearchError::Corrupt("note body is not UTF-8".to_owned()));
                }
            }
            Err(_) => return Err(SearchError::Corrupt("note body is not UTF-8".to_owned())),
        }
    }
    if !carry.is_empty() {
        return Err(SearchError::Corrupt("note body is not UTF-8".to_owned()));
    }
    Ok(())
}

fn trailing_chars(value: &str, count: usize) -> String {
    let start = value
        .char_indices()
        .rev()
        .nth(count.saturating_sub(1))
        .map_or(0, |(index, _)| index);
    value[start..].to_owned()
}

pub fn normalize_query(value: &str) -> String {
    let limited = value.chars().take(MAX_QUERY_CHARS).collect::<String>();
    normalize_text(&limited)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_text(value: &str) -> String {
    value
        .nfkc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
}

fn metadata_score(query: &str, entry: &CatalogEntry) -> Option<(MatchKind, f32)> {
    let title = normalize_query(&entry.title);
    if let Some(score) = field_score(query, &title, 5_000.0, 4_600.0, 4_200.0, 3_800.0) {
        return Some((MatchKind::Title, score));
    }
    entry
        .tags
        .iter()
        .filter_map(|tag| {
            field_score(
                query,
                &normalize_query(tag),
                3_400.0,
                3_100.0,
                2_800.0,
                2_400.0,
            )
        })
        .max_by(f32::total_cmp)
        .map(|score| (MatchKind::Tag, score))
}

fn field_score(
    query: &str,
    candidate: &str,
    exact: f32,
    prefix: f32,
    contains: f32,
    fuzzy: f32,
) -> Option<f32> {
    if candidate == query {
        Some(exact)
    } else if candidate.starts_with(query) {
        Some(
            prefix
                - (candidate
                    .chars()
                    .count()
                    .saturating_sub(query.chars().count()) as f32),
        )
    } else if let Some(position) = candidate.find(query) {
        Some(contains - position as f32)
    } else {
        subsequence_penalty(query, candidate).map(|penalty| fuzzy - penalty as f32)
    }
}

fn subsequence_penalty(query: &str, candidate: &str) -> Option<usize> {
    let mut query_chars = query.chars();
    let mut expected = query_chars.next()?;
    let mut first = None;
    let mut matched = 0_usize;
    for (index, character) in candidate.chars().enumerate() {
        if character == expected {
            first.get_or_insert(index);
            matched += 1;
            if let Some(next) = query_chars.next() {
                expected = next;
            } else {
                let span = index.saturating_sub(first.unwrap_or(0)).saturating_add(1);
                return Some(span.saturating_sub(matched) + first.unwrap_or(0));
            }
        }
    }
    None
}

fn bounded_snippet(body: &str, normalized_query: &str) -> String {
    let (normalized_body, original_character_offsets) = normalize_with_offsets(body);
    let match_character = normalized_body
        .find(normalized_query)
        .map(|byte| normalized_body[..byte].chars().count())
        .unwrap_or(0);
    let start_character = original_character_offsets
        .get(match_character)
        .copied()
        .unwrap_or(0)
        .saturating_sub(48);
    let mut snippet = body
        .chars()
        .skip(start_character)
        .take(MAX_SNIPPET_CHARS)
        .collect::<String>();
    snippet = snippet.split_whitespace().collect::<Vec<_>>().join(" ");
    if start_character > 0 {
        snippet.insert(0, '…');
    }
    if body.chars().count() > start_character.saturating_add(MAX_SNIPPET_CHARS) {
        snippet.push('…');
    }
    snippet
}

/// Normalizes text while retaining the source character offset for every
/// normalized character. Normalizing by extended grapheme cluster preserves
/// canonical composition while ensuring compatibility expansions (for example
/// `ﬃ` -> `ffi`) still point back to the one source character that
/// produced them.
fn normalize_with_offsets(value: &str) -> (String, Vec<usize>) {
    let mut normalized = String::new();
    let mut original_character_offsets = Vec::new();
    let mut original_character = 0_usize;
    for grapheme in value.graphemes(true) {
        for character in grapheme.nfkc().flat_map(char::to_lowercase) {
            normalized.push(character);
            original_character_offsets.push(original_character);
        }
        original_character = original_character.saturating_add(grapheme.chars().count());
    }
    (normalized, original_character_offsets)
}

fn escape_query_parser(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len());
    for character in query.chars() {
        if matches!(
            character,
            '+' | '-'
                | '&'
                | '|'
                | '!'
                | '('
                | ')'
                | '{'
                | '}'
                | '['
                | ']'
                | '^'
                | '"'
                | '~'
                | '*'
                | '?'
                | ':'
                | '\\'
                | '/'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn generation_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let counter = GENERATION_ID.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{counter}", std::process::id())
}

fn read_current_pointer(search_root: &Path) -> Result<String, SearchError> {
    let pointer_path = search_root.join("CURRENT");
    let metadata = fs::symlink_metadata(&pointer_path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SearchError::Corrupt(
            "CURRENT pointer is not a regular file".to_owned(),
        ));
    }
    let pointer = fs::read_to_string(pointer_path)?;
    let name = pointer.trim();
    if !name.starts_with("generation-")
        || name.len() > 128
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err(SearchError::Corrupt("invalid CURRENT pointer".to_owned()));
    }
    Ok(name.to_owned())
}

fn write_current_pointer(search_root: &Path, generation_name: &str) -> Result<(), SearchError> {
    let temporary = search_root.join(format!("CURRENT.tmp-{}", generation_suffix()));
    write_synced(&temporary, format!("{generation_name}\n").as_bytes())?;
    fs::rename(&temporary, search_root.join("CURRENT"))?;
    sync_directory(search_root)?;
    Ok(())
}

fn validate_manifest(generation_path: &Path) -> Result<(), SearchError> {
    let manifest = fs::read_to_string(generation_path.join("stillus.manifest"))?;
    if manifest != MANIFEST {
        return Err(SearchError::Corrupt("manifest version mismatch".to_owned()));
    }
    Ok(())
}

fn write_catalog(
    generation_path: &Path,
    stamps: &BTreeMap<String, FileStamp>,
) -> Result<(), SearchError> {
    let bytes = catalog_bytes(stamps);
    write_synced(&generation_path.join("stillus.catalog"), &bytes)
}

fn write_catalog_atomic(
    generation_path: &Path,
    stamps: &BTreeMap<String, FileStamp>,
) -> Result<(), SearchError> {
    let temporary = generation_path.join(format!("stillus.catalog.tmp-{}", generation_suffix()));
    write_synced(&temporary, &catalog_bytes(stamps))?;
    fs::rename(&temporary, generation_path.join("stillus.catalog"))?;
    sync_directory(generation_path)?;
    Ok(())
}

fn catalog_bytes(stamps: &BTreeMap<String, FileStamp>) -> Vec<u8> {
    let mut output = format!("STILLUS_SEARCH_CATALOG\nschema={INDEX_SCHEMA_VERSION}\n");
    for (path, stamp) in stamps {
        output.push_str(&hex_encode(path.as_bytes()));
        output.push('\t');
        output.push_str(&stamp.len.to_string());
        output.push('\t');
        output.push_str(&stamp.modified_nanos.to_string());
        output.push('\n');
    }
    output.into_bytes()
}

fn read_catalog(generation_path: &Path) -> Result<BTreeMap<String, FileStamp>, SearchError> {
    let source = fs::read_to_string(generation_path.join("stillus.catalog"))?;
    let mut lines = source.lines();
    if lines.next() != Some("STILLUS_SEARCH_CATALOG") || lines.next() != Some("schema=1") {
        return Err(SearchError::Corrupt("catalog header mismatch".to_owned()));
    }
    let mut stamps = BTreeMap::new();
    for line in lines {
        let mut fields = line.split('\t');
        let encoded = fields
            .next()
            .ok_or_else(|| SearchError::Corrupt("catalog path missing".to_owned()))?;
        let len = fields
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| SearchError::Corrupt("catalog length invalid".to_owned()))?;
        let modified_nanos = fields
            .next()
            .and_then(|value| value.parse::<u128>().ok())
            .ok_or_else(|| SearchError::Corrupt("catalog timestamp invalid".to_owned()))?;
        if fields.next().is_some() {
            return Err(SearchError::Corrupt("catalog fields invalid".to_owned()));
        }
        let path = String::from_utf8(hex_decode(encoded)?)
            .map_err(|_| SearchError::Corrupt("catalog path is not UTF-8".to_owned()))?;
        stamps.insert(
            path,
            FileStamp {
                len,
                modified_nanos,
            },
        );
    }
    Ok(stamps)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[(byte >> 4) as usize]));
        output.push(char::from(HEX[(byte & 0x0f) as usize]));
    }
    output
}

fn hex_decode(value: &str) -> Result<Vec<u8>, SearchError> {
    if !value.len().is_multiple_of(2) {
        return Err(SearchError::Corrupt("catalog path hex invalid".to_owned()));
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_digit(pair[0])?;
            let low = hex_digit(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_digit(value: u8) -> Result<u8, SearchError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(SearchError::Corrupt("catalog path hex invalid".to_owned())),
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), SearchError> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(any(unix, windows))]
fn sync_directory(path: &Path) -> Result<(), SearchError> {
    stillus_platform::sync_directory(path)?;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn sync_directory(_path: &Path) -> Result<(), SearchError> {
    Ok(())
}

fn cleanup_generations(
    search_root: &Path,
    current_generation: &str,
    strict: bool,
) -> Result<(), SearchError> {
    cleanup_generations_with_fault(search_root, current_generation, strict, PurgeFault::None)
}

fn cleanup_generations_with_fault(
    search_root: &Path,
    current_generation: &str,
    strict: bool,
    fault: PurgeFault,
) -> Result<(), SearchError> {
    let entries = match fs::read_dir(search_root) {
        Ok(entries) => entries,
        Err(_error) if !strict => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) if !strict => continue,
            Err(error) => return Err(error.into()),
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) if !strict => continue,
            Err(error) => return Err(error.into()),
        };
        if file_type.is_dir()
            && name != current_generation
            && (name.starts_with("generation-") || name.starts_with(".building-"))
        {
            let result = fs::remove_dir_all(entry.path());
            if strict {
                result?;
            }
        } else if file_type.is_file() && name.starts_with("CURRENT.tmp-") {
            let result = fs::remove_file(entry.path());
            if strict {
                result?;
            }
        }
    }
    if strict {
        if fault == PurgeFault::BeforeCleanupSync {
            return Err(SearchError::Io(
                "injected failure before durable search cleanup sync".to_owned(),
            ));
        }
        sync_directory(search_root)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use stillus_frontmatter::MetadataPatch;
    use stillus_secure::{BodyEnvelopeWriter, MasterPassword};
    use stillus_storage::{open_versioned, rewrite_metadata_versioned};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct TestWorkspace {
        root: PathBuf,
    }

    impl TestWorkspace {
        fn new() -> Self {
            let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("stillus-search-test-{}-{id}", std::process::id()));
            fs::create_dir_all(root.join("notes")).unwrap();
            Self { root }
        }

        fn note(&self, name: &str, title: &str, tags: &[&str], body: &str) -> PathBuf {
            let path = self.root.join("notes").join(name);
            let tags = tags
                .iter()
                .map(|tag| format!("'{tag}'"))
                .collect::<Vec<_>>()
                .join(", ");
            fs::write(
                &path,
                format!("---\ntitle: '{title}'\ntags: [{tags}]\n---\n# {title}\n{body}"),
            )
            .unwrap();
            path
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn normalization_is_unicode_case_insensitive_and_bounded() {
        assert_eq!(normalize_query("  CAFE\u{301}   Plan  "), "café plan");
        assert_eq!(normalize_query("ＡＢＣ"), "abc");
        assert_eq!(
            normalize_query(&"x".repeat(400)).chars().count(),
            MAX_QUERY_CHARS
        );
        assert!(normalize_query(" \n\t ").is_empty());
    }

    #[test]
    fn ranking_prefers_title_then_tag_and_has_stable_ties() {
        let workspace = TestWorkspace::new();
        workspace.note("b.md", "Project Delta", &["Archive"], "marker body");
        workspace.note("a.md", "Project Alpha", &["Project"], "marker body");
        let index = SearchIndex::open_or_rebuild(&workspace.root).unwrap();
        let title = index.query("project", 10).unwrap();
        assert_eq!(title[0].match_kind, MatchKind::Title);
        assert_eq!(title[1].match_kind, MatchKind::Title);
        assert_eq!(
            title[0].relative_path,
            "notes/Project Alpha.md".replace("Project Alpha", "a")
        );
        let fuzzy = index.query("pjd", 10).unwrap();
        assert!(
            fuzzy
                .iter()
                .all(|result| result.match_kind == MatchKind::Title)
        );
    }

    #[test]
    fn rebuild_searches_title_tag_body_and_preserves_notes() {
        let workspace = TestWorkspace::new();
        let alpha = workspace.note(
            "Alpha.md",
            "Quarterly Planning",
            &["Finance"],
            "A uniquely searchable body marker.",
        );
        let before = fs::read(&alpha).unwrap();
        let mut index = SearchIndex::open_or_rebuild(&workspace.root).unwrap();
        assert_eq!(
            index.query("quarter", 10).unwrap()[0].match_kind,
            MatchKind::Title
        );
        assert_eq!(
            index.query("finance", 10).unwrap()[0].match_kind,
            MatchKind::Tag
        );
        let body = index.query("searchable", 10).unwrap();
        assert_eq!(body[0].match_kind, MatchKind::Body);
        assert!(body[0].snippet.contains("searchable"));
        index.rebuild().unwrap();
        assert_eq!(fs::read(alpha).unwrap(), before);
        assert_eq!(index.query("searchable", 10).unwrap().len(), 1);
    }

    #[test]
    fn protected_notes_index_only_plaintext_title_and_tags() {
        let workspace = TestWorkspace::new();
        let note = workspace.note(
            "Visible Vault.md",
            "Visible Vault",
            &["WorkTag"],
            "body-secret-oracle",
        );
        let mut index = SearchIndex::open_or_rebuild(&workspace.root).unwrap();
        assert_eq!(index.query("body-secret-oracle", 10).unwrap().len(), 1);

        index.purge(&note).unwrap();
        let password = MasterPassword::new("search protection password".to_owned());
        let body = b"# Visible Vault\nbody-secret-oracle";
        let mut encrypted =
            BodyEnvelopeWriter::new_for_test(Vec::new(), &password, body.len() as u64).unwrap();
        encrypted.write_all(body).unwrap();
        let encrypted = encrypted.finish().unwrap();
        let mut protected_file = b"---\ntitle: 'Visible Vault'\ntags: ['WorkTag']\nstillus_encryption: age-body-v1\n---\n".to_vec();
        protected_file.extend_from_slice(&encrypted);
        fs::write(&note, protected_file).unwrap();
        index.restore_after_failed_purge(&note).unwrap();

        let title = index.query("visible vault", 10).unwrap();
        assert_eq!(title.len(), 1);
        assert_eq!(title[0].match_kind, MatchKind::Title);
        assert_eq!(title[0].title, "Visible Vault");
        let tag = index.query("worktag", 10).unwrap();
        assert_eq!(tag.len(), 1);
        assert_eq!(tag[0].match_kind, MatchKind::Tag);
        assert!(index.query("body-secret-oracle", 10).unwrap().is_empty());
        assert!(!tree_contains(index.search_root(), b"body-secret-oracle"));

        let protected_version = open_versioned(&note).unwrap().1;
        let edited = rewrite_metadata_versioned(
            &note,
            &protected_version,
            &MetadataPatch {
                tags: Some(vec!["ChangedTag".to_owned()]),
                ..MetadataPatch::default()
            },
        )
        .unwrap();
        index.reconcile().unwrap();
        assert!(index.query("worktag", 10).unwrap().is_empty());
        assert_eq!(index.query("changedtag", 10).unwrap().len(), 1);
        assert!(index.query("body-secret-oracle", 10).unwrap().is_empty());

        rewrite_metadata_versioned(
            &note,
            &edited.version,
            &MetadataPatch {
                deleted: Some(true),
                ..MetadataPatch::default()
            },
        )
        .unwrap();
        index.reconcile().unwrap();
        assert!(index.query("visible vault", 10).unwrap().is_empty());
        assert!(index.query("changedtag", 10).unwrap().is_empty());
    }

    #[test]
    fn reconcile_updates_save_rename_tag_delete_and_external_changes() {
        let workspace = TestWorkspace::new();
        let alpha = workspace.note("Alpha.md", "Alpha", &["Old"], "firstmarker");
        let mut index = SearchIndex::open_or_rebuild(&workspace.root).unwrap();
        fs::write(
            &alpha,
            "---\ntitle: 'Alpha'\ntags: ['New']\n---\nsecondmarker plus bytes",
        )
        .unwrap();
        let report = index.reconcile().unwrap();
        assert_eq!(report.added_or_updated, 1);
        assert!(index.query("firstmarker", 10).unwrap().is_empty());
        assert_eq!(index.query("secondmarker", 10).unwrap().len(), 1);
        assert_eq!(
            index.query("new", 10).unwrap()[0].match_kind,
            MatchKind::Tag
        );

        let renamed = workspace.root.join("notes/Renamed.md");
        fs::rename(&alpha, &renamed).unwrap();
        let report = index.reconcile().unwrap();
        assert_eq!(report.added_or_updated, 1);
        assert_eq!(report.removed, 1);
        assert_eq!(
            index.query("secondmarker", 10).unwrap()[0].relative_path,
            "notes/Renamed.md"
        );

        fs::write(
            &renamed,
            "---\ntitle: 'Alpha'\ntags: ['New']\ndeleted: true\n---\nsecondmarker plus bytes",
        )
        .unwrap();
        assert_eq!(index.reconcile().unwrap().removed, 1);
        assert!(index.query("secondmarker", 10).unwrap().is_empty());
        assert!(renamed.exists());

        fs::write(
            &renamed,
            "---\ntitle: 'Alpha'\ntags: ['New']\ndeleted: false\n---\nsecondmarker plus bytes",
        )
        .unwrap();
        assert_eq!(index.reconcile().unwrap().added_or_updated, 1);
        assert_eq!(index.query("secondmarker", 10).unwrap().len(), 1);
    }

    #[test]
    fn reconcile_reuses_unchanged_frontmatter_and_detects_content_edits() {
        let workspace = TestWorkspace::new();
        let alpha = workspace.note("Alpha.md", "Alpha", &["Stable"], "firstmarker");
        workspace.note("Beta.md", "Beta", &[], "othermarker");
        let mut index = SearchIndex::open_or_rebuild(&workspace.root).unwrap();

        assert_eq!(index.reconcile().unwrap(), ReconcileReport::default());
        assert_eq!(index.last_reconcile_scanned_files, 0);

        fs::write(
            &alpha,
            "---\ntitle: 'Alpha'\ntags: ['Changed']\n---\nsecondmarker with a different length",
        )
        .unwrap();
        assert_eq!(
            index.reconcile().unwrap(),
            ReconcileReport {
                added_or_updated: 1,
                removed: 0,
            }
        );
        assert_eq!(index.last_reconcile_scanned_files, 1);
        assert!(index.query("firstmarker", 10).unwrap().is_empty());
        assert_eq!(index.query("secondmarker", 10).unwrap().len(), 1);
        assert_eq!(
            index.query("changed", 10).unwrap()[0].match_kind,
            MatchKind::Tag
        );
    }

    #[test]
    fn missing_corrupt_and_interrupted_generations_recover_without_note_writes() {
        let workspace = TestWorkspace::new();
        let note = workspace.note("Stable.md", "Stable", &[], "oldmarker");
        let index = SearchIndex::open_or_rebuild(&workspace.root).unwrap();
        let old_generation = index.generation_name().to_owned();
        drop(index);

        fs::write(&note, "---\ntitle: 'Stable'\ntags: []\n---\nnewmarker").unwrap();
        let modified_before_recovery = fs::read(&note).unwrap();
        let error = match SearchIndex::build_and_publish(
            &workspace.root,
            &BTreeSet::new(),
            PublishFault::BeforePointer,
        ) {
            Err(error) => error,
            Ok(_) => panic!("fault injection unexpectedly published a generation"),
        };
        assert!(matches!(error, SearchError::Io(_)));
        let mut reopened = SearchIndex::open_current(&workspace.root).unwrap();
        assert_eq!(reopened.generation_name(), old_generation);
        assert_eq!(reopened.query("oldmarker", 10).unwrap().len(), 1);
        reopened.reconcile().unwrap();
        assert_eq!(reopened.query("newmarker", 10).unwrap().len(), 1);

        let manifest = reopened
            .search_root()
            .join(reopened.generation_name())
            .join("stillus.manifest");
        fs::write(manifest, "broken").unwrap();
        drop(reopened);
        let rebuilt = SearchIndex::open_or_rebuild(&workspace.root).unwrap();
        assert_eq!(rebuilt.query("newmarker", 10).unwrap().len(), 1);
        assert_eq!(fs::read(note).unwrap(), modified_before_recovery);
    }

    #[test]
    fn purge_is_idempotent_and_physically_removes_indexed_plaintext() {
        let workspace = TestWorkspace::new();
        let secret = workspace.note("Neutral.md", "Private", &["Vault"], "diskpurgeuniquemarker");
        workspace.note("Other.md", "Other", &[], "publicmarker");
        let mut index = SearchIndex::open_or_rebuild(&workspace.root).unwrap();
        assert_eq!(index.query("diskpurgeuniquemarker", 10).unwrap().len(), 1);
        index.purge(&secret).unwrap();
        index.purge(&secret).unwrap();
        assert!(index.query("diskpurgeuniquemarker", 10).unwrap().is_empty());
        assert_eq!(index.query("publicmarker", 10).unwrap().len(), 1);
        assert!(!tree_contains(
            index.search_root(),
            b"diskpurgeuniquemarker"
        ));
    }

    #[test]
    fn purge_and_restore_cover_independent_markers_and_are_idempotent() {
        let workspace = TestWorkspace::new();
        let secret = workspace.note(
            "filenameuniquemarker.md",
            "titleuniquemarker",
            &["taguniquemarker"],
            "bodyuniquemarker",
        );
        workspace.note("Other.md", "Other", &[], "publicrestoremarker");
        let mut index = SearchIndex::open_or_rebuild(&workspace.root).unwrap();
        let encoded_filename = hex_encode(b"notes/filenameuniquemarker.md");
        assert_eq!(index.query("titleuniquemarker", 10).unwrap().len(), 1);
        assert_eq!(index.query("taguniquemarker", 10).unwrap().len(), 1);
        assert_eq!(index.query("bodyuniquemarker", 10).unwrap().len(), 1);
        assert!(tree_contains(
            index.search_root(),
            encoded_filename.as_bytes()
        ));

        index.purge(&secret).unwrap();
        index.purge(&secret).unwrap();
        for marker in [
            "filenameuniquemarker",
            "titleuniquemarker",
            "taguniquemarker",
            "bodyuniquemarker",
        ] {
            assert!(index.query(marker, 10).unwrap().is_empty());
            assert!(!tree_contains(index.search_root(), marker.as_bytes()));
        }
        assert!(!tree_contains(
            index.search_root(),
            encoded_filename.as_bytes()
        ));
        assert_eq!(index.query("publicrestoremarker", 10).unwrap().len(), 1);

        let purged_generation = index.generation_name().to_owned();
        index.restore_after_failed_purge(&secret).unwrap();
        assert_ne!(index.generation_name(), purged_generation);
        assert_eq!(index.query("titleuniquemarker", 10).unwrap().len(), 1);
        assert_eq!(index.query("taguniquemarker", 10).unwrap().len(), 1);
        assert_eq!(index.query("bodyuniquemarker", 10).unwrap().len(), 1);
        assert!(tree_contains(
            index.search_root(),
            encoded_filename.as_bytes()
        ));

        let restored_generation = index.generation_name().to_owned();
        index.restore_after_failed_purge(&secret).unwrap();
        assert_eq!(index.generation_name(), restored_generation);
        assert_eq!(managed_generation_count(index.search_root()), 1);
    }

    #[test]
    fn failed_restore_keeps_purge_published_and_removes_unpublished_plaintext() {
        let workspace = TestWorkspace::new();
        let secret = workspace.note(
            "Rollback.md",
            "rollbacktitlemarker",
            &["rollbacktagmarker"],
            "rollbackbodymarker",
        );
        let mut index = SearchIndex::open_or_rebuild(&workspace.root).unwrap();
        index.purge(&secret).unwrap();
        let purged_generation = index.generation_name().to_owned();

        let error = index
            .restore_after_failed_purge_with_fault(&secret, PublishFault::BeforePointer)
            .unwrap_err();
        assert!(matches!(error, SearchError::Io(_)));
        assert_eq!(index.generation_name(), purged_generation);
        assert!(index.excluded.contains("notes/Rollback.md"));
        assert!(index.query("rollbacktitlemarker", 10).unwrap().is_empty());
        assert!(index.query("rollbacktagmarker", 10).unwrap().is_empty());
        assert!(index.query("rollbackbodymarker", 10).unwrap().is_empty());
        assert!(!tree_contains(index.search_root(), b"rollbackbodymarker"));
        assert_eq!(managed_generation_count(index.search_root()), 1);
        assert_eq!(
            read_current_pointer(index.search_root()).unwrap(),
            purged_generation
        );
    }

    #[test]
    fn post_publish_purge_error_can_be_idempotently_restored() {
        let workspace = TestWorkspace::new();
        let secret = workspace.note(
            "Post Publish.md",
            "postpublishtitlemarker",
            &["postpublishtagmarker"],
            "postpublishbodymarker",
        );
        let mut index = SearchIndex::open_or_rebuild(&workspace.root).unwrap();
        assert_eq!(index.query("postpublishbodymarker", 10).unwrap().len(), 1);

        let error = index
            .purge_with_fault(&secret, PurgeFault::AfterPublish)
            .unwrap_err();
        assert!(matches!(error, SearchError::Io(_)));
        assert!(index.excluded.contains("notes/Post Publish.md"));
        assert!(index.query("postpublishbodymarker", 10).unwrap().is_empty());

        index.restore_after_failed_purge(&secret).unwrap();
        assert!(!index.excluded.contains("notes/Post Publish.md"));
        assert_eq!(index.query("postpublishtitlemarker", 10).unwrap().len(), 1);
        assert_eq!(index.query("postpublishtagmarker", 10).unwrap().len(), 1);
        assert_eq!(index.query("postpublishbodymarker", 10).unwrap().len(), 1);
    }

    #[test]
    fn purge_reports_failure_until_removed_plaintext_generations_are_durable() {
        let workspace = TestWorkspace::new();
        let secret = workspace.note(
            "Durable.md",
            "durabletitlemarker",
            &["durabletagmarker"],
            "durablebodymarker",
        );
        workspace.note("Public.md", "Public", &[], "publicdurablemarker");
        let mut index = SearchIndex::open_or_rebuild(&workspace.root).unwrap();
        let plaintext_generation = index.generation_name().to_owned();
        let encoded_path = hex_encode(b"notes/Durable.md");
        assert!(tree_contains(
            &index.search_root().join(&plaintext_generation),
            encoded_path.as_bytes()
        ));

        let error = index
            .purge_with_fault(&secret, PurgeFault::BeforeCleanupSync)
            .unwrap_err();
        assert!(matches!(error, SearchError::Io(_)));
        assert_ne!(index.generation_name(), plaintext_generation);
        assert!(!index.search_root().join(plaintext_generation).exists());
        assert!(index.query("durablebodymarker", 10).unwrap().is_empty());
        assert_eq!(index.query("publicdurablemarker", 10).unwrap().len(), 1);

        index.purge(&secret).unwrap();
        assert_eq!(managed_generation_count(index.search_root()), 1);
        assert!(!tree_contains(index.search_root(), encoded_path.as_bytes()));
        assert!(!tree_contains(index.search_root(), b"durabletitlemarker"));
        assert!(!tree_contains(index.search_root(), b"durabletagmarker"));
        assert!(!tree_contains(index.search_root(), b"durablebodymarker"));
    }

    #[test]
    fn snippets_and_result_sets_remain_bounded() {
        let workspace = TestWorkspace::new();
        for index in 0..80 {
            workspace.note(
                &format!("Note {index:03}.md"),
                &format!("Note {index:03}"),
                &[],
                &format!(
                    "{} boundedmarker {}",
                    "before ".repeat(80),
                    "after ".repeat(80)
                ),
            );
        }
        let index = SearchIndex::open_or_rebuild(&workspace.root).unwrap();
        let results = index.query("boundedmarker", usize::MAX).unwrap();
        assert_eq!(results.len(), MAX_RESULTS);
        assert!(
            results
                .iter()
                .all(|result| result.snippet.chars().count() <= MAX_SNIPPET_CHARS + 2)
        );
    }

    #[test]
    fn body_search_spans_bounded_chunk_boundaries() {
        let workspace = TestWorkspace::new();
        let body = format!(
            "{}crossboundaryneedle",
            "x".repeat(BODY_READ_BYTES.saturating_sub(5))
        );
        workspace.note("Boundary.md", "Boundary", &[], &body);
        let index = SearchIndex::open_or_rebuild(&workspace.root).unwrap();
        let results = index.query("crossboundaryneedle", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].match_kind, MatchKind::Body);
        assert!(results[0].snippet.contains("crossboundaryneedle"));
    }

    #[test]
    fn invalid_utf8_after_indexed_chunks_removes_every_partial_document() {
        let workspace = TestWorkspace::new();
        let path = workspace.root.join("notes/Invalid Body.md");
        let mut bytes = b"---\ntitle: 'Invalid Body'\ntags: []\n---\n".to_vec();
        bytes.extend_from_slice(b"partialutf8marker ");
        bytes.extend(std::iter::repeat_n(b'x', BODY_READ_BYTES));
        bytes.push(0xff);
        fs::write(&path, bytes).unwrap();

        let index = SearchIndex::open_or_rebuild(&workspace.root).unwrap();
        assert!(index.query("partialutf8marker", 10).unwrap().is_empty());
        assert_eq!(index.query("invalid body", 10).unwrap().len(), 1);
    }

    #[test]
    fn malformed_front_matter_remains_searchable_by_fallback_title_and_body() {
        let workspace = TestWorkspace::new();
        fs::write(
            workspace.root.join("notes/Malformed Metadata.md"),
            "---\ntitle: [\n---\nmalformedbodymarker\n",
        )
        .unwrap();

        let index = SearchIndex::open_or_rebuild(&workspace.root).unwrap();
        let title_results = index.query("malformed metadata", 10).unwrap();
        assert_eq!(title_results.len(), 1);
        assert_eq!(title_results[0].match_kind, MatchKind::Title);
        let body_results = index.query("malformedbodymarker", 10).unwrap();
        assert_eq!(body_results.len(), 1);
        assert_eq!(body_results[0].match_kind, MatchKind::Body);
    }

    #[test]
    fn snippet_offsets_map_nfkc_expansions_back_to_source_characters() {
        let workspace = TestWorkspace::new();
        let body = format!("{} nfkcneedle after", "ﬃ".repeat(80));
        workspace.note("Nfkc.md", "Nfkc", &[], &body);

        let index = SearchIndex::open_or_rebuild(&workspace.root).unwrap();
        let results = index.query("nfkcneedle", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].snippet.contains("nfkcneedle"));
        assert!(results[0].snippet.starts_with('…'));
    }

    #[cfg(unix)]
    #[test]
    fn managed_search_symlink_is_rejected_without_external_writes() {
        use std::os::unix::fs::symlink;

        let workspace = TestWorkspace::new();
        workspace.note("Safe.md", "Safe", &[], "safe body");
        let external = workspace.root.join("external-search-target");
        fs::create_dir(&external).unwrap();
        let marker = external.join("marker");
        fs::write(&marker, b"unchanged").unwrap();
        fs::create_dir(workspace.root.join(".stillus")).unwrap();
        symlink(&external, workspace.root.join(".stillus/search")).unwrap();

        let error = match SearchIndex::open_or_rebuild(&workspace.root) {
            Err(error) => error,
            Ok(_) => panic!("symlinked search root was accepted"),
        };
        assert!(matches!(error, SearchError::InvalidPath(_)));
        assert_eq!(fs::read(marker).unwrap(), b"unchanged");
        assert_eq!(fs::read_dir(external).unwrap().count(), 1);
    }

    fn tree_contains(root: &Path, needle: &[u8]) -> bool {
        let mut stack = vec![root.to_path_buf()];
        while let Some(path) = stack.pop() {
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                if let Ok(entries) = fs::read_dir(path) {
                    stack.extend(entries.flatten().map(|entry| entry.path()));
                }
            } else if fs::read(path)
                .ok()
                .is_some_and(|bytes| bytes.windows(needle.len()).any(|window| window == needle))
            {
                return true;
            }
        }
        false
    }

    fn managed_generation_count(search_root: &Path) -> usize {
        fs::read_dir(search_root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.file_type().is_ok_and(|file_type| file_type.is_dir())
                    && entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("generation-")
            })
            .count()
    }
}
