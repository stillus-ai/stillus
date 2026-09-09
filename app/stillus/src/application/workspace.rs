// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use stillus_core::*;
use stillus_editor::ByteRange;
use stillus_engine::{EngineId, ItemSummary, SearchHit, SearchRequest, TaskScheduler};
use stillus_secure::MasterPassword;
use stillus_security::{SecurityStore, WorkspaceSecurityState};
use stillus_storage::IntegrityFailure;

enum ReadVersion {
    Note {
        target: String,
        version: NoteVersion,
    },
    Ai(stillus_ai::AiSettings),
    Settings(super::settings::PublicSettings),
}

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

pub(crate) struct Workspace {
    pub(super) core: stillus_core::WorkspaceSession,
    session: u64,
    pub(super) wall_time: std::time::SystemTime,
    next_target: u64,
    targets: BTreeMap<String, PathBuf>,
    versions: BTreeMap<String, ReadVersion>,
    next_version: u64,
    pub(super) actions_dirty: bool,
    pub(super) operations: super::tools::Operations,
    rss_service: Option<super::rss::Service>,
    pub(super) search_sender: Option<std::sync::mpsc::SyncSender<super::search::SearchCommand>>,
}

#[allow(
    dead_code,
    reason = "Typed commands are shared with the upcoming built-in assistant."
)]
impl Workspace {
    pub(crate) fn open(root: impl AsRef<Path>) -> Result<Self, CoreError> {
        Ok(Self {
            core: stillus_core::WorkspaceSession::open(root)?,
            session: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            wall_time: std::time::SystemTime::now(),
            next_target: 0,
            targets: BTreeMap::new(),
            versions: BTreeMap::new(),
            next_version: 0,
            actions_dirty: false,
            operations: super::tools::Operations::default(),
            rss_service: None,
            search_sender: None,
        })
    }
    pub(crate) fn bind_search(
        &mut self,
        sender: std::sync::mpsc::SyncSender<super::search::SearchCommand>,
    ) {
        self.search_sender = Some(sender);
    }
    pub(super) fn session_id(&self) -> u64 {
        self.session
    }
    pub(crate) fn actions_busy(&self) -> bool {
        self.operations.writing()
    }
    pub(super) fn expected_version(
        &self,
        id: &str,
        version: &str,
    ) -> Result<NoteVersion, super::tools::ActionError> {
        match self.versions.get(version) {
            Some(ReadVersion::Note { target, version }) if target == id => Ok(*version),
            _ => Err(super::tools::ActionError::Conflict),
        }
    }
    pub(crate) fn ensure_rss(&mut self) {
        if self.rss_service.is_none() {
            self.rss_service = Some(super::rss::Service::start(self.root().into()));
        }
    }
    pub(crate) fn send_rss(
        &mut self,
        command: super::rss::Command,
    ) -> Result<(), super::tools::ActionError> {
        self.ensure_rss();
        self.rss_service
            .as_ref()
            .expect("RSS service")
            .sender
            .try_send(command)
            .map_err(|_| super::tools::ActionError::Busy)
    }
    pub(crate) fn receive_rss(&mut self) -> Option<super::rss::Snapshot> {
        self.ensure_rss();
        self.rss_service.as_ref()?.receiver.try_recv().ok()
    }
    fn selected_path(&self) -> Option<PathBuf> {
        self.core
            .selected_note()
            .and_then(|i| self.core.notes().get(i))
            .map(|n| n.path.clone())
    }
    fn track_rename(&mut self, old: Option<PathBuf>) {
        if let (Some(old), Some(new)) = (old, self.selected_path()) {
            if old != new && !self.core.notes().iter().any(|n| n.path == old) {
                self.remap(&old, &new);
            }
        }
    }
    pub(super) fn remap(&mut self, old: &Path, new: &Path) {
        for path in self.targets.values_mut() {
            if path == old {
                *path = new.into();
            }
        }
    }
    pub(crate) fn target_page(
        &mut self,
        offset: usize,
        limit: usize,
    ) -> Vec<(String, NoteSummary)> {
        self.targets.retain(|_, path| {
            self.core.notes().iter().any(|n| &n.path == path)
                || self
                    .core
                    .external_files()
                    .iter()
                    .any(|file| &file.path == path)
        });
        self.core
            .notes()
            .iter()
            .skip(offset)
            .take(limit.min(100))
            .map(|note| {
                let id = self
                    .targets
                    .iter()
                    .find(|(_, p)| *p == &note.path)
                    .map(|(id, _)| id.clone())
                    .unwrap_or_else(|| {
                        self.next_target += 1;
                        let id = format!("notes/{:x}/{:x}", self.session, self.next_target);
                        self.targets.insert(id.clone(), note.path.clone());
                        id
                    });
                (id, note.clone())
            })
            .collect()
    }
    pub(crate) fn target_id(&mut self, path: &Path) -> Result<String, super::tools::ActionError> {
        if !self.core.notes().iter().any(|note| note.path == path)
            && !self
                .core
                .external_files()
                .iter()
                .any(|file| file.path == path)
        {
            return Err(super::tools::ActionError::NotFound);
        }
        if let Some((id, _)) = self
            .targets
            .iter()
            .find(|(_, current)| current.as_path() == path)
        {
            return Ok(id.clone());
        }
        self.next_target += 1;
        let prefix = if self
            .core
            .external_files()
            .iter()
            .any(|file| file.path == path)
        {
            "external"
        } else {
            "notes"
        };
        let id = format!("{prefix}/{:x}/{:x}", self.session, self.next_target);
        self.targets.insert(id.clone(), path.to_owned());
        Ok(id)
    }
    #[cfg(test)]
    pub(crate) fn targets(&mut self) -> Vec<(String, NoteSummary)> {
        self.target_page(0, 100)
    }
    pub(crate) fn resolve_target(&self, id: &str) -> Result<PathBuf, super::tools::ActionError> {
        self.targets
            .get(id)
            .filter(|path| {
                self.core.notes().iter().any(|n| &n.path == *path)
                    || self
                        .core
                        .external_files()
                        .iter()
                        .any(|file| &file.path == *path)
            })
            .cloned()
            .ok_or(super::tools::ActionError::NotFound)
    }
    pub(crate) fn read_target(
        &mut self,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<(NoteRead, String), super::tools::ActionError> {
        let path = self.resolve_target(id)?;
        let read = self.core.read_note_at(&path, offset, limit)?;
        let existing = self
            .versions
            .iter()
            .find(|(_, entry)| matches!(entry, ReadVersion::Note { target, version } if target == id && *version == read.version))
            .map(|(token, _)| token.clone());
        let token = existing.unwrap_or_else(|| {
            self.next_version += 1;
            let token = format!("versions/{:016x}/{:016x}", self.session, self.next_version);
            self.versions.insert(
                token.clone(),
                ReadVersion::Note {
                    target: id.into(),
                    version: read.version,
                },
            );
            if self.versions.len() > 256 {
                self.versions.pop_first();
            }
            token
        });
        Ok((read, token))
    }
    pub(crate) fn edit_target(
        &mut self,
        id: &str,
        version: &str,
        edit: NoteEdit,
        now_ms: u64,
    ) -> Result<AddressedEdit, super::tools::ActionError> {
        let path = self.resolve_target(id)?;
        let expected = self.expected_version(id, version)?;
        let timestamp = format_utc_timestamp(self.wall_time)?;
        Ok(self
            .core
            .begin_note_edit(&path, expected, edit, now_ms, &timestamp)?)
    }
    pub fn root(&self) -> &Path {
        self.core.root()
    }

    pub fn notes(&self) -> &[NoteSummary] {
        self.core.notes()
    }

    pub fn categories(&self) -> &[CategorySummary] {
        self.core.categories()
    }

    pub fn selected_note(&self) -> Option<usize> {
        self.core.selected_note()
    }

    pub fn external_files(&self) -> &[ExternalFileSummary] {
        self.core.external_files()
    }

    pub fn rss_subscriptions(&self) -> Vec<RssSubscriptionSummary> {
        self.core.rss_subscriptions()
    }

    pub fn rss_background_snapshot(&self) -> RssEngine {
        self.core.rss_background_snapshot()
    }

    pub fn accept_rss_snapshot(&mut self, engine: RssEngine) {
        let old = self.selected_path();
        self.core.accept_rss_snapshot(engine);
        self.track_rename(old);
    }

    pub fn rss_preferences(&self, id: &ItemId) -> Result<RssPreferences, CoreError> {
        self.core.rss_preferences(id)
    }

    pub fn rss_schedule(&self, id: &ItemId) -> Result<RssSchedule, CoreError> {
        self.core.rss_schedule(id)
    }

    pub fn visit_rss(&self, id: &ItemId, now: u64) -> Result<(), CoreError> {
        self.core.visit_rss(id, now)
    }

    pub fn save_rss_preferences(
        &mut self,
        id: &ItemId,
        expected: u64,
        preferences: RssPreferences,
    ) -> Result<(), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.save_rss_preferences(id, expected, preferences);
        self.track_rename(old);
        result
    }

    pub fn non_document_items(&self) -> Vec<stillus_engine::ItemSummary> {
        self.core.non_document_items()
    }
    pub fn selected_engine_item(&self) -> Option<&(EngineId, ItemId)> {
        self.core.selected_engine_item()
    }
    pub(super) fn accept_engine_items(
        &mut self,
        engine: &EngineId,
        items: Vec<stillus_engine::ItemSummary>,
    ) {
        self.core.accept_engine_items(engine, items)
    }
    pub(super) fn open_engine_item(
        &mut self,
        engine: &EngineId,
        id: &ItemId,
    ) -> Result<(), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        self.core.open_engine_item(engine, id)
    }
    pub(super) fn update_engine_metadata(
        &mut self,
        engine: &EngineId,
        id: &ItemId,
        version: &str,
        patch: stillus_engine::CommonMetadataPatch,
    ) -> Result<(), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        self.core.update_engine_metadata(engine, id, version, patch)
    }
    pub fn selected_rss(&self) -> Option<&ItemId> {
        self.core.selected_rss()
    }

    pub fn engine_toolbar_actions(&self, engine_id: &EngineId) -> Vec<ToolbarAction> {
        self.core.engine_toolbar_actions(engine_id)
    }

    pub fn rss_toolbar_actions(&self) -> Vec<ToolbarAction> {
        self.core.rss_toolbar_actions()
    }

    pub fn rss_feed(&self, item_id: &ItemId) -> Result<(RssFeedCache, RssReadState), CoreError> {
        self.core.rss_feed(item_id)
    }

    pub fn open_rss(&mut self, item_id: &ItemId) -> Result<(), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.open_rss(item_id);
        self.track_rename(old);
        result
    }

    pub fn create_rss(
        &mut self,
        url: &str,
        categories: Vec<String>,
        favorited: bool,
        timestamp: &str,
    ) -> Result<ItemId, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.create_rss(url, categories, favorited, timestamp);
        self.track_rename(old);
        result
    }

    pub fn rename_rss(&mut self, title: &str, timestamp: &str) -> Result<(), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.rename_rss(title, timestamp);
        self.track_rename(old);
        result
    }

    pub fn update_selected_rss_metadata(
        &mut self,
        timestamp: &str,
        update: impl FnOnce(&mut RssSubscription),
    ) -> Result<(), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.update_selected_rss_metadata(timestamp, update);
        self.track_rename(old);
        result
    }

    pub fn set_selected_rss_categories(
        &mut self,
        categories: &[String],
        timestamp: &str,
    ) -> Result<(), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.set_selected_rss_categories(categories, timestamp);
        self.track_rename(old);
        result
    }

    pub fn rss_refresh_request(&self, item_id: &ItemId) -> Result<RssRefreshRequest, CoreError> {
        self.core.rss_refresh_request(item_id)
    }

    pub fn finish_rss_refresh(&mut self, result: RssRefreshResult) -> Result<(), CoreError> {
        let old = self.selected_path();
        let result = self.core.finish_rss_refresh(result);
        self.track_rename(old);
        result
    }

    pub fn mark_rss_read(&mut self, entry_id: &str, timestamp: &str) -> Result<bool, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.mark_rss_read(entry_id, timestamp);
        self.track_rename(old);
        result
    }

    pub fn selected_target(&self) -> Option<DocumentTarget> {
        self.core.selected_target()
    }

    pub fn selected_item(&self) -> Option<(EngineId, ItemId)> {
        self.core.selected_item()
    }

    pub fn engine_ids(&self) -> impl Iterator<Item = &EngineId> {
        self.core.engine_ids()
    }

    pub fn external_file_extensions(&self) -> Vec<String> {
        self.core.external_file_extensions()
    }

    pub fn engine_catalog(&self) -> Result<Vec<ItemSummary>, CoreError> {
        self.core.engine_catalog()
    }

    pub fn attach_external_file(&mut self, path: &Path) -> Result<DocumentTarget, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.attach_external_file(path);
        self.track_rename(old);
        result
    }

    pub fn open_external_file(&mut self, path: &Path) -> Result<DocumentTarget, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.open_external_file(path);
        self.track_rename(old);
        result
    }

    pub fn open_external_item(
        &mut self,
        engine_id: &EngineId,
        item_id: &ItemId,
    ) -> Result<(), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.open_external_item(engine_id, item_id);
        self.track_rename(old);
        result
    }

    pub fn close_external_file(
        &mut self,
        engine_id: &EngineId,
        item_id: &ItemId,
    ) -> Result<bool, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.close_external_file(engine_id, item_id);
        self.track_rename(old);
        result
    }

    pub fn search_all_engines(&self, request: &SearchRequest) -> Result<Vec<SearchHit>, CoreError> {
        self.core.search_all_engines(request)
    }

    pub fn selected_document_supports_local_search(&self) -> bool {
        self.core.selected_document_supports_local_search()
    }

    pub fn search_selected_document(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ByteRange>, CoreError> {
        self.core.search_selected_document(query, limit)
    }

    pub fn task_scheduler(&self) -> &TaskScheduler {
        self.core.task_scheduler()
    }

    pub fn task_scheduler_mut(&mut self) -> &mut TaskScheduler {
        self.core.task_scheduler_mut()
    }

    pub fn recovery_diagnostics(&self) -> &[String] {
        self.core.recovery_diagnostics()
    }

    pub fn has_master_password(&self) -> bool {
        self.core.has_master_password()
    }

    pub fn master_password_configured(&self) -> bool {
        self.core.master_password_configured()
    }

    pub fn security_unlocked(&self) -> bool {
        self.core.security_unlocked()
    }

    pub fn security_state(&self) -> WorkspaceSecurityState {
        self.core.security_state()
    }

    pub fn configure_workspace_security(
        &mut self,
        password: MasterPassword,
    ) -> Result<(), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.configure_workspace_security(password);
        self.track_rename(old);
        result
    }

    pub fn unlock_workspace_security(&mut self, password: MasterPassword) -> Result<(), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.unlock_workspace_security(password);
        self.track_rename(old);
        result
    }

    pub fn security_store(&self) -> &SecurityStore {
        self.core.security_store()
    }

    pub fn referenced_secret_count(&self) -> Result<usize, CoreError> {
        self.core.referenced_secret_count()
    }

    pub fn has_protected_notes(&self) -> bool {
        self.core.has_protected_notes()
    }

    pub fn protected_note_count(&self) -> usize {
        self.core.protected_note_count()
    }

    pub fn protected_recovery_count(&self) -> Result<usize, CoreError> {
        self.core.protected_recovery_count()
    }

    pub fn begin_change_master_password(
        &mut self,
        current: MasterPassword,
        new: MasterPassword,
    ) -> Result<SecureJob, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.begin_change_master_password(current, new);
        self.track_rename(old);
        result
    }

    pub fn secure_operation_pending(&self) -> bool {
        self.core.secure_operation_pending()
    }

    pub fn integrity_failure(&self) -> Option<&IntegrityFailure> {
        self.core.integrity_failure()
    }

    pub fn begin_integrity_resolution(
        &mut self,
        resolution: IntegrityResolution,
    ) -> Result<SecureJob, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.begin_integrity_resolution(resolution);
        self.track_rename(old);
        result
    }

    pub fn selected_is_protected(&self) -> bool {
        self.core.selected_is_protected()
    }

    pub fn select_protected_note(&mut self, note_index: usize) -> Result<(), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.select_protected_note(note_index);
        self.track_rename(old);
        result
    }

    pub fn unlock_note(
        &mut self,
        note_index: usize,
        password: MasterPassword,
    ) -> Result<(), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.unlock_note(note_index, password);
        self.track_rename(old);
        result
    }

    pub fn begin_unlock_note(
        &mut self,
        note_index: usize,
        password: MasterPassword,
    ) -> Result<SecureJob, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.begin_unlock_note(note_index, password);
        self.track_rename(old);
        result
    }

    pub fn begin_open_protected_note(&mut self, note_index: usize) -> Result<SecureJob, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.begin_open_protected_note(note_index);
        self.track_rename(old);
        result
    }

    pub fn lock_selected(&mut self) -> Result<(), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.lock_selected();
        self.track_rename(old);
        result
    }

    pub fn protect_selected(
        &mut self,
        password: Option<MasterPassword>,
    ) -> Result<PathBuf, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.protect_selected(password);
        self.track_rename(old);
        result
    }

    pub fn begin_protect_selected(
        &mut self,
        password: Option<MasterPassword>,
    ) -> Result<SecureJob, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.begin_protect_selected(password);
        self.track_rename(old);
        result
    }

    pub fn disable_protection_selected(&mut self) -> Result<PathBuf, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.disable_protection_selected();
        self.track_rename(old);
        result
    }

    pub fn begin_disable_protection_selected(&mut self) -> Result<SecureJob, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.begin_disable_protection_selected();
        self.track_rename(old);
        result
    }

    pub fn document(&self) -> Option<&DocumentSession> {
        self.core.document()
    }

    pub fn document_mut(&mut self) -> Option<&mut DocumentSession> {
        self.core.document_mut()
    }

    pub fn apply_selected_at(
        &mut self,
        command: EditorCommand,
        now_ms: u64,
    ) -> Result<CommandOutcome, CoreError> {
        let old = self.selected_path();
        let result = self.core.apply_selected_at(command, now_ms);
        self.track_rename(old);
        result
    }

    pub fn open_note(&mut self, note_index: usize) -> Result<(), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.open_note(note_index);
        self.track_rename(old);
        result
    }

    pub fn begin_autosave(
        &mut self,
        now_ms: u64,
        modified: String,
    ) -> Result<Option<SaveJob>, CoreError> {
        if self.operations.writing() {
            return Ok(None);
        }
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.begin_autosave(now_ms, modified);
        self.track_rename(old);
        result
    }

    pub fn finish_autosave(&mut self, completion: SaveCompletion) -> Result<(), CoreError> {
        let old = self.selected_path();
        let result = self.core.finish_autosave(completion);
        self.track_rename(old);
        result
    }

    pub fn retry_autosave(&mut self, now_ms: u64) -> bool {
        let old = self.selected_path();
        let result = self.core.retry_autosave(now_ms);
        self.track_rename(old);
        result
    }

    pub fn next_autosave_deadline(&self) -> Option<u64> {
        self.core.next_autosave_deadline()
    }

    pub fn next_persistence_deadline(&self) -> Option<u64> {
        if self.operations.writing() {
            return None;
        }
        self.core.next_persistence_deadline()
    }

    pub fn begin_persistence(
        &mut self,
        now_ms: u64,
        modified: String,
    ) -> Result<Option<PersistenceJob>, CoreError> {
        if self.operations.writing() {
            return Ok(None);
        }
        let old = self.selected_path();
        let result = self.core.begin_persistence(now_ms, modified);
        self.track_rename(old);
        result
    }

    pub fn finish_persistence(
        &mut self,
        completion: PersistenceCompletion,
    ) -> Result<(), CoreError> {
        let old = self.selected_path();
        let result = self.core.finish_persistence(completion);
        self.track_rename(old);
        result
    }

    pub fn restore_recovery(&mut self, note_index: usize, now_ms: u64) -> Result<(), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.restore_recovery(note_index, now_ms);
        self.track_rename(old);
        result
    }

    pub fn restore_external_recovery(
        &mut self,
        engine_id: &EngineId,
        item_id: &ItemId,
        now_ms: u64,
    ) -> Result<(), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self
            .core
            .restore_external_recovery(engine_id, item_id, now_ms);
        self.track_rename(old);
        result
    }

    pub fn begin_restore_protected_recovery(
        &mut self,
        note_index: usize,
        now_ms: u64,
    ) -> Result<SecureJob, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self
            .core
            .begin_restore_protected_recovery(note_index, now_ms);
        self.track_rename(old);
        result
    }

    pub fn poll_external(&mut self, now_ms: u64) -> Result<ExternalPoll, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.poll_external(now_ms);
        self.track_rename(old);
        result
    }

    pub fn begin_poll_external(&mut self, now_ms: u64) -> Result<ExternalPollStart, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.begin_poll_external(now_ms);
        self.track_rename(old);
        result
    }

    pub fn discard_local_and_reload(&mut self) -> Result<(), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.discard_local_and_reload();
        self.track_rename(old);
        result
    }

    pub fn begin_discard_protected_local_and_reload(&mut self) -> Result<SecureJob, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.begin_discard_protected_local_and_reload();
        self.track_rename(old);
        result
    }

    pub fn create_note(&mut self, title: &str, timestamp: &str) -> Result<usize, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.create_note(title, timestamp);
        self.track_rename(old);
        result
    }

    pub fn rename_selected(&mut self, title: &str, timestamp: &str) -> Result<(), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.rename_selected(title, timestamp);
        self.track_rename(old);
        result
    }

    pub fn begin_rename_protected_selected(
        &mut self,
        title: &str,
        timestamp: &str,
    ) -> Result<SecureJob, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.begin_rename_protected_selected(title, timestamp);
        self.track_rename(old);
        result
    }

    pub fn set_deleted_selected(
        &mut self,
        deleted: bool,
        timestamp: &str,
    ) -> Result<PathBuf, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.set_deleted_selected(deleted, timestamp);
        self.track_rename(old);
        result
    }

    pub fn begin_set_deleted_protected_selected(
        &mut self,
        deleted: bool,
        timestamp: &str,
    ) -> Result<SecureJob, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self
            .core
            .begin_set_deleted_protected_selected(deleted, timestamp);
        self.track_rename(old);
        result
    }

    pub fn add_tag_selected(&mut self, tag: &str, timestamp: &str) -> Result<bool, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.add_tag_selected(tag, timestamp);
        self.track_rename(old);
        result
    }

    pub fn begin_add_tag_protected_selected(
        &mut self,
        tag: &str,
        timestamp: &str,
    ) -> Result<Option<SecureJob>, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.begin_add_tag_protected_selected(tag, timestamp);
        self.track_rename(old);
        result
    }

    pub fn remove_tag_selected(&mut self, tag: &str, timestamp: &str) -> Result<bool, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.remove_tag_selected(tag, timestamp);
        self.track_rename(old);
        result
    }

    pub fn set_category_note_order(
        &mut self,
        category: &str,
        ordered_paths: &[PathBuf],
    ) -> Result<bool, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.set_category_note_order(category, ordered_paths);
        self.track_rename(old);
        result
    }

    pub fn clear_category_note_order(&mut self, category: &str) -> Result<bool, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.clear_category_note_order(category);
        self.track_rename(old);
        result
    }

    pub fn set_favorited_note_order(
        &mut self,
        ordered_paths: &[PathBuf],
    ) -> Result<bool, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.set_favorited_note_order(ordered_paths);
        self.track_rename(old);
        result
    }

    pub fn clear_favorited_note_order(&mut self) -> Result<bool, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.clear_favorited_note_order();
        self.track_rename(old);
        result
    }

    pub fn set_catalog_order(
        &mut self,
        order_key: &str,
        ordered: &[CatalogOrderItem],
    ) -> Result<bool, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.set_catalog_order(order_key, ordered);
        self.track_rename(old);
        result
    }

    pub fn clear_catalog_order(&mut self, order_key: &str) -> Result<bool, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.clear_catalog_order(order_key);
        self.track_rename(old);
        result
    }

    pub fn begin_remove_tag_protected_selected(
        &mut self,
        tag: &str,
        timestamp: &str,
    ) -> Result<Option<SecureJob>, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self
            .core
            .begin_remove_tag_protected_selected(tag, timestamp);
        self.track_rename(old);
        result
    }

    pub fn toggle_pinned_selected(&mut self, timestamp: &str) -> Result<bool, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.toggle_pinned_selected(timestamp);
        self.track_rename(old);
        result
    }

    pub fn begin_toggle_pinned_protected_selected(
        &mut self,
        timestamp: &str,
    ) -> Result<(bool, SecureJob), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.begin_toggle_pinned_protected_selected(timestamp);
        self.track_rename(old);
        result
    }

    pub fn toggle_favorited_selected(&mut self, timestamp: &str) -> Result<bool, CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self.core.toggle_favorited_selected(timestamp);
        self.track_rename(old);
        result
    }

    pub fn begin_toggle_favorited_protected_selected(
        &mut self,
        timestamp: &str,
    ) -> Result<(bool, SecureJob), CoreError> {
        if self.operations.writing() {
            return Err(CoreError::UnsavedChanges);
        }
        let old = self.selected_path();
        let result = self
            .core
            .begin_toggle_favorited_protected_selected(timestamp);
        self.track_rename(old);
        result
    }

    pub fn finish_secure_operation(
        &mut self,
        completion: SecureCompletion,
    ) -> Result<SecureOutcome, CoreError> {
        let old = self.selected_path();
        let result = self.core.finish_secure_operation(completion);
        self.track_rename(old);
        result
    }
}

impl Workspace {
    pub(super) fn document_target(
        &self,
        id: &str,
    ) -> Result<DocumentTarget, super::actions::ActionError> {
        let path = self.resolve_target(id)?;
        if let Some(index) = self.notes().iter().position(|note| note.path == path) {
            return Ok(DocumentTarget::WorkspaceNote(index));
        }
        self.external_files()
            .iter()
            .find(|file| file.path == path)
            .map(|file| DocumentTarget::ExternalFile {
                engine_id: file.engine_id.clone(),
                item_id: file.item_id.clone(),
            })
            .ok_or(super::actions::ActionError::NotFound)
    }
    pub(super) fn verify_read(
        &mut self,
        id: &str,
        version: &str,
    ) -> Result<DocumentTarget, super::actions::ActionError> {
        let expected = self.expected_version(id, version)?;
        let path = self.resolve_target(id)?;
        if self.core.read_note_at(&path, 0, 1)?.version != expected {
            return Err(super::actions::ActionError::Conflict);
        }
        self.document_target(id)
    }
}

impl Workspace {
    #[cfg(test)]
    pub(super) fn set_rss_executor(&mut self, executor: super::rss::Executor) {
        self.rss_service = Some(super::rss::Service::with_executor(
            self.root().into(),
            executor,
        ));
    }
}

impl Workspace {
    pub(super) fn remember_ai(&mut self, settings: stillus_ai::AiSettings) -> String {
        self.next_version = self.next_version.saturating_add(1);
        let token = format!("versions/{:016x}/{:016x}", self.session, self.next_version);
        self.versions
            .insert(token.clone(), ReadVersion::Ai(settings));
        while self.versions.len() > 256 {
            self.versions.pop_first();
        }
        token
    }
    pub(super) fn ai_expected(
        &self,
        token: &str,
    ) -> Result<stillus_ai::AiSettings, super::actions::ActionError> {
        match self.versions.get(token) {
            Some(ReadVersion::Ai(settings)) => Ok(settings.clone()),
            _ => Err(super::actions::ActionError::Conflict),
        }
    }
}

impl Workspace {
    pub(super) fn remember_settings(
        &mut self,
        settings: super::settings::PublicSettings,
    ) -> String {
        self.next_version = self.next_version.saturating_add(1);
        let token = format!("versions/{:016x}/{:016x}", self.session, self.next_version);
        self.versions
            .insert(token.clone(), ReadVersion::Settings(settings));
        while self.versions.len() > 256 {
            self.versions.pop_first();
        }
        token
    }
    pub(super) fn settings_expected(
        &self,
        token: &str,
    ) -> Result<super::settings::PublicSettings, super::actions::ActionError> {
        match self.versions.get(token) {
            Some(ReadVersion::Settings(settings)) => Ok(settings.clone()),
            _ => Err(super::actions::ActionError::Conflict),
        }
    }
}
