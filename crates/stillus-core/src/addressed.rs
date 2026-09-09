// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Versioned document operations that do not depend on the selected sidebar row.
use super::*;
use std::io::{self, Write};

pub const MAX_ACTION_TEXT: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoteVersion {
    file: FileVersion,
    buffer: Option<u64>,
}

#[derive(Debug)]
pub struct NoteRead {
    pub text: String,
    pub offset: usize,
    pub total_bytes: u64,
    pub version: NoteVersion,
}

pub struct NoteEdit {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

#[derive(Default)]
pub struct NoteMetadataEdit {
    pub tags: Option<Vec<String>>,
    pub pinned: Option<bool>,
    pub favorited: Option<bool>,
    pub deleted: Option<bool>,
}

fn invalid() -> CoreError {
    CoreError::NoteUnavailable("invalid document range".into())
}
fn conflict() -> CoreError {
    CoreError::Save(SaveError::Conflict)
}

fn plain_file(path: &Path) -> Result<(fs::File, FileVersion, u64), CoreError> {
    let (mut file, version) = open_versioned(path)?;
    let scan = scan_reader(&mut file).map_err(|_| invalid())?;
    let offset = match scan.status {
        FrontMatterStatus::Plain => 0,
        FrontMatterStatus::Parsed(parsed) if parsed.metadata.encryption.is_none() => {
            parsed.body_offset
        }
        FrontMatterStatus::Parsed(_) => return Err(CoreError::MasterPasswordRequired),
        FrontMatterStatus::Invalid { .. } => return Err(invalid()),
    };
    Ok((file, version, offset))
}

use stillus_platform::fs;

impl WorkspaceSession {
    fn ensure_addressed_write_ready(&self) -> Result<(), CoreError> {
        self.ensure_no_secure_operation()?;
        if self.pending_integrity.is_some()
            || self.password_change_recovery_blocked
            || self.document.as_ref().is_some_and(|document| {
                document.autosave.saving_revision.is_some()
                    || document.autosave.recovery_saving_revision.is_some()
            })
        {
            return Err(CoreError::UnsavedChanges);
        }
        Ok(())
    }

    pub fn create_note_addressed(
        &mut self,
        title: &str,
        timestamp: &str,
    ) -> Result<PathBuf, CoreError> {
        self.ensure_addressed_write_ready()?;
        let commit = create_note_file(&self.root, title, timestamp)?;
        self.finish_note_edit(&commit.path, &commit.path)?;
        Ok(commit.path)
    }

    fn verify_note_version(&self, path: &Path, expected: NoteVersion) -> Result<usize, CoreError> {
        self.ensure_addressed_write_ready()?;
        let index = self.addressed_index(path)?;
        let (file, version, _) = plain_file(path)?;
        drop(file);
        if version != expected.file {
            return Err(conflict());
        }
        let document = self
            .document
            .as_ref()
            .filter(|d| d.target == DocumentTarget::WorkspaceNote(index));
        if expected.buffer != document.map(DocumentSession::content_revision) {
            return Err(conflict());
        }
        if document.is_some_and(DocumentSession::operation_blocked)
            || self.notes[index].recovery_available
        {
            return Err(CoreError::UnsavedChanges);
        }
        Ok(index)
    }

    pub fn rename_note_addressed(
        &mut self,
        path: &Path,
        expected: NoteVersion,
        title: &str,
        timestamp: &str,
    ) -> Result<PathBuf, CoreError> {
        let _lock =
            stillus_platform::OperationLock::directory(&self.root).map_err(|_| invalid())?;
        let index = self.verify_note_version(path, expected)?;
        let commit = rename_note_file(&self.root, path, &expected.file, title, timestamp)?;
        if self.selected_note == Some(index) {
            self.refresh_and_open(&commit.path)?;
        } else {
            self.finish_note_edit(path, &commit.path)?;
        }
        Ok(commit.path)
    }

    pub fn update_note_metadata_addressed(
        &mut self,
        path: &Path,
        expected: NoteVersion,
        edit: NoteMetadataEdit,
        timestamp: &str,
    ) -> Result<(), CoreError> {
        let _lock =
            stillus_platform::OperationLock::directory(&self.root).map_err(|_| invalid())?;
        let index = self.verify_note_version(path, expected)?;
        let tags = edit
            .tags
            .map(|tags| {
                tags.iter()
                    .map(|tag| validate_tag(tag))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let mut order = self.notes[index].order.clone();
        if let Some(tags) = &tags {
            order.retain(|category, _| category == FAVORITED_ORDER_KEY || tags.contains(category));
        }
        if edit.favorited == Some(false) {
            order.remove(FAVORITED_ORDER_KEY);
        }
        rewrite_metadata_versioned(
            path,
            &expected.file,
            &MetadataPatch {
                order: Some(order),
                tags,
                pinned: edit.pinned,
                favorited: edit.favorited,
                deleted: edit.deleted,
                modified: Some(timestamp.into()),
                ..MetadataPatch::default()
            },
        )?;
        if self.selected_note == Some(index) {
            self.refresh_and_open(path)?;
        } else {
            self.finish_note_edit(path, path)?;
        }
        Ok(())
    }

    fn addressed_index(&self, path: &Path) -> Result<usize, CoreError> {
        self.ensure_no_secure_operation()?;
        if self.pending_integrity.is_some() || self.password_change_recovery_blocked {
            return Err(CoreError::UnsavedChanges);
        }
        let index = self
            .notes
            .iter()
            .position(|note| note.path == path)
            .ok_or_else(invalid)?;
        let note = &self.notes[index];
        if note.protection == NoteProtection::Protected {
            return Err(CoreError::MasterPasswordRequired);
        }
        if !note.availability.is_ready() {
            return Err(invalid());
        }
        Ok(index)
    }

    fn addressed_target(&self, path: &Path) -> Result<DocumentTarget, CoreError> {
        if let Some(file) = self.external_files.iter().find(|file| file.path == path) {
            self.ensure_no_secure_operation()?;
            if self.pending_integrity.is_some() || self.password_change_recovery_blocked {
                return Err(CoreError::UnsavedChanges);
            }
            if !matches!(file.availability, ItemAvailability::Ready) {
                return Err(invalid());
            }
            return Ok(DocumentTarget::ExternalFile {
                engine_id: file.engine_id.clone(),
                item_id: file.item_id.clone(),
            });
        }
        self.addressed_index(path)
            .map(DocumentTarget::WorkspaceNote)
    }
    fn addressed_recovery(&self, target: &DocumentTarget) -> bool {
        match target {
            DocumentTarget::WorkspaceNote(index) => self.notes[*index].recovery_available,
            DocumentTarget::ExternalFile { engine_id, item_id } => {
                self.external_files.iter().any(|file| {
                    file.engine_id == *engine_id
                        && file.item_id == *item_id
                        && file.recovery_available
                })
            }
        }
    }

    pub fn read_note_at(
        &self,
        path: &Path,
        offset: usize,
        limit: usize,
    ) -> Result<NoteRead, CoreError> {
        let target = self.addressed_target(path)?;
        let limit = limit.clamp(1, MAX_ACTION_TEXT);
        if let Some(document) = self.document.as_ref().filter(|d| d.target == target) {
            let total = document.len_bytes();
            if offset > total {
                return Err(invalid());
            }
            let mut end = offset.saturating_add(limit).min(total);
            while end > offset
                && !document
                    .editor
                    .is_codepoint_boundary(ByteOffset::new(end))?
            {
                end -= 1;
            }
            return Ok(NoteRead {
                text: document.editor.slice(ByteRange::new(
                    ByteOffset::new(offset),
                    ByteOffset::new(end),
                )?)?,
                offset,
                total_bytes: total as u64,
                version: NoteVersion {
                    file: document.file_version.ok_or_else(invalid)?,
                    buffer: Some(document.content_revision()),
                },
            });
        }
        let (mut file, version, body) =
            action_file(path, matches!(target, DocumentTarget::ExternalFile { .. }))?;
        let length = file
            .metadata()
            .map_err(|_| invalid())?
            .len()
            .saturating_sub(body);
        if offset as u64 > length {
            return Err(invalid());
        }
        file.seek(SeekFrom::Start(body + offset as u64))
            .map_err(|_| invalid())?;
        let mut bytes = Vec::new();
        file.take(limit as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| invalid())?;
        let utf8_length = match std::str::from_utf8(&bytes) {
            Ok(_) => bytes.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => return Err(invalid()),
        };
        bytes.truncate(utf8_length);
        let text = String::from_utf8(bytes).map_err(|_| invalid())?;
        if open_versioned(path)?.1 != version {
            return Err(conflict());
        }
        Ok(NoteRead {
            text,
            offset,
            total_bytes: length,
            version: NoteVersion {
                file: version,
                buffer: None,
            },
        })
    }

    pub fn begin_note_edit(
        &mut self,
        path: &Path,
        expected: NoteVersion,
        edit: NoteEdit,
        now_ms: u64,
        timestamp: &str,
    ) -> Result<AddressedEdit, CoreError> {
        self.ensure_addressed_write_ready()?;
        let target = self.addressed_target(path)?;
        if edit.text.len() > MAX_ACTION_TEXT || edit.end < edit.start {
            return Err(invalid());
        }
        let recovery = self.addressed_recovery(&target);
        if let Some(document) = self.document.as_mut().filter(|d| d.target == target) {
            if document.file_version != Some(expected.file)
                || expected.buffer != Some(document.content_revision())
            {
                return Err(conflict());
            }
            if recovery && document.autosave.recovery_revision == 0 {
                return Err(CoreError::UnsavedChanges);
            }
            match document.save_status() {
                SaveStatus::Clean { .. } | SaveStatus::Dirty { .. } => {}
                SaveStatus::Conflict { .. } => return Err(conflict()),
                _ => return Err(CoreError::UnsavedChanges),
            }
            // Catch changes made outside the app even before the next external poll.
            if open_versioned(path)?.1 != expected.file {
                return Err(conflict());
            }
            let outcome = document.apply_at(
                EditorCommand::ReplaceRange {
                    start: edit.start,
                    end: edit.end,
                    text: edit.text,
                },
                now_ms,
            )?;
            if let DocumentTarget::WorkspaceNote(index) = target {
                self.notes[index].title = document.title().into();
            }
            return Ok(AddressedEdit::Applied(outcome));
        }
        if expected.buffer.is_some() {
            return Err(conflict());
        }
        if recovery {
            return Err(CoreError::UnsavedChanges);
        }
        let (mut file, version, body_offset) =
            action_file(path, matches!(target, DocumentTarget::ExternalFile { .. }))?;
        if version != expected.file {
            return Err(conflict());
        }
        let body_len = file
            .metadata()
            .map_err(|_| invalid())?
            .len()
            .saturating_sub(body_offset);
        if edit.end as u64 > body_len {
            return Err(invalid());
        }
        for offset in [edit.start, edit.end] {
            if offset as u64 == body_len {
                continue;
            }
            file.seek(SeekFrom::Start(body_offset + offset as u64))
                .map_err(|_| invalid())?;
            let mut byte = [0];
            file.read_exact(&mut byte).map_err(|_| invalid())?;
            if byte[0] & 0xc0 == 0x80 {
                return Err(invalid());
            }
        }
        Ok(AddressedEdit::Job(NoteEditJob {
            path: path.into(),
            workspace: self.root.clone(),
            version,
            edit,
            body_offset,
            timestamp: timestamp.into(),
            external: match target {
                DocumentTarget::ExternalFile { engine_id, item_id } => Some((engine_id, item_id)),
                _ => None,
            },
        }))
    }

    /// Prepare inactive recovery without borrowing or replacing the visible editor.
    pub fn begin_addressed_restore(
        &self,
        path: &Path,
        expected: NoteVersion,
    ) -> Result<NoteRestoreJob, CoreError> {
        self.ensure_addressed_write_ready()?;
        let target = self.addressed_target(path)?;
        if self
            .document
            .as_ref()
            .is_some_and(|document| document.target == target)
            || expected.buffer.is_some()
        {
            return Err(CoreError::UnsavedChanges);
        }
        if !self.addressed_recovery(&target) {
            return Err(invalid());
        }
        if self.read_note_at(path, 0, 1)?.version != expected {
            return Err(conflict());
        }
        Ok(NoteRestoreJob {
            workspace: self.root.clone(),
            path: path.to_owned(),
            expected,
            external: matches!(target, DocumentTarget::ExternalFile { .. }),
        })
    }

    /// Reconcile one committed target while retaining the active buffer and selection.
    pub fn finish_note_edit(&mut self, old_path: &Path, new_path: &Path) -> Result<(), CoreError> {
        let selected_path = self
            .selected_note
            .and_then(|i| self.notes.get(i))
            .map(|n| n.path.clone());
        self.refresh_notes()?;
        if let Some(path) = selected_path {
            let path = if path == old_path { new_path } else { &path };
            let index = self
                .notes
                .iter()
                .position(|n| n.path == path)
                .ok_or_else(invalid)?;
            self.selected_note = Some(index);
            if let Some(document) = self.document.as_mut().filter(|d| !d.is_external()) {
                document.note_index = index;
                document.target = DocumentTarget::WorkspaceNote(index);
                self.notes[index].title = document.title().into();
            }
        }
        Ok(())
    }
}

pub enum AddressedEdit {
    Applied(CommandOutcome),
    Job(NoteEditJob),
}

pub struct NoteEditJob {
    path: PathBuf,
    workspace: PathBuf,
    version: FileVersion,
    edit: NoteEdit,
    body_offset: u64,
    timestamp: String,
    external: Option<(EngineId, ItemId)>,
}

impl NoteEditJob {
    pub fn execute(self) -> Result<PathBuf, CoreError> {
        let _lock =
            stillus_platform::OperationLock::directory(&self.workspace).map_err(|_| invalid())?;
        let (mut source, current, body) = action_file(&self.path, self.external.is_some())?;
        if current != self.version || body != self.body_offset {
            return Err(conflict());
        }
        let store = RecoveryStore::new(&self.workspace);
        let key = match &self.external {
            Some((engine_id, item_id)) => {
                store.key_for_external(engine_id.as_str(), item_id.as_str())?
            }
            None => store.key_for_note(&self.path)?,
        };
        if store.scan().records.iter().any(|record| record.key == key) {
            return Err(CoreError::UnsavedChanges);
        }
        if self.external.is_some() {
            let commit = rewrite_external_file_versioned(&self.path, &self.version, |output| {
                source.seek(SeekFrom::Start(0))?;
                let copied = io::copy(&mut (&mut source).take(self.edit.start as u64), output)?;
                if copied != self.edit.start as u64 {
                    return Err(io::Error::other("source changed"));
                }
                output.write_all(self.edit.text.as_bytes())?;
                source.seek(SeekFrom::Start(self.edit.end as u64))?;
                io::copy(&mut source, output)?;
                Ok(())
            })?;
            return Ok(commit.path);
        }
        let mut prefix = Vec::new();
        source.seek(SeekFrom::Start(body)).map_err(|_| invalid())?;
        (&mut source)
            .take(self.edit.start.min(stillus_storage::BODY_TITLE_SCAN_BYTES) as u64)
            .read_to_end(&mut prefix)
            .map_err(|_| invalid())?;
        if prefix.len() < stillus_storage::BODY_TITLE_SCAN_BYTES {
            prefix.extend(
                self.edit
                    .text
                    .bytes()
                    .take(stillus_storage::BODY_TITLE_SCAN_BYTES - prefix.len()),
            );
            if prefix.len() < stillus_storage::BODY_TITLE_SCAN_BYTES {
                source
                    .seek(SeekFrom::Start(body + self.edit.end as u64))
                    .map_err(|_| invalid())?;
                (&mut source)
                    .take((stillus_storage::BODY_TITLE_SCAN_BYTES - prefix.len()) as u64)
                    .read_to_end(&mut prefix)
                    .map_err(|_| invalid())?;
            }
        }
        let prefix = match std::str::from_utf8(&prefix) {
            Ok(text) => text,
            Err(error) if error.error_len().is_none() => {
                std::str::from_utf8(&prefix[..error.valid_up_to()]).map_err(|_| invalid())?
            }
            Err(_) => return Err(invalid()),
        };
        let title = project_body_title(prefix).unwrap_or_else(|| EMPTY_NOTE_TITLE.into());
        let commit = rewrite_note_with_title(
            &self.workspace,
            &self.path,
            &self.version,
            &MetadataPatch {
                title: Some(title.clone()),
                modified: Some(self.timestamp),
                ..MetadataPatch::default()
            },
            &title,
            |output| {
                source.seek(SeekFrom::Start(body))?;
                let copied = io::copy(&mut (&mut source).take(self.edit.start as u64), output)?;
                if copied != self.edit.start as u64 {
                    return Err(io::Error::other("source changed"));
                }
                output.write_all(self.edit.text.as_bytes())?;
                source.seek(SeekFrom::Start(body + self.edit.end as u64))?;
                io::copy(&mut source, output)?;
                Ok(())
            },
        )?;
        Ok(commit.path)
    }
}

fn action_file(path: &Path, external: bool) -> Result<(fs::File, FileVersion, u64), CoreError> {
    if external {
        let (file, version) = open_versioned(path)?;
        Ok((file, version, 0))
    } else {
        plain_file(path)
    }
}

/// A single bounded document worker reuses recovery validation and persistence.
pub struct NoteRestoreJob {
    workspace: PathBuf,
    path: PathBuf,
    expected: NoteVersion,
    external: bool,
}
impl NoteRestoreJob {
    pub fn execute(self) -> Result<PathBuf, CoreError> {
        let mut workspace = WorkspaceSession::open(&self.workspace)?;
        let target = if self.external {
            workspace.attach_external_file(&self.path)?
        } else {
            workspace.addressed_target(&self.path)?
        };
        if workspace.read_note_at(&self.path, 0, 1)?.version != self.expected {
            return Err(conflict());
        }
        match target {
            DocumentTarget::WorkspaceNote(index) => workspace.restore_recovery(index, 0)?,
            DocumentTarget::ExternalFile { engine_id, item_id } => {
                workspace.restore_external_recovery(&engine_id, &item_id, 0)?
            }
        }
        if workspace.document().ok_or_else(invalid)?.file_version != Some(self.expected.file) {
            return Err(conflict());
        }
        if matches!(
            workspace.document().ok_or_else(invalid)?.save_status(),
            SaveStatus::Conflict { .. }
        ) {
            return Err(conflict());
        }
        workspace.retry_autosave(0);
        let timestamp = format_utc_timestamp(SystemTime::now())?;
        let job = workspace
            .begin_persistence(u64::MAX, timestamp)?
            .ok_or_else(invalid)?;
        workspace.finish_persistence(job.execute())?;
        if !matches!(
            workspace.document().ok_or_else(invalid)?.save_status(),
            SaveStatus::Clean { .. }
        ) {
            return Err(conflict());
        }
        Ok(workspace
            .document_path_and_recovery_key(workspace.document().ok_or_else(invalid)?.target())?
            .0)
    }
}
