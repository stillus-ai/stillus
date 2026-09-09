// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Platform-independent workspace, document-session and viewport orchestration.

mod addressed;
mod delete_diagnostics;
mod document_title;
pub use addressed::{
    AddressedEdit, MAX_ACTION_TEXT, NoteEdit, NoteEditJob, NoteMetadataEdit, NoteRead,
    NoteRestoreJob, NoteVersion,
};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use stillus_editor::{
    ByteOffset, ByteRange, EditGroup, Editor, EditorError, EditorSnapshot, Selection,
    next_word_boundary_in_text, previous_word_boundary_in_text,
};
pub use stillus_engine::{
    EngineError, EngineId, EngineUiCapabilities, ExternalFileSummary, ItemAvailability, ItemId,
    ToolbarAction,
};
use stillus_engine::{
    EngineRegistry, FileEngine, FileEngineFactory, ItemSummary, LocalSearchDocument,
    LocalSearchMatch, LocalSearchRequest, SearchHit, SearchRequest, TaskScheduler,
};
use stillus_frontmatter::{
    FrontMatterScan, FrontMatterStatus, MetadataPatch, patch_front_matter, scan_reader,
};
use stillus_markdown::{MarkdownEngineFactory, markdown_engine_id};
use stillus_recovery::{RecoveryError, RecoveryKey, RecoveryRecord, RecoveryStore};
use stillus_rss::RssEngineFactory;
pub use stillus_rss::rss_engine_id;
pub use stillus_rss::{
    RssDecision, RssEngine, RssEntry, RssFeedCache, RssFilterError, RssFilterMode, RssPreferences,
    RssReadState, RssRefreshRequest, RssRefreshResult, RssSchedule, RssSubscription,
    RssSubscriptionSummary, execute_refresh as execute_rss_refresh,
    open_original as open_rss_original,
};
use stillus_secure::{MasterPassword, SecureError, decrypt_body};
use stillus_security::{SecurityError, SecurityStore, VaultId, WorkspaceSecurityState};
use stillus_storage::{
    EMPTY_NOTE_TITLE, FileVersion, IntegrityFailure, NoteOperationError, NoteScanResult,
    PasswordChangeCommit, PasswordChangeError, PasswordChangePhase, PasswordChangeTarget,
    ProtectedBodyRewrite, SaveCommit, SaveError, SecurityRotationTargets, VerifiedSave,
    cleanup_stale_secure_temps, create_note as create_note_file, disable_body_protection,
    initialize_workspace as initialize_workspace_layout, load_pending_integrity_failure,
    open_versioned, project_body_title, protect_note_body, recover_password_change,
    rename_note as rename_note_file, repair_workspace, restore_secure_backup,
    rewrite_external_file_versioned, rewrite_metadata_versioned, rewrite_note_with_title,
    rewrite_protected_body_with_title, rewrite_protected_metadata_versioned,
    rotate_workspace_security, scan_note, scan_workspace, validate_note_title, validate_tag,
};

pub const MAX_VIEWPORT_LINES: usize = 256;
pub const MAX_VIEWPORT_BYTES: usize = 256 * 1024;
pub const DEFAULT_VIEWPORT_LINES: usize = 48;
pub const DEFAULT_OVERSCAN_LINES: usize = 8;
pub const AUTOSAVE_DEBOUNCE_MS: u64 = 750;
pub const RECOVERY_DEBOUNCE_MS: u64 = 200;
pub const PROTECTED_RECOVERY_DEBOUNCE_MS: u64 = 2_000;
pub const UNDO_GROUP_TIMEOUT_MS: u64 = 750;
/// Reserved canonical `order` key for the Favorites sidebar group.
pub const FAVORITED_ORDER_KEY: &str = "__favorited";

pub fn initialize_workspace(root: impl AsRef<Path>) -> Result<(), CoreError> {
    initialize_workspace_layout(root)
        .map_err(|error| CoreError::Workspace(format!("workspace initialization failed: {error}")))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NoteAvailability {
    Ready,
    InvalidMetadata(String),
    IoError(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoteProtection {
    Plain,
    Protected,
}

impl NoteAvailability {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoteSummary {
    pub path: PathBuf,
    pub title: String,
    pub tags: Vec<String>,
    pub pinned: bool,
    pub favorited: bool,
    pub deleted: bool,
    pub created: Option<String>,
    pub modified: Option<String>,
    pub order: BTreeMap<String, u32>,
    pub recovery_available: bool,
    pub availability: NoteAvailability,
    pub protection: NoteProtection,
    body_offset: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CategorySummary {
    pub name: String,
    pub note_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DocumentTarget {
    WorkspaceNote(usize),
    ExternalFile {
        engine_id: EngineId,
        item_id: ItemId,
    },
}

impl From<usize> for DocumentTarget {
    fn from(value: usize) -> Self {
        Self::WorkspaceNote(value)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum CoreError {
    Engine(EngineError),
    Workspace(String),
    NoteUnavailable(String),
    UnsavedChanges,
    Save(SaveError),
    Operation(NoteOperationError),
    Recovery(RecoveryError),
    Secure(SecureError),
    Security(SecurityError),
    PasswordChange(PasswordChangeError),
    MasterPasswordRequired,
    Clock(String),
    Editor(EditorError),
}

impl fmt::Display for CoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Engine(error) => write!(formatter, "engine error: {error}"),
            Self::Workspace(message) => write!(formatter, "workspace error: {message}"),
            Self::NoteUnavailable(message) => write!(formatter, "note unavailable: {message}"),
            Self::UnsavedChanges => {
                formatter.write_str("current note has unsaved changes or an active save")
            }
            Self::Save(error) => write!(formatter, "save error: {error}"),
            Self::Operation(error) => write!(formatter, "operation error: {error}"),
            Self::Recovery(error) => write!(formatter, "recovery error: {error}"),
            Self::Secure(_) => formatter.write_str("protected note could not be opened"),
            Self::Security(error) => write!(formatter, "{error}"),
            Self::PasswordChange(error) => write!(formatter, "{error}"),
            Self::MasterPasswordRequired => formatter.write_str("master password is required"),
            Self::Clock(message) => write!(formatter, "clock error: {message}"),
            Self::Editor(error) => write!(formatter, "editor error: {error}"),
        }
    }
}

impl std::error::Error for CoreError {}

impl CoreError {
    pub fn is_master_password_authentication_failure(&self) -> bool {
        matches!(
            self,
            Self::Secure(_)
                | Self::Security(SecurityError::AuthenticationFailed)
                | Self::MasterPasswordRequired
        )
    }
}

impl From<EditorError> for CoreError {
    fn from(error: EditorError) -> Self {
        Self::Editor(error)
    }
}

impl From<SaveError> for CoreError {
    fn from(error: SaveError) -> Self {
        Self::Save(error)
    }
}

impl From<NoteOperationError> for CoreError {
    fn from(error: NoteOperationError) -> Self {
        Self::Operation(error)
    }
}

impl From<RecoveryError> for CoreError {
    fn from(error: RecoveryError) -> Self {
        Self::Recovery(error)
    }
}

impl From<SecureError> for CoreError {
    fn from(error: SecureError) -> Self {
        Self::Secure(error)
    }
}

impl From<SecurityError> for CoreError {
    fn from(error: SecurityError) -> Self {
        Self::Security(error)
    }
}

impl From<PasswordChangeError> for CoreError {
    fn from(error: PasswordChangeError) -> Self {
        Self::PasswordChange(error)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CatalogOrderItem {
    Note(PathBuf),
    Engine(EngineId, ItemId),
}

#[derive(Clone, Copy)]
enum NoteOrderSpec<'a> {
    Clear,
    Paths(&'a [PathBuf]),
    Ranks(&'a BTreeMap<PathBuf, u32>),
}

pub struct WorkspaceSession {
    root: PathBuf,
    engine_registry: EngineRegistry,
    engines: Vec<Box<dyn FileEngine>>,
    rss_engine: RssEngine,
    task_scheduler: TaskScheduler,
    notes: Vec<NoteSummary>,
    external_files: Vec<ExternalFileSummary>,
    categories: Vec<CategorySummary>,
    selected_note: Option<usize>,
    selected_external: Option<(EngineId, ItemId)>,
    selected_engine: Option<(EngineId, ItemId)>,
    catalog_items: Vec<ItemSummary>,
    document: Option<DocumentSession>,
    recovery_store: RecoveryStore,
    recovery_diagnostics: Vec<String>,
    security_store: SecurityStore,
    security_state: WorkspaceSecurityState,
    vault_id: Option<VaultId>,
    master_password: Option<MasterPassword>,
    secure_operation_generation: u64,
    pending_secure_operation: Option<PendingSecureOperation>,
    pending_integrity: Option<PendingIntegrity>,
    password_change_recovery_blocked: bool,
}

impl WorkspaceSession {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, CoreError> {
        let root = root.as_ref().to_path_buf();
        // Validate before recovery/repair can create any coordination marker.
        for directory in [&root, &root.join("notes")] {
            let metadata = std::fs::symlink_metadata(directory)
                .map_err(|error| CoreError::Workspace(error.to_string()))?;
            if !metadata.file_type().is_dir() {
                return Err(CoreError::Workspace(format!(
                    "workspace requires a real directory: {}",
                    directory.display()
                )));
            }
        }
        recover_password_change(&root)?;
        let cleanup_diagnostic = cleanup_stale_secure_temps(&root)
            .err()
            .map(|error| format!("secure temp cleanup: {error}"));
        repair_workspace(&root).map_err(|error| CoreError::Workspace(error.to_string()))?;
        let scan =
            scan_workspace(&root).map_err(|error| CoreError::Workspace(error.to_string()))?;
        let mut notes = scan
            .notes
            .into_iter()
            .map(|note| note_summary(note.path, note.result))
            .collect::<Vec<_>>();
        let recovery_store = RecoveryStore::new(&root);
        let mut recovery_scan = recovery_store.scan();
        let protected_recovery_scan = recovery_store.scan_protected();
        recovery_scan
            .diagnostics
            .extend(protected_recovery_scan.diagnostics);
        if let Some(diagnostic) = cleanup_diagnostic {
            recovery_scan.diagnostics.push(diagnostic);
        }
        for note in &mut notes {
            if let Ok(key) = recovery_store.key_for_note(&note.path) {
                note.recovery_available = if note.protection == NoteProtection::Protected {
                    recovery_store.protected_exists(&key).unwrap_or(false)
                } else {
                    recovery_scan.records.iter().any(|record| record.key == key)
                };
            }
        }
        sort_notes(&mut notes);
        let legacy_protected_data = notes
            .iter()
            .any(|note| note.protection == NoteProtection::Protected)
            || !protected_recovery_scan.records.is_empty();
        let security_store = SecurityStore::new(&root);
        let security_catalog = security_store.inspect(legacy_protected_data)?;
        let mut engine_registry = EngineRegistry::default();
        let markdown_factory: Arc<dyn FileEngineFactory> = Arc::new(MarkdownEngineFactory);
        let rss_factory: Arc<dyn FileEngineFactory> = Arc::new(RssEngineFactory);
        engine_registry
            .register(markdown_factory.clone())
            .map_err(|error| CoreError::Workspace(error.to_string()))?;
        engine_registry
            .register(rss_factory)
            .map_err(|error| CoreError::Workspace(error.to_string()))?;
        let rss_engine =
            RssEngine::open(&root).map_err(|error| CoreError::Workspace(error.to_string()))?;
        engine_registry
            .register(Arc::new(stillus_chat::ChatEngineFactory))
            .map_err(|e| CoreError::Workspace(e.to_string()))?;
        let chat_catalog = match stillus_chat::ChatStore::open(&root).and_then(|store| store.list())
        {
            Ok(items) => items,
            Err(error) => {
                recovery_scan
                    .diagnostics
                    .push(format!("chat catalog: {error:?}"));
                Vec::new()
            }
        };
        let catalog_items = chat_catalog.into_iter().map(|s| s.item).collect::<Vec<_>>();
        let mut category_counts = BTreeMap::<String, usize>::new();
        for note in notes
            .iter()
            .filter(|note| note.availability.is_ready() && !note.deleted)
        {
            for category in &note.tags {
                *category_counts.entry(category.clone()).or_default() += 1;
            }
        }
        for item in rss_engine
            .subscriptions()
            .iter()
            .filter(|item| !item.deleted)
        {
            for category in &item.categories {
                *category_counts.entry(category.clone()).or_default() += 1;
            }
        }
        for item in catalog_items.iter().filter(|i| !i.metadata.deleted) {
            for category in &item.metadata.categories {
                *category_counts.entry(category.clone()).or_default() += 1;
            }
        }
        let categories = category_counts
            .into_iter()
            .map(|(name, note_count)| CategorySummary { name, note_count })
            .collect();
        let engines = vec![
            markdown_factory
                .open(&root)
                .map_err(|error| CoreError::Workspace(error.to_string()))?,
        ];
        let pending_integrity = match load_pending_integrity_failure(&root) {
            Ok(failure) => failure.map(|failure| PendingIntegrity {
                failure: Box::new(failure),
                retry: None,
            }),
            Err(error) => {
                recovery_scan
                    .diagnostics
                    .push(format!("secure integrity journal: {error}"));
                None
            }
        };
        Ok(Self {
            root,
            engine_registry,
            engines,
            rss_engine,
            task_scheduler: TaskScheduler::default(),
            notes,
            external_files: Vec::new(),
            categories,
            selected_note: None,
            selected_external: None,
            selected_engine: None,
            catalog_items,
            document: None,
            recovery_store,
            recovery_diagnostics: recovery_scan.diagnostics,
            security_store,
            security_state: security_catalog.state,
            vault_id: None,
            master_password: None,
            secure_operation_generation: 0,
            pending_secure_operation: None,
            pending_integrity,
            password_change_recovery_blocked: false,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn notes(&self) -> &[NoteSummary] {
        &self.notes
    }

    pub fn categories(&self) -> &[CategorySummary] {
        &self.categories
    }

    pub fn selected_note(&self) -> Option<usize> {
        self.selected_note
    }

    pub fn external_files(&self) -> &[ExternalFileSummary] {
        &self.external_files
    }

    pub fn rss_subscriptions(&self) -> Vec<RssSubscriptionSummary> {
        self.rss_engine.summaries()
    }

    pub fn rss_background_snapshot(&self) -> RssEngine {
        self.rss_engine.clone()
    }

    pub fn accept_rss_snapshot(&mut self, engine: RssEngine) {
        if engine.config_revision() >= self.rss_engine.config_revision() {
            self.rss_engine = engine;
        }
    }

    pub fn rss_preferences(&self, id: &ItemId) -> Result<RssPreferences, CoreError> {
        self.rss_engine
            .preferences(id)
            .map_err(|e| CoreError::Workspace(e.to_string()))
    }

    pub fn rss_schedule(&self, id: &ItemId) -> Result<RssSchedule, CoreError> {
        self.rss_feed(id).map(|(_, state)| state.schedule)
    }

    /// Caller supplies Unix milliseconds; background callers perform the write
    /// off the UI thread, using the same operation as a manual visit.
    pub fn visit_rss(&self, id: &ItemId, now: u64) -> Result<(), CoreError> {
        self.rss_engine
            .update_state(id, |state| {
                state.schedule.visit(now);
                Ok(())
            })
            .map_err(|e| CoreError::Workspace(e.to_string()))
    }

    pub fn save_rss_preferences(
        &mut self,
        id: &ItemId,
        expected: u64,
        preferences: RssPreferences,
    ) -> Result<(), CoreError> {
        self.rss_engine
            .save_preferences(id, expected, preferences)
            .map_err(|e| CoreError::Workspace(e.to_string()))
    }

    pub fn selected_rss(&self) -> Option<&ItemId> {
        self.selected_engine
            .as_ref()
            .filter(|(engine, _)| engine == &rss_engine_id())
            .map(|(_, id)| id)
    }

    /// Toolbar controls an engine declares for its items. The UI builds every
    /// engine surface from this list with one shared set of controls, so a new
    /// engine inherits them without touching the view code.
    pub fn engine_toolbar_actions(&self, engine_id: &EngineId) -> Vec<ToolbarAction> {
        self.engine_registry
            .get(engine_id)
            .map(|factory| factory.ui_capabilities().toolbar_actions)
            .unwrap_or_default()
    }

    pub fn rss_toolbar_actions(&self) -> Vec<ToolbarAction> {
        self.engine_toolbar_actions(&rss_engine_id())
    }

    pub fn rss_feed(&self, item_id: &ItemId) -> Result<(RssFeedCache, RssReadState), CoreError> {
        self.rss_engine
            .feed(item_id)
            .map_err(|error| CoreError::Workspace(error.to_string()))
    }

    pub fn open_rss(&mut self, item_id: &ItemId) -> Result<(), CoreError> {
        self.open_engine_item(&rss_engine_id(), item_id)
    }
    pub fn selected_engine_item(&self) -> Option<&(EngineId, ItemId)> {
        self.selected_engine.as_ref()
    }
    pub fn non_document_items(&self) -> Vec<ItemSummary> {
        let mut items = self.rss_engine.items().unwrap_or_default();
        items.extend(self.catalog_items.clone());
        items
    }
    pub fn accept_engine_items(&mut self, engine: &EngineId, items: Vec<ItemSummary>) {
        self.catalog_items.retain(|i| &i.engine_id != engine);
        self.catalog_items.extend(items);
        self.refresh_catalog_categories();
    }
    pub fn open_engine_item(
        &mut self,
        engine: &EngineId,
        item_id: &ItemId,
    ) -> Result<(), CoreError> {
        self.ensure_no_secure_operation()?;
        if self
            .document
            .as_ref()
            .is_some_and(DocumentSession::has_unsaved_work)
        {
            return Err(CoreError::UnsavedChanges);
        }
        if !self
            .non_document_items()
            .iter()
            .any(|i| &i.engine_id == engine && &i.item_id == item_id)
        {
            return Err(CoreError::NoteUnavailable("unknown engine item".into()));
        }
        self.document = None;
        self.selected_note = None;
        self.selected_external = None;
        self.selected_engine = Some((engine.clone(), item_id.clone()));
        Ok(())
    }
    pub fn update_engine_metadata(
        &mut self,
        engine: &EngineId,
        id: &ItemId,
        version: &str,
        patch: stillus_engine::CommonMetadataPatch,
    ) -> Result<(), CoreError> {
        self.ensure_workspace_action_ready()?;
        if engine == &rss_engine_id() {
            self.rss_engine
                .update_metadata(id, version, patch)
                .map_err(|e| CoreError::Workspace(e.to_string()))?;
        } else {
            let factory = self
                .engine_registry
                .get(engine)
                .ok_or_else(|| CoreError::NoteUnavailable("engine unavailable".into()))?;
            let mut instance = factory
                .open(&self.root)
                .map_err(|e| CoreError::Workspace(e.to_string()))?;
            instance
                .update_metadata(id, version, patch)
                .map_err(|e| CoreError::Workspace(e.to_string()))?;
            let items = instance
                .items()
                .map_err(|e| CoreError::Workspace(e.to_string()))?;
            self.accept_engine_items(engine, items);
        }
        self.refresh_catalog_categories();
        Ok(())
    }

    pub fn create_rss(
        &mut self,
        url: &str,
        categories: Vec<String>,
        favorited: bool,
        timestamp: &str,
    ) -> Result<ItemId, CoreError> {
        self.ensure_workspace_action_ready()?;
        let categories = normalize_rss_categories(&categories)?;
        let id = self
            .rss_engine
            .create_subscription(url, categories, favorited, timestamp)
            .map_err(|error| CoreError::Workspace(error.to_string()))?;
        self.refresh_catalog_categories();
        self.open_rss(&id)?;
        Ok(id)
    }

    pub fn rename_rss(&mut self, title: &str, timestamp: &str) -> Result<(), CoreError> {
        let id = self
            .selected_rss()
            .cloned()
            .ok_or_else(|| CoreError::NoteUnavailable("no RSS subscription is selected".into()))?;
        let version = self
            .rss_engine
            .subscriptions()
            .iter()
            .find(|item| item.id == id)
            .ok_or_else(|| CoreError::NoteUnavailable("RSS subscription is unavailable".into()))?
            .revision;
        apply_rss_metadata(
            &mut self.rss_engine,
            &id,
            version,
            stillus_engine::CommonMetadataPatch {
                title: Some(title.into()),
                ..Default::default()
            },
            timestamp,
        )
    }

    pub fn update_selected_rss_metadata(
        &mut self,
        timestamp: &str,
        update: impl FnOnce(&mut stillus_rss::RssSubscription),
    ) -> Result<(), CoreError> {
        let id = self
            .selected_rss()
            .cloned()
            .ok_or_else(|| CoreError::NoteUnavailable("no RSS subscription is selected".into()))?;
        let mut item = self
            .rss_engine
            .subscriptions()
            .iter()
            .find(|item| item.id == id)
            .ok_or_else(|| CoreError::NoteUnavailable("RSS subscription is unavailable".into()))?
            .clone();
        let version = item.revision;
        update(&mut item);
        apply_rss_metadata(
            &mut self.rss_engine,
            &id,
            version,
            stillus_engine::CommonMetadataPatch {
                title: item.title_override,
                categories: Some(item.categories),
                pinned: Some(item.pinned),
                favorited: Some(item.favorited),
                deleted: Some(item.deleted),
                order: Some(item.order),
            },
            timestamp,
        )?;
        self.refresh_catalog_categories();
        Ok(())
    }

    pub fn set_selected_rss_categories(
        &mut self,
        categories: &[String],
        timestamp: &str,
    ) -> Result<(), CoreError> {
        let normalized = normalize_rss_categories(categories)?;
        self.update_selected_rss_metadata(timestamp, |item| item.categories = normalized)
    }

    pub fn rss_refresh_request(&self, item_id: &ItemId) -> Result<RssRefreshRequest, CoreError> {
        self.rss_engine
            .refresh_request(item_id)
            .map_err(|error| CoreError::Workspace(error.to_string()))
    }

    pub fn finish_rss_refresh(&mut self, result: RssRefreshResult) -> Result<(), CoreError> {
        self.rss_engine
            .apply_refresh(result)
            .map_err(|error| CoreError::Workspace(error.to_string()))
    }

    pub fn mark_rss_read(&mut self, entry_id: &str, timestamp: &str) -> Result<bool, CoreError> {
        let item_id = self.selected_rss().cloned().ok_or_else(|| {
            CoreError::NoteUnavailable("no RSS subscription is selected".to_owned())
        })?;
        self.rss_engine
            .mark_read(&item_id, entry_id, timestamp)
            .map_err(|error| CoreError::Workspace(error.to_string()))
    }

    pub fn selected_target(&self) -> Option<DocumentTarget> {
        if let Some(index) = self.selected_note {
            return Some(DocumentTarget::WorkspaceNote(index));
        }
        self.selected_external
            .as_ref()
            .map(|(engine_id, item_id)| DocumentTarget::ExternalFile {
                engine_id: engine_id.clone(),
                item_id: item_id.clone(),
            })
    }

    pub fn selected_item(&self) -> Option<(EngineId, ItemId)> {
        if let Some(selected) = &self.selected_engine {
            return Some(selected.clone());
        }
        if let Some(selected) = &self.selected_external {
            return Some(selected.clone());
        }
        let note = self.notes.get(self.selected_note?)?;
        let relative = note
            .path
            .strip_prefix(&self.root)
            .ok()?
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        Some((markdown_engine_id(), ItemId::new(relative).ok()?))
    }

    pub fn engine_ids(&self) -> impl Iterator<Item = &EngineId> {
        self.engine_registry.ids()
    }

    pub fn external_file_extensions(&self) -> Vec<String> {
        self.engine_registry
            .external_file_extensions()
            .map(str::to_owned)
            .collect()
    }

    pub fn engine_catalog(&self) -> Result<Vec<ItemSummary>, CoreError> {
        let mut items = Vec::new();
        for engine in &self.engines {
            items.extend(
                engine
                    .items()
                    .map_err(|error| CoreError::Workspace(error.to_string()))?,
            );
        }
        items.extend(
            self.rss_engine
                .items()
                .map_err(|error| CoreError::Workspace(error.to_string()))?,
        );
        items.extend(self.catalog_items.clone());
        Ok(items)
    }

    pub fn attach_external_file(&mut self, path: &Path) -> Result<DocumentTarget, CoreError> {
        let requested = path.to_path_buf();
        if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(CoreError::NoteUnavailable(
                "external file symlinks are not supported".to_owned(),
            ));
        }
        if let Ok(canonical) = requested.canonicalize()
            && let Some(index) = self
                .notes
                .iter()
                .position(|note| note.path.canonicalize().is_ok_and(|path| path == canonical))
        {
            return Ok(DocumentTarget::WorkspaceNote(index));
        }
        let engine_id = self
            .engine_registry
            .external_engine_for_path(path)
            .ok_or_else(|| {
                CoreError::NoteUnavailable(format!(
                    "unsupported external file type: {}",
                    path.display()
                ))
            })?;
        let engine = self
            .engines
            .iter_mut()
            .find(|engine| engine.id() == engine_id)
            .ok_or_else(|| CoreError::Workspace("external engine is unavailable".to_owned()))?;
        let mut summary = engine
            .open_external_file(path)
            .map_err(|error| CoreError::NoteUnavailable(error.to_string()))?;
        let key = self
            .recovery_store
            .key_for_external(summary.engine_id.as_str(), summary.item_id.as_str())?;
        summary.recovery_available = self.recovery_store.open(&key).is_ok();
        if let Some(existing) = self.external_files.iter_mut().find(|existing| {
            existing.engine_id == summary.engine_id && existing.item_id == summary.item_id
        }) {
            *existing = summary.clone();
        } else {
            self.external_files.push(summary.clone());
        }
        Ok(DocumentTarget::ExternalFile {
            engine_id: summary.engine_id,
            item_id: summary.item_id,
        })
    }

    pub fn open_external_file(&mut self, path: &Path) -> Result<DocumentTarget, CoreError> {
        let target = self.attach_external_file(path)?;
        match &target {
            DocumentTarget::WorkspaceNote(index) => self.open_note(*index)?,
            DocumentTarget::ExternalFile { engine_id, item_id } => {
                self.open_external_item(engine_id, item_id)?;
            }
        }
        Ok(target)
    }

    pub fn open_external_item(
        &mut self,
        engine_id: &EngineId,
        item_id: &ItemId,
    ) -> Result<(), CoreError> {
        self.ensure_no_secure_operation()?;
        if self
            .document
            .as_ref()
            .is_some_and(DocumentSession::has_unsaved_work)
        {
            return Err(CoreError::UnsavedChanges);
        }
        let summary = self
            .external_files
            .iter()
            .find(|file| &file.engine_id == engine_id && &file.item_id == item_id)
            .ok_or_else(|| CoreError::NoteUnavailable("unknown external file".to_owned()))?;
        if !matches!(summary.availability, ItemAvailability::Ready) {
            return Err(CoreError::NoteUnavailable(match &summary.availability {
                ItemAvailability::Invalid(message) | ItemAvailability::Unavailable(message) => {
                    message.clone()
                }
                ItemAvailability::NeedsUnlock => "external file is locked".to_owned(),
                ItemAvailability::Ready => unreachable!(),
            }));
        }
        let (file, version) = open_versioned(&summary.path)?;
        let target = DocumentTarget::ExternalFile {
            engine_id: engine_id.clone(),
            item_id: item_id.clone(),
        };
        self.document = Some(DocumentSession::from_versioned_reader(
            target,
            summary.title.clone(),
            file,
            version,
        )?);
        self.selected_note = None;
        self.selected_external = Some((engine_id.clone(), item_id.clone()));
        self.selected_engine = None;
        Ok(())
    }

    pub fn close_external_file(
        &mut self,
        engine_id: &EngineId,
        item_id: &ItemId,
    ) -> Result<bool, CoreError> {
        let selected = self
            .selected_external
            .as_ref()
            .is_some_and(|selected| &selected.0 == engine_id && &selected.1 == item_id);
        if selected
            && self
                .document
                .as_ref()
                .is_some_and(DocumentSession::has_unsaved_work)
        {
            return Err(CoreError::UnsavedChanges);
        }
        let Some(index) = self
            .external_files
            .iter()
            .position(|file| &file.engine_id == engine_id && &file.item_id == item_id)
        else {
            return Ok(false);
        };
        self.external_files.remove(index);
        if let Some(engine) = self
            .engines
            .iter_mut()
            .find(|engine| engine.id() == *engine_id)
        {
            engine
                .close_external_file(item_id)
                .map_err(|error| CoreError::Workspace(error.to_string()))?;
        }
        if selected {
            self.selected_external = None;
            self.document = None;
        }
        Ok(true)
    }

    pub fn search_all_engines(&self, request: &SearchRequest) -> Result<Vec<SearchHit>, CoreError> {
        let mut hits = Vec::new();
        for engine in &self.engines {
            if let Some(provider) = engine.search_provider() {
                hits.extend(
                    provider
                        .search(request)
                        .map_err(|error| CoreError::Workspace(error.to_string()))?,
                );
            }
        }
        hits.sort_by(|left, right| {
            right
                .score_micros
                .cmp(&left.score_micros)
                .then_with(|| left.engine_id.cmp(&right.engine_id))
                .then_with(|| left.item_id.cmp(&right.item_id))
        });
        hits.truncate(request.limit);
        Ok(hits)
    }

    pub fn selected_document_supports_local_search(&self) -> bool {
        let Some((engine_id, _)) = self.selected_item() else {
            return false;
        };
        self.engine_registry
            .get(&engine_id)
            .is_some_and(|factory| factory.capabilities().local_search)
            && self
                .engines
                .iter()
                .find(|engine| engine.id() == engine_id)
                .and_then(|engine| engine.local_search_provider())
                .is_some()
            && self.document.is_some()
    }

    pub fn search_selected_document(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ByteRange>, CoreError> {
        let document = self.document.as_ref().ok_or_else(|| {
            CoreError::NoteUnavailable("no document is open for local search".to_owned())
        })?;
        let (engine_id, item_id) = self.selected_item().ok_or_else(|| {
            CoreError::NoteUnavailable("no engine item is selected for local search".to_owned())
        })?;
        let factory = self
            .engine_registry
            .get(&engine_id)
            .ok_or_else(|| CoreError::Workspace(format!("engine {engine_id} is not registered")))?;
        if !factory.capabilities().local_search {
            return Err(CoreError::Workspace(format!(
                "engine {engine_id} does not support local search"
            )));
        }
        let provider = self
            .engines
            .iter()
            .find(|engine| engine.id() == engine_id)
            .and_then(|engine| engine.local_search_provider())
            .ok_or_else(|| {
                CoreError::Workspace(format!("engine {engine_id} does not provide local search"))
            })?;
        let mut matches = provider
            .search_document(
                &LocalSearchRequest {
                    item_id,
                    query: query.to_owned(),
                    limit,
                },
                document,
            )
            .map_err(|error| CoreError::Workspace(error.to_string()))?;
        matches.truncate(limit);
        matches
            .into_iter()
            .map(|found| {
                let start = ByteOffset::new(found.start_byte);
                let end = ByteOffset::new(found.end_byte);
                let valid = found.start_byte <= found.end_byte
                    && found.end_byte <= document.len_bytes()
                    && document
                        .editor
                        .is_codepoint_boundary(start)
                        .unwrap_or(false)
                    && document.editor.is_codepoint_boundary(end).unwrap_or(false);
                if !valid {
                    return Err(CoreError::Workspace(format!(
                        "engine {engine_id} returned an invalid local search range"
                    )));
                }
                ByteRange::new(start, end).map_err(CoreError::Editor)
            })
            .collect()
    }

    pub fn task_scheduler(&self) -> &TaskScheduler {
        &self.task_scheduler
    }

    pub fn task_scheduler_mut(&mut self) -> &mut TaskScheduler {
        &mut self.task_scheduler
    }

    pub fn recovery_diagnostics(&self) -> &[String] {
        &self.recovery_diagnostics
    }

    pub fn has_master_password(&self) -> bool {
        self.master_password.is_some()
    }

    pub fn master_password_configured(&self) -> bool {
        matches!(
            self.security_state,
            WorkspaceSecurityState::ConfiguredLocked | WorkspaceSecurityState::Unlocked
        )
    }

    pub fn security_unlocked(&self) -> bool {
        self.security_state == WorkspaceSecurityState::Unlocked && self.master_password.is_some()
    }

    pub fn security_state(&self) -> WorkspaceSecurityState {
        self.security_state
    }

    pub fn configure_workspace_security(
        &mut self,
        password: MasterPassword,
    ) -> Result<(), CoreError> {
        self.ensure_no_secure_operation()?;
        let vault_id = self.security_store.configure(&password)?;
        self.vault_id = Some(vault_id);
        self.master_password = Some(password);
        self.security_state = WorkspaceSecurityState::Unlocked;
        Ok(())
    }

    pub fn unlock_workspace_security(&mut self, password: MasterPassword) -> Result<(), CoreError> {
        self.ensure_no_secure_operation()?;
        let vault_id = self.security_store.unlock(&password)?;
        self.vault_id = Some(vault_id);
        self.master_password = Some(password);
        self.security_state = WorkspaceSecurityState::Unlocked;
        Ok(())
    }

    pub fn security_store(&self) -> &SecurityStore {
        &self.security_store
    }

    pub fn referenced_secret_count(&self) -> Result<usize, CoreError> {
        let mut count = 0;
        for engine in &self.engines {
            count += engine
                .referenced_secrets()
                .map_err(|error| CoreError::Workspace(error.to_string()))?
                .len();
        }
        Ok(count)
    }

    pub fn has_protected_notes(&self) -> bool {
        self.notes
            .iter()
            .any(|note| note.protection == NoteProtection::Protected)
    }

    pub fn protected_note_count(&self) -> usize {
        self.notes
            .iter()
            .filter(|note| note.protection == NoteProtection::Protected)
            .count()
    }

    pub fn protected_recovery_count(&self) -> Result<usize, CoreError> {
        Ok(self.recovery_store.protected_artifact_paths()?.len())
    }

    pub fn begin_change_master_password(
        &mut self,
        current: MasterPassword,
        new: MasterPassword,
    ) -> Result<SecureJob, CoreError> {
        self.ensure_no_secure_operation()?;
        if current.is_empty() || new.is_empty() {
            return Err(CoreError::NoteUnavailable(
                "current and new master passwords are required".to_owned(),
            ));
        }
        if current.same_secret(&new) {
            return Err(CoreError::NoteUnavailable(
                "new master password must differ from the current password".to_owned(),
            ));
        }
        if self
            .document
            .as_ref()
            .is_some_and(DocumentSession::operation_blocked)
        {
            return Err(CoreError::UnsavedChanges);
        }
        if !self.master_password_configured() {
            return Err(CoreError::NoteUnavailable(
                "workspace master password is not configured".to_owned(),
            ));
        }
        let vault_id = self.security_store.unlock(&current)?;
        let verifier_path = self.security_store.verifier_path();
        let verifier = PasswordChangeTarget {
            version: open_versioned(&verifier_path)?.1,
            path: verifier_path,
        };
        let mut referenced_secrets = Vec::new();
        for engine in &self.engines {
            referenced_secrets.extend(
                engine
                    .referenced_secrets()
                    .map_err(|error| CoreError::Workspace(error.to_string()))?,
            );
        }
        let secret_paths = self
            .security_store
            .referenced_secret_paths(&referenced_secrets, &current)?;
        let mut secrets = Vec::with_capacity(secret_paths.len());
        for path in secret_paths {
            secrets.push(PasswordChangeTarget {
                version: open_versioned(&path)?.1,
                path,
            });
        }
        let mut notes = Vec::new();
        for note in self
            .notes
            .iter()
            .filter(|note| note.protection == NoteProtection::Protected)
        {
            if !note.availability.is_ready() {
                return Err(CoreError::NoteUnavailable(format!(
                    "protected note {} is unavailable",
                    note.path.display()
                )));
            }
            let version = open_versioned(&note.path)?.1;
            notes.push(PasswordChangeTarget {
                path: note.path.clone(),
                version,
            });
        }
        let recovery_paths = self.recovery_store.protected_artifact_paths()?;
        let secret_catalog = self.security_store.inspect(true)?.secrets;
        for index in 0..self.engines.len() {
            if let Err(error) = self.engines[index].quiesce() {
                for engine in &mut self.engines[..index] {
                    engine.resume();
                }
                return Err(CoreError::Workspace(error.to_string()));
            }
        }
        self.task_scheduler.cancel_all();
        self.vault_id = Some(vault_id);
        self.security_state = WorkspaceSecurityState::Unlocked;
        Ok(self.start_secure_job(
            SecureOperationKind::ChangeMasterPassword,
            SecureJobTask::ChangeMasterPassword(Box::new(ChangeMasterPasswordJob {
                workspace: self.root.clone(),
                verifier,
                secrets,
                secret_catalog,
                notes,
                recovery_paths,
                current,
                new,
            })),
        ))
    }

    pub fn secure_operation_pending(&self) -> bool {
        self.pending_secure_operation.is_some()
    }

    pub fn integrity_failure(&self) -> Option<&IntegrityFailure> {
        self.pending_integrity
            .as_ref()
            .map(|pending| pending.failure.as_ref())
    }

    pub fn begin_integrity_resolution(
        &mut self,
        resolution: IntegrityResolution,
    ) -> Result<SecureJob, CoreError> {
        if self.pending_secure_operation.is_some() {
            return Err(CoreError::UnsavedChanges);
        }
        let pending = self.pending_integrity.as_ref().ok_or_else(|| {
            CoreError::NoteUnavailable("there is no integrity incident to resolve".to_owned())
        })?;
        if resolution == IntegrityResolution::Retry && pending.retry.is_none() {
            return Err(CoreError::NoteUnavailable(
                "retry is unavailable after restart; restore the verified backup".to_owned(),
            ));
        }
        Ok(self.start_secure_job(
            SecureOperationKind::Integrity,
            SecureJobTask::ResolveIntegrity(Box::new(IntegrityResolutionJob {
                workspace: self.root.clone(),
                failure: pending.failure.as_ref().clone(),
                retry: pending.retry.clone(),
                resolution,
            })),
        ))
    }

    pub fn selected_is_protected(&self) -> bool {
        self.selected_note
            .and_then(|index| self.notes.get(index))
            .is_some_and(|note| note.protection == NoteProtection::Protected)
    }

    pub fn select_protected_note(&mut self, note_index: usize) -> Result<(), CoreError> {
        self.ensure_workspace_action_ready()?;
        let note = self.notes.get(note_index).ok_or_else(|| {
            CoreError::NoteUnavailable(format!("unknown note index {note_index}"))
        })?;
        if note.protection != NoteProtection::Protected || !note.availability.is_ready() {
            return Err(CoreError::NoteUnavailable(
                "selected note is not an available protected note".to_owned(),
            ));
        }
        self.select_protected_note_for_loading(note_index);
        Ok(())
    }

    fn select_protected_note_for_loading(&mut self, note_index: usize) {
        if let Some(previous_index) = self.document.as_ref().and_then(|document| {
            (document.note_index() != note_index && document.is_protected())
                .then_some(document.note_index())
        }) && let Some(previous) = self.notes.get_mut(previous_index)
        {
            redact_protected_projection(previous);
        }
        self.document = None;
        self.selected_note = Some(note_index);
        self.selected_external = None;
        self.selected_engine = None;
        self.refresh_catalog_categories();
    }

    pub fn unlock_note(
        &mut self,
        note_index: usize,
        password: MasterPassword,
    ) -> Result<(), CoreError> {
        let completion = self.begin_unlock_note(note_index, password)?.execute();
        match self.finish_secure_operation(completion)? {
            SecureOutcome::Unlocked => Ok(()),
            _ => Err(CoreError::NoteUnavailable(
                "unlock completed with an unexpected outcome".to_owned(),
            )),
        }
    }

    pub fn begin_unlock_note(
        &mut self,
        note_index: usize,
        password: MasterPassword,
    ) -> Result<SecureJob, CoreError> {
        if self
            .document
            .as_ref()
            .is_some_and(DocumentSession::has_unsaved_work)
        {
            return Err(CoreError::UnsavedChanges);
        }
        self.ensure_no_secure_operation()?;
        let note = self.notes.get(note_index).ok_or_else(|| {
            CoreError::NoteUnavailable(format!("unknown note index {note_index}"))
        })?;
        if note.protection != NoteProtection::Protected || !note.availability.is_ready() {
            return Err(CoreError::NoteUnavailable(
                "selected note is not an available protected note".to_owned(),
            ));
        }
        Ok(self.start_secure_job(
            SecureOperationKind::Unlock,
            SecureJobTask::Load(LoadProtectedJob {
                note_index,
                path: note.path.clone(),
                password,
                purpose: ProtectedLoadPurpose::Unlock {
                    adopt_password: true,
                },
                recovery_cleanup: None,
            }),
        ))
    }

    pub fn begin_open_protected_note(&mut self, note_index: usize) -> Result<SecureJob, CoreError> {
        let password = self
            .master_password
            .as_ref()
            .ok_or(CoreError::MasterPasswordRequired)?
            .clone();
        let mut job = self.begin_unlock_note(note_index, password)?;
        if let SecureJobTask::Load(load) = job.task.as_mut() {
            load.purpose = ProtectedLoadPurpose::Unlock {
                adopt_password: false,
            };
        }
        // The decrypt job can be noticeably slower than a frame. Publish the
        // target selection and redact the old editor buffer before the worker
        // starts so the UI never renders plaintext from the previous note
        // under the newly selected sidebar row.
        self.select_protected_note_for_loading(note_index);
        Ok(job)
    }

    pub fn lock_selected(&mut self) -> Result<(), CoreError> {
        self.ensure_workspace_action_ready()?;
        let verifier_configured = matches!(
            self.security_state,
            WorkspaceSecurityState::ConfiguredLocked | WorkspaceSecurityState::Unlocked
        );
        let note_index = self
            .selected_note
            .ok_or_else(|| CoreError::NoteUnavailable("no note is selected".to_owned()))?;
        let note = self
            .notes
            .get_mut(note_index)
            .ok_or_else(|| CoreError::NoteUnavailable("selected note disappeared".to_owned()))?;
        if note.protection != NoteProtection::Protected {
            return Err(CoreError::NoteUnavailable(
                "selected note is not protected".to_owned(),
            ));
        }
        self.document = None;
        self.master_password = None;
        self.vault_id = None;
        self.security_state = if verifier_configured {
            WorkspaceSecurityState::ConfiguredLocked
        } else {
            WorkspaceSecurityState::LegacyLocked
        };
        redact_protected_projection(note);
        self.refresh_catalog_categories();
        Ok(())
    }

    pub fn protect_selected(
        &mut self,
        password: Option<MasterPassword>,
    ) -> Result<PathBuf, CoreError> {
        let completion = self.begin_protect_selected(password)?.execute();
        match self.finish_secure_operation(completion)? {
            SecureOutcome::Protected(path) => Ok(path),
            _ => Err(CoreError::NoteUnavailable(
                "protect completed with an unexpected outcome".to_owned(),
            )),
        }
    }

    pub fn begin_protect_selected(
        &mut self,
        password: Option<MasterPassword>,
    ) -> Result<SecureJob, CoreError> {
        let (note_index, path, version) = self.selected_operation_target()?;
        if self.notes[note_index].protection == NoteProtection::Protected {
            return Err(CoreError::NoteUnavailable(
                "selected note is already protected".to_owned(),
            ));
        }
        let (password, authentication_path) = match password {
            Some(candidate) => {
                let authentication_path = self
                    .notes
                    .iter()
                    .find(|note| note.protection == NoteProtection::Protected)
                    .map(|protected| protected.path.clone());
                (candidate, authentication_path)
            }
            None => (
                self.master_password
                    .as_ref()
                    .ok_or(CoreError::MasterPasswordRequired)?
                    .clone(),
                None,
            ),
        };
        match self.security_state {
            WorkspaceSecurityState::Unconfigured => {
                self.vault_id = Some(self.security_store.configure(&password)?);
                self.security_state = WorkspaceSecurityState::Unlocked;
                self.master_password = Some(password.clone());
            }
            WorkspaceSecurityState::ConfiguredLocked => {
                self.vault_id = Some(self.security_store.unlock(&password)?);
                self.security_state = WorkspaceSecurityState::Unlocked;
                self.master_password = Some(password.clone());
            }
            WorkspaceSecurityState::Unlocked => {}
            WorkspaceSecurityState::LegacyLocked => {}
            WorkspaceSecurityState::Blocked => {
                return Err(CoreError::Security(SecurityError::Blocked(
                    "workspace security is blocked".to_owned(),
                )));
            }
        }
        let old_key = self.recovery_store.key_for_note(&path)?;
        Ok(self.start_secure_job(
            SecureOperationKind::Protect,
            SecureJobTask::Protect(ProtectJob {
                path,
                version,
                title: self.notes[note_index].title.clone(),
                password,
                authentication_path,
                recovery_store: self.recovery_store.clone(),
                recovery_key: old_key,
            }),
        ))
    }

    pub fn disable_protection_selected(&mut self) -> Result<PathBuf, CoreError> {
        let completion = self.begin_disable_protection_selected()?.execute();
        match self.finish_secure_operation(completion)? {
            SecureOutcome::ProtectionDisabled(path) => Ok(path),
            _ => Err(CoreError::NoteUnavailable(
                "disable protection completed with an unexpected outcome".to_owned(),
            )),
        }
    }

    pub fn begin_disable_protection_selected(&mut self) -> Result<SecureJob, CoreError> {
        let (note_index, path, version) = self.selected_operation_target()?;
        if self.notes[note_index].protection != NoteProtection::Protected {
            return Err(CoreError::NoteUnavailable(
                "selected note is not protected".to_owned(),
            ));
        }
        let password = match self.document.as_ref().map(|document| &document.protection) {
            Some(DocumentProtection::Protected(protected)) => protected.password.clone(),
            _ => return Err(CoreError::MasterPasswordRequired),
        };
        let key = self.recovery_store.key_for_note(&path)?;
        Ok(self.start_secure_job(
            SecureOperationKind::DisableProtection,
            SecureJobTask::DisableProtection(DisableProtectionJob {
                workspace: self.root.clone(),
                path,
                version,
                password,
                title: self.notes[note_index].title.clone(),
                recovery_store: self.recovery_store.clone(),
                recovery_key: key,
            }),
        ))
    }

    pub fn document(&self) -> Option<&DocumentSession> {
        self.document.as_ref()
    }

    pub fn document_mut(&mut self) -> Option<&mut DocumentSession> {
        self.document.as_mut()
    }

    pub fn apply_selected_at(
        &mut self,
        command: EditorCommand,
        now_ms: u64,
    ) -> Result<CommandOutcome, CoreError> {
        self.ensure_no_secure_operation()?;
        let document = self.document.as_mut().ok_or_else(|| {
            CoreError::NoteUnavailable("editor input requires an open note".to_owned())
        })?;
        let outcome = document.apply_at(command, now_ms)?;
        if outcome.text_changed
            && let DocumentTarget::WorkspaceNote(note_index) = document.target()
        {
            let note = self.notes.get_mut(*note_index).ok_or_else(|| {
                CoreError::NoteUnavailable("selected note disappeared".to_owned())
            })?;
            note.title = document.title().to_owned();
        }
        Ok(outcome)
    }

    pub fn open_note(&mut self, note_index: usize) -> Result<(), CoreError> {
        self.ensure_no_secure_operation()?;
        if self
            .document
            .as_ref()
            .is_some_and(DocumentSession::has_unsaved_work)
        {
            return Err(CoreError::UnsavedChanges);
        }
        let note = self.notes.get(note_index).ok_or_else(|| {
            CoreError::NoteUnavailable(format!("unknown note index {note_index}"))
        })?;
        if !note.availability.is_ready() {
            return Err(CoreError::NoteUnavailable(match &note.availability {
                NoteAvailability::Ready => unreachable!(),
                NoteAvailability::InvalidMetadata(message) | NoteAvailability::IoError(message) => {
                    message.clone()
                }
            }));
        }
        if note.protection == NoteProtection::Protected {
            let completion = self.begin_open_protected_note(note_index)?.execute();
            return match self.finish_secure_operation(completion)? {
                SecureOutcome::Unlocked => Ok(()),
                _ => Err(CoreError::NoteUnavailable(
                    "open protected note completed with an unexpected outcome".to_owned(),
                )),
            };
        }
        let document = load_document(note_index, &note.title, &note.path)?;
        let previously_open_protected = self.document.as_ref().and_then(|current| {
            (current.note_index() != note_index && current.is_protected())
                .then_some(current.note_index())
        });
        if let Some(previous_index) = previously_open_protected
            && let Some(previous) = self.notes.get_mut(previous_index)
        {
            redact_protected_projection(previous);
        }
        self.document = Some(document);
        self.selected_note = Some(note_index);
        self.selected_external = None;
        self.selected_engine = None;
        self.refresh_catalog_categories();
        Ok(())
    }

    pub fn begin_autosave(
        &mut self,
        now_ms: u64,
        modified: String,
    ) -> Result<Option<SaveJob>, CoreError> {
        if self.pending_secure_operation.is_some() || self.pending_integrity.is_some() {
            return Ok(None);
        }
        let target = match self.document.as_ref() {
            Some(document) => document.target().clone(),
            None => return Ok(None),
        };
        let (path, recovery_key) = self.document_path_and_recovery_key(&target)?;
        let document = self.document.as_mut().expect("document checked above");
        document.begin_autosave(
            self.root.clone(),
            path,
            now_ms,
            modified,
            self.recovery_store.clone(),
            recovery_key,
        )
    }

    pub fn finish_autosave(&mut self, mut completion: SaveCompletion) -> Result<(), CoreError> {
        let integrity_failure = completion.integrity_failure.clone();
        let retry_job = completion.retry_job.take();
        let committed = completion.result.as_ref().ok().cloned().or_else(|| {
            integrity_failure
                .as_ref()
                .map(|failure| failure.commit.clone())
        });
        let document = self.document.as_mut().ok_or_else(|| {
            CoreError::NoteUnavailable("save completed without an open note".to_owned())
        })?;
        if document.target() != &completion.target {
            return Err(CoreError::NoteUnavailable(
                "save completed for a different note".to_owned(),
            ));
        }
        document.finish_autosave(completion);
        if let Some(failure) = &integrity_failure {
            document.file_version = Some(failure.commit.version);
        }
        if let Some(commit) = committed
            && let DocumentTarget::WorkspaceNote(_) = document.target()
        {
            let current_title = document.title().to_owned();
            if let Some(note) = self.notes.get_mut(document.note_index()) {
                note.path.clone_from(&commit.path);
                note.title = current_title;
            }
            sort_notes(&mut self.notes);
            let next_index = self
                .notes
                .iter()
                .position(|note| note.path == commit.path)
                .ok_or_else(|| {
                    CoreError::NoteUnavailable("saved note disappeared after title sort".to_owned())
                })?;
            document.note_index = next_index;
            document.target = DocumentTarget::WorkspaceNote(next_index);
            self.selected_note = Some(next_index);
            self.selected_external = None;
            self.selected_engine = None;
            self.refresh_catalog_categories();
        }
        if let Some(failure) = integrity_failure {
            self.pending_integrity = Some(PendingIntegrity {
                failure,
                retry: retry_job.map(IntegrityRetry::Autosave),
            });
        }
        Ok(())
    }

    pub fn retry_autosave(&mut self, now_ms: u64) -> bool {
        if self.pending_integrity.is_some() {
            return false;
        }
        self.document
            .as_mut()
            .is_some_and(|document| document.retry_autosave(now_ms))
    }

    pub fn next_autosave_deadline(&self) -> Option<u64> {
        self.document
            .as_ref()
            .and_then(DocumentSession::next_autosave_deadline)
    }

    pub fn next_persistence_deadline(&self) -> Option<u64> {
        self.document
            .as_ref()
            .and_then(DocumentSession::next_persistence_deadline)
    }

    pub fn begin_persistence(
        &mut self,
        now_ms: u64,
        modified: String,
    ) -> Result<Option<PersistenceJob>, CoreError> {
        if self.pending_secure_operation.is_some() || self.pending_integrity.is_some() {
            return Ok(None);
        }
        let target = match self.document.as_ref() {
            Some(document) => document.target().clone(),
            None => return Ok(None),
        };
        let (path, key) = self.document_path_and_recovery_key(&target)?;
        let document = self.document.as_mut().expect("document checked above");
        if document.is_protected()
            && let Some(job) = document.begin_autosave(
                self.root.clone(),
                path.clone(),
                now_ms,
                modified.clone(),
                self.recovery_store.clone(),
                key.clone(),
            )?
        {
            return Ok(Some(PersistenceJob::Save(job)));
        }
        if let Some(job) = document.begin_recovery(self.recovery_store.clone(), key.clone(), now_ms)
        {
            return Ok(Some(PersistenceJob::Recovery(job)));
        }
        if document.is_protected() {
            return Ok(None);
        }
        Ok(document
            .begin_autosave(
                self.root.clone(),
                path,
                now_ms,
                modified,
                self.recovery_store.clone(),
                key,
            )?
            .map(PersistenceJob::Save))
    }

    pub fn finish_persistence(
        &mut self,
        completion: PersistenceCompletion,
    ) -> Result<(), CoreError> {
        match completion {
            PersistenceCompletion::Recovery(completion) => {
                let target = completion.target.clone();
                let recovery_written = completion.result.is_ok();
                let document = self.document.as_mut().ok_or_else(|| {
                    CoreError::NoteUnavailable("recovery completed without an open note".to_owned())
                })?;
                if document.target() != &target {
                    return Err(CoreError::NoteUnavailable(
                        "recovery completed for a different note".to_owned(),
                    ));
                }
                document.finish_recovery(completion);
                if recovery_written {
                    match target {
                        DocumentTarget::WorkspaceNote(note_index) => {
                            if let Some(note) = self.notes.get_mut(note_index) {
                                note.recovery_available = true;
                            }
                        }
                        DocumentTarget::ExternalFile { engine_id, item_id } => {
                            if let Some(file) = self
                                .external_files
                                .iter_mut()
                                .find(|file| file.engine_id == engine_id && file.item_id == item_id)
                            {
                                file.recovery_available = true;
                            }
                        }
                    }
                }
            }
            PersistenceCompletion::Save(completion) => {
                let completion_target = completion.target.clone();
                let cleanup_error = completion.cleanup_error.clone();
                let committed = completion.result.as_ref().ok().cloned();
                let canonical_saved = committed.is_some();
                self.finish_autosave(completion)?;
                if canonical_saved
                    && self
                        .document
                        .as_ref()
                        .is_some_and(|document| !document.has_unsaved_work())
                {
                    let target = self
                        .document
                        .as_ref()
                        .map(|document| document.target().clone())
                        .unwrap_or(completion_target);
                    let recovery_remains = cleanup_error.is_some()
                        && self
                            .document_path_and_recovery_key(&target)
                            .ok()
                            .is_some_and(|(_, key)| self.recovery_store.open(&key).is_ok());
                    match target {
                        DocumentTarget::WorkspaceNote(note_index) => {
                            if let Some(note) = self.notes.get_mut(note_index) {
                                note.recovery_available = recovery_remains;
                            }
                        }
                        DocumentTarget::ExternalFile { engine_id, item_id } => {
                            if let Some(file) = self
                                .external_files
                                .iter_mut()
                                .find(|file| file.engine_id == engine_id && file.item_id == item_id)
                            {
                                file.recovery_available = recovery_remains;
                            }
                        }
                    }
                    if let Some(error) = cleanup_error {
                        let diagnostic = format!(
                            "canonical save succeeded but stale recovery cleanup failed; a later save will retry cleanup: {error}"
                        );
                        if !self.recovery_diagnostics.contains(&diagnostic) {
                            self.recovery_diagnostics.push(diagnostic);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn restore_recovery(&mut self, note_index: usize, now_ms: u64) -> Result<(), CoreError> {
        if self
            .document
            .as_ref()
            .is_some_and(DocumentSession::has_unsaved_work)
        {
            return Err(CoreError::UnsavedChanges);
        }
        let note = self.notes.get(note_index).ok_or_else(|| {
            CoreError::NoteUnavailable(format!("unknown note index {note_index}"))
        })?;
        let path = note.path.clone();
        let title = note.title.clone();
        let protection = note.protection;
        if protection == NoteProtection::Protected {
            let completion = self
                .begin_restore_protected_recovery(note_index, now_ms)?
                .execute();
            return match self.finish_secure_operation(completion)? {
                SecureOutcome::RecoveryRestored => Ok(()),
                _ => Err(CoreError::NoteUnavailable(
                    "protected recovery completed with an unexpected outcome".to_owned(),
                )),
            };
        }
        let key = self.recovery_store.key_for_note(&path)?;
        let artifact = self.recovery_store.open(&key)?;
        let record = artifact.record;
        if record.revision == 0 {
            return Err(CoreError::Recovery(RecoveryError::InvalidArtifact(
                "recovery revision must be positive".to_owned(),
            )));
        }
        let mut document = load_document(note_index, &title, &path)?;
        let recovered = Editor::from_reader(artifact.body.take(record.body_len))?;
        if recovered.len_bytes() as u64 != record.body_len
            || recovered.checksum_fnv1a() != record.body_checksum
        {
            return Err(CoreError::Recovery(RecoveryError::InvalidArtifact(
                "recovery body length/checksum mismatch".to_owned(),
            )));
        }
        let conflict = document.disk_checksum != record.base_checksum;
        document.editor = recovered;
        document.autosave.revision = record.revision;
        document.autosave.saved_revision = 0;
        document.autosave.recovery_revision = record.revision;
        document.autosave.deadline_ms =
            (!conflict).then_some(now_ms.saturating_add(AUTOSAVE_DEBOUNCE_MS));
        document.autosave.external_conflict = conflict.then(|| {
            "disk note changed since this recovery snapshot; both versions are preserved".to_owned()
        });
        self.document = Some(document);
        self.selected_note = Some(note_index);
        self.selected_external = None;
        self.selected_engine = None;
        Ok(())
    }

    pub fn restore_external_recovery(
        &mut self,
        engine_id: &EngineId,
        item_id: &ItemId,
        now_ms: u64,
    ) -> Result<(), CoreError> {
        if self
            .document
            .as_ref()
            .is_some_and(DocumentSession::has_unsaved_work)
        {
            return Err(CoreError::UnsavedChanges);
        }
        let file = self
            .external_files
            .iter()
            .find(|file| &file.engine_id == engine_id && &file.item_id == item_id)
            .ok_or_else(|| CoreError::NoteUnavailable("unknown external file".to_owned()))?;
        let path = file.path.clone();
        let title = file.title.clone();
        let target = DocumentTarget::ExternalFile {
            engine_id: engine_id.clone(),
            item_id: item_id.clone(),
        };
        let key = self
            .recovery_store
            .key_for_external(engine_id.as_str(), item_id.as_str())?;
        let artifact = self.recovery_store.open(&key)?;
        let record = artifact.record;
        if record.revision == 0 {
            return Err(CoreError::Recovery(RecoveryError::InvalidArtifact(
                "recovery revision must be positive".to_owned(),
            )));
        }
        let (reader, version) = open_versioned(&path)?;
        let mut document = DocumentSession::from_versioned_reader(target, title, reader, version)?;
        let recovered = Editor::from_reader(artifact.body.take(record.body_len))?;
        if recovered.len_bytes() as u64 != record.body_len
            || recovered.checksum_fnv1a() != record.body_checksum
        {
            return Err(CoreError::Recovery(RecoveryError::InvalidArtifact(
                "recovery body length/checksum mismatch".to_owned(),
            )));
        }
        let conflict = document.disk_checksum != record.base_checksum;
        document.editor = recovered;
        document.autosave.revision = record.revision;
        document.autosave.saved_revision = 0;
        document.autosave.recovery_revision = record.revision;
        document.autosave.deadline_ms =
            (!conflict).then_some(now_ms.saturating_add(AUTOSAVE_DEBOUNCE_MS));
        document.autosave.external_conflict = conflict.then(|| {
            "disk file changed since this recovery snapshot; both versions are preserved".to_owned()
        });
        self.document = Some(document);
        self.selected_note = None;
        self.selected_external = Some((engine_id.clone(), item_id.clone()));
        Ok(())
    }

    pub fn begin_restore_protected_recovery(
        &mut self,
        note_index: usize,
        now_ms: u64,
    ) -> Result<SecureJob, CoreError> {
        self.ensure_no_secure_operation()?;
        if self
            .document
            .as_ref()
            .is_some_and(DocumentSession::has_unsaved_work)
        {
            return Err(CoreError::UnsavedChanges);
        }
        let note = self.notes.get(note_index).ok_or_else(|| {
            CoreError::NoteUnavailable(format!("unknown note index {note_index}"))
        })?;
        if note.protection != NoteProtection::Protected {
            return Err(CoreError::NoteUnavailable(
                "recovery target is not protected".to_owned(),
            ));
        }
        let path = note.path.clone();
        let password = self
            .master_password
            .as_ref()
            .ok_or(CoreError::MasterPasswordRequired)?
            .clone();
        let key = self.recovery_store.key_for_note(&path)?;
        Ok(self.start_secure_job(
            SecureOperationKind::RestoreRecovery,
            SecureJobTask::RestoreRecovery(RestoreProtectedRecoveryJob {
                note_index,
                path,
                password,
                store: self.recovery_store.clone(),
                key,
                now_ms,
            }),
        ))
    }

    pub fn poll_external(&mut self, now_ms: u64) -> Result<ExternalPoll, CoreError> {
        match self.begin_poll_external(now_ms)? {
            ExternalPollStart::Immediate(result) => Ok(result),
            ExternalPollStart::Secure(job) => match self.finish_secure_operation(job.execute())? {
                SecureOutcome::ExternalPoll(result) => Ok(result),
                _ => Err(CoreError::NoteUnavailable(
                    "external reload completed with an unexpected outcome".to_owned(),
                )),
            },
        }
    }

    fn document_path_and_recovery_key(
        &self,
        target: &DocumentTarget,
    ) -> Result<(PathBuf, RecoveryKey), CoreError> {
        match target {
            DocumentTarget::WorkspaceNote(index) => {
                let note = self.notes.get(*index).ok_or_else(|| {
                    CoreError::NoteUnavailable("selected note disappeared".to_owned())
                })?;
                Ok((
                    note.path.clone(),
                    self.recovery_store.key_for_note(&note.path)?,
                ))
            }
            DocumentTarget::ExternalFile { engine_id, item_id } => {
                let file = self
                    .external_files
                    .iter()
                    .find(|file| &file.engine_id == engine_id && &file.item_id == item_id)
                    .ok_or_else(|| {
                        CoreError::NoteUnavailable("selected external file disappeared".to_owned())
                    })?;
                Ok((
                    file.path.clone(),
                    self.recovery_store
                        .key_for_external(engine_id.as_str(), item_id.as_str())?,
                ))
            }
        }
    }

    pub fn begin_poll_external(&mut self, now_ms: u64) -> Result<ExternalPollStart, CoreError> {
        if self.pending_secure_operation.is_some() || self.pending_integrity.is_some() {
            return Ok(ExternalPollStart::Immediate(ExternalPoll::Deferred));
        }
        let Some(document) = self.document.as_mut() else {
            return Ok(ExternalPollStart::Immediate(ExternalPoll::Unchanged));
        };
        if document.autosave.saving_revision.is_some()
            || document.autosave.recovery_saving_revision.is_some()
        {
            return Ok(ExternalPollStart::Immediate(ExternalPoll::Deferred));
        }
        let target = document.target().clone();
        let (path, protection) = match &target {
            DocumentTarget::WorkspaceNote(index) => {
                let note = self.notes.get(*index).ok_or_else(|| {
                    CoreError::NoteUnavailable("selected note disappeared".to_owned())
                })?;
                (note.path.clone(), note.protection)
            }
            DocumentTarget::ExternalFile { engine_id, item_id } => {
                let file = self
                    .external_files
                    .iter()
                    .find(|file| &file.engine_id == engine_id && &file.item_id == item_id)
                    .ok_or_else(|| {
                        CoreError::NoteUnavailable("selected external file disappeared".to_owned())
                    })?;
                (file.path.clone(), NoteProtection::Plain)
            }
        };
        let current_version = match open_versioned(&path) {
            Ok((_file, version)) => version,
            Err(error) => {
                if let DocumentTarget::ExternalFile { engine_id, item_id } = &target
                    && let Some(file) = self
                        .external_files
                        .iter_mut()
                        .find(|file| &file.engine_id == engine_id && &file.item_id == item_id)
                {
                    file.availability = ItemAvailability::Unavailable(error.to_string());
                }
                document.autosave.external_conflict = Some(error.to_string());
                document.autosave.recovery_deadline_ms = Some(now_ms);
                return Ok(ExternalPollStart::Immediate(ExternalPoll::Conflict));
            }
        };
        if document.file_version == Some(current_version) {
            return Ok(ExternalPollStart::Immediate(ExternalPoll::Unchanged));
        }
        if document.has_unsaved_work() {
            document.autosave.external_conflict = Some(
                "file changed on disk while local edits were pending; both versions are preserved"
                    .to_owned(),
            );
            document.autosave.recovery_deadline_ms = Some(now_ms);
            return Ok(ExternalPollStart::Immediate(ExternalPoll::Conflict));
        }
        if let DocumentTarget::ExternalFile { .. } = target {
            let (file, version) = open_versioned(&path)?;
            if let DocumentTarget::ExternalFile { engine_id, item_id } = &target
                && let Some(summary) = self
                    .external_files
                    .iter_mut()
                    .find(|file| &file.engine_id == engine_id && &file.item_id == item_id)
            {
                summary.availability = ItemAvailability::Ready;
            }
            self.document = Some(DocumentSession::from_versioned_reader(
                target,
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("External")
                    .to_owned(),
                file,
                version,
            )?);
            return Ok(ExternalPollStart::Immediate(ExternalPoll::Reloaded));
        }
        let DocumentTarget::WorkspaceNote(note_index) = target else {
            unreachable!()
        };
        if protection == NoteProtection::Plain {
            // The external writer may have changed the front matter too, so the
            // note summary (title, tags, flags) is rescanned with the document.
            let note_index = self.refresh_plain_note(note_index, &path)?;
            self.document = Some(load_document(
                note_index,
                &self.notes[note_index].title,
                &path,
            )?);
            return Ok(ExternalPollStart::Immediate(ExternalPoll::Reloaded));
        }
        let password = self
            .master_password
            .as_ref()
            .ok_or(CoreError::MasterPasswordRequired)?
            .clone();
        let job = self.start_secure_job(
            SecureOperationKind::ExternalReload,
            SecureJobTask::Load(LoadProtectedJob {
                note_index,
                path,
                password,
                purpose: ProtectedLoadPurpose::ExternalReload,
                recovery_cleanup: None,
            }),
        );
        Ok(ExternalPollStart::Secure(job))
    }

    pub fn discard_local_and_reload(&mut self) -> Result<(), CoreError> {
        let target = self
            .document
            .as_ref()
            .map(|document| document.target().clone())
            .ok_or_else(|| CoreError::NoteUnavailable("no open note".to_owned()))?;
        if let DocumentTarget::ExternalFile { engine_id, item_id } = target {
            let file = self
                .external_files
                .iter_mut()
                .find(|file| file.engine_id == engine_id && file.item_id == item_id)
                .ok_or_else(|| {
                    CoreError::NoteUnavailable("selected external file disappeared".to_owned())
                })?;
            let (reader, version) = open_versioned(&file.path)?;
            let replacement = DocumentSession::from_versioned_reader(
                DocumentTarget::ExternalFile {
                    engine_id: engine_id.clone(),
                    item_id: item_id.clone(),
                },
                file.title.clone(),
                reader,
                version,
            )?;
            let key = self
                .recovery_store
                .key_for_external(engine_id.as_str(), item_id.as_str())?;
            self.recovery_store.remove(&key)?;
            file.recovery_available = false;
            self.document = Some(replacement);
            return Ok(());
        }
        let DocumentTarget::WorkspaceNote(note_index) = target else {
            unreachable!()
        };
        let note = self
            .notes
            .get(note_index)
            .ok_or_else(|| CoreError::NoteUnavailable("selected note disappeared".to_owned()))?;
        let path = note.path.clone();
        let protection = note.protection;
        if protection == NoteProtection::Protected {
            let completion = self.begin_discard_protected_local_and_reload()?.execute();
            return match self.finish_secure_operation(completion)? {
                SecureOutcome::DiscardedAndReloaded => Ok(()),
                _ => Err(CoreError::NoteUnavailable(
                    "protected discard completed with an unexpected outcome".to_owned(),
                )),
            };
        }
        let (note_index, replacement) = match protection {
            NoteProtection::Plain => {
                let note_index = self.refresh_plain_note(note_index, &path)?;
                let document = load_document(note_index, &self.notes[note_index].title, &path)?;
                (note_index, document)
            }
            NoteProtection::Protected => unreachable!(),
        };
        let key = self.recovery_store.key_for_note(&path)?;
        match protection {
            NoteProtection::Plain => self.recovery_store.remove(&key)?,
            NoteProtection::Protected => unreachable!(),
        };
        self.document = Some(replacement);
        self.selected_note = Some(note_index);
        self.selected_external = None;
        self.selected_engine = None;
        if let Some(note) = self.notes.get_mut(note_index) {
            note.recovery_available = false;
        }
        Ok(())
    }

    pub fn begin_discard_protected_local_and_reload(&mut self) -> Result<SecureJob, CoreError> {
        self.ensure_no_secure_operation()?;
        let document = self
            .document
            .as_ref()
            .ok_or_else(|| CoreError::NoteUnavailable("no open note".to_owned()))?;
        if document.autosave.saving_revision.is_some()
            || document.autosave.recovery_saving_revision.is_some()
        {
            return Err(CoreError::UnsavedChanges);
        }
        let note_index = document.note_index();
        let note = self
            .notes
            .get(note_index)
            .ok_or_else(|| CoreError::NoteUnavailable("selected note disappeared".to_owned()))?;
        if note.protection != NoteProtection::Protected {
            return Err(CoreError::NoteUnavailable(
                "selected note is not protected".to_owned(),
            ));
        }
        let DocumentProtection::Protected(protected) = &document.protection else {
            return Err(CoreError::MasterPasswordRequired);
        };
        let path = note.path.clone();
        let key = self.recovery_store.key_for_note(&path)?;
        Ok(self.start_secure_job(
            SecureOperationKind::DiscardReload,
            SecureJobTask::Load(LoadProtectedJob {
                note_index,
                path,
                password: protected.password.clone(),
                purpose: ProtectedLoadPurpose::DiscardReload,
                recovery_cleanup: Some((self.recovery_store.clone(), key)),
            }),
        ))
    }

    /// Rescans one plain note so its summary matches on-disk front matter and
    /// returns the note's possibly re-sorted index.
    fn refresh_plain_note(&mut self, note_index: usize, path: &Path) -> Result<usize, CoreError> {
        let result = scan_note(path).map_err(|error| CoreError::Workspace(error.to_string()))?;
        let mut note = note_summary(path.to_path_buf(), result);
        if let Ok(key) = self.recovery_store.key_for_note(path) {
            note.recovery_available = self
                .recovery_store
                .scan()
                .records
                .iter()
                .any(|record| record.key == key);
        }
        let slot = self.notes.get_mut(note_index).ok_or_else(|| {
            CoreError::NoteUnavailable(format!("reloaded note index {note_index} disappeared"))
        })?;
        if slot.path != path {
            return Err(CoreError::NoteUnavailable(format!(
                "reloaded note {} no longer matches index {note_index}",
                path.display()
            )));
        }
        *slot = note;
        sort_notes(&mut self.notes);
        let index = self
            .notes
            .iter()
            .position(|note| note.path == path)
            .ok_or_else(|| {
                CoreError::NoteUnavailable(format!(
                    "reloaded note {} (index {note_index}) was not found by workspace scan",
                    path.display()
                ))
            })?;
        self.refresh_catalog_categories();
        self.selected_note = Some(index);
        self.selected_external = None;
        self.selected_engine = None;
        Ok(index)
    }

    pub fn create_note(&mut self, title: &str, timestamp: &str) -> Result<usize, CoreError> {
        self.ensure_workspace_action_ready()?;
        let commit = create_note_file(&self.root, title, timestamp)?;
        self.refresh_and_open(&commit.path)
    }

    pub fn rename_selected(&mut self, title: &str, timestamp: &str) -> Result<(), CoreError> {
        let (note_index, path, version) = self.selected_operation_target()?;
        if self.notes[note_index].protection == NoteProtection::Protected {
            let completion = self
                .begin_rename_protected_selected(title, timestamp)?
                .execute();
            return match self.finish_secure_operation(completion)? {
                SecureOutcome::MetadataChanged => Ok(()),
                _ => Err(CoreError::NoteUnavailable(
                    "protected rename completed with an unexpected outcome".to_owned(),
                )),
            };
        }
        let commit = rename_note_file(&self.root, &path, &version, title, timestamp)?;
        self.refresh_and_open(&commit.path)?;
        Ok(())
    }

    pub fn begin_rename_protected_selected(
        &mut self,
        title: &str,
        timestamp: &str,
    ) -> Result<SecureJob, CoreError> {
        let title = validate_note_title(title)?;
        self.begin_protected_metadata(
            MetadataPatch {
                title: Some(title.clone()),
                modified: Some(timestamp.to_owned()),
                ..MetadataPatch::default()
            },
            Some(title),
        )
    }

    pub fn set_deleted_selected(
        &mut self,
        deleted: bool,
        timestamp: &str,
    ) -> Result<PathBuf, CoreError> {
        let (note_index, path, version) =
            delete_diagnostics::result(true, "SelectedTarget", self.selected_operation_target())?;
        let protection = self.notes[note_index].protection;
        if self.notes[note_index].deleted == deleted {
            return Ok(path);
        }
        let key = delete_diagnostics::result(
            true,
            "RecoveryKey",
            self.recovery_store
                .key_for_note(&path)
                .map_err(CoreError::from),
        )?;
        delete_diagnostics::result(
            true,
            "RecoveryRemove",
            match protection {
                NoteProtection::Plain => self.recovery_store.remove(&key),
                NoteProtection::Protected => self.recovery_store.remove_protected(&key),
            }
            .map_err(CoreError::from),
        )?;
        if protection == NoteProtection::Protected {
            let completion = delete_diagnostics::result(
                true,
                "SecureBegin",
                self.begin_set_deleted_protected_selected(deleted, timestamp),
            )?
            .execute();
            return match delete_diagnostics::result(
                true,
                "SecureFinish",
                self.finish_secure_operation(completion),
            )? {
                SecureOutcome::MetadataChanged => Ok(self
                    .selected_note
                    .and_then(|index| self.notes.get(index))
                    .map(|note| note.path.clone())
                    .unwrap_or(path)),
                _ => Err(CoreError::NoteUnavailable(
                    "protected delete completed with an unexpected outcome".to_owned(),
                )),
            };
        }
        delete_diagnostics::result(
            true,
            "RewriteMetadata",
            rewrite_metadata_versioned(
                &path,
                &version,
                &MetadataPatch {
                    deleted: Some(deleted),
                    modified: Some(timestamp.to_owned()),
                    ..MetadataPatch::default()
                },
            )
            .map_err(CoreError::from),
        )?;
        self.refresh_and_open_for_operation(&path, true)?;
        if deleted {
            self.document = None;
            self.selected_note = None;
        }
        Ok(path)
    }

    pub fn begin_set_deleted_protected_selected(
        &mut self,
        deleted: bool,
        timestamp: &str,
    ) -> Result<SecureJob, CoreError> {
        let (note_index, path, _) = self.selected_operation_target()?;
        if self.notes[note_index].deleted == deleted {
            return Err(CoreError::NoteUnavailable(
                "note already has the requested deleted state".to_owned(),
            ));
        }
        let key = self.recovery_store.key_for_note(&path)?;
        self.recovery_store.remove_protected(&key)?;
        self.begin_protected_metadata(
            MetadataPatch {
                deleted: Some(deleted),
                modified: Some(timestamp.to_owned()),
                ..MetadataPatch::default()
            },
            None,
        )
    }

    pub fn add_tag_selected(&mut self, tag: &str, timestamp: &str) -> Result<bool, CoreError> {
        let tag = validate_tag(tag)?;
        let (note_index, _path, _version) = self.selected_operation_target()?;
        let mut tags = self.notes[note_index].tags.clone();
        if tags.iter().any(|existing| existing == &tag) {
            return Ok(false);
        }
        tags.push(tag);
        if self.notes[note_index].protection == NoteProtection::Protected {
            let completion = self
                .begin_protected_metadata(
                    MetadataPatch {
                        tags: Some(tags),
                        modified: Some(timestamp.to_owned()),
                        ..MetadataPatch::default()
                    },
                    None,
                )?
                .execute();
            return match self.finish_secure_operation(completion)? {
                SecureOutcome::MetadataChanged => Ok(true),
                _ => Err(CoreError::NoteUnavailable(
                    "protected tag addition completed with an unexpected outcome".to_owned(),
                )),
            };
        }
        self.rewrite_selected_metadata(
            MetadataPatch {
                tags: Some(tags),
                modified: Some(timestamp.to_owned()),
                ..MetadataPatch::default()
            },
            note_index,
        )?;
        Ok(true)
    }

    pub fn begin_add_tag_protected_selected(
        &mut self,
        tag: &str,
        timestamp: &str,
    ) -> Result<Option<SecureJob>, CoreError> {
        let tag = validate_tag(tag)?;
        let note_index = self.selected_operation_target()?.0;
        let mut tags = self.notes[note_index].tags.clone();
        if tags.iter().any(|existing| existing == &tag) {
            return Ok(None);
        }
        tags.push(tag);
        self.begin_protected_metadata(
            MetadataPatch {
                tags: Some(tags),
                modified: Some(timestamp.to_owned()),
                ..MetadataPatch::default()
            },
            None,
        )
        .map(Some)
    }

    pub fn remove_tag_selected(&mut self, tag: &str, timestamp: &str) -> Result<bool, CoreError> {
        let tag = validate_tag(tag)?;
        let (note_index, _path, _version) = self.selected_operation_target()?;
        let mut tags = self.notes[note_index].tags.clone();
        let previous_len = tags.len();
        tags.retain(|existing| existing != &tag);
        if tags.len() == previous_len {
            return Ok(false);
        }
        let mut order = self.notes[note_index].order.clone();
        if tag != FAVORITED_ORDER_KEY {
            order.remove(&tag);
        }
        if self.notes[note_index].protection == NoteProtection::Protected {
            let completion = self
                .begin_protected_metadata(
                    MetadataPatch {
                        tags: Some(tags),
                        order: Some(order),
                        modified: Some(timestamp.to_owned()),
                        ..MetadataPatch::default()
                    },
                    None,
                )?
                .execute();
            return match self.finish_secure_operation(completion)? {
                SecureOutcome::MetadataChanged => Ok(true),
                _ => Err(CoreError::NoteUnavailable(
                    "protected tag removal completed with an unexpected outcome".to_owned(),
                )),
            };
        }
        self.rewrite_selected_metadata(
            MetadataPatch {
                tags: Some(tags),
                order: Some(order),
                modified: Some(timestamp.to_owned()),
                ..MetadataPatch::default()
            },
            note_index,
        )?;
        Ok(true)
    }

    pub fn set_category_note_order(
        &mut self,
        category: &str,
        ordered_paths: &[PathBuf],
    ) -> Result<bool, CoreError> {
        let category = validate_tag(category)?;
        if category == FAVORITED_ORDER_KEY {
            return Err(CoreError::NoteUnavailable(
                "category name is reserved for Favorites ordering".to_owned(),
            ));
        }
        self.update_note_order(&category, NoteOrderSpec::Paths(ordered_paths), |note| {
            note.tags.iter().any(|tag| tag == &category)
        })
    }

    pub fn clear_category_note_order(&mut self, category: &str) -> Result<bool, CoreError> {
        let category = validate_tag(category)?;
        if category == FAVORITED_ORDER_KEY {
            return Err(CoreError::NoteUnavailable(
                "category name is reserved for Favorites ordering".to_owned(),
            ));
        }
        self.update_note_order(&category, NoteOrderSpec::Clear, |note| {
            note.tags.iter().any(|tag| tag == &category)
        })
    }

    pub fn set_favorited_note_order(
        &mut self,
        ordered_paths: &[PathBuf],
    ) -> Result<bool, CoreError> {
        self.update_note_order(
            FAVORITED_ORDER_KEY,
            NoteOrderSpec::Paths(ordered_paths),
            |note| note.favorited,
        )
    }

    pub fn clear_favorited_note_order(&mut self) -> Result<bool, CoreError> {
        self.update_note_order(FAVORITED_ORDER_KEY, NoteOrderSpec::Clear, |note| {
            note.favorited
        })
    }

    pub fn set_catalog_order(
        &mut self,
        order_key: &str,
        ordered: &[CatalogOrderItem],
    ) -> Result<bool, CoreError> {
        self.ensure_workspace_action_ready()?;
        let order_key = if order_key == FAVORITED_ORDER_KEY {
            FAVORITED_ORDER_KEY.to_owned()
        } else {
            validate_tag(order_key)?
        };
        let matches_note = |note: &NoteSummary| {
            if order_key == FAVORITED_ORDER_KEY {
                note.favorited
            } else {
                note.tags.iter().any(|tag| tag == &order_key)
            }
        };
        let mut expected = BTreeMap::<CatalogOrderItem, bool>::new();
        for note in self
            .notes
            .iter()
            .filter(|note| note.availability.is_ready() && !note.deleted && matches_note(note))
        {
            expected.insert(CatalogOrderItem::Note(note.path.clone()), note.pinned);
        }
        for item in self.non_document_items().iter().filter(|i| {
            !i.metadata.deleted
                && if order_key == FAVORITED_ORDER_KEY {
                    i.metadata.favorited
                } else {
                    i.metadata.categories.contains(&order_key)
                }
        }) {
            expected.insert(
                CatalogOrderItem::Engine(item.engine_id.clone(), item.item_id.clone()),
                item.metadata.pinned,
            );
        }
        let supplied = ordered.iter().cloned().collect::<BTreeSet<_>>();
        if ordered.len() != expected.len()
            || supplied.len() != ordered.len()
            || supplied != expected.keys().cloned().collect()
        {
            return Err(CoreError::NoteUnavailable(
                "catalog order must contain every item in its scope exactly once".to_owned(),
            ));
        }
        let mut pinned_rank = 0_u32;
        let mut regular_rank = 0_u32;
        let mut note_ranks = BTreeMap::new();
        let mut engine_ranks = BTreeMap::new();
        for item in ordered {
            let pinned = expected.get(item).copied().ok_or_else(|| {
                CoreError::NoteUnavailable("catalog order target disappeared".to_owned())
            })?;
            let rank = if pinned {
                let rank = pinned_rank;
                pinned_rank = pinned_rank.checked_add(1).ok_or_else(|| {
                    CoreError::NoteUnavailable("too many pinned catalog items".to_owned())
                })?;
                rank
            } else {
                let rank = regular_rank;
                regular_rank = regular_rank.checked_add(1).ok_or_else(|| {
                    CoreError::NoteUnavailable("too many catalog items".to_owned())
                })?;
                rank
            };
            match item {
                CatalogOrderItem::Note(path) => {
                    note_ranks.insert(path.clone(), rank);
                }
                CatalogOrderItem::Engine(engine_id, item_id) => {
                    engine_ranks.insert((engine_id.clone(), item_id.clone()), rank);
                }
            }
        }
        let mut changed =
            self.update_note_order(&order_key, NoteOrderSpec::Ranks(&note_ranks), matches_note)?;
        for ((engine, id), rank) in engine_ranks {
            let item = self
                .non_document_items()
                .into_iter()
                .find(|i| i.engine_id == engine && i.item_id == id)
                .ok_or_else(|| CoreError::NoteUnavailable("catalog target disappeared".into()))?;
            let mut order = item.metadata.order;
            if order.get(&order_key) == Some(&rank) {
                continue;
            }
            order.insert(order_key.clone(), rank);
            self.update_engine_metadata(
                &engine,
                &id,
                &item.metadata_version,
                stillus_engine::CommonMetadataPatch {
                    order: Some(order),
                    ..Default::default()
                },
            )?;
            changed = true;
        }
        Ok(changed)
    }

    pub fn clear_catalog_order(&mut self, order_key: &str) -> Result<bool, CoreError> {
        self.ensure_workspace_action_ready()?;
        let order_key = if order_key == FAVORITED_ORDER_KEY {
            FAVORITED_ORDER_KEY.to_owned()
        } else {
            validate_tag(order_key)?
        };
        let matches_note = |note: &NoteSummary| {
            if order_key == FAVORITED_ORDER_KEY {
                note.favorited
            } else {
                note.tags.iter().any(|tag| tag == &order_key)
            }
        };
        let mut changed = self.update_note_order(&order_key, NoteOrderSpec::Clear, matches_note)?;
        let targets = self
            .non_document_items()
            .into_iter()
            .filter(|i| {
                !i.metadata.deleted
                    && i.metadata.order.contains_key(&order_key)
                    && if order_key == FAVORITED_ORDER_KEY {
                        i.metadata.favorited
                    } else {
                        i.metadata.categories.contains(&order_key)
                    }
            })
            .collect::<Vec<_>>();
        for item in targets {
            let mut order = item.metadata.order;
            order.remove(&order_key);
            self.update_engine_metadata(
                &item.engine_id,
                &item.item_id,
                &item.metadata_version,
                stillus_engine::CommonMetadataPatch {
                    order: Some(order),
                    ..Default::default()
                },
            )?;
            changed = true;
        }
        Ok(changed)
    }

    fn update_note_order(
        &mut self,
        order_key: &str,
        order_spec: NoteOrderSpec<'_>,
        matches_scope: impl Fn(&NoteSummary) -> bool,
    ) -> Result<bool, CoreError> {
        self.ensure_workspace_action_ready()?;
        let targets = self
            .notes
            .iter()
            .enumerate()
            .filter(|(_, note)| {
                note.availability.is_ready() && !note.deleted && matches_scope(note)
            })
            .map(|(index, note)| (index, note.path.clone(), note.pinned))
            .collect::<Vec<_>>();
        if targets
            .iter()
            .any(|(index, _, _)| self.notes[*index].recovery_available)
        {
            return Err(CoreError::UnsavedChanges);
        }

        let ranks = if let NoteOrderSpec::Paths(paths) = order_spec {
            if paths.len() != targets.len() {
                return Err(CoreError::NoteUnavailable(
                    "note order must contain every note in its scope exactly once".to_owned(),
                ));
            }
            let target_paths = targets
                .iter()
                .map(|(_, path, _)| path.clone())
                .collect::<BTreeSet<_>>();
            let supplied_paths = paths.iter().cloned().collect::<BTreeSet<_>>();
            if supplied_paths.len() != paths.len() || supplied_paths != target_paths {
                return Err(CoreError::NoteUnavailable(
                    "note order contains a missing, duplicate, or foreign note".to_owned(),
                ));
            }
            let pinned_by_path = targets
                .iter()
                .map(|(_, path, pinned)| (path.clone(), *pinned))
                .collect::<BTreeMap<_, _>>();
            let mut pinned_rank = 0_u32;
            let mut regular_rank = 0_u32;
            let mut ranks = BTreeMap::new();
            for path in paths {
                let pinned = pinned_by_path.get(path).copied().ok_or_else(|| {
                    CoreError::NoteUnavailable("category order target disappeared".to_owned())
                })?;
                let rank = if pinned {
                    let rank = pinned_rank;
                    pinned_rank = pinned_rank.checked_add(1).ok_or_else(|| {
                        CoreError::NoteUnavailable("too many pinned notes to order".to_owned())
                    })?;
                    rank
                } else {
                    let rank = regular_rank;
                    regular_rank = regular_rank.checked_add(1).ok_or_else(|| {
                        CoreError::NoteUnavailable("too many notes to order".to_owned())
                    })?;
                    rank
                };
                ranks.insert(path.clone(), rank);
            }
            Some(ranks)
        } else if let NoteOrderSpec::Ranks(ranks) = order_spec {
            let target_paths = targets
                .iter()
                .map(|(_, path, _)| path.clone())
                .collect::<BTreeSet<_>>();
            let supplied_paths = ranks.keys().cloned().collect::<BTreeSet<_>>();
            if supplied_paths != target_paths {
                return Err(CoreError::NoteUnavailable(
                    "note order ranks contain a missing or foreign note".to_owned(),
                ));
            }
            Some(ranks.clone())
        } else {
            None
        };

        let mut changed = false;
        for (index, path, _) in targets {
            let mut order = self.notes[index].order.clone();
            match &ranks {
                Some(ranks) => {
                    let rank = ranks.get(&path).copied().ok_or_else(|| {
                        CoreError::NoteUnavailable("note order rank disappeared".to_owned())
                    })?;
                    order.insert(order_key.to_owned(), rank);
                }
                None => {
                    order.remove(order_key);
                }
            }
            if order == self.notes[index].order {
                continue;
            }
            let version = open_versioned(&path)?.1;
            let patch = MetadataPatch {
                order: Some(order),
                ..MetadataPatch::default()
            };
            let commit = if self.notes[index].protection == NoteProtection::Protected {
                let frontmatter = match scan_note(&path)
                    .map_err(|error| CoreError::NoteUnavailable(error.to_string()))?
                {
                    NoteScanResult::Protected(scan) => scan.frontmatter,
                    _ => {
                        return Err(CoreError::NoteUnavailable(
                            "order target is not a valid protected note".to_owned(),
                        ));
                    }
                };
                let rewrite = patch_front_matter(&frontmatter, &patch)
                    .map_err(|error| CoreError::Save(SaveError::Patch(error)))?
                    .ok_or_else(|| {
                        CoreError::Save(SaveError::InvalidTarget(
                            "protected order patch was empty".to_owned(),
                        ))
                    })?;
                let next_frontmatter = scan_reader(Cursor::new(&rewrite.prefix))
                    .map_err(|error| CoreError::NoteUnavailable(error.to_string()))?;
                match (ProtectedMetadataJob {
                    note_index: index,
                    path: path.clone(),
                    version,
                    patch: patch.clone(),
                    next_frontmatter,
                    workspace: self.root.clone(),
                    rename_title: None,
                })
                .execute()?
                {
                    SecureJobResult::MetadataChanged { commit, .. } => commit,
                    SecureJobResult::IntegrityFailure { failure, retry } => {
                        self.pending_integrity = Some(PendingIntegrity {
                            failure,
                            retry: Some(retry),
                        });
                        return Ok(true);
                    }
                    _ => {
                        return Err(CoreError::NoteUnavailable(
                            "protected order update returned an unexpected result".to_owned(),
                        ));
                    }
                }
            } else {
                rewrite_metadata_versioned(&path, &version, &patch)?
            };
            let recovery_available = self.notes[index].recovery_available;
            let result =
                scan_note(&path).map_err(|error| CoreError::Workspace(error.to_string()))?;
            let mut refreshed = note_summary(path.clone(), result);
            refreshed.recovery_available = recovery_available;
            self.notes[index] = refreshed;
            if let Some(document) = self
                .document
                .as_mut()
                .filter(|document| document.note_index() == index)
            {
                document.file_version = Some(commit.version);
                if let DocumentProtection::Protected(protected) = &mut document.protection {
                    let NoteScanResult::Protected(scan) = scan_note(&path)
                        .map_err(|error| CoreError::Workspace(error.to_string()))?
                    else {
                        return Err(CoreError::NoteUnavailable(
                            "protected note changed format during order update".to_owned(),
                        ));
                    };
                    protected.frontmatter = scan.frontmatter;
                }
            }
            changed = true;
        }
        Ok(changed)
    }

    pub fn begin_remove_tag_protected_selected(
        &mut self,
        tag: &str,
        timestamp: &str,
    ) -> Result<Option<SecureJob>, CoreError> {
        let tag = validate_tag(tag)?;
        let note_index = self.selected_operation_target()?.0;
        let mut tags = self.notes[note_index].tags.clone();
        let previous_len = tags.len();
        tags.retain(|existing| existing != &tag);
        if tags.len() == previous_len {
            return Ok(None);
        }
        let mut order = self.notes[note_index].order.clone();
        if tag != FAVORITED_ORDER_KEY {
            order.remove(&tag);
        }
        self.begin_protected_metadata(
            MetadataPatch {
                tags: Some(tags),
                order: Some(order),
                modified: Some(timestamp.to_owned()),
                ..MetadataPatch::default()
            },
            None,
        )
        .map(Some)
    }

    pub fn toggle_pinned_selected(&mut self, timestamp: &str) -> Result<bool, CoreError> {
        let (note_index, _path, _version) = self.selected_operation_target()?;
        let value = !self.notes[note_index].pinned;
        if self.notes[note_index].protection == NoteProtection::Protected {
            let (value, job) = self.begin_toggle_pinned_protected_selected(timestamp)?;
            return match self.finish_secure_operation(job.execute())? {
                SecureOutcome::MetadataChanged => Ok(value),
                _ => Err(CoreError::NoteUnavailable(
                    "protected pin toggle completed with an unexpected outcome".to_owned(),
                )),
            };
        }
        self.rewrite_selected_metadata(
            MetadataPatch {
                pinned: Some(value),
                modified: Some(timestamp.to_owned()),
                ..MetadataPatch::default()
            },
            note_index,
        )?;
        Ok(value)
    }

    pub fn begin_toggle_pinned_protected_selected(
        &mut self,
        timestamp: &str,
    ) -> Result<(bool, SecureJob), CoreError> {
        let note_index = self.selected_operation_target()?.0;
        let value = !self.notes[note_index].pinned;
        let job = self.begin_protected_metadata(
            MetadataPatch {
                pinned: Some(value),
                modified: Some(timestamp.to_owned()),
                ..MetadataPatch::default()
            },
            None,
        )?;
        Ok((value, job))
    }

    pub fn toggle_favorited_selected(&mut self, timestamp: &str) -> Result<bool, CoreError> {
        let (note_index, _path, _version) = self.selected_operation_target()?;
        let value = !self.notes[note_index].favorited;
        if self.notes[note_index].protection == NoteProtection::Protected {
            let (value, job) = self.begin_toggle_favorited_protected_selected(timestamp)?;
            return match self.finish_secure_operation(job.execute())? {
                SecureOutcome::MetadataChanged => Ok(value),
                _ => Err(CoreError::NoteUnavailable(
                    "protected favorite toggle completed with an unexpected outcome".to_owned(),
                )),
            };
        }
        let mut order = self.notes[note_index].order.clone();
        if !value {
            order.remove(FAVORITED_ORDER_KEY);
        }
        self.rewrite_selected_metadata(
            MetadataPatch {
                favorited: Some(value),
                order: (!value).then_some(order),
                modified: Some(timestamp.to_owned()),
                ..MetadataPatch::default()
            },
            note_index,
        )?;
        Ok(value)
    }

    pub fn begin_toggle_favorited_protected_selected(
        &mut self,
        timestamp: &str,
    ) -> Result<(bool, SecureJob), CoreError> {
        let note_index = self.selected_operation_target()?.0;
        let value = !self.notes[note_index].favorited;
        let mut order = self.notes[note_index].order.clone();
        if !value {
            order.remove(FAVORITED_ORDER_KEY);
        }
        let job = self.begin_protected_metadata(
            MetadataPatch {
                favorited: Some(value),
                order: (!value).then_some(order),
                modified: Some(timestamp.to_owned()),
                ..MetadataPatch::default()
            },
            None,
        )?;
        Ok((value, job))
    }

    fn rewrite_selected_metadata(
        &mut self,
        patch: MetadataPatch,
        note_index: usize,
    ) -> Result<(), CoreError> {
        let (selected_index, path, version) = self.selected_operation_target()?;
        if selected_index != note_index {
            return Err(CoreError::NoteUnavailable(
                "selected note changed during metadata operation".to_owned(),
            ));
        }
        if self.notes[note_index].protection == NoteProtection::Protected {
            let completion = self.begin_protected_metadata(patch, None)?.execute();
            return match self.finish_secure_operation(completion)? {
                SecureOutcome::MetadataChanged => Ok(()),
                _ => Err(CoreError::NoteUnavailable(
                    "protected metadata rewrite completed with an unexpected outcome".to_owned(),
                )),
            };
        }
        rewrite_metadata_versioned(&path, &version, &patch)?;
        self.refresh_and_open(&path)?;
        Ok(())
    }

    fn begin_protected_metadata(
        &mut self,
        patch: MetadataPatch,
        rename_title: Option<String>,
    ) -> Result<SecureJob, CoreError> {
        let (note_index, path, version) = self.selected_operation_target()?;
        if self.notes[note_index].protection != NoteProtection::Protected {
            return Err(CoreError::NoteUnavailable(
                "selected note is not protected".to_owned(),
            ));
        }
        let frontmatter = match scan_note(&path)
            .map_err(|error| CoreError::NoteUnavailable(error.to_string()))?
        {
            NoteScanResult::Protected(scan) => scan.frontmatter,
            _ => {
                return Err(CoreError::NoteUnavailable(
                    "selected note is not a valid protected note".to_owned(),
                ));
            }
        };
        let rewrite = patch_front_matter(&frontmatter, &patch)
            .map_err(|error| CoreError::Save(SaveError::Patch(error)))?
            .ok_or_else(|| {
                CoreError::Save(SaveError::InvalidTarget(
                    "protected metadata patch was empty".to_owned(),
                ))
            })?;
        let next_frontmatter = scan_reader(Cursor::new(&rewrite.prefix))
            .map_err(|error| CoreError::NoteUnavailable(error.to_string()))?;
        Ok(self.start_secure_job(
            SecureOperationKind::Metadata,
            SecureJobTask::Metadata(Box::new(ProtectedMetadataJob {
                note_index,
                path,
                version,
                patch,
                next_frontmatter,
                workspace: self.root.clone(),
                rename_title,
            })),
        ))
    }

    fn start_secure_job(&mut self, kind: SecureOperationKind, task: SecureJobTask) -> SecureJob {
        debug_assert!(self.pending_secure_operation.is_none());
        self.secure_operation_generation = self.secure_operation_generation.saturating_add(1);
        let operation_id = self.secure_operation_generation;
        self.pending_secure_operation = Some(PendingSecureOperation {
            id: operation_id,
            kind,
        });
        SecureJob {
            operation_id,
            kind,
            task: Box::new(task),
        }
    }

    fn ensure_no_secure_operation(&self) -> Result<(), CoreError> {
        if self.pending_secure_operation.is_some()
            || self.pending_integrity.is_some()
            || self.password_change_recovery_blocked
        {
            return Err(CoreError::UnsavedChanges);
        }
        Ok(())
    }

    pub fn finish_secure_operation(
        &mut self,
        completion: SecureCompletion,
    ) -> Result<SecureOutcome, CoreError> {
        let pending = self.pending_secure_operation.ok_or_else(|| {
            CoreError::NoteUnavailable("secure completion has no active operation".to_owned())
        })?;
        if pending.id != completion.operation_id || pending.kind != completion.kind {
            return Err(CoreError::NoteUnavailable(
                "stale secure completion does not match the active operation".to_owned(),
            ));
        }
        let resolving_integrity = completion.kind == SecureOperationKind::Integrity;
        self.pending_secure_operation = None;
        let result = match completion.result {
            Ok(result) => result,
            Err(error) => {
                if completion.kind == SecureOperationKind::ChangeMasterPassword {
                    for engine in &mut self.engines {
                        engine.resume();
                    }
                    if matches!(&error, CoreError::PasswordChange(error) if error.blocks_workspace())
                    {
                        self.password_change_recovery_blocked = true;
                    }
                    if let Some(document) = self.document.as_mut()
                        && let Some(note) = self.notes.get(document.note_index())
                        && let Ok((_file, version)) = open_versioned(&note.path)
                    {
                        document.file_version = Some(version);
                    }
                }
                let partial_note_operation = matches!(
                    &error,
                    CoreError::Operation(NoteOperationError::PartialCommit { .. })
                );
                let partial_metadata_save =
                    matches!(&error, CoreError::Save(SaveError::PostReplaceSync { .. }));
                if partial_note_operation
                    && matches!(
                        completion.kind,
                        SecureOperationKind::Protect
                            | SecureOperationKind::DisableProtection
                            | SecureOperationKind::Metadata
                    )
                {
                    self.document = None;
                    self.selected_note = None;
                    repair_workspace(&self.root)
                        .map_err(|repair| CoreError::Workspace(repair.to_string()))?;
                    self.refresh_notes()?;
                } else if partial_metadata_save && completion.kind == SecureOperationKind::Metadata
                {
                    self.document = None;
                    self.selected_note = None;
                    self.refresh_notes()?;
                }
                return Err(error);
            }
        };

        match result {
            SecureJobResult::Loaded {
                note_index,
                path,
                password,
                mut document,
                projection,
                purpose,
            } => {
                let note = self.notes.get(note_index).ok_or_else(|| {
                    CoreError::NoteUnavailable("secure load target disappeared".to_owned())
                })?;
                if note.path != path || note.protection != NoteProtection::Protected {
                    return Err(CoreError::NoteUnavailable(
                        "secure load target changed before completion".to_owned(),
                    ));
                }
                if matches!(
                    purpose,
                    ProtectedLoadPurpose::Unlock {
                        adopt_password: true
                    }
                ) {
                    let vault_id = match self.security_state {
                        WorkspaceSecurityState::LegacyLocked
                        | WorkspaceSecurityState::Unconfigured => {
                            self.security_store.configure(&password)?
                        }
                        WorkspaceSecurityState::ConfiguredLocked
                        | WorkspaceSecurityState::Unlocked => {
                            self.security_store.unlock(&password)?
                        }
                        WorkspaceSecurityState::Blocked => {
                            return Err(CoreError::Security(SecurityError::Blocked(
                                "workspace security is blocked".to_owned(),
                            )));
                        }
                    };
                    self.vault_id = Some(vault_id);
                    self.security_state = WorkspaceSecurityState::Unlocked;
                }
                let previously_open_protected = self.document.as_ref().and_then(|current| {
                    (current.note_index() != note_index && current.is_protected())
                        .then_some(current.note_index())
                });
                apply_protected_projection(
                    self.notes.get_mut(note_index).ok_or_else(|| {
                        CoreError::NoteUnavailable("secure load target disappeared".to_owned())
                    })?,
                    &projection,
                );
                if let Some(previous_index) = previously_open_protected
                    && let Some(previous) = self.notes.get_mut(previous_index)
                {
                    redact_protected_projection(previous);
                }
                sort_notes(&mut self.notes);
                let current_index = self
                    .notes
                    .iter()
                    .position(|note| note.path == path)
                    .ok_or_else(|| {
                        CoreError::NoteUnavailable(
                            "secure load target disappeared after sorting".to_owned(),
                        )
                    })?;
                document.note_index = current_index;
                document.target = DocumentTarget::WorkspaceNote(current_index);
                self.document = Some(*document);
                self.selected_note = Some(current_index);
                self.selected_external = None;
                self.selected_engine = None;
                self.refresh_catalog_categories();
                match purpose {
                    ProtectedLoadPurpose::Unlock { adopt_password } => {
                        if adopt_password {
                            self.master_password = Some(password);
                        }
                        Ok(SecureOutcome::Unlocked)
                    }
                    ProtectedLoadPurpose::ExternalReload => {
                        Ok(SecureOutcome::ExternalPoll(ExternalPoll::Reloaded))
                    }
                    ProtectedLoadPurpose::DiscardReload => {
                        if let Some(note) = self.notes.get_mut(current_index) {
                            note.recovery_available = false;
                        }
                        Ok(SecureOutcome::DiscardedAndReloaded)
                    }
                    ProtectedLoadPurpose::RestoreRecovery => Ok(SecureOutcome::RecoveryRestored),
                }
            }
            SecureJobResult::Protected { commit, password } => {
                if matches!(
                    self.security_state,
                    WorkspaceSecurityState::LegacyLocked | WorkspaceSecurityState::Unconfigured
                ) {
                    self.vault_id = Some(self.security_store.configure(&password)?);
                } else {
                    self.vault_id = Some(self.security_store.unlock(&password)?);
                }
                self.security_state = WorkspaceSecurityState::Unlocked;
                self.master_password = Some(password);
                self.document = None;
                self.selected_note = None;
                self.refresh_notes()?;
                let protected_index = self
                    .notes
                    .iter()
                    .position(|note| note.path == commit.path)
                    .ok_or_else(|| {
                        CoreError::NoteUnavailable(
                            "protected note was not found after commit".to_owned(),
                        )
                    })?;
                self.selected_note = Some(protected_index);
                self.selected_external = None;
                self.selected_engine = None;
                Ok(SecureOutcome::Protected(commit.path))
            }
            SecureJobResult::ProtectionDisabled {
                commit,
                cleanup_error,
            } => {
                if resolving_integrity {
                    self.pending_integrity = None;
                }
                self.document = None;
                self.selected_note = None;
                self.refresh_and_open(&commit.path)?;
                if let Some(error) = cleanup_error {
                    return Err(CoreError::Recovery(error));
                }
                Ok(SecureOutcome::ProtectionDisabled(commit.path))
            }
            SecureJobResult::MetadataChanged {
                note_index,
                path,
                commit,
                patch,
                next_frontmatter,
            } => {
                if resolving_integrity {
                    self.pending_integrity = None;
                }
                let deleted_change = patch.deleted;
                if self
                    .notes
                    .get(note_index)
                    .is_none_or(|note| note.path != path)
                {
                    return Err(CoreError::NoteUnavailable(
                        "protected metadata target changed before completion".to_owned(),
                    ));
                }
                if let Some(document) = self
                    .document
                    .as_mut()
                    .filter(|document| document.note_index() == note_index)
                {
                    document.file_version = Some(commit.version);
                    if let DocumentProtection::Protected(protected) = &mut document.protection {
                        protected.frontmatter = *next_frontmatter;
                    }
                    if let Some(title) = &patch.title {
                        document.title.clone_from(title);
                    }
                }
                let note = self.notes.get_mut(note_index).ok_or_else(|| {
                    CoreError::NoteUnavailable(
                        "selected note disappeared after protected rewrite".to_owned(),
                    )
                })?;
                if let Some(title) = patch.title {
                    note.title = title;
                }
                if let Some(tags) = patch.tags {
                    note.tags = tags;
                }
                if let Some(pinned) = patch.pinned {
                    note.pinned = pinned;
                }
                if let Some(favorited) = patch.favorited {
                    note.favorited = favorited;
                }
                if let Some(deleted) = patch.deleted {
                    note.deleted = deleted;
                }
                if let Some(modified) = patch.modified {
                    note.modified = Some(modified);
                }
                if let Some(order) = patch.order {
                    note.order = order;
                }
                note.path.clone_from(&commit.path);
                sort_notes(&mut self.notes);
                let current_index = self
                    .notes
                    .iter()
                    .position(|note| note.path == commit.path)
                    .ok_or_else(|| {
                        CoreError::NoteUnavailable(
                            "protected metadata target disappeared after sorting".to_owned(),
                        )
                    })?;
                if let Some(document) = self.document.as_mut() {
                    document.note_index = current_index;
                    document.target = DocumentTarget::WorkspaceNote(current_index);
                }
                self.selected_note = Some(current_index);
                self.selected_external = None;
                self.selected_engine = None;
                self.refresh_catalog_categories();
                if deleted_change == Some(true) {
                    self.document = None;
                    self.selected_note = None;
                }
                Ok(SecureOutcome::MetadataChanged)
            }
            SecureJobResult::IntegrityFailure { failure, retry } => {
                if let Some(index) = self
                    .notes
                    .iter()
                    .position(|note| note.path == failure.backup.source_path)
                {
                    self.notes[index].path.clone_from(&failure.commit.path);
                    if let Some(document) = self
                        .document
                        .as_mut()
                        .filter(|document| document.note_index() == index)
                    {
                        document.file_version = Some(failure.commit.version);
                    }
                }
                self.pending_integrity = Some(PendingIntegrity {
                    failure,
                    retry: Some(retry),
                });
                Ok(SecureOutcome::IntegrityFailure)
            }
            SecureJobResult::IntegrityRestored { commit } => {
                self.pending_integrity = None;
                if let Ok(key) = self.recovery_store.key_for_note(&commit.path) {
                    self.recovery_store.remove_protected(&key)?;
                }
                self.document = None;
                self.selected_note = None;
                self.refresh_notes()?;
                self.selected_note = self.notes.iter().position(|note| note.path == commit.path);
                Ok(SecureOutcome::IntegrityRestored(commit.path))
            }
            SecureJobResult::IntegrityAutosave(completion) => {
                let cleanup_succeeded =
                    completion.result.is_ok() && completion.cleanup_error.is_none();
                self.pending_integrity = None;
                if let Some(document) = self
                    .document
                    .as_mut()
                    .filter(|document| document.note_index() == completion.note_index)
                {
                    document.autosave.saving_revision = Some(completion.revision);
                }
                self.finish_autosave(completion)?;
                if self.pending_integrity.is_some() {
                    Ok(SecureOutcome::IntegrityFailure)
                } else {
                    if cleanup_succeeded
                        && let Some(index) = self.document.as_ref().map(DocumentSession::note_index)
                        && let Some(note) = self.notes.get_mut(index)
                    {
                        note.recovery_available = false;
                    }
                    Ok(SecureOutcome::IntegrityRetried)
                }
            }
            SecureJobResult::MasterPasswordChanged { commit, password } => {
                self.master_password = Some(password.clone());
                self.security_state = WorkspaceSecurityState::Unlocked;
                if let Some(document) = self.document.as_mut()
                    && document.is_protected()
                    && let Some(note) = self.notes.get(document.note_index())
                    && let Some((_, version)) = commit
                        .note_versions
                        .iter()
                        .find(|(path, _)| path == &note.path)
                {
                    document.file_version = Some(*version);
                    if let DocumentProtection::Protected(protected) = &mut document.protection {
                        protected.password = password;
                    }
                }
                for engine in &mut self.engines {
                    engine.security_rotated();
                    engine.resume();
                }
                Ok(SecureOutcome::MasterPasswordChanged {
                    notes: commit.note_count,
                    recovery: commit.recovery_count,
                    secrets: commit.secret_count,
                })
            }
        }
    }

    fn selected_operation_target(&self) -> Result<(usize, PathBuf, FileVersion), CoreError> {
        self.ensure_workspace_action_ready()?;
        let note_index = self
            .selected_note
            .ok_or_else(|| CoreError::NoteUnavailable("no note is selected".to_owned()))?;
        let note = self
            .notes
            .get(note_index)
            .ok_or_else(|| CoreError::NoteUnavailable("selected note disappeared".to_owned()))?;
        if note.recovery_available {
            return Err(CoreError::UnsavedChanges);
        }
        let version = if let Some(document) = self
            .document
            .as_ref()
            .filter(|document| document.note_index() == note_index)
        {
            document.file_version.ok_or_else(|| {
                CoreError::NoteUnavailable("selected note has no filesystem version".to_owned())
            })?
        } else if note.protection == NoteProtection::Protected {
            open_versioned(&note.path)?.1
        } else {
            return Err(CoreError::NoteUnavailable(
                "selected note is not open".to_owned(),
            ));
        };
        Ok((note_index, note.path.clone(), version))
    }

    fn ensure_workspace_action_ready(&self) -> Result<(), CoreError> {
        if self.pending_secure_operation.is_some()
            || self.pending_integrity.is_some()
            || self.password_change_recovery_blocked
            || self
                .document
                .as_ref()
                .is_some_and(DocumentSession::operation_blocked)
        {
            return Err(CoreError::UnsavedChanges);
        }
        Ok(())
    }

    fn refresh_and_open(&mut self, path: &Path) -> Result<usize, CoreError> {
        self.refresh_and_open_for_operation(path, false)
    }

    fn refresh_and_open_for_operation(
        &mut self,
        path: &Path,
        diagnose_delete: bool,
    ) -> Result<usize, CoreError> {
        delete_diagnostics::result(diagnose_delete, "RefreshScan", self.refresh_notes())?;
        let note_index = delete_diagnostics::result(
            diagnose_delete,
            "RefreshFind",
            self.notes
                .iter()
                .position(|note| note.path == path)
                .ok_or_else(|| {
                    CoreError::NoteUnavailable(format!(
                        "committed note {} was not found by workspace scan",
                        path.display()
                    ))
                }),
        )?;
        let note = &self.notes[note_index];
        self.document = Some(delete_diagnostics::result(
            diagnose_delete,
            "RefreshOpen",
            load_document(note_index, &note.title, &note.path),
        )?);
        self.selected_note = Some(note_index);
        self.selected_external = None;
        self.selected_engine = None;
        Ok(note_index)
    }

    fn refresh_catalog_categories(&mut self) {
        let mut counts = BTreeMap::<String, usize>::new();
        for note in self
            .notes
            .iter()
            .filter(|note| note.availability.is_ready() && !note.deleted)
        {
            for category in &note.tags {
                *counts.entry(category.clone()).or_default() += 1;
            }
        }
        for item in self
            .non_document_items()
            .iter()
            .filter(|i| !i.metadata.deleted)
        {
            for category in &item.metadata.categories {
                *counts.entry(category.clone()).or_default() += 1;
            }
        }
        self.categories = counts
            .into_iter()
            .map(|(name, note_count)| CategorySummary { name, note_count })
            .collect();
    }

    fn refresh_notes(&mut self) -> Result<(), CoreError> {
        let scan =
            scan_workspace(&self.root).map_err(|error| CoreError::Workspace(error.to_string()))?;
        let mut notes = scan
            .notes
            .into_iter()
            .map(|note| note_summary(note.path, note.result))
            .collect::<Vec<_>>();
        let recovery_scan = self.recovery_store.scan();
        let protected_recovery_scan = self.recovery_store.scan_protected();
        for note in &mut notes {
            if let Ok(key) = self.recovery_store.key_for_note(&note.path) {
                note.recovery_available = if note.protection == NoteProtection::Protected {
                    self.recovery_store.protected_exists(&key).unwrap_or(false)
                } else {
                    recovery_scan.records.iter().any(|record| record.key == key)
                };
            }
        }
        sort_notes(&mut notes);
        self.notes = notes;
        self.refresh_catalog_categories();
        self.recovery_diagnostics = recovery_scan
            .diagnostics
            .into_iter()
            .chain(protected_recovery_scan.diagnostics)
            .collect();
        Ok(())
    }
}

fn load_document(
    note_index: usize,
    title: &str,
    path: &Path,
) -> Result<DocumentSession, CoreError> {
    let (mut file, file_version) = open_versioned(path)?;
    let scan =
        scan_reader(&mut file).map_err(|error| CoreError::NoteUnavailable(error.to_string()))?;
    let body_offset = match scan.status {
        FrontMatterStatus::Plain => 0,
        FrontMatterStatus::Parsed(parsed) => parsed.body_offset,
        FrontMatterStatus::Invalid { issue, .. } => {
            return Err(CoreError::NoteUnavailable(issue.to_string()));
        }
    };
    file.seek(SeekFrom::Start(body_offset))
        .map_err(|error| CoreError::NoteUnavailable(error.to_string()))?;
    DocumentSession::from_versioned_reader(note_index, title.to_owned(), file, file_version)
}

#[derive(Clone)]
struct ProtectedProjection {
    title: String,
    tags: Vec<String>,
    pinned: bool,
    favorited: bool,
    deleted: bool,
    created: Option<String>,
    modified: Option<String>,
    order: BTreeMap<String, u32>,
}

fn load_protected_document(
    note_index: usize,
    path: &Path,
    password: &MasterPassword,
) -> Result<(DocumentSession, ProtectedProjection), CoreError> {
    let (mut file, file_version) = open_versioned(path)?;
    let frontmatter = scan_reader(&mut file).map_err(|_| CoreError::Secure(SecureError))?;
    let body_offset = match &frontmatter.status {
        FrontMatterStatus::Parsed(parsed) if parsed.metadata.encryption.is_some() => {
            parsed.body_offset
        }
        _ => return Err(CoreError::Secure(SecureError)),
    };
    let (_, projection) = protected_projection(&frontmatter, path)?;
    file.seek(SeekFrom::Start(body_offset))
        .map_err(|_| CoreError::Secure(SecureError))?;
    let reader = decrypt_body(file, password)?;
    let mut document = DocumentSession::from_versioned_reader(
        note_index,
        projection.title.clone(),
        reader,
        file_version,
    )
    .map_err(|_| CoreError::Secure(SecureError))?;
    document.protection = DocumentProtection::Protected(Box::new(ProtectedDocument {
        password: password.clone(),
        frontmatter,
    }));
    document.refresh_title_from_body();
    let mut projection = projection;
    projection.title = document.title().to_owned();
    Ok((document, projection))
}

fn authenticate_protected_note(path: &Path, password: &MasterPassword) -> Result<(), CoreError> {
    let (mut file, _) = open_versioned(path)?;
    let scan = scan_reader(&mut file).map_err(|_| CoreError::Secure(SecureError))?;
    let body_offset = match scan.status {
        FrontMatterStatus::Parsed(parsed) if parsed.metadata.encryption.is_some() => {
            parsed.body_offset
        }
        _ => return Err(CoreError::Secure(SecureError)),
    };
    file.seek(SeekFrom::Start(body_offset))
        .map_err(|_| CoreError::Secure(SecureError))?;
    let mut decrypted = decrypt_body(file, password)?;
    std::io::copy(&mut decrypted, &mut std::io::sink())
        .map_err(|_| CoreError::Secure(SecureError))?;
    Ok(())
}

fn protected_projection(
    frontmatter: &FrontMatterScan,
    path: &Path,
) -> Result<(u64, ProtectedProjection), CoreError> {
    let fallback_title = path
        .file_stem()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Untitled".to_owned());
    match &frontmatter.status {
        FrontMatterStatus::Parsed(parsed) => Ok((
            parsed.body_offset,
            ProtectedProjection {
                title: parsed.metadata.title.clone().unwrap_or(fallback_title),
                tags: parsed.metadata.tags.clone(),
                pinned: parsed.metadata.pinned.unwrap_or(false),
                favorited: parsed.metadata.favorited.unwrap_or(false),
                deleted: parsed.metadata.deleted.unwrap_or(false),
                created: parsed.metadata.created.clone(),
                modified: parsed.metadata.modified.clone(),
                order: parsed.metadata.order.clone(),
            },
        )),
        FrontMatterStatus::Plain => Ok((
            0,
            ProtectedProjection {
                title: fallback_title,
                tags: Vec::new(),
                pinned: false,
                favorited: false,
                deleted: false,
                created: None,
                modified: None,
                order: BTreeMap::new(),
            },
        )),
        FrontMatterStatus::Invalid { .. } => Err(CoreError::Secure(SecureError)),
    }
}

fn apply_protected_projection(note: &mut NoteSummary, projection: &ProtectedProjection) {
    note.title.clone_from(&projection.title);
    note.tags.clone_from(&projection.tags);
    note.pinned = projection.pinned;
    note.favorited = projection.favorited;
    note.deleted = projection.deleted;
    note.created.clone_from(&projection.created);
    note.modified.clone_from(&projection.modified);
    note.order.clone_from(&projection.order);
}

fn redact_protected_projection(note: &mut NoteSummary) {
    debug_assert_eq!(note.protection, NoteProtection::Protected);
    let recovery_available = note.recovery_available;
    if let Ok(result) = scan_note(&note.path) {
        let refreshed = note_summary(note.path.clone(), result);
        if refreshed.protection == NoteProtection::Protected && refreshed.availability.is_ready() {
            *note = refreshed;
            note.recovery_available = recovery_available;
        }
    }
}

fn note_summary(path: PathBuf, result: NoteScanResult) -> NoteSummary {
    let fallback_title = path
        .file_stem()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Untitled".to_owned());
    match result {
        NoteScanResult::Protected(scan) => match scan.frontmatter.status {
            FrontMatterStatus::Parsed(parsed) => NoteSummary {
                title: parsed.metadata.title.unwrap_or(fallback_title),
                tags: parsed.metadata.tags,
                pinned: parsed.metadata.pinned.unwrap_or(false),
                favorited: parsed.metadata.favorited.unwrap_or(false),
                deleted: parsed.metadata.deleted.unwrap_or(false),
                created: parsed.metadata.created,
                modified: parsed.metadata.modified,
                order: parsed.metadata.order,
                recovery_available: false,
                path,
                availability: NoteAvailability::Ready,
                protection: NoteProtection::Protected,
                body_offset: Some(scan.body_offset),
            },
            _ => unreachable!("protected scan always contains parsed front matter"),
        },
        NoteScanResult::LegacyProtected => NoteSummary {
            path,
            title: "Неподдерживаемая защищённая заметка".to_owned(),
            tags: Vec::new(),
            pinned: false,
            favorited: false,
            deleted: false,
            created: None,
            modified: None,
            order: BTreeMap::new(),
            recovery_available: false,
            availability: NoteAvailability::IoError(
                "unsupported legacy protected format".to_owned(),
            ),
            protection: NoteProtection::Protected,
            body_offset: None,
        },
        NoteScanResult::InvalidProtected(message) => NoteSummary {
            path,
            title: fallback_title,
            tags: Vec::new(),
            pinned: false,
            favorited: false,
            deleted: false,
            created: None,
            modified: None,
            order: BTreeMap::new(),
            recovery_available: false,
            availability: NoteAvailability::IoError(message),
            protection: NoteProtection::Protected,
            body_offset: None,
        },
        NoteScanResult::Scanned(scan) => match scan.frontmatter.status {
            FrontMatterStatus::Parsed(parsed) => NoteSummary {
                title: scan
                    .body_title
                    .unwrap_or_else(|| parsed.metadata.title.clone().unwrap_or(fallback_title)),
                tags: parsed.metadata.tags,
                pinned: parsed.metadata.pinned.unwrap_or(false),
                favorited: parsed.metadata.favorited.unwrap_or(false),
                deleted: parsed.metadata.deleted.unwrap_or(false),
                created: parsed.metadata.created,
                modified: parsed.metadata.modified,
                order: parsed.metadata.order,
                recovery_available: false,
                path,
                availability: NoteAvailability::Ready,
                protection: NoteProtection::Plain,
                body_offset: Some(parsed.body_offset),
            },
            FrontMatterStatus::Plain => NoteSummary {
                path,
                title: scan.body_title.unwrap_or(fallback_title),
                tags: Vec::new(),
                pinned: false,
                favorited: false,
                deleted: false,
                created: None,
                modified: None,
                order: BTreeMap::new(),
                recovery_available: false,
                availability: NoteAvailability::Ready,
                protection: NoteProtection::Plain,
                body_offset: Some(0),
            },
            FrontMatterStatus::Invalid { issue, .. } => NoteSummary {
                path,
                title: fallback_title,
                tags: Vec::new(),
                pinned: false,
                favorited: false,
                deleted: false,
                created: None,
                modified: None,
                order: BTreeMap::new(),
                recovery_available: false,
                availability: NoteAvailability::InvalidMetadata(issue.to_string()),
                protection: NoteProtection::Plain,
                body_offset: None,
            },
        },
        NoteScanResult::IoError(message) => NoteSummary {
            path,
            title: fallback_title,
            tags: Vec::new(),
            pinned: false,
            favorited: false,
            deleted: false,
            created: None,
            modified: None,
            order: BTreeMap::new(),
            recovery_available: false,
            availability: NoteAvailability::IoError(message),
            protection: NoteProtection::Plain,
            body_offset: None,
        },
    }
}

fn sort_notes(notes: &mut [NoteSummary]) {
    notes.sort_by(|left, right| {
        right
            .pinned
            .cmp(&left.pinned)
            .then_with(|| left.title.to_lowercase().cmp(&right.title.to_lowercase()))
            .then_with(|| left.path.cmp(&right.path))
    });
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SaveStatus {
    Clean {
        revision: u64,
    },
    Dirty {
        revision: u64,
        deadline_ms: u64,
    },
    Saving {
        revision: u64,
        dirty_after_start: bool,
    },
    Error {
        revision: u64,
        message: String,
    },
    Conflict {
        revision: u64,
        message: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryStatus {
    None,
    Pending { revision: u64 },
    Saving { revision: u64 },
    Saved { revision: u64 },
    Error { revision: u64, message: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExternalPoll {
    Unchanged,
    Deferred,
    Reloaded,
    Conflict,
}

pub enum ExternalPollStart {
    Immediate(ExternalPoll),
    Secure(SecureJob),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecureOutcome {
    Unlocked,
    Protected(PathBuf),
    ProtectionDisabled(PathBuf),
    MetadataChanged,
    ExternalPoll(ExternalPoll),
    DiscardedAndReloaded,
    RecoveryRestored,
    IntegrityFailure,
    IntegrityRetried,
    IntegrityRestored(PathBuf),
    MasterPasswordChanged {
        notes: usize,
        recovery: usize,
        secrets: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecurePhase {
    Validating,
    PreparingVerifier,
    PreparingSecrets,
    PreparingNotes,
    PreparingRecovery,
    BackingUpNotes,
    BackingUpSecrets,
    ReplacingRecovery,
    ReplacingSecrets,
    ReplacingNotes,
    ReplacingVerifier,
    Verifying,
    RollingBack,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecureProgress {
    pub operation_id: u64,
    pub phase: SecurePhase,
    pub completed: usize,
    pub total: usize,
    pub percent: Option<u8>,
}

pub enum SecureWorkerEvent {
    Progress(SecureProgress),
    Completed(Box<SecureCompletion>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SecureOperationKind {
    Unlock,
    Protect,
    DisableProtection,
    Metadata,
    ExternalReload,
    DiscardReload,
    RestoreRecovery,
    Integrity,
    ChangeMasterPassword,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingSecureOperation {
    id: u64,
    kind: SecureOperationKind,
}

struct PendingIntegrity {
    failure: Box<IntegrityFailure>,
    retry: Option<IntegrityRetry>,
}

#[derive(Clone)]
enum IntegrityRetry {
    Autosave(Box<SaveJob>),
    DisableProtection(Box<DisableProtectionJob>),
    Metadata(Box<ProtectedMetadataJob>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrityResolution {
    Retry,
    Restore,
}

#[derive(Clone)]
pub struct SaveJob {
    target: DocumentTarget,
    revision: u64,
    path: PathBuf,
    workspace: PathBuf,
    title: String,
    expected_version: FileVersion,
    modified: String,
    snapshot: EditorSnapshot,
    snapshot_checksum: u64,
    recovery_store: RecoveryStore,
    recovery_key: RecoveryKey,
    protected: Option<ProtectedDocument>,
}

impl SaveJob {
    pub fn note_index(&self) -> usize {
        match &self.target {
            DocumentTarget::WorkspaceNote(index) => *index,
            DocumentTarget::ExternalFile { .. } => usize::MAX,
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn len_bytes(&self) -> usize {
        self.snapshot.len_bytes()
    }

    pub fn execute(self) -> SaveCompletion {
        let note_index = self.note_index();
        let retry_job = Box::new(self.clone());
        let snapshot = self.snapshot;
        let patch = MetadataPatch {
            title: Some(self.title.clone()),
            modified: Some(self.modified),
            ..MetadataPatch::default()
        };
        let (result, integrity_failure, protected_frontmatter) =
            if matches!(self.target, DocumentTarget::ExternalFile { .. }) {
                (
                    rewrite_external_file_versioned(
                        &self.path,
                        &self.expected_version,
                        move |writer| snapshot.write_to(writer),
                    ),
                    None,
                    None,
                )
            } else if let Some(protected) = &self.protected {
                match patch_front_matter(&protected.frontmatter, &patch) {
                    Ok(Some(rewrite)) => match scan_reader(Cursor::new(&rewrite.prefix)) {
                        Ok(scan) => {
                            let verified = rewrite_protected_body_with_title(
                                &self.workspace,
                                &self.path,
                                &self.expected_version,
                                ProtectedBodyRewrite {
                                    password: &protected.password,
                                    patch: &patch,
                                    title: &self.title,
                                    body_len: snapshot.len_bytes() as u64,
                                },
                                move |writer| snapshot.write_to(writer),
                            );
                            match verified {
                                Ok(VerifiedSave::Verified(commit)) => {
                                    (Ok(commit), None, Some(scan))
                                }
                                Ok(VerifiedSave::IntegrityFailure(failure)) => (
                                    Err(SaveError::InvalidTarget(failure.message.clone())),
                                    Some(failure),
                                    Some(scan),
                                ),
                                Err(error) => (Err(error), None, None),
                            }
                        }
                        Err(error) => (
                            Err(SaveError::InvalidTarget(format!(
                                "protected metadata rewrite could not be scanned: {error}"
                            ))),
                            None,
                            None,
                        ),
                    },
                    Ok(None) => (
                        Err(SaveError::InvalidTarget(
                            "protected autosave produced no metadata rewrite".to_owned(),
                        )),
                        None,
                        None,
                    ),
                    Err(error) => (Err(SaveError::Patch(error)), None, None),
                }
            } else {
                (
                    rewrite_note_with_title(
                        &self.workspace,
                        &self.path,
                        &self.expected_version,
                        &patch,
                        &self.title,
                        move |writer| snapshot.write_to(writer),
                    ),
                    None,
                    None,
                )
            };
        let cleanup_error = result.as_ref().ok().and_then(|_| {
            let removal = if let Some(protected) = &self.protected {
                self.recovery_store.remove_protected_saved(
                    &self.recovery_key,
                    &protected.password,
                    self.revision,
                )
            } else {
                self.recovery_store
                    .remove_saved(&self.recovery_key, self.revision)
            };
            removal.err().map(|error| error.to_string())
        });
        SaveCompletion {
            note_index,
            target: self.target,
            revision: self.revision,
            result,
            saved_checksum: self.snapshot_checksum,
            cleanup_error,
            protected_frontmatter: protected_frontmatter.map(Box::new),
            retry_job: integrity_failure.as_ref().map(|_| retry_job),
            integrity_failure,
        }
    }
}

pub struct SaveCompletion {
    note_index: usize,
    target: DocumentTarget,
    revision: u64,
    result: Result<SaveCommit, SaveError>,
    saved_checksum: u64,
    cleanup_error: Option<String>,
    protected_frontmatter: Option<Box<FrontMatterScan>>,
    retry_job: Option<Box<SaveJob>>,
    integrity_failure: Option<Box<IntegrityFailure>>,
}

pub struct RecoveryJob {
    target: DocumentTarget,
    revision: u64,
    store: RecoveryStore,
    key: RecoveryKey,
    base_checksum: u64,
    snapshot: EditorSnapshot,
    protected: Option<ProtectedDocument>,
}

impl RecoveryJob {
    pub fn execute(self) -> RecoveryCompletion {
        let body_len = self.snapshot.len_bytes() as u64;
        let body_checksum = self.snapshot.checksum_fnv1a();
        let snapshot = self.snapshot;
        let result = if let Some(protected) = &self.protected {
            (|| {
                let _operation = stillus_platform::OperationLock::directory(self.store.workspace())
                    .map_err(|error| RecoveryError::Io(error.to_string()))?;
                let security = SecurityStore::new(self.store.workspace());
                let catalog = security
                    .inspect(true)
                    .map_err(|error| RecoveryError::Io(error.to_string()))?;
                if catalog.verifier.is_some() {
                    security.unlock(&protected.password).map_err(|_| {
                        RecoveryError::InvalidArtifact(
                            "workspace password changed; old-password recovery was not written"
                                .to_owned(),
                        )
                    })?;
                }
                self.store.write_protected(
                    &self.key,
                    &protected.password,
                    self.revision,
                    self.base_checksum,
                    body_len,
                    body_checksum,
                    move |writer| snapshot.write_to(writer),
                )
            })()
        } else {
            self.store.write(
                &self.key,
                self.revision,
                self.base_checksum,
                body_len,
                body_checksum,
                move |writer| snapshot.write_to(writer),
            )
        };
        RecoveryCompletion {
            target: self.target,
            revision: self.revision,
            result,
        }
    }
}

pub struct RecoveryCompletion {
    target: DocumentTarget,
    revision: u64,
    result: Result<RecoveryRecord, RecoveryError>,
}

pub enum PersistenceJob {
    Recovery(RecoveryJob),
    Save(SaveJob),
}

impl PersistenceJob {
    pub fn execute(self) -> PersistenceCompletion {
        match self {
            Self::Recovery(job) => PersistenceCompletion::Recovery(job.execute()),
            Self::Save(job) => PersistenceCompletion::Save(job.execute()),
        }
    }
}

pub enum PersistenceCompletion {
    Recovery(RecoveryCompletion),
    Save(SaveCompletion),
}

impl PersistenceCompletion {
    pub fn canonical_verified(&self) -> bool {
        matches!(self, Self::Save(completion) if completion.result.is_ok())
    }
}

pub struct SecureJob {
    operation_id: u64,
    kind: SecureOperationKind,
    task: Box<SecureJobTask>,
}

impl SecureJob {
    pub fn operation_id(&self) -> u64 {
        self.operation_id
    }

    pub fn execute(self) -> SecureCompletion {
        self.execute_with_progress(|_| {})
    }

    pub fn execute_with_progress(self, mut emit: impl FnMut(SecureProgress)) -> SecureCompletion {
        let operation_id = self.operation_id;
        let result = match *self.task {
            SecureJobTask::Load(job) => job.execute(),
            SecureJobTask::Protect(job) => job.execute(),
            SecureJobTask::DisableProtection(job) => job.execute(),
            SecureJobTask::Metadata(job) => (*job).execute(),
            SecureJobTask::RestoreRecovery(job) => job.execute(),
            SecureJobTask::ResolveIntegrity(job) => (*job).execute(),
            SecureJobTask::ChangeMasterPassword(job) => (*job).execute(|progress| {
                emit(SecureProgress {
                    operation_id,
                    phase: match progress.phase {
                        PasswordChangePhase::Validating => SecurePhase::Validating,
                        PasswordChangePhase::PreparingVerifier => SecurePhase::PreparingVerifier,
                        PasswordChangePhase::PreparingSecrets => SecurePhase::PreparingSecrets,
                        PasswordChangePhase::PreparingNotes => SecurePhase::PreparingNotes,
                        PasswordChangePhase::PreparingRecovery => SecurePhase::PreparingRecovery,
                        PasswordChangePhase::BackingUpNotes => SecurePhase::BackingUpNotes,
                        PasswordChangePhase::BackingUpSecrets => SecurePhase::BackingUpSecrets,
                        PasswordChangePhase::ReplacingRecovery => SecurePhase::ReplacingRecovery,
                        PasswordChangePhase::ReplacingSecrets => SecurePhase::ReplacingSecrets,
                        PasswordChangePhase::ReplacingNotes => SecurePhase::ReplacingNotes,
                        PasswordChangePhase::ReplacingVerifier => SecurePhase::ReplacingVerifier,
                        PasswordChangePhase::Verifying => SecurePhase::Verifying,
                        PasswordChangePhase::RollingBack => SecurePhase::RollingBack,
                    },
                    completed: progress.completed,
                    total: progress.total,
                    percent: progress.percent,
                });
            }),
        };
        SecureCompletion {
            operation_id: self.operation_id,
            kind: self.kind,
            result,
        }
    }
}

pub struct SecureCompletion {
    operation_id: u64,
    kind: SecureOperationKind,
    result: Result<SecureJobResult, CoreError>,
}

impl SecureCompletion {
    pub fn operation_id(&self) -> u64 {
        self.operation_id
    }
}

enum SecureJobTask {
    Load(LoadProtectedJob),
    Protect(ProtectJob),
    DisableProtection(DisableProtectionJob),
    Metadata(Box<ProtectedMetadataJob>),
    RestoreRecovery(RestoreProtectedRecoveryJob),
    ResolveIntegrity(Box<IntegrityResolutionJob>),
    ChangeMasterPassword(Box<ChangeMasterPasswordJob>),
}

enum SecureJobResult {
    Loaded {
        note_index: usize,
        path: PathBuf,
        password: MasterPassword,
        document: Box<DocumentSession>,
        projection: ProtectedProjection,
        purpose: ProtectedLoadPurpose,
    },
    Protected {
        commit: SaveCommit,
        password: MasterPassword,
    },
    ProtectionDisabled {
        commit: SaveCommit,
        cleanup_error: Option<RecoveryError>,
    },
    MetadataChanged {
        note_index: usize,
        path: PathBuf,
        commit: SaveCommit,
        patch: MetadataPatch,
        next_frontmatter: Box<FrontMatterScan>,
    },
    IntegrityFailure {
        failure: Box<IntegrityFailure>,
        retry: IntegrityRetry,
    },
    IntegrityRestored {
        commit: SaveCommit,
    },
    IntegrityAutosave(SaveCompletion),
    MasterPasswordChanged {
        commit: PasswordChangeCommit,
        password: MasterPassword,
    },
}

struct ChangeMasterPasswordJob {
    workspace: PathBuf,
    verifier: PasswordChangeTarget,
    secrets: Vec<PasswordChangeTarget>,
    secret_catalog: Vec<PathBuf>,
    notes: Vec<PasswordChangeTarget>,
    recovery_paths: Vec<PathBuf>,
    current: MasterPassword,
    new: MasterPassword,
}

impl ChangeMasterPasswordJob {
    fn execute(
        self,
        progress: impl FnMut(stillus_storage::PasswordChangeProgress),
    ) -> Result<SecureJobResult, CoreError> {
        let _operation = stillus_platform::OperationLock::directory(&self.workspace)
            .map_err(|error| CoreError::Workspace(error.to_string()))?;
        let recovery_store = RecoveryStore::new(&self.workspace);
        let mut current_notes = BTreeSet::new();
        for note in scan_workspace(&self.workspace)
            .map_err(|error| CoreError::Workspace(error.to_string()))?
            .notes
        {
            match note.result {
                NoteScanResult::Protected(_) => {
                    current_notes.insert(note.path);
                }
                NoteScanResult::Scanned(_) => {}
                _ => {
                    return Err(CoreError::Workspace(
                        "workspace contains an unavailable note; password change was cancelled"
                            .to_owned(),
                    ));
                }
            }
        }
        let expected_notes = self.notes.iter().map(|note| note.path.clone()).collect();
        let current_recovery: BTreeSet<_> = recovery_store
            .protected_artifact_paths()?
            .into_iter()
            .collect();
        let expected_recovery = self.recovery_paths.iter().cloned().collect();
        let current_secrets: BTreeSet<_> = SecurityStore::new(&self.workspace)
            .inspect(true)?
            .secrets
            .into_iter()
            .collect();
        let expected_secrets = self.secret_catalog.iter().cloned().collect();
        if current_notes != expected_notes
            || current_recovery != expected_recovery
            || current_secrets != expected_secrets
        {
            return Err(CoreError::Workspace(
                "encrypted workspace contents changed in another window; reload before changing the password"
                    .to_owned(),
            ));
        }
        for path in &self.recovery_paths {
            recovery_store.validate_protected_artifact(path, &self.current)?;
        }
        let commit = rotate_workspace_security(
            &self.workspace,
            SecurityRotationTargets {
                verifier: Some(&self.verifier),
                secrets: &self.secrets,
                notes: &self.notes,
                recovery: &self.recovery_paths,
            },
            &self.current,
            &self.new,
            progress,
        )?;
        Ok(SecureJobResult::MasterPasswordChanged {
            commit,
            password: self.new,
        })
    }
}

#[derive(Clone, Copy)]
enum ProtectedLoadPurpose {
    Unlock { adopt_password: bool },
    ExternalReload,
    DiscardReload,
    RestoreRecovery,
}

struct LoadProtectedJob {
    note_index: usize,
    path: PathBuf,
    password: MasterPassword,
    purpose: ProtectedLoadPurpose,
    recovery_cleanup: Option<(RecoveryStore, RecoveryKey)>,
}

impl LoadProtectedJob {
    fn execute(self) -> Result<SecureJobResult, CoreError> {
        let (document, projection) =
            load_protected_document(self.note_index, &self.path, &self.password)?;
        if let Some((store, key)) = self.recovery_cleanup {
            store.remove_protected(&key)?;
        }
        Ok(SecureJobResult::Loaded {
            note_index: self.note_index,
            path: self.path,
            password: self.password,
            document: Box::new(document),
            projection,
            purpose: self.purpose,
        })
    }
}

struct RestoreProtectedRecoveryJob {
    note_index: usize,
    path: PathBuf,
    password: MasterPassword,
    store: RecoveryStore,
    key: RecoveryKey,
    now_ms: u64,
}

impl RestoreProtectedRecoveryJob {
    fn execute(self) -> Result<SecureJobResult, CoreError> {
        let artifact = self.store.open_protected(&self.key, &self.password)?;
        let record = artifact.record;
        if record.revision == 0 {
            return Err(CoreError::Recovery(RecoveryError::InvalidArtifact(
                "recovery revision must be positive".to_owned(),
            )));
        }
        let (mut document, projection) =
            load_protected_document(self.note_index, &self.path, &self.password)?;
        let recovered = Editor::from_reader(artifact.body.take(record.body_len))
            .map_err(|_| CoreError::Secure(SecureError))?;
        if recovered.len_bytes() as u64 != record.body_len
            || recovered.checksum_fnv1a() != record.body_checksum
        {
            return Err(CoreError::Recovery(RecoveryError::InvalidArtifact(
                "recovery body length/checksum mismatch".to_owned(),
            )));
        }
        let conflict = document.disk_checksum != record.base_checksum;
        document.editor = recovered;
        document.autosave.revision = record.revision;
        document.autosave.saved_revision = 0;
        document.autosave.recovery_revision = record.revision;
        document.autosave.deadline_ms =
            (!conflict).then_some(self.now_ms.saturating_add(AUTOSAVE_DEBOUNCE_MS));
        document.autosave.external_conflict = conflict.then(|| {
            "disk note changed since this recovery snapshot; both versions are preserved".to_owned()
        });
        Ok(SecureJobResult::Loaded {
            note_index: self.note_index,
            path: self.path,
            password: self.password,
            document: Box::new(document),
            projection,
            purpose: ProtectedLoadPurpose::RestoreRecovery,
        })
    }
}

struct ProtectJob {
    path: PathBuf,
    version: FileVersion,
    title: String,
    password: MasterPassword,
    authentication_path: Option<PathBuf>,
    recovery_store: RecoveryStore,
    recovery_key: RecoveryKey,
}

impl ProtectJob {
    fn execute(self) -> Result<SecureJobResult, CoreError> {
        let _operation = stillus_platform::OperationLock::file(&self.path)
            .map_err(|error| CoreError::Workspace(error.to_string()))?;
        if let Some(authentication_path) = self.authentication_path {
            authenticate_protected_note(&authentication_path, &self.password)?;
        }
        if let Some(workspace) = self.path.parent().and_then(Path::parent) {
            let security = SecurityStore::new(workspace);
            if security.inspect(true)?.verifier.is_some() {
                security.unlock(&self.password)?;
            }
        }
        self.recovery_store.remove(&self.recovery_key)?;
        let commit = protect_note_body(&self.path, &self.version, &self.password, &self.title)?;
        Ok(SecureJobResult::Protected {
            commit,
            password: self.password,
        })
    }
}

#[derive(Clone)]
struct DisableProtectionJob {
    workspace: PathBuf,
    path: PathBuf,
    version: FileVersion,
    password: MasterPassword,
    title: String,
    recovery_store: RecoveryStore,
    recovery_key: RecoveryKey,
}

impl DisableProtectionJob {
    fn execute(self) -> Result<SecureJobResult, CoreError> {
        let retry = self.clone();
        let verified = disable_body_protection(
            &self.workspace,
            &self.path,
            &self.version,
            &self.password,
            &self.title,
        )?;
        let commit = match verified {
            VerifiedSave::Verified(commit) => commit,
            VerifiedSave::IntegrityFailure(failure) => {
                return Ok(SecureJobResult::IntegrityFailure {
                    failure,
                    retry: IntegrityRetry::DisableProtection(Box::new(retry)),
                });
            }
        };
        let cleanup_error = self
            .recovery_store
            .remove_protected(&self.recovery_key)
            .err();
        Ok(SecureJobResult::ProtectionDisabled {
            commit,
            cleanup_error,
        })
    }
}

#[derive(Clone)]
struct ProtectedMetadataJob {
    note_index: usize,
    path: PathBuf,
    version: FileVersion,
    patch: MetadataPatch,
    next_frontmatter: FrontMatterScan,
    workspace: PathBuf,
    rename_title: Option<String>,
}

impl ProtectedMetadataJob {
    fn execute(self) -> Result<SecureJobResult, CoreError> {
        let retry = self.clone();
        let verified = rewrite_protected_metadata_versioned(
            &self.workspace,
            &self.path,
            &self.version,
            &self.patch,
            self.rename_title.as_deref(),
        )?;
        let commit = match verified {
            VerifiedSave::Verified(commit) => commit,
            VerifiedSave::IntegrityFailure(failure) => {
                return Ok(SecureJobResult::IntegrityFailure {
                    failure,
                    retry: IntegrityRetry::Metadata(Box::new(retry)),
                });
            }
        };
        Ok(SecureJobResult::MetadataChanged {
            note_index: self.note_index,
            path: self.path,
            commit,
            patch: self.patch,
            next_frontmatter: Box::new(self.next_frontmatter),
        })
    }
}

struct IntegrityResolutionJob {
    workspace: PathBuf,
    failure: IntegrityFailure,
    retry: Option<IntegrityRetry>,
    resolution: IntegrityResolution,
}

impl IntegrityResolutionJob {
    fn execute(self) -> Result<SecureJobResult, CoreError> {
        let _operation = stillus_platform::OperationLock::directory(&self.workspace)
            .map_err(|error| CoreError::Workspace(error.to_string()))?;
        let restored = restore_secure_backup(&self.workspace, &self.failure)?;
        if self.resolution == IntegrityResolution::Restore {
            return Ok(SecureJobResult::IntegrityRestored { commit: restored });
        }
        match self.retry.ok_or_else(|| {
            CoreError::NoteUnavailable(
                "retry is unavailable after restart; restore the verified backup".to_owned(),
            )
        })? {
            IntegrityRetry::Autosave(mut job) => {
                job.path = restored.path;
                job.expected_version = restored.version;
                Ok(SecureJobResult::IntegrityAutosave(job.execute()))
            }
            IntegrityRetry::DisableProtection(mut job) => {
                job.path = restored.path;
                job.version = restored.version;
                (*job).execute()
            }
            IntegrityRetry::Metadata(mut job) => {
                job.path = restored.path;
                job.version = restored.version;
                job.execute()
            }
        }
    }
}

#[derive(Default)]
struct AutosaveTracker {
    revision: u64,
    saved_revision: u64,
    deadline_ms: Option<u64>,
    saving_revision: Option<u64>,
    error: Option<SaveError>,
    recovery_revision: u64,
    recovery_deadline_ms: Option<u64>,
    recovery_saving_revision: Option<u64>,
    recovery_error: Option<RecoveryError>,
    external_conflict: Option<String>,
}

pub struct DocumentSession {
    target: DocumentTarget,
    note_index: usize,
    title: String,
    editor: Editor,
    file_version: Option<FileVersion>,
    disk_checksum: u64,
    autosave: AutosaveTracker,
    protection: DocumentProtection,
    undo_group: Option<UndoGroupState>,
}

#[derive(Clone, Copy)]
struct UndoGroupState {
    group: EditGroup,
    last_edit_ms: u64,
}

#[derive(Clone)]
struct ProtectedDocument {
    password: MasterPassword,
    frontmatter: FrontMatterScan,
}

#[derive(Clone, Default)]
enum DocumentProtection {
    #[default]
    Plain,
    Protected(Box<ProtectedDocument>),
}

fn whitespace_prefix_end(text: &str, start: usize) -> usize {
    let mut end = start;
    for (offset, character) in text[start..].char_indices() {
        if !character.is_whitespace() {
            break;
        }
        end = start + offset + character.len_utf8();
    }
    end
}

fn notable_task_prefix_edit(line: &str) -> (usize, usize, &'static str) {
    let indentation_end = whitespace_prefix_end(line, 0);
    let Some(bullet) = line[indentation_end..].chars().next() else {
        return (indentation_end, indentation_end, "- [x] ");
    };
    if !matches!(bullet, '*' | '+' | '-') {
        return (indentation_end, indentation_end, "- [x] ");
    }

    let after_bullet = indentation_end + bullet.len_utf8();
    let after_bullet_whitespace = whitespace_prefix_end(line, after_bullet);
    if after_bullet_whitespace > after_bullet {
        let task = &line[after_bullet_whitespace..];
        let replacement = if task.starts_with("[ ]") {
            Some("- [x] ")
        } else if task.starts_with("[x]") || task.starts_with("[X]") {
            Some("- [ ] ")
        } else {
            None
        };
        if let Some(replacement) = replacement {
            let checkbox_end = after_bullet_whitespace + 3;
            let prefix_end = whitespace_prefix_end(line, checkbox_end);
            return (indentation_end, prefix_end, replacement);
        }
    }

    let prefix_end = whitespace_prefix_end(line, after_bullet);
    (indentation_end, prefix_end, "- [x] ")
}

fn remap_offset_after_replace(
    offset: ByteOffset,
    range: ByteRange,
    replacement_len: usize,
) -> ByteOffset {
    let offset = offset.get();
    let start = range.start().get();
    let end = range.end().get();
    if offset < start {
        return ByteOffset::new(offset);
    }
    if range.is_empty() || offset >= end {
        return ByteOffset::new(start + replacement_len + offset.saturating_sub(end));
    }
    ByteOffset::new(start + (offset - start).min(replacement_len))
}

impl DocumentSession {
    pub fn from_reader(
        target: impl Into<DocumentTarget>,
        title: impl Into<String>,
        reader: impl Read,
    ) -> Result<Self, CoreError> {
        let editor = Editor::from_reader(reader)?;
        let disk_checksum = editor.checksum_fnv1a();
        let target = target.into();
        let note_index = match &target {
            DocumentTarget::WorkspaceNote(index) => *index,
            DocumentTarget::ExternalFile { .. } => usize::MAX,
        };
        Ok(Self {
            target,
            note_index,
            title: title.into(),
            editor,
            file_version: None,
            disk_checksum,
            autosave: AutosaveTracker::default(),
            protection: DocumentProtection::Plain,
            undo_group: None,
        })
    }

    fn from_versioned_reader(
        target: impl Into<DocumentTarget>,
        title: impl Into<String>,
        reader: impl Read,
        file_version: FileVersion,
    ) -> Result<Self, CoreError> {
        let mut document = Self::from_reader(target, title, reader)?;
        document.file_version = Some(file_version);
        Ok(document)
    }

    pub fn note_index(&self) -> usize {
        self.note_index
    }

    pub fn target(&self) -> &DocumentTarget {
        &self.target
    }

    pub fn is_external(&self) -> bool {
        matches!(self.target, DocumentTarget::ExternalFile { .. })
    }

    pub fn is_protected(&self) -> bool {
        matches!(self.protection, DocumentProtection::Protected(_))
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    fn refresh_title_from_body(&mut self) {
        let byte_limit = self
            .editor
            .len_bytes()
            .min(stillus_storage::BODY_TITLE_SCAN_BYTES);
        let mut end = ByteOffset::new(byte_limit);
        while end.get() > 0
            && self
                .editor
                .is_codepoint_boundary(end)
                .is_ok_and(|value| !value)
        {
            end = ByteOffset::new(end.get() - 1);
        }
        let title = ByteRange::new(ByteOffset::new(0), end)
            .ok()
            .and_then(|range| self.editor.slice(range).ok())
            .and_then(|body| project_body_title(&body))
            .unwrap_or_else(|| EMPTY_NOTE_TITLE.to_owned());
        self.title = title;
    }

    pub fn len_bytes(&self) -> usize {
        self.editor.len_bytes()
    }

    pub fn line_count(&self) -> usize {
        self.editor.line_count()
    }

    pub fn selection(&self) -> Selection {
        self.editor.selection()
    }

    pub fn selection_character_count(&self) -> usize {
        self.editor.selection_character_count()
    }

    pub fn content_revision(&self) -> u64 {
        self.autosave.revision
    }

    pub fn find_case_insensitive(&self, query: &str, limit: usize) -> Vec<ByteRange> {
        self.editor.find_case_insensitive(query, limit)
    }

    pub fn save_status(&self) -> SaveStatus {
        if let Some(message) = &self.autosave.external_conflict {
            return SaveStatus::Conflict {
                revision: self.autosave.revision,
                message: message.clone(),
            };
        }
        if let Some(revision) = self.autosave.saving_revision {
            return SaveStatus::Saving {
                revision,
                dirty_after_start: self.autosave.revision > revision,
            };
        }
        if let Some(error) = &self.autosave.error {
            return SaveStatus::Error {
                revision: self.autosave.revision,
                message: error.to_string(),
            };
        }
        if self.autosave.revision == self.autosave.saved_revision {
            SaveStatus::Clean {
                revision: self.autosave.revision,
            }
        } else {
            SaveStatus::Dirty {
                revision: self.autosave.revision,
                deadline_ms: self.autosave.deadline_ms.unwrap_or(0),
            }
        }
    }

    pub fn has_unsaved_work(&self) -> bool {
        self.autosave.revision != self.autosave.saved_revision
            || self.autosave.saving_revision.is_some()
    }

    fn operation_blocked(&self) -> bool {
        self.has_unsaved_work()
            || self.autosave.recovery_saving_revision.is_some()
            || self.autosave.external_conflict.is_some()
    }

    pub fn recovery_status(&self) -> RecoveryStatus {
        if let Some(error) = &self.autosave.recovery_error {
            return RecoveryStatus::Error {
                revision: self.autosave.revision,
                message: error.to_string(),
            };
        }
        if let Some(revision) = self.autosave.recovery_saving_revision {
            return RecoveryStatus::Saving { revision };
        }
        if self.autosave.recovery_revision == self.autosave.revision {
            if self.autosave.revision == 0 {
                RecoveryStatus::None
            } else {
                RecoveryStatus::Saved {
                    revision: self.autosave.revision,
                }
            }
        } else if self.autosave.revision > self.autosave.saved_revision {
            RecoveryStatus::Pending {
                revision: self.autosave.revision,
            }
        } else {
            RecoveryStatus::None
        }
    }

    pub fn next_autosave_deadline(&self) -> Option<u64> {
        (self.autosave.saving_revision.is_none()
            && self.autosave.error.is_none()
            && self.autosave.external_conflict.is_none())
        .then_some(self.autosave.deadline_ms)
        .flatten()
    }

    fn next_persistence_deadline(&self) -> Option<u64> {
        let recovery_deadline = (self.autosave.recovery_saving_revision.is_none()
            && self.autosave.recovery_error.is_none()
            && self.autosave.revision > self.autosave.saved_revision
            && self.autosave.recovery_revision != self.autosave.revision)
            .then_some(self.autosave.recovery_deadline_ms)
            .flatten();
        [self.next_autosave_deadline(), recovery_deadline]
            .into_iter()
            .flatten()
            .min()
    }

    pub fn cursor_line(&self) -> Result<usize, CoreError> {
        Ok(self
            .editor
            .line_of_offset(self.editor.selection().focus())?)
    }

    pub fn cursor_byte_column(&self) -> Result<usize, CoreError> {
        let focus = self.editor.selection().focus();
        let line = self.editor.line_of_offset(focus)?;
        let start = self
            .editor
            .offset_of_line(line)
            .unwrap_or(ByteOffset::new(0));
        Ok(focus.get() - start.get())
    }

    pub fn viewport(&self, request: ViewportRequest) -> Result<ViewportSnapshot, CoreError> {
        let total_lines = self.editor.line_count();
        let overscan = request.overscan_lines.min(MAX_VIEWPORT_LINES / 2);
        let start_line = request
            .first_line
            .saturating_sub(overscan)
            .min(total_lines.saturating_sub(1));
        let requested_lines = request
            .visible_lines
            .max(1)
            .saturating_add(overscan.saturating_mul(2))
            .min(MAX_VIEWPORT_LINES);
        let mut lines = Vec::with_capacity(requested_lines);
        let mut rendered_bytes = 0_usize;

        for line_index in start_line..total_lines.min(start_line + requested_lines) {
            let start = self
                .editor
                .offset_of_line(line_index)
                .unwrap_or(ByteOffset::new(self.editor.len_bytes()));
            let full_end = self
                .editor
                .offset_of_line(line_index + 1)
                .unwrap_or(ByteOffset::new(self.editor.len_bytes()));
            let available = MAX_VIEWPORT_BYTES.saturating_sub(rendered_bytes);
            let mut end =
                ByteOffset::new(full_end.get().min(start.get().saturating_add(available)));
            while end > start && !self.editor.is_codepoint_boundary(end)? {
                end = ByteOffset::new(end.get() - 1);
            }
            let truncated = end < full_end;
            let raw = self.editor.slice(ByteRange::new(start, end)?)?;
            rendered_bytes = rendered_bytes.saturating_add(raw.len());
            let text = raw
                .strip_suffix('\n')
                .unwrap_or(&raw)
                .strip_suffix('\r')
                .unwrap_or_else(|| raw.strip_suffix('\n').unwrap_or(&raw))
                .to_owned();
            lines.push(ViewportLine {
                line_index,
                start,
                end,
                text,
                truncated,
            });
            if truncated || rendered_bytes == MAX_VIEWPORT_BYTES {
                break;
            }
        }

        let last_line = lines
            .last()
            .map(|line| line.line_index)
            .unwrap_or(start_line);
        let truncated_after = lines.last().is_some_and(|line| line.truncated)
            || last_line.saturating_add(1) < total_lines;
        Ok(ViewportSnapshot {
            start_line,
            total_lines,
            rendered_bytes,
            truncated_before: start_line > 0,
            truncated_after,
            lines,
        })
    }

    pub fn apply(&mut self, command: EditorCommand) -> Result<CommandOutcome, CoreError> {
        self.apply_at(command, 0)
    }

    pub fn apply_at(
        &mut self,
        command: EditorCommand,
        now_ms: u64,
    ) -> Result<CommandOutcome, CoreError> {
        let selection_before = self.editor.selection();
        let mut text_changed = false;
        let mut clipboard = None;
        let edit_group = self.edit_group_for(&command);
        self.prepare_history_group(edit_group, now_ms);
        match command {
            EditorCommand::ReplaceRange { start, end, text } => {
                let range = ByteRange::new(ByteOffset::new(start), ByteOffset::new(end))?;
                let shift = |offset: ByteOffset| {
                    let offset = offset.get();
                    ByteOffset::new(if offset <= start {
                        offset
                    } else if offset >= end {
                        offset - (end - start) + text.len()
                    } else {
                        start + text.len()
                    })
                };
                let selection = Selection::new(
                    shift(selection_before.anchor()),
                    shift(selection_before.focus()),
                );
                self.editor
                    .replace_with_selection(range, &text, selection)?;
                text_changed = start != end || !text.is_empty();
            }
            EditorCommand::Insert(text) => {
                text_changed = !text.is_empty() || !self.editor.selection().is_caret();
                let range = self.editor.selection().normalized();
                if edit_group == Some(EditGroup::Typing) {
                    self.editor
                        .replace_grouped(range, &text, EditGroup::Typing)?;
                } else {
                    self.editor.replace(range, &text)?;
                }
            }
            EditorCommand::Paste(text) => {
                text_changed = !text.is_empty() || !self.editor.selection().is_caret();
                self.editor.replace_selection(&text)?;
            }
            EditorCommand::Backspace => {
                text_changed = self.delete_backward(edit_group == Some(EditGroup::Backspace))?
            }
            EditorCommand::DeleteForward => {
                text_changed = self.delete_forward(edit_group == Some(EditGroup::DeleteForward))?
            }
            EditorCommand::ToggleTaskDone => text_changed = self.toggle_task_done()?,
            EditorCommand::MoveLeft { extend } => self.move_horizontal(false, extend)?,
            EditorCommand::MoveRight { extend } => self.move_horizontal(true, extend)?,
            EditorCommand::MoveWordLeft { extend } => self.move_word(false, extend)?,
            EditorCommand::MoveWordRight { extend } => self.move_word(true, extend)?,
            EditorCommand::MoveLineStart { extend } => self.move_line_edge(false, extend)?,
            EditorCommand::MoveLineEnd { extend } => self.move_line_edge(true, extend)?,
            EditorCommand::MoveDocumentStart { extend } => {
                self.set_moved_selection(ByteOffset::new(0), extend)?
            }
            EditorCommand::MoveDocumentEnd { extend } => {
                self.set_moved_selection(ByteOffset::new(self.editor.len_bytes()), extend)?
            }
            EditorCommand::MoveUp { extend } => self.move_vertical(-1, extend)?,
            EditorCommand::MoveDown { extend } => self.move_vertical(1, extend)?,
            EditorCommand::SetCaret { offset, extend } => {
                self.set_moved_selection(ByteOffset::new(offset), extend)?
            }
            EditorCommand::SetSelection { anchor, focus } => self.editor.set_selection(
                Selection::new(ByteOffset::new(anchor), ByteOffset::new(focus)),
            )?,
            EditorCommand::SelectAll => self.editor.set_selection(Selection::new(
                ByteOffset::new(0),
                ByteOffset::new(self.editor.len_bytes()),
            ))?,
            EditorCommand::Copy => clipboard = self.selected_text()?,
            EditorCommand::Cut => {
                clipboard = self.selected_text()?;
                if clipboard.is_some() {
                    self.editor.replace_selection("")?;
                    text_changed = true;
                }
            }
            EditorCommand::Undo => text_changed = self.editor.undo(),
            EditorCommand::Redo => text_changed = self.editor.redo(),
        }
        if text_changed {
            self.undo_group = edit_group.map(|group| UndoGroupState {
                group,
                last_edit_ms: now_ms,
            });
        } else if edit_group.is_some() {
            self.editor.break_history_group();
            self.undo_group = None;
        }
        if text_changed {
            if !self.is_external() {
                self.refresh_title_from_body();
            }
            self.autosave.revision = self.autosave.revision.saturating_add(1);
            self.autosave.deadline_ms = Some(now_ms.saturating_add(AUTOSAVE_DEBOUNCE_MS));
            self.autosave.error = None;
            let recovery_debounce = if self.is_protected() {
                PROTECTED_RECOVERY_DEBOUNCE_MS
            } else {
                RECOVERY_DEBOUNCE_MS
            };
            self.autosave.recovery_deadline_ms = Some(now_ms.saturating_add(recovery_debounce));
            self.autosave.recovery_error = None;
        }
        let selection = self.editor.selection();
        Ok(CommandOutcome {
            text_changed,
            selection_changed: selection != selection_before,
            selection,
            clipboard,
            content_revision: self.autosave.revision,
        })
    }

    fn begin_autosave(
        &mut self,
        workspace: PathBuf,
        path: PathBuf,
        now_ms: u64,
        modified: String,
        recovery_store: RecoveryStore,
        recovery_key: RecoveryKey,
    ) -> Result<Option<SaveJob>, CoreError> {
        if self.autosave.saving_revision.is_some()
            || self.autosave.error.is_some()
            || self.autosave.external_conflict.is_some()
            || self.autosave.revision == self.autosave.saved_revision
            || self
                .autosave
                .deadline_ms
                .is_none_or(|deadline| deadline > now_ms)
        {
            return Ok(None);
        }
        let expected_version = self.file_version.ok_or_else(|| {
            CoreError::NoteUnavailable("document has no filesystem version".to_owned())
        })?;
        let revision = self.autosave.revision;
        let snapshot = self.editor.snapshot();
        let snapshot_checksum = snapshot.checksum_fnv1a();
        self.autosave.saving_revision = Some(revision);
        Ok(Some(SaveJob {
            target: self.target.clone(),
            revision,
            path,
            workspace,
            title: self.title.clone(),
            expected_version,
            modified,
            snapshot,
            snapshot_checksum,
            recovery_store,
            recovery_key,
            protected: match &self.protection {
                DocumentProtection::Plain => None,
                DocumentProtection::Protected(protected) => Some(protected.as_ref().clone()),
            },
        }))
    }

    fn finish_autosave(&mut self, completion: SaveCompletion) {
        if self.autosave.saving_revision != Some(completion.revision) {
            self.autosave.error = Some(SaveError::InvalidTarget(
                "autosave completion revision does not match active job".to_owned(),
            ));
            return;
        }
        self.autosave.saving_revision = None;
        match completion.result {
            Ok(commit) => {
                self.file_version = Some(commit.version);
                if let (DocumentProtection::Protected(protected), Some(frontmatter)) =
                    (&mut self.protection, completion.protected_frontmatter)
                {
                    protected.frontmatter = *frontmatter;
                }
                self.disk_checksum = completion.saved_checksum;
                self.autosave.saved_revision =
                    self.autosave.saved_revision.max(completion.revision);
                self.autosave.error = None;
                if self.autosave.saved_revision == self.autosave.revision {
                    self.autosave.deadline_ms = None;
                }
                if completion.cleanup_error.is_none()
                    && self.autosave.recovery_revision <= completion.revision
                {
                    self.autosave.recovery_revision = 0;
                }
                self.autosave.recovery_error = completion.cleanup_error.map(RecoveryError::Io);
            }
            Err(error) => {
                self.autosave.error = Some(error);
                self.autosave.deadline_ms = None;
            }
        }
    }

    fn retry_autosave(&mut self, now_ms: u64) -> bool {
        if self.autosave.saving_revision.is_some()
            || self.autosave.revision == self.autosave.saved_revision
            || self.autosave.external_conflict.is_some()
        {
            return false;
        }
        self.autosave.error = None;
        self.autosave.deadline_ms = Some(now_ms);
        true
    }

    fn begin_recovery(
        &mut self,
        store: RecoveryStore,
        key: RecoveryKey,
        now_ms: u64,
    ) -> Option<RecoveryJob> {
        if self.autosave.recovery_saving_revision.is_some()
            || self.autosave.recovery_error.is_some()
            || self.autosave.revision == self.autosave.saved_revision
            || self.autosave.recovery_revision == self.autosave.revision
            || self
                .autosave
                .recovery_deadline_ms
                .is_none_or(|deadline| deadline > now_ms)
        {
            return None;
        }
        let revision = self.autosave.revision;
        self.autosave.recovery_saving_revision = Some(revision);
        Some(RecoveryJob {
            target: self.target.clone(),
            revision,
            store,
            key,
            base_checksum: self.disk_checksum,
            snapshot: self.editor.snapshot(),
            protected: match &self.protection {
                DocumentProtection::Plain => None,
                DocumentProtection::Protected(protected) => Some(protected.as_ref().clone()),
            },
        })
    }

    fn finish_recovery(&mut self, completion: RecoveryCompletion) {
        if self.autosave.recovery_saving_revision != Some(completion.revision) {
            self.autosave.recovery_error = Some(RecoveryError::InvalidArtifact(
                "recovery completion revision does not match active job".to_owned(),
            ));
            return;
        }
        self.autosave.recovery_saving_revision = None;
        match completion.result {
            Ok(record) => {
                self.autosave.recovery_revision =
                    self.autosave.recovery_revision.max(record.revision);
                self.autosave.recovery_error = None;
                if self.autosave.recovery_revision == self.autosave.revision {
                    self.autosave.recovery_deadline_ms = None;
                }
            }
            Err(error) => {
                self.autosave.recovery_error = Some(error);
                self.autosave.recovery_deadline_ms = None;
            }
        }
    }

    fn selected_text(&self) -> Result<Option<String>, CoreError> {
        let range = self.editor.selection().normalized();
        if range.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.editor.slice(range)?))
    }

    fn toggle_task_done(&mut self) -> Result<bool, CoreError> {
        let selection = self.editor.selection();
        let line = self.editor.line_of_offset(selection.normalized().start())?;
        let (line_start, line_end) = self.line_content_bounds(line)?;
        let line_text = self.editor.slice(ByteRange::new(line_start, line_end)?)?;
        let (prefix_start, prefix_end, replacement) = notable_task_prefix_edit(&line_text);
        let range = ByteRange::new(
            ByteOffset::new(line_start.get() + prefix_start),
            ByteOffset::new(line_start.get() + prefix_end),
        )?;
        let selection_after = Selection::new(
            remap_offset_after_replace(selection.anchor(), range, replacement.len()),
            remap_offset_after_replace(selection.focus(), range, replacement.len()),
        );
        self.editor
            .replace_with_selection(range, replacement, selection_after)?;
        Ok(true)
    }

    fn edit_group_for(&self, command: &EditorCommand) -> Option<EditGroup> {
        let selection = self.editor.selection();
        if !selection.is_caret() {
            return None;
        }
        match command {
            EditorCommand::Insert(text)
                if !text.is_empty() && !text.chars().any(char::is_whitespace) =>
            {
                Some(EditGroup::Typing)
            }
            EditorCommand::Backspace => Some(EditGroup::Backspace),
            EditorCommand::DeleteForward => Some(EditGroup::DeleteForward),
            _ => None,
        }
    }

    fn prepare_history_group(&mut self, group: Option<EditGroup>, now_ms: u64) {
        let continues = group.is_some_and(|group| {
            self.undo_group.is_some_and(|previous| {
                previous.group == group
                    && now_ms.saturating_sub(previous.last_edit_ms) <= UNDO_GROUP_TIMEOUT_MS
            })
        });
        if !continues {
            self.editor.break_history_group();
        }
        if group.is_none() {
            self.undo_group = None;
        }
    }

    fn delete_backward(&mut self, grouped: bool) -> Result<bool, CoreError> {
        let selection = self.editor.selection();
        if !selection.is_caret() {
            self.editor.replace_selection("")?;
            return Ok(true);
        }
        let focus = selection.focus();
        let Some(previous) = self.editor.previous_grapheme(focus)? else {
            return Ok(false);
        };
        let range = ByteRange::new(previous, focus)?;
        if grouped {
            self.editor
                .replace_grouped(range, "", EditGroup::Backspace)?;
        } else {
            self.editor.delete(range)?;
        }
        Ok(true)
    }

    fn delete_forward(&mut self, grouped: bool) -> Result<bool, CoreError> {
        let selection = self.editor.selection();
        if !selection.is_caret() {
            self.editor.replace_selection("")?;
            return Ok(true);
        }
        let focus = selection.focus();
        let Some(next) = self.editor.next_grapheme(focus)? else {
            return Ok(false);
        };
        let range = ByteRange::new(focus, next)?;
        if grouped {
            self.editor
                .replace_grouped(range, "", EditGroup::DeleteForward)?;
        } else {
            self.editor.delete(range)?;
        }
        Ok(true)
    }

    fn move_horizontal(&mut self, forward: bool, extend: bool) -> Result<(), CoreError> {
        let selection = self.editor.selection();
        let target = if !extend && !selection.is_caret() {
            if forward {
                selection.normalized().end()
            } else {
                selection.normalized().start()
            }
        } else if forward {
            self.editor
                .next_grapheme(selection.focus())?
                .unwrap_or(selection.focus())
        } else {
            self.editor
                .previous_grapheme(selection.focus())?
                .unwrap_or(selection.focus())
        };
        self.set_moved_selection(target, extend)?;
        Ok(())
    }

    fn move_word(&mut self, forward: bool, extend: bool) -> Result<(), CoreError> {
        let selection = self.editor.selection();
        if !extend && !selection.is_caret() {
            let target = if forward {
                selection.normalized().end()
            } else {
                selection.normalized().start()
            };
            self.set_moved_selection(target, false)?;
            return Ok(());
        }

        let focus = selection.focus();
        let line = self.editor.line_of_offset(focus)?;
        let (line_start, line_end) = self.line_content_bounds(line)?;
        let window_start = if forward {
            focus
        } else {
            self.bounded_codepoint_offset(
                line_start,
                focus.get().saturating_sub(MAX_VIEWPORT_BYTES),
                true,
            )?
        };
        let window_end = if forward {
            self.bounded_codepoint_offset(
                focus,
                line_end
                    .get()
                    .min(focus.get().saturating_add(MAX_VIEWPORT_BYTES)),
                false,
            )?
        } else {
            focus
        };
        let text = self
            .editor
            .slice(ByteRange::new(window_start, window_end)?)?;
        let local_focus = focus.get() - window_start.get();
        let local_target = if forward {
            next_word_boundary_in_text(&text, local_focus)?
        } else {
            previous_word_boundary_in_text(&text, local_focus)?
        };
        self.set_moved_selection(
            ByteOffset::new(window_start.get() + local_target.get()),
            extend,
        )?;
        Ok(())
    }

    fn move_line_edge(&mut self, end: bool, extend: bool) -> Result<(), CoreError> {
        let focus = self.editor.selection().focus();
        let line = self.editor.line_of_offset(focus)?;
        let (start, finish) = self.line_content_bounds(line)?;
        self.set_moved_selection(if end { finish } else { start }, extend)?;
        Ok(())
    }

    fn line_content_bounds(&self, line: usize) -> Result<(ByteOffset, ByteOffset), CoreError> {
        let start = self
            .editor
            .offset_of_line(line)
            .unwrap_or(ByteOffset::new(self.editor.len_bytes()));
        let mut end = self
            .editor
            .offset_of_line(line + 1)
            .unwrap_or(ByteOffset::new(self.editor.len_bytes()));
        if let Some(previous) = self.editor.previous_codepoint(end)?
            && self.editor.slice(ByteRange::new(previous, end)?)? == "\n"
        {
            end = previous;
            if let Some(before_cr) = self.editor.previous_codepoint(end)?
                && self.editor.slice(ByteRange::new(before_cr, end)?)? == "\r"
            {
                end = before_cr;
            }
        }
        Ok((start, end))
    }

    fn bounded_codepoint_offset(
        &self,
        minimum: ByteOffset,
        candidate: usize,
        round_up: bool,
    ) -> Result<ByteOffset, CoreError> {
        let mut offset = ByteOffset::new(candidate.max(minimum.get()));
        while !self.editor.is_codepoint_boundary(offset)? {
            offset = ByteOffset::new(if round_up {
                offset.get().saturating_add(1)
            } else {
                offset.get().saturating_sub(1)
            });
        }
        Ok(offset)
    }

    fn move_vertical(&mut self, delta: isize, extend: bool) -> Result<(), CoreError> {
        let selection = self.editor.selection();
        let focus = selection.focus();
        let current_line = self.editor.line_of_offset(focus)?;
        let current_start = self
            .editor
            .offset_of_line(current_line)
            .unwrap_or(ByteOffset::new(0));
        let column = focus.get() - current_start.get();
        let target_line = current_line
            .saturating_add_signed(delta)
            .min(self.editor.line_count().saturating_sub(1));
        let target_start = self
            .editor
            .offset_of_line(target_line)
            .unwrap_or(ByteOffset::new(self.editor.len_bytes()));
        let mut target_end = self
            .editor
            .offset_of_line(target_line + 1)
            .unwrap_or(ByteOffset::new(self.editor.len_bytes()));
        if let Some(previous) = self.editor.previous_codepoint(target_end)? {
            if self.editor.slice(ByteRange::new(previous, target_end)?)? == "\n" {
                target_end = previous;
                if let Some(before_cr) = self.editor.previous_codepoint(target_end)? {
                    if self.editor.slice(ByteRange::new(before_cr, target_end)?)? == "\r" {
                        target_end = before_cr;
                    }
                }
            }
        }
        let mut target = ByteOffset::new(
            target_start
                .get()
                .saturating_add(column)
                .min(target_end.get()),
        );
        while target > target_start && !self.editor.is_codepoint_boundary(target)? {
            target = ByteOffset::new(target.get() - 1);
        }
        self.set_moved_selection(target, extend)?;
        Ok(())
    }

    fn set_moved_selection(&mut self, target: ByteOffset, extend: bool) -> Result<(), CoreError> {
        let selection = self.editor.selection();
        self.editor.set_selection(if extend {
            Selection::new(selection.anchor(), target)
        } else {
            Selection::caret(target)
        })?;
        Ok(())
    }
}

impl LocalSearchDocument for DocumentSession {
    fn find_case_insensitive(&self, query: &str, limit: usize) -> Vec<LocalSearchMatch> {
        self.editor
            .find_case_insensitive(query, limit)
            .into_iter()
            .map(|range| LocalSearchMatch {
                start_byte: range.start().get(),
                end_byte: range.end().get(),
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ViewportRequest {
    pub first_line: usize,
    pub visible_lines: usize,
    pub overscan_lines: usize,
}

impl Default for ViewportRequest {
    fn default() -> Self {
        Self {
            first_line: 0,
            visible_lines: DEFAULT_VIEWPORT_LINES,
            overscan_lines: DEFAULT_OVERSCAN_LINES,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewportLine {
    pub line_index: usize,
    pub start: ByteOffset,
    pub end: ByteOffset,
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewportSnapshot {
    pub start_line: usize,
    pub total_lines: usize,
    pub rendered_bytes: usize,
    pub truncated_before: bool,
    pub truncated_after: bool,
    pub lines: Vec<ViewportLine>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditorCommand {
    ReplaceRange {
        start: usize,
        end: usize,
        text: String,
    },
    Insert(String),
    Paste(String),
    Backspace,
    DeleteForward,
    ToggleTaskDone,
    MoveLeft {
        extend: bool,
    },
    MoveRight {
        extend: bool,
    },
    MoveWordLeft {
        extend: bool,
    },
    MoveWordRight {
        extend: bool,
    },
    MoveLineStart {
        extend: bool,
    },
    MoveLineEnd {
        extend: bool,
    },
    MoveDocumentStart {
        extend: bool,
    },
    MoveDocumentEnd {
        extend: bool,
    },
    MoveUp {
        extend: bool,
    },
    MoveDown {
        extend: bool,
    },
    SetCaret {
        offset: usize,
        extend: bool,
    },
    SetSelection {
        anchor: usize,
        focus: usize,
    },
    SelectAll,
    Copy,
    Cut,
    Undo,
    Redo,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutcome {
    pub text_changed: bool,
    pub selection_changed: bool,
    pub selection: Selection,
    pub clipboard: Option<String>,
    pub content_revision: u64,
}

pub fn format_utc_timestamp(time: SystemTime) -> Result<String, CoreError> {
    let duration = time
        .duration_since(UNIX_EPOCH)
        .map_err(|error| CoreError::Clock(error.to_string()))?;
    let total_seconds = i64::try_from(duration.as_secs())
        .map_err(|_| CoreError::Clock("timestamp is outside supported range".to_owned()))?;
    let days = total_seconds / 86_400;
    let seconds_in_day = total_seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_in_day / 3_600;
    let minute = (seconds_in_day % 3_600) / 60;
    let second = seconds_in_day % 60;
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}Z",
        duration.subsec_millis()
    ))
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let days = days_since_epoch + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

/// Shared category validation for native and addressed RSS operations.
pub fn normalize_rss_categories(categories: &[String]) -> Result<Vec<String>, CoreError> {
    if categories.len() > 100 {
        return Err(CoreError::NoteUnavailable("too many RSS categories".into()));
    }
    let mut normalized = Vec::new();
    for category in categories {
        let category = validate_tag(category)?;
        if category == FAVORITED_ORDER_KEY {
            return Err(CoreError::NoteUnavailable(
                "category name is reserved for Favorites ordering".into(),
            ));
        }
        if !normalized.contains(&category) {
            normalized.push(category);
        }
    }
    Ok(normalized)
}

pub fn apply_rss_metadata(
    engine: &mut RssEngine,
    id: &ItemId,
    version: u64,
    mut patch: stillus_engine::CommonMetadataPatch,
    timestamp: &str,
) -> Result<(), CoreError> {
    if let Some(categories) = &patch.categories {
        patch.categories = Some(normalize_rss_categories(categories)?);
    }
    if let Some(title) = &patch.title {
        let title = title.trim();
        if title.is_empty() || title.len() > 200 {
            return Err(CoreError::NoteUnavailable("invalid RSS title".into()));
        }
        patch.title = Some(title.into());
    }
    engine
        .update_metadata_at(id, &version.to_string(), patch, timestamp)
        .map_err(CoreError::Engine)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use stillus_secure::{EnvelopeKind, EnvelopeMetadata, EnvelopeWriter};

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn document_text(document: &DocumentSession) -> String {
        document
            .editor
            .slice(
                ByteRange::new(
                    ByteOffset::new(0),
                    ByteOffset::new(document.editor.len_bytes()),
                )
                .unwrap(),
            )
            .unwrap()
    }

    #[test]
    fn session_surfaces_the_toolbar_actions_its_engines_declare() {
        let workspace = TestWorkspace::new();
        let session = WorkspaceSession::open(workspace.path()).unwrap();

        assert_eq!(
            session.rss_toolbar_actions(),
            vec![
                ToolbarAction::Refresh,
                ToolbarAction::Filters,
                ToolbarAction::Rename,
                ToolbarAction::Categories,
                ToolbarAction::Pin,
                ToolbarAction::Favorite,
                ToolbarAction::Delete,
                ToolbarAction::Restore,
            ]
        );
        assert!(
            session
                .engine_toolbar_actions(&EngineId::new("missing").unwrap())
                .is_empty()
        );
    }

    #[test]
    fn workspace_scan_derives_categories_and_opens_only_note_body() {
        let workspace = TestWorkspace::new();
        workspace.write_note(
            "project.md",
            "---\ntitle: Project Alpha\ntags: [Work, Tasks]\npinned: true\nfavorited: true\n---\n# Project Alpha\n\nBody 🦀\n",
        );
        workspace.write_note("plain.md", "# Plain\n");
        workspace.write_note("broken.md", "---\ntitle: [broken\n---\nbody\n");

        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        assert_eq!(session.notes().len(), 3);
        assert_eq!(
            session.categories(),
            &[
                CategorySummary {
                    name: "Tasks".to_owned(),
                    note_count: 1,
                },
                CategorySummary {
                    name: "Work".to_owned(),
                    note_count: 1,
                },
            ]
        );
        let project = session
            .notes()
            .iter()
            .position(|note| note.title == "Project Alpha")
            .unwrap();
        session.open_note(project).unwrap();
        let snapshot = session
            .document()
            .unwrap()
            .viewport(ViewportRequest::default())
            .unwrap();
        assert_eq!(snapshot.lines[0].text, "# Project Alpha");
        assert!(!snapshot.lines.iter().any(|line| line.text == "---"));

        let broken = session
            .notes()
            .iter()
            .position(|note| note.title == "broken")
            .unwrap();
        assert!(matches!(
            session.open_note(broken),
            Err(CoreError::NoteUnavailable(_))
        ));
    }

    #[test]
    fn external_markdown_is_whole_text_stable_deduplicated_and_saved_in_place() {
        let workspace = TestWorkspace::new();
        workspace.write_note("inside.md", "# Inside\n");
        let external = workspace.path().join("External.TXT");
        let original = "---\ntitle: literal\ntags: [not, metadata]\n---\n# External\n";
        fs::write(&external, original).unwrap();
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        assert_eq!(
            session.external_file_extensions(),
            ["markdown", "md", "txt"]
        );
        session.open_note(0).unwrap();
        assert!(session.selected_document_supports_local_search());
        let workspace_matches = session.search_selected_document("inside", 10).unwrap();
        assert_eq!(workspace_matches.len(), 1);
        assert_eq!(workspace_matches[0].start().get(), 2);
        assert_eq!(workspace_matches[0].end().get(), 8);

        let target = session.attach_external_file(&external).unwrap();
        assert!(matches!(target, DocumentTarget::ExternalFile { .. }));
        assert_eq!(session.attach_external_file(&external).unwrap(), target);
        assert_eq!(session.external_files().len(), 1);
        let DocumentTarget::ExternalFile { engine_id, item_id } = target else {
            unreachable!()
        };
        session.open_external_item(&engine_id, &item_id).unwrap();
        assert_eq!(session.selected_note(), None);
        assert_eq!(session.document().unwrap().title(), "External.TXT");
        assert_eq!(document_text(session.document().unwrap()), original);
        assert!(session.selected_document_supports_local_search());
        let external_matches = session.search_selected_document("external", 10).unwrap();
        let external_start = original.find("External").unwrap();
        assert_eq!(external_matches.len(), 1);
        assert_eq!(external_matches[0].start().get(), external_start);
        assert_eq!(
            external_matches[0].end().get(),
            external_start + "External".len()
        );

        session
            .apply_selected_at(EditorCommand::SelectAll, 1)
            .unwrap();
        let replacement = "---\nstill: literal\n---\nchanged\n";
        session
            .apply_selected_at(EditorCommand::Insert(replacement.to_owned()), 2)
            .unwrap();
        assert_eq!(session.document().unwrap().title(), "External.TXT");
        let save = session
            .begin_autosave(2 + AUTOSAVE_DEBOUNCE_MS, "ignored".to_owned())
            .unwrap()
            .unwrap();
        session.finish_autosave(save.execute()).unwrap();
        assert_eq!(fs::read_to_string(&external).unwrap(), replacement);

        let inside = workspace.note_path("inside.md");
        let canonical = inside.canonicalize().unwrap();
        stillus_platform::diagnostics::path_comparison(
            stillus_platform::diagnostics::PathOperation::WorkspaceNote,
            &canonical,
            &session.notes()[0].path,
        );
        for path in [&inside, &canonical] {
            assert_eq!(
                session.attach_external_file(path).unwrap(),
                DocumentTarget::WorkspaceNote(0)
            );
        }
        assert_eq!(session.external_files().len(), 1);
    }

    #[test]
    fn external_recovery_survives_workspace_restart() {
        let workspace = TestWorkspace::new();
        let external = workspace.path().join("recover.md");
        fs::write(&external, "disk\n").unwrap();
        let (engine_id, item_id) = {
            let mut session = WorkspaceSession::open(workspace.path()).unwrap();
            let DocumentTarget::ExternalFile { engine_id, item_id } =
                session.open_external_file(&external).unwrap()
            else {
                unreachable!()
            };
            session
                .apply_selected_at(EditorCommand::Insert("recovered ".to_owned()), 0)
                .unwrap();
            let PersistenceJob::Recovery(job) = session
                .begin_persistence(RECOVERY_DEBOUNCE_MS, "ignored".to_owned())
                .unwrap()
                .unwrap()
            else {
                panic!("recovery must be persisted before canonical autosave")
            };
            session
                .finish_persistence(PersistenceCompletion::Recovery(job.execute()))
                .unwrap();
            (engine_id, item_id)
        };

        let mut restarted = WorkspaceSession::open(workspace.path()).unwrap();
        restarted.attach_external_file(&external).unwrap();
        assert!(restarted.external_files()[0].recovery_available);
        restarted
            .restore_external_recovery(&engine_id, &item_id, 1_000)
            .unwrap();
        assert_eq!(
            document_text(restarted.document().unwrap()),
            "recovered disk\n"
        );
    }

    #[test]
    fn front_matter_separator_is_hidden_in_editor_and_preserved_by_autosave() {
        let workspace = TestWorkspace::new();
        workspace.write_note(
            "Note.md",
            "---\ntitle: Note\nmodified: old\n---\n\n# Note\nBody\n",
        );
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.open_note(0).unwrap();

        let snapshot = session
            .document()
            .unwrap()
            .viewport(ViewportRequest::default())
            .unwrap();
        assert_eq!(snapshot.lines[0].text, "# Note");
        assert_eq!(snapshot.lines[1].text, "Body");

        session
            .apply_selected_at(EditorCommand::SelectAll, 1)
            .unwrap();
        session
            .apply_selected_at(EditorCommand::Insert("# Note\nChanged\n".to_owned()), 2)
            .unwrap();
        let save = session
            .begin_autosave(
                2 + AUTOSAVE_DEBOUNCE_MS,
                "2026-09-03T12:00:00.000Z".to_owned(),
            )
            .unwrap()
            .unwrap();
        session.finish_autosave(save.execute()).unwrap();
        assert!(
            fs::read_to_string(workspace.note_path("Note.md"))
                .unwrap()
                .ends_with("---\n\n# Note\nChanged\n")
        );

        let created_index = session
            .create_note("Created", "2026-09-03T12:00:01.000Z")
            .unwrap();
        assert_eq!(session.selected_note(), Some(created_index));
        let created = session
            .document()
            .unwrap()
            .viewport(ViewportRequest::default())
            .unwrap();
        assert_eq!(created.lines[0].text, "# Created");
        assert_eq!(
            fs::read_to_string(workspace.note_path("Created.md")).unwrap(),
            "---\nfavorited: false\npinned: false\ntags: []\ntitle: 'Created'\ncreated: '2026-09-03T12:00:01.000Z'\nmodified: '2026-09-03T12:00:01.000Z'\n---\n\n# Created"
        );
    }

    #[test]
    fn first_line_title_is_live_then_relocates_and_sorts_on_commit() {
        let workspace = TestWorkspace::new();
        workspace.write_note(
            "a.md",
            "---\ntitle: Legacy\nmodified: old\n---\n# Zebra\nbody\n",
        );
        workspace.write_note("b.md", "---\ntitle: Middle\n---\n# Middle\n");
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        let zebra = session
            .notes()
            .iter()
            .position(|note| note.title == "Zebra")
            .unwrap();
        session.open_note(zebra).unwrap();
        session
            .apply_selected_at(EditorCommand::SelectAll, 1)
            .unwrap();
        session
            .apply_selected_at(EditorCommand::Insert("# Alpha\nbody\n".to_owned()), 2)
            .unwrap();
        assert_eq!(session.notes()[zebra].title, "Alpha");
        assert_eq!(session.notes()[0].title, "Middle");
        session.apply_selected_at(EditorCommand::Undo, 3).unwrap();
        assert_eq!(session.notes()[zebra].title, "Zebra");
        session.apply_selected_at(EditorCommand::Redo, 4).unwrap();
        assert_eq!(session.notes()[zebra].title, "Alpha");

        let save = session
            .begin_autosave(4 + AUTOSAVE_DEBOUNCE_MS, "now".to_owned())
            .unwrap()
            .unwrap();
        session.finish_autosave(save.execute()).unwrap();
        assert_eq!(session.notes()[0].title, "Alpha");
        assert_eq!(session.selected_note(), Some(0));
        let path = workspace.note_path("Alpha.md");
        assert!(path.is_file());
        assert!(!workspace.note_path("a.md").exists());
        let saved = fs::read_to_string(path).unwrap();
        assert!(saved.contains("title: 'Alpha'\n"));
        assert!(saved.ends_with("# Alpha\nbody\n"));
    }

    #[test]
    fn notes_are_sorted_pinned_then_unicode_lowercase_title_then_path() {
        let workspace = TestWorkspace::new();
        workspace.write_note("z.md", "---\ntitle: beta\npinned: false\n---\n");
        workspace.write_note("a.md", "---\ntitle: Alpha\npinned: false\n---\n");
        workspace.write_note("c.md", "---\ntitle: alpha\npinned: false\n---\n");
        workspace.write_note("u.md", "---\ntitle: Ёж\npinned: false\n---\n");
        workspace.write_note("p2.md", "---\ntitle: Zulu\npinned: true\n---\n");
        workspace.write_note("p1.md", "---\ntitle: alpha pinned\npinned: true\n---\n");

        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        assert_eq!(
            session
                .notes()
                .iter()
                .map(|note| note.title.as_str())
                .collect::<Vec<_>>(),
            ["alpha pinned", "Zulu", "Alpha", "alpha", "beta", "Ёж"]
        );

        fs::write(
            workspace.note_path("z.md"),
            "---\ntitle: aardvark\npinned: true\n---\n",
        )
        .unwrap();
        session.refresh_notes().unwrap();
        assert_eq!(session.notes()[0].title, "aardvark");
        assert!(session.notes()[0].pinned);
    }

    #[test]
    fn viewport_is_hard_bounded_and_keeps_utf8_boundaries() {
        let text = "🦀".repeat(MAX_VIEWPORT_BYTES);
        let document = DocumentSession {
            target: DocumentTarget::WorkspaceNote(0),
            note_index: 0,
            title: "large".to_owned(),
            editor: Editor::new(&text),
            file_version: None,
            disk_checksum: 0,
            autosave: AutosaveTracker::default(),
            protection: DocumentProtection::Plain,
            undo_group: None,
        };
        let snapshot = document
            .viewport(ViewportRequest {
                first_line: 0,
                visible_lines: usize::MAX,
                overscan_lines: usize::MAX,
            })
            .unwrap();
        assert!(snapshot.lines.len() <= MAX_VIEWPORT_LINES);
        assert!(snapshot.rendered_bytes <= MAX_VIEWPORT_BYTES);
        assert!(snapshot.lines[0].truncated);
        assert!(snapshot.truncated_after);
        assert!(std::str::from_utf8(snapshot.lines[0].text.as_bytes()).is_ok());
    }

    #[test]
    fn toggle_task_done_matches_notable_prefix_normalization_and_line_endings() {
        for (before, after) in [
            ("- [ ] task", "- [x] task"),
            ("- [x]task", "- [ ] task"),
            ("*   [X]\t done", "- [ ] done"),
            ("+ \t[ ]\tthing", "- [x] thing"),
            ("  *   ordinary", "  - [x] ordinary"),
            ("  text", "  - [x] text"),
            ("  ", "  - [x] "),
        ] {
            let mut document =
                DocumentSession::from_reader(0, "task", Cursor::new(before)).unwrap();
            let outcome = document.apply(EditorCommand::ToggleTaskDone).unwrap();
            assert!(outcome.text_changed, "before={before:?}");
            assert_eq!(document_text(&document), after, "before={before:?}");
        }

        let mut crlf = DocumentSession::from_reader(
            0,
            "task",
            Cursor::new("title\r\n  +   task\r\nneighbor\r\n"),
        )
        .unwrap();
        let task_offset = "title\r\n  +".len();
        crlf.apply(EditorCommand::SetCaret {
            offset: task_offset,
            extend: false,
        })
        .unwrap();
        crlf.apply(EditorCommand::ToggleTaskDone).unwrap();
        assert_eq!(
            document_text(&crlf),
            "title\r\n  - [x] task\r\nneighbor\r\n"
        );
    }

    #[test]
    fn toggle_task_done_changes_only_top_selected_line_and_preserves_direction() {
        let original = "head\n  +   first\n🦀 second\nlast";
        let line_start = "head\n".len();
        let edit_start = line_start + "  ".len();
        let edit_end = line_start + "  +   ".len();
        let selection_low = edit_start + 1;
        let selection_high = "head\n  +   first\n🦀".len();
        let expected = "head\n  - [x] first\n🦀 second\nlast";
        let delta = "- [x] ".len() - (edit_end - edit_start);

        for reversed in [false, true] {
            let mut document =
                DocumentSession::from_reader(0, "task", Cursor::new(original)).unwrap();
            let (anchor, focus) = if reversed {
                (selection_high, selection_low)
            } else {
                (selection_low, selection_high)
            };
            document
                .apply(EditorCommand::SetSelection { anchor, focus })
                .unwrap();

            let toggled = document.apply(EditorCommand::ToggleTaskDone).unwrap();
            assert_eq!(document_text(&document), expected);
            let expected_low = selection_low;
            let expected_high = selection_high + delta;
            assert_eq!(
                toggled.selection,
                if reversed {
                    Selection::new(
                        ByteOffset::new(expected_high),
                        ByteOffset::new(expected_low),
                    )
                } else {
                    Selection::new(
                        ByteOffset::new(expected_low),
                        ByteOffset::new(expected_high),
                    )
                }
            );

            assert!(document.apply(EditorCommand::Undo).unwrap().text_changed);
            assert_eq!(document_text(&document), original);
            assert!(!document.apply(EditorCommand::Undo).unwrap().text_changed);
            assert!(document.apply(EditorCommand::Redo).unwrap().text_changed);
            assert_eq!(document_text(&document), expected);
        }
    }

    #[test]
    fn editor_commands_are_grapheme_safe_and_restore_selection_history() {
        let mut document = DocumentSession {
            target: DocumentTarget::WorkspaceNote(0),
            note_index: 0,
            title: "unicode".to_owned(),
            editor: Editor::new("a🦀e\u{301}\nlast"),
            file_version: None,
            disk_checksum: 0,
            autosave: AutosaveTracker::default(),
            protection: DocumentProtection::Plain,
            undo_group: None,
        };
        document.apply(EditorCommand::SelectAll).unwrap();
        let copied = document.apply(EditorCommand::Copy).unwrap();
        assert_eq!(copied.clipboard.as_deref(), Some("a🦀e\u{301}\nlast"));
        document
            .apply(EditorCommand::Insert("новый".to_owned()))
            .unwrap();
        assert_eq!(document.len_bytes(), "новый".len());
        document.apply(EditorCommand::Undo).unwrap();
        assert_eq!(document.len_bytes(), "a🦀e\u{301}\nlast".len());
        document.apply(EditorCommand::Redo).unwrap();
        document.apply(EditorCommand::Backspace).unwrap();
        assert_eq!(document.len_bytes(), "новы".len());
        document
            .apply(EditorCommand::Paste("й 🦀".to_owned()))
            .unwrap();
        let snapshot = document.viewport(ViewportRequest::default()).unwrap();
        assert_eq!(snapshot.lines[0].text, "новый 🦀");
    }

    #[test]
    fn editor_undo_groups_typing_and_deletion_by_time_and_interaction() {
        let mut document = DocumentSession::from_reader(0, "undo", Cursor::new("")).unwrap();
        for (now_ms, text) in [(0, "a"), (100, "b"), (200, "c")] {
            document
                .apply_at(EditorCommand::Insert(text.to_owned()), now_ms)
                .unwrap();
        }
        document.apply_at(EditorCommand::Undo, 250).unwrap();
        assert_eq!(document.len_bytes(), 0);
        document.apply_at(EditorCommand::Redo, 260).unwrap();

        document
            .apply_at(EditorCommand::Insert(" ".to_owned()), 300)
            .unwrap();
        document
            .apply_at(EditorCommand::Insert("d".to_owned()), 350)
            .unwrap();
        document.apply_at(EditorCommand::Undo, 400).unwrap();
        assert_eq!(
            document
                .editor
                .slice(ByteRange::new(ByteOffset::new(0), ByteOffset::new(4)).unwrap())
                .unwrap(),
            "abc "
        );
        document.apply_at(EditorCommand::Undo, 410).unwrap();
        assert_eq!(
            document
                .editor
                .slice(ByteRange::new(ByteOffset::new(0), ByteOffset::new(3)).unwrap())
                .unwrap(),
            "abc"
        );

        document
            .apply_at(EditorCommand::Insert("x".to_owned()), 2_000)
            .unwrap();
        document
            .apply_at(EditorCommand::Insert("y".to_owned()), 3_000)
            .unwrap();
        document.apply_at(EditorCommand::Undo, 3_100).unwrap();
        assert_eq!(
            document
                .editor
                .slice(ByteRange::new(ByteOffset::new(0), ByteOffset::new(4)).unwrap())
                .unwrap(),
            "abcx"
        );

        document.apply_at(EditorCommand::Backspace, 3_200).unwrap();
        document.apply_at(EditorCommand::Backspace, 3_250).unwrap();
        assert_eq!(document.len_bytes(), 2);
        document.apply_at(EditorCommand::Undo, 3_300).unwrap();
        assert_eq!(
            document
                .editor
                .slice(ByteRange::new(ByteOffset::new(0), ByteOffset::new(4)).unwrap())
                .unwrap(),
            "abcx"
        );
    }

    #[test]
    fn word_line_and_document_navigation_preserves_shift_anchor() {
        let text = "zero  один two\r\nlast";
        let mut document = DocumentSession::from_reader(0, "movement", Cursor::new(text)).unwrap();
        let inside_one = text.find("один").unwrap() + "о".len();
        document
            .apply(EditorCommand::SetCaret {
                offset: inside_one,
                extend: false,
            })
            .unwrap();
        document
            .apply(EditorCommand::MoveWordLeft { extend: false })
            .unwrap();
        assert_eq!(
            document.selection().focus().get(),
            text.find("один").unwrap()
        );
        document
            .apply(EditorCommand::MoveWordRight { extend: true })
            .unwrap();
        assert_eq!(
            document.selection().anchor().get(),
            text.find("один").unwrap()
        );
        assert_eq!(
            document.selection().focus().get(),
            text.find("один").unwrap() + "один".len()
        );

        document
            .apply(EditorCommand::MoveLineEnd { extend: false })
            .unwrap();
        assert_eq!(document.selection().focus().get(), text.find('\r').unwrap());
        document
            .apply(EditorCommand::MoveLineStart { extend: true })
            .unwrap();
        assert_eq!(document.selection().focus().get(), 0);
        assert_eq!(
            document.selection().anchor().get(),
            text.find('\r').unwrap()
        );
        document
            .apply(EditorCommand::MoveDocumentEnd { extend: false })
            .unwrap();
        assert_eq!(document.selection().focus().get(), text.len());
        document
            .apply(EditorCommand::MoveDocumentStart { extend: true })
            .unwrap();
        assert_eq!(
            document.selection(),
            Selection::new(ByteOffset::new(text.len()), ByteOffset::new(0))
        );
    }

    #[test]
    fn vertical_and_directed_selection_movement_stays_on_boundaries() {
        let mut document = DocumentSession {
            target: DocumentTarget::WorkspaceNote(0),
            note_index: 0,
            title: "movement".to_owned(),
            editor: Editor::new("абв\nx🦀z\nq"),
            file_version: None,
            disk_checksum: 0,
            autosave: AutosaveTracker::default(),
            protection: DocumentProtection::Plain,
            undo_group: None,
        };
        for _ in 0..3 {
            document
                .apply(EditorCommand::MoveRight { extend: false })
                .unwrap();
        }
        document
            .apply(EditorCommand::MoveDown { extend: true })
            .unwrap();
        let selection = document.selection();
        assert!(selection.anchor() < selection.focus());
        document
            .apply(EditorCommand::MoveLeft { extend: false })
            .unwrap();
        assert!(document.selection().is_caret());
    }

    #[test]
    fn direct_caret_movement_validates_boundaries_and_can_extend_selection() {
        let mut document = DocumentSession {
            target: DocumentTarget::WorkspaceNote(0),
            note_index: 0,
            title: "pointer movement".to_owned(),
            editor: Editor::new("alpha\n🦀 beta"),
            file_version: None,
            disk_checksum: 0,
            autosave: AutosaveTracker::default(),
            protection: DocumentProtection::Plain,
            undo_group: None,
        };

        let moved = document
            .apply(EditorCommand::SetCaret {
                offset: 5,
                extend: false,
            })
            .unwrap();
        assert!(moved.selection_changed);
        assert!(!moved.text_changed);
        assert_eq!(moved.content_revision, 0);
        assert_eq!(document.selection(), Selection::caret(ByteOffset::new(5)));

        let repeated = document
            .apply(EditorCommand::SetCaret {
                offset: 5,
                extend: false,
            })
            .unwrap();
        assert!(!repeated.selection_changed);
        assert!(!repeated.text_changed);
        assert_eq!(repeated.content_revision, 0);

        document
            .apply(EditorCommand::SetCaret {
                offset: 10,
                extend: true,
            })
            .unwrap();
        assert_eq!(
            document.selection(),
            Selection::new(ByteOffset::new(5), ByteOffset::new(10))
        );

        let collapsed = document
            .apply(EditorCommand::SetCaret {
                offset: 10,
                extend: false,
            })
            .unwrap();
        assert!(collapsed.selection_changed);
        assert!(!collapsed.text_changed);
        assert_eq!(collapsed.content_revision, 0);
        assert_eq!(document.selection(), Selection::caret(ByteOffset::new(10)));

        document
            .apply(EditorCommand::SetCaret {
                offset: 5,
                extend: true,
            })
            .unwrap();
        assert_eq!(
            document.selection(),
            Selection::new(ByteOffset::new(10), ByteOffset::new(5))
        );

        let invalid = document.apply(EditorCommand::SetCaret {
            offset: 8,
            extend: false,
        });
        assert!(matches!(invalid, Err(CoreError::Editor(_))));
        assert_eq!(
            document.selection(),
            Selection::new(ByteOffset::new(10), ByteOffset::new(5))
        );

        let out_of_range = document.apply(EditorCommand::SetCaret {
            offset: usize::MAX,
            extend: false,
        });
        assert!(matches!(out_of_range, Err(CoreError::Editor(_))));
        assert_eq!(
            document.selection(),
            Selection::new(ByteOffset::new(10), ByteOffset::new(5))
        );
        assert_eq!(document.len_bytes(), "alpha\n🦀 beta".len());
        assert_eq!(document.save_status(), SaveStatus::Clean { revision: 0 });
    }

    #[test]
    fn direct_selection_is_unicode_safe_and_does_not_create_a_revision() {
        let mut document = DocumentSession {
            target: DocumentTarget::WorkspaceNote(0),
            note_index: 0,
            title: "selection".to_owned(),
            editor: Editor::new("alpha Привет omega"),
            file_version: None,
            disk_checksum: 0,
            autosave: AutosaveTracker::default(),
            protection: DocumentProtection::Plain,
            undo_group: None,
        };
        let start = "alpha ".len();
        let end = start + "Привет".len();

        let selected = document
            .apply_at(
                EditorCommand::SetSelection {
                    anchor: start,
                    focus: end,
                },
                42,
            )
            .unwrap();
        assert!(selected.selection_changed);
        assert!(!selected.text_changed);
        assert_eq!(selected.content_revision, 0);
        assert_eq!(document.save_status(), SaveStatus::Clean { revision: 0 });
        assert_eq!(document.recovery_status(), RecoveryStatus::None);

        let copied = document.apply_at(EditorCommand::Copy, 43).unwrap();
        assert_eq!(copied.clipboard.as_deref(), Some("Привет"));
        assert!(!copied.text_changed);
        assert_eq!(copied.content_revision, 0);

        let selection_before = document.selection();
        let invalid = document.apply_at(
            EditorCommand::SetSelection {
                anchor: start,
                focus: start + 1,
            },
            44,
        );
        assert!(matches!(invalid, Err(CoreError::Editor(_))));
        assert_eq!(document.selection(), selection_before);
        assert_eq!(document.save_status(), SaveStatus::Clean { revision: 0 });
    }

    #[test]
    fn autosave_tracks_revisions_and_never_cleans_a_newer_edit() {
        let workspace = TestWorkspace::new();
        workspace.write_note(
            "note.md",
            "---\ntitle: Note\ncreated: '2022-02-03T18:57:43.598Z'\nfuture: keep\n---\nbody\n",
        );
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.open_note(0).unwrap();

        let outcome = session
            .document_mut()
            .unwrap()
            .apply_at(EditorCommand::Insert("first ".to_owned()), 10)
            .unwrap();
        assert!(outcome.text_changed);
        assert_eq!(outcome.content_revision, 1);
        assert_eq!(
            session.document().unwrap().save_status(),
            SaveStatus::Dirty {
                revision: 1,
                deadline_ms: 10 + AUTOSAVE_DEBOUNCE_MS,
            }
        );
        assert!(
            session
                .begin_autosave(759, "2026-09-01T12:00:00.000Z".to_owned())
                .unwrap()
                .is_none()
        );

        let first_job = session
            .begin_autosave(760, "2026-09-01T12:00:00.000Z".to_owned())
            .unwrap()
            .unwrap();
        assert_eq!(first_job.revision(), 1);
        assert_eq!(
            session.document().unwrap().save_status(),
            SaveStatus::Saving {
                revision: 1,
                dirty_after_start: false,
            }
        );
        session
            .document_mut()
            .unwrap()
            .apply_at(EditorCommand::Insert("second ".to_owned()), 800)
            .unwrap();
        assert_eq!(
            session.document().unwrap().save_status(),
            SaveStatus::Saving {
                revision: 1,
                dirty_after_start: true,
            }
        );

        session.finish_autosave(first_job.execute()).unwrap();
        assert_eq!(
            session.document().unwrap().save_status(),
            SaveStatus::Dirty {
                revision: 2,
                deadline_ms: 800 + AUTOSAVE_DEBOUNCE_MS,
            }
        );
        let first_path = session.notes()[session.selected_note().unwrap()]
            .path
            .clone();
        let first_disk = fs::read_to_string(first_path).unwrap();
        assert!(first_disk.ends_with("first body\n"));
        assert!(!first_disk.contains("second"));
        assert!(first_disk.contains("future: keep\n"));

        let second_job = session
            .begin_autosave(1_550, "2026-09-01T12:00:01.000Z".to_owned())
            .unwrap()
            .unwrap();
        session.finish_autosave(second_job.execute()).unwrap();
        assert_eq!(
            session.document().unwrap().save_status(),
            SaveStatus::Clean { revision: 2 }
        );
        let second_path = session.notes()[session.selected_note().unwrap()]
            .path
            .clone();
        let second_disk = fs::read_to_string(second_path).unwrap();
        assert!(second_disk.ends_with("first second body\n"));
        assert!(second_disk.contains("modified: '2026-09-01T12:00:01.000Z'\n"));
        assert!(second_disk.contains("created: '2022-02-03T18:57:43.598Z'\n"));
    }

    #[test]
    fn autosave_conflict_is_visible_retryable_and_keeps_local_buffer() {
        let workspace = TestWorkspace::new();
        workspace.write_note("a.md", "---\ntitle: A\n---\nbody\n");
        workspace.write_note("b.md", "---\ntitle: B\n---\nother\n");
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.open_note(0).unwrap();
        session
            .document_mut()
            .unwrap()
            .apply_at(EditorCommand::Insert("local ".to_owned()), 0)
            .unwrap();
        assert_eq!(session.open_note(1), Err(CoreError::UnsavedChanges));

        fs::write(
            workspace.note_path("a.md"),
            "---\ntitle: External\n---\nexternal\n",
        )
        .unwrap();
        let job = session
            .begin_autosave(750, "2026-09-01T12:00:00.000Z".to_owned())
            .unwrap()
            .unwrap();
        session.finish_autosave(job.execute()).unwrap();
        assert!(matches!(
            session.document().unwrap().save_status(),
            SaveStatus::Error { ref message, .. } if message.contains("changed")
        ));
        let snapshot = session
            .document()
            .unwrap()
            .viewport(ViewportRequest::default())
            .unwrap();
        assert_eq!(snapshot.lines[0].text, "local body");
        assert!(session.retry_autosave(1_000));
        assert!(
            session
                .begin_autosave(1_000, "2026-09-01T12:00:01.000Z".to_owned())
                .unwrap()
                .is_some()
        );
        assert_eq!(
            fs::read_to_string(workspace.note_path("a.md")).unwrap(),
            "---\ntitle: External\n---\nexternal\n"
        );
    }

    #[test]
    fn utc_timestamp_is_rfc3339_with_milliseconds() {
        assert_eq!(
            format_utc_timestamp(UNIX_EPOCH).unwrap(),
            "1970-01-01T00:00:00.000Z"
        );
        assert_eq!(
            format_utc_timestamp(UNIX_EPOCH + std::time::Duration::from_millis(1_709_164_800_123))
                .unwrap(),
            "2024-02-29T00:00:00.123Z"
        );
    }

    #[test]
    fn recovery_survives_restart_restores_explicitly_and_cleans_after_save() {
        let workspace = TestWorkspace::new();
        workspace.write_note(
            "note.md",
            "---\ntitle: Note\ncreated: '2022-02-03T18:57:43.598Z'\n---\nbody\n",
        );
        {
            let mut session = WorkspaceSession::open(workspace.path()).unwrap();
            session.open_note(0).unwrap();
            session
                .document_mut()
                .unwrap()
                .apply_at(EditorCommand::Insert("recovered ".to_owned()), 0)
                .unwrap();
            assert!(
                session
                    .begin_persistence(199, "2026-09-01T00:00:00.000Z".to_owned())
                    .unwrap()
                    .is_none()
            );
            let PersistenceJob::Recovery(job) = session
                .begin_persistence(200, "2026-09-01T00:00:00.000Z".to_owned())
                .unwrap()
                .unwrap()
            else {
                panic!("recovery must run before autosave");
            };
            session
                .finish_persistence(PersistenceCompletion::Recovery(job.execute()))
                .unwrap();
            assert_eq!(
                session.document().unwrap().recovery_status(),
                RecoveryStatus::Saved { revision: 1 }
            );
        }

        let mut restarted = WorkspaceSession::open(workspace.path()).unwrap();
        assert!(restarted.notes()[0].recovery_available);
        restarted.restore_recovery(0, 1_000).unwrap();
        assert_eq!(
            restarted
                .document()
                .unwrap()
                .viewport(ViewportRequest::default())
                .unwrap()
                .lines[0]
                .text,
            "recovered body"
        );
        assert!(matches!(
            restarted.document().unwrap().save_status(),
            SaveStatus::Dirty { revision: 1, .. }
        ));
        let PersistenceJob::Save(job) = restarted
            .begin_persistence(1_750, "2026-09-01T00:00:01.000Z".to_owned())
            .unwrap()
            .unwrap()
        else {
            panic!("restored buffer should autosave after debounce");
        };
        restarted
            .finish_persistence(PersistenceCompletion::Save(job.execute()))
            .unwrap();
        assert!(!restarted.notes()[0].recovery_available);
        assert!(
            RecoveryStore::new(workspace.path())
                .scan()
                .records
                .is_empty()
        );
        let saved_path = restarted.notes()[restarted.selected_note().unwrap()]
            .path
            .clone();
        assert!(
            fs::read_to_string(saved_path)
                .unwrap()
                .ends_with("recovered body\n")
        );
    }

    #[cfg(unix)]
    #[test]
    fn successful_save_hides_stale_recovery_when_cleanup_fails_and_keeps_diagnostic() {
        use std::os::unix::fs::symlink;

        let workspace = TestWorkspace::new();
        workspace.write_note("note.md", "---\ntitle: Note\n---\nbody\n");
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.open_note(0).unwrap();
        session
            .document_mut()
            .unwrap()
            .apply_at(EditorCommand::Insert("saved ".to_owned()), 0)
            .unwrap();
        let recovery = session
            .begin_persistence(RECOVERY_DEBOUNCE_MS, "ignored".to_owned())
            .unwrap()
            .unwrap();
        assert!(matches!(recovery, PersistenceJob::Recovery(_)));
        session.finish_persistence(recovery.execute()).unwrap();
        assert!(session.notes()[0].recovery_available);

        let save = session
            .begin_persistence(AUTOSAVE_DEBOUNCE_MS, "2026-09-02T00:00:00.000Z".to_owned())
            .unwrap()
            .unwrap();
        assert!(matches!(save, PersistenceJob::Save(_)));
        let recovery_path = fs::read_dir(workspace.path().join(".stillus/recovery"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let retained = workspace.path().join("retained-recovery");
        fs::rename(&recovery_path, &retained).unwrap();
        symlink(&retained, &recovery_path).unwrap();

        let PersistenceCompletion::Save(completion) = save.execute() else {
            panic!("save job returned a recovery completion");
        };
        assert!(completion.result.is_ok());
        assert!(completion.cleanup_error.is_some());
        session
            .finish_persistence(PersistenceCompletion::Save(completion))
            .unwrap();
        assert_eq!(
            session.document().unwrap().save_status(),
            SaveStatus::Clean { revision: 1 }
        );
        assert!(!session.notes()[0].recovery_available);
        assert!(matches!(
            session.document().unwrap().recovery_status(),
            RecoveryStatus::Error { .. }
        ));
        assert!(
            session
                .recovery_diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.contains("canonical save succeeded"))
        );
        assert!(
            recovery_path
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!fs::read(&retained).unwrap().is_empty());

        let restarted = WorkspaceSession::open(workspace.path()).unwrap();
        assert!(!restarted.notes()[0].recovery_available);
        assert!(!restarted.recovery_diagnostics().is_empty());
    }

    #[test]
    fn relocated_save_does_not_attach_old_path_recovery_after_cleanup_failure() {
        let workspace = TestWorkspace::new();
        workspace.write_note("note.md", "---\ntitle: Note\n---\nbody\n");
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.open_note(0).unwrap();
        session
            .document_mut()
            .unwrap()
            .apply_at(EditorCommand::Insert("saved ".to_owned()), 0)
            .unwrap();
        let recovery = session
            .begin_persistence(RECOVERY_DEBOUNCE_MS, "ignored".to_owned())
            .unwrap()
            .unwrap();
        session.finish_persistence(recovery.execute()).unwrap();
        let recovery_path = fs::read_dir(workspace.path().join(".stillus/recovery"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let recovery_bytes = fs::read(&recovery_path).unwrap();

        let save = session
            .begin_persistence(AUTOSAVE_DEBOUNCE_MS, "2026-09-02T00:00:00.000Z".to_owned())
            .unwrap()
            .unwrap();
        let PersistenceCompletion::Save(mut completion) = save.execute() else {
            panic!("save job returned a recovery completion");
        };
        assert!(completion.result.is_ok());
        fs::write(&recovery_path, recovery_bytes).unwrap();
        completion.cleanup_error = Some("injected cleanup failure".to_owned());
        session
            .finish_persistence(PersistenceCompletion::Save(completion))
            .unwrap();

        assert!(!session.notes()[0].recovery_available);
        assert!(!WorkspaceSession::open(workspace.path()).unwrap().notes()[0].recovery_available);
    }

    #[test]
    fn protected_autosave_precedes_recovery_and_success_cancels_snapshot() {
        let workspace = TestWorkspace::new();
        let password = MasterPassword::new("protected scheduling password".to_owned());
        workspace.write_protected_note(
            "ntrm-11111111111111111111111111111111.md",
            "scheduled.md",
            b"---\ntitle: Scheduled\n---\nbody\n",
            &password,
        );
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.unlock_note(0, password).unwrap();
        session
            .document_mut()
            .unwrap()
            .apply_at(EditorCommand::Insert("saved ".to_owned()), 0)
            .unwrap();

        assert_eq!(
            session.next_persistence_deadline(),
            Some(AUTOSAVE_DEBOUNCE_MS)
        );
        assert!(
            session
                .begin_persistence(RECOVERY_DEBOUNCE_MS, "ignored".to_owned())
                .unwrap()
                .is_none()
        );
        let save = session
            .begin_persistence(AUTOSAVE_DEBOUNCE_MS, "2026-09-02T00:00:00.000Z".to_owned())
            .unwrap()
            .unwrap();
        assert!(matches!(save, PersistenceJob::Save(_)));
        session.finish_persistence(save.execute()).unwrap();
        assert_eq!(
            session.document().unwrap().save_status(),
            SaveStatus::Clean { revision: 1 }
        );
        assert!(
            session
                .begin_persistence(PROTECTED_RECOVERY_DEBOUNCE_MS, "ignored".to_owned())
                .unwrap()
                .is_none()
        );
        assert!(!workspace.path().join(".stillus/recovery").exists());
    }

    #[test]
    fn protected_integrity_retry_keeps_revision_and_recovery_until_verified() {
        let workspace = TestWorkspace::new();
        let password = MasterPassword::new("integrity retry password".to_owned());
        workspace.write_protected_note(
            "ntrm-33333333333333333333333333333333.md",
            "retry.md",
            b"---\ntitle: Retry\n---\nold body\n",
            &password,
        );
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.unlock_note(0, password).unwrap();
        session
            .document_mut()
            .unwrap()
            .apply_at(EditorCommand::SelectAll, 0)
            .unwrap();
        session
            .document_mut()
            .unwrap()
            .apply_at(EditorCommand::Insert("new secret body".to_owned()), 0)
            .unwrap();

        session.document_mut().unwrap().autosave.error = Some(SaveError::InvalidTarget(
            "force recovery before integrity test".to_owned(),
        ));
        let recovery = session
            .begin_persistence(PROTECTED_RECOVERY_DEBOUNCE_MS, "ignored".to_owned())
            .unwrap()
            .unwrap();
        assert!(matches!(recovery, PersistenceJob::Recovery(_)));
        session.finish_persistence(recovery.execute()).unwrap();
        assert!(session.notes()[0].recovery_available);

        assert!(session.retry_autosave(PROTECTED_RECOVERY_DEBOUNCE_MS));
        fs::write(
            workspace
                .path()
                .join(".stillus/test-corrupt-protected-save"),
            b"once",
        )
        .unwrap();
        let save = session
            .begin_persistence(
                PROTECTED_RECOVERY_DEBOUNCE_MS,
                "2026-09-04T00:00:00.000Z".to_owned(),
            )
            .unwrap()
            .unwrap();
        session.finish_persistence(save.execute()).unwrap();
        assert!(session.integrity_failure().is_some());
        assert!(session.notes()[0].recovery_available);
        assert!(matches!(
            session.document().unwrap().save_status(),
            SaveStatus::Error { .. }
        ));

        let retry = session
            .begin_integrity_resolution(IntegrityResolution::Retry)
            .unwrap();
        assert_eq!(
            session.finish_secure_operation(retry.execute()).unwrap(),
            SecureOutcome::IntegrityRetried
        );
        assert!(session.integrity_failure().is_none());
        assert_eq!(
            session.document().unwrap().save_status(),
            SaveStatus::Clean { revision: 1 }
        );
        assert!(!session.notes()[0].recovery_available);
    }

    #[test]
    fn protected_integrity_journal_survives_restart_and_restore_discards_failed_buffer() {
        let workspace = TestWorkspace::new();
        let password = MasterPassword::new("integrity restore password".to_owned());
        let path = workspace.write_protected_note(
            "ntrm-44444444444444444444444444444444.md",
            "restore.md",
            b"---\ntitle: Restore\n---\nverified old body\n",
            &password,
        );
        let original = fs::read(&path).unwrap();
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.unlock_note(0, password).unwrap();
        session
            .document_mut()
            .unwrap()
            .apply_at(EditorCommand::SelectAll, 0)
            .unwrap();
        session
            .document_mut()
            .unwrap()
            .apply_at(
                EditorCommand::Insert("discard this failed edit".to_owned()),
                0,
            )
            .unwrap();
        fs::create_dir_all(workspace.path().join(".stillus")).unwrap();
        fs::write(
            workspace
                .path()
                .join(".stillus/test-corrupt-protected-save"),
            b"once",
        )
        .unwrap();
        let save = session
            .begin_persistence(AUTOSAVE_DEBOUNCE_MS, "2026-09-04T00:00:00.000Z".to_owned())
            .unwrap()
            .unwrap();
        session.finish_persistence(save.execute()).unwrap();
        assert!(session.integrity_failure().is_some());
        drop(session);

        let mut restarted = WorkspaceSession::open(workspace.path()).unwrap();
        assert!(restarted.integrity_failure().is_some());
        assert!(
            restarted
                .begin_integrity_resolution(IntegrityResolution::Retry)
                .is_err()
        );
        let restore = restarted
            .begin_integrity_resolution(IntegrityResolution::Restore)
            .unwrap();
        assert_eq!(
            restarted
                .finish_secure_operation(restore.execute())
                .unwrap(),
            SecureOutcome::IntegrityRestored(path.clone())
        );
        assert!(restarted.integrity_failure().is_none());
        assert!(restarted.document().is_none());
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn protected_save_conflict_still_persists_delayed_encrypted_recovery() {
        let workspace = TestWorkspace::new();
        let password = MasterPassword::new("protected conflict recovery password".to_owned());
        let path = workspace.write_protected_note(
            "ntrm-22222222222222222222222222222222.md",
            "conflict.md",
            b"---\ntitle: Conflict\n---\nbody\n",
            &password,
        );
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.unlock_note(0, password.clone()).unwrap();
        session
            .document_mut()
            .unwrap()
            .apply_at(EditorCommand::Insert("local-secret-marker ".to_owned()), 0)
            .unwrap();
        let save = session
            .begin_persistence(AUTOSAVE_DEBOUNCE_MS, "2026-09-02T00:00:00.000Z".to_owned())
            .unwrap()
            .unwrap();
        workspace.replace_protected_note(
            &path,
            "conflict.md",
            b"---\ntitle: External\n---\nexternal\n",
            &password,
        );
        session.finish_persistence(save.execute()).unwrap();
        assert!(matches!(
            session.document().unwrap().save_status(),
            SaveStatus::Error { .. }
        ));
        assert!(
            session
                .begin_persistence(PROTECTED_RECOVERY_DEBOUNCE_MS - 1, "ignored".to_owned())
                .unwrap()
                .is_none()
        );
        let recovery = session
            .begin_persistence(PROTECTED_RECOVERY_DEBOUNCE_MS, "ignored".to_owned())
            .unwrap()
            .unwrap();
        assert!(matches!(recovery, PersistenceJob::Recovery(_)));
        session.finish_persistence(recovery.execute()).unwrap();
        assert!(session.notes()[0].recovery_available);
        assert_eq!(
            session.document().unwrap().recovery_status(),
            RecoveryStatus::Saved { revision: 1 }
        );
        let recovery_bytes = fs::read(
            fs::read_dir(workspace.path().join(".stillus/recovery"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        assert!(!contains_bytes(&recovery_bytes, b"local-secret-marker"));
    }

    #[test]
    fn external_poll_reloads_clean_and_preserves_both_dirty_conflict_versions() {
        let workspace = TestWorkspace::new();
        workspace.write_note("note.md", "---\ntitle: Note\n---\noriginal\n");
        let path = workspace.note_path("note.md");
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.open_note(0).unwrap();

        fs::write(&path, "---\ntitle: Note\n---\nexternal clean\n").unwrap();
        assert_eq!(session.poll_external(10).unwrap(), ExternalPoll::Reloaded);
        assert_eq!(
            session
                .document()
                .unwrap()
                .viewport(ViewportRequest::default())
                .unwrap()
                .lines[0]
                .text,
            "external clean"
        );

        session
            .document_mut()
            .unwrap()
            .apply_at(EditorCommand::Insert("local ".to_owned()), 20)
            .unwrap();
        let PersistenceJob::Recovery(job) = session
            .begin_persistence(220, "2026-09-01T00:00:00.000Z".to_owned())
            .unwrap()
            .unwrap()
        else {
            panic!("expected recovery job");
        };
        session
            .finish_persistence(PersistenceCompletion::Recovery(job.execute()))
            .unwrap();
        fs::write(&path, "---\ntitle: Note\n---\nexternal dirty\n").unwrap();
        assert_eq!(session.poll_external(300).unwrap(), ExternalPoll::Conflict);
        assert!(matches!(
            session.document().unwrap().save_status(),
            SaveStatus::Conflict { .. }
        ));
        assert_eq!(session.next_persistence_deadline(), None);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "---\ntitle: Note\n---\nexternal dirty\n"
        );
        assert_eq!(
            session
                .document()
                .unwrap()
                .viewport(ViewportRequest::default())
                .unwrap()
                .lines[0]
                .text,
            "local external clean"
        );
        assert_eq!(RecoveryStore::new(workspace.path()).scan().records.len(), 1);

        session.discard_local_and_reload().unwrap();
        assert_eq!(
            session
                .document()
                .unwrap()
                .viewport(ViewportRequest::default())
                .unwrap()
                .lines[0]
                .text,
            "external dirty"
        );
        assert!(
            RecoveryStore::new(workspace.path())
                .scan()
                .records
                .is_empty()
        );
    }

    #[test]
    fn note_and_category_operations_refresh_canonical_workspace_state() {
        let workspace = TestWorkspace::new();
        workspace.write_note(
            "old.md",
            "---\ntitle: Old\ntags: [Work]\npinned: false\nfavorited: false\ncreated: '2022-02-03T18:57:43.598Z'\norder: {'Work': 4, 'Keep': 8}\nfuture: keep\n---\n# Body\nbytes\n",
        );
        let timestamp = "2026-09-01T12:34:56.789Z";
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.open_note(0).unwrap();

        assert!(session.add_tag_selected("Задачи", timestamp).unwrap());
        assert!(!session.add_tag_selected("Задачи", timestamp).unwrap());
        assert_eq!(
            session.categories(),
            &[
                CategorySummary {
                    name: "Work".to_owned(),
                    note_count: 1,
                },
                CategorySummary {
                    name: "Задачи".to_owned(),
                    note_count: 1,
                },
            ]
        );
        assert!(session.remove_tag_selected("Work", timestamp).unwrap());
        assert!(!session.remove_tag_selected("Missing", timestamp).unwrap());
        assert_eq!(session.categories()[0].name, "Задачи");
        assert!(session.toggle_pinned_selected(timestamp).unwrap());
        assert!(session.toggle_favorited_selected(timestamp).unwrap());

        session.rename_selected("Renamed", timestamp).unwrap();
        assert!(!workspace.note_path("old.md").exists());
        let renamed = workspace.note_path("Renamed.md");
        let output = fs::read_to_string(&renamed).unwrap();
        assert!(output.contains("title: 'Renamed'\n"));
        assert!(output.contains("tags:\n  - 'Задачи'\n"));
        assert!(output.contains("pinned: true\n"));
        assert!(output.contains("favorited: true\n"));
        assert!(output.contains("order:\n  'Keep': 8\n"));
        assert!(!output.contains("'Work': 4"));
        assert!(output.contains("future: keep\n"));
        assert!(output.ends_with("# Body\nbytes\n"));
        assert_eq!(session.notes()[0].path, renamed);

        let created_index = session.create_note("Created", timestamp).unwrap();
        assert_eq!(session.selected_note(), Some(created_index));
        assert_eq!(session.document().unwrap().title(), "Created");
        assert_eq!(session.notes().len(), 2);
        let created = workspace.note_path("Created.md");
        assert!(created.exists());
        let deleted_path = session.set_deleted_selected(true, timestamp).unwrap();
        assert_eq!(deleted_path, created);
        assert!(created.exists());
        assert!(
            fs::read_to_string(&created)
                .unwrap()
                .contains("deleted: true\n")
        );
        assert!(renamed.exists());
        assert_eq!(session.notes().len(), 2);
        assert!(session.notes().iter().any(|note| note.deleted));
        assert_eq!(session.selected_note(), None);
        assert!(session.document().is_none());

        let deleted_index = session
            .notes()
            .iter()
            .position(|note| note.deleted)
            .unwrap();
        session.open_note(deleted_index).unwrap();
        session.set_deleted_selected(false, timestamp).unwrap();
        assert!(!session.notes()[session.selected_note().unwrap()].deleted);
        assert!(
            fs::read_to_string(&created)
                .unwrap()
                .contains("deleted: false\n")
        );
    }

    #[test]
    fn category_note_order_is_canonical_per_tag_and_separate_per_pin_partition() {
        let workspace = TestWorkspace::new();
        workspace.write_note(
            "a.md",
            "---\ntitle: Alpha\ntags: [Work, Shared]\npinned: true\nmodified: original-a\norder: {'Shared': 7}\n---\nAlpha\n",
        );
        workspace.write_note(
            "b.md",
            "---\ntitle: Beta\ntags: [Work]\npinned: true\nmodified: original-b\n---\nBeta\n",
        );
        workspace.write_note(
            "c.md",
            "---\ntitle: Charlie\ntags: [Work]\npinned: false\nmodified: original-c\n---\nCharlie\n",
        );
        workspace.write_note(
            "d.md",
            "---\ntitle: Delta\ntags: [Work]\npinned: false\nmodified: original-d\n---\nDelta\n",
        );
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        let path = |name: &str| workspace.note_path(name);
        assert!(matches!(
            session.clear_category_note_order(FAVORITED_ORDER_KEY),
            Err(CoreError::NoteUnavailable(_))
        ));
        assert!(
            session
                .set_category_note_order(
                    "Work",
                    &[path("b.md"), path("a.md"), path("d.md"), path("c.md")],
                )
                .unwrap()
        );

        let order = |name: &str| {
            session
                .notes()
                .iter()
                .find(|note| note.path == path(name))
                .unwrap()
                .order["Work"]
        };
        assert_eq!((order("b.md"), order("a.md")), (0, 1));
        assert_eq!((order("d.md"), order("c.md")), (0, 1));
        assert_eq!(
            session
                .notes()
                .iter()
                .find(|note| note.path == path("a.md"))
                .unwrap()
                .order
                .get("Shared"),
            Some(&7)
        );
        assert!(
            fs::read_to_string(path("a.md"))
                .unwrap()
                .contains("modified: original-a\n")
        );

        assert!(session.clear_category_note_order("Work").unwrap());
        let alpha = session
            .notes()
            .iter()
            .find(|note| note.path == path("a.md"))
            .unwrap();
        assert_eq!(alpha.order, BTreeMap::from([("Shared".to_owned(), 7)]));
        assert!(
            fs::read_to_string(path("a.md"))
                .unwrap()
                .contains("'Shared': 7\n")
        );
        assert!(
            session
                .notes()
                .iter()
                .filter(|note| note.path != path("a.md"))
                .all(|note| note.order.is_empty())
        );
        assert!(!session.clear_category_note_order("Work").unwrap());
    }

    #[test]
    fn favorited_note_order_uses_reserved_key_and_is_removed_when_unfavorited() {
        let workspace = TestWorkspace::new();
        workspace.write_note(
            "a.md",
            "---\ntitle: Alpha\ntags: [Work]\npinned: true\nfavorited: true\n---\nAlpha\n",
        );
        workspace.write_note(
            "b.md",
            "---\ntitle: Beta\ntags: [Work]\npinned: true\nfavorited: true\n---\nBeta\n",
        );
        workspace.write_note(
            "c.md",
            "---\ntitle: Charlie\ntags: [Work]\nfavorited: true\n---\nCharlie\n",
        );
        workspace.write_note(
            "plain.md",
            "---\ntitle: Plain\ntags: [Work]\nfavorited: false\n---\nPlain\n",
        );
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        let path = |name: &str| workspace.note_path(name);
        assert!(
            session
                .set_favorited_note_order(&[path("b.md"), path("a.md"), path("c.md")])
                .unwrap()
        );

        let order = |session: &WorkspaceSession, name: &str| {
            session
                .notes()
                .iter()
                .find(|note| note.path == path(name))
                .unwrap()
                .order
                .get(FAVORITED_ORDER_KEY)
                .copied()
        };
        assert_eq!(
            (
                order(&session, "b.md"),
                order(&session, "a.md"),
                order(&session, "c.md")
            ),
            (Some(0), Some(1), Some(0))
        );
        assert_eq!(order(&session, "plain.md"), None);

        let alpha_index = session
            .notes()
            .iter()
            .position(|note| note.path == path("a.md"))
            .unwrap();
        session.open_note(alpha_index).unwrap();
        assert!(
            !session
                .toggle_favorited_selected("2026-09-04T12:00:00Z")
                .unwrap()
        );
        assert_eq!(order(&session, "a.md"), None);
        assert!(
            !fs::read_to_string(path("a.md"))
                .unwrap()
                .contains("__favorited")
        );

        assert!(session.clear_favorited_note_order().unwrap());
        assert_eq!(order(&session, "b.md"), None);
        assert_eq!(order(&session, "c.md"), None);
        assert!(!session.clear_favorited_note_order().unwrap());
    }

    #[test]
    fn operations_reject_dirty_or_stale_selected_note_without_overwrite() {
        let workspace = TestWorkspace::new();
        let original = "---\ntitle: Note\ntags: []\n---\nbody\n";
        workspace.write_note("note.md", original);
        let timestamp = "2026-09-01T12:34:56.789Z";
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.open_note(0).unwrap();
        session
            .document_mut()
            .unwrap()
            .apply_at(EditorCommand::Insert("local ".to_owned()), 10)
            .unwrap();
        assert_eq!(
            session.rename_selected("No", timestamp),
            Err(CoreError::UnsavedChanges)
        );
        assert_eq!(
            session.create_note("No", timestamp),
            Err(CoreError::UnsavedChanges)
        );
        assert_eq!(
            session.set_deleted_selected(true, timestamp),
            Err(CoreError::UnsavedChanges)
        );
        assert_eq!(
            fs::read_to_string(workspace.note_path("note.md")).unwrap(),
            original
        );
        assert!(!workspace.note_path("No.md").exists());

        let mut fresh = WorkspaceSession::open(workspace.path()).unwrap();
        fresh.open_note(0).unwrap();
        let external = "---\ntitle: External\ntags: []\n---\nexternal\n";
        fs::write(workspace.note_path("note.md"), external).unwrap();
        assert!(matches!(
            fresh.toggle_pinned_selected(timestamp),
            Err(CoreError::Save(SaveError::Conflict))
        ));
        assert_eq!(
            fs::read_to_string(workspace.note_path("note.md")).unwrap(),
            external
        );
    }

    #[test]
    fn protected_note_keeps_metadata_visible_through_unlock_autosave_and_relock() {
        let workspace = TestWorkspace::new();
        let password = MasterPassword::new("correct horse battery staple".to_owned());
        let plaintext = b"---\ntitle: Secret Project\ntags: [Private]\npinned: true\n---\nSecret Project\nsecret-marker body\n";
        let protected_path = workspace.write_protected_note(
            "ntrm-4f842085313649d4a89538759f9f808e.md",
            "meaningful-secret-name.md",
            plaintext,
            &password,
        );
        let original_ciphertext = fs::read(&protected_path).unwrap();
        assert!(!contains_bytes(&original_ciphertext, b"secret-marker"));
        assert!(contains_bytes(&original_ciphertext, b"Secret Project"));
        assert!(contains_bytes(&original_ciphertext, b"Private"));
        assert!(contains_bytes(
            &original_ciphertext,
            b"stillus_encryption: age-body-v1"
        ));

        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        assert_eq!(session.notes().len(), 1);
        assert_eq!(session.notes()[0].title, "Secret Project");
        assert_eq!(session.notes()[0].tags, ["Private"]);
        assert_eq!(session.notes()[0].protection, NoteProtection::Protected);
        session.select_protected_note(0).unwrap();
        assert_eq!(session.selected_note(), Some(0));
        assert!(session.document().is_none());
        assert_eq!(session.open_note(0), Err(CoreError::MasterPasswordRequired));
        assert!(matches!(
            session.unlock_note(0, MasterPassword::new("wrong".to_owned())),
            Err(CoreError::Secure(_))
        ));
        assert_eq!(fs::read(&protected_path).unwrap(), original_ciphertext);

        session.unlock_note(0, password.clone()).unwrap();
        assert_eq!(session.notes()[0].title, "Secret Project");
        assert_eq!(session.notes()[0].tags, ["Private"]);
        assert!(session.document().unwrap().is_protected());
        session
            .document_mut()
            .unwrap()
            .apply_at(EditorCommand::SelectAll, 0)
            .unwrap();
        session
            .document_mut()
            .unwrap()
            .apply_at(
                EditorCommand::Insert("Edited Secret\nedited-secret-marker".to_owned()),
                0,
            )
            .unwrap();

        // A protected recovery intentionally trails canonical autosave. Force
        // the save-error branch here so this end-to-end security test still
        // exercises encrypted recovery before a successful retry.
        session.document_mut().unwrap().autosave.error = Some(SaveError::InvalidTarget(
            "injected pre-recovery save failure".to_owned(),
        ));

        let recovery = session
            .begin_persistence(PROTECTED_RECOVERY_DEBOUNCE_MS, "ignored".to_owned())
            .unwrap()
            .unwrap();
        assert!(matches!(recovery, PersistenceJob::Recovery(_)));
        session.finish_persistence(recovery.execute()).unwrap();
        let recovery_dir = workspace.path().join(".stillus/recovery");
        let recovery_bytes = fs::read(
            fs::read_dir(&recovery_dir)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        assert!(!contains_bytes(&recovery_bytes, b"edited-secret-marker"));

        assert!(session.retry_autosave(PROTECTED_RECOVERY_DEBOUNCE_MS));

        let save = session
            .begin_persistence(
                PROTECTED_RECOVERY_DEBOUNCE_MS,
                "2026-09-02T00:00:00.000Z".to_owned(),
            )
            .unwrap()
            .unwrap();
        assert!(matches!(save, PersistenceJob::Save(_)));
        session.finish_persistence(save.execute()).unwrap();
        let committed_path = session.notes()[session.selected_note().unwrap()]
            .path
            .clone();
        assert_eq!(committed_path, workspace.note_path("Edited Secret.md"));
        let ciphertext = fs::read(&committed_path).unwrap();
        assert!(!contains_bytes(&ciphertext, b"edited-secret-marker"));
        assert!(
            !workspace.path().join(".stillus/recovery").exists()
                || fs::read_dir(workspace.path().join(".stillus/recovery"))
                    .unwrap()
                    .next()
                    .is_none()
        );

        session.lock_selected().unwrap();
        assert!(session.document().is_none());
        assert!(!session.has_master_password());
        assert!(matches!(
            session.begin_open_protected_note(0),
            Err(CoreError::MasterPasswordRequired)
        ));
        assert_eq!(session.notes()[0].title, "Edited Secret");
        let mut reopened = WorkspaceSession::open(workspace.path()).unwrap();
        reopened.unlock_note(0, password).unwrap();
        let body = reopened
            .document()
            .unwrap()
            .viewport(ViewportRequest::default())
            .unwrap();
        assert_eq!(body.lines[0].text, "Edited Secret");
        assert_eq!(body.lines[1].text, "edited-secret-marker");
    }

    #[test]
    fn protected_metadata_and_external_conflict_never_publish_plaintext() {
        let workspace = TestWorkspace::new();
        let password = MasterPassword::new("vault password".to_owned());
        let path = workspace.write_protected_note(
            "ntrm-8d07e75a086a4c25bf02f015e9c69642.md",
            "old-secret-name.md",
            b"---\ntitle: Old Secret\ntags: []\n---\nbody-secret-marker\n",
            &password,
        );
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.unlock_note(0, password.clone()).unwrap();
        session
            .rename_selected("Renamed Secret", "2026-09-02T00:00:00.000Z")
            .unwrap();
        session
            .add_tag_selected("HiddenTag", "2026-09-02T00:00:01.000Z")
            .unwrap();
        session
            .toggle_favorited_selected("2026-09-02T00:00:02.000Z")
            .unwrap();
        assert_eq!(session.notes()[0].title, "Renamed Secret");
        assert_eq!(session.notes()[0].tags, ["HiddenTag"]);
        assert!(session.notes()[0].favorited);
        let renamed_path = session.notes()[session.selected_note().unwrap()]
            .path
            .clone();
        assert_eq!(renamed_path, workspace.note_path("Renamed Secret.md"));
        assert!(!path.exists());
        let bytes = fs::read(&renamed_path).unwrap();
        assert!(contains_bytes(&bytes, b"Renamed Secret"));
        assert!(contains_bytes(&bytes, b"HiddenTag"));
        assert!(!contains_bytes(&bytes, b"body-secret-marker"));

        session
            .document_mut()
            .unwrap()
            .apply_at(EditorCommand::Insert("local-marker".to_owned()), 10)
            .unwrap();
        workspace.replace_protected_note(
            &renamed_path,
            "external.md",
            b"---\ntitle: External\ntags: []\n---\nexternal-marker\n",
            &password,
        );
        assert_eq!(session.poll_external(11).unwrap(), ExternalPoll::Conflict);
        assert!(matches!(
            session.document().map(DocumentSession::save_status),
            Some(SaveStatus::Conflict { .. })
        ));
        assert!(!contains_bytes(
            &fs::read(&renamed_path).unwrap(),
            b"local-marker"
        ));
    }

    #[test]
    fn secure_unlock_runs_only_in_execute_and_publishes_only_in_finish() {
        let workspace = TestWorkspace::new();
        let password = MasterPassword::new("background unlock password".to_owned());
        let protected_path = workspace.write_protected_note(
            "ntrm-dddddddddddddddddddddddddddddddd.md",
            "background.md",
            b"---\ntitle: Background Secret\ntags: [Hidden]\n---\nbackground body\n",
            &password,
        );
        let original_ciphertext = fs::read(&protected_path).unwrap();
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();

        let wrong_job = session
            .begin_unlock_note(0, MasterPassword::new("wrong password".to_owned()))
            .unwrap();
        assert!(session.secure_operation_pending());
        assert!(session.document().is_none());
        assert_eq!(session.notes()[0].title, "Background Secret");
        assert_eq!(fs::read(&protected_path).unwrap(), original_ciphertext);

        let wrong_completion = wrong_job.execute();
        assert!(session.document().is_none());
        assert_eq!(session.notes()[0].title, "Background Secret");
        assert_eq!(fs::read(&protected_path).unwrap(), original_ciphertext);
        assert!(matches!(
            session.finish_secure_operation(wrong_completion),
            Err(CoreError::Secure(_))
        ));
        assert!(!session.secure_operation_pending());
        assert!(session.document().is_none());

        let job = session.begin_unlock_note(0, password).unwrap();
        assert!(session.document().is_none());
        let completion = job.execute();
        assert!(session.document().is_none());
        assert_eq!(session.notes()[0].title, "Background Secret");
        assert_eq!(
            session.finish_secure_operation(completion).unwrap(),
            SecureOutcome::Unlocked
        );
        assert_eq!(session.document().unwrap().title(), "background body");
        assert_eq!(session.notes()[0].tags, ["Hidden"]);
    }

    #[test]
    fn protected_external_reload_and_discard_publish_only_in_finish() {
        let workspace = TestWorkspace::new();
        let password = MasterPassword::new("background reload password".to_owned());
        let protected_path = workspace.write_protected_note(
            "ntrm-ffffffffffffffffffffffffffffffff.md",
            "reload.md",
            b"---\ntitle: Before Reload\n---\nbefore reload body\n",
            &password,
        );
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.unlock_note(0, password.clone()).unwrap();
        workspace.replace_protected_note(
            &protected_path,
            "reload.md",
            b"---\ntitle: After Reload\n---\nafter reload body\n",
            &password,
        );

        let ExternalPollStart::Secure(reload) = session.begin_poll_external(10).unwrap() else {
            panic!("clean protected external change must require a secure job");
        };
        assert_eq!(session.document().unwrap().title(), "before reload body");
        let reload_completion = reload.execute();
        assert_eq!(session.document().unwrap().title(), "before reload body");
        assert_eq!(
            session.finish_secure_operation(reload_completion).unwrap(),
            SecureOutcome::ExternalPoll(ExternalPoll::Reloaded)
        );
        assert_eq!(session.document().unwrap().title(), "after reload body");

        session
            .document_mut()
            .unwrap()
            .apply_at(EditorCommand::SelectAll, 20)
            .unwrap();
        session
            .document_mut()
            .unwrap()
            .apply_at(EditorCommand::Insert("local discarded body".to_owned()), 20)
            .unwrap();
        let discard = session.begin_discard_protected_local_and_reload().unwrap();
        assert!(
            session
                .begin_persistence(10_000, "2026-09-02T00:00:00.000Z".to_owned())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            session
                .document()
                .unwrap()
                .viewport(ViewportRequest::default())
                .unwrap()
                .lines[0]
                .text,
            "local discarded body"
        );
        let discard_completion = discard.execute();
        assert_eq!(
            session
                .document()
                .unwrap()
                .viewport(ViewportRequest::default())
                .unwrap()
                .lines[0]
                .text,
            "local discarded body"
        );
        assert_eq!(
            session.finish_secure_operation(discard_completion).unwrap(),
            SecureOutcome::DiscardedAndReloaded
        );
        assert_eq!(
            session
                .document()
                .unwrap()
                .viewport(ViewportRequest::default())
                .unwrap()
                .lines[0]
                .text,
            "after reload body"
        );
        assert!(!contains_bytes(
            &fs::read(&protected_path).unwrap(),
            b"local discarded body"
        ));
    }

    #[test]
    fn protected_recovery_restore_publishes_only_in_finish() {
        let workspace = TestWorkspace::new();
        let password = MasterPassword::new("background recovery password".to_owned());
        workspace.write_protected_note(
            "ntrm-abababababababababababababababab.md",
            "recovery.md",
            b"---\ntitle: Recovery Secret\n---\ncanonical body\n",
            &password,
        );
        {
            let mut session = WorkspaceSession::open(workspace.path()).unwrap();
            session.unlock_note(0, password.clone()).unwrap();
            session
                .document_mut()
                .unwrap()
                .apply_at(EditorCommand::SelectAll, 0)
                .unwrap();
            session
                .document_mut()
                .unwrap()
                .apply_at(
                    EditorCommand::Insert("recovered protected body".to_owned()),
                    0,
                )
                .unwrap();
            session.document_mut().unwrap().autosave.error = Some(SaveError::InvalidTarget(
                "injected save failure before recovery".to_owned(),
            ));
            let recovery = session
                .begin_persistence(PROTECTED_RECOVERY_DEBOUNCE_MS, "ignored".to_owned())
                .unwrap()
                .unwrap();
            assert!(matches!(recovery, PersistenceJob::Recovery(_)));
            session.finish_persistence(recovery.execute()).unwrap();
        }

        let mut restarted = WorkspaceSession::open(workspace.path()).unwrap();
        assert!(restarted.notes()[0].recovery_available);
        restarted.unlock_note(0, password).unwrap();
        let restore = restarted
            .begin_restore_protected_recovery(0, 10_000)
            .unwrap();
        assert_eq!(
            restarted
                .document()
                .unwrap()
                .viewport(ViewportRequest::default())
                .unwrap()
                .lines[0]
                .text,
            "canonical body"
        );
        let completion = restore.execute();
        assert_eq!(
            restarted
                .document()
                .unwrap()
                .viewport(ViewportRequest::default())
                .unwrap()
                .lines[0]
                .text,
            "canonical body"
        );
        assert_eq!(
            restarted.finish_secure_operation(completion).unwrap(),
            SecureOutcome::RecoveryRestored
        );
        assert_eq!(
            restarted
                .document()
                .unwrap()
                .viewport(ViewportRequest::default())
                .unwrap()
                .lines[0]
                .text,
            "recovered protected body"
        );
    }

    #[test]
    fn secure_mutations_use_begin_execute_finish_without_plaintext_leaks() {
        let workspace = TestWorkspace::new();
        let original = "---\ntitle: Async Vault\ntags: []\n---\nAsync Vault\nasync-secret-marker\n";
        let plain_path = workspace.note_path("async-vault.md");
        workspace.write_note("async-vault.md", original);
        let password = MasterPassword::new("background mutation password".to_owned());
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.open_note(0).unwrap();

        let protect = session
            .begin_protect_selected(Some(password.clone()))
            .unwrap();
        assert_eq!(fs::read_to_string(&plain_path).unwrap(), original);
        assert!(!session.document().unwrap().is_protected());
        let protect_completion = protect.execute();
        assert!(plain_path.exists());
        assert!(!session.document().unwrap().is_protected());
        let SecureOutcome::Protected(protected_path) =
            session.finish_secure_operation(protect_completion).unwrap()
        else {
            panic!("protect completion must publish the protected path");
        };
        assert!(session.document().is_none());
        assert_eq!(protected_path, plain_path);
        assert!(!contains_bytes(
            &fs::read(&protected_path).unwrap(),
            b"async-secret-marker"
        ));

        let protected_index = session
            .notes()
            .iter()
            .position(|note| note.path == protected_path)
            .unwrap();
        let unlock = session
            .begin_unlock_note(protected_index, password.clone())
            .unwrap();
        let unlock_completion = unlock.execute();
        assert!(session.document().is_none());
        assert_eq!(
            session.finish_secure_operation(unlock_completion).unwrap(),
            SecureOutcome::Unlocked
        );

        let before_rewrite = fs::read(&protected_path).unwrap();
        let rename = session
            .begin_rename_protected_selected("Renamed Async Vault", "2026-09-02T00:00:00.000Z")
            .unwrap();
        assert_eq!(fs::read(&protected_path).unwrap(), before_rewrite);
        assert_eq!(session.document().unwrap().title(), "Async Vault");
        let rename_completion = rename.execute();
        let renamed_protected_path = workspace.note_path("Renamed Async Vault.md");
        assert!(!protected_path.exists());
        assert_ne!(fs::read(&renamed_protected_path).unwrap(), before_rewrite);
        assert_eq!(session.document().unwrap().title(), "Async Vault");
        assert_eq!(
            session.finish_secure_operation(rename_completion).unwrap(),
            SecureOutcome::MetadataChanged
        );
        assert_eq!(session.document().unwrap().title(), "Renamed Async Vault");
        let protected_path = session.notes()[session.selected_note().unwrap()]
            .path
            .clone();
        assert_eq!(
            protected_path,
            workspace.note_path("Renamed Async Vault.md")
        );
        assert!(contains_bytes(
            &fs::read(&protected_path).unwrap(),
            b"Renamed Async Vault"
        ));

        let renamed_plain_path = workspace.note_path("Renamed Async Vault.md");
        let disable = session.begin_disable_protection_selected().unwrap();
        assert!(renamed_plain_path.exists());
        assert!(contains_bytes(
            &fs::read(&renamed_plain_path).unwrap(),
            stillus_secure::ARMORED_AGE_PREFIX
        ));
        assert!(session.document().unwrap().is_protected());
        let disable_completion = disable.execute();
        assert!(renamed_plain_path.exists());
        assert!(session.document().unwrap().is_protected());
        assert_eq!(
            session.finish_secure_operation(disable_completion).unwrap(),
            SecureOutcome::ProtectionDisabled(renamed_plain_path.clone())
        );
        assert!(!session.document().unwrap().is_protected());
        let restored = fs::read_to_string(&renamed_plain_path).unwrap();
        assert!(restored.contains("Renamed Async Vault"));
        assert!(restored.contains("async-secret-marker"));
    }

    #[test]
    fn stale_secure_completion_is_rejected_without_publishing_plaintext() {
        let workspace = TestWorkspace::new();
        let password = MasterPassword::new("stale completion password".to_owned());
        let protected_path = workspace.write_protected_note(
            "ntrm-eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee.md",
            "stale.md",
            b"---\ntitle: Stale Secret\n---\nstale-secret-marker\n",
            &password,
        );
        let original_ciphertext = fs::read(&protected_path).unwrap();
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        let completion = session.begin_unlock_note(0, password).unwrap().execute();
        let pending = session.pending_secure_operation.as_mut().unwrap();
        pending.id = pending.id.saturating_add(1);

        assert!(matches!(
            session.finish_secure_operation(completion),
            Err(CoreError::NoteUnavailable(ref message))
                if message.contains("stale secure completion")
        ));
        assert!(session.secure_operation_pending());
        assert!(session.document().is_none());
        assert_eq!(session.notes()[0].title, "Stale Secret");
        assert_eq!(fs::read(&protected_path).unwrap(), original_ciphertext);
    }

    #[test]
    fn core_protect_and_disable_round_trip_preserves_master_context() {
        let workspace = TestWorkspace::new();
        let original =
            "---\ntitle: Vault Note\ntags: [Secure]\n---\nVault Note\ncore-lock-marker\n";
        workspace.write_note("vault-note.md", original);
        let password = MasterPassword::new("shared master".to_owned());
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.open_note(0).unwrap();
        let protected_path = session.protect_selected(Some(password.clone())).unwrap();
        assert_eq!(protected_path, workspace.note_path("vault-note.md"));
        assert!(workspace.note_path("vault-note.md").exists());
        assert_eq!(session.notes().len(), 1);
        assert_eq!(session.notes()[0].protection, NoteProtection::Protected);
        assert_eq!(session.notes()[0].title, "Vault Note");
        assert!(!contains_bytes(
            &fs::read(&protected_path).unwrap(),
            b"core-lock-marker"
        ));

        session.open_note(0).unwrap();
        assert_eq!(session.document().unwrap().title(), "Vault Note");
        let restored_path = session.disable_protection_selected().unwrap();
        assert_eq!(restored_path, workspace.note_path("Vault Note.md"));
        let restored = fs::read_to_string(&restored_path).unwrap();
        assert!(restored.contains("title: 'Vault Note'"));
        assert!(restored.contains("tags: [Secure]"));
        assert!(restored.ends_with("Vault Note\ncore-lock-marker\n"));
        assert!(!restored.contains("stillus_encryption"));
        assert_eq!(session.notes()[0].protection, NoteProtection::Plain);
        assert!(session.master_password_configured());
        assert!(!protected_path.exists());

        drop(session);
        let mut reopened = WorkspaceSession::open(workspace.path()).unwrap();
        assert!(reopened.master_password_configured());
        assert!(!reopened.security_unlocked());
        reopened
            .unlock_workspace_security(MasterPassword::new("shared master".to_owned()))
            .unwrap();
        assert!(reopened.security_unlocked());
    }

    #[cfg(unix)]
    #[test]
    fn protection_aborts_before_canonical_mutation_when_plain_recovery_cleanup_fails() {
        use std::os::unix::fs::symlink;

        let workspace = TestWorkspace::new();
        let original = "---\ntitle: Cleanup Guard\n---\ncanonical plaintext\n";
        workspace.write_note("cleanup-guard.md", original);
        {
            let mut session = WorkspaceSession::open(workspace.path()).unwrap();
            session.open_note(0).unwrap();
            session
                .document_mut()
                .unwrap()
                .apply_at(EditorCommand::Insert("obsolete ".to_owned()), 0)
                .unwrap();
            let recovery = session
                .begin_persistence(RECOVERY_DEBOUNCE_MS, "ignored".to_owned())
                .unwrap()
                .unwrap();
            assert!(matches!(recovery, PersistenceJob::Recovery(_)));
            session.finish_persistence(recovery.execute()).unwrap();
        }

        let recovery_directory = workspace.path().join(".stillus/recovery");
        let artifact = fs::read_dir(&recovery_directory)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let outside = workspace.path().join("obsolete-recovery-outside");
        fs::rename(&artifact, &outside).unwrap();
        symlink(&outside, &artifact).unwrap();

        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.open_note(0).unwrap();
        let result = session.protect_selected(Some(MasterPassword::new(
            "cleanup guard password".to_owned(),
        )));
        assert!(matches!(result, Err(CoreError::Recovery(_))));
        assert_eq!(
            fs::read_to_string(workspace.note_path("cleanup-guard.md")).unwrap(),
            original
        );
        assert!(
            artifact
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            fs::read_dir(workspace.path().join("notes"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("ntrm-"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn protected_soft_delete_patches_plaintext_metadata_without_unlock() {
        let workspace = TestWorkspace::new();
        let password = MasterPassword::new("protected soft delete password".to_owned());
        let protected_path = workspace.write_protected_note(
            "ntrm-cccccccccccccccccccccccccccccccc.md",
            "private-trash.md",
            b"---\ntitle: Private Trash\nfavorited: true\norder: {'__favorited': 4}\n---\nsecret trash body\n",
            &password,
        );
        let original_bytes = fs::read(&protected_path).unwrap();
        fn armor(bytes: &[u8]) -> &[u8] {
            let prefix = stillus_secure::ARMORED_AGE_PREFIX;
            let offset = bytes
                .windows(prefix.len())
                .position(|window| window == prefix)
                .unwrap();
            &bytes[offset..]
        }
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.select_protected_note(0).unwrap();
        assert!(
            !session
                .toggle_favorited_selected("2026-09-03T11:59:00.000Z")
                .unwrap()
        );
        let unfavorited_bytes = fs::read(&protected_path).unwrap();
        assert!(!contains_bytes(&unfavorited_bytes, b"__favorited"));
        assert_eq!(armor(&unfavorited_bytes), armor(&original_bytes));
        session
            .set_deleted_selected(true, "2026-09-03T12:00:00.000Z")
            .unwrap();
        assert!(protected_path.exists());
        let deleted_bytes = fs::read(&protected_path).unwrap();
        assert!(contains_bytes(&deleted_bytes, b"deleted: true"));
        assert!(contains_bytes(
            &deleted_bytes,
            stillus_secure::ARMORED_AGE_PREFIX
        ));
        assert!(!contains_bytes(&deleted_bytes, b"secret trash body"));
        assert_eq!(armor(&deleted_bytes), armor(&original_bytes));

        let mut restarted = WorkspaceSession::open(workspace.path()).unwrap();
        assert!(restarted.notes()[0].deleted);
        assert_eq!(restarted.notes()[0].title, "Private Trash");
        restarted.select_protected_note(0).unwrap();
        restarted
            .set_deleted_selected(false, "2026-09-03T12:01:00.000Z")
            .unwrap();
        assert!(protected_path.exists());
        assert!(!restarted.notes()[restarted.selected_note().unwrap()].deleted);
    }

    #[test]
    fn existing_vault_rejects_a_different_master_before_protecting_plaintext() {
        let workspace = TestWorkspace::new();
        let correct = MasterPassword::new("existing master".to_owned());
        workspace.write_protected_note(
            "ntrm-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.md",
            "existing.md",
            b"existing protected marker",
            &correct,
        );
        workspace.write_note("plain.md", "plain must survive");
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        let protected_index = session
            .notes()
            .iter()
            .position(|note| note.protection == NoteProtection::Protected)
            .unwrap();
        session.unlock_note(protected_index, correct).unwrap();
        let plain_index = session
            .notes()
            .iter()
            .position(|note| note.protection == NoteProtection::Plain)
            .unwrap();
        session.open_note(plain_index).unwrap();
        assert!(session.has_master_password());
        let result = session.protect_selected(Some(MasterPassword::new("different".to_owned())));
        assert!(
            matches!(result, Err(CoreError::Secure(_))),
            "unexpected protection result: {result:?}"
        );
        assert_eq!(
            fs::read_to_string(workspace.note_path("plain.md")).unwrap(),
            "plain must survive"
        );
    }

    #[test]
    fn password_change_rejects_notes_protected_after_target_collection() {
        let workspace = TestWorkspace::new();
        workspace.write_note("plain.md", "plaintext body");
        let old = MasterPassword::new("old catalog password".to_owned());
        let new = MasterPassword::new("new catalog password".to_owned());
        let mut first = WorkspaceSession::open(workspace.path()).unwrap();
        first.configure_workspace_security(old.clone()).unwrap();
        let pending = first
            .begin_change_master_password(old.clone(), new.clone())
            .unwrap();
        let mut second = WorkspaceSession::open(workspace.path()).unwrap();
        second.open_note(0).unwrap();
        let protection = second.begin_protect_selected(Some(old.clone())).unwrap();
        second
            .finish_secure_operation(protection.execute())
            .unwrap();
        let protected_path = second.notes()[0].path.clone();
        let before = fs::read(&protected_path).unwrap();
        assert!(first.finish_secure_operation(pending.execute()).is_err());
        assert_eq!(fs::read(protected_path).unwrap(), before);
        let security = stillus_security::SecurityStore::new(workspace.path());
        assert!(security.unlock(&old).is_ok());
        assert!(security.unlock(&new).is_err());
    }

    #[test]
    fn password_change_rejects_pending_old_password_recovery() {
        let workspace = TestWorkspace::new();
        let old = MasterPassword::new("old recovery password".to_owned());
        let new = MasterPassword::new("new recovery password".to_owned());
        let path = workspace.write_protected_note(
            "ntrm-33333333333333333333333333333333.md",
            "private.md",
            b"---\ntitle: Private\n---\nbody\n",
            &old,
        );
        let mut first = WorkspaceSession::open(workspace.path()).unwrap();
        first.configure_workspace_security(old.clone()).unwrap();
        first.unlock_note(0, old.clone()).unwrap();
        first
            .document_mut()
            .unwrap()
            .apply_at(EditorCommand::Insert("unsaved private text".to_owned()), 0)
            .unwrap();
        let key = first.recovery_store.key_for_note(&path).unwrap();
        let store = first.recovery_store.clone();
        let pending = first
            .document_mut()
            .unwrap()
            .begin_recovery(store, key, PROTECTED_RECOVERY_DEBOUNCE_MS)
            .unwrap();
        let mut second = WorkspaceSession::open(workspace.path()).unwrap();
        let change = second
            .begin_change_master_password(old, new.clone())
            .unwrap();
        second.finish_secure_operation(change.execute()).unwrap();
        assert!(pending.execute().result.is_err());
        assert!(
            first
                .recovery_store
                .protected_artifact_paths()
                .unwrap()
                .is_empty()
        );
        assert!(
            stillus_security::SecurityStore::new(workspace.path())
                .unlock(&new)
                .is_ok()
        );
    }

    #[test]
    fn another_window_password_change_invalidates_pending_protection() {
        let workspace = TestWorkspace::new();
        workspace.write_note("plain.md", "plaintext remains unchanged");
        let old = MasterPassword::new("old shared verifier".to_owned());
        let new = MasterPassword::new("new shared verifier".to_owned());
        let mut first = WorkspaceSession::open(workspace.path()).unwrap();
        first.configure_workspace_security(old.clone()).unwrap();
        first.open_note(0).unwrap();
        let pending = first.begin_protect_selected(Some(old.clone())).unwrap();
        let mut second = WorkspaceSession::open(workspace.path()).unwrap();
        let changed = second
            .begin_change_master_password(old, new)
            .unwrap()
            .execute();
        second.finish_secure_operation(changed).unwrap();
        assert!(first.finish_secure_operation(pending.execute()).is_err());
        assert_eq!(
            fs::read_to_string(workspace.note_path("plain.md")).unwrap(),
            "plaintext remains unchanged"
        );
    }

    #[test]
    fn configured_verifier_rotates_without_notes_or_recovery() {
        let workspace = TestWorkspace::new();
        let old = MasterPassword::new("old verifier password".to_owned());
        let new = MasterPassword::new("new verifier password".to_owned());
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.configure_workspace_security(old.clone()).unwrap();
        assert!(session.master_password_configured());
        let mut progress = Vec::new();
        let completion = session
            .begin_change_master_password(old.clone(), new.clone())
            .unwrap()
            .execute_with_progress(|event| progress.push(event));
        assert_eq!(
            session.finish_secure_operation(completion).unwrap(),
            SecureOutcome::MasterPasswordChanged {
                notes: 0,
                recovery: 0,
                secrets: 0,
            }
        );
        assert!(progress.iter().any(|event| {
            event.phase == SecurePhase::PreparingVerifier
                && event.completed == 1
                && event.total == 1
        }));
        assert!(
            progress
                .iter()
                .filter(|event| event.phase == SecurePhase::Validating)
                .all(|event| event.percent == Some(0))
        );
        assert!(progress.iter().any(|event| {
            event.phase == SecurePhase::PreparingVerifier
                && event.completed == 1
                && event.percent.is_some_and(|percent| percent >= 80)
        }));
        assert_eq!(progress.last().and_then(|event| event.percent), Some(99));

        let mut reopened = WorkspaceSession::open(workspace.path()).unwrap();
        assert!(reopened.unlock_workspace_security(old).is_err());
        reopened.unlock_workspace_security(new).unwrap();
    }

    #[test]
    fn master_password_change_updates_every_note_and_keeps_open_document_clean() {
        let workspace = TestWorkspace::new();
        let old = MasterPassword::new("old workspace master".to_owned());
        let new = MasterPassword::new("new workspace master".to_owned());
        workspace.write_protected_note(
            "First.md",
            "First.md",
            b"---\ntitle: First\n---\nfirst secret\n",
            &old,
        );
        workspace.write_protected_note(
            "Deleted.md",
            "Deleted.md",
            b"---\ntitle: Deleted\ndeleted: true\nfuture: keep\n---\ndeleted secret\n",
            &old,
        );
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        let first = session
            .notes()
            .iter()
            .position(|note| note.title == "First")
            .unwrap();
        session.unlock_note(first, old.clone()).unwrap();
        let mut progress = Vec::new();
        let completion = session
            .begin_change_master_password(old.clone(), new.clone())
            .unwrap()
            .execute_with_progress(|event| progress.push(event));
        assert_eq!(
            session.finish_secure_operation(completion).unwrap(),
            SecureOutcome::MasterPasswordChanged {
                notes: 2,
                recovery: 0,
                secrets: 0,
            }
        );
        assert!(matches!(
            session.document().unwrap().save_status(),
            SaveStatus::Clean { .. }
        ));
        assert!(session.document().unwrap().is_protected());
        assert!(progress.iter().any(|event| {
            event.phase == SecurePhase::ReplacingNotes
                && event.completed == 2
                && event.total == 2
                && event.percent.is_some_and(|percent| percent < 100)
        }));
        assert_eq!(progress.last().and_then(|event| event.percent), Some(99));
        assert!(
            progress
                .iter()
                .filter_map(|event| event.percent)
                .collect::<Vec<_>>()
                .windows(2)
                .all(|values| values[0] <= values[1])
        );

        let mut reopened = WorkspaceSession::open(workspace.path()).unwrap();
        let first = reopened
            .notes()
            .iter()
            .position(|note| note.title == "First")
            .unwrap();
        assert!(matches!(
            reopened.unlock_note(first, old),
            Err(CoreError::Secure(_))
        ));
        reopened.unlock_note(first, new).unwrap();
        assert_eq!(reopened.protected_note_count(), 2);
    }

    #[test]
    fn legacy_whole_file_note_is_reported_without_mutation() {
        let workspace = TestWorkspace::new();
        let path = workspace.note_path("ntrm-0123456789abcdef0123456789abcdef.md");
        let password = MasterPassword::new("legacy password".to_owned());
        let payload = b"---\ntitle: Legacy\n---\nlegacy body\n";
        let metadata = EnvelopeMetadata::new(
            EnvelopeKind::Note,
            "Legacy.md".to_owned(),
            payload.len() as u64,
        )
        .unwrap();
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let mut writer = EnvelopeWriter::new_for_test(file, &password, metadata).unwrap();
        writer.write_all(payload).unwrap();
        writer.finish().unwrap();
        let before = fs::read(&path).unwrap();

        let session = WorkspaceSession::open(workspace.path()).unwrap();
        assert_eq!(session.notes().len(), 1);
        assert_eq!(session.notes()[0].protection, NoteProtection::Protected);
        assert_eq!(
            session.notes()[0].availability,
            NoteAvailability::IoError("unsupported legacy protected format".to_owned())
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn switching_away_from_an_unlocked_note_redacts_its_list_projection() {
        let workspace = TestWorkspace::new();
        let password = MasterPassword::new("session master".to_owned());
        let protected_path = workspace.write_protected_note(
            "ntrm-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.md",
            "private-title.md",
            b"---\ntitle: Private Title\ntags: [SecretTag]\n---\nprivate body",
            &password,
        );
        workspace.write_note("public.md", "public body");
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        let protected_index = session
            .notes()
            .iter()
            .position(|note| note.path == protected_path)
            .unwrap();
        session.unlock_note(protected_index, password).unwrap();
        let protected_index = session
            .notes()
            .iter()
            .position(|note| note.path == protected_path)
            .unwrap();
        assert_eq!(session.notes()[protected_index].title, "private body");
        let public_index = session
            .notes()
            .iter()
            .position(|note| note.protection == NoteProtection::Plain)
            .unwrap();
        session.open_note(public_index).unwrap();
        let protected_index = session
            .notes()
            .iter()
            .position(|note| note.path == protected_path)
            .unwrap();
        assert_eq!(session.notes()[protected_index].title, "Private Title");
        assert_eq!(session.notes()[protected_index].tags, ["SecretTag"]);
    }

    #[test]
    fn rss_participates_in_catalog_categories_read_state_and_mixed_order() {
        let workspace = TestWorkspace::new();
        workspace.write_note(
            "note.md",
            "---\ntitle: Note\ntags:\n  - Work\nfavorited: true\n---\nbody\n",
        );
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        let rss_id = session
            .create_rss(
                "https://example.test/feed",
                vec!["Work".to_owned()],
                true,
                "2025-09-01T10:00:00Z",
            )
            .unwrap();
        assert_eq!(session.engine_catalog().unwrap().len(), 2);
        assert_eq!(
            session
                .categories()
                .iter()
                .find(|category| category.name == "Work")
                .unwrap()
                .note_count,
            2
        );

        let entries = (0..12)
            .map(|index| RssEntry {
                id: format!("entry/{index}"),
                title: format!("Entry {index}"),
                author: None,
                published: None,
                updated: None,
                summary: String::new(),
                link: None,
            })
            .collect();
        session
            .finish_rss_refresh(RssRefreshResult::Fetched {
                item_id: rss_id.clone(),
                cache: RssFeedCache {
                    title: Some("Example".to_owned()),
                    entries,
                    fetched_at: Some("2025-09-02T10:00:00Z".to_owned()),
                    ..RssFeedCache::default()
                },
            })
            .unwrap();
        assert_eq!(session.rss_subscriptions()[0].unread, 10);
        assert!(
            session
                .mark_rss_read("entry/0", "2025-09-02T10:01:00Z")
                .unwrap()
        );
        assert_eq!(session.rss_subscriptions()[0].unread, 9);

        let note_path = session.notes()[0].path.clone();
        assert!(
            session
                .set_catalog_order(
                    FAVORITED_ORDER_KEY,
                    &[
                        CatalogOrderItem::Engine(rss_engine_id(), rss_id.clone()),
                        CatalogOrderItem::Note(note_path),
                    ],
                )
                .unwrap()
        );
        assert_eq!(
            session.rss_subscriptions()[0]
                .subscription
                .order
                .get(FAVORITED_ORDER_KEY),
            Some(&0)
        );
        assert_eq!(session.notes()[0].order.get(FAVORITED_ORDER_KEY), Some(&1));

        session
            .rename_rss("Local title", "2025-09-02T10:02:00Z")
            .unwrap();
        assert_eq!(session.rss_subscriptions()[0].display_title, "Local title");
        session
            .update_selected_rss_metadata("2025-09-02T10:03:00Z", |item| item.deleted = true)
            .unwrap();
        assert_eq!(session.categories()[0].note_count, 1);
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    struct TestWorkspace {
        root: PathBuf,
    }

    impl TestWorkspace {
        fn new() -> Self {
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("stillus-core-test-{}-{id}", std::process::id()));
            fs::create_dir_all(root.join("notes")).unwrap();
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
        }

        fn write_note(&self, name: &str, contents: &str) {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.root.join("notes").join(name))
                .unwrap();
            file.write_all(contents.as_bytes()).unwrap();
        }

        fn note_path(&self, name: &str) -> PathBuf {
            self.root.join("notes").join(name)
        }

        fn write_protected_note(
            &self,
            name: &str,
            _original_filename: &str,
            plaintext: &[u8],
            password: &MasterPassword,
        ) -> PathBuf {
            let path = self.note_path(name);
            fs::write(&path, plaintext).unwrap();
            let (_, version) = open_versioned(&path).unwrap();
            let scan = scan_reader(Cursor::new(plaintext)).unwrap();
            let title = match scan.status {
                FrontMatterStatus::Parsed(parsed) => parsed
                    .metadata
                    .title
                    .unwrap_or_else(|| "Защищённая заметка".to_owned()),
                _ => "Защищённая заметка".to_owned(),
            };
            protect_note_body(&path, &version, password, &title).unwrap();
            path
        }

        fn replace_protected_note(
            &self,
            path: &Path,
            _original_filename: &str,
            plaintext: &[u8],
            password: &MasterPassword,
        ) {
            let temp = self.note_path("external-replacement.tmp");
            fs::write(&temp, plaintext).unwrap();
            let (_, version) = open_versioned(&temp).unwrap();
            let scan = scan_reader(Cursor::new(plaintext)).unwrap();
            let title = match scan.status {
                FrontMatterStatus::Parsed(parsed) => parsed
                    .metadata
                    .title
                    .unwrap_or_else(|| "Защищённая заметка".to_owned()),
                _ => "Защищённая заметка".to_owned(),
            };
            protect_note_body(&temp, &version, password, &title).unwrap();
            fs::rename(temp, path).unwrap();
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }
}
