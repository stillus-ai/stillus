// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use super::*;

impl WorkspaceSession {
    /// Updates public metadata of a catalog row without selecting or decrypting it.
    /// An unrelated dirty editor remains open; active writers and target recovery block.
    pub fn update_catalog_note_metadata(
        &mut self,
        path: &Path,
        edit: NoteMetadataEdit,
        timestamp: &str,
    ) -> Result<(), CoreError> {
        let _operation = stillus_platform::OperationLock::directory(&self.root)
            .map_err(|error| CoreError::Workspace(error.to_string()))?;
        self.ensure_catalog_metadata_ready()?;
        let index = self
            .notes
            .iter()
            .position(|note| note.path == path)
            .ok_or_else(|| CoreError::NoteUnavailable("catalog note is unavailable".into()))?;
        let note = self.notes[index].clone();
        if !note.availability.is_ready() {
            return Err(CoreError::NoteUnavailable(
                "catalog note is unavailable".into(),
            ));
        }
        if self
            .document
            .as_ref()
            .filter(|document| document.target == DocumentTarget::WorkspaceNote(index))
            .is_some_and(DocumentSession::operation_blocked)
        {
            return Err(CoreError::UnsavedChanges);
        }
        let key = self.recovery_store.key_for_note(path)?;
        let recovery = self.recovery_store.scan();
        let protected_recovery = self.recovery_store.scan_protected();
        if note.recovery_available
            || recovery.records.iter().any(|record| record.key == key)
            || self.recovery_store.protected_exists(&key)?
            || !recovery.diagnostics.is_empty()
            || !protected_recovery.diagnostics.is_empty()
        {
            return Err(CoreError::UnsavedChanges);
        }
        let (mut file, version) = open_versioned(path)?;
        let frontmatter =
            scan_reader(&mut file).map_err(|error| CoreError::Workspace(error.to_string()))?;
        let metadata = match &frontmatter.status {
            FrontMatterStatus::Parsed(parsed) => parsed.metadata.clone(),
            FrontMatterStatus::Plain => Default::default(),
            _ => return Err(CoreError::Save(SaveError::Conflict)),
        };
        if metadata.tags != note.tags
            || metadata.order != note.order
            || metadata.pinned.unwrap_or(false) != note.pinned
            || metadata.favorited.unwrap_or(false) != note.favorited
            || metadata.deleted.unwrap_or(false) != note.deleted
            || metadata.encryption.is_some() != (note.protection == NoteProtection::Protected)
            || self
                .document
                .as_ref()
                .filter(|document| document.target == DocumentTarget::WorkspaceNote(index))
                .is_some_and(|document| document.file_version != Some(version))
        {
            return Err(CoreError::Save(SaveError::Conflict));
        }
        let tags = match edit.tags {
            Some(tags) => normalize_rss_categories(&tags)?,
            None => note.tags,
        };
        let mut order = note.order;
        order.retain(|category, _| category == FAVORITED_ORDER_KEY || tags.contains(category));
        if edit.favorited == Some(false) {
            order.remove(FAVORITED_ORDER_KEY);
        }
        let patch = MetadataPatch {
            tags: Some(tags),
            order: Some(order),
            pinned: edit.pinned,
            favorited: edit.favorited,
            deleted: edit.deleted,
            modified: Some(timestamp.to_owned()),
            ..Default::default()
        };
        let rewrite = patch_front_matter(&frontmatter, &patch)
            .map_err(|error| CoreError::Save(SaveError::Patch(error)))?
            .ok_or_else(|| {
                CoreError::Save(SaveError::InvalidTarget("empty metadata edit".into()))
            })?;
        let next = scan_reader(Cursor::new(&rewrite.prefix))
            .map_err(|error| CoreError::Workspace(error.to_string()))?;
        self.apply_catalog_note_metadata(super::catalog_categories::NoteMetadataChange {
            index,
            path: path.to_path_buf(),
            version,
            patch,
            frontmatter: next,
            protected: note.protection == NoteProtection::Protected,
        })?;
        self.finish_note_edit(path, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stillus_platform::fs;

    #[test]
    fn catalog_metadata_does_not_open_the_target_or_replace_an_unrelated_dirty_editor() {
        check_target_metadata(false);
    }

    #[test]
    fn catalog_metadata_preserves_locked_ciphertext_and_unknown_yaml() {
        check_target_metadata(true);
    }

    #[test]
    fn catalog_metadata_rejects_an_external_metadata_change() {
        let root =
            std::env::temp_dir().join(format!("stillus-catalog-stale-{}", std::process::id()));
        fs::create_dir_all(root.join("notes")).unwrap();
        let target = root.join("notes/Target.md");
        fs::write(&target, "# Target\n").unwrap();
        let mut workspace = WorkspaceSession::open(&root).unwrap();
        let version = open_versioned(&target).unwrap().1;
        rewrite_metadata_versioned(
            &target,
            &version,
            &MetadataPatch {
                pinned: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(
            workspace.update_catalog_note_metadata(
                &target,
                NoteMetadataEdit {
                    deleted: Some(true),
                    ..Default::default()
                },
                "2026-09-09T00:00:00Z"
            ),
            Err(CoreError::Save(SaveError::Conflict))
        ));
        let scan = scan_reader(Cursor::new(fs::read(&target).unwrap())).unwrap();
        let FrontMatterStatus::Parsed(frontmatter) = scan.status else {
            panic!("updated metadata");
        };
        assert_eq!(frontmatter.metadata.pinned, Some(true));
        assert_ne!(frontmatter.metadata.deleted, Some(true));
        drop(workspace);
        fs::remove_dir_all(root).unwrap();
    }

    fn check_target_metadata(protected: bool) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "stillus-catalog-metadata-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("notes")).unwrap();
        let current = root.join("notes/Current.md");
        let target = root.join("notes/Target.md");
        fs::write(&current, "# Current\n").unwrap();
        fs::write(
            &target,
            "---\ncustom: preserved\n---\n# Target\nbody stays\n",
        )
        .unwrap();
        if protected {
            let version = open_versioned(&target).unwrap().1;
            let password = MasterPassword::new("catalog fixture password".into());
            protect_note_body(&target, &version, &password, "Target").unwrap();
        }
        let before = fs::read(&target).unwrap();
        let before_scan = scan_reader(Cursor::new(&before)).unwrap();
        let FrontMatterStatus::Parsed(before_frontmatter) = before_scan.status else {
            panic!("fixture front matter");
        };
        let mut workspace = WorkspaceSession::open(&root).unwrap();
        let index = workspace
            .notes
            .iter()
            .position(|note| note.path == current)
            .unwrap();
        workspace.open_note(index).unwrap();
        workspace
            .apply_selected_at(EditorCommand::Insert("dirty".into()), 0)
            .unwrap();
        let revision = workspace.document().unwrap().content_revision();
        workspace
            .update_catalog_note_metadata(
                &target,
                NoteMetadataEdit {
                    pinned: Some(true),
                    deleted: Some(true),
                    ..Default::default()
                },
                "2026-09-09T00:00:00Z",
            )
            .unwrap();
        assert_eq!(
            workspace.notes[workspace.selected_note.unwrap()].path,
            current
        );
        assert_eq!(workspace.document().unwrap().content_revision(), revision);
        assert!(workspace.document().unwrap().has_unsaved_work());
        assert_eq!(fs::read_to_string(&current).unwrap(), "# Current\n");
        let after = fs::read(&target).unwrap();
        let after_scan = scan_reader(Cursor::new(&after)).unwrap();
        let FrontMatterStatus::Parsed(after_frontmatter) = after_scan.status else {
            panic!("saved front matter");
        };
        assert!(
            std::str::from_utf8(&after[..after_frontmatter.body_offset as usize])
                .unwrap()
                .contains("custom: preserved")
        );
        assert_eq!(
            &before[before_frontmatter.body_offset as usize..],
            &after[after_frontmatter.body_offset as usize..]
        );
        let note = workspace
            .notes
            .iter()
            .find(|note| note.path == target)
            .unwrap();
        assert!(note.pinned && note.deleted);
        assert_eq!(note.protection == NoteProtection::Protected, protected);
        assert!(!workspace.has_master_password());
        drop(workspace);
        fs::remove_dir_all(root).unwrap();
    }
}
