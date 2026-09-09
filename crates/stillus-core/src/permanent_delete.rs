// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use super::*;

/// Captured before the confirmation dialog; callers cannot substitute its target
/// or version after the user confirms deletion.
#[derive(Clone, Debug)]
pub struct PermanentNoteDeletion {
    path: PathBuf,
    version: FileVersion,
}

impl WorkspaceSession {
    pub fn prepare_note_deletion(&self, path: &Path) -> Result<PermanentNoteDeletion, CoreError> {
        let _operation = stillus_platform::OperationLock::directory(&self.root)
            .map_err(|error| CoreError::Workspace(error.to_string()))?;
        self.ensure_note_deletion_ready(path)?;
        let (file, version) = open_versioned(path)?;
        drop(file);
        Ok(PermanentNoteDeletion {
            path: path.to_path_buf(),
            version,
        })
    }

    pub fn delete_note_permanently(
        &mut self,
        request: PermanentNoteDeletion,
    ) -> Result<(), CoreError> {
        let _operation = stillus_platform::OperationLock::directory(&self.root)
            .map_err(|error| CoreError::Workspace(error.to_string()))?;
        self.ensure_note_deletion_ready(&request.path)?;
        let result = stillus_storage::delete_trashed_note_versioned(
            &self.root,
            &request.path,
            &request.version,
        );
        if result.is_ok() || matches!(&result, Err(NoteOperationError::PartialCommit { .. })) {
            let deleting_selected = self
                .selected_note
                .and_then(|index| self.notes.get(index))
                .is_some_and(|note| note.path == request.path);
            if deleting_selected {
                self.selected_note = None;
                self.document = None;
            }
            // Reconcile remaining indices without opening/decrypting another note.
            self.finish_note_edit(&request.path, &request.path)?;
        }
        result.map_err(CoreError::from)
    }

    fn ensure_note_deletion_ready(&self, path: &Path) -> Result<(), CoreError> {
        self.ensure_workspace_action_ready()?;
        let note = self
            .notes
            .iter()
            .find(|note| note.path == path)
            .ok_or_else(|| CoreError::NoteUnavailable("deletion target is unavailable".into()))?;
        if !note.deleted || !note.availability.is_ready() {
            return Err(CoreError::NoteUnavailable(
                "only a valid trashed note can be deleted permanently".into(),
            ));
        }
        let key = self.recovery_store.key_for_note(path)?;
        let plain = self.recovery_store.scan();
        let protected = self.recovery_store.scan_protected();
        // Do not destroy the canonical owner of recovery work. Diagnostics mean
        // recovery cannot be proven absent, so require resolution before removal.
        if note.recovery_available
            || plain.records.iter().any(|record| record.key == key)
            || self.recovery_store.protected_exists(&key)?
            || !plain.diagnostics.is_empty()
            || !protected.diagnostics.is_empty()
        {
            return Err(CoreError::UnsavedChanges);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stillus_platform::fs;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "stillus-core-permanent-delete-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("notes")).unwrap();
            fs::write(
                root.join("notes/Trash.md"),
                "---\ntitle: Trash\ndeleted: true\n---\n# Trash\nbody\n",
            )
            .unwrap();
            fs::write(root.join("notes/Keep.md"), "# Keep\n").unwrap();
            Self(root)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn permanent_delete_reconciles_selection_without_removing_other_notes() {
        let fixture = Fixture::new();
        let mut workspace = WorkspaceSession::open(&fixture.0).unwrap();
        let path = fixture.0.join("notes/Trash.md");
        let index = workspace
            .notes
            .iter()
            .position(|note| note.path == path)
            .unwrap();
        workspace.open_note(index).unwrap();
        let request = workspace.prepare_note_deletion(&path).unwrap();
        workspace.delete_note_permanently(request).unwrap();
        assert!(workspace.selected_note.is_none());
        assert!(workspace.document.is_none());
        assert_eq!(workspace.notes.len(), 1);
        assert_eq!(workspace.notes[0].title, "Keep");
        assert!(fixture.0.join("notes/Keep.md").exists());
    }

    #[test]
    fn permanent_delete_rejects_a_changed_note_after_confirmation() {
        let fixture = Fixture::new();
        let mut workspace = WorkspaceSession::open(&fixture.0).unwrap();
        let path = fixture.0.join("notes/Trash.md");
        let request = workspace.prepare_note_deletion(&path).unwrap();
        fs::write(&path, "---\ndeleted: false\n---\nrestored elsewhere\n").unwrap();
        assert!(matches!(
            workspace.delete_note_permanently(request),
            Err(CoreError::Operation(NoteOperationError::Conflict))
        ));
        assert!(path.exists());
    }

    #[test]
    fn permanent_delete_refuses_dirty_documents_and_existing_recovery() {
        let fixture = Fixture::new();
        let mut workspace = WorkspaceSession::open(&fixture.0).unwrap();
        let path = fixture.0.join("notes/Trash.md");
        let index = workspace
            .notes
            .iter()
            .position(|note| note.path == path)
            .unwrap();
        workspace.open_note(index).unwrap();
        workspace
            .apply_selected_at(EditorCommand::Insert("unsaved".into()), 0)
            .unwrap();
        assert!(matches!(
            workspace.prepare_note_deletion(&path),
            Err(CoreError::UnsavedChanges)
        ));
        let recovery = workspace
            .begin_persistence(RECOVERY_DEBOUNCE_MS, "2026-09-09T00:00:00Z".into())
            .unwrap()
            .expect("recovery is due before canonical autosave");
        workspace.finish_persistence(recovery.execute()).unwrap();
        let reopened = WorkspaceSession::open(&fixture.0).unwrap();
        assert!(matches!(
            reopened.prepare_note_deletion(&path),
            Err(CoreError::UnsavedChanges)
        ));
        assert!(path.exists());
    }

    #[test]
    fn permanent_delete_handles_a_locked_protected_note_without_decrypting() {
        let fixture = Fixture::new();
        let path = fixture.0.join("notes/Trash.md");
        let version = open_versioned(&path).unwrap().1;
        let password = MasterPassword::new("delete fixture password".into());
        protect_note_body(&path, &version, &password, "Trash").unwrap();
        let mut workspace = WorkspaceSession::open(&fixture.0).unwrap();
        assert!(!workspace.has_master_password());
        let request = workspace.prepare_note_deletion(&path).unwrap();
        workspace.delete_note_permanently(request).unwrap();
        assert!(!path.exists());
        assert_eq!(workspace.notes.len(), 1);
    }
}
